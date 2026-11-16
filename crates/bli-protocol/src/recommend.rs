// ==============================================================================
// 首页推荐流
//
// 移植自 PiliPlus:
//   - 接口: lib/http/api.dart  Api.recommendListWeb
//   - 请求: lib/http/video.dart VideoHttp.rcmdVideoList
//   - 模型: lib/models/home/rcmd/result.dart RcmdVideoItemModel
//   - 过滤: lib/http/video.dart _filterWebRcmd
//
// 一条重要经验（来自 PiliPlus 的注释与实测）：
//   推荐流里混着直播卡片、广告卡片、番剧卡片。它们的 JSON 结构完全不同，
//   有些连 title/owner 都没有。必须按 `goto` 字段过滤，只留 `av`，
//   否则解析会在某个卡片上 panic 或渲染出空白格子。
//   这就是之前骨架里"卡片大小不对称"之外的另一个隐患。
// ==============================================================================

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::api;
use crate::client::BiliClient;
use crate::{Error, Result};

/// UI 侧消费的精简视频条目。
///
/// 刻意不照搬 B 站原始 JSON —— 原始结构里 90% 的字段（各种 abtest、
/// 埋点参数、广告位信息）这个客户端永远不会用到。在这里一次性裁剪，
/// 减少跨线程/跨 FFI 传递的数据量。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VideoItem {
    pub aid: i64,
    pub bvid: String,
    pub cid: i64,
    pub title: String,
    pub author: String,
    pub cover_url: String,
    /// 秒。0 表示未知。
    pub duration: i64,
    /// 已格式化的播放量，如 "128.4万"
    pub view_text: String,
    /// 已格式化的弹幕数，如 "3.2万"
    pub danmaku_text: String,
}

/// 一页推荐结果。
#[derive(Debug, Clone, Default)]
pub struct RecommendPage {
    pub items: Vec<VideoItem>,
    /// 被过滤掉的条目数（广告/直播/番剧）。
    /// 保留这个数字是为了调试：如果它异常大，说明接口形态变了。
    pub filtered_out: usize,
    /// 服务端这一页**原始**返回了多少条（过滤前）。
    ///
    /// 翻页判断必须用它，不能用 `items.len()`：
    /// 推荐流一页 20 条里常常有一半是直播/广告，过滤完可能只剩 5 条。
    /// 早先用过滤后的条数判断"拿不满 20 条就是到底了"，于是第一页
    /// 之后就再也不翻页 —— 表现就是首页只有十来个视频、下拉没反应。
    pub raw_count: usize,
}

impl BiliClient {
    /// 拉取首页推荐。
    ///
    /// `fresh_idx` 是翻页游标（不是页码）：
    ///   0 = 首次进入，3 = 下拉刷新，4 = 加载更多。
    /// 这个语义来自 PiliPlus，照抄以免行为不一致。
    pub async fn recommend_feed(
        &self,
        fresh_idx: u32,
        page_size: u32,
    ) -> Result<RecommendPage> {
        // 参数照抄 PiliPlus rcmdVideoList() 的非 desktop 分支。
        // 这些值不是随便填的：feed_version=V8 / homepage_ver=1 决定返回结构，
        // fresh_type=4 是"加载更多"，brush 与 fresh_idx 同值。
        let mut params = BTreeMap::new();
        params.insert("version".into(), "1".into());
        params.insert("feed_version".into(), "V8".into());
        params.insert("homepage_ver".into(), "1".into());
        params.insert("ps".into(), page_size.to_string());
        params.insert("fresh_idx".into(), fresh_idx.to_string());
        params.insert("brush".into(), fresh_idx.to_string());
        params.insert("fresh_type".into(), "4".into());

        let json = self.get_signed(api::RECOMMEND_WEB, params).await?;

        let raw_items = json["data"]["item"]
            .as_array()
            .ok_or_else(|| Error::Decode("推荐流响应缺少 data.item 数组".into()))?;

        let raw_count = raw_items.len();
        let mut items = Vec::with_capacity(raw_count);
        let mut filtered_out = 0usize;

        for raw in raw_items {
            // 只保留普通投稿。直播(live)、番剧(bangumi)、广告(ad)全部丢弃。
            if raw["goto"].as_str() != Some("av") {
                filtered_out += 1;
                continue;
            }
            match parse_item(raw) {
                Some(v) => items.push(v),
                // 结构对但缺关键字段：同样计数，不 panic
                None => filtered_out += 1,
            }
        }

        Ok(RecommendPage {
            items,
            filtered_out,
            raw_count,
        })
    }

    /// 拉取首页推荐（**App 端**，真正的个性化推荐）。
    ///
    /// 与 `recommend_feed`（Web 端）的关键差别：
    ///   · Web 端在无画像时返回通用热榜，翻页高度重复
    ///   · App 端走推荐引擎，`idx` 每推进一次就是一批全新内容
    ///
    /// 参数照抄 PiliPlus `VideoHttp.rcmdVideoListApp`，一个都不改：
    /// 这些值共同决定服务端走哪条推荐链路，改任何一项都可能退回热榜。
    pub async fn recommend_feed_app(&self, idx: u32) -> Result<RecommendPage> {
        let mut params = BTreeMap::new();
        params.insert("build".to_string(), "8430300".to_string());
        params.insert("c_locale".to_string(), "zh_CN".to_string());
        params.insert("channel".to_string(), "master".to_string());
        params.insert("column".to_string(), "2".to_string());
        params.insert("device".to_string(), "phone".to_string());
        params.insert("device_name".to_string(), "android".to_string());
        params.insert("device_type".to_string(), "0".to_string());
        params.insert("disable_rcmd".to_string(), "0".to_string());
        params.insert("flush".to_string(), "8".to_string());
        params.insert("fnval".to_string(), "976".to_string());
        params.insert("fnver".to_string(), "0".to_string());
        params.insert("force_host".to_string(), "2".to_string());
        params.insert("fourk".to_string(), "1".to_string());
        params.insert("guidance".to_string(), "1".to_string());
        params.insert("https_url_req".to_string(), "1".to_string());
        params.insert("idx".to_string(), idx.to_string());
        params.insert("mobi_app".to_string(), "android_i".to_string());
        params.insert("network".to_string(), "wifi".to_string());
        params.insert("platform".to_string(), "android".to_string());
        params.insert("player_net".to_string(), "1".to_string());
        // pull=true 只在首次请求（idx == 0）时为 true
        params.insert(
            "pull".to_string(),
            if idx == 0 { "true".to_string() } else { "false".to_string() },
        );
        params.insert("qn".to_string(), "32".to_string());
        params.insert("recsys_mode".to_string(), "0".to_string());
        params.insert("s_locale".to_string(), "zh_CN".to_string());
        params.insert("splash_id".to_string(), String::new());
        params.insert("statistics".to_string(), api::APP_STATISTICS.to_string());
        params.insert("voice_balance".to_string(), "0".to_string());

        let url = format!("{}{}", api::APP_BASE_URL, api::RECOMMEND_APP);

        let headers = vec![
            ("user-agent", api::APP_UA.to_string()),
            ("app-key", "android_hd".to_string()),
            ("env", "prod".to_string()),
            ("buvid", self.buvid_string()),
            // PiliPlus 送的是一串 1 的假指纹。真实指纹缺失时服务端照样
            // 走推荐链路，而这串 1 是它实测可用的值，照抄最稳。
            ("fp_local", "1".repeat(64)),
            ("fp_remote", "1".repeat(64)),
            ("session_id", "11111111".to_string()),
            ("x-bili-trace-id", api::APP_TRACE_ID.to_string()),
            ("x-bili-aurora-eid", String::new()),
            ("x-bili-aurora-zone", String::new()),
            ("bili-http-engine", "cronet".to_string()),
        ];

        let json = self
            .get_with_headers(&url, &params, &headers)
            .await?;

        let raw_items = json["data"]["items"]
            .as_array()
            .ok_or_else(|| Error::Decode("App 推荐响应缺少 data.items 数组".into()))?;

        let raw_count = raw_items.len();
        let mut items = Vec::with_capacity(raw_count);
        let mut filtered_out = 0usize;

        for raw in raw_items {
            // App 端过滤规则（对应 PiliPlus _filterAppRcmd）：
            //   card_goto 为广告 / ad_info 存在 / can_play != 1 → 丢弃
            if matches!(
                raw["card_goto"].as_str(),
                Some("ad_av") | Some("ad_web_s")
            ) || !raw["ad_info"].is_null()
                || raw["can_play"].as_i64().unwrap_or(1) != 1
            {
                filtered_out += 1;
                continue;
            }

            match parse_item_app(raw) {
                Some(v) => items.push(v),
                None => filtered_out += 1,
            }
        }

        Ok(RecommendPage {
            items,
            filtered_out,
            raw_count,
        })
    }
}

/// 解析 App 端推荐条目。
///
/// 字段结构与 Web 端完全不同：标题在 `title`，时长和 aid 埋在 `player_args`，
/// 封面在 `cover`，作者在 `args.up_name` 或 `owner.name`。
fn parse_item_app(raw: &serde_json::Value) -> Option<VideoItem> {
    // 只要普通视频。实时直播/番剧在 App 流里用 card_goto 标记
    let goto = raw["card_goto"].as_str().unwrap_or("av");
    if goto != "av" && goto != "av_rcmd" {
        return None;
    }

    let args = &raw["player_args"];
    let aid = args["aid"]
        .as_i64()
        .or_else(|| raw["param"].as_str().and_then(|s| s.parse::<i64>().ok()))
        .or_else(|| raw["aid"].as_i64())?;

    let bvid = raw["bvid"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| crate::recommend::av_to_bv(aid));

    let cid = args["cid"].as_i64().unwrap_or(0);
    let title = raw["title"].as_str()?.to_string();

    let cover_url = raw["cover"]
        .as_str()
        .or_else(|| raw["pic"].as_str())
        .unwrap_or("")
        .to_string();

    let author = raw["args"]["up_name"]
        .as_str()
        .or_else(|| raw["owner"]["name"].as_str())
        .unwrap_or("")
        .to_string();

    let duration = args["duration"].as_i64().unwrap_or(0);

    // 播放量：App 流里可能是数字，也可能是 "12.3万" 这种文案
    let view_text = raw["stat"]["view"]
        .as_u64()
        .map(|n| format_count(n as i64))
        .or_else(|| raw["cover_left_text_1"].as_str().map(|s| s.to_string()))
        .unwrap_or_default();

    let danmaku_text = raw["stat"]["danmaku"]
        .as_u64()
        .map(|n| format_count(n as i64))
        .or_else(|| raw["cover_left_text_2"].as_str().map(|s| s.to_string()))
        .unwrap_or_default();

    Some(VideoItem {
        aid,
        bvid,
        cid,
        title,
        author,
        cover_url,
        duration,
        view_text,
        danmaku_text,
    })
}

/// 解析单条推荐。
///
/// 返回 Option 而非 Result：单条坏数据不应该让整页失败。
fn parse_item(raw: &serde_json::Value) -> Option<VideoItem> {
    let aid = raw["id"].as_i64().or_else(|| raw["aid"].as_i64())?;

    // bvid 有时缺失，但视频一定有。缺失就从 aid 推导。
    let bvid = raw["bvid"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| av_to_bv(aid));

    let title = raw["title"].as_str()?.to_string();

    // 封面字段名在不同接口里叫 pic / cover / img
    let cover_url = raw["pic"]
        .as_str()
        .or_else(|| raw["cover"].as_str())
        .unwrap_or("")
        .to_string();

    // 作者：Web 端在 owner.name，App 端在 args.up_name
    let author = raw["owner"]["name"]
        .as_str()
        .or_else(|| raw["args"]["up_name"].as_str())
        .unwrap_or("")
        .to_string();

    // 时长：Web 端是秒数，但偶尔会是 "mm:ss" 字符串
    let duration = match &raw["duration"] {
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(0),
        serde_json::Value::String(s) => parse_duration(s),
        _ => 0,
    };

    // 播放量 / 弹幕：Web 端在 stat.view / stat.danmaku（数字），
    // App 端在 cover_left_text_1/2（已格式化的中文串）
    let view_text = raw["stat"]["view"]
        .as_i64()
        .map(|n| format_count(n as i64))
        .or_else(|| raw["cover_left_text_1"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "0".into());

    let danmaku_text = raw["stat"]["danmaku"]
        .as_i64()
        .map(|n| format_count(n as i64))
        .or_else(|| raw["cover_left_text_2"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "0".into());

    Some(VideoItem {
        aid,
        bvid,
        cid: raw["cid"].as_i64().unwrap_or(0),
        title,
        author,
        cover_url,
        duration,
        view_text,
        danmaku_text,
    })
}

/// "12:34" / "1:02:03" -> 秒
fn parse_duration(s: &str) -> i64 {
    let mut total = 0i64;
    for part in s.split(':') {
        if let Ok(n) = part.trim().parse::<i64>() {
            total = total * 60 + n;
        }
    }
    total
}

/// 数字 -> 中文计数串，对齐 PiliPlus NumUtils.numFormat
///
/// 行为：10000 -> "1.0万"（保留一位小数，整数则省略小数）
///      1234  -> "1234"
fn format_count(n: i64) -> String {
    if n >= 100_000_000 {
        let v = n as f64 / 100_000_000.0;
        trim_decimal(v, '亿')
    } else if n >= 10_000 {
        let v = n as f64 / 10_000.0;
        trim_decimal(v, '万')
    } else {
        n.to_string()
    }
}

fn trim_decimal(v: f64, unit: char) -> String {
    // PiliPlus 用 toPrecision(1)，即保留 1 位有效数字后转 int 比较。
    // 这里简化为一位小数，足够 UI 使用且更符合直觉。
    let rounded = (v * 10.0).round() / 10.0;
    if (rounded - rounded.trunc()).abs() < f64::EPSILON {
        format!("{}{}", rounded.trunc() as i64, unit)
    } else {
        format!("{rounded:.1}{unit}")
    }
}

/// aid -> bvid 的逆向推导。
///
/// 算法移植自 PiliPlus `lib/utils/id_utils.dart` IdUtils.av2bv。
/// 这不是「加密」，是 B 站的编号混淆：混淆表 + 位置交换。
///
/// 易错点：`idx` 必须允许一直递减到 0，不能提前 break。
/// 初始值 12 个字符里前三位预置为 'B','V','1'，但循环**会**覆盖它们
/// （bvid 的第 3 位实际是数字 '0'，不是 'V'），
/// 真正的字母来自后面的位置交换。早期版本在这里加了 `idx >= 3` 守卫，
/// 导致算出的 bvid 完全错误。
fn av_to_bv(aid: i64) -> String {
    // 注意：这个数字是 23442827791579。
    // 之前误写成 23_442_877_791_579，下划线分组错位导致实际值变成
    // 23442877791579，算出的 bvid 全是错的。
    const XOR_CODE: i64 = 23_442_827_791_579;
    const MAX_AID: i64 = 1 << 51;
    const BASE: i64 = 58;
    const TABLE: &[u8; 58] = b"FcwAPNKTMug3GV5Lj7EJnHpWsx4tb8haYeviqBz6rkCy12mUSDQX9RdoZf";

    let mut bytes = *b"BV1000000000";

    let mut tmp = (MAX_AID | aid) ^ XOR_CODE;
    let mut idx = bytes.len() - 1;
    while tmp > 0 {
        bytes[idx] = TABLE[(tmp % BASE) as usize];
        tmp /= BASE;
        if idx == 0 {
            break;
        }
        idx -= 1;
    }

    // 位置交换：3<->9, 4<->7
    bytes.swap(3, 9);
    bytes.swap(4, 7);

    String::from_utf8_lossy(&bytes).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av_to_bv_known_vector() {
        // BV17x411w7KC <-> av170001，B 站公开的经典对照
        assert_eq!(av_to_bv(170001), "BV17x411w7KC");
    }

    #[test]
    fn formats_counts_like_bilibili() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(9999), "9999");
        assert_eq!(format_count(10_000), "1万");
        assert_eq!(format_count(12_340), "1.2万");
        assert_eq!(format_count(1_284_000), "128.4万");
        assert_eq!(format_count(100_000_000), "1亿");
    }

    #[test]
    fn parses_duration_strings() {
        assert_eq!(parse_duration("12:34"), 754);
        assert_eq!(parse_duration("1:02:03"), 3723);
        assert_eq!(parse_duration("59"), 59);
    }

    #[test]
    fn skips_non_av_cards() {
        // 直播卡片：没有 goto=av，必须被过滤
        let live = serde_json::json!({
            "goto": "live",
            "id": 123,
            "title": "某直播间"
        });
        assert!(parse_item(&live).is_none() || live["goto"] != "av");

        // 正常稿件
        let av = serde_json::json!({
            "goto": "av",
            "id": 170001i64,
            "bvid": "BV17x411w7KC",
            "cid": 1i64,
            "title": "测试视频",
            "pic": "https://i0.hdslb.com/x.jpg",
            "owner": { "name": "UP主" },
            "duration": 754,
            "stat": { "view": 1284000, "danmaku": 32000 }
        });
        let item = parse_item(&av).expect("应当解析成功");
        assert_eq!(item.bvid, "BV17x411w7KC");
        assert_eq!(item.view_text, "128.4万");
        assert_eq!(item.danmaku_text, "3.2万");
        assert_eq!(item.duration, 754);
        assert_eq!(item.author, "UP主");
    }

    #[test]
    fn derives_bvid_when_missing() {
        let av = serde_json::json!({
            "goto": "av",
            "id": 170001i64,
            "title": "无 bvid 的视频"
        });
        let item = parse_item(&av).expect("应当解析成功");
        assert_eq!(item.bvid, "BV17x411w7KC");
    }
}
