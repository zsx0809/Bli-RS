// ==============================================================================
// 推荐流数据桥接
//
// 线程模型（本项目最容易踩坑的地方，改之前务必读完）
// ------------------------------------------------------------------------------
// Slint 的类型里，**只有** `Weak<AppWindow>` 和纯数据能跨线程：
//
//   Send      : Weak<AppWindow>, String, i64, Vec<纯数据>
//   NOT Send  : AppWindow, Image(OpaqueImage), ModelRc, VecModel, Rc<...>
//
// 最隐蔽的一个坑：`Image` **不是 Send**（内部是 VRc<OpaqueImageVTable>）。
// 所以 `VideoItemData` 整体也不能跨线程 —— 它含 `cover: image` 字段。
//
// 数据流因此拆成三段，每段只传纯数据：
//
//   1. 后台线程 --(Vec<RawItem> 纯字符串)--> UI 线程构建 VideoItemData
//   2. 后台线程 --(bvid, w, h, rgba 字节)--> UI 线程构建 Image 并回填
//   3. UI 线程 --(可见行范围)--> 决定要下载哪些封面
//
// 分页与懒加载
// ------------------------------------------------------------------------------
// 翻页用 fresh_idx 游标（不是页码）：0=首屏, 然后每次递增。
// 实测连续用 4 会拿到重复内容，必须把 fresh_idx 往上加。
//
// 封面懒加载：只下载「可见行 ± 2 行」内的卡片封面。
// 之前是一次性下 20 张（约 4 秒），改成按需后首屏 4 张并发约 0.5 秒。
// ==============================================================================

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::mpsc;

use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use crate::{AppWindow, VideoItemData, VideoRowPair};

/// 纯字符串版本的条目 —— 可以跨线程。
///
/// 存在的唯一理由是 `VideoItemData.cover` 是 Image（非 Send）。
struct RawItem {
    bvid: String,
    title: String,
    author: String,
    views: String,
    danmaku: String,
    duration: String,
    cover_url: String,
}

impl RawItem {
    /// 在 UI 线程调用，补上空 Image 变成完整的 VideoItemData。
    fn into_slint(self) -> VideoItemData {
        VideoItemData {
            id: self.bvid.into(),
            title: self.title.into(),
            author: self.author.into(),
            views: self.views.into(),
            danmaku: self.danmaku.into(),
            duration: self.duration.into(),
            cover_url: self.cover_url.into(),
            cover: Image::default(),
        }
    }
}

fn to_raw(v: &bli_protocol::recommend::VideoItem) -> RawItem {
    RawItem {
        bvid: v.bvid.clone(),
        title: v.title.clone(),
        author: v.author.clone(),
        views: v.view_text.clone(),
        danmaku: v.danmaku_text.clone(),
        duration: format_duration(v.duration),
        cover_url: v.cover_url.clone(),
    }
}

/// 秒 -> "mm:ss" 或 "h:mm:ss"，对齐 PiliPlus DurationUtils.formatDuration。
fn format_duration(secs: i64) -> String {
    if secs <= 0 {
        return "00:00".into();
    }
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// 从 UI 侧的 VideoItemData 反推可序列化的 VideoItem。
///
/// 只在写首页缓存时用。数值字段（aid/cid/duration 秒）在 UI 侧已经被
/// 格式化成字符串了，这里无处可还原，所以留默认值 —— 缓存只用于
/// **渲染卡片**，打开视频时一律重新请求详情，不依赖这些数字。
fn row_to_item(v: &VideoItemData) -> bli_protocol::recommend::VideoItem {
    bli_protocol::recommend::VideoItem {
        aid: 0,
        bvid: v.id.to_string(),
        cid: 0,
        title: v.title.to_string(),
        author: v.author.to_string(),
        cover_url: v.cover_url.to_string(),
        duration: 0,
        view_text: v.views.to_string(),
        danmaku_text: v.danmaku.to_string(),
    }
}

/// 把扁平列表两两配对成行。
fn pair_rows(items: Vec<VideoItemData>) -> Vec<VideoRowPair> {
    let mut rows = Vec::with_capacity(items.len().div_ceil(2));
    let mut iter = items.into_iter();

    while let Some(left) = iter.next() {
        match iter.next() {
            Some(right) => rows.push(VideoRowPair {
                left,
                right,
                has_right: true,
            }),
            // 奇数尾行：右卡留空，UI 侧画等宽占位防止左卡被拉伸
            None => rows.push(VideoRowPair {
                left,
                right: VideoItemData::default(),
                has_right: false,
            }),
        }
    }

    rows
}
// ==============================================================================
// 全局状态
//
// ★ 这里曾经有一个极隐蔽、后果却很严重的 bug：翻页只翻得动一次。
//
//   原实现把 IN_FLIGHT / FRESH_IDX 放在 thread_local 里，而
//   spawn_page_request 是在 **UI 线程**把 IN_FLIGHT 置 true，
//   然后丢给一个**新建的工作线程**去请求，再在工作线程里把它置回 false。
//   thread_local 每个线程一份 —— 工作线程改的是它自己那份，
//   UI 线程那份永远停在 true。
//
//   后果：第二次 load_more() 一进来就看到 IN_FLIGHT == true，直接 return。
//   表现为「首页拉到第一页之后再也翻不动」「刷新以后也只有那一批视频」。
//   同一个错误也让 FRESH_IDX 卡住，永远请求同一页。
//
//   所以请求状态必须是**进程级**的，用 static + Mutex/Atomic，
//   只有真正只在 UI 线程访问的东西（比如 MODEL 句柄）才适合 thread_local。
// ==============================================================================

/// 当前翻页游标。0 表示还没加载过任何一页。
static FRESH_IDX: std::sync::Mutex<u32> = std::sync::Mutex::new(0);
/// 是否正在加载（首屏或下一页），防止重复触发。**必须进程级**（见上面的说明）。
static IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 已出现过的 bvid，用于跨页去重
static SEEN_BVIDS: std::sync::Mutex<Option<HashSet<String>>> = std::sync::Mutex::new(None);

use std::sync::atomic::Ordering;

fn idx_load() -> u32 {
    match FRESH_IDX.lock() {
        Ok(g) => *g,
        Err(e) => *e.into_inner(),
    }
}

fn idx_store(v: u32) {
    match FRESH_IDX.lock() {
        Ok(mut g) => *g = v,
        Err(e) => *e.into_inner() = v,
    }
}

/// 首页列表缓存文件（放 filesDir，不用 cacheDir —— 系统清缓存时不该把
/// 「首页长什么样」一起清掉，否则冷启动又是白屏）。
fn cache_file() -> std::path::PathBuf {
    let base = crate::android_window::files_dir()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/data/data/com.bli.rs/files"));
    let _ = std::fs::create_dir_all(&base);
    base.join("feed_cache.json")
}

/// 读上次的首页列表。任何解析问题都当作"没有缓存"，不报错。
fn cache_read() -> Option<Vec<bli_protocol::recommend::VideoItem>> {
    let text = std::fs::read_to_string(cache_file()).ok()?;
    let items: Vec<bli_protocol::recommend::VideoItem> = serde_json::from_str(&text).ok()?;
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

/// 把当前列表写盘。封面的 bvid/url 都在 VideoItem 里，所以整份可序列化。
fn cache_write(items: &[bli_protocol::recommend::VideoItem]) {
    // 只留前 60 条：缓存的作用是冷启动第一屏，攒太多只会让启动读盘变慢
    let slice = &items[..items.len().min(60)];
    match serde_json::to_string(slice) {
        Ok(s) => {
            let _ = std::fs::write(cache_file(), s);
        }
        Err(e) => log::warn!("首页缓存写入失败: {e}"),
    }
}

/// 首屏加载。
///
/// 有本地缓存时**先显示缓存再静默刷新** —— 冷启动不再白屏等网络。
pub fn load_feed(window: slint::Weak<AppWindow>, _fresh_idx: u32) {
    // ---- 1. 有缓存：先铺屏，再后台刷新 ----
    if let Some(items) = cache_read() {
        let covers: Vec<(String, String)> = items
            .iter()
            .filter(|v| !v.cover_url.is_empty())
            .map(|v| (v.bvid.clone(), v.cover_url.clone()))
            .collect();
        let raws: Vec<RawItem> = items.iter().map(to_raw).collect();
        let n = items.len();

        let _ = window.upgrade_in_event_loop(move |w| {
            let ui_items: Vec<VideoItemData> =
                raws.into_iter().map(RawItem::into_slint).collect();
            let model = Rc::new(VecModel::from(pair_rows(ui_items)));
            w.set_rows(ModelRc::from(model.clone()));
            MODEL.with(|m| *m.borrow_mut() = Some(model));
            w.set_feed_state(2);
            w.set_feed_error(Default::default());
            w.set_has_more(true);
            w.set_loading_more(false);
            w.set_more_error(Default::default());
            ensure_cover_pump();
            spawn_batched_loader(covers);
        });

        crate::mxprobe::log_public(&format!(
            "[Feed] 冷启动命中本地缓存 {n} 条，先上屏，后台继续刷新"
        ));

        // 刷新时不显示骨架屏，也不动已有列表（quiet = true）
        idx_store(0);
        spawn_page_request(window, Mode::Refresh, /* quiet = */ true);
        return;
    }

    // ---- 2. 无缓存：骨架屏 + 网络 ----
    idx_store(0);
    if let Ok(mut g) = SEEN_BVIDS.lock() {
        g.get_or_insert_with(HashSet::new).clear();
    }

    let weak = window.clone();
    let _ = window.upgrade_in_event_loop(|w| {
        w.set_feed_state(1);
        w.set_feed_error(Default::default());
        w.set_has_more(true);
        w.set_loading_more(false);
        w.set_more_error(Default::default());
        w.set_g_refreshing(false);
        w.set_g_pull(Default::default());
    });

    spawn_page_request(weak, Mode::Replace, /* quiet = */ false);
}

/// 下拉刷新。
///
/// 与冷启动的区别：**保留现有列表**，把新拉到的内容插到顶部。
/// 用户下拉时屏幕上本来就有内容，突然全白再慢慢长出来是最差的体验。
pub fn refresh_feed(window: slint::Weak<AppWindow>) {
    // 刷新从 fresh_idx = 0 重新开始，对应服务端的一批新推荐
    idx_store(0);

    let weak = window.clone();
    let _ = window.upgrade_in_event_loop(|w| {
        w.set_feed_error(Default::default());
        w.set_more_error(Default::default());
        w.set_g_refreshing(true);
    });

    spawn_page_request(weak, Mode::Prepend, /* quiet = */ true);
}

/// 加载下一页（滚动到底部触发）。
pub fn load_more(window: slint::Weak<AppWindow>) {
    // 正在加载就忽略（IN_FLIGHT 是进程级的，这里判断才有效）
    if IN_FLIGHT.load(Ordering::SeqCst) {
        return;
    }

    let _ = window.upgrade_in_event_loop(|w| {
        w.set_loading_more(true);
        w.set_more_error(Default::default());
    });

    spawn_page_request(window, Mode::Append, /* quiet = */ true);
}

/// 一页数据怎么合进当前列表。
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// 替换整个列表（冷启动首屏）
    Replace,
    /// 追加到末尾（滚动到底加载更多）
    Append,
    /// 插到最前面（下拉刷新）
    Prepend,
    /// 只在列表为空时才替换（冷启动的静默刷新；已有内容就不动）
    Refresh,
}

/// 统一的「拉一页数据」实现。
fn spawn_page_request(weak: slint::Weak<AppWindow>, mode: Mode, quiet: bool) {
    // compare_exchange 而不是先 load 再 store：两次 load_more 同时进来
    // 时，只有一个能抢到。这是进程级状态的正确用法。
    if IN_FLIGHT
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let fresh_idx = idx_load();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                IN_FLIGHT.store(false, Ordering::SeqCst);
                report_error(&weak, format!("创建运行时失败: {e}"), mode, quiet);
                return;
            }
        };

        let result = rt.block_on(async {
            let client = bli_protocol::BiliClient::new().map_err(|e| e.to_string())?;
            client
                .recommend_feed(fresh_idx, 20)
                .await
                .map_err(|e| e.to_string())
        });

        // 无论成败都必须放开，否则一次失败就永久卡死翻页
        IN_FLIGHT.store(false, Ordering::SeqCst);

        match result {
            Ok(page) => {
                // 翻页判据必须用**过滤前**的原始条数。
                // 用过滤后的 items.len() 会让"这一页广告很多"被误解成"到底了"，
                // 于是推荐流永远只有一页。见 RecommendPage::raw_count 的注释。
                let count = page.raw_count;
                if page.filtered_out > 0 {
                    crate::mxprobe::log_public(&format!(
                        "[Feed] fresh_idx={fresh_idx} 原始 {count} 条，过滤掉 {} 条，保留 {} 条",
                        page.filtered_out,
                        page.items.len()
                    ));
                }

                // 跨页去重：推荐流偶尔会把同一个视频重复推回来
                let mut fresh: Vec<bli_protocol::recommend::VideoItem> = Vec::new();
                {
                    let mut guard = match SEEN_BVIDS.lock() {
                        Ok(g) => g,
                        Err(e) => e.into_inner(),
                    };
                    let seen = guard.get_or_insert_with(HashSet::new);
                    for it in page.items {
                        if seen.insert(it.bvid.clone()) {
                            fresh.push(it);
                        }
                    }
                }

                // 游标递增（0,1,2,3…，与 PiliPlus RcmdController 一致）
                idx_store(fresh_idx + 1);

                let covers: Vec<(String, String)> = fresh
                    .iter()
                    .filter(|v| !v.cover_url.is_empty())
                    .map(|v| (v.bvid.clone(), v.cover_url.clone()))
                    .collect();

                let raws: Vec<RawItem> = fresh.iter().map(to_raw).collect();

                let _ = weak.upgrade_in_event_loop(move |w| {
                    let new_items: Vec<VideoItemData> =
                        raws.into_iter().map(RawItem::into_slint).collect();

                    // 当前列表快照：必须是已保存的 MODEL 实例，
                    // 不能读 w.get_rows()（值类型数组属性读出来的是副本）
                    let mut all: Vec<VideoRowPair> = Vec::with_capacity(64);
                    if let Some(ref em) = MODEL.with(|m| m.borrow().clone()) {
                        for i in 0..em.row_count() {
                            if let Some(r) = em.row_data(i) {
                                all.push(r);
                            }
                        }
                    }

                    match mode {
                        Mode::Replace => {
                            all = pair_rows(new_items);
                        }
                        Mode::Prepend => {
                            // 新内容插到顶部：先排新的一批，再接旧列表。
                            // 旧列表最后一行若是单卡（奇数），要重新整理 ——
                            // 直接拼接会留下一个"只有左卡"的空洞。
                            let mut merged = new_items;
                            for row in all.drain(..) {
                                merged.push(row.left);
                                if row.has_right {
                                    merged.push(row.right);
                                }
                            }
                            all = pair_rows(merged);
                        }
                        Mode::Append => {
                            let mut incoming = new_items.into_iter();
                            // 上一页最后一行可能只有左卡，新数据补到右侧
                            if all.last().is_some_and(|r| !r.has_right) {
                                if let Some(first) = incoming.next() {
                                    if let Some(last) = all.last_mut() {
                                        last.right = first;
                                        last.has_right = true;
                                    }
                                }
                            }
                            let rest: Vec<VideoItemData> = incoming.collect();
                            all.extend(pair_rows(rest));
                        }
                        Mode::Refresh => {
                            // 静默刷新：列表已有内容就完全不动，
                            // 免得用户正在看的时候列表突然整体换掉
                            if all.is_empty() {
                                all = pair_rows(new_items);
                            }
                        }
                    }

                    // 写盘要用，先拷一份扁平列表再 move
                    let cache_snapshot: Vec<bli_protocol::recommend::VideoItem> = all
                        .iter()
                        .flat_map(|row| {
                            let mut v = vec![row_to_item(&row.left)];
                            if row.has_right {
                                v.push(row_to_item(&row.right));
                            }
                            v
                        })
                        .collect();

                    let model = Rc::new(VecModel::from(all));
                    w.set_rows(ModelRc::from(model.clone()));
                    MODEL.with(|m| *m.borrow_mut() = Some(model));
                    ensure_cover_pump();

                    // 一页 20 条：拿满了说明后面还有，拿不满说明到底了。
                    // 用 raw_count（过滤前），不是过滤后的条数。
                    w.set_has_more(count >= 20);
                    w.set_loading_more(false);
                    w.set_g_refreshing(false);
                    w.set_g_pull(Default::default());
                    w.set_feed_state(2);
                    w.set_feed_error(Default::default());

                    spawn_batched_loader(covers);

                    // 整份列表写盘（不是只写本页）。
                    //
                    // 为什么要按合并后的完整列表写：缓存的作用是让下次
                    // 冷启动直接看到"上次关掉时的那屏内容"。只写本页的话，
                    // 翻到第 5 页关掉，下次打开只剩第 5 页那 20 条，比原来还少。
                    cache_write(&cache_snapshot);
                });


            }
            Err(e) => {
                log::warn!("推荐流加载失败: {e}");
                report_error(&weak, e, mode, quiet);
            }
        }
    });
}

/// 封面加载队列：拿到数据就按批请求，不做可见性判断。
///
/// 为什么不做可见性懒加载：
///   试过按「可见行范围」驱动，问题是 Slint 的 out property 派生值
///   没有可靠的变更通知，只能靠轮询，而轮询 + 每次新建 Timer
///   会堆出一堆互相打架的下载任务，最后一张图都出不来。
///
/// 现在的策略更朴素也更可靠（用户提出的分批思路）：
///   首屏一次只显示约 10 张卡，那就先加载前 10 张封面；
///   这批回来后，再加载接下来的 10 张，依次推进。
///   并发上限自然被控制住，且首屏出图最快。
const COVER_BATCH: usize = 10;

/// 把整批封面拆成 COVER_BATCH 大小的批次，顺序推进。
fn spawn_batched_loader(items: Vec<(String, String)>) {
    if items.is_empty() {
        return;
    }

    let batches: Vec<Vec<(String, String)>> = items
        .chunks(COVER_BATCH)
        .map(|c| c.to_vec())
        .collect();

    std::thread::spawn(move || {
        for batch in batches {
            let (tx, rx) = mpsc::channel();
            crate::cover::spawn_cover_loader(batch, tx);

            // 等这一批全部解完再开下一批。
            // rx 在发送端全部 drop 后会迭代结束。
            for decoded in rx {
                if let Ok(mut q) = COVER_RESULTS.lock() {
                    q.push(decoded);
                }
            }
        }
    });
}

/// 解码完成的封面队列。
///
/// **必须是全局 static，不能是 thread_local** ——
/// 写入发生在下载线程，读取在 UI 线程，thread_local 会让两边
/// 看到各自独立的实例，写入永远读不到。
/// 这个 bug 花了不少时间才定位，别改回去。
static COVER_RESULTS: std::sync::Mutex<Vec<crate::cover::DecodedCover>> =
    std::sync::Mutex::new(Vec::new());

thread_local! {
    /// 全局唯一的封面回填 Timer 是否已启动（仅 UI 线程使用，thread_local 合适）。
    static PUMP_STARTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 启动封面回填泵（幂等，只启动一次）。
///
/// **必须在 UI 线程调用**，且在窗口创建之后、事件循环启动之前。
/// 它内部会建 slint::Timer，Timer 绑定到创建线程的事件循环。
///
/// 每 16ms 检查队列，有新图就逐行 `set_row_data`。
pub fn ensure_cover_pump() {
    if PUMP_STARTED.with(|c| c.replace(true)) {
        return;
    }

    let timer = Rc::new(slint::Timer::default());
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(16),
        move || {
            // 一次性把队列取空
            let drained: Vec<crate::cover::DecodedCover> = match COVER_RESULTS.lock() {
                Ok(mut q) => std::mem::take(&mut *q),
                Err(_) => Vec::new(),
            };

            if drained.is_empty() {
                return;
            }

            let Some(model) = MODEL.with(|m| m.borrow().clone()) else {
                return;
            };

            for d in drained {
                let buffer =
                    SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&d.rgba, d.width, d.height);
                fill_cover(&model, &d.bvid, Image::from_rgba8(buffer));
            }
        },
    );

    // Timer 要活到进程结束
    std::mem::forget(timer);
}

thread_local! {
    /// 当前 rows 对应的 VecModel 实例。
    ///
    /// 必须持有实例本身，而不是每次 `get_rows()` 拿一个新的 ModelRc 包装 ——
    /// 对 `in-out property <[...]>` 这种值类型属性，每次读取都可能得到新包装，
    /// 对副本 `set_row_data` 不会反映到 UI。持有实例才能逐行回填封面。
    static MODEL: RefCell<Option<Rc<VecModel<VideoRowPair>>>> = const { RefCell::new(None) };
}

/// 在模型里找到 bvid 对应的卡片并填入封面。
fn fill_cover(model: &Rc<VecModel<VideoRowPair>>, bvid: &str, img: Image) {
    for i in 0..model.row_count() {
        let Some(mut pair) = model.row_data(i) else {
            continue;
        };
        let mut changed = false;

        if pair.left.id.as_str() == bvid {
            pair.left.cover = img.clone();
            changed = true;
        }
        if pair.has_right && pair.right.id.as_str() == bvid {
            pair.right.cover = img.clone();
            changed = true;
        }

        if changed {
            model.set_row_data(i, pair);
            return;
        }
    }
}

fn report_error(window: &slint::Weak<AppWindow>, msg: String, mode: Mode, quiet: bool) {
    let _ = window.upgrade_in_event_loop(move |w| {
        w.set_loading_more(false);
        w.set_g_refreshing(false);
        w.set_g_pull(Default::default());

        match mode {
            // 加载更多失败：列表保留，底部显示「上滑重试」
            Mode::Append => {
                w.set_more_error(msg.as_str().into());
            }
            // 刷新失败：列表里已经有东西的话不要整屏报错，
            // 只在底部留一句提示（用户还在看原来的内容）
            Mode::Prepend | Mode::Refresh if quiet => {
                w.set_more_error(msg.as_str().into());
            }
            _ => {
                w.set_feed_error(msg.as_str().into());
                w.set_feed_state(3);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(0), "00:00");
        assert_eq!(format_duration(-5), "00:00");
        assert_eq!(format_duration(40), "00:40");
        assert_eq!(format_duration(754), "12:34");
        assert_eq!(format_duration(3723), "1:02:03");
    }

    #[test]
    fn pairs_even_and_odd() {
        let mk = |id: &str| VideoItemData {
            id: id.into(),
            ..Default::default()
        };

        let rows = pair_rows(vec![mk("a"), mk("b")]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].has_right);

        let rows = pair_rows(vec![mk("a"), mk("b"), mk("c")]);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].has_right);
        assert!(!rows[1].has_right);
        assert_eq!(rows[1].left.id.as_str(), "c");

        assert!(pair_rows(vec![]).is_empty());
    }
}
