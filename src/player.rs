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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use crate::{AppWindow, CommentItemData, RelatedVideoData};

/// 全局解码器句柄
static CURRENT: Mutex<Option<crate::video_decoder::VideoDecoder>> = Mutex::new(None);
/// 全局播放会话序列号，杜绝多任务并发争抢 Surface 与 Codec 冲突
static PLAY_SESSION: AtomicU64 = AtomicU64::new(0);

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
}


/// 把播放链路失败原因写进 UI。
///
/// 为什么必须有这一步：早期所有失败分支都只 `log` 然后 `return`，
/// 界面上永远停在"视频加载中…"。用户看到的是一个不动也不报错的播放器，
/// 唯一出路是手动返回，体感就是"点了一下没反应，然后自己跳了下一个视频"。
fn report_fail(window: &slint::Weak<AppWindow>, msg: String) {
    crate::mxprobe::log_public(&format!("[播放失败] {msg}"));
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
    crate::video_surface::hide_surface();
}

/// 播放指定 bvid 并切换到二级页面
pub fn play(window: slint::Weak<AppWindow>, bvid: String) {
    let my_session = PLAY_SESSION.fetch_add(1, Ordering::SeqCst) + 1;
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
        let (aid, cid, _is_vertical, face_url, pic_url) = match detail_res {
            Ok(d) => {
                let aid = d.aid;
                let cid = d.cid;
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

                (aid, cid, is_vert, face, pic)
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
                if let Ok(comments) = rt_c.block_on(client_c.video_comments(aid)) {
                    let raw_comments: Vec<RawComment> = comments
                        .into_iter()
                        .map(|c| RawComment {
                            user_name: c.user_name,
                            user_face: c.user_face,
                            content: c.content,
                            likes: c.likes,
                            pubtime: c.pubtime,
                        })
                        .collect();

                    let _ = w_comment.upgrade_in_event_loop(move |w| {
                        let ui_comments: Vec<CommentItemData> = raw_comments
                            .into_iter()
                            .map(|c| CommentItemData {
                                user_name: c.user_name.into(),
                                user_face: Image::default(),
                                content: c.content.into(),
                                likes: c.likes.into(),
                                pubtime: c.pubtime.into(),
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
    });
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
