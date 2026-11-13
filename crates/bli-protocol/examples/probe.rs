// Bli-RS 协议层探针
//
// 用途：在真机/桌面上直接验证 B 站接口是否通、签名是否对、解析是否正常，
// 不必经过 UI。这是把「协议层」独立成 crate 的主要收益 ——
// 出问题时能立刻判断是网络/协议的问题，还是 UI 的问题。
//
// 用法:
//   cargo run -p bli-protocol --example probe
//   cargo run -p bli-protocol --example probe -- --pages 3

use bli_protocol::BiliClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pages: u32 = std::env::args()
        .skip_while(|a| a != "--pages")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    println!("=== Bli-RS 协议层探针 ===");

    let client = BiliClient::new()?;
    println!("[ok] HTTP 客户端已创建");

    // 1. Wbi 密钥
    let key = client.mixin_key().await?;
    println!("[ok] Wbi mixin_key = {key}");

    // 2. 推荐流
    let mut total = 0usize;
    let mut filtered = 0usize;

    for page in 0..pages {
        // fresh_idx: 0 首次, 之后用 4 表示加载更多
        let fresh_idx = if page == 0 { 0 } else { 4 };

        match client.recommend_feed(fresh_idx, 20).await {
            Ok(res) => {
                total += res.items.len();
                filtered += res.filtered_out;
                println!(
                    "[ok] 第 {} 页: {} 条有效, {} 条被过滤",
                    page + 1,
                    res.items.len(),
                    res.filtered_out
                );

                if page == 0 {
                    println!("\n--- 首页预览 ---");
                    for (i, v) in res.items.iter().take(6).enumerate() {
                        println!(
                            "{}. [{}] {} | {} | {} | {}",
                            i + 1,
                            v.duration,
                            v.title,
                            v.author,
                            v.view_text,
                            v.bvid
                        );
                        if i == 0 {
                            println!("    cover: {}", v.cover_url);
                        }
                    }
                    println!();
                }
            }
            Err(e) => {
                eprintln!("[!!] 第 {} 页失败: {e}", page + 1);
                return Err(e.into());
            }
        }
    }

    println!("\n=== 汇总 ===");
    println!("有效条目: {total}");
    println!("过滤条目: {filtered}");

    if total == 0 {
        eprintln!("[!!] 一条都没拿到 —— 接口形态可能变了");
        std::process::exit(1);
    }

    println!("[ok] 协议层工作正常");
    Ok(())
}
