// 播放地址探针：验证能否拿到 DASH 轨道，并把视频流下载到本地供后续测试。
use bli_protocol::BiliClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bvid = std::env::args().nth(1).unwrap_or_else(|| "BV1PKWj62ERK".into());
    println!("=== playurl 探针: {bvid} ===");

    let c = BiliClient::new()?;

    let cid = c.video_cid(&bvid).await?;
    println!("[ok] cid = {cid}");

    let info = c.play_url(&bvid, cid).await?;
    println!("[ok] 时长 {} 秒", info.duration);
    println!("视频轨 {} 条，音频轨 {} 条", info.video.len(), info.audio.len());

    println!("\n--- 视频轨 ---");
    for t in &info.video {
        println!(
            "  id={:<4} {}x{} {:<22} {:>8} bps  {}",
            t.id,
            t.width.unwrap_or(0),
            t.height.unwrap_or(0),
            t.codecs,
            t.bandwidth,
            t.frame_rate.as_deref().unwrap_or("-")
        );
    }

    println!("\n--- 音频轨 ---");
    for t in &info.audio {
        println!("  id={:<6} {:<16} {:>8} bps", t.id, t.codecs, t.bandwidth);
    }

    let v = info.pick_video(Some(1080)).ok_or("没有可用视频轨")?;
    let a = info.pick_audio().ok_or("没有可用音频轨")?;
    println!("\n选中视频: {}x{} {}", v.width.unwrap_or(0), v.height.unwrap_or(0), v.codecs);
    println!("选中音频: {} {}", a.id, a.codecs);

    // 下载视频流到本地，供 MediaExtractor / MediaCodec 的可行性测试。
    //
    // 必须走 BiliClient::download_bytes —— 它内部做了三件事：
    //   1. 发 Range 请求（整文件请求会被 CDN 以 567 拒绝）
    //   2. 不带 cookie（带了反而触发风控）
    //   3. 不解压（CDN 的 Content-Encoding 头与实际 body 不符）
    // 直接 new 一个 reqwest::Client 下载会失败。
    let url = v.primary_url().unwrap();
    println!("\n下载视频流（{} 左右）…", v.bandwidth / 8);
    match c.download_bytes(url).await {
        Ok(bytes) => {
            std::fs::write("/tmp/sample_video.m4s", &bytes)?;
            println!("[ok] /tmp/sample_video.m4s  {} 字节", bytes.len());
        }
        Err(e) => println!("[!!] 视频下载失败: {e}"),
    }

    let aurl = a.primary_url().unwrap();
    println!("下载音频流…");
    match c.download_bytes(aurl).await {
        Ok(bytes) => {
            std::fs::write("/tmp/sample_audio.m4s", &bytes)?;
            println!("[ok] /tmp/sample_audio.m4s  {} 字节", bytes.len());
        }
        Err(e) => println!("[!!] 音频下载失败: {e}"),
    }

    Ok(())
}
