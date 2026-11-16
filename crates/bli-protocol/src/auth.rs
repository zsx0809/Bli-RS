// ==============================================================================
// 登录态
//
// 为什么走**扫码登录**而不是账号密码：
//
//   1. 密码登录要 RSA。B 站 web 端密码是 RSA(PKCS1) 加密后再发的，
//      公钥来自 /x/passport-login/web/key，加 salt 拼在密码前。
//      这在纯 Rust 侧要引 rsa + pkcs8 + base64，还要处理加密填充，
//      是一整条依赖链。
//
//   2. 更致命的是**极验（Geetest）**。B 站现在对密码登录基本必出
//      滑块/点选验证，那个验证码是 JS 在 WebView 里跑的，native 侧
//      没有可行的替代实现。
//
//   3. 扫码登录只需要两个 HTTP GET：
//        /x/passport-login/web/qrcode/generate  → 拿 url + qrcode_key
//        /x/passport-login/web/qrcode/poll      → 轮询状态
//      第一次 poll 返回 code=0 时，**Set-Cookie 里直接就是完整登录态**。
//      用户拿手机 B 站 App 扫一下，我们这边就登录好了。
//
//   实测（2026-11-16，本机 curl）：
//     generate → {"code":0,"data":{"url":"https://account.bilibili.com/h5/...",
//                                  "qrcode_key":"e1cdcd22..."}}
//     poll     → {"code":0,"data":{"code":86101,"message":"未扫码"}}
//
//   poll 的 data.code 语义（照 PiliPlus 的注释）：
//     0     = 成功，此时响应头带 Set-Cookie
//     86101 = 未扫码
//     86090 = 已扫码，待确认
//     86038 = 二维码已失效
//
// ==============================================================================

use crate::{api, Error, Result};

/// 登录态：整条 cookie 串原样保存。
///
/// 为什么不拆成 SESSDATA / bili_jct / DedeUserID 三个字段：
///   B 站下发的 cookie 不止这三个（还有 buvid3/buvid4/b_nut/SESSDATA 等
///   七八个），而且**登录会刷新 buvid**。只挑三个拼回去会让某些接口
///   （尤其是 playurl 的高清档、评论发表）判定为异常登录。
///   原样保存整串最稳。
///
/// 唯一单独取出来的是 `bili_jct`：它是 CSRF token，
/// 所有 POST（发评论、点赞、投币）都必须作为 `csrf` 参数带上。
#[derive(Debug, Clone, Default)]
pub struct Auth {
    /// 完整 cookie 串，形如 "SESSDATA=xxx; bili_jct=yyy; ..."
    pub cookie: String,
    /// CSRF token（就是 cookie 里的 bili_jct）
    pub csrf: String,
    /// 用户 mid
    pub mid: u64,
    /// 昵称
    pub uname: String,
    /// 头像 URL
    pub face: String,
    /// 是否大会员
    pub is_vip: bool,
    /// 等级
    pub level: u32,
}

impl Auth {
    pub fn is_logged_in(&self) -> bool {
        !self.cookie.is_empty() && !self.csrf.is_empty()
    }

    /// 从 Set-Cookie 头列表里拼出 cookie 串。
    ///
    /// 输入是 reqwest 的 `HeaderMap::get_all("set-cookie")` 逐条拿到的值，
    /// 形如 `SESSDATA=abc123; Path=/; Domain=.bilibili.com; HttpOnly`。
    /// 我们只要 `name=value` 那一段。
    pub fn from_set_cookie(raw: &[String]) -> Self {
        let mut parts: Vec<String> = Vec::new();
        let mut csrf = String::new();

        for line in raw {
            // 一条 Set-Cookie 里的第一段才是 name=value
            let Some(first) = line.split(';').next() else {
                continue;
            };
            let first = first.trim();
            if first.is_empty() {
                continue;
            }
            let Some((name, value)) = first.split_once('=') else {
                continue;
            };
            if name.trim() == "bili_jct" {
                csrf = value.trim().to_string();
            }
            parts.push(format!("{}={}", name.trim(), value.trim()));
        }

        // DedeUserID 也在 cookie 里，顺手取出来当 mid
        let mid = parts
            .iter()
            .find_map(|p| p.strip_prefix("DedeUserID="))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);

        Self {
            cookie: parts.join("; "),
            csrf,
            mid,
            ..Default::default()
        }
    }

    /// 从已保存的 cookie 串恢复（启动时读盘用）。
    pub fn from_cookie_string(cookie: &str) -> Self {
        let mut csrf = String::new();
        let mut mid = 0u64;
        for part in cookie.split(';') {
            let part = part.trim();
            if let Some((name, value)) = part.split_once('=') {
                match name.trim() {
                    "bili_jct" => csrf = value.trim().to_string(),
                    "DedeUserID" => mid = value.trim().parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
        Self {
            cookie: cookie.to_string(),
            csrf,
            mid,
            ..Default::default()
        }
    }
}

/// 扫码登录第一步的产物。
pub struct QrCode {
    /// 二维码内容（一个 https URL，B 站 App 扫它）
    pub url: String,
    /// 轮询用的 key
    pub key: String,
}

/// 申请一个登录二维码。
pub async fn qrcode_generate(client: &reqwest::Client) -> Result<QrCode> {
    let url = format!("{}{}", api::PASSPORT_BASE_URL, api::QRCODE_GENERATE);

    let resp = client
        .get(&url)
        .header("user-agent", api::BROWSER_UA_PC)
        .header("referer", api::REFERER)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(Error::Network(format!("HTTP {}", resp.status())));
    }

    let json: serde_json::Value = resp.json().await?;
    let code = json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = json
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("申请二维码失败")
            .to_string();
        return Err(Error::Api { code, message: msg });
    }

    let data = &json["data"];
    let url = data["url"]
        .as_str()
        .ok_or_else(|| Error::Decode("二维码响应缺少 url".into()))?
        .to_string();
    let key = data["qrcode_key"]
        .as_str()
        .ok_or_else(|| Error::Decode("二维码响应缺少 qrcode_key".into()))?
        .to_string();

    Ok(QrCode { url, key })
}

/// 轮询结果。
pub enum PollState {
    /// 还没扫
    Waiting,
    /// 扫了，等用户在手机上点确认
    Scanned,
    /// 二维码过期，要重新申请
    Expired,
    /// 登录成功
    Success(Box<Auth>),
}

/// 轮询一次扫码状态。
///
/// 注意：登录成功时，登录态在 **响应头 Set-Cookie** 里，不在 body 里。
/// body 只有一个 `{"code":0,"data":{"code":0,"message":"","url":"...","refresh_token":"..."}}`。
/// 所以要拿 header，不能只 `resp.json()`。
pub async fn qrcode_poll(client: &reqwest::Client, key: &str) -> Result<PollState> {
    let url = format!("{}{}", api::PASSPORT_BASE_URL, api::QRCODE_POLL);

    let resp = client
        .get(&url)
        .query(&[("qrcode_key", key)])
        .header("user-agent", api::BROWSER_UA_PC)
        .header("referer", api::REFERER)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(Error::Network(format!("HTTP {}", resp.status())));
    }

    // 先抓 Set-Cookie，再消费 body
    let set_cookies: Vec<String> = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .collect();

    let json: serde_json::Value = resp.json().await?;
    let code = json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = json
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("轮询失败")
            .to_string();
        return Err(Error::Api { code, message: msg });
    }

    let inner = json["data"]["code"].as_i64().unwrap_or(-1);
    match inner {
        0 => {
            let auth = Auth::from_set_cookie(&set_cookies);
            if !auth.is_logged_in() {
                // 极少数情况下 cookie 会在下一个请求才下发，这里当作失败，
                // 让上层重试而不是硬塞一个空登录态进去。
                return Err(Error::Decode(
                    "登录成功但没拿到 cookie（可能被风控拦截）".into(),
                ));
            }
            Ok(PollState::Success(Box::new(auth)))
        }
        86101 => Ok(PollState::Waiting),
        86090 => Ok(PollState::Scanned),
        86038 => Ok(PollState::Expired),
        // 其它（如 86100/86102）统一当等待，不打断用户
        _ => Ok(PollState::Waiting),
    }
}

/// 用登录态补齐用户信息（昵称、头像、等级、大会员）。
///
/// `/x/web-interface/nav` 未登录返回 code=-101，已登录返回 code=0 + data。
/// 这里拿到的东西写回 Auth，UI 才能显示"已登录：某某某"。
pub async fn fetch_nav(client: &reqwest::Client, auth: &mut Auth) -> Result<()> {
    let url = format!("{}{}", api::API_BASE_URL, api::NAV);

    let resp = client
        .get(&url)
        .header("user-agent", api::BROWSER_UA_PC)
        .header("referer", api::REFERER)
        .header("cookie", &auth.cookie)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(Error::Network(format!("HTTP {}", resp.status())));
    }

    let json: serde_json::Value = resp.json().await?;
    let code = json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = json
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("获取用户信息失败")
            .to_string();
        return Err(Error::Api { code, message: msg });
    }

    let data = &json["data"];
    if data["isLogin"].as_bool() != Some(true) {
        return Err(Error::Other("cookie 无效（isLogin=false）".into()));
    }

    auth.mid = data["mid"].as_u64().unwrap_or(auth.mid);
    auth.uname = data["uname"].as_str().unwrap_or("").to_string();
    auth.face = data["face"].as_str().unwrap_or("").to_string();
    auth.level = data["level_info"]["current_level"].as_u64().unwrap_or(0) as u32;
    // vipStatus>0 且 vipType==2 才是有效大会员（与评论区的判定一致）
    let vip_status = data["vipStatus"].as_u64().unwrap_or(0);
    let vip_type = data["vipType"].as_u64().unwrap_or(0);
    auth.is_vip = vip_status > 0 && vip_type == 2;

    Ok(())
}

/// 发表评论 / 回复。
///
/// 参数对应 B 站的 reply/add 合约：
///   type   = 1（视频）
///   oid    = aid
///   root   = 根评论 rpid（回复一级评论时传它）
///   parent = 被回复的那条 rpid（楼中楼里回复别人时传）
///   message= 正文，最多 1000 字
///   csrf   = cookie 里的 bili_jct
///
/// root / parent 都为 0 时就是发一条顶层评论。
pub async fn reply_add(
    client: &reqwest::Client,
    auth: &Auth,
    aid: u64,
    message: &str,
    root: u64,
    parent: u64,
) -> Result<()> {
    if !auth.is_logged_in() {
        return Err(Error::Other("未登录".into()));
    }

    let url = format!("{}{}", api::API_BASE_URL, api::REPLY_ADD);

    let mut form: Vec<(String, String)> = vec![
        ("type".into(), "1".into()),
        ("oid".into(), aid.to_string()),
        ("message".into(), message.to_string()),
        ("csrf".into(), auth.csrf.clone()),
        // plat=2 表示安卓客户端，风控对它的容忍度比 web 高
        ("plat".into(), "2".into()),
    ];
    if root > 0 {
        form.push(("root".into(), root.to_string()));
    }
    if parent > 0 {
        form.push(("parent".into(), parent.to_string()));
    }

    let resp = client
        .post(&url)
        .header("user-agent", api::BROWSER_UA_PC)
        .header("referer", api::REFERER)
        .header("cookie", &auth.cookie)
        .form(&form)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(Error::Network(format!("HTTP {}", resp.status())));
    }

    let json: serde_json::Value = resp.json().await?;
    let code = json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = json
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("发送失败")
            .to_string();
        return Err(Error::Api { code, message: msg });
    }

    Ok(())
}

/// 拉已登录态下的评论列表。
///
/// 这是「评论区永远只有三条」的根因所在 —— 两条路径**完全不同**：
///
///   未登录：GET /x/v2/reply/wbi/main
///           参数 oid / type / mode / pagination_str
///           只给 3 条热门，reply_control 里**没有 location**（IP 属地）
///
///   已登录：GET /x/v2/reply
///           参数 oid / type / sort / pn / ps
///           按页给 20 条，带 location、楼中楼预置、rcount
///
/// 对照 PiliPlus lib/http/reply.dart:replyList —— 它也是这么分叉的
/// （`isLogin ? Api.replyList : '${Api.replyList}/main'`）。
///
/// sort: 0 = 按时间，1 = 按热度（PiliPlus 里的 `sort + 2` 是给未登录
///       那条路径折算 mode 用的，登录路径直接传 sort）。
pub async fn comments_page(
    client: &reqwest::Client,
    auth: Option<&Auth>,
    aid: u64,
    sort: u32,
    pn: u32,
) -> Result<serde_json::Value> {
    let logged_in = auth.map(|a| a.is_logged_in()).unwrap_or(false);

    let (path, params): (&str, Vec<(&str, String)>) = if logged_in {
        (
            "/x/v2/reply",
            vec![
                ("oid", aid.to_string()),
                ("type", "1".to_string()),
                ("sort", sort.to_string()),
                ("pn", pn.to_string()),
                ("ps", "20".to_string()),
            ],
        )
    } else {
        // 未登录：仍然用 wbi/main，分页靠 pagination_str。
        // 第一页给空 offset 即可（服务端会返回下一页的 offset 在
        // data.cursor.pagination_reply.next_offset 里）。
        (
            "/x/v2/reply/wbi/main",
            vec![
                ("oid", aid.to_string()),
                ("type", "1".to_string()),
                ("mode", "3".to_string()),
                ("pagination_str", "{\"offset\":\"\"}".to_string()),
            ],
        )
    };

    let url = format!("{}{path}", api::API_BASE_URL);

    let mut req = client
        .get(&url)
        .header("user-agent", api::BROWSER_UA_PC)
        .header("referer", api::REFERER)
        .query(&params);

    if let Some(a) = auth {
        if a.is_logged_in() {
            req = req.header("cookie", &a.cookie);
        }
    }

    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(Error::Network(format!("HTTP {}", resp.status())));
    }

    Ok(resp.json().await?)
}
