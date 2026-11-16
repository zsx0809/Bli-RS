// ==============================================================================
// 登录
//
// 走**扫码登录**。理由见 crates/bli-protocol/src/auth.rs 顶部的长注释：
// 密码登录要 RSA + 极验（滑块在 WebView 里跑，native 侧无解），
// 扫码只需要两个 HTTP GET。
//
// 数据流：
//   1. UI 点「登录」→ login_start()
//   2. 后台线程 generate 二维码 → 画成 RGBA 位图 → 送回 UI 显示
//   3. 同时起轮询线程，每 2 秒 poll 一次
//   4. poll 返回 Success → 把 Auth 写进 settings + BiliClient + 全局状态
//   5. UI 收到 on_login_state → 抽屉切换成已登录形态、评论区重新拉
//
// 为什么登录态要持久化到磁盘：
//   NativeActivity 在后台被杀掉后重启是常态。cookie 丢了就要重新扫码，
//   这对用户是不可接受的。存 files_dir()/settings.conf。
// ==============================================================================


use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer};

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use bli_protocol::auth::{self, Auth, PollState};

use crate::AppWindow;

/// 全局登录态。None = 游客。
static AUTH: Mutex<Option<Auth>> = Mutex::new(None);
/// 当前轮询是否在跑（防止重复起轮询线程）
static POLLING: AtomicBool = AtomicBool::new(false);
/// 轮询代数。每次登录/退出 +1，旧线程看到代数变了就自己退出。
static POLL_GEN: AtomicU64 = AtomicU64::new(0);
/// 用户头像下载完成后回填用的代数，避免旧请求覆盖新头像
static FACE_GEN: AtomicU64 = AtomicU64::new(0);

/// 全局窗口弱引用。
///
/// 后端线程（登录轮询、评论回填）需要能主动往 UI 写东西，但它们
/// 启动时拿不到 AppWindow。在 init 阶段登记一次，后面各处复用。
static WINDOW: Mutex<Option<slint::Weak<AppWindow>>> = Mutex::new(None);

const KEY_COOKIE: &str = "auth.cookie";
const KEY_MID: &str = "auth.mid";

/// 登记全局窗口引用（init 时调一次）。
pub fn set_window(w: slint::Weak<AppWindow>) {
    if let Ok(mut g) = WINDOW.lock() {
        *g = Some(w);
    }
}

/// 取全局窗口弱引用。
pub fn global_window() -> Option<slint::Weak<AppWindow>> {
    WINDOW.lock().ok().and_then(|g| g.clone())
}

/// 启动时从磁盘恢复登录态。
///
/// 恢复之后必须再做一次 fetch_nav：cookie 可能已经过期（B 站 cookie
/// 有效期约半年，但改密码/退出全设备会立刻失效）。如果 nav 返回 -101，
/// 说明这份 cookie 是死的，直接清掉，免得后续所有请求带着一个坏
/// cookie 被风控额外关注。
pub fn restore(client: bli_protocol::BiliClient) {
    let cookie = crate::settings::get_string(KEY_COOKIE);
    if cookie.is_empty() {
        crate::mxprobe::log_public("[Login] 没有已保存的登录态");
        return;
    }

    let mut a = Auth::from_cookie_string(&cookie);
    if !a.is_logged_in() {
        crate::mxprobe::log_public("[Login] 已保存的 cookie 缺 bili_jct，丢弃");
        crate::settings::remove(KEY_COOKIE);
        return;
    }

    crate::mxprobe::log_public(&format!(
        "[Login] 发现已保存的登录态 mid={}，校验中…",
        a.mid
    ));

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return,
        };

        rt.block_on(async {
            let http = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
            {
                Ok(c) => c,
                Err(_) => return,
            };

            match auth::fetch_nav(&http, &mut a).await {
                Ok(()) => {
                    crate::mxprobe::log_public(&format!(
                        "[Login] 登录态有效: {} (LV{}) —— 回填 UI",
                        a.uname, a.level
                    ));
                    persist_and_apply(a, client);
                }
                Err(e) => {
                    crate::mxprobe::log_public(&format!("[Login] 登录态已失效（{e}），清除"));
                    crate::settings::remove(KEY_COOKIE);
                }
            }
        });
    });
}

/// 把登录态写盘、装进 client、更新 UI。
///
/// 关键点：UI 更新**不能**寄居在头像下载线程里。
///
/// 上一版把 `set_logged_in` / `set_login_name` 全塞进头像线程的
/// `upgrade_in_event_loop` 闭包里，于是：
///   · 那个线程 upgrade 失败 → 整个登录态在 UI 上不生效
///   · 头像下载慢或失败 → 连"已登录"三个字都看不到
/// 用户实测就是"扫码完了还是未登录、头像也没加载"。
///
/// 现在拆成两段：
///   1. 文本状态（昵称/等级/是否登录）**立刻**写，不等网络
///   2. 头像单独一个线程，回来只覆盖 user_face
///
/// `window` 参数已废弃，统一用全局弱引用 —— restore() 在启动阶段
/// 没有 AppWindow 句柄，走参数的话那条路径永远不更新 UI。
fn persist_and_apply(a: Auth, client: bli_protocol::BiliClient) {
    crate::settings::set_string(KEY_COOKIE, &a.cookie);
    crate::settings::set_u64(KEY_MID, a.mid);

    // client 里的 cookie 影响所有后续请求（评论分页 / playurl 高清 / 发评论）
    client.set_auth(Some(a.clone()));

    let (uname, face, level, is_vip, mid) = (
        a.uname.clone(),
        a.face.clone(),
        a.level,
        a.is_vip,
        a.mid,
    );

    if let Ok(mut g) = AUTH.lock() {
        *g = Some(a);
    }

    crate::mxprobe::log_public(&format!(
        "[Login] 已登录: {uname} mid={mid} LV{level} vip={is_vip} face={}",
        if face.is_empty() { "空" } else { &face[..face.len().min(48)] }
    ));

    // ---- 第一段：文本状态，立即生效 ----
    let text_gen = FACE_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    match global_window() {
        Some(w) => {
            let uname_ui = uname.clone();
            let face_ui = face.clone();
            let logged = std::sync::Arc::new(AtomicBool::new(false));
            let logged_flag = logged.clone();
            let ok = w.upgrade_in_event_loop(move |win| {
                win.set_logged_in(true);
                win.set_login_name(uname_ui.as_str().into());
                win.set_login_level(level as i32);
                win.set_login_vip(is_vip);
                win.set_login_face_url(face_ui.as_str().into());
                win.set_show_login(false);
                win.set_login_status("登录成功".into());
                win.set_login_qr_loading(false);
                logged_flag.store(true, Ordering::SeqCst);
            });
            if ok.is_err() {
                crate::mxprobe::log_public("[Login] 警告：UI 事件队列已关闭，登录态只留在内存");
            }
        }
        None => crate::mxprobe::log_public("[Login] 警告：没有登记过窗口，UI 无法更新"),
    }

    // ---- 第二段：头像，后台下载后单独覆盖 ----
    if face.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let rgba = crate::player::fetch_rgba_blocking(&face);
        let Some((fw, fh, data)) = rgba else {
            crate::mxprobe::log_public("[Login] 头像下载失败");
            return;
        };
        let Some(w) = global_window() else { return };
        let _ = w.upgrade_in_event_loop(move |win| {
            if FACE_GEN.load(Ordering::SeqCst) != text_gen {
                return; // 期间又登录了别的账号
            }
            let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&data, fw, fh);
            win.set_user_face(Image::from_rgba8(buf));
            crate::mxprobe::log_public("[Login] 头像已上屏");
        });
    });
}

/// 当前登录态快照。
pub fn current() -> Option<Auth> {
    AUTH.lock().ok().and_then(|g| g.clone())
}

pub fn is_logged_in() -> bool {
    current().map(|a| a.is_logged_in()).unwrap_or(false)
}

/// 点「登录」：申请二维码并开始轮询。
///
/// 全程在后台线程 —— UI 线程绝对不能碰网络（会卡住渲染，
/// 表现是界面冻住几百毫秒）。
pub fn start(window: &AppWindow, client: bli_protocol::BiliClient) {
    // 每次 start 都推进代数：旧轮询线程看到代数变了会自己退出。
    // 这里**不**因为"已有轮询"而提前返回 —— 用户点"刷新"就是想让
    // 旧的那一轮作废。重复点击的代价只是多一次 generate 请求。
    let gen = POLL_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    POLLING.store(true, Ordering::SeqCst);

    // UI 先进入"加载中"
    {
        let w = window.as_weak();
        let _ = w.upgrade_in_event_loop(|win| {
            win.set_login_qr(Image::default());
            win.set_login_status("正在获取二维码…".into());
            win.set_login_qr_loading(true);
        });
    }

    let w_weak = window.as_weak();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                finish(gen);
                report(&w_weak, format!("创建运行时失败: {e}"));
                return;
            }
        };

        rt.block_on(async {
            let http = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    finish(gen);
                    report(&w_weak, format!("创建 HTTP 客户端失败: {e}"));
                    return;
                }
            };

            let qr = match auth::qrcode_generate(&http).await {
                Ok(q) => q,
                Err(e) => {
                    finish(gen);
                    report(&w_weak, format!("获取二维码失败: {e}"));
                    return;
                }
            };

            crate::mxprobe::log_public(&format!(
                "[Login] 二维码已获取 key={}…",
                &qr.key[..qr.key.len().min(8)]
            ));

            // 画二维码（纯计算，很快，几十毫秒）
            let bitmap = render_qr(&qr.url);

            {
                let w = w_weak.clone();
                let _ = w.upgrade_in_event_loop(move |win| {
                    if let Some((side, data)) = bitmap {
                        let buf =
                            SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&data, side, side);
                        win.set_login_qr(Image::from_rgba8(buf));
                        win.set_login_status("请用 B 站 App 扫码".into());
                    } else {
                        win.set_login_status("二维码生成失败".into());
                    }
                    win.set_login_qr_loading(false);
                });
            }

            // ---- 轮询 ----
            //
            // 2 秒一次，最多 90 次（3 分钟，和二维码有效期一致）。
            // 每次检查代数：用户在别处重新登录 / 退出登录时，
            // 这个线程应该安静地死掉，而不是继续抢状态。
            for _ in 0..90 {
                if POLL_GEN.load(Ordering::SeqCst) != gen {
                    return; // 被别人接管了，静默退出，不要清 POLLING
                }

                tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                if POLL_GEN.load(Ordering::SeqCst) != gen {
                    return;
                }

                match auth::qrcode_poll(&http, &qr.key).await {
                    Ok(PollState::Waiting) => {}
                    Ok(PollState::Scanned) => {
                        let w = w_weak.clone();
                        let _ = w.upgrade_in_event_loop(|win| {
                            win.set_login_status("已扫码，请在手机上确认".into());
                        });
                    }
                    Ok(PollState::Expired) => {
                        let w = w_weak.clone();
                        let _ = w.upgrade_in_event_loop(|win| {
                            win.set_login_status("二维码已过期，请刷新".into());
                            win.set_login_qr_loading(false);
                        });
                        crate::mxprobe::log_public("[Login] 二维码过期");
                        finish(gen);
                        return;
                    }
                    Ok(PollState::Success(mut a)) => {
                        // 补齐昵称/头像/等级：poll 只给 cookie，不给用户信息
                        if let Err(e) = auth::fetch_nav(&http, &mut a).await {
                            crate::mxprobe::log_public(&format!(
                                "[Login] 拿 cookie 成功但 nav 失败（{e}），仍按已登录处理"
                            ));
                        }

                        // 先清掉"生成中"：否则登录成功了面板还停在 loading 态，
                        // 用户看到的就是"扫完了没反应"。
                        if let Some(w) = global_window() {
                            let _ = w.upgrade_in_event_loop(|win| {
                                win.set_login_qr_loading(false);
                                win.set_login_status("登录成功".into());
                                win.set_show_login(false);
                            });
                        }

                        persist_and_apply(*a, client.clone());
                        finish(gen);

                        // 登录后立刻刷新评论区：未登录只有 3 条，登录后有 20 条/页
                        let cur_bvid = crate::player::current_bvid();
                        if !cur_bvid.is_empty() {
                            crate::player::reload_comments(client.clone(), cur_bvid);
                        }
                        return;
                    }
                    Err(e) => {
                        crate::mxprobe::log_public(&format!("[Login] 轮询出错: {e}"));
                        // 不中断：网络抖动很常见，继续下一轮
                    }
                }
            }

            let w = w_weak.clone();
            let _ = w.upgrade_in_event_loop(|win| {
                win.set_login_status("二维码已过期，请刷新".into());
                win.set_login_qr_loading(false);
            });
            finish(gen);
        });
    });
}

/// 收尾：只有当代数仍然是自己的时候才把 POLLING 复位。
///
/// 为什么必须判代数：用户点了"刷新"之后，新线程已经把 POLLING 置 true，
/// 旧线程若此时无条件置 false，新线程就再也没人置回去了 ——
/// 下一次点登录会被当成"已经在轮询中"。
fn finish(gen: u64) {
    if POLL_GEN.load(Ordering::SeqCst) == gen {
        POLLING.store(false, Ordering::SeqCst);
    }
}

/// 取消当前轮询（用户关掉了登录面板 / 点了退出 / 点了刷新）。
///
/// 只推进代数，**不**复位 POLLING —— 复位由 start() 负责。
/// 如果这里也复位，会出现这个竞态：
///   cancel() 复位 → 旧线程还在跑 → start() 看到 false 就开新线程
///   → 两个线程同时轮询同一个 key，其中一个拿到 86038 就把
///     刚生成的新二维码判成过期。
pub fn cancel() {
    POLL_GEN.fetch_add(1, Ordering::SeqCst);
}

/// 退出登录。
pub fn logout(window: &AppWindow, client: bli_protocol::BiliClient) {
    cancel();

    crate::settings::remove(KEY_COOKIE);
    crate::settings::remove(KEY_MID);

    client.set_auth(None);
    if let Ok(mut g) = AUTH.lock() {
        *g = None;
    }
    FACE_GEN.fetch_add(1, Ordering::SeqCst);

    crate::mxprobe::log_public("[Login] 已退出登录");

    let w = window.as_weak();
    let _ = w.upgrade_in_event_loop(|win| {
        win.set_logged_in(false);
        win.set_login_name("".into());
        win.set_login_face_url("".into());
        win.set_user_face(Image::default());
        win.set_login_level(0);
        win.set_login_vip(false);
        win.set_login_status("".into());
        win.set_login_qr(Image::default());
        win.set_show_login(false);
    });

    // 退出后评论区要退回未登录形态（3 条热门）
    let cur_bvid = crate::player::current_bvid();
    if !cur_bvid.is_empty() {
        crate::player::reload_comments(client, cur_bvid);
    }
}

/// 发评论 / 回复。
///
/// 成功后重新拉一次第一页评论 —— 本地插一条会漏掉楼中楼、
/// 时间格式化、等级徽章这些服务端算好的字段，重新拉更省事也更新鲜。
pub fn post_comment(
    window: &AppWindow,
    client: bli_protocol::BiliClient,
    aid: u64,
    bvid: String,
    text: String,
    root: u64,
    parent: u64,
) {
    let Some(a) = current() else {
        crate::mxprobe::log_public("[Comment] 未登录，无法发送");
        let w = window.as_weak();
        let _ = w.upgrade_in_event_loop(|win| {
            win.set_show_login(true);
            win.set_login_status("回复前需要先登录".into());
        });
        return;
    };

    if text.trim().is_empty() {
        return;
    }

    crate::mxprobe::log_public(&format!(
        "[Comment] 发送中 aid={aid} root={root} parent={parent} 长度={}",
        text.chars().count()
    ));

    let w_weak = window.as_weak();
    {
        let w = w_weak.clone();
        let _ = w.upgrade_in_event_loop(|win| {
            win.set_comment_sending(true);
        });
    }

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return,
        };

        rt.block_on(async {
            let http = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    report(&w_weak, format!("网络错误: {e}"));
                    finish_sending(&w_weak);
                    return;
                }
            };

            match auth::reply_add(&http, &a, aid, text.trim(), root, parent).await {
                Ok(()) => {
                    crate::mxprobe::log_public("[Comment] 发送成功");
                    let w = w_weak.clone();
                    let _ = w.upgrade_in_event_loop(|win| {
                        win.set_comment_text("".into());
                        win.set_comment_sending(false);
                        win.set_comment_status("已发送".into());
                    });
                    // 重新拉评论，让新评论出现
                    crate::player::reload_comments(client, bvid);
                }
                Err(e) => {
                    crate::mxprobe::log_public(&format!("[Comment] 发送失败: {e}"));
                    let msg = format!("发送失败: {e}");
                    let w = w_weak.clone();
                    let _ = w.upgrade_in_event_loop(move |win| {
                        win.set_comment_sending(false);
                        win.set_comment_status(msg.as_str().into());
                    });
                }
            }
        });
    });
}

fn finish_sending(w: &slint::Weak<AppWindow>) {
    let w = w.clone();
    let _ = w.upgrade_in_event_loop(|win| {
        win.set_comment_sending(false);
    });
}

fn report(w: &slint::Weak<AppWindow>, msg: String) {
    crate::mxprobe::log_public(&format!("[Login] {msg}"));
    let w = w.clone();
    let _ = w.upgrade_in_event_loop(move |win| {
        win.set_login_status(msg.as_str().into());
        win.set_login_qr_loading(false);
    });
}

/// 把二维码 URL 画成 RGBA 位图。
///
/// 为什么自己画而不是请求 B 站给图片：
///   generate 接口只给 URL 字符串，没有图。要么引一个能画 QR 的库
///   （qrcodegen），要么把 URL 交给 WebView 渲染 —— 后者要额外造一个
///   不可见的 WebView，对一个纯 Rust 项目来说太重。
///
/// qrcodegen 输出的是 bool 矩阵。这里做两件事：
///   1. 加 quiet zone（四周留 4 个模块的白边，扫描器认这个）
///   2. 每个模块放大成 scale x scale 的方块
///
/// 返回 (边长, RGBA 字节)。边长是正方形。
fn render_qr(text: &str) -> Option<(u32, Vec<u8>)> {
    use qrcodegen::{QrCode, QrCodeEcc};

    // Ecc::Medium 在容错率和模块数之间平衡：URL 约 100 字符时
    // 生成 41x41 左右的矩阵，画到 480px 上每个模块约 11px，很清晰。
    let code = QrCode::encode_text(text, QrCodeEcc::Medium).ok()?;
    let modules = code.size();
    // quiet zone 4 个模块是 QR 规范要求的最小值
    let quiet = 4;
    let total = modules + quiet * 2;
    // 目标边长 480px：UI 上显示约 200dp，2.4x 密度下正好
    let target = 480u32;
    let scale = (target / total as u32).max(1);
    let side = total as u32 * scale;

    let mut data = vec![0u8; (side * side * 4) as usize];
    // 先全部填白（含 quiet zone）
    for px in data.chunks_exact_mut(4) {
        px[0] = 255;
        px[1] = 255;
        px[2] = 255;
        px[3] = 255;
    }

    for my in 0..modules {
        for mx in 0..modules {
            if !code.get_module(mx as i32, my as i32) {
                continue;
            }
            let x0 = (mx + quiet) as u32 * scale;
            let y0 = (my + quiet) as u32 * scale;
            for dy in 0..scale {
                for dx in 0..scale {
                    let x = x0 + dx;
                    let y = y0 + dy;
                    let idx = ((y * side + x) * 4) as usize;
                    data[idx] = 0;
                    data[idx + 1] = 0;
                    data[idx + 2] = 0;
                    data[idx + 3] = 255;
                }
            }
        }
    }

    Some((side, data))
}

/// 供协议层的 comments_page 使用：把当前登录态和 client 一起给出去。
pub fn auth_opt() -> Option<Auth> {
    current()
}

/// 登录后重新拉评论时要用到的 UI 数据填充入口。
///
/// 放在这里而不是 player 里，是因为评论的解析逻辑（parse_reply）
/// 已经在 detail.rs 里，这里只负责"把 json 变成 UI 模型"。
pub fn build_comment_models(
    json: &serde_json::Value,
    owner_mid: u64,
) -> Vec<bli_protocol::CommentItem> {
    let replies = json["data"]["replies"]
        .as_array()
        .or_else(|| json["data"]["top_replies"].as_array())
        .cloned()
        .unwrap_or_default();

    replies
        .iter()
        .map(|r| bli_protocol::detail::parse_reply_public(r, owner_mid))
        .collect()
}
