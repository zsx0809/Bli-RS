// ==============================================================================
// Bli-RS 协议层
//
// 职责边界（重要）：
//   这个 crate 只负责「把 B 站的数据取回来并解析成 Rust 结构体」。
//   它不知道 UI 长什么样，也不依赖 Slint / 任何 GUI 框架。
//   这样它可以在 chroot 里单独跑测试、单独跑 probe 验证接口，
//   不必为了测一个 HTTP 请求去链接 fontconfig。
//
// 移植自 PiliPlus `lib/http/` 与 `lib/utils/wbi_sign.dart`。
// ==============================================================================

pub mod api;
pub mod client;
pub mod detail;
pub mod playurl;
pub mod recommend;
pub mod wbi;

pub use client::BiliClient;
pub use detail::{CommentItem, RelatedVideo, VideoDetail};

/// 统一的错误类型。
///
/// 分成几类是为了让 UI 层能决定怎么呈现：
/// 网络错误可以「重试」，协议错误（code != 0）通常要提示具体原因。
#[derive(Debug)]
pub enum Error {
    /// 传输层失败：DNS、连接、TLS、超时
    Network(String),
    /// 响应体不是预期格式
    Decode(String),
    /// B 站业务错误码（响应 code != 0）
    Api { code: i64, message: String },
    /// 其他
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Network(e) => write!(f, "网络错误: {e}"),
            Error::Decode(e) => write!(f, "解析错误: {e}"),
            Error::Api { code, message } => write!(f, "接口错误 [{code}]: {message}"),
            Error::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Network(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Decode(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
