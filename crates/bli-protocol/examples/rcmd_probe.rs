use bli_protocol::BiliClient;

#[tokio::main]
async fn main() {
    let client = BiliClient::new().unwrap();
    for idx in [0u32, 1, 2] {
        match client.recommend_feed_app(idx).await {
            Ok(page) => {
                println!(
                    "idx={idx} raw={} kept={} filtered={}",
                    page.raw_count,
                    page.items.len(),
                    page.filtered_out
                );
                for it in page.items.iter().take(3) {
                    println!("   {} | {}", it.title.chars().take(26).collect::<String>(), it.author);
                }
            }
            Err(e) => println!("idx={idx} ERR {e}"),
        }
    }
}
