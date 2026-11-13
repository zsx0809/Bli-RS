// ==============================================================================
// 视频详情与相关推荐 (Video Detail & Related Videos)
//
// 对应接口：
//   - VIDEO_VIEW: /x/web-interface/wbi/view
//   - RELATED:    /x/web-interface/archive/related
// ==============================================================================

use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

use crate::api;
use crate::client::BiliClient;
use crate::Result;

/// 视频详细元数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoDetail {
    pub bvid: String,
    pub aid: u64,
    pub cid: u64,
    pub title: String,
    pub desc: String,
    pub pic: String,
    pub width: u32,
    pub height: u32,
    /// 是否为竖屏视频 (height > width)
    pub is_vertical: bool,
    pub owner_mid: u64,
    pub owner_name: String,
    pub owner_face: String,
    pub likes: u64,
    pub coins: u64,
    pub favorites: u64,
    pub shares: u64,
    pub views: u64,
    pub danmaku: u64,
    pub pubdate: i64,
}

impl VideoDetail {
    pub fn views_str(&self) -> String {
        format_num(self.views)
    }

    pub fn danmaku_str(&self) -> String {
        format_num(self.danmaku)
    }

    pub fn likes_str(&self) -> String {
        format_num(self.likes)
    }

    pub fn coins_str(&self) -> String {
        format_num(self.coins)
    }

    pub fn favorites_str(&self) -> String {
        format_num(self.favorites)
    }

    pub fn shares_str(&self) -> String {
        format_num(self.shares)
    }

    pub fn pubdate_str(&self) -> String {
        // 简单将时间戳转为大致日期表达，避免引入 chrono 重型依赖
        let secs = self.pubdate;
        if secs <= 0 {
            return String::new();
        }
        // 年/月/日 简单换算
        let days = secs / 86400;
        // 以 1970-01-01 为起点估算年月日
        let mut y = 1970;
        let mut d = days;
        loop {
            let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
            let ydays = if leap { 366 } else { 365 };
            if d < ydays {
                break;
            }
            d -= ydays;
            y += 1;
        }
        let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
        let mdays = [
            31,
            if leap { 29 } else { 28 },
            31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
        ];
        let mut m = 1;
        for &dim in &mdays {
            if d < dim {
                break;
            }
            d -= dim;
            m += 1;
        }
        format!("{y}年{m}月{}日", d + 1)
    }
}

/// 相关推荐视频条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedVideo {
    pub bvid: String,
    pub title: String,
    pub cover_url: String,
    pub author: String,
    pub views: String,
    pub danmaku: String,
    pub duration: String,
}

impl BiliClient {
    /// 拉取视频完整详情
    pub async fn video_detail(&self, bvid: &str) -> Result<VideoDetail> {
        let mut params = BTreeMap::new();
        params.insert("bvid".into(), bvid.to_string());

        let json = self.get_signed(api::VIDEO_VIEW, params).await?;
        let data = &json["data"];

        let aid = data["aid"].as_u64().unwrap_or(0);
        let cid = data["cid"].as_u64().unwrap_or(0);
        let title = data["title"].as_str().unwrap_or("").to_string();
        let desc = data["desc"].as_str().unwrap_or("").to_string();
        let pic = data["pic"].as_str().unwrap_or("").to_string();

        let width = data["dimension"]["width"].as_u64().unwrap_or(1920) as u32;
        let height = data["dimension"]["height"].as_u64().unwrap_or(1080) as u32;
        let is_vertical = height > width;

        let owner_mid = data["owner"]["mid"].as_u64().unwrap_or(0);
        let owner_name = data["owner"]["name"].as_str().unwrap_or("").to_string();
        let owner_face = data["owner"]["face"].as_str().unwrap_or("").to_string();

        let stat = &data["stat"];
        let likes = stat["like"].as_u64().unwrap_or(0);
        let coins = stat["coin"].as_u64().unwrap_or(0);
        let favorites = stat["favorite"].as_u64().unwrap_or(0);
        let shares = stat["share"].as_u64().unwrap_or(0);
        let views = stat["view"].as_u64().unwrap_or(0);
        let danmaku = stat["danmaku"].as_u64().unwrap_or(0);
        let pubdate = data["pubdate"].as_i64().unwrap_or(0);

        Ok(VideoDetail {
            bvid: bvid.to_string(),
            aid,
            cid,
            title,
            desc,
            pic,
            width,
            height,
            is_vertical,
            owner_mid,
            owner_name,
            owner_face,
            likes,
            coins,
            favorites,
            shares,
            views,
            danmaku,
            pubdate,
        })
    }

    /// 拉取相关推荐视频列表
    pub async fn related_videos(&self, bvid: &str) -> Result<Vec<RelatedVideo>> {
        let mut params = BTreeMap::new();
        params.insert("bvid".into(), bvid.to_string());

        let url = format!("{}{}", api::API_BASE_URL, api::RELATED);
        let json = self.get_checked(&url, &params).await?;

        let Some(list) = json["data"].as_array() else {
            return Ok(Vec::new());
        };

        let mut res = Vec::with_capacity(list.len());
        for v in list {
            let bvid = v["bvid"].as_str().unwrap_or("").to_string();
            if bvid.is_empty() {
                continue;
            }
            let title = v["title"].as_str().unwrap_or("").to_string();
            let cover_url = v["pic"].as_str().unwrap_or("").to_string();
            let author = v["owner"]["name"].as_str().unwrap_or("").to_string();
            let views = format_num(v["stat"]["view"].as_u64().unwrap_or(0));
            let danmaku = format_num(v["stat"]["danmaku"].as_u64().unwrap_or(0));

            let duration_sec = v["duration"].as_u64().unwrap_or(0);
            let m = duration_sec / 60;
            let s = duration_sec % 60;
            let duration = format!("{m}:{s:02}");

            res.push(RelatedVideo {
                bvid,
                title,
                cover_url,
                author,
                views,
                danmaku,
                duration,
            });
        }

        Ok(res)
    }

    /// 拉取视频评论列表 (oid 为 aid)
    ///
    /// `owner_mid` 是视频作者 mid，用来给评论打「UP」标。
    /// 传 0 表示不标注（调用方拿不到作者时）。
    pub async fn video_comments(&self, aid: u64, owner_mid: u64) -> Result<Vec<CommentItem>> {
        let mut params = BTreeMap::new();
        params.insert("oid".into(), aid.to_string());
        params.insert("type".into(), "1".into());
        params.insert("mode".into(), "3".into());

        // 走 WBI 签名请求，避免 -352 或 -403 风控拦截
        let json = self.get_signed(api::REPLY_LIST, params).await?;

        let Some(replies) = json["data"]["replies"].as_array() else {
            return Ok(Vec::new());
        };

        // 一次性诊断：确认这条链路上到底有没有 reply_control.location。
        //
        // 背景：PiliPlus 的做法是走 gRPC（reply_item_grpc.dart），
        // 而 Web 端点 /x/v2/reply/wbi/main 的字段集并不完全一致。
        // 不实测就只能猜，猜错的表现是"IP 属地永远不显示"且没有任何报错。
        if let Some(first) = replies.first() {
            let has_rc = first.get("reply_control").is_some();
            let loc = first["reply_control"]["location"].as_str().unwrap_or("");
            // 用 eprintln 而不是 log::info!：宿主的 Android 入口没有装
            // logger，log crate 的宏不会产生任何输出。eprintln 在 Android 上
            // 会进 logcat（tag 由 std 固定），至少能被抓到。
            eprintln!(
                "[reply] 首条顶层键数={} 有reply_control={} location={:?} 全部键={:?}",
                first.as_object().map(|o| o.len()).unwrap_or(0),
                has_rc,
                loc,
                first
                    .as_object()
                    .map(|o| o.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default()
            );
        }

        let res = replies
            .iter()
            .map(|r| parse_reply(r, owner_mid))
            .collect();

        Ok(res)
    }
}

/// 解析单条评论（含楼中楼）。
///
/// 字段路径对照 PiliPlus 的 models_new/reply/*.dart：
///   content.message         正文
///   member.uname / avatar   昵称 / 头像
///   member.level_info.current_level   等级
///   member.vip.vipStatus>0 && vipType==2  有效大会员
///   reply_control.location  "IP属地：广东"
///   replies[]               楼中楼（顶层通常预置前 3 条）
///   count                   总回复数
fn parse_reply(r: &serde_json::Value, owner_mid: u64) -> CommentItem {
    let member = &r["member"];
    let mid = member["mid"].as_u64().unwrap_or(0);

    let vip_status = member["vip"]["vipStatus"].as_u64().unwrap_or(0);
    let vip_type = member["vip"]["vipType"].as_u64().unwrap_or(0);

    // 属地：接口给的是 "IP属地：广东" 这种整串，前端直接用。
    // 极少数老数据放在 reply_control.location 之外，这里只认这一个位置。
    let location = r["reply_control"]["location"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    // 楼中楼：接口预置的那几条。它们自己不再带 replies，递归一层就够。
    let sub_replies = r["replies"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|sub| parse_reply_flat(sub, owner_mid))
                .collect()
        })
        .unwrap_or_default();

    CommentItem {
        rpid: r["rpid"].as_u64().unwrap_or(0),
        user_name: member["uname"].as_str().unwrap_or("").to_string(),
        user_face: member["avatar"].as_str().unwrap_or("").to_string(),
        content: r["content"]["message"].as_str().unwrap_or("").to_string(),
        likes: format_num(r["like"].as_u64().unwrap_or(0)),
        pubtime: format_relative_time(r["ctime"].as_i64().unwrap_or(0)),
        level: member["level_info"]["current_level"].as_u64().unwrap_or(0) as u32,
        // vipType == 2 才是「年度大会员」，其它值只是曾经开过
        is_vip: vip_status > 0 && vip_type == 2,
        is_up: owner_mid > 0 && mid == owner_mid,
        location,
        reply_count: r["rcount"]
            .as_u64()
            .or_else(|| r["count"].as_u64())
            .unwrap_or(0) as u32,
        sub_replies,
    }
}

/// 楼中楼条目：结构同上，但不再递归解析 replies（B 站只有两层）。
fn parse_reply_flat(r: &serde_json::Value, owner_mid: u64) -> CommentItem {
    let mut c = parse_reply(r, owner_mid);
    c.sub_replies = Vec::new();
    c
}

fn format_relative_time(secs: i64) -> String {
    if secs <= 0 {
        return "刚刚".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(secs);
    let diff = now - secs;
    if diff < 60 {
        "刚刚".to_string()
    } else if diff < 3600 {
        format!("{}分钟前", diff / 60)
    } else if diff < 86400 {
        format!("{}小时前", diff / 3600)
    } else if diff < 86400 * 30 {
        format!("{}天前", diff / 86400)
    } else {
        // 大于一个月简单格式化
        let days = secs / 86400;
        let mut y = 1970;
        let mut d = days;
        loop {
            let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
            let ydays = if leap { 366 } else { 365 };
            if d < ydays {
                break;
            }
            d -= ydays;
            y += 1;
        }
        let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
        let mdays = [
            31,
            if leap { 29 } else { 28 },
            31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
        ];
        let mut m = 1;
        for &dim in &mdays {
            if d < dim {
                break;
            }
            d -= dim;
            m += 1;
        }
        format!("{m}-{:02}", d + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn fetch_comments_real() {
        let client = BiliClient::new().unwrap();
        // 经典视频 aid: 170001 (BV17x411w7KC)
        let comments = client.video_comments(170001).await.unwrap();
        println!("获取到 {} 条评论", comments.len());
        assert!(!comments.is_empty());
        println!("首条评论: {}: {}", comments[0].user_name, comments[0].content);
    }
}

/// 评论区条目
///
/// 字段来源（对着 PiliPlus 的 models_new/reply 核对过，不是猜的）：
///   · 等级   member.level_info.current_level
///   · 大会员 member.vip.vipStatus / vipType（vipType==2 才是有效大会员）
///   · UP主标 mid 是否等于视频作者
///   · 属地   reply_control.location（形如 "IP属地：广东"）
///   · 楼中楼 replies[] —— 顶层评论里通常预置前 3 条
///   · 回复数 count
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentItem {
    pub rpid: u64,
    pub user_name: String,
    pub user_face: String,
    pub content: String,
    pub likes: String,
    pub pubtime: String,

    /// 用户等级 0~6
    pub level: u32,
    /// 是否大会员
    pub is_vip: bool,
    /// 是否视频作者（UP 主）
    pub is_up: bool,
    /// IP 属地，原始形如 "IP属地：广东"；空串表示接口没给
    pub location: String,
    /// 该评论的总回复数（含楼中楼）
    pub reply_count: u32,
    /// 楼中楼（接口通常只带前 3 条）
    pub sub_replies: Vec<CommentItem>,
}

fn format_num(n: u64) -> String {
    if n >= 100_000_000 {
        format!("{:.1}亿", n as f64 / 100_000_000.0)
    } else if n >= 10_000 {
        format!("{:.1}万", n as f64 / 10_000.0)
    } else {
        n.to_string()
    }
}
