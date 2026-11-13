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
/// 全局播放会话序列号，杜绝多任务并发争抢 Surface 与 Codec 冲突
static PLAY_SESSION: AtomicU64 = AtomicU64::new(0);
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
    duration_us: i64,
}

static PENDING_STREAM: Mutex<Option<PendingStream>> = Mutex::new(None);

thread_local! {
    /// 相关推荐视频模型句柄（仅在 UI 线程访问，杜绝跨线程 Send 冲突）
    static RELATED_MODEL: RefCell<Option<Rc<VecModel<RelatedVideoData>>>> = const { RefCell::new(None) };
}

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
    stop_current();

    // 注意：这里**不**解除系统手势排除区。
    // 解除会让左边缘侧滑被系统吃掉并 finish 掉 NativeActivity，
    // 表现就是"左边侧滑直接退出 App"。
    // 详情页的边缘返回由 UI 侧的 EdgeBack 手势处理，不交给系统。

    // 1. UI 立即切入详情页，并初始化为加载状态 (封面垫底)
    let w_clone = window.clone();
    let bvid_initial = bvid.clone();
    let _ = window.upgrade_in_event_loop(move |w| {
        w.set_app_mode(1);
        // 新视频进来先把"等待起播"清掉，避免沿用上一个视频的状态
        w.set_awaiting_play(false);
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

        let client = match bli_protocol::BiliClient::new() {
            Ok(c) => c,
            Err(e) => {
                report_fail(&w_clone, format!("初始化网络失败: {e}"));
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
        if aid > 0 {
            let w_comment = w_clone.clone();
            let client_c = client.clone();
            std::thread::spawn(move || {
                let rt_c = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(r) => r,
                    Err(_) => return,
                };
                if let Ok(comments) = rt_c.block_on(client_c.video_comments(aid, owner_mid)) {
                    let raw_comments: Vec<RawComment> = comments
                        .into_iter()
                        .map(|c| RawComment {
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

                    // 先把文字内容上屏（头像留空），头像随后异步补 ——
                    //   评论主体是文字，头像只是装饰；等头像下载完再整体渲染
                    //   会让评论区白屏好几秒。
                    let face_urls: Vec<String> =
                        raw_comments.iter().map(|c| c.user_face.clone()).collect();

                    let w_face_list = w_comment.clone();
                    let _ = w_comment.upgrade_in_event_loop(move |w| {
                        let ui_comments: Vec<CommentItemData> = raw_comments
                            .into_iter()
                            .map(|c| {
                                let sub_authors: Vec<slint::SharedString> =
                                    c.sub_authors.iter().map(|s| s.as_str().into()).collect();
                                let sub_replies: Vec<slint::SharedString> =
                                    c.sub_contents.iter().map(|s| s.as_str().into()).collect();
                                CommentItemData {
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
                            })
                            .collect();
                        w.set_video_comments(ModelRc::new(Rc::new(VecModel::from(ui_comments))));
                    });

                    // 头像异步下载并逐条回填。
                    //
                    // 为什么逐条回填而不是攒齐一次性设置：
                    //   20 条评论的头像，弱网下攒齐可能要十几秒，期间评论区
                    //   全是灰圆点。逐条回填的话，谁的先下来谁先显示。
                    //
                    // 为什么只下前 20 个：
                    //   接口一页就是 20 条，再往下要翻页 —— 翻页功能还没做，
                    //   预先下载屏幕外的头像纯属浪费流量。
                    spawn_comment_faces(w_face_list, face_urls, my_session);
                }
            });
        }

        // 3.3 拉取相关视频推荐
        let rel_res = rt.block_on(client.related_videos(&bvid));
        if let Ok(related_list) = rel_res {
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

            let covers_to_download: Vec<(usize, String)> = raw_rel
                .iter()
                .enumerate()
                .take(12)
                .filter(|(_, r)| !r.cover_url.is_empty())
                .map(|(i, r)| (i, r.cover_url.clone()))
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

            if !covers_to_download.is_empty() {
                let w_cov_ui = w_clone.clone();
                std::thread::spawn(move || {
                    let rt_cov = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                        Ok(r) => r,
                        Err(_) => return,
                    };
                    for (idx, url) in covers_to_download {
                        if PLAY_SESSION.load(Ordering::SeqCst) != my_session {
                            return;
                        }
                        if let Some((w, h, rgba)) = rt_cov.block_on(download_raw_image(&url)) {
                            let w_ui = w_cov_ui.clone();
                            let _ = w_ui.upgrade_in_event_loop(move |_| {
                                RELATED_MODEL.with(|cell| {
                                    if let Some(ref m) = *cell.borrow() {
                                        if let Some(mut row) = m.row_data(idx) {
                                            let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                                            row.cover = Image::from_rgba8(buf);
                                            m.set_row_data(idx, row);
                                        }
                                    }
                                });
                            });
                        }
                    }
                });
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

        // 优先 720P / 480P，移动端秒级快速下载，杜绝 1080P 超大体积与限速导致长时间卡在加载中
        let Some(vtrack) = info.pick_video(Some(720)) else {
            report_fail(&w_clone, "该视频没有可用的视频流（可能是充电专属或地区限制）".into());
            return;
        };
        let Some(url) = vtrack.primary_url().map(|s| s.to_string()) else {
            report_fail(&w_clone, "视频流地址为空".into());
            return;
        };

        crate::mxprobe::log_public(&format!(
            "[Player] 选中分辨率: {}x{} 编码: {}",
            vtrack.width.unwrap_or(0),
            vtrack.height.unwrap_or(0),
            vtrack.codecs
        ));

        // 记录总时长（playurl 给的是秒，内部统一用微秒）。
        // UI 的手势进度是 0.0~1.0，seek 时要用它换算成 PTS。
        let dur_us = info.duration.saturating_mul(1_000_000) as i64;
        DURATION_US.store(dur_us, Ordering::SeqCst);
        crate::mxprobe::log_public(&format!("[Player] 总时长: {}s", info.duration));

        // ---- 自动播放开关 ----
        //
        // 关闭时不下载视频流：这是"进入详情页但不播放"的核心 ——
        // 既不浪费流量，也不会占着解码器。播放地址已经拿到并缓存在下面，
        // 用户点播放时直接用，不需要再请求一次 playurl。
        if !auto_play {
            crate::mxprobe::log_public("[Player] 自动播放已关闭，等待用户手动起播");
            // 把地址存起来，供 start_playback 使用
            if let Ok(mut pending) = PENDING_STREAM.lock() {
                *pending = Some(PendingStream {
                    session: my_session,
                    url: url.clone(),
                    bvid: bvid.clone(),
                    duration_us: dur_us,
                });
            }
            // 显示中央播放按钮：封面已垫底，用户看到的是"封面 + 大播放键"，
            // 语义清楚，不需要对着黑屏猜。
            let w_wait = w_clone.clone();
            let _ = w_wait.upgrade_in_event_loop(move |w| {
                w.set_awaiting_play(true);
            });
            return;
        }

        // 自动播放：直接起播
        start_playback(w_clone.clone(), my_session, bvid.clone(), url, client.clone(), dur_us);
    });
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
    let cache_dir = crate::android_window::cache_dir()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let p = std::path::PathBuf::from("/data/data/com.bli.rs/cache");
            if p.exists() || std::fs::create_dir_all(&p).is_ok() {
                p
            } else {
                std::env::temp_dir()
            }
        });
    let _ = std::fs::create_dir_all(&cache_dir);
    let path = cache_dir.join(format!("v_{bvid}.m4s"));
    let path_str = path.to_string_lossy().to_string();

    // 残留的半截文件必须删掉：它会被当成完整视频解析。
    let _ = std::fs::remove_file(&path);

    // 先问出总长度。
    //
    // AMediaExtractor 的解封装阶段会调 getSize()，拿不到确定值
    // （返回 -1）就会一直卡住不返回 —— 实测如此。
    // 所以在建数据源之前，先发一个 Range: bytes=0-0 的小请求拿
    // Content-Range 里的 total（只消耗 1 字节流量）。
    let total_len = {
        let rt_probe = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(r) => r,
            Err(_) => {
                report_fail(&w_clone, "初始化网络失败".into());
                return;
            }
        };
        rt_probe.block_on(client.probe_content_length(&url))
    };

    match total_len {
        Some(n) => crate::mxprobe::log_public(&format!(
            "[Player] 视频总长度 {:.1} MB",
            n as f64 / 1_048_576.0
        )),
        None => crate::mxprobe::log_public("[Player] 未能探测总长度（将退回非流式读取）"),
    }

    let buffer = crate::stream_source::SharedBuffer::new(path.clone(), total_len);

    crate::mxprobe::log_public("[Player] 启动边下边播（AMediaDataSource 流式解码）…");

    // 首段阈值 512KB：够解出 ftyp/moov 与前若干关键帧，通常 0.2~0.6 秒。
    let (tx_ready, rx_ready) = std::sync::mpsc::sync_channel::<()>(1);
    {
        let buffer_dl = buffer.clone();
        let client_dl = client.clone();
        let url_dl = url.clone();
        let path_dl = path.clone();

        std::thread::spawn(move || {
            let rt_dl = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => {
                    crate::mxprobe::log_public(&format!("[下载] 运行时创建失败: {e}"));
                    buffer_dl.fail();
                    return;
                }
            };

            // ---- 带断点续传的重试循环 ----
            //
            // 为什么必须重试：B 站 CDN 在长连接上经常中途掐断
            // （日志里的 "error decoding response body" 就是这个）。
            // 一小时的视频要连续下几十 MB，单次请求基本不可能不断。
            //
            // 策略：失败后从已下载的字节数继续（HTTP Range 续传），
            // 最多重试 6 次，每次间隔递增。只要总进度在推进就继续，
            // 连续 3 次没有任何进展才放弃 —— 这样弱网下也能下完。
            const MAX_RETRY: u32 = 6;
            let mut retry = 0u32;
            let mut last_size = 0u64;
            let mut stall = 0u32;
            let mut ready_sent = false;

            loop {
                let start_size = path_dl.metadata().map(|m| m.len()).unwrap_or(0);

                let tx2 = tx_ready.clone();
                let res = rt_dl.block_on(client_dl.download_stream_to_file_from(
                    &url_dl,
                    &path_dl,
                    start_size,
                    512_000,
                    move || {
                        let _ = tx2.send(());
                    },
                ));
                ready_sent = ready_sent || start_size > 0;

                match res {
                    Ok(_written) => {
                        let total = path_dl.metadata().map(|m| m.len()).unwrap_or(0);
                        crate::mxprobe::log_public(&format!(
                            "[下载] 完成 {:.1} MB",
                            total as f64 / 1_048_576.0
                        ));
                        buffer_dl.finish(total);
                        return;
                    }
                    Err(e) => {
                        let now_size = path_dl.metadata().map(|m| m.len()).unwrap_or(0);
                        // 把已经写下的部分同步给解码器，让它能继续播
                        let delta = now_size.saturating_sub(buffer_dl.available_now());
                        if delta > 0 {
                            buffer_dl.advance(delta);
                        }

                        if now_size > last_size {
                            // 有进展，重试计数不清零但 stall 归零
                            stall = 0;
                            last_size = now_size;
                        } else {
                            stall += 1;
                        }

                        retry += 1;
                        crate::mxprobe::log_public(&format!(
                            "[下载] 中断({e})，已下载 {:.1} MB，第 {retry} 次重试",
                            now_size as f64 / 1_048_576.0
                        ));

                        if retry >= MAX_RETRY || stall >= 3 {
                            crate::mxprobe::log_public("[下载] 重试耗尽，放弃");
                            buffer_dl.fail();
                            return;
                        }

                        // 递增退避：0.5s, 1s, 1.5s...
                        std::thread::sleep(std::time::Duration::from_millis(
                            500 * retry as u64,
                        ));
                    }
                }
            }
        });
    }

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
    crate::video_surface::set_visible(true);

    let w_ready = w_clone.clone();
    let w_err = w_clone.clone();

    crate::mxprobe::log_public(&format!("[Player] 启动解码器: session={my_session}, path={path_str}"));
    let dec = crate::video_decoder::VideoDecoder::start(
        buffer,
        surface,
        move || {
            crate::mxprobe::log_public("[Player] 首帧成功送显，通知 UI 淡出封面占位图");
            let _ = w_ready.upgrade_in_event_loop(|w| {
                let mut d = w.get_current_detail();
                d.is_ready = true;
                w.set_current_detail(d);
            });
        },
        move |e| {
            crate::mxprobe::log_public(&format!("[Player] 解码失败通知: {e}"));
            report_fail(&w_err, format!("视频解码失败: {e}"));
        },
    );

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

/// 评论头像批量下载并逐条回填。
///
/// 为什么要单独一个函数、还要带 session：
///   用户在评论区翻着，切走了视频，这些下载中的头像必须能自我终止，
///   否则会往已经换掉的列表里写数据（表现为新视频评论区混进旧头像）。
fn spawn_comment_faces(
    window: slint::Weak<AppWindow>,
    urls: Vec<String>,
    session: u64,
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

            let w_ui = window.clone();
            let _ = w_ui.upgrade_in_event_loop(move |ui| {
                // 通过 AppWindow 重新取当前模型，避免跨线程持有 ModelRc
                let model = ui.get_video_comments();
                if let Some(mut row) = model.row_data(idx) {
                    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                    row.user_face = Image::from_rgba8(buf);
                    model.set_row_data(idx, row);
                }
                let _ = &ui;
            });
        }
    });
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
        loop {
            if PLAY_SESSION.load(Ordering::SeqCst) != session {
                return;
            }

            let pts = {
                match CURRENT.lock() {
                    Ok(cur) => match cur.as_ref() {
                        Some(d) => d.controls.last_pts_us(),
                        None => return,
                    },
                    Err(_) => return,
                }
            };

            // 注意：pts == 0 是**合法值**（首帧就从 0 开始），
            // 不能像之前那样 `pts <= 0` 直接跳过 —— 那会让进度条在
            // 开播后的前若干帧一直停在 0，看起来像没在走。
            // 只有负数才是"还没有数据"，那才跳过。
            if pts < 0 {
                std::thread::sleep(std::time::Duration::from_millis(250));
                continue;
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
/// 归一化进度 × 总时长 = 目标 PTS，交给解码线程执行
/// AMediaExtractor_seekTo + 丢帧校正（见 video_decoder::decode_loop）。
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
    if let Ok(cur) = CURRENT.lock() {
        if let Some(dec) = cur.as_ref() {
            dec.controls.request_seek(target_us);
        } else {
            crate::mxprobe::log_public("[Seek] 当前没有解码器实例");
        }
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

    if let Ok(cur) = CURRENT.lock() {
        if let Some(dec) = cur.as_ref() {
            dec.controls.request_seek(target);
        }
    }
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
            // 同步到 UI，让播放键在三角/双竖线之间切换
            let w = window.clone();
            let _ = w.upgrade_in_event_loop(move |ui| {
                ui.set_play_paused(p);
            });
        }
        None => crate::mxprobe::log_public("[Play] 解码器已消失，忽略"),
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
        let mut d = w.get_current_detail();
        d.is_ready = false;
        d.load_error = "".into();
        w.set_current_detail(d);
    });

    let client = match bli_protocol::BiliClient::new() {
        Ok(c) => c,
        Err(e) => {
            report_fail(&window, format!("初始化网络失败: {e}"));
            return;
        }
    };

    start_playback(window, p.session, p.bvid, p.url, client, p.duration_us);
}
