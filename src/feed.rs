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
// Slint 的 AppWindow 是单实例，用 thread_local 存请求状态最简单。
// 换多窗口时这里要改成按窗口 id 索引的 Map。
// ==============================================================================

thread_local! {
    /// 已经请求过封面的 bvid，避免重复下载
    static REQUESTED_COVERS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());

    /// 当前翻页游标。0 表示还没加载过任何一页。
    static FRESH_IDX: RefCell<u32> = const { RefCell::new(0) };

    /// 是否正在加载（首屏或下一页），防止重复触发
    static IN_FLIGHT: RefCell<bool> = const { RefCell::new(false) };

    /// bvid -> 封面URL，供懒加载时查
    static COVER_URLS: RefCell<std::collections::HashMap<String, String>> =
        RefCell::new(std::collections::HashMap::new());
}

/// 首屏加载 / 下拉刷新。
pub fn load_feed(window: slint::Weak<AppWindow>, _fresh_idx: u32) {
    // 刷新时把游标与去重表都清空，相当于重新开始
    FRESH_IDX.with(|c| *c.borrow_mut() = 0);
    REQUESTED_COVERS.with(|c| c.borrow_mut().clear());
    COVER_URLS.with(|c| c.borrow_mut().clear());

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

    spawn_page_request(weak, /* append = */ false);
}

/// 加载下一页（滚动到底部触发）。
pub fn load_more(window: slint::Weak<AppWindow>) {
    // 正在加载、或已经没有更多了，就直接忽略
    if IN_FLIGHT.with(|c| *c.borrow()) {
        return;
    }

    let _ = window.upgrade_in_event_loop(|w| {
        w.set_loading_more(true);
        w.set_more_error(Default::default());
    });

    spawn_page_request(window, /* append = */ true);
}

/// 统一的「拉一页数据」实现。
///
/// `append = false` 是首屏/刷新（替换整个列表），
/// `append = true` 是加载更多（追加到末尾）。
fn spawn_page_request(weak: slint::Weak<AppWindow>, append: bool) {
    if IN_FLIGHT.with(|c| *c.borrow()) {
        return;
    }
    IN_FLIGHT.with(|c| *c.borrow_mut() = true);

    let fresh_idx = FRESH_IDX.with(|c| *c.borrow());

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                IN_FLIGHT.with(|c| *c.borrow_mut() = false);
                report_error(&weak, format!("创建运行时失败: {e}"), append);
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

        IN_FLIGHT.with(|c| *c.borrow_mut() = false);

        match result {
            Ok(page) => {
                let count = page.items.len();

                // 记录 bvid -> 封面地址，供后续懒加载
                COVER_URLS.with(|m| {
                    let mut m = m.borrow_mut();
                    for it in &page.items {
                        m.insert(it.bvid.clone(), it.cover_url.clone());
                    }
                });

                // 收集封面清单，稍后在 UI 线程启动分批加载
                let covers: Vec<(String, String)> = page
                    .items
                    .iter()
                    .filter(|v| !v.cover_url.is_empty())
                    .map(|v| (v.bvid.clone(), v.cover_url.clone()))
                    .collect();

                let raws: Vec<RawItem> = page.items.iter().map(to_raw).collect();

                // 游标递增：0, 1, 2, 3 ...
                //
                // 与 PiliPlus RcmdController 完全一致（它 page++ 后直接当 fresh_idx）。
                // 早先想当然地「0 之后跳到 4」，结果第 2、3 页内容与首屏大量重复 ——
                // 服务端把 fresh_idx 当作内容游标，跳号就跳过了中间的推荐位。
                FRESH_IDX.with(|c| *c.borrow_mut() = fresh_idx + 1);

                let weak_for_ui = weak.clone();
                let _ = weak.upgrade_in_event_loop(move |w| {
                    let new_items: Vec<VideoItemData> =
                        raws.into_iter().map(RawItem::into_slint).collect();

                    if append {
                        // 追加：从**已保存的模型实例**读出内容，而不是 w.get_rows()。
                        // get_rows() 对值类型数组属性可能拿不到最新内容，
                        // 早期症状就是「加载更多之后列表反而回到第一页」——
                        // 因为读到的 existing 是空或旧的，只在它后面接了新数据。
                        let existing_model = MODEL.with(|m| m.borrow().clone());
                        let mut all: Vec<VideoRowPair> = Vec::with_capacity(32);
                        if let Some(ref em) = existing_model {
                            for i in 0..em.row_count() {
                                if let Some(r) = em.row_data(i) {
                                    all.push(r);
                                }
                            }
                        }

                        // 追加时要注意：上一页的最后一行可能只有左卡（奇数个），
                        // 这时新数据的第一条应该补到那一行的右侧，而不是另起一行。
                        let mut incoming = new_items.into_iter();
                        let needs_right = all.last().is_some_and(|r| !r.has_right);

                        if needs_right {
                            if let Some(first) = incoming.next() {
                                if let Some(last) = all.last_mut() {
                                    last.right = first;
                                    last.has_right = true;
                                }
                            }
                        }

                        let rest: Vec<VideoItemData> = incoming.collect();
                        all.extend(pair_rows(rest));

                        let model = Rc::new(VecModel::from(all));
                        w.set_rows(ModelRc::from(model.clone()));
                        MODEL.with(|m| *m.borrow_mut() = Some(model));
                        ensure_cover_pump();
                        w.set_loading_more(false);
                    } else {
                        let rows = pair_rows(new_items);
                        let model = Rc::new(VecModel::from(rows));
                        w.set_rows(ModelRc::from(model.clone()));
                        MODEL.with(|m| *m.borrow_mut() = Some(model));
                        ensure_cover_pump();
                        w.set_feed_state(2);
                        w.set_loading_more(false);
                        w.set_g_refreshing(false);
                        w.set_g_pull(Default::default());
                    }

                    // 一页 20 条，拿不满说明到底了
                    w.set_has_more(count >= 20);

                    // 开始下载这批封面（内部会按 10 张一批推进）
                    spawn_batched_loader(covers);
                });

                let _ = weak_for_ui;
            }
            Err(e) => {
                log::warn!("推荐流加载失败: {e}");
                report_error(&weak, e, append);
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

/// 封面下载 + 回填。
///
/// 关键：持有同一份 `Rc<VecModel>`，逐行 `set_row_data`。
/// 之前每次 `w.get_rows()` 取模型，对于值类型数组属性会拿到新包装，
/// 对副本 set_row_data 不反映到 UI —— 表现为只有前几张图能显示。
fn spawn_cover_pump(
    _window: slint::Weak<AppWindow>,
    model: Rc<VecModel<VideoRowPair>>,
    covers: Vec<(String, String)>,
) {
    let (tx, rx) = mpsc::channel::<crate::cover::DecodedCover>();
    crate::cover::spawn_cover_loader(covers, tx);

    let timer = Rc::new(slint::Timer::default());
    let timer_for_cb = timer.clone();

    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(16),
        move || {
            let mut disconnected = false;

            loop {
                match rx.try_recv() {
                    Ok(d) => {
                        let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                            &d.rgba, d.width, d.height,
                        );
                        fill_cover(&model, &d.bvid, Image::from_rgba8(buffer));
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }

            if disconnected {
                timer_for_cb.stop();
            }
        },
    );
}

fn report_error(window: &slint::Weak<AppWindow>, msg: String, append: bool) {
    let _ = window.upgrade_in_event_loop(move |w| {
        if append {
            // 加载更多失败：列表保留，底部显示可重试
            w.set_loading_more(false);
            w.set_more_error(msg.as_str().into());
        } else {
            w.set_feed_error(msg.as_str().into());
            w.set_feed_state(3);
            w.set_g_refreshing(false);
            w.set_g_pull(Default::default());
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
