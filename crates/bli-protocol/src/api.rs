// ==============================================================================
// API 端点常量
//
// 全部照抄 PiliPlus `lib/http/api.dart` 与 `lib/http/constants.dart`，
// 不做「优化」——URL 和参数是对方服务端合约，改动即失效。
// ==============================================================================

pub const BASE_URL: &str = "https://www.bilibili.com";
pub const API_BASE_URL: &str = "https://api.bilibili.com";
pub const APP_BASE_URL: &str = "https://app.bilibili.com";

/// 首页推荐（Web 端，需要 Wbi 签名）
/// PiliPlus: Api.recommendListWeb
pub const RECOMMEND_WEB: &str = "/x/web-interface/wbi/index/top/feed/rcmd";

/// 用户信息 / Wbi 密钥
pub const NAV: &str = "/x/web-interface/nav";

/// 热门视频
pub const POPULAR: &str = "/x/web-interface/popular";

/// 搜索建议
pub const SEARCH_SUGGEST: &str = "/x/web-interface/suggest";

/// 搜索（分类）
pub const SEARCH_BY_TYPE: &str = "/x/web-interface/wbi/search/type";

// ------------------------------------------------------------------------------
// 请求头
//
// PiliPlus 用的是 Safari UA + Referer。Referer 对 B 站图片 CDN 与部分接口
// 是必需的（防盗链），UA 则影响返回的数据形态。
// ------------------------------------------------------------------------------

pub const BROWSER_UA_PC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.2 Safari/605.1.15";

pub const REFERER: &str = "https://www.bilibili.com";

/// 视频详情（含 aid/cid/dimension）
pub const VIDEO_VIEW: &str = "/x/web-interface/wbi/view";

/// 相关视频推荐
pub const RELATED: &str = "/x/web-interface/archive/related";

/// 评论区主列表
pub const REPLY_LIST: &str = "/x/v2/reply/wbi/main";

/// 播放地址（DASH）
///
/// fnval 决定返回哪些轨道：
///   16   = DASH（音视频分离）
///   4048 = DASH + 全部清晰度 + 杜比/HDR/8K
/// 我们用 4048，与 PiliPlus 一致。
pub const PLAY_URL: &str = "/x/player/wbi/playurl";

/// fnval 参数值：DASH + 高码率 + 杜比/8K
pub const FNVAL_DASH_ALL: u32 = 4048;
