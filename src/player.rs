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
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
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
    // 3.5 边下边播：流式写入缓存，首段 1.5MB 就绪后即刻起播，杜绝整段漫长等待
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

    if !path.exists() || path.metadata().map(|m| m.len()).unwrap_or(0) < 500_000 {
        crate::mxprobe::log_public("[Player] 启动边下边播流式拉取…");
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let client_dl = client.clone();
        let url_dl = url.clone();
        let path_dl = path.clone();

        std::thread::spawn(move || {
            let rt_dl = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(r) => r,
                Err(_) => {
                    let _ = tx.send(false);
                    return;
                }
            };

            let res = rt_dl.block_on(client_dl.download_stream_to_file(
                &url_dl,
                &path_dl,
                1_500_000, // 首段阈值 1.5MB（包含 ftyp/moov 与前几秒关键帧，通常只需 0.1~0.3 秒）
                move || {
                    let _ = tx.send(true);
                },
            ));

            if let Err(e) = res {
                crate::mxprobe::log_public(&format!("[流式下载错误] {e}"));
            }
        });

        // 等待首批 1.5MB 就绪信号，放行起播
        match rx.recv_timeout(std::time::Duration::from_secs(12)) {
            Ok(true) => {
                crate::mxprobe::log_public("[Player] 首段数据缓冲完毕，即刻进入播放！");
            }
            _ => {
                if !path.exists() || path.metadata().map(|m| m.len()).unwrap_or(0) < 300_000 {
                    report_fail(&w_clone, "缓冲视频流超时，请检查网络".into());
                    return;
                }
            }
        }
    } else {
        crate::mxprobe::log_public(&format!("[Player] 使用已缓存视频流: {path_str}"));
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
        path_str,
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

/// 播放进度搬运线程：解码器 PTS → Slint 的 progress / 时间文案。
fn spawn_progress_poller(window: slint::Weak<AppWindow>, session: u64, duration_us: i64) {
    if duration_us <= 0 {
        return;
    }
    std::thread::spawn(move || {
        let total_secs = duration_us / 1_000_000;
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

            if pts <= 0 {
                std::thread::sleep(std::time::Duration::from_millis(250));
                continue;
            }

            let frac = (pts as f64 / duration_us as f64).clamp(0.0, 1.0) as f32;
            let cur_secs = (pts / 1_000_000).max(0);
            let _ = window.upgrade_in_event_loop(move |ui| {
                ui.set_play_progress(frac);
                ui.set_play_time_current(fmt_time(cur_secs).into());
                ui.set_play_time_total(fmt_time(total_secs as i64).into());
            });

            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    });
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
        if i == 5 || i == 15 || i == 25 {
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

/// 把归一化亮度写到窗口（WindowManager.LayoutParams.screenBrightness）。
///
/// 为什么用窗口亮度而不是 Settings.System.SCREEN_BRIGHTNESS：
///   后者要 WRITE_SETTINGS 特殊权限，且会污染用户全局设置。
///   窗口亮度只作用于本 App 的窗口，退出即失效 —— 这也是所有播放器
///   （B 站、YouTube、Plex）调亮度的标准做法。
pub fn set_brightness(v: f32) {
    #[cfg(target_os = "android")]
    {
        let v = v.clamp(0.0, 1.0);
        let Some(vm) = crate::android_window::java_vm() else { return };
        let Ok(mut env) = vm.attach_current_thread() else { return };
        let Some(activity) = crate::android_window::activity_object() else { return };

        // BRIGHTNESS_OVERRIDE_NONE = -1（交还系统控制）
        // 0.0 ~ 1.0 是窗口亮度覆盖值
        let target = if v <= 0.005 { 0.0f32 } else { v };

        let Ok(window) = env
            .call_method(&activity, "getWindow", "()Landroid/view/Window;", &[])
            .and_then(|r| r.l())
        else {
            return;
        };
        let Ok(attrs) = env
            .call_method(&window, "getAttributes", "()Landroid/view/WindowManager$LayoutParams;", &[])
            .and_then(|r| r.l())
        else {
            return;
        };
        let _ = env.set_field(&attrs, "screenBrightness", "F", jni::objects::JValue::Float(target));
        let _ = env.call_method(
            &window,
            "setAttributes",
            "(Landroid/view/WindowManager$LayoutParams;)V",
            &[jni::objects::JValue::Object(&attrs)],
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
        // FLAG_SHOW_UI = 1（顺带显示系统音量条，用户有双重反馈）
        const FLAG_SHOW_UI: i32 = 1;

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
                jni::objects::JValue::Int(FLAG_SHOW_UI),
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

/// 播放 / 暂停切换。
///
/// 三种情形：
///   1. 还没有解码器（自动播放关闭，界面停在封面）→ 真正起播
///   2. 解码器在跑 → 暂停
///   3. 解码器已暂停 → 继续
pub fn toggle_play(window: slint::Weak<AppWindow>) {
    // 情形 1：需要从「待播」状态启动
    let has_decoder = CURRENT
        .lock()
        .map(|c| c.is_some())
        .unwrap_or(false);

    if !has_decoder {
        start_pending(window);
        return;
    }

    if let Ok(cur) = CURRENT.lock() {
        if let Some(dec) = cur.as_ref() {
            let now = !dec.controls.is_paused();
            dec.controls.set_paused(now);
            crate::mxprobe::log_public(&format!(
                "[Play] {}",
                if now { "已暂停" } else { "继续播放" }
            ));
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
