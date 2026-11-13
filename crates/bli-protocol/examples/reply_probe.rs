// 评论接口字段探针：把真实响应里我们关心的字段全部打印出来。
//
// 目的：不靠猜字段名。B 站的 reply/wbi/main 响应结构很杂，
// 等级、IP 属地、楼中楼分别在 member.level_info / reply_control / replies 下，
// 具体路径写过一次就再也记不准了，所以留这个探针。
use bli_protocol::BiliClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bvid = std::env::args().nth(1).unwrap_or_else(|| "BV1WEWs66EBP".into());
    let c = BiliClient::new()?;
    let d = c.video_detail(&bvid).await?;
    let aid = d.aid;
    println!("aid = {aid}");

    let mut params = std::collections::BTreeMap::new();
    params.insert("oid".to_string(), aid.to_string());
    params.insert("type".to_string(), "1".to_string());
    params.insert("mode".to_string(), "3".to_string());
    let json = c.get_signed("/x/v2/reply/wbi/main", params).await?;

    let replies = json["data"]["replies"].as_array().cloned().unwrap_or_default();
    println!("顶层评论 {} 条\n", replies.len());

    for (i, r) in replies.iter().take(3).enumerate() {
        println!("===== 评论 {i} 原始结构 =====");
        println!("{}", serde_json::to_string_pretty(r).unwrap_or_default());
        println!();
    }
    Ok(())
}
