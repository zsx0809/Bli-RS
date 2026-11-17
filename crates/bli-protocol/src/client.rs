// ==============================================================================
// HTTP 客户端
//
// 为什么用 reqwest + rustls：
//   - rustls 是纯 Rust TLS 实现，交叉编译到 Android 不需要 OpenSSL 的 C 依赖。
//     这一点在 chroot 交叉编译时是决定性的（openssl-src 需要完整的 C 工具链）。
//   - ring（rustls 的密码学后端）的 C/汇编部分已实测可在 Termux clang 下编译。
//
// 为什么要自定义 UA 和 Referer：
//   B 站对无 Referer 的图片请求返回 403（防盗链），
//   对非浏览器 UA 可能返回精简过的 JSON。这两个头是接口合约的一部分。
// ==============================================================================

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::api;
use crate::auth::Auth;
use crate::wbi;
use crate::{Error, Result};

#[derive(Clone)]
pub struct BiliClient {
    http: reqwest::Client,
    /// 缓存 mixin_key，避免每个请求都去拉 nav。
    /// 用 Arc<Mutex> 是因为要跨 await 点共享，且 Clone 给多个并发任务用。
    mixin: Arc<tokio::sync::Mutex<Option<(String, i64, bool)>>>,
    /// 设备指纹 cookie（buvid3）。
    ///
    /// 为什么必须有：推荐流不需要它，但 playurl / view 这类接口会校验。
    /// 缺了直接返回 HTTP 412（Precondition Failed）——不是 JSON 错误码，
    /// 是风控在 HTTP 层就拦掉了，连响应体都没有。
    /// 格式照抄 PiliPlus IdUtils.genBuvid3：UUID(大写) + 5位随机 + "infoc"。
    buvid: String,
    /// 登录态。None = 游客。
    ///
    /// 为什么挂在 client 上而不是各接口自己收参数：
    ///   登录之后**每一个** bilibili 请求都要带 cookie —— 评论分页
    ///   （不带就只有 3 条热门）、playurl 的高清档、IP 属地、
    ///   点赞投币、发评论。逐个接口传参一定会漏，漏掉的地方表现
    ///   还很隐蔽（比如评论区"只有三条"）。挂在 client 上一处生效。
    ///
    /// 用 std 的 RwLock 而不是 tokio 的：读多写极少，且持锁期间不做 await。
    auth: Arc<std::sync::RwLock<Option<Auth>>>,
}

impl BiliClient {
    pub fn new() -> Result<Self> {
        Self::with_buvid(None)
    }

    /// 用指定的 buvid3 创建客户端。传 None 则随机生成一个。
    ///
    /// ★ 为什么 buvid 必须持久化，不能每次启动都换新的：
    ///
    ///   buvid 是 B 站识别"这台设备"的唯一标识。推荐引擎用它累积
    ///   行为画像（看过什么、看了多久、点了什么）。如果每次启动都
    ///   生成一个新的，服务端看到的就是一台**全新的、零行为的设备**，
    ///   只能给出内容泛化的通用流 —— 也就是"推的都不是一类东西"。
    ///
    ///   调用方应当把首次生成的 buvid 存盘，之后一直复用。
    pub fn with_buvid(buvid: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            // 15 秒对 JSON 接口够用，但下载视频流会超时。
            // 这里统一放宽到 120 秒，接口层各自不需要再单独设置。
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10))
            // 必须显式开启：Cargo 的 feature 只是把解码器编进来，
            // 不调这两个方法 reqwest 不会发送 Accept-Encoding，
            // 也不会解压响应。之前 playurl 接口能通是因为那些响应很小、
            // 走的是 identity；视频流走压缩体就报 "error decoding response body"。
            .gzip(true)
            .brotli(true)
            // 连接池上限：移动端不需要太宽，太多反而占内存
            .pool_max_idle_per_host(4)
            .build()
            .map_err(|e| Error::Network(format!("创建 HTTP 客户端失败: {e}")))?;

        Ok(Self {
            http,
            mixin: Arc::new(tokio::sync::Mutex::new(None)),
            buvid: buvid.unwrap_or_else(gen_buvid3),
            auth: Arc::new(std::sync::RwLock::new(None)),
        })
    }

    /// 发送 GET 并解析为 JSON。**不**检查 code 字段。
    ///
    /// 分成「裸请求」和「带 code 检查」两层，是因为 B 站有些接口
    /// 会在 code != 0 的同时返回有效 payload（nav 未登录时就是 code=-101
    /// 但 wbi_img 完整可用）。统一在底层判 code 会把这些场景堵死。
    async fn get_raw(&self, url: &str, params: &BTreeMap<String, String>) -> Result<serde_json::Value> {
        let mut req = self.http.get(url).header("user-agent", api::BROWSER_UA_PC);

        // Referer 与 buvid3 cookie 只对 bilibili 域加，避免污染第三方请求。
        // buvid3 是风控必需的设备标识，缺了 playurl 会 412。
        //
        // 已登录时把登录 cookie 拼在 buvid3 后面：登录态下 B 站会刷新
        // buvid（buvid3/buvid4/b_nut），我们保存的整串里本来就带着，
        // 但首次本地生成的 buvid3 仍要保留 —— 两者并存 B 站取后者。
        if url.contains("bilibili.com") {
            req = req.header("referer", api::REFERER);
            req = req.header("cookie", self.cookie_header());
        }

        if !params.is_empty() {
            req = req.query(params);
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Network(format!("HTTP {status}")));
        }

        Ok(resp.json().await?)
    }

    /// 裸请求 + 业务码检查。绝大多数接口走这条。
    pub(crate) async fn get_checked(
        &self,
        url: &str,
        params: &BTreeMap<String, String>,
    ) -> Result<serde_json::Value> {
        let json = self.get_raw(url, params).await?;

        let code = json.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        if code != 0 {
            let msg = json
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("未知错误")
                .to_string();
            return Err(Error::Api { code, message: msg });
        }

        Ok(json)
    }

    /// App 端接口专用 GET：签名 + 完整 App 头，**不带 cookie**。
    ///
    /// 为什么单独一个方法而不是复用 get_with_headers：
    ///   1. App 端接口不认 cookie（PiliPlus 的 isApp 分支明确跳过
    ///      cookie 管理），带上反而可能与 sign 冲突
    ///   2. App 端要求 referer 缺失时补 www.bilibili.com，但不要
    ///      网页那套 cookie 逻辑
    ///
    /// 签名由调用方先做好（见 app_sign）。
    pub async fn get_app(
        &self,
        url: &str,
        params: &BTreeMap<String, String>,
        headers: &[(&str, String)],
    ) -> Result<serde_json::Value> {
        let mut req = self.http.get(url);

        for (k, v) in headers {
            req = req.header(*k, v);
        }
        if !headers.iter().any(|(k, _)| *k == "referer") {
            req = req.header("referer", api::REFERER);
        }

        if !params.is_empty() {
            req = req.query(params);
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Network(format!("HTTP {status}")));
        }

        let json: serde_json::Value = resp.json().await?;

        let code = json.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        if code != 0 {
            let msg = json
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("未知错误")
                .to_string();
            return Err(Error::Api { code, message: msg });
        }

        Ok(json)
    }

    /// 带自定义请求头的 GET（绝对 URL，不签名）。
    ///
    /// 用于 App 端接口：它们不用 WBI 签名，但要求一整套 App 请求头
    /// （User-Agent / buvid / fp_local / statistics / env …）。
    /// 少任何一个都可能被降级成网页访客身份，拿到的就是热榜而不是推荐。
    pub async fn get_with_headers(
        &self,
        url: &str,
        params: &BTreeMap<String, String>,
        headers: &[(&str, String)],
    ) -> Result<serde_json::Value> {
        let mut req = self.http.get(url);

        for (k, v) in headers {
            req = req.header(*k, v);
        }

        // cookie 统一由这里注入，避免调用方忘了带 buvid3 导致风控 412
        req = req.header("cookie", self.cookie_header());

        if !params.is_empty() {
            req = req.query(params);
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Network(format!("HTTP {status}")));
        }

        let json: serde_json::Value = resp.json().await?;

        let code = json.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        if code != 0 {
            let msg = json
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("未知错误")
                .to_string();
            return Err(Error::Api { code, message: msg });
        }

        Ok(json)
    }

    /// App 端请求签名（appsign）。
    ///
    /// 算法照抄 PiliPlus `AppSign.appSign`：
    ///   1. params 里塞 appkey 与 ts（秒级时间戳字符串）
    ///   2. 按 key 字典序排序
    ///   3. 拼成 `k1=v1&k2=v2`（每个 key/value 都做 URL 编码）
    ///   4. sign = MD5(那个串 + appSec)
    ///
    /// 返回的 sign 要作为 **query 参数** 一起发出去，不是 header。
    ///
    /// 为什么这个不能省：App 端推荐接口靠它区分"真 App 请求"和
    /// "带了个 App UA 的网页请求"。不签名的请求会被降级成网页访客，
    /// 推荐退化成通用流 —— 症状恰好就是"怎么刷都是不相干的内容"。
    pub(crate) fn app_sign(&self, params: &mut BTreeMap<String, String>) {
        use md5::{Digest, Md5};

        params.insert("appkey".into(), api::APP_KEY.into());
        params.insert(
            "ts".into(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .to_string(),
        );

        // BTreeMap 天然按 key 排序，正好符合要求
        let query = params
            .iter()
            .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        let mut hasher = Md5::new();
        hasher.update(query.as_bytes());
        hasher.update(api::APP_SEC.as_bytes());
        let sign = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();

        params.insert("sign".into(), sign);
    }

    /// 登录用户的 `x-bili-aurora-eid`：mid 的字节与固定密钥逐位异或后 base64。
    ///
    /// 照抄 PiliPlus `IdUtils.genAuroraEid`。未登录返回空串。
    pub(crate) fn aurora_eid(&self) -> String {
        use base64::Engine as _;

        let mid = match self.auth() {
            Some(a) if a.mid > 0 => a.mid,
            _ => return String::new(),
        };

        const KEY: &[u8] = b"ad1va46a7lza";
        let mut bytes = mid.to_string().into_bytes();
        for (i, b) in bytes.iter_mut().enumerate() {
            *b ^= KEY[i % KEY.len()];
        }
        base64::engine::general_purpose::STANDARD
            .encode(&bytes)
            .replace('=', "")
    }

    /// 激活 buvid（设备指纹上报）。
    ///
    /// 只在**首次**拿到新 buvid 时调用一次。成功之后同一 buvid 不需要重复激活
    /// （服务端已记住），失败也无所谓 —— 下次启动会再试。
    ///
    /// payload 结构与 PiliPlus `InitHttp.buvidActive` 完全一致：
    ///   一段随机字节（尾部嵌 "IEND" 伪装成 PNG 尾）base64 编码后截取 50 字符，
    ///   放进字段映射里。字段号（3064 / 39c8 / 3c43 / adca / bfe9）是
    ///   服务端约定的，不能改。
    ///
    /// 返回值只是"请求是否发出去了"，服务端返回什么不影响判定 ——
    /// 这个接口本身不返回有意义的业务数据。
    pub async fn activate_buvid(&self) -> Result<()> {
        use base64::Engine as _;

        // 32 个随机字节 + 固定的 PNG 尾标记 + 4 个随机字节
        let mut raw: Vec<u8> = Vec::with_capacity(45);
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15);
        let mut state = seed | 1;
        for _ in 0..32 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            raw.push((state & 0xff) as u8);
        }
        for b in [0u8, 0, 0, 0, 73, 69, 78, 68] {
            raw.push(b);
        }
        for _ in 0..4 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            raw.push((state & 0xff) as u8);
        }

        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        // 取尾部 50 字符（PiliPlus 用 substring(len-50)）
        let tail = if encoded.len() > 50 {
            &encoded[encoded.len() - 50..]
        } else {
            &encoded[..]
        };

        let payload = serde_json::json!({
            "3064": 1,
            "39c8": "333.1387.fp.risk",
            "3c43": {
                "adca": "Linux",
                "bfe9": tail,
            },
        });

        let url = format!("{}{}", api::API_BASE_URL, api::ACTIVATE_BUVID);

        let resp = self
            .http
            .post(&url)
            .header("user-agent", api::BROWSER_UA_PC)
            .header("referer", api::REFERER)
            .header("cookie", self.cookie_header())
            .header("content-type", "application/json")
            .json(&serde_json::json!({ "payload": payload.to_string() }))
            .send()
            .await?;

        log::debug!(
            "[Buvid] 激活请求已发送，HTTP {}",
            resp.status()
        );
        Ok(())
    }

    /// 暴露底层 HTTP 客户端。
    ///
    /// 给那些需要自己控制 header（比如带 cookie 的评论分页、发评论 POST）
    /// 的场景用。不鼓励在协议层之外直接发请求 —— 但把 reqwest::Client
    /// 包在 BiliClient 里会导致每个功能都要加一层方法，权衡下来提供
    /// 一个受控的逃生口更实际。
    pub fn raw_http(&self) -> reqwest::Client {
        self.http.clone()
    }

    /// 当前该带的 cookie 串。
    ///
    /// 已登录 → 登录 cookie（其后拼 buvid3，若登录串里没带）
    /// 未登录 → 只有 buvid3
    fn cookie_header(&self) -> String {
        let guard = match self.auth.read() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        match guard.as_ref() {
            Some(a) if !a.cookie.is_empty() => {
                if a.cookie.contains("buvid3=") {
                    a.cookie.clone()
                } else {
                    // 用**账号自带的** buvid，不是 client 上那个。
                    // 已登录时 a.buvid 才是这个账号的稳定设备标识。
                    let b = if a.buvid.is_empty() {
                        &self.buvid
                    } else {
                        &a.buvid
                    };
                    format!("{}; buvid3={}", a.cookie, b)
                }
            }
            _ => format!("buvid3={}", self.buvid),
        }
    }

    /// 当前生效的 buvid：已登录用账号的，未登录用 client 的（游客）。
    pub fn effective_buvid(&self) -> String {
        let guard = match self.auth.read() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        match guard.as_ref() {
            Some(a) if !a.buvid.is_empty() => a.buvid.clone(),
            _ => self.buvid.clone(),
        }
    }

    /// 对外暴露设备指纹（buvid3）。
    ///
    /// App 端接口把这个值放在 **HTTP header `buvid`** 里，而不是 cookie。
    /// 两者必须一致，否则风控会把请求与设备对不上而降级。
    pub fn buvid_string(&self) -> String {
        self.effective_buvid()
    }

    /// 装入 / 清空登录态。传 None 表示退出登录。
    ///
    /// 换登录态时必须**同时**清掉 mixin_key 缓存：
    /// WBI 密钥是按账号签发的，游客的 key 用在登录态请求上会被判 -352。
    pub fn set_auth(&self, auth: Option<Auth>) {
        let mut guard = match self.auth.write() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        *guard = auth;
        drop(guard);
        // mixin 是 async Mutex，这里用 try_lock —— 拿不到就让它下次自然过期，
        // 不值得为了清缓存去阻塞一个同步函数。
        if let Ok(mut m) = self.mixin.try_lock() {
            *m = None;
        }
    }

    /// 读一份登录态快照。
    pub fn auth(&self) -> Option<Auth> {
        let guard = match self.auth.read() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        guard.clone()
    }

    /// 是否已登录。
    pub fn is_logged_in(&self) -> bool {
        self.auth().map(|a| a.is_logged_in()).unwrap_or(false)
    }

    /// 普通 GET（无签名）。
    pub async fn get(
        &self,
        path: &str,
        params: BTreeMap<String, String>,
    ) -> Result<serde_json::Value> {
        self.get_checked(&format!("{}{}", api::API_BASE_URL, path), &params)
            .await
    }

    /// 带 Wbi 签名的 GET。
    ///
    /// 签名必须建立在**最终**参数集合上 —— 所以由调用方给全参数，
    /// 这里补 wts/w_rid 后再发。
    pub async fn get_signed(
        &self,
        path: &str,
        mut params: BTreeMap<String, String>,
    ) -> Result<serde_json::Value> {
        let key = self.mixin_key().await?;
        wbi::sign_with_key(&mut params, &key);
        self.get_checked(&format!("{}{}", api::API_BASE_URL, path), &params)
            .await
    }

    /// 直接下载一个二进制资源（视频/音频流）。
    ///
    /// CDN 的三个坑（都是实测踩出来的）：
    ///
    /// 1. **必须用 Range 请求**。
    ///    不带 Range 的整文件请求一律返回 HTTP 567（CDN 自定义拒绝码）。
    ///    带 `Range: bytes=0-` 立刻变 206。这大概是因为视频 CDN 只对
    ///    分段请求开放，整文件请求被当成爬虫。
    ///
    /// 2. **不要带 cookie**。带 buvid3 会让某些节点返回 567。
    ///
    /// 3. **必须开启 gzip/brotli 解码**，否则报 "error decoding response body"。
    pub async fn download_bytes(&self, url: &str) -> Result<Vec<u8>> {
        // 这里刻意另建一个**不解压**的 client。
        //
        // 原因：CDN 对 Range 请求返回的响应带 Content-Encoding 头，
        // 但实际 body 是未压缩的原始字节。reqwest 的自动解压会尝试
        // 按 gzip/br 解，直接报 "error decoding response body"。
        //
        // 我们明确发送 `Accept-Encoding: identity`，并关掉自动解压，
        // 拿到的就是裸字节，直接落盘即可。
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(15))
            // 不调用 .gzip()/.brotli() → 不发送压缩协商，也不解压
            .build()
            .map_err(|e| Error::Network(format!("创建下载客户端失败: {e}")))?;

        let resp = client
            .get(url)
            .header("user-agent", api::BROWSER_UA_PC)
            .header("referer", api::REFERER)
            .header("accept-encoding", "identity")
            .header("range", "bytes=0-")
            .send()
            .await?;

        let status = resp.status();
        if status != reqwest::StatusCode::PARTIAL_CONTENT && !status.is_success() {
            return Err(Error::Network(format!("HTTP {status}")));
        }

        Ok(resp.bytes().await?.to_vec())
    }

    /// 流式下载媒体流到本地文件，并在写入达到 `initial_threshold_bytes`（例如 1.5MB）时触发 `on_ready` 回调，
    /// 从而实现边下载边播放（首段即播，极速秒开）。
    /// 从指定偏移继续下载（断点续传）。
    ///
    /// `resume_from` > 0 时用 `Range: bytes=N-` 续传，并**追加**写入
    /// （不是截断重建）。这是长视频下载能在弱网下完成的前提：
    /// CDN 每隔几十秒就可能掐一次连接，没有续传就永远下不完。
    pub async fn download_stream_to_file_from<F>(
        &self,
        url: &str,
        dest_path: &std::path::Path,
        resume_from: u64,
        initial_threshold_bytes: u64,
        on_ready: F,
    ) -> Result<u64>
    where
        F: FnOnce() + Send + 'static,
    {
        use std::io::{Seek, SeekFrom, Write};

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| Error::Network(format!("创建下载客户端失败: {e}")))?;

        let range = if resume_from > 0 {
            format!("bytes={resume_from}-")
        } else {
            "bytes=0-".to_string()
        };

        let mut resp = client
            .get(url)
            .header("user-agent", api::BROWSER_UA_PC)
            .header("referer", api::REFERER)
            .header("accept-encoding", "identity")
            .header("range", range)
            .send()
            .await?;

        let status = resp.status();
        if status != reqwest::StatusCode::PARTIAL_CONTENT && !status.is_success() {
            return Err(Error::Network(format!("HTTP {status}")));
        }

        // 续传时打开已有文件并定位到末尾；首次则新建
        let mut file = if resume_from > 0 {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(dest_path)
                .map_err(|e| Error::Other(format!("打开缓存文件失败: {e}")))?;
            f.seek(SeekFrom::Start(resume_from))
                .map_err(|e| Error::Other(format!("定位写入位置失败: {e}")))?;
            f
        } else {
            std::fs::File::create(dest_path)
                .map_err(|e| Error::Other(format!("创建缓存文件失败: {e}")))?
        };

        let mut downloaded = resume_from;
        let mut ready_called = false;
        let mut on_ready_opt = Some(on_ready);

        while let Some(chunk) = resp.chunk().await.map_err(|e| Error::Network(e.to_string()))? {
            file.write_all(&chunk)
                .map_err(|e| Error::Other(format!("写入缓存文件失败: {e}")))?;
            downloaded += chunk.len() as u64;

            if !ready_called && downloaded >= initial_threshold_bytes {
                ready_called = true;
                file.flush().ok();
                if let Some(cb) = on_ready_opt.take() {
                    cb();
                }
            }
        }

        file.flush().ok();
        if !ready_called {
            if let Some(cb) = on_ready_opt.take() {
                cb();
            }
        }

        Ok(downloaded)
    }

    /// 用 HTTP Range 探测资源的**总长度**（Content-Range 里的 total）。
    ///
    /// 为什么必须单独问一次：
    ///   边下边播要给 AMediaExtractor 一个确定的 getSize()，
    ///   否则它卡死在解封装阶段（实测）。而下载是一次性的流式写盘，
    ///   写之前就得知道总长度，所以先发一个 Range: bytes=0-0 的小请求。
    ///
    /// 为什么用 `bytes=0-0` 而不是 HEAD：
    ///   B 站 CDN 对 HEAD 的支持不一致（部分节点 405）。
    ///   `bytes=0-0` 必定返回 206 + Content-Range: bytes 0-0/TOTAL。
    ///   代价是只下载 1 字节。
    pub async fn probe_content_length(&self, url: &str) -> Option<u64> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .ok()?;

        let resp = client
            .get(url)
            .header("user-agent", api::BROWSER_UA_PC)
            .header("referer", api::REFERER)
            .header("accept-encoding", "identity")
            .header("range", "bytes=0-0")
            .send()
            .await
            .ok()?;

        // Content-Range: bytes 0-0/12345678
        let cr = resp.headers().get("content-range")?.to_str().ok()?;
        let total = cr.rsplit('/').next()?.trim();
        total.parse::<u64>().ok()
    }

    pub async fn download_stream_to_file<F>(
        &self,
        url: &str,
        dest_path: &std::path::Path,
        initial_threshold_bytes: u64,
        on_ready: F,
    ) -> Result<()>
    where
        F: FnOnce() + Send + 'static,
    {
        use std::io::Write;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| Error::Network(format!("创建下载客户端失败: {e}")))?;

        let mut resp = client
            .get(url)
            .header("user-agent", api::BROWSER_UA_PC)
            .header("referer", api::REFERER)
            .header("accept-encoding", "identity")
            .header("range", "bytes=0-")
            .send()
            .await?;

        let status = resp.status();
        if status != reqwest::StatusCode::PARTIAL_CONTENT && !status.is_success() {
            return Err(Error::Network(format!("HTTP {status}")));
        }

        let mut file = std::fs::File::create(dest_path)
            .map_err(|e| Error::Other(format!("创建本地缓存文件失败: {e}")))?;

        let mut downloaded = 0u64;
        let mut ready_called = false;
        let mut on_ready_opt = Some(on_ready);

        while let Some(chunk) = resp.chunk().await.map_err(|e| Error::Network(e.to_string()))? {
            file.write_all(&chunk)
                .map_err(|e| Error::Other(format!("写入缓存文件失败: {e}")))?;
            downloaded += chunk.len() as u64;

            if !ready_called && downloaded >= initial_threshold_bytes {
                ready_called = true;
                file.flush().ok();
                if let Some(cb) = on_ready_opt.take() {
                    cb();
                }
            }
        }

        file.flush().ok();
        if !ready_called {
            if let Some(cb) = on_ready_opt.take() {
                cb();
            }
        }

        Ok(())
    }

    /// 取 mixin_key，当天有效。
    ///
    /// 关键点：nav 接口在**未登录**时返回 `code: -101`，但 `data.wbi_img`
    /// 是完全有效的。所以这里必须用 `get_raw` 而不是 `get_checked` ——
    /// 否则未登录用户永远拿不到签名密钥，推荐流直接不可用。
    /// WBI 密钥缓存键：(key, day, 是否登录).
    ///
    /// ★ 为什么要把"是否登录"放进缓存键：
    ///
    ///   WBI 密钥是**按身份签发**的。游客拿到的 mixin_key 与登录态拿到的
    ///   通常一致（都来自当日固定值），但混用会导致 w_rid 校验失败或
    ///   被服务端降级为游客视角 —— 表现就是"明明登录了，推荐还是泛化流"。
    ///
    ///   之前的实现只在 set_auth 时清缓存，但启动阶段是「先 client()、
    ///   再 restore 登录态、再立刻发 feed 请求」，中间任何一个用到
    ///   mixin_key 的调用都可能把游客密钥写进缓存。把登录态纳入键值
    ///   之后，两种状态的缓存互不干扰，换状态也必然重新取。
    pub async fn mixin_key(&self) -> Result<String> {
        let today = utc_day();
        let logged_in = self.is_logged_in();

        // 快速路径：已有当天、同身份的缓存
        {
            let guard = self.mixin.lock().await;
            if let Some((key, day, was_logged_in)) = guard.as_ref() {
                if *day == today && *was_logged_in == logged_in {
                    return Ok(key.clone());
                }
            }
        }

        // 慢路径：拉 nav（允许 -101）
        let url = format!("{}{}", api::API_BASE_URL, api::NAV);
        let json = self.get_raw(&url, &BTreeMap::new()).await?;

        let img = json["data"]["wbi_img"]["img_url"]
            .as_str()
            .ok_or_else(|| Error::Decode("nav: 缺少 wbi_img.img_url".into()))?;
        let sub = json["data"]["wbi_img"]["sub_url"]
            .as_str()
            .ok_or_else(|| Error::Decode("nav: 缺少 wbi_img.sub_url".into()))?;

        let key = wbi::get_mixin_key(&format!("{}{}", file_stem(img), file_stem(sub)));

        let mut guard = self.mixin.lock().await;
        *guard = Some((key.clone(), today, logged_in));
        Ok(key)
    }
}

impl BiliClient {
    /// 公开的 buvid3 生成器。
    ///
    /// 给宿主用：宿主负责把它存盘并在后续启动时复用一个固定值
    /// （见 `with_buvid` 的注释 —— 换 buvid 等于换设备，
    /// 推荐画像会被清零）。
    pub fn random_buvid3() -> String {
        gen_buvid3()
    }
}

/// 生成 buvid3。
///
/// 格式：8-4-4-4-12 的大写十六进制 UUID + 5 位十进制随机数 + "infoc"。
/// B 站只校验形态，不做服务端签发，所以本地随机即可。
/// 百分号编码，与 Dart 的 `Uri.encodeComponent` 行为一致：
/// 只保留 A-Z a-z 0-9 - _ . ! ~ * ' ( )
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => out.push(b as char),
            b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn gen_buvid3() -> String {
    // 不引 uuid crate：这里只需要"看起来随机且形态正确"，
    // 用系统时间的纳秒位混一个简单的线性同余就够了。
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678_9abc_def0);

    let mut state = seed | 1;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let h = |mut v: u64| -> String {
        let mut s = String::with_capacity(16);
        for _ in 0..16 {
            let nib = (v & 0xf) as u8;
            s.push((if nib < 10 { b'0' + nib } else { b'A' + nib - 10 }) as char);
            v >>= 4;
        }
        s
    };

    let a = h(next());
    let b = h(next());

    format!(
        "{}-{}-{}-{}-{}{:05}infoc",
        &a[0..8],
        &a[8..12],
        &a[12..16],
        &b[0..4],
        &b[4..16],
        next() % 100000
    )
}

fn utc_day() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86400) as i64)
        .unwrap_or(0)
}

fn file_stem(url: &str) -> &str {
    let last = url.rsplit('/').next().unwrap_or(url);
    match last.rfind('.') {
        Some(pos) => &last[..pos],
        None => last,
    }
}
