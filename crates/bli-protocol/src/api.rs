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
///
/// 注意这个接口是**游客视角**的：没有登录画像时返回的是通用热榜，
/// 内容高度重复。真正个性化的推荐在 App 端接口（见 RECOMMEND_APP）。
pub const RECOMMEND_WEB: &str = "/x/web-interface/wbi/index/top/feed/rcmd";

/// 首页推荐（App 端，真正的个性化推荐）
/// PiliPlus: Api.recommendListApp —— appBaseUrl + /x/v2/feed/index
///
/// 为什么必须用这个：
///   Web 端 rcmd 接口在未登录 / 无画像时返回的是「热门」类内容，
///   翻十几页都是同一批视频。App 端接口走推荐引擎，`idx` 游标
///   每次推进都会给出全新的内容。
pub const RECOMMEND_APP: &str = "/x/v2/feed/index";

/// App 端请求标识（照抄 PiliPlus Constants.userAgentApp / statisticsApp）
///
/// 服务端按 UA 区分返回结构：带 app UA 才走推荐引擎，
/// 用浏览器 UA 会被当成网页访客，又退回热榜。
pub const APP_UA: &str = "Mozilla/5.0 BiliDroid/8.43.0 (bbcallen@gmail.com) \
os/android model/android mobi_app/android build/8430300 channel/master \
innerVer/8430300 osVer/15 network/2";

pub const APP_STATISTICS: &str =
    "{\"appId\":1,\"platform\":3,\"version\":\"8.43.0\",\"abtest\":\"\"}";

pub const APP_TRACE_ID: &str = "11111111111111111111111111111111:1111111111111111:0:0";

/// buvid 激活（设备指纹上报）
///
/// ★ 这是个性化推荐能不能生效的**关键一环**，很容易被漏掉。
///
///   本地随机生成的 buvid3 只是一个格式正确的字符串，服务端并不认识它。
///   必须调一次这个接口上报设备指纹（payload 是一段 base64 编码的
///   字段映射），服务端才会把这个 buvid 标记为"已激活的真实设备"，
///   之后推荐引擎才愿意基于它算画像。
///
///   不激活的表现：推荐接口能正常返回，但内容是内容泛化的通用流，
///   和用户实际兴趣无关。PiliPlus 在 Accounts.refresh() 里对所有
///   账号（含匿名）都跑一遍 InitHttp.buvidActive()。
pub const ACTIVATE_BUVID: &str = "/x/internal/gaia-gateway/ExClimbWuzhi";

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

/// 通行证服务（登录相关全部在 passport.bilibili.com，不在 api.bilibili.com）
pub const PASSPORT_BASE_URL: &str = "https://passport.bilibili.com";

/// 扫码登录：申请二维码
pub const QRCODE_GENERATE: &str = "/x/passport-login/web/qrcode/generate";

/// 扫码登录：轮询状态（成功时登录态在 Set-Cookie 里）
pub const QRCODE_POLL: &str = "/x/passport-login/web/qrcode/poll";

/// 发表评论 / 回复（POST，必须带 csrf）
pub const REPLY_ADD: &str = "/x/v2/reply/add";

/// 播放地址（DASH）
///
/// fnval 决定返回哪些轨道：
///   16   = DASH（音视频分离）
///   4048 = DASH + 全部清晰度 + 杜比/HDR/8K
/// 我们用 4048，与 PiliPlus 一致。
pub const PLAY_URL: &str = "/x/player/wbi/playurl";

/// fnval 参数值：DASH + 高码率 + 杜比/8K
pub const FNVAL_DASH_ALL: u32 = 4048;

/// UP 主关系统计（粉丝数）
///
/// 不需要 WBI 签名，普通 GET 即可。
/// 响应：{ "data": { "mid":.., "follower":.., "following":.. } }
pub const RELATION_STAT: &str = "/x/relation/stat";

/// UP 主投稿数量
///
/// 响应：{ "data": { "video":.., "bangumi":.., ... } }
/// 不需要签名。
pub const SPACE_NAVNUM: &str = "/x/space/navnum";
