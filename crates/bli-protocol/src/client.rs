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
use crate::wbi;
use crate::{Error, Result};

#[derive(Clone)]
pub struct BiliClient {
    http: reqwest::Client,
    /// 缓存 mixin_key，避免每个请求都去拉 nav。
    /// 用 Arc<Mutex> 是因为要跨 await 点共享，且 Clone 给多个并发任务用。
    mixin: Arc<tokio::sync::Mutex<Option<(String, i64)>>>,
    /// 设备指纹 cookie（buvid3）。
    ///
    /// 为什么必须有：推荐流不需要它，但 playurl / view 这类接口会校验。
    /// 缺了直接返回 HTTP 412（Precondition Failed）——不是 JSON 错误码，
    /// 是风控在 HTTP 层就拦掉了，连响应体都没有。
    /// 格式照抄 PiliPlus IdUtils.genBuvid3：UUID(大写) + 5位随机 + "infoc"。
    buvid: String,
}

impl BiliClient {
    pub fn new() -> Result<Self> {
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
            buvid: gen_buvid3(),
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
        if url.contains("bilibili.com") {
            req = req
                .header("referer", api::REFERER)
                .header("cookie", format!("buvid3={}", self.buvid));
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
    pub async fn mixin_key(&self) -> Result<String> {
        let today = utc_day();

        // 快速路径：已有当天缓存
        {
            let guard = self.mixin.lock().await;
            if let Some((key, day)) = guard.as_ref() {
                if *day == today {
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
        *guard = Some((key.clone(), today));
        Ok(key)
    }
}

/// 生成 buvid3。
///
/// 格式：8-4-4-4-12 的大写十六进制 UUID + 5 位十进制随机数 + "infoc"。
/// B 站只校验形态，不做服务端签发，所以本地随机即可。
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
