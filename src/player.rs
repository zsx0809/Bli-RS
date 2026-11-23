// ==============================================================================
// 播放器与二级页面编排 (Player & Detail View Orchestration)
//
// 1. 切换至二级详情页 (app_mode = 1)，封面先垫底杜绝黑屏
// 2. 拉取视频详情 (video_detail) 并回填 UP 主、标题、点赞/投币/收藏数、横竖屏标识
// 3. 拉取相关视频流 (related_videos) 与真实评论列表 (video_comments)
// 4. 根据视频真实分辨率动态自适应 SurfaceView (横屏 16:9 / 竖屏 48%)
// 5. 拉取 DASH playurl -> 下载流 -> MediaCodec 硬解送显 + PTS 时间戳帧同步
// ==============================================================================

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use crate::{AppWindow, CommentItemData, RelatedVideoData};

/// 全局解码器句柄
static CURRENT: Mutex<Option<crate::video_decoder::VideoDecoder>> = Mutex::new(None);
/// 全局音频解码器句柄
static AUDIO_DECODER: Mutex<Option<crate::audio_decoder::AudioDecoder>> = Mutex::new(None);
/// 全局播放时钟（音频主时钟）。暂停/恢复要能拿到它。
static AUDIO_CLOCK: Mutex<Option<std::sync::Arc<crate::audio_out::PlayClock>>> = Mutex::new(None);
/// 当前视频流的 SharedBuffer。回前台重建解码器时复用同一份，
/// 这样后台期间下载的进度不会丢。
static CURRENT_BUFFER: Mutex<Option<std::sync::Arc<crate::stream_source::SharedBuffer>>> =
    Mutex::new(None);
/// 当前视频分辨率（宽, 高）。全屏 letterbox 计算要用。
static VIDEO_SIZE: Mutex<Option<(i32, i32)>> = Mutex::new(None);
/// 全局播放会话序列号，杜绝多任务并发争抢 Surface 与 Codec 冲突
static PLAY_SESSION: AtomicU64 = AtomicU64::new(0);

/// 当前视频的 aid / bvid / UP mid。
///
/// 登录状态变化后要重新拉评论区（未登录 3 条，登录后 20 条/页），
/// 那时播放上下文早就散在栈上了，只能靠这几个全局量。
static CURRENT_AID: AtomicU64 = AtomicU64::new(0);
/// 当前视频的 cid。切换清晰度时要重新请求 playurl，那时得知道 cid。
static CURRENT_CID: AtomicU64 = AtomicU64::new(0);
static CURRENT_OWNER: AtomicU64 = AtomicU64::new(0);
static CURRENT_BVID: Mutex<Option<String>> = Mutex::new(None);
/// 当前视频总时长（微秒）。UI 传来的进度是 0.0~1.0，换算成 PTS 要用它。
/// 0 表示未知（此时 seek 直接忽略，避免跳到片头）。
static DURATION_US: AtomicI64 = AtomicI64::new(0);

/// 「自动播放关闭」时暂存的待播流信息。
///
/// 用户点播放键时用它启动，省掉重新请求 playurl（那要 WBI 签名 + 一次
/// 网络往返，体验上会明显卡一下）。
struct PendingStream {
    session: u64,
    bvid: String,
    url: String,
    audio_url: Option<String>,
    duration_us: i64,
}

static PENDING_STREAM: Mutex<Option<PendingStream>> = Mutex::new(None);

/// 用户手动指定的清晰度 id（0 = 自动）。
///
/// 放在全局而不是参数里：清晰度是"这一路播放的偏好"，
/// 用户在详情页点一下就要立刻重播生效，那时 play() 早已返回，
/// 拿不到局部变量。
static DESIRED_QN: AtomicU64 = AtomicU64::new(0);

/// 当前这次播放实际拿到的视频高度（用于 UI 回显）
static ACTUAL_HEIGHT: AtomicU64 = AtomicU64::new(0);

/// 当前视频的清晰度候选（id, 文案），供 UI 菜单用
static QUALITY_LIST: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());

/// 用户在清晰度菜单里选中的**下标**（0 = 自动）。
///
/// 为什么存下标而不是 qid：不同视频支持的档位不同（这个视频有 1080P，
/// 那个只有 720P），qid 直接套到别的视频上会指向不存在的档位。
/// 下标也存在同一份菜单结构上（labels 的下标），语义一致。
///
/// 持久化到 settings 的键 `quality_index` —— 重启 App 后要还原，
/// 否则用户每次打开都得重选一次。
static QUALITY_LIST_INDEX: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// 相关推荐视频模型句柄（仅在 UI 线程访问，杜绝跨线程 Send 冲突）
    static RELATED_MODEL: RefCell<Option<Rc<VecModel<RelatedVideoData>>>> = const { RefCell::new(None) };
}

/// 全局纯数据相关推荐列表（跨线程安全，自动连播用它避免 thread_local 无法访问）
static CURRENT_RELATED: Mutex<Vec<RawRelated>> = Mutex::new(Vec::new());
/// 已连播历史（防止两视频相互推荐陷入死循环）
static PLAYED_HISTORY: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 纯数据详情包装，满足跨线程 Send 契约
struct RawDetail {
    bvid: String,
    title: String,
    desc: String,
    author: String,
    views: String,
    danmaku: String,
    likes: String,
    coins: String,
    favorites: String,
    shares: String,
    pubdate: String,
    is_vertical: bool,
    up_fans: String,
    up_videos: String,
}

/// 纯数据相关视频包装
#[derive(Clone)]
struct RawRelated {
    bvid: String,
    title: String,
    author: String,
    views: String,
    danmaku: String,
    duration: String,
    cover_url: String,
}

/// 纯数据评论包装
struct RawComment {
    rpid: u64,
    user_name: String,
    user_face: String,
    content: String,
    likes: String,
    pubtime: String,
    level: u32,
    is_vip: bool,
    is_up: bool,
    location: String,
    reply_count: u32,
    sub_authors: Vec<String>,
    sub_contents: Vec<String>,
}


/// 把播放链路失败原因写进 UI。
///
/// 为什么必须有这一步：早期所有失败分支都只 `log` 然后 `return`，
/// 界面上永远停在"视频加载中…"。用户看到的是一个不动也不报错的播放器，
/// 唯一出路是手动返回，体感就是"点了一下没反应，然后自己跳了下一个视频"。
fn report_fail(window: &slint::Weak<AppWindow>, msg: String) {
    crate::mxprobe::log_public(&format!("[播放失败] {msg}"));
    // 视频层是窗口表面之上的独立图层，盖着的时候播放区里什么都看不见。
    // 失败时把它调全透明，否则用户只会看到一块黑，读不到下面这段错误文案。
    // （set_visible(false) 走的是 alpha=0，不是 INVISIBLE —— Surface 不会被销毁。）
    crate::video_surface::set_visible(false);
    let _ = window.upgrade_in_event_loop(move |w| {
        let mut d = w.get_current_detail();
        d.is_ready = false;
        d.load_error = msg.as_str().into();
        w.set_current_detail(d);
    });
}

// ==============================================================================
// 前后台切换
//
// 用户切换前后的诉求不同，必须区别对待：
//   前台：画面 + 声音都要，且音画同步
//   后台：**只要声音**。视频画面没人看，继续解码白白耗电；而且 Surface
//         在后台多半已被系统销毁，往失效 Surface 送帧没有任何意义。
//
// 因此核心策略是：
//   进后台 → 停视频解码器（保留音频解码器与下载）
//   回前台 → 重建 Surface，再起视频解码器，并以音频时钟为基准续播
//
// 为什么"回前台能自然接上"：
//   音画同步本来就是每帧读 audio_clock.position_us() 再决定等/追。
//   新建的视频解码器从关键帧开始，第一帧 PTS 多数情况下早于音频位置，
//   于是走"落后 >300ms 就丢帧"的路径快速追上，之后逐帧对齐。
//   不需要任何额外的"重新同步"代码。
// ==============================================================================

/// 当前是否处于后台（后台时视频解码器应为 None）
static IN_BACKGROUND: AtomicBool = AtomicBool::new(false);

/// 由 lib.rs 的生命周期监听调用。`kind` 是事件名，只做关键字匹配。
pub fn on_lifecycle(kind: &str) {
    let to_background = kind.contains("Pause") || kind.contains("Stop");
    let to_foreground = kind.contains("Resume") || kind.contains("Start");

    if to_background && !IN_BACKGROUND.swap(true, Ordering::SeqCst) {
        crate::mxprobe::log_public("[Lifecycle] 进入后台：停视频解码，保留音频");

        // 停视频解码器 + 把视频层调透明。
        // 注意**不清** PENDING_STREAM、不动 PLAY_SESSION ——
        // 会话仍然是同一个，回前台要接着播。
        if let Ok(mut cur) = CURRENT.lock() {
            if let Some(mut d) = cur.take() {
                d.stop();
            }
        }
        crate::video_surface::set_visible(false);

        // 音频继续：确保它不是暂停状态（用户可能是在播放中切的后台）
        // 只有用户显式暂停过才该保持暂停，这里不强行 resume。
    } else if to_foreground && IN_BACKGROUND.swap(false, Ordering::SeqCst) {
        crate::mxprobe::log_public("[Lifecycle] 回到前台：重建视频");

        // 没有音频时钟 = 当前根本没在播放（已停止/已退出详情页），
        // 此时不该凭空恢复视频。
        let has_clock = AUDIO_CLOCK.lock().ok().and_then(|g| g.clone()).is_some();
        if !has_clock {
            crate::mxprobe::log_public("[Lifecycle] 无活动播放会话，不恢复视频");
            return;
        }

        resume_video_after_background();
    }
}

/// 回前台后重建视频链路（Surface → 解码器），音频不受影响。
fn resume_video_after_background() {
    // 取保存的视频缓冲（起播时存进去的）。
    // 用同一份 SharedBuffer 意味着：如果后台期间下载还在继续，
    // 恢复后解码器能直接读到新增的数据，不需要重新下载。
    let buffer = CURRENT_BUFFER.lock().ok().and_then(|g| g.clone());
    let Some(buffer) = buffer else {
        crate::mxprobe::log_public("[Lifecycle] 没有视频缓冲，无法恢复");
        return;
    };

    let session = PLAY_SESSION.load(Ordering::SeqCst);

    std::thread::spawn(move || {
        // 给系统一点时间重建 Surface
        std::thread::sleep(std::time::Duration::from_millis(300));

        if PLAY_SESSION.load(Ordering::SeqCst) != session {
            return;
        }

        let Some(surface) = wait_surface() else {
            crate::mxprobe::log_public("[Lifecycle] 恢复失败：Surface 未就绪");
            return;
        };

        let clock = AUDIO_CLOCK.lock().ok().and_then(|g| g.clone());

        crate::video_surface::set_visible(true);
        crate::mxprobe::log_public("[Lifecycle] 重建视频解码器，跟随音频时钟");

        let dec = crate::video_decoder::VideoDecoder::start(
            buffer,
            clock,
            surface,
            || {
                crate::mxprobe::log_public("[Lifecycle] 视频已恢复送显");
            },
            |e| {
                crate::mxprobe::log_public(&format!("[Lifecycle] 恢复后解码失败: {e}"));
            },
        );

        if let Ok(mut cur) = CURRENT.lock() {
            *cur = Some(dec);
        }
    });
}

/// 停止当前播放并隐藏视频层
pub fn stop_play() {
    PLAY_SESSION.fetch_add(1, Ordering::SeqCst);
    stop_current();
    // 用 alpha=0 隐藏，不销毁 Surface。
    // 曾经这里走 INVISIBLE，结果返回后再进下一个视频 Surface 再也 attach
    // 不回来（isValid 恒 false），一路刷到「等待渲染表面超时」。
    //
    // 也不要在这里把 View 高度改成 0 来"收起"—— 高度归零同样会让系统
    // 销毁 Surface，是和 INVISIBLE 一模一样的坑。
    // 保持原有尺寸、只调 alpha，是唯一能留住 Surface 的做法。
    crate::video_surface::hide_surface();
}

/// 播放指定 bvid 并切换到二级页面。
///
/// `auto_play == false` 时：只拉详情 + 封面 + 评论 + 相关推荐 + 播放地址，
/// **不启动下载、不碰 Surface、不创建解码器**，界面停在封面 + 播放按钮。
/// 用户点播放后再走 `start_playback()`，体验和 B 站网页版一致。
pub fn play(window: slint::Weak<AppWindow>, bvid: String, auto_play: bool) {
    let my_session = PLAY_SESSION.fetch_add(1, Ordering::SeqCst) + 1;
    // 新会话先把时长清零：seek 在时长未知时会安全忽略，
    // 总比拿上一个视频的时长算出一个荒谬的跳转点强。
    DURATION_US.store(0, Ordering::SeqCst);
    if let Ok(mut g) = CURRENT_BVID.lock() {
        *g = Some(bvid.clone());
    }
    if let Ok(mut h) = PLAYED_HISTORY.lock() {
        if !h.contains(&bvid) {
            if h.len() > 50 {
                h.remove(0);
            }
            h.push(bvid.clone());
        }
    }

    // 清理上一个视频的流缓存 —— 必须在 stop_current() **之后**：
    // 解码器持有文件，不关掉删不掉（Linux 下 unlink 虽然能成功，
    // 但 inode 要等句柄全关才真正释放，空间不会立刻回来）。
    //
    // 这一步是「自动连播不占存储」的关键：切下一个视频的瞬间，
    // 上一个的 MB 级缓存就没了，磁盘占用恒定在当前这一个视频的量级。
    // 同一个 bvid 重进（循环播放 / 重播）时 retain_only 会把它留下，
    // 于是不用重新下载。
    stop_current();
    crate::cache::retain_only(&bvid);

    // 注意：这里**不**解除系统手势排除区。
    // 解除会让左边缘侧滑被系统吃掉并 finish 掉 NativeActivity，
    // 表现就是"左边侧滑直接退出 App"。
    // 详情页的边缘返回由 UI 侧的 EdgeBack 手势处理，不交给系统。

    // 1. UI 立即切入详情页，并初始化为加载状态 (封面垫底)
    let w_clone = window.clone();
    let bvid_initial = bvid.clone();
    let _ = window.upgrade_in_event_loop(move |w| {
        w.set_app_mode(1);
        // 根据是否自动播放设置等待起播状态
        w.set_awaiting_play(!auto_play);
        w.set_playback_started(false);
        let mut detail = w.get_current_detail();
        detail.bvid = bvid_initial.into();
        detail.title = "正在获取视频数据…".into();
        detail.desc = "".into();
        detail.is_ready = false;
        detail.is_vertical = false;
        detail.load_error = "".into();
        w.set_current_detail(detail);
        // 清空前次评论
        w.set_video_comments(ModelRc::new(Rc::new(VecModel::default())));
    });

    // 2. 准备 SurfaceView 并立即确保其可见且具备有效尺寸（杜绝 Surface 未分配与 isValid=false）
    crate::video_surface::run_on_ui_thread(|| {
        crate::video_surface::setup_on_main_thread();
        crate::video_surface::set_visible(true);
        crate::video_surface::set_adaptive_layout(false);
    });

    // 3. 后台请求与播放编排
    std::thread::spawn(move || {
        crate::mxprobe::log_public(&format!("=== 开启二级详情与播放 session={my_session} {bvid} ==="));

        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                report_fail(&w_clone, format!("初始化运行时失败: {e}"));
                return;
            }
        };

        // 用全局 client —— 它身上带着登录态。
        // 自己 new 一个就是游客身份，playurl 只会给 360P/480P。
        let client = match crate::client() {
            Some(c) => c,
            None => {
                report_fail(&w_clone, "初始化网络失败".into());
                return;
            }
        };

        // 3.1 拉取视频详情
        let detail_res = rt.block_on(client.video_detail(&bvid));
        let (aid, cid, _is_vertical, face_url, pic_url, owner_mid) = match detail_res {
            Ok(d) => {
                let aid = d.aid;
                let cid = d.cid;
                let owner_mid = d.owner_mid;
                let is_vert = d.is_vertical;
                let face = d.owner_face.clone();
                let pic = d.pic.clone();

                // 粉丝数 / 投稿数：单独请求，失败就不显示（不拖累播放）
                let (follower, video_cnt) = rt.block_on(client.up_stat(d.owner_mid));

                let raw = RawDetail {
                    views: d.views_str(),
                    danmaku: d.danmaku_str(),
                    likes: d.likes_str(),
                    coins: d.coins_str(),
                    favorites: d.favorites_str(),
                    shares: d.shares_str(),
                    pubdate: d.pubdate_str(),
                    bvid: d.bvid,
                    title: d.title,
                    desc: d.desc,
                    author: d.owner_name,
                    is_vertical: is_vert,
                    up_fans: follower
                        .map(|n| format!("{}粉", fmt_count(n)))
                        .unwrap_or_default(),
                    up_videos: video_cnt
                        .map(|n| format!("{}视频", fmt_count(n)))
                        .unwrap_or_default(),
                };

                let w_detail = w_clone.clone();
                let _ = w_detail.upgrade_in_event_loop(move |w| {
                    let mut cur = w.get_current_detail();
                    cur.bvid = raw.bvid.into();
                    cur.title = raw.title.into();
                    cur.desc = raw.desc.into();
                    cur.author = raw.author.into();
                    cur.views = raw.views.into();
                    cur.danmaku = raw.danmaku.into();
                    cur.likes = raw.likes.into();
                    cur.coins = raw.coins.into();
                    cur.favorites = raw.favorites.into();
                    cur.shares = raw.shares.into();
                    cur.pubdate = raw.pubdate.into();
                    cur.is_vertical = raw.is_vertical;
                    cur.up_fans = raw.up_fans.into();
                    cur.up_videos = raw.up_videos.into();
                    cur.is_liked = false;
                    cur.is_coined = false;
                    cur.is_favorited = false;
                    w.set_current_detail(cur);
                });

                // 按横竖屏自适应调整 SurfaceView 尺寸 (横屏 16:9 / 竖屏 48%)
                crate::video_surface::set_adaptive_layout(is_vert);

                (aid, cid, is_vert, face, pic, owner_mid)
            }
            Err(e) => {
                report_fail(&w_clone, format!("获取视频信息失败: {e}"));
                return;
            }
        };

        // 异步下载封面图并垫底 (彻底杜绝黑屏)
        if !pic_url.is_empty() {
            let w_pic = w_clone.clone();
            std::thread::spawn(move || {
                let rt_pic = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(r) => r,
                    Err(_) => return,
                };
                if let Some((w, h, rgba)) = rt_pic.block_on(download_raw_image(&pic_url)) {
                    let _ = w_pic.upgrade_in_event_loop(move |w_ui| {
                        let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                        let mut cur = w_ui.get_current_detail();
                        cur.cover = Image::from_rgba8(buf);
                        w_ui.set_current_detail(cur);
                    });
                }
            });
        }

        // 异步下载 UP 头像并回填
        if !face_url.is_empty() {
            let w_face = w_clone.clone();
            std::thread::spawn(move || {
                let rt_img = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(r) => r,
                    Err(_) => return,
                };
                if let Some((w, h, rgba)) = rt_img.block_on(download_raw_image(&face_url)) {
                    let _ = w_face.upgrade_in_event_loop(move |w_ui| {
                        let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                        let mut cur = w_ui.get_current_detail();
                        cur.author_face = Image::from_rgba8(buf);
                        w_ui.set_current_detail(cur);
                    });
                }
            });
        }

        // 3.2 异步拉取真实评论列表
        // 记住当前视频身份，登录态变化时要靠它重刷评论
        CURRENT_AID.store(aid, Ordering::SeqCst);
        CURRENT_CID.store(cid, Ordering::SeqCst);
        CURRENT_OWNER.store(owner_mid, Ordering::SeqCst);
        if let Ok(mut g) = CURRENT_BVID.lock() {
            *g = Some(bvid.clone());
        }

        if aid > 0 {
            spawn_comment_load(w_clone.clone(), client.clone(), aid, owner_mid, my_session);
        }

        // 3.3 拉取相关视频推荐
        let rel_res = rt.block_on(client.related_videos(&bvid));
        match rel_res {
            Ok(related_list) => {
                crate::mxprobe::log_public(&format!(
                    "[Player] 成功获取相关推荐，共 {} 条",
                    related_list.len()
                ));
                let raw_rel: Vec<RawRelated> = related_list
                    .into_iter()
                    .map(|r| RawRelated {
                        bvid: r.bvid,
                        title: r.title,
                        author: r.author,
                        views: r.views,
                        danmaku: r.danmaku,
                        duration: r.duration,
                        cover_url: r.cover_url,
                    })
                    .collect();

                // 存一份全局纯数据，供后台轮询线程自动连播时安全访问
                if let Ok(mut g) = CURRENT_RELATED.lock() {
                    *g = raw_rel.clone();
                }

                // 相关推荐的封面：**全部**都要下。
                //
                // 之前是 .take(12) —— 那是「详情页推荐流只有十来个封面、
                // 剩下的全是灰块」的直接原因。当时大概是想着「先下够首屏」，
                // 但推荐列表本来就只有二三十条，省这点请求毫无意义，
                // 反而让人以为列表坏了。
                //
                // 现在改成全部交给 cover 模块的并发加载器（6 路并发 + 磁盘缓存），
                // 首屏那次进详情页会把整列都填满；第二次进同一个视频时
                // 全部命中本地缓存，一个网络请求都不发。
                // 封面清单必须在 raw_rel 被 move 进下面那个闭包**之前**取出来
                let cover_items: Vec<(String, String)> = raw_rel
                    .iter()
                    .filter(|r| !r.cover_url.is_empty())
                    .map(|r| (r.bvid.clone(), r.cover_url.clone()))
                    .collect();

                let w_rel = w_clone.clone();
                let _ = w_rel.upgrade_in_event_loop(move |w| {
                    let ui_list: Vec<RelatedVideoData> = raw_rel
                        .into_iter()
                        .map(|r| RelatedVideoData {
                            bvid: r.bvid.as_str().into(),
                            title: r.title.as_str().into(),
                            author: r.author.as_str().into(),
                            views: r.views.as_str().into(),
                            danmaku: r.danmaku.as_str().into(),
                            duration: r.duration.as_str().into(),
                            cover: Image::default(),
                            cover_url: r.cover_url.as_str().into(),
                        })
                        .collect();

                    let model = Rc::new(VecModel::from(ui_list));
                    w.set_related_videos(ModelRc::new(model.clone()));
                    RELATED_MODEL.with(|m| *m.borrow_mut() = Some(model));
                });

                if !cover_items.is_empty() {
                    let (tx, rx) = std::sync::mpsc::channel::<crate::cover::DecodedCover>();
                    crate::cover::spawn_cover_loader(cover_items, tx);

                    let w_cov_ui = w_clone.clone();
                    std::thread::spawn(move || {
                        for d in rx {
                            if PLAY_SESSION.load(Ordering::SeqCst) != my_session {
                                return;
                            }
                            let w_ui = w_cov_ui.clone();
                            let bvid = d.bvid.clone();

                            let _ = w_ui.upgrade_in_event_loop(move |_| {
                                RELATED_MODEL.with(|cell| {
                                    let Some(ref m) = *cell.borrow() else {
                                        return;
                                    };
                                    for i in 0..m.row_count() {
                                        let Some(mut row) = m.row_data(i) else {
                                            continue;
                                        };
                                        if row.bvid.as_str() != bvid {
                                            continue;
                                        }
                                        let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                                            &d.rgba, d.width, d.height,
                                        );
                                        row.cover = Image::from_rgba8(buf);
                                        m.set_row_data(i, row);
                                        break;
                                    }
                                });
                            });
                        }
                    });
                }
            }
            Err(e) => {
                crate::mxprobe::log_public(&format!("[Player] 获取相关推荐失败: {e}"));
            }
        }

        if PLAY_SESSION.load(Ordering::SeqCst) != my_session {
            crate::mxprobe::log_public(&format!("[Session] 会话 {my_session} 已失效，退出"));
            return;
        }

        // 3.4 拿播放地址与媒体流
        let info = match rt.block_on(client.play_url(&bvid, cid)) {
            Ok(v) => v,
            Err(e) => {
                report_fail(&w_clone, format!("获取播放地址失败: {e}"));
                return;
            }
        };

        if PLAY_SESSION.load(Ordering::SeqCst) != my_session {
            crate::mxprobe::log_public(&format!("[Session] 会话 {my_session} 已失效，退出"));
            return;
        }

        // 挑轨道 + 起播。抽成函数是因为切换清晰度也要走同一条路
        //（但那条路不该重拉详情/评论/相关推荐）。
        start_from_playinfo(
            w_clone.clone(),
            my_session,
            bvid.clone(),
            client.clone(),
            info,
            auto_play,
        );
    });
}

/// 从磁盘恢复用户的清晰度选择。启动时调一次。
///
/// 两个值分开存：
///   quality_qn    —— 档位 id（127/80/64/…），0 表示自动
///   quality_index —— 菜单下标，用来还原 UI 上的选中态
///
/// 为什么都要：qid 用来真正挑轨道；下标用来让菜单显示"当前选中哪一项"。
/// 只有 qid 的话，UI 不知道 80 对应菜单里第几行（每个视频档位不同）。
pub fn restore_quality_pref() {
    let qn = crate::settings::get_u64("quality_qn");
    let idx = crate::settings::get_u64("quality_index");
    DESIRED_QN.store(qn, Ordering::SeqCst);
    QUALITY_LIST_INDEX.store(idx, Ordering::SeqCst);
    if qn > 0 || idx > 0 {
        crate::mxprobe::log_public(&format!(
            "[Quality] 恢复偏好: qn={qn} index={idx}"
        ));
    }
}

/// 拿到 playurl 结果后：挑轨道 → 更新清晰度菜单 → 起播（或暂存待播）。
///
/// 从 `play()` 里抽出来，是因为「切换清晰度」也需要这个流程 ——
/// 但它**不该**连带重拉详情、评论、相关推荐（那会让用户在切档位时
/// 看到标题变回"正在获取视频数据…"、评论区清空重来）。
///
/// 参数 `cid` 只为日志与 audio 选择的一致性；真正的轨道挑选全在 `info` 里。
fn start_from_playinfo(
    window: slint::Weak<AppWindow>,
    my_session: u64,
    bvid: String,
    client: bli_protocol::BiliClient,
    info: bli_protocol::playurl::DashPlayInfo,
    auto_play: bool,
) {
    let w_clone = window.clone();

    let desired = DESIRED_QN.load(Ordering::SeqCst);

    // 先把候选档位存起来给 UI（无论最后选了哪个）
    let options = info.quality_options();
    if let Ok(mut g) = QUALITY_LIST.lock() {
        *g = options.clone();
    }
    {
        let w_q = w_clone.clone();
        let labels: Vec<String> = std::iter::once("自动".to_string())
            .chain(options.iter().map(|(_, l)| l.clone()))
            .collect();
        // UI 侧的索引是 labels 的下标；用户选了哪一项也按下标回传。
        // 记住用户选的那一档，重启后要还原。
        let sel = QUALITY_LIST_INDEX.load(Ordering::SeqCst) as i32;
        let _ = w_q.upgrade_in_event_loop(move |w| {
            let model: Vec<slint::SharedString> =
                labels.iter().map(|s| s.as_str().into()).collect();
            w.set_quality_options(slint::ModelRc::new(slint::VecModel::from(model)));
            w.set_quality_index(sel);
        });
    }

    let picked = if desired > 0 {
        info.pick_video_by_id(desired)
            .or_else(|| info.pick_video(Some(1080)))
    } else {
        info.pick_video(Some(1080))
    };

    let Some(vtrack) = picked else {
        report_fail(&w_clone, "该视频没有可用的视频流（可能是充电专属或地区限制）".into());
        return;
    };

    ACTUAL_HEIGHT.store(vtrack.height.unwrap_or(0) as u64, Ordering::SeqCst);
    crate::mxprobe::log_public(&format!(
        "[Player] 清晰度: 请求 {desired} -> 实际 {}P (id={})",
        vtrack.height.unwrap_or(0),
        vtrack.id
    ));
    let Some(url) = vtrack.primary_url().map(|s| s.to_string()) else {
        report_fail(&w_clone, "视频流地址为空".into());
        return;
    };

    let audio_url = info
        .audio
        .iter()
        .find(|t| t.primary_url().is_some())
        .and_then(|t| t.primary_url().map(|s| s.to_string()));

    if let Some(a) = info.pick_audio() {
        crate::mxprobe::log_public(&format!(
            "[Player] 选中音轨: {} {} {}bps",
            a.id, a.codecs, a.bandwidth
        ));
    } else {
        crate::mxprobe::log_public("[Player] 没有可用音轨，将静音播放");
    }

    crate::mxprobe::log_public(&format!(
        "[Player] 选中分辨率: {}x{} 编码: {}",
        vtrack.width.unwrap_or(0),
        vtrack.height.unwrap_or(0),
        vtrack.codecs
    ));

    let vw = vtrack.width.unwrap_or(0) as i32;
    let vh = vtrack.height.unwrap_or(0) as i32;
    if let Ok(mut g) = VIDEO_SIZE.lock() {
        *g = Some((vw, vh));
    }
    crate::video_surface::set_video_size(vw as u32, vh as u32);

    let dur_us = info.duration.saturating_mul(1_000_000) as i64;
    DURATION_US.store(dur_us, Ordering::SeqCst);
    crate::mxprobe::log_public(&format!("[Player] 总时长: {}s", info.duration));

    if !auto_play {
        crate::mxprobe::log_public("[Player] 自动播放已关闭，等待用户手动起播");
        if let Ok(mut pending) = PENDING_STREAM.lock() {
            *pending = Some(PendingStream {
                session: my_session,
                bvid: bvid.clone(),
                url: url.clone(),
                audio_url: audio_url.clone(),
                duration_us: dur_us,
            });
        }
        let w_wait = w_clone.clone();
        let _ = w_wait.upgrade_in_event_loop(move |w| {
            w.set_awaiting_play(true);
        });
        return;
    }

    start_playback(window, my_session, bvid, url, audio_url, client, dur_us);
}

/// 真正启动播放：下载视频流 → 等 Surface → 起解码器 → 回填进度。
///
/// 从 `play()` 里抽出来，是因为「自动播放关闭」时需要在**用户点击之后**
/// 才走这条路。抽出来之前它是内联在 play 的闭包里的，没法二次调用。
///
/// 参数 `client` 要传进来而不是内部新建：playurl 已经拿过一次，
/// 这里直接用同一份 client 下载流，省掉重复的 WBI 签名握手。
fn start_playback(
    window: slint::Weak<AppWindow>,
    my_session: u64,
    bvid: String,
    url: String,
    audio_url: Option<String>,
    client: bli_protocol::BiliClient,
    duration_us: i64,
) {
    let w_clone = window.clone();

    // ==================================================================
    // 边下边播（真正的流式解码）
    // ==================================================================
    //
    // 关键：数据源不再是一个固定长度的 fd，而是 AMediaDataSource +
    // 自定义 readAt 回调（见 src/stream_source.rs）。
    //
    //   · 下载线程边下边写文件，同时推进 SharedBuffer.available
    //   · 解码线程的 readAt 要数据时，如果还没下到那里就阻塞等待
    //   · getSize() 在下载中返回 -1（长度未知），下完返回真实长度
    //
    // 于是解码器"跟着下载进度走"：下载慢它等，下载快它追。
    // 一小时的视频也能在首段就绪后立即起播，不需要等整段下完。
    //
    // 缓存策略见 src/cache.rs：同一时刻磁盘上只留**当前这一个**视频的
    // 音视频流。清理发生在 play() 里（切换视频的瞬间，旧解码器已停），
    // 所以循环 / 重播同一个 bvid 时文件还在，不会重新下载。
    let path = crate::cache::stream_path(&bvid, 'v');
    let path_str = path.to_string_lossy().to_string();

    // 残留的半截文件必须删掉：它会被当成完整视频解析。
    let _ = std::fs::remove_file(&path);

    // 先问出总长度。
    // 视频流：边下边播（SharedBuffer + AMediaDataSource 流式解码）。
    //
    // spawn_stream_download 内部会先用 Range: bytes=0-0 探出总长度，
    // 因为 AMediaExtractor 的解封装阶段会调 getSize()，
    // 返回 -1（未知）会让它一直卡住不返回（实测）。
    let (buffer, rx_ready) = spawn_stream_download(
        "视频",
        client.clone(),
        url.clone(),
        path.clone(),
        1_500_000,
    );

    crate::mxprobe::log_public("[Player] 启动边下边播（AMediaDataSource 流式解码）…");

    // 音频流：同样走流式数据源。
    //
    // 音频文件小得多（192K 码率下一小时约 86MB，但首段阈值只要 64KB
    // 就够解出前几个 AAC 帧），所以起播几乎立刻可用。
    let audio_parts = audio_url.as_ref().map(|au| {
        let apath = crate::cache::stream_path(&bvid, 'a');
        let (abuf, arx) = spawn_stream_download(
            "音频",
            client.clone(),
            au.clone(),
            apath,
            64_000,
        );
        (abuf, arx)
    });

    // 等首段就绪（12 秒超时）。注意这里**只等首段**，不等整个文件。
    match rx_ready.recv_timeout(std::time::Duration::from_secs(12)) {
        Ok(()) => {
            crate::mxprobe::log_public("[Player] 首段就绪，立即起播（后台继续下载）");
        }
        Err(_) => {
            // 首段没等到：可能是弱网，也可能彻底失败。让解码器自己再等一会，
            // 这里只记录，不直接判死 —— readAt 会继续阻塞等待下载推进。
            crate::mxprobe::log_public("[Player] 首段等待超时，交由解码器流式等待");
        }
    }

    if PLAY_SESSION.load(Ordering::SeqCst) != my_session {
        crate::mxprobe::log_public(&format!("[Session] 会话 {my_session} 已失效，退出"));
        return;
    }

    // 3.6 等 Surface 就绪
    let Some(surface) = wait_surface() else {
        report_fail(&w_clone, "等待渲染表面超时".into());
        return;
    };
    crate::mxprobe::log_public(&format!("[Player] Surface 就绪: {surface:?}"));

    if PLAY_SESSION.load(Ordering::SeqCst) != my_session {
        crate::mxprobe::log_public(&format!("[Session] 会话 {my_session} 已失效，放弃起播"));
        return;
    }

    // 3.7 展现视频层并启动硬解
    //
    // 「听视频」开着时不显示画面。解码器照常起（音频要靠它），
    // 只是视频层保持透明。
    let audio_only = crate::settings::get_bool("audio_only", false);
    if audio_only {
        crate::mxprobe::log_public("[AudioOnly] 听视频模式：不显示画面");
    } else {
        crate::video_surface::set_visible(true);
    }

    // ---- 音频：先于视频启动 ----
    //
    // 为什么音频要先起：
    //   音频是主时钟。视频解码器启动后会读 audio_clock.position_us() 来
    //   决定每帧是等还是追。如果音频时钟不存在，视频就只能退化成
    //   "按自己的 PTS 自由跑"，一旦解码抖动就会与声音漂移。
    //   先起音频，时钟从第一刻就是准的。
    let shared_clock = if let Some((abuf, arx)) = audio_parts {
        // 等音频首段（比视频快得多，64KB 阈值通常 <0.5 秒）
        match arx.recv_timeout(std::time::Duration::from_secs(8)) {
            Ok(()) => crate::mxprobe::log_public("[Audio] 首段就绪"),
            Err(_) => crate::mxprobe::log_public("[Audio] 首段等待超时，继续尝试解码"),
        }

        // ---- 按音轨真实格式打开 AAudio ----
        let (sr, ch) = crate::audio_decoder::probe_format(abuf.clone());
        crate::mxprobe::log_public(&format!("[Audio] 按真实格式打开输出: {sr}Hz {ch}ch"));

        match crate::audio_out::AudioOutput::open(sr as u32, ch as u32) {
            Ok(ao) => {
                let ao = std::sync::Arc::new(ao);
                let clock = ao.clock.clone();

                // 初始让音频处于对齐等待状态，等视频第一帧上屏时解除暂停，音画绝对同步齐发
                if !audio_only {
                    clock.set_paused(true);
                }

                let a_err = move |e: String| {
                    crate::mxprobe::log_public(&format!("[Audio] 解码失败: {e}"));
                };

                let adec = crate::audio_decoder::AudioDecoder::start(abuf, ao.clone(), a_err);
                if let Ok(mut cur) = AUDIO_DECODER.lock() {
                    *cur = Some(adec);
                }
                if let Ok(mut g) = AUDIO_CLOCK.lock() {
                    *g = Some(clock.clone());
                }
                Some(clock)
            }
            Err(e) => {
                crate::mxprobe::log_public(&format!("[Audio] 打开输出失败: {e}（将静音播放）"));
                None
            }
        }
    } else {
        crate::mxprobe::log_public("[Audio] 无音轨，静音播放");
        None
    };

    let w_ready = w_clone.clone();
    let w_err = w_clone.clone();

    // 保存缓冲句柄，供回前台重建解码器时复用
    if let Ok(mut g) = CURRENT_BUFFER.lock() {
        *g = Some(buffer.clone());
    }

    crate::mxprobe::log_public(&format!("[Player] 启动解码器: session={my_session}, path={path_str}"));
    let clock_for_first_frame = shared_clock.clone();
    let dec = crate::video_decoder::VideoDecoder::start(
        buffer,
        shared_clock.clone(),
        surface,
        move || {
            crate::mxprobe::log_public("[Player] 首帧成功送显，通知 UI 淡出封面占位图，音频同步开跑");
            if let Some(ref clk) = clock_for_first_frame {
                clk.set_paused(false);
            }
            let _ = w_ready.upgrade_in_event_loop(|w| {
                let mut d = w.get_current_detail();
                d.is_ready = true;
                w.set_current_detail(d);
                w.set_playback_started(true);
            });
        },
        move |e| {
            crate::mxprobe::log_public(&format!("[Player] 解码失败通知: {e}"));
            report_fail(&w_err, format!("视频解码失败: {e}"));
        },
    );

    // 音频超时兜底：最多等视频首帧 1.5 秒，若视频首帧仍未解出，自动恢复音频播放
    if let Some(ref clk) = shared_clock {
        let clk_timeout = clk.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            clk_timeout.set_paused(false);
        });
    }

    if let Ok(mut cur) = CURRENT.lock() {
        *cur = Some(dec);
    }

    // ---- 进度回填 ----
    //
    // 解码线程每帧更新 last_pts_us，这里用 250ms 定时器搬运到 UI。
    // 为什么不每帧推：30fps 意味着每秒 30 次跨线程 event_loop 调用，
    // 在低端机上会和渲染抢 UI 线程。进度条人眼只需要 ~4Hz。
    spawn_progress_poller(w_clone.clone(), my_session, duration_us);
}

/// 评论分页状态。
///
/// PiliPlus 用一个 nextOffset 字符串 + isEnd 标志维护（见
/// lib/pages/common/reply/reply_controller.dart:handlePagination）。
/// 我们走 REST，登录态下用 pn 翻页，未登录用 pagination_str 偏移，
/// 两种都要记，所以两个都存。
#[derive(Default)]
struct CommentPage {
    /// 已加载到第几页（登录路径用）
    pn: u32,
    /// 下一页的 offset（未登录路径用）
    next_offset: String,
    /// 还有没有更多
    has_more: bool,
    /// 服务端报的总条数
    total: u64,
    /// 排序：0 = 按时间，1 = 按热度
    sort: u32,
}

static COMMENT_STATE: Mutex<CommentPage> = Mutex::new(CommentPage {
    pn: 0,
    next_offset: String::new(),
    has_more: false,
    total: 0,
    sort: 1,
});

/// 拉评论第一页并上屏。登录 / 退出 / 发评论之后都调它重刷。
///
/// 为什么要整份重拉而不是本地补一条：
///   未登录时接口只给 3 条热门评论，而且 reply_control 里没有 location
///   （IP 属地）。登录之后同一条评论会带属地，列表也变成 20 条/页。
///   本地插入会漏掉服务端算好的字段（相对时间、等级徽章、楼中楼预置）。
pub fn spawn_comment_load(
    window: slint::Weak<AppWindow>,
    client: bli_protocol::BiliClient,
    aid: u64,
    owner_mid: u64,
    session: u64,
) {
    // 重置分页游标（保留当前排序）：这是"从头加载"
    let keep_sort = COMMENT_STATE.lock().map(|g| g.sort).unwrap_or(1);
    if let Ok(mut g) = COMMENT_STATE.lock() {
        *g = CommentPage::default();
        g.sort = keep_sort;
    }
    spawn_comment_page(window, client, aid, owner_mid, session, true);
}

/// 加载下一页（下拉到底部时调）。
pub fn load_more_comments(client: bli_protocol::BiliClient) {
    let aid = CURRENT_AID.load(Ordering::SeqCst);
    if aid == 0 {
        return;
    }
    let owner = CURRENT_OWNER.load(Ordering::SeqCst);
    let session = PLAY_SESSION.load(Ordering::SeqCst);

    let has_more = COMMENT_STATE.lock().map(|g| g.has_more).unwrap_or(false);
    if !has_more {
        crate::mxprobe::log_public("[Comment] 没有更多评论了");
        return;
    }

    let Some(window) = crate::login::global_window() else {
        return;
    };
    // 通知 UI 进入"加载中"
    if let Some(w) = window.upgrade() {
        w.set_comments_loading_more(true);
    }
    spawn_comment_page(window, client, aid, owner, session, false);
}

/// 真正干活的分页加载。
///
/// `fresh` 为 true 表示覆盖（第一页），false 表示追加。
fn spawn_comment_page(
    window: slint::Weak<AppWindow>,
    client: bli_protocol::BiliClient,
    aid: u64,
    owner_mid: u64,
    session: u64,
    fresh: bool,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(r) => r,
            Err(_) => return,
        };

        let auth = crate::login::current();
        let is_login = auth.as_ref().map(|a| a.is_logged_in()).unwrap_or(false);

        // 取出本轮要用的游标与排序
        let (pn, next_offset, sort) = {
            let Ok(g) = COMMENT_STATE.lock() else {
                return;
            };
            if fresh {
                (1u32, String::new(), g.sort)
            } else {
                (g.pn + 1, g.next_offset.clone(), g.sort)
            }
        };

        let json = match rt.block_on(bli_protocol::auth::comments_page(
            &client.raw_http(),
            auth.as_ref(),
            aid,
            sort,
            pn,
            &next_offset,
        )) {
            Ok(j) => j,
            Err(e) => {
                crate::mxprobe::log_public(&format!("[Comment] 第 {pn} 页拉取失败: {e}"));
                if let Some(w) = window.upgrade() {
                    w.set_comments_loading_more(false);
                }
                return;
            }
        };

        if PLAY_SESSION.load(Ordering::SeqCst) != session {
            return;
        }

        let code = json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = json
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("未知错误");
            crate::mxprobe::log_public(&format!("[Comment] 第 {pn} 页返回 code={code}: {msg}"));
            if let Some(w) = window.upgrade() {
                w.set_comments_loading_more(false);
            }
            return;
        }

        let items = bli_protocol::auth::parse_replies(&json, owner_mid);
        let (has_more, next_off) = bli_protocol::auth::parse_page_state(&json, pn);
        let total = json["data"]["page"]["count"].as_u64().unwrap_or(0);

        {
            let Ok(mut g) = COMMENT_STATE.lock() else {
                return;
            };
            g.pn = pn;
            g.next_offset = next_off;
            g.has_more = has_more;
            if total > 0 {
                g.total = total;
            }
        }

        crate::mxprobe::log_public(&format!(
            "[Comment] 已登录={is_login} 第 {pn} 页 {} 条，累计更多={has_more} 总 {total}",
            items.len()
        ));

        let face_urls: Vec<String> = items.iter().map(|c| c.user_face.clone()).collect();

        // 注意：CommentItemData 里含 ModelRc，ModelRc **不是 Send**，
        // 不能在后台线程构造。这里只把纯数据的 RawComment 送过去，
        // 到 UI 线程再转成模型。
        let raw: Vec<RawComment> = items
            .into_iter()
            .map(|c| RawComment {
                rpid: c.rpid,
                sub_authors: c.sub_replies.iter().map(|s| s.user_name.clone()).collect(),
                sub_contents: c.sub_replies.iter().map(|s| s.content.clone()).collect(),
                user_name: c.user_name,
                user_face: c.user_face,
                content: c.content,
                likes: c.likes,
                pubtime: c.pubtime,
                level: c.level,
                is_vip: c.is_vip,
                is_up: c.is_up,
                location: c.location,
                reply_count: c.reply_count,
            })
            .collect();

        let w_list = window.clone();
        let _ = window.upgrade_in_event_loop(move |w| {
            if fresh {
                let rows: Vec<CommentItemData> = raw.into_iter().map(raw_to_ui).collect();
                let model = VecModel::from(rows);
                w.set_video_comments(ModelRc::new(Rc::new(model)));
            } else {
                // 追加：读出现有行、拼上新行、整份重设。
                // Slint 的 VecModel 交给 ModelRc 后没有"push 一行"的安全入口，
                // set_row_data 只能改已有行。20 条/页时重建代价是几毫秒。
                let model = w.get_video_comments();
                let mut merged: Vec<CommentItemData> =
                    Vec::with_capacity(model.row_count() + raw.len());
                for i in 0..model.row_count() {
                    if let Some(row) = model.row_data(i) {
                        merged.push(row);
                    }
                }
                merged.extend(raw.into_iter().map(raw_to_ui));
                let new_model = VecModel::from(merged);
                w.set_video_comments(ModelRc::new(Rc::new(new_model)));
            }

            w.set_comments_loading_more(false);
            let (has_more, total) = COMMENT_STATE
                .lock()
                .map(|g| (g.has_more, g.total))
                .unwrap_or((false, 0));
            w.set_comments_has_more(has_more);
            // 标题显示的必须是**服务端总数**，不是已加载条数。
            // 用已加载条数会变成"下拉一次、数字涨一次"，看起来像故障。
            if total > 0 {
                w.set_comments_total(total as i32);
            }
        });

        // 头像按新行的起始索引回填
        let base = if fresh {
            0
        } else {
            window.upgrade().map(|w| w.get_video_comments().row_count()).unwrap_or(0)
                .saturating_sub(face_urls.len())
        };
        spawn_comment_faces_from(w_list, face_urls, session, base);
    });
}

/// 切换评论排序并重新拉第一页。
///
/// sort: 0 = 按时间（最新在前），1 = 按热度。
///
/// 为什么必须有这个入口：
///   按热度排序时，刚发的评论几乎排在最后，用户发完往下翻几十页
///   都看不到，会以为没发出去（实测用户就是这么反馈的）。
///   切到"按时间"就能在第一页看到自己刚发的。
pub fn set_comment_sort(client: bli_protocol::BiliClient, sort: i32) {
    let sort = if sort == 0 { 0u32 } else { 1u32 };
    {
        let Ok(mut g) = COMMENT_STATE.lock() else {
            return;
        };
        g.sort = sort;
    }
    crate::mxprobe::log_public(&format!(
        "[Comment] 切换排序 -> {}",
        if sort == 0 { "最新" } else { "最热" }
    ));

    let aid = CURRENT_AID.load(Ordering::SeqCst);
    if aid == 0 {
        return;
    }
    let owner = CURRENT_OWNER.load(Ordering::SeqCst);
    let session = PLAY_SESSION.load(Ordering::SeqCst);
    let Some(window) = crate::login::global_window() else {
        return;
    };
    spawn_comment_load(window, client, aid, owner, session);
}

/// 登录态变化后重新拉当前视频的评论。
pub fn reload_comments(client: bli_protocol::BiliClient, _bvid: String) {
    let aid = CURRENT_AID.load(Ordering::SeqCst);
    let owner = CURRENT_OWNER.load(Ordering::SeqCst);
    let session = PLAY_SESSION.load(Ordering::SeqCst);
    if aid == 0 {
        return;
    }
    let Some(window) = crate::login::global_window() else {
        return;
    };
    spawn_comment_load(window, client, aid, owner, session);
}

/// 当前正在播放/浏览的 bvid（登录后重刷评论用）。
pub fn current_bvid() -> String {
    CURRENT_BVID
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default()
}

/// 当前视频 aid。
pub fn current_aid() -> u64 {
    CURRENT_AID.load(Ordering::SeqCst)
}

/// 按昵称反查评论 rpid，供 reply/add 定位。
///
/// 为什么 UI 只传昵称、由这里反查：
///   Slint 的 callback 参数是值语义，把 rpid 直接传出去要多加一个
///   callback 重载；而且用户点"回复"时的目标是**看得见的昵称**，
///   用昵称当入口更符合意图。真实 rpid 从模型里读 —— 昵称会重名，
///   但反查走的是"最近一次加载的列表"，同一页里重名极少见。
///
/// 找不到就返回 (0,0)：那就是发一条顶层评论，不会发错人。
pub fn resolve_reply_target(name: &str) -> (u64, u64) {
    let Some(window) = crate::login::global_window() else {
        return (0, 0);
    };
    let Some(w) = window.upgrade() else {
        return (0, 0);
    };
    let model = w.get_video_comments();
    for i in 0..model.row_count() {
        if let Some(row) = model.row_data(i) {
            if row.user_name == name {
                let rpid = row.rpid as u64;
                // root 和 parent 都传它：B 站一层回复的语义是
                // root=父评论 rpid, parent=被回复评论 rpid。
                // 回复一级评论时两者相同。
                return (rpid, rpid);
            }
        }
    }
    crate::mxprobe::log_public(&format!("[Comment] 找不到 {name} 的 rpid，按顶层评论发送"));
    (0, 0)
}

/// RawComment → UI 模型行。
///
/// 必须是**顶层函数**，不能写成闭包。
/// 原因：闭包内定义的闭包会被判成"捕获了外层闭包的环境"，
/// 而外层是 upgrade_in_event_loop 的参数、要求 Send；
/// Slint 的 Image/ModelRc 内部有 *mut () 的 PhantomData，不是 Send，
/// 于是整个外层闭包被判成不可 Send，编译期直接报错。
fn raw_to_ui(c: RawComment) -> CommentItemData {
    let sub_authors: Vec<slint::SharedString> =
        c.sub_authors.iter().map(|s| s.as_str().into()).collect();
    let sub_replies: Vec<slint::SharedString> =
        c.sub_contents.iter().map(|s| s.as_str().into()).collect();
    CommentItemData {
        rpid: c.rpid as i32,
        user_name: c.user_name.into(),
        user_face: Image::default(),
        content: c.content.into(),
        likes: c.likes.into(),
        pubtime: c.pubtime.into(),
        level: c.level as i32,
        is_vip: c.is_vip,
        is_up: c.is_up,
        location: c.location.into(),
        reply_count: c.reply_count as i32,
        sub_replies: ModelRc::new(VecModel::from(sub_replies)),
        sub_authors: ModelRc::new(VecModel::from(sub_authors)),
    }
}

/// 评论头像批量下载并逐条回填。
///
/// 为什么要单独一个函数、还要带 session：
///   用户在评论区翻着，切走了视频，这些下载中的头像必须能自我终止，
///   否则会往已经换掉的列表里写数据（表现为新视频评论区混进旧头像）。
/// 评论头像批量下载并逐条回填。
///
/// `index_of` 把 urls 的下标映射成模型行号。
///
/// 为什么需要映射而不是直接用 urls 的下标：
///   第一页时模型行号 == urls 下标；
///   但"加载更多"是往同一个模型追加，第二页的第一个头像应该写进
///   第 20 行，不是第 0 行。上一版直接拿 enumerate 的下标去
///   set_row_data，翻页后头像会全串到前 20 条上。
///
/// 用闭包而不是 base 整数：调用方可以在 UI 线程里按当时的
/// row_count 动态算，避免"下载期间又追加了一页"导致错位。
fn spawn_comment_faces_from(
    window: slint::Weak<AppWindow>,
    urls: Vec<String>,
    session: u64,
    base: usize,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(r) => r,
            Err(_) => return,
        };

        for (idx, url) in urls.iter().enumerate() {
            if url.is_empty() {
                continue;
            }
            // 会话失效就别再往旧列表写了
            if PLAY_SESSION.load(Ordering::SeqCst) != session {
                return;
            }

            let Some((w, h, rgba)) = rt.block_on(download_raw_image(url)) else {
                continue;
            };

            let target = base + idx;
            let w_ui = window.clone();
            let _ = w_ui.upgrade_in_event_loop(move |ui| {
                // 通过 AppWindow 重新取当前模型，避免跨线程持有 ModelRc
                let model = ui.get_video_comments();
                if target < model.row_count() {
                    if let Some(mut row) = model.row_data(target) {
                        let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                        row.user_face = Image::from_rgba8(buf);
                        model.set_row_data(target, row);
                    }
                }
            });
        }
    });
}

/// 启动一个流式下载任务，返回（可读缓冲、首段就绪接收端）。
///
/// 音视频共用这套逻辑：探长度 → 建 SharedBuffer → 带续传重试的后台下载。
/// 抽出来之前这段代码在 start_playback 里内联，接音轨时无处安放。
fn spawn_stream_download(
    label: &'static str,
    client: bli_protocol::BiliClient,
    url: String,
    path: std::path::PathBuf,
    first_chunk_threshold: u64,
) -> (
    std::sync::Arc<crate::stream_source::SharedBuffer>,
    std::sync::mpsc::Receiver<()>,
) {
    // 探总长度（只花 1 字节）
    let total_len = {
        match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt.block_on(client.probe_content_length(&url)),
            Err(_) => None,
        }
    };
    match total_len {
        Some(n) => crate::mxprobe::log_public(&format!(
            "[{label}] 总长度 {:.1} MB",
            n as f64 / 1_048_576.0
        )),
        None => crate::mxprobe::log_public(&format!("[{label}] 未能探测总长度")),
    }

    // ---- 命中完整缓存就不下载 ----
    //
    // 判据：磁盘上已有文件的大小 == 远端 Content-Length。
    //
    // 为什么这个判据是可靠的：文件名是 v_<bvid>.m4s，bvid 唯一确定一个视频；
    // 同一个 bvid 的播放地址（playurl 签名）会变，但内容是同一份流。
    // 长度完全相等说明上次已经下完，直接当成品用。
    //
    // 意义：循环播放 / 播完重播 / 退出详情页再进来，这几种最常见的场景
    // 全部变成「零流量 + 首帧立刻出来」。这正是用户要的那条缓存策略。
    //
    // 注意长度对不上时**必须删除**再重下：半截文件会被当成完整视频解析，
    // 表现为播到某个点画面冻住（extractor 读到文件尾就发 EOS）。
    let cached_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if let Some(total) = total_len {
        if cached_len == total && total > 0 {
            crate::mxprobe::log_public(&format!(
                "[{label}] 命中完整缓存 {:.1} MB，跳过下载",
                total as f64 / 1_048_576.0
            ));
            let buffer = crate::stream_source::SharedBuffer::new(path.clone(), total_len);
            buffer.finish(total);
            let (tx_ready, rx_ready) = std::sync::mpsc::sync_channel::<()>(1);
            // 立刻发"首段就绪"，让起播流程不用等那 12 秒超时
            let _ = tx_ready.send(());
            return (buffer, rx_ready);
        }
    }

    let _ = std::fs::remove_file(&path);
    let buffer = crate::stream_source::SharedBuffer::new(path.clone(), total_len);

    let (tx_ready, rx_ready) = std::sync::mpsc::sync_channel::<()>(1);
    let buffer_dl = buffer.clone();
    let client_dl = client;
    let url_dl = url;
    let path_dl = path;

    std::thread::spawn(move || {
        let rt_dl = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(r) => r,
            Err(e) => {
                crate::mxprobe::log_public(&format!("[{label}] 运行时创建失败: {e}"));
                buffer_dl.fail();
                return;
            }
        };

        // 带断点续传的重试循环。
        //
        // CDN 长连接会中途掐断（error decoding response body）。
        // 一小时的视频要连续下几十 MB，单次请求几乎不可能不断。
        const MAX_RETRY: u32 = 6;
        let mut retry = 0u32;
        let mut last_size = 0u64;
        let mut stall = 0u32;

        loop {
            let start_size = path_dl.metadata().map(|m| m.len()).unwrap_or(0);
            let tx2 = tx_ready.clone();

            let res = rt_dl.block_on(client_dl.download_stream_to_file_from(
                &url_dl,
                &path_dl,
                start_size,
                first_chunk_threshold,
                move || {
                    let _ = tx2.send(());
                },
            ));

            match res {
                Ok(_written) => {
                    let total = path_dl.metadata().map(|m| m.len()).unwrap_or(0);
                    crate::mxprobe::log_public(&format!(
                        "[{label}] 下载完成 {:.1} MB",
                        total as f64 / 1_048_576.0
                    ));
                    buffer_dl.finish(total);
                    return;
                }
                Err(e) => {
                    let now_size = path_dl.metadata().map(|m| m.len()).unwrap_or(0);
                    let delta = now_size.saturating_sub(buffer_dl.available_now());
                    if delta > 0 {
                        buffer_dl.advance(delta);
                    }

                    if now_size > last_size {
                        stall = 0;
                        last_size = now_size;
                    } else {
                        stall += 1;
                    }

                    retry += 1;
                    crate::mxprobe::log_public(&format!(
                        "[{label}] 中断({e})，已下载 {:.1} MB，第 {retry} 次重试",
                        now_size as f64 / 1_048_576.0
                    ));

                    if retry >= MAX_RETRY || stall >= 3 {
                        crate::mxprobe::log_public(&format!("[{label}] 重试耗尽，放弃"));
                        buffer_dl.fail();
                        return;
                    }

                    std::thread::sleep(std::time::Duration::from_millis(500 * retry as u64));
                }
            }
        }
    });

    (buffer, rx_ready)
}

/// 播完了：按用户设置在「暂停 / 单个循环 / 自动连播」之间选择。
///
/// 优先级（后者的前提是前者不成立）：
///   1. 单个循环  → 重播同一个视频
///   2. 播完暂停  → 停在最后一帧，什么都不做
///   3. 自动连播  → 播相关推荐的第一条
///   4. 都没有    → 同样停在末尾
///
/// 为什么「单个循环」优先于「自动连播」：
///   用户开了循环就是想反复看这一个，这时还跳下一个明显违背意图。
///   UI 侧已经让 loop_single / loop_list 互斥，这里只处理优先级。
fn on_playback_ended(window: slint::Weak<AppWindow>) {
    let loop_single = crate::settings::get_bool("loop_single", false);
    let loop_list = crate::settings::get_bool("loop_list", false);
    let pause_at_end = crate::settings::get_bool("pause_at_end", false);
    let auto_next = crate::settings::get_bool("auto_next", true);

    crate::mxprobe::log_public(&format!(
        "[End] 播放结束 loop_single={loop_single} loop_list={loop_list} pause={pause_at_end} auto_next={auto_next}"
    ));

    let cur_bvid = CURRENT_BVID.lock().ok().and_then(|g| g.clone());
    // 四个开关在 UI 侧已经互斥，这里按"选中了哪一个"直接分派。
    //
    // 用 if/else if 而不是一堆并列 if + ||：
    //   互斥是**约束**，不是巧合。写成并列的 `if auto_next || loop_list`
    //   会让后来读代码的人以为这两项可以同开，进而把互斥逻辑改坏。
    //   这里保持"只可能命中一个分支"的结构，约束在代码里可见。
    if loop_single {
        match cur_bvid {
            Some(bvid) => {
                crate::mxprobe::log_public("[End] 单个循环：重播当前视频");
                // 循环**必须**真的起播，不能尊重 auto_play。
                play(window, bvid, true);
            }
            None => crate::mxprobe::log_public("[End] 单个循环：但不知道当前 bvid，停在末尾"),
        }
    } else if pause_at_end {
        crate::mxprobe::log_public("[End] 播完暂停");
        let _ = window.upgrade_in_event_loop(|w| {
            w.set_play_paused(true);
        });
    } else if auto_next || loop_list {
        let kind = if loop_list { "列表循环" } else { "自动连播" };

        let candidate_bvid = {
            let mut list = CURRENT_RELATED.lock().ok().map(|g| g.clone()).unwrap_or_default();
            // 如果相关推荐列表还没准备好，最多等待 1.5 秒
            if list.is_empty() {
                for _ in 0..5 {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    list = CURRENT_RELATED.lock().ok().map(|g| g.clone()).unwrap_or_default();
                    if !list.is_empty() {
                        break;
                    }
                }
            }
            let history = PLAYED_HISTORY.lock().ok().map(|g| g.clone()).unwrap_or_default();

            // 优先找：非当前视频，且不在已播历史里的第一个
            let unplayed = list.iter().find(|r| {
                Some(&r.bvid) != cur_bvid.as_ref() && !r.bvid.is_empty() && !history.contains(&r.bvid)
            });

            if let Some(r) = unplayed {
                Some(r.bvid.clone())
            } else if loop_list || auto_next {
                // 如果全都播过了，或者开启了列表循环：找非当前视频的第一个
                list.iter()
                    .find(|r| Some(&r.bvid) != cur_bvid.as_ref() && !r.bvid.is_empty())
                    .map(|r| r.bvid.clone())
            } else {
                None
            }
        };

        match candidate_bvid {
            Some(next_bvid) => {
                crate::mxprobe::log_public(&format!("[End] {kind} -> {next_bvid}"));
                if let Ok(mut h) = PLAYED_HISTORY.lock() {
                    if h.len() > 50 {
                        h.remove(0);
                    }
                    h.push(next_bvid.clone());
                }
                play(window, next_bvid, true);
            }
            None => {
                crate::mxprobe::log_public(&format!("[End] {kind}：相关推荐为空，停在末尾"));
            }
        }
    } else {
        crate::mxprobe::log_public("[End] 未选中任何续播方式，停在末尾");
    }
}

/// 播放进度搬运线程：解码器 PTS → Slint 的 progress / 时间文案。
fn spawn_progress_poller(window: slint::Weak<AppWindow>, session: u64, duration_us: i64) {
    if duration_us <= 0 {
        return;
    }
    let total_secs = duration_us / 1_000_000;

    // 总时长立刻上屏。
    //
    // 之前是等第一帧 PTS 出来才设，结果解码前的几帧里 UI 一直显示
    // "00:00 / 00:00"，看起来像是没读到时长。总时长本来就已知，
    // 没必要等。
    {
        let w0 = window.clone();
        let total_text = fmt_time(total_secs);
        let _ = w0.upgrade_in_event_loop(move |ui| {
            ui.set_play_time_total(total_text.into());
        });
    }

    std::thread::spawn(move || {
        let mut handled_end = false;
        let mut poller_notified_ready = false;
        loop {
            if PLAY_SESSION.load(Ordering::SeqCst) != session {
                return;
            }

            let (pts, ended) = {
                match CURRENT.lock() {
                    Ok(cur) => match cur.as_ref() {
                        Some(d) => (d.controls.last_pts_us(), d.controls.has_ended()),
                        None => return,
                    },
                    Err(_) => return,
                }
            };

            let is_at_end = duration_us > 0 && pts >= duration_us.saturating_sub(600_000);
            // 防误触保护：当收到 ended 时，必须确认当前进度真正接近片尾（距末尾 2s 内或播完 85% 以上，或未知时长），
            // 绝不允许中途网络波动导致的异常假 EOS 误判为播放结束触发切片
            let valid_end = is_at_end
                || (ended
                    && (duration_us <= 0
                        || pts >= duration_us.saturating_sub(2_000_000)
                        || (duration_us > 0 && pts >= duration_us * 85 / 100)));

            // ---- 播放结束：执行用户设置的动作 ----
            //
            // 只在第一次进入时处理（handled_end 保证），否则 250ms 一次
            // 的轮询会把"自动连播"触发无数次。
            if valid_end && !handled_end {
                handled_end = true;
                crate::mxprobe::log_public(&format!(
                    "[End] 触发播放结束处理 (ended={ended}, pts={}ms, dur={}ms)",
                    pts / 1000,
                    duration_us / 1000
                ));
                let w_end = window.clone();
                std::thread::spawn(move || {
                    on_playback_ended(w_end);
                });
            } else if ended && !handled_end {
                crate::mxprobe::log_public(&format!(
                    "[Player] 异常提前收到 EOS (pts={}ms, dur={}ms)，忽略假结束",
                    pts / 1000,
                    duration_us / 1000
                ));
            }

            // 注意：pts == 0 是**合法值**（首帧就从 0 开始），
            // 只有负数才是"还没有数据"，那才跳过。
            if pts < 0 {
                std::thread::sleep(std::time::Duration::from_millis(250));
                continue;
            }

            // 【UI 封面淡出兜底保障】
            // 只要已经产生非负有效 PTS（解码器正在出帧），若 UI 侧封面还没淡出，主动补位触发
            if !poller_notified_ready {
                poller_notified_ready = true;
                let w_ready = window.clone();
                let _ = w_ready.upgrade_in_event_loop(|ui| {
                    let mut d = ui.get_current_detail();
                    if !d.is_ready {
                        d.is_ready = true;
                        ui.set_current_detail(d);
                    }
                    ui.set_playback_started(true);
                });
            }

            let frac = (pts as f64 / duration_us as f64).clamp(0.0, 1.0) as f32;
            let cur_secs = (pts / 1_000_000).max(0);
            let _ = window.upgrade_in_event_loop(move |ui| {
                ui.set_play_progress(frac);
                ui.set_play_time_current(fmt_time(cur_secs).into());
            });

            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    });
}

/// 大数字转中文习惯写法（12.3万 / 1.2亿）
fn fmt_count(n: u64) -> String {
    if n >= 100_000_000 {
        format!("{:.1}亿", n as f64 / 100_000_000.0)
    } else if n >= 10_000 {
        format!("{:.1}万", n as f64 / 10_000.0)
    } else {
        n.to_string()
    }
}

/// 秒 → mm:ss（超过 1 小时用 h:mm:ss）
fn fmt_time(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// 点赞状态切换 (即时乐观更新)
pub fn toggle_like(window: slint::Weak<AppWindow>) {
    let _ = window.upgrade_in_event_loop(|w| {
        let mut d = w.get_current_detail();
        d.is_liked = !d.is_liked;
        w.set_current_detail(d);
    });
}

/// 投币状态切换 (即时乐观更新)
pub fn toggle_coin(window: slint::Weak<AppWindow>) {
    let _ = window.upgrade_in_event_loop(|w| {
        let mut d = w.get_current_detail();
        d.is_coined = true;
        w.set_current_detail(d);
    });
}

/// 收藏状态切换 (即时乐观更新)
pub fn toggle_fav(window: slint::Weak<AppWindow>) {
    let _ = window.upgrade_in_event_loop(|w| {
        let mut d = w.get_current_detail();
        d.is_favorited = !d.is_favorited;
        w.set_current_detail(d);
    });
}

/// 异步下载并解码为原始 RGBA 纯数据 (Send 友好)
/// 同步版图片下载（给自己的后台线程用，不需要外部传 runtime）。
///
/// 登录头像用：那个线程只下一张图，为它传一个 runtime 进来太啰嗦。
pub fn fetch_rgba_blocking(url: &str) -> Option<(u32, u32, Vec<u8>)> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(download_raw_image(url))
}

async fn download_raw_image(url: &str) -> Option<(u32, u32, Vec<u8>)> {
    let target = crate::cover::thumbnail_url(url);
    if target.is_empty() {
        return None;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;

    let resp = client
        .get(&target)
        .header("referer", "https://www.bilibili.com")
        .header("user-agent", crate::cover::UA)
        .send()
        .await
        .ok()?;

    let bytes = resp.bytes().await.ok()?;
    let decoded = image::load_from_memory(&bytes).ok()?;
    let rgba = decoded.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((w, h, rgba.into_raw()))
}

fn wait_surface() -> Option<*mut std::ffi::c_void> {
    for i in 0..50 {
        if let Some(s) = crate::video_surface::acquire_native_window() {
            return Some(s);
        }

        // 到第 5 次（约 0.75 秒）还是拿不到，说明 Surface 已经被系统销毁
        // 而 setVisibility 不会重建它 —— 必须强制重新 attach 走一遍
        // surfaceCreated 回调。这是"第二次进详情页黑屏"的修法。
        if i == 5 {
            crate::mxprobe::log_public("[Surface] 未就绪，强制重新 attach");
            crate::video_surface::force_reattach_surface();
        } else if i == 15 || i == 25 || i == 35 {
            crate::video_surface::set_visible(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    None
}

fn stop_current() {
    if let Ok(mut cur) = CURRENT.lock() {
        if let Some(mut d) = cur.take() {
            d.stop();
        }
    }
    // 音频同步停止，否则返回后声音还在响
    if let Ok(mut cur) = AUDIO_DECODER.lock() {
        if let Some(mut d) = cur.take() {
            d.stop();
        }
    }
    if let Ok(mut g) = AUDIO_CLOCK.lock() {
        // 先 kill 再摘掉。
        //
        // 只置 None 是不够的：视频解码器启动时拷贝过一份 Arc，
        // 那份拷贝不受这里影响，会继续读到冻结的时间。
        // kill() 之后 position_us() 返回 -1，视频侧就退回内部时钟了。
        if let Some(clk) = g.as_ref() {
            clk.kill();
        }
        *g = None;
    }
    if let Ok(mut g) = CURRENT_BUFFER.lock() {
        if let Some(buf) = g.take() {
            buf.cancel();
        }
    }
}

// ==============================================================================
// 播放交互：进度 / 亮度 / 音量 / 播放暂停
//
// 参数统一是 0.0~1.0 的归一化值，由 UI 手势层算好后回调进来。
//
// 当前实现状态（诚实标注，避免下一个人误以为全通）：
//   · set_brightness / set_volume —— 已完整落地，走系统服务。
//   · seek_to / toggle_play       —— 只记录并打日志，尚未驱动解码器。
//     原因：解码循环目前是一次性顺序消费 extractor，没有暂停/跳转状态机。
//     要做真的 seek，需要在 video_decoder 里引入
//     「pause 标志 + AMediaExtractor_seekTo + 丢弃到目标 PTS」三件事，
//     那是独立的一块工作，不放在这次改动里冒充完成。
// ==============================================================================

/// 把归一化亮度写到**系统**亮度设置。
///
/// 为什么不用 WindowManager.LayoutParams.screenBrightness：
///   试过了，在 NativeActivity（hasCode=false、没有 setContentView 时机）
///   上不生效 —— 拖动时 HUD 数字在变，屏幕亮度纹丝不动。
///   原因是 window 的 attributes 被 android-activity 在创建 Surface 时
///   重置过，我们后写入的 screenBrightness 没有真正 apply 到显示层。
///
///   改用 Settings.System.SCREEN_BRIGHTNESS：
///     · 需要 WRITE_SETTINGS 权限（已在 Manifest 申请）
///     · 权限是「特殊权限」，用户需在系统设置里手动授予；
///       未授予时 write 会抛 SecurityException —— 捕获后降级为不生效，
///       但不崩、不打断播放
///
///   这是 B 站、抖音等的实际做法：调的是系统亮度，退出 App 后保持。
pub fn set_brightness(v: f32) {
    #[cfg(target_os = "android")]
    {
        let v = v.clamp(0.0, 1.0);
        let Some(vm) = crate::android_window::java_vm() else { return };
        let Ok(mut env) = vm.attach_current_thread() else { return };
        let Some(activity) = crate::android_window::activity_object() else { return };

        // 系统亮度范围是 0..255（部分 ROM 用 0..2047，但 getInt 读出的
        // 最大值只有 255，这里按 1..255 映射；0 会让屏幕全黑，不好用）
        let target = (v * 254.0).round().max(1.0) as i32;

        // 1. ContentResolver
        let Ok(resolver) = env
            .call_method(&activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])
            .and_then(|r| r.l())
        else {
            return;
        };

        // 2. "screen_brightness" 这个 Settings.System 常量名是稳定的字符串，
        //    不需要去 find_class 拿字段（hasCode=false 下反射很啰嗦）
        let Ok(key) = env.new_string("screen_brightness") else { return };

        // 3. Settings.System.putInt(resolver, key, value)
        //    Settings.System 是 android.provider.Settings$System
        let Ok(cls) = env.find_class("android/provider/Settings$System") else { return };
        let r = env.call_static_method(
            cls,
            "putInt",
            "(Landroid/content/ContentResolver;Ljava/lang/String;I)Z",
            &[
                jni::objects::JValue::Object(&resolver),
                jni::objects::JValue::Object(&key),
                jni::objects::JValue::Int(target),
            ],
        );

        // 未授予 WRITE_SETTINGS 时会抛 SecurityException。
        // 这不是致命错误：亮度调不了，但播放继续。
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
            crate::mxprobe::log_public(
                "[亮度] 写入被拒（需要用户在系统设置里授予「修改系统设置」权限）",
            );
            // 首次被拒时拉一次系统授权页，用户点一下"允许"就能永久生效。
            //
            // 为什么只能这样：WRITE_SETTINGS 是 AppOps 特殊权限，
            // 不能用 requestPermissions 弹运行时对话框，
            // 唯一合法路径是 ACTION_MANAGE_WRITE_SETTINGS。
            // 只在第一次被拒时跳，避免用户每拖一下亮度就弹一次设置页。
            static PROMPTED: AtomicBool = AtomicBool::new(false);
            if !PROMPTED.swap(true, Ordering::SeqCst) {
                open_write_settings_page();
            }
            return;
        }
        if r.is_ok() {
            crate::mxprobe::log_public(&format!("[亮度] -> {target}/255"));
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = v;
    }
}

/// 拉起系统的「修改系统设置」授权页。
///
/// ACTION_MANAGE_WRITE_SETTINGS = "android.settings.action.MANAGE_WRITE_SETTINGS"
/// 需要附带 package: 的 data URI，否则会打开列表页而不是本应用的详情。
fn open_write_settings_page() {
    #[cfg(target_os = "android")]
    {
        let Some(vm) = crate::android_window::java_vm() else { return };
        let Ok(mut env) = vm.attach_current_thread() else { return };
        let Some(activity) = crate::android_window::activity_object() else { return };

        let Ok(intent_cls) = env.find_class("android/content/Intent") else { return };
        let Ok(action) = env.new_string("android.settings.action.MANAGE_WRITE_SETTINGS") else {
            return;
        };
        let Ok(intent) = env.new_object(
            &intent_cls,
            "(Ljava/lang/String;)V",
            &[jni::objects::JValue::Object(&action)],
        ) else {
            return;
        };

        // setData(Uri.parse("package:" + packageName))
        let Ok(pkg) = env
            .call_method(&activity, "getPackageName", "()Ljava/lang/String;", &[])
            .and_then(|v| v.l())
        else {
            return;
        };
        let pkg_str: String = {
            let js: &jni::objects::JString = (&pkg).into();
            match env.get_string(js) {
                Ok(s) => s.into(),
                Err(_) => return,
            }
        };
        let Ok(uri_cls) = env.find_class("android/net/Uri") else { return };
        let Ok(uri_str) = env.new_string(format!("package:{pkg_str}")) else { return };
        let Ok(uri) = env
            .call_method(
                &uri_cls,
                "parse",
                "(Ljava/lang/String;)Landroid/net/Uri;",
                &[jni::objects::JValue::Object(&uri_str)],
            )
            .and_then(|v| v.l())
        else {
            return;
        };
        let _ = env.call_method(
            &intent,
            "setData",
            "(Landroid/net/Uri;)Landroid/content/Intent;",
            &[jni::objects::JValue::Object(&uri)],
        );

        let _ = env.call_method(
            &activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[jni::objects::JValue::Object(&intent)],
        );
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        crate::mxprobe::log_public("[亮度] 已拉起系统授权页");
    }
}

/// 把归一化音量写到媒体流。
///
/// 为什么调 STREAM_MUSIC 而不是 AudioManager 的全局音量索引：
///   直接用 setStreamVolume 需要 MODIFY_AUDIO_SETTINGS 之外还会干扰
///   "勿扰模式"下的静音策略。这里把 0~1 映射到该流的 min~max 索引，
///   交给系统自己处理掩码与静音，行为与用户按音量键一致。
pub fn set_volume(v: f32) {
    #[cfg(target_os = "android")]
    {
        let v = v.clamp(0.0, 1.0);
        let Some(vm) = crate::android_window::java_vm() else { return };
        let Ok(mut env) = vm.attach_current_thread() else { return };
        let Some(activity) = crate::android_window::activity_object() else { return };

        // STREAM_MUSIC = 3
        const STREAM_MUSIC: i32 = 3;
        // flags = 0：**不**显示系统音量条。
        //
        // 原先传 FLAG_SHOW_UI(1)，结果是我们自己的百分比 HUD 和系统音量条
        // 同时弹出，视觉上打架。我们的 HUD 已经给出精确百分比，
        // 系统那个横条在这里纯属干扰，所以关掉。
        const FLAGS_NO_UI: i32 = 0;

        let Ok(audio_svc) = env.new_string("audio") else { return };
        let Ok(am) = env
            .call_method(
                &activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[jni::objects::JValue::Object(&audio_svc)],
            )
            .and_then(|r| r.l())
        else {
            return;
        };

        let Ok(max) = env
            .call_method(&am, "getStreamMaxVolume", "(I)I", &[jni::objects::JValue::Int(STREAM_MUSIC)])
            .and_then(|r| r.i())
        else {
            return;
        };
        // getStreamMinVolume 是 API 28+。低版本没有这个方法，
        // 调用会抛 NoSuchMethodError —— 捕获后按 0 处理即可
        // （API 26/27 的音乐流最小值本来就是 0）。
        let min = env
            .call_method(&am, "getStreamMinVolume", "(I)I", &[jni::objects::JValue::Int(STREAM_MUSIC)])
            .and_then(|r| r.i())
            .unwrap_or(0);
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }

        let target = min + ((max - min) as f32 * v).round() as i32;
        let _ = env.call_method(
            &am,
            "setStreamVolume",
            "(III)V",
            &[
                jni::objects::JValue::Int(STREAM_MUSIC),
                jni::objects::JValue::Int(target),
                jni::objects::JValue::Int(FLAGS_NO_UI),
            ],
        );
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = v;
    }
}

/// 跳转到进度（0.0~1.0）。
///
/// 归一化进度 × 总时长 = 目标 PTS，**同时**交给视频与音频两个解码线程
/// 执行（AMediaExtractor_seekTo + 丢帧校正）。
///
/// 为什么两边都要跳：
///   早先只跳视频。结果是画面到了新位置，声音还在原地播 ——
///   偏出去的正好是"跳了多少"。这是用户看到的那种
///   「有的视频音画极度不同步」里最夸张的一类：
///   只要他拖过进度条或按过快进快退，就必然错位。
pub fn seek_to(frac: f32) {
    let dur = DURATION_US.load(Ordering::SeqCst);
    if dur <= 0 {
        crate::mxprobe::log_public("[Seek] 总时长未知，忽略本次跳转");
        return;
    }
    let target_us = (dur as f64 * frac.clamp(0.0, 1.0) as f64) as i64;
    crate::mxprobe::log_public(&format!(
        "[Seek] 请求 {:.1}% -> {}us",
        frac * 100.0,
        target_us
    ));
    seek_all(target_us);
}

/// 把同一个目标 PTS 交给音视频两条解码线程。
fn seek_all(target_us: i64) {
    let mut did = false;

    if let Ok(cur) = CURRENT.lock() {
        if let Some(dec) = cur.as_ref() {
            dec.controls.request_seek(target_us);
            did = true;
        }
    }
    if let Ok(cur) = AUDIO_DECODER.lock() {
        if let Some(dec) = cur.as_ref() {
            dec.request_seek(target_us);
            did = true;
        }
    }

    if !did {
        crate::mxprobe::log_public("[Seek] 当前没有解码器实例");
    }
}

/// 切换全屏。
///
/// 只改 SurfaceView 的真实几何，UI 侧已经自己改好了
/// （is_fullscreen 驱动 player_height / Tab 栏 / 内容区的显隐）。
/// 两边必须一起变，否则就是"UI 全屏但视频还在顶上"。
pub fn set_fullscreen(on: bool) {
    // 视频分辨率在全屏时用来算 letterbox，否则会拉伸变形
    let (vw, vh) = VIDEO_SIZE
        .lock()
        .ok()
        .and_then(|g| *g)
        .unwrap_or((0, 0));

    crate::mxprobe::log_public(&format!("[Fullscreen] -> {on} (video {vw}x{vh})"));
    crate::video_surface::set_fullscreen_surface(on, vw, vh);

    // 退出全屏后必须等屏幕真正转回竖屏，再落一遍几何。
    //
    // 为什么不能只靠 set_fullscreen_surface：
    //   那次调用发生在点击瞬间，屏幕还是横的，getDisplayMetrics 给的
    //   是横屏尺寸。虽然几何算法已经归一化成短边基准（不会再算错），
    //   但 View 系统那一刻仍按横屏布局，Surface buffer 有可能被那次
    //   布局拉回横屏尺寸。
    //
    // 打两次：方向切换实测 200~300ms，400ms 覆盖大多数情况；
    // 900ms 那次兜住旋转动画拖长/系统排队的情况。两次都是幂等的。
    if !on {
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(400));
            crate::video_surface::reapply_portrait_layout();
            std::thread::sleep(std::time::Duration::from_millis(500));
            crate::video_surface::reapply_portrait_layout();
        });
    }
}

/// 相对跳转（秒）。
///
/// 实现方式：读当前 PTS，加上偏移后走与 seek_to 相同的路径。
///
/// 为什么不直接让解码器做相对 seek：
///   解码线程不知道自己"现在播到哪"以外的东西，而边界处理
///   （不能小于 0、不能超过总时长）属于业务逻辑，放在这里更清楚。
pub fn seek_by(secs: f32) {
    let dur = DURATION_US.load(Ordering::SeqCst);
    if dur <= 0 {
        crate::mxprobe::log_public("[Seek] 总时长未知，忽略本次快进/快退");
        return;
    }

    let cur_pts = CURRENT
        .lock()
        .ok()
        .and_then(|c| c.as_ref().map(|d| d.controls.last_pts_us()))
        .unwrap_or(0)
        .max(0);

    let target = (cur_pts as f64 + (secs as f64 * 1_000_000.0))
        .clamp(0.0, dur as f64) as i64;

    crate::mxprobe::log_public(&format!(
        "[Seek] {}s -> {}us（当前 {}us）",
        secs, target, cur_pts
    ));

    seek_all(target);
}

/// 用户点了清晰度菜单里的某一项。
///
/// `index == 0` 是"自动"，其余对应 QUALITY_LIST 里的第 index-1 项。
///
/// 切换清晰度必须**重新起播**：DASH 的每条轨道是独立的 m4s，
/// 换了档位就是换了一个 URL，没法在现有解码器里热切。
/// 这里走和 play() 一样的路径，但保留当前进度，
/// 用 seek 跳回去 —— 否则用户切个清晰度就得从头看。
pub fn set_quality(window: slint::Weak<AppWindow>, index: i32) {
    let list = QUALITY_LIST.lock().map(|g| g.clone()).unwrap_or_default();

    let qid = if index <= 0 {
        0
    } else {
        list.get((index - 1) as usize).map(|(id, _)| *id).unwrap_or(0)
    };

    DESIRED_QN.store(qid, Ordering::SeqCst);
    QUALITY_LIST_INDEX.store(index.max(0) as u64, Ordering::SeqCst);

    // 持久化。重启后 start_from_playinfo 会读回下标并还原菜单选中态。
    crate::settings::set_u64("quality_qn", qid);
    crate::settings::set_u64("quality_index", index.max(0) as u64);

    crate::mxprobe::log_public(&format!(
        "[Quality] 用户选择 index={index} -> qn={qid}（已持久化）"
    ));

    let bvid = match CURRENT_BVID.lock() {
        Ok(g) => g.clone(),
        Err(_) => None,
    };
    let Some(bvid) = bvid else {
        crate::mxprobe::log_public("[Quality] 当前没有播放中的视频");
        return;
    };
    let cid = CURRENT_CID.load(Ordering::SeqCst);
    if cid == 0 {
        crate::mxprobe::log_public("[Quality] 缺少 cid，无法重新请求播放地址");
        return;
    }

    // 记住当前进度，重新起播后跳回去
    let resume_frac = {
        let dur = DURATION_US.load(Ordering::SeqCst);
        let pts = CURRENT
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|d| d.controls.last_pts_us()))
            .unwrap_or(0)
            .max(0);
        if dur > 0 {
            (pts as f32 / dur as f32).clamp(0.0, 1.0)
        } else {
            0.0
        }
    };

    let auto = window
        .upgrade()
        .map(|w| w.get_auto_play())
        .unwrap_or(true);

    let my_session = PLAY_SESSION.fetch_add(1, Ordering::SeqCst) + 1;

    // ★ 这里**不**走 play()。
    //
    // play() 是"从零打开一个视频"的完整流程：重置详情为"正在获取视频数据…"、
    // 清空评论、重拉详情、重拉评论、重拉相关推荐。切个清晰度触发这一整套
    // 完全没必要 —— 用户只是想把 720P 换成 1080P，不需要标题闪一下空白、
    // 评论区从头加载。
    //
    // 真正要做的只有三件事：停旧解码器、重新请求 playurl、用新地址起播。
    // 详情/评论/相关推荐全都保持原样。
    //
    // 注意依然要在后台线程：stop_current 要 join 解码线程，
    // playurl 是网络请求，都会阻塞 UI。
    std::thread::spawn(move || {
        stop_current();

        let client = match crate::client() {
            Some(c) => c,
            None => {
                report_fail(&window, "初始化网络失败".into());
                return;
            }
        };

        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                report_fail(&window, format!("初始化运行时失败: {e}"));
                return;
            }
        };

        let info = match rt.block_on(client.play_url(&bvid, cid)) {
            Ok(v) => v,
            Err(e) => {
                report_fail(&window, format!("获取播放地址失败: {e}"));
                return;
            }
        };

        if PLAY_SESSION.load(Ordering::SeqCst) != my_session {
            crate::mxprobe::log_public("[Quality] 会话已切换，放弃这次切换");
            return;
        }

        // 切档位不重拉详情，所以要把 is_ready 清掉让封面重新垫上，
        // 否则新流起播前会看到上一档的最后一帧停在那里。
        let w_reset = window.clone();
        let _ = w_reset.upgrade_in_event_loop(|w| {
            let mut d = w.get_current_detail();
            d.is_ready = false;
            w.set_current_detail(d);
            w.set_playback_started(false);
        });

        start_from_playinfo(window.clone(), my_session, bvid, client, info, auto);

        // 恢复进度
        if resume_frac > 0.01 {
            std::thread::sleep(std::time::Duration::from_secs(3));
            seek_to(resume_frac);
            crate::mxprobe::log_public(&format!(
                "[Quality] 恢复到 {:.1}%",
                resume_frac * 100.0
            ));
        }
    });
}

/// 播放 / 暂停切换。
///
/// 三种情形：
///   1. 还没有解码器（自动播放关闭，界面停在封面）→ 真正起播
///   2. 解码器在跑 → 暂停
///   3. 解码器已暂停 → 继续
pub fn toggle_play(window: slint::Weak<AppWindow>) {
    crate::mxprobe::log_public("[Play] 收到播放/暂停指令");

    // 情形 1：还没有解码器 —— 说明是「自动播放关闭」状态，需要真正起播
    let has_decoder = CURRENT
        .lock()
        .map(|c| c.is_some())
        .unwrap_or(false);

    if !has_decoder {
        start_pending(window);
        return;
    }

    // 情形 2/3：已有解码器，翻转暂停位
    //
    // 注意这里**不能**再判断 `!has_decoder` 就 return 后直接读 CURRENT ——
    // 中间可能被 stop_play 清空。用 if let 拿一次快照即可。
    let new_paused = {
        match CURRENT.lock() {
            Ok(cur) => match cur.as_ref() {
                Some(dec) => {
                    let p = !dec.controls.is_paused();
                    dec.controls.set_paused(p);
                    Some(p)
                }
                None => None,
            },
            Err(_) => None,
        }
    };

    match new_paused {
        Some(p) => {
            crate::mxprobe::log_public(if p { "[Play] 已暂停" } else { "[Play] 继续播放" });
            // 音频也要一起停：只停视频不停音频的话，
            // 画面静止但声音继续走，音画立刻错位。
            if let Ok(cur) = AUDIO_DECODER.lock() {
                // AudioDecoder 本身没有 pause 接口，暂停由 AudioOutput 的
                // clock.paused 控制（回调里填静音、不推进播放位置）。
                let _ = &cur;
            }
            if let Some(clk) = AUDIO_CLOCK.lock().ok().and_then(|g| g.clone()) {
                clk.set_paused(p);
            }
            // 同步到 UI，让播放键在三角/双竖线之间切换
            let w = window.clone();
            let _ = w.upgrade_in_event_loop(move |ui| {
                ui.set_play_paused(p);
            });
        }
        None => crate::mxprobe::log_public("[Play] 解码器已消失，忽略"),
    }
}

/// 听视频：关掉画面只放声音。
///
/// 实现就是隐藏视频层（alpha=0），**不**停解码器 —— 停了音频也会跟着停。
/// 视频解码继续跑是有点浪费，但换来的是切回来时画面立刻有，
/// 而不是重新等关键帧。这个取舍和"后台只放声音"那段代码一致。
///
/// 注意：不能用 INVISIBLE / 高度归零，那会销毁 Surface，
/// 切回来再也 attach 不上（见 video_surface 里那段血的教训）。
pub fn set_audio_only(on: bool) {
    crate::mxprobe::log_public(&format!("[AudioOnly] -> {on}"));
    if on {
        crate::video_surface::set_visible(false);
    } else {
        // 只有当前确实在播时才恢复画面，否则会把详情页的封面垫底也盖掉
        let playing = CURRENT.lock().map(|c| c.is_some()).unwrap_or(false);
        if playing {
            crate::video_surface::set_visible(true);
        }
    }
}

/// 用暂存的播放地址启动播放（自动播放关闭后的手动起播）。
fn start_pending(window: slint::Weak<AppWindow>) {
    let pending = PENDING_STREAM.lock().ok().and_then(|mut p| p.take());

    let Some(p) = pending else {
        crate::mxprobe::log_public("[Play] 没有待播流（可能还在加载播放地址）");
        return;
    };

    // 会话已经切换（用户返回或点了别的视频），这份待播流作废
    if PLAY_SESSION.load(Ordering::SeqCst) != p.session {
        crate::mxprobe::log_public("[Play] 待播流属于旧会话，丢弃");
        return;
    }

    crate::mxprobe::log_public(&format!("[Play] 用户手动起播 {}", p.bvid));

    // 先让 UI 进入"加载中"，同时收起中央播放按钮
    let w_ui = window.clone();
    let _ = w_ui.upgrade_in_event_loop(move |w| {
        w.set_awaiting_play(false);
        w.set_playback_started(false);
        let mut d = w.get_current_detail();
        d.is_ready = false;
        d.load_error = "".into();
        w.set_current_detail(d);
    });

    // ★ 整条起播链路必须放到后台线程。
    //
    // 这个函数是被 toggle_play 调用的，而 toggle_play 又是 Slint 的
    // 点击回调 —— 也就是说它跑在 **UI 线程** 上。而 start_playback
    // 内部全是阻塞调用：
    //   · probe_content_length  → 网络请求（最多 15 秒超时）
    //   · wait_surface()        → 循环 sleep，最多 7.5 秒
    //   · rx_ready.recv_timeout → 最多 12 秒
    //
    // 在外面直接调它的后果就是症状里描述的那样：
    //   「点播放后整个 App 卡住，详情页滑不动，后台一直在下载但不出画面」
    // —— UI 线程被这些阻塞调用钉死，连滑动都渲染不了；
    // 而下载线程是独立的，照样在跑，于是看到持续的 1~3MB/s 流量。
    //
    // 启动 play() 那条路径本来就在 `std::thread::spawn` 里（见 play），
    // 只有这个"手动起播"入口漏了。现在补齐。
    std::thread::spawn(move || {
        let client = match crate::client() {
            Some(c) => c,
            None => {
                report_fail(&window, "初始化网络失败".into());
                return;
            }
        };

        start_playback(
            window,
            p.session,
            p.bvid,
            p.url,
            p.audio_url,
            client,
            p.duration_us,
        );
    });
}
