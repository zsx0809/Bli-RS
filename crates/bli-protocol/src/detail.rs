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
    pub async fn video_comments(&self, aid: u64) -> Result<Vec<CommentItem>> {
        let mut params = BTreeMap::new();
        params.insert("oid".into(), aid.to_string());
        params.insert("type".into(), "1".into());
        params.insert("mode".into(), "3".into());

        // 走 WBI 签名请求，避免 -352 或 -403 风控拦截
        let json = self.get_signed(api::REPLY_LIST, params).await?;

        let Some(replies) = json["data"]["replies"].as_array() else {
            return Ok(Vec::new());
        };

        let mut res = Vec::with_capacity(replies.len());
        for r in replies {
            let rpid = r["rpid"].as_u64().unwrap_or(0);
            let user_name = r["member"]["uname"].as_str().unwrap_or("").to_string();
            let user_face = r["member"]["avatar"].as_str().unwrap_or("").to_string();
            let content = r["content"]["message"].as_str().unwrap_or("").to_string();
            let likes = format_num(r["like"].as_u64().unwrap_or(0));
            let ctime = r["ctime"].as_i64().unwrap_or(0);
            let pubtime = format_relative_time(ctime);

            res.push(CommentItem {
                rpid,
                user_name,
                user_face,
                content,
                likes,
                pubtime,
            });
        }

        Ok(res)
    }
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentItem {
    pub rpid: u64,
    pub user_name: String,
    pub user_face: String,
    pub content: String,
    pub likes: String,
    pub pubtime: String,
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
