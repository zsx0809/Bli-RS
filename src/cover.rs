// ==============================================================================
// 封面图加载
//
// 为什么需要这一层：
//   Slint 的 `Image.source` 只接受本地路径或内嵌资源，**不接受 http URL**。
//   所以必须自己下载、解码成像素缓冲区，再交给 Slint 的 Image。
//
// 尺寸策略（关键）：
//   B 站原图动辄 2475x1392 / 375KB。20 张卡就是 7.5MB 流量 + 巨大的解码内存。
//   用 CDN 的缩略图后缀 `@480w_270h_1c.webp` 后只有 28KB —— 体积差 13 倍。
//   卡片实际显示宽度约 180dp（2x 密度下约 360 物理像素），480w 足够。
//
// 线程模型（踩过的坑，别改）：
//   Slint 的 `Image`、`VecModel` 都**不是 Send**，不能跨线程持有。
//   所以后台线程只负责「下载 + 解码成原始 RGBA 字节」，
//   把纯数据的 `(bvid, 宽, 高, Vec<u8>)` 通过 channel 送回 UI 线程，
//   由 UI 线程构造 `Image` 并写入模型。
//   早期版本试图在后台线程里持有 Rc<VecModel> 回调，编译期就被 Send 拦下了。
// ==============================================================================

use std::sync::mpsc::Sender;

/// 解码完成的一张封面，纯数据、可跨线程。
pub struct DecodedCover {
    pub bvid: String,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// 把 B 站封面 URL 转成合适尺寸的缩略图 URL。
///
/// 两个变换：
/// 1. http -> https。B 站返回的封面 URL 是 http 的，Android 9+ 默认禁止
///    明文 HTTP，不转会直接被系统拦掉。
/// 2. 追加 CDN 缩略图后缀。已有 @ 后缀的不重复追加。
pub fn thumbnail_url(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }

    let https = if let Some(rest) = raw.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        raw.to_string()
    };

    // 已经带 @ 尺寸后缀就不动它
    if https.contains('@') {
        return https;
    }

    // 480w_270h_1c: 宽 480、高 270（16:9）、1c 裁切填满
    // webp 比 jpg 再省 30% 左右，Android 4.0+ 全部支持
    format!("{https}@480w_270h_1c.webp")
}

/// 在后台线程下载并解码一批封面，每张解码完就通过 channel 送回。
///
/// 逐张回传而不是「全部完成再发」：
///   20 张图串行下载要好几秒，用户会先看到一片空白。
///   逐张到达能立刻填充，体验上是「图片逐渐出现」。
pub fn spawn_cover_loader(items: Vec<(String, String)>, tx: Sender<DecodedCover>) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                log::warn!("封面加载：创建运行时失败: {e}");
                return;
            }
        };

        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(12))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                log::warn!("封面加载：创建客户端失败: {e}");
                return;
            }
        };

        // 并发下载。
        //
        // 之前是串行 for 循环：一张下完再下一张，每张 200-400ms，
        // 20 张要 6-8 秒 —— 表现为「一秒一张」。
        // 实际上瓶颈是手机网络的 RTT 而不是带宽（缩略图才 28KB），
        // 并发能把这 20 个 RTT 重叠起来，整体降到 1 秒内。
        //
        // 并发度取 6：移动端不算激进，能躲开大部分 CDN 的限流，
        // 也不会因为同时开太多连接而拖慢首屏的文字请求。
        const CONCURRENCY: usize = 6;

        rt.block_on(async move {
            let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
            let mut tasks = Vec::with_capacity(items.len());

            for (bvid, url) in items {
                let client = client.clone();
                let tx = tx.clone();
                let sem = sem.clone();

                tasks.push(tokio::spawn(async move {
                    // 拿到许可才开始下载，否则排队
                    let Ok(_permit) = sem.acquire().await else {
                        return;
                    };

                    let target = thumbnail_url(&url);
                    if target.is_empty() {
                        return;
                    }

                    // B 站 CDN 有防盗链：不带 Referer 返回 403
                    let Ok(resp) = client
                        .get(&target)
                        .header("referer", "https://www.bilibili.com")
                        .header("user-agent", UA)
                        .send()
                        .await
                    else {
                        log::warn!("封面 {bvid}: 请求失败");
                        return;
                    };

                    let Ok(bytes) = resp.bytes().await else {
                        log::warn!("封面 {bvid}: 读取响应体失败");
                        return;
                    };

                    // 解码是 CPU 密集操作，但仍放在这里：
                    // 20 张 480x270 的 webp 解码总共约 30-50ms，
                    // 分摊到 6 个并发任务上几乎不可感知，不值得再开线程池。
                    let Ok(decoded) = image::load_from_memory(&bytes) else {
                        log::warn!("封面 {bvid}: 解码失败");
                        return;
                    };

                    let rgba = decoded.to_rgba8();
                    let (width, height) = rgba.dimensions();

                    let _ = tx.send(DecodedCover {
                        bvid,
                        width,
                        height,
                        rgba: rgba.into_raw(),
                    });
                }));
            }

            // 等全部完成。
            // tx 的原始副本要在这里 drop，否则 channel 不会关闭，
            // UI 侧的 Timer 永远等不到 Disconnected，会一直空转。
            drop(tx);
            for t in tasks {
                let _ = t.await;
            }
        });
    });
}

pub const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.2 Safari/605.1.15";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrades_http_and_appends_thumbnail() {
        assert_eq!(
            thumbnail_url("http://i1.hdslb.com/bfs/archive/abc.jpg"),
            "https://i1.hdslb.com/bfs/archive/abc.jpg@480w_270h_1c.webp"
        );
    }

    #[test]
    fn keeps_existing_suffix() {
        assert_eq!(
            thumbnail_url("https://i0.hdslb.com/bfs/archive/abc.jpg@320w_180h_1c.webp"),
            "https://i0.hdslb.com/bfs/archive/abc.jpg@320w_180h_1c.webp"
        );
    }

    #[test]
    fn empty_stays_empty() {
        assert_eq!(thumbnail_url(""), "");
    }

    #[test]
    fn already_https_untouched_scheme() {
        assert_eq!(
            thumbnail_url("https://i2.hdslb.com/x.jpg"),
            "https://i2.hdslb.com/x.jpg@480w_270h_1c.webp"
        );
    }
}
