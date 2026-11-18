//! 类型化错误与稳定的 HTTP 错误码。
//! @author kongweiguang
//!
//! 页面只按错误码查本地化文案，绝不显示服务端 message。因此这里把「给页面的码」
//! 和「给日志/右栏的诊断文本」分开：诊断文本可以包含操作系统原因，但不能含本机绝对路径
//! 或令牌，HTTP 响应只序列化码与固定短句。

use std::fmt;

use serde::Serialize;

/// 稳定的错误码集合；新增码需要同步 `web/app.js` 的 `error.*` 文案表。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    Unauthorized,
    SessionExpired,
    InvalidSecret,
    Forbidden,
    NotFound,
    DirNotFound,
    TicketExpired,
    TooLarge,
    Busy,
    Conflict,
    Quota,
    BadRequest,
    PathRejected,
    Server,
    Unavailable,
}

impl ErrorCode {
    /// 使用小写下划线串，页面侧对码做白名单清洗后拼 `error.<code>`。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::SessionExpired => "session_expired",
            Self::InvalidSecret => "invalid_secret",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::DirNotFound => "dir_not_found",
            Self::TicketExpired => "ticket_expired",
            Self::TooLarge => "too_large",
            Self::Busy => "busy",
            Self::Conflict => "conflict",
            Self::Quota => "quota",
            Self::BadRequest => "bad_request",
            Self::PathRejected => "path_rejected",
            Self::Server => "server",
            Self::Unavailable => "unavailable",
        }
    }

    /// HTTP 状态与业务码分离：页面用状态判断会话失效，用码选择文案。
    pub const fn status(self) -> u16 {
        match self {
            Self::Unauthorized | Self::SessionExpired | Self::InvalidSecret => 401,
            Self::Forbidden | Self::PathRejected => 403,
            Self::NotFound | Self::DirNotFound => 404,
            Self::TicketExpired => 410,
            Self::TooLarge => 413,
            Self::Busy => 429,
            Self::Conflict => 409,
            Self::Quota => 507,
            Self::BadRequest => 400,
            Self::Unavailable => 503,
            Self::Server => 500,
        }
    }
}

/// 内部错误：对外只暴露码，诊断文本保留给右栏与日志。
#[derive(Clone, Debug)]
pub struct AppError {
    pub code: ErrorCode,
    /// 诊断说明；不得包含本机绝对路径、令牌或文件名以外的敏感信息。
    pub detail: String,
}

impl AppError {
    /// 统一构造，避免调用点各自拼装码与说明。
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// 路径越界、非法名字等一律走固定码，避免把用户输入回显到诊断里。
    pub fn rejected() -> Self {
        Self::new(ErrorCode::PathRejected, "path rejected")
    }

    /// IO 失败保留错误种类，便于区分磁盘满、占用与权限问题。
    pub fn io(error: std::io::Error) -> Self {
        let code = match error.kind() {
            std::io::ErrorKind::NotFound => ErrorCode::NotFound,
            std::io::ErrorKind::PermissionDenied => ErrorCode::Forbidden,
            std::io::ErrorKind::AlreadyExists => ErrorCode::Conflict,
            std::io::ErrorKind::StorageFull => ErrorCode::Quota,
            // 配额上限与「磁盘满」在页面上都需要用户腾出空间；分开映射会让
            // 用户看到两个无法区分的文案，因此统一归入 quota。
            std::io::ErrorKind::QuotaExceeded => ErrorCode::Quota,
            // 超过单文件上限属于请求本身过大，和「服务内部错误」区分开才能让页面
            // 提示换小文件而不是让用户无意义重试。
            std::io::ErrorKind::FileTooLarge => ErrorCode::TooLarge,
            _ => ErrorCode::Server,
        };
        Self::new(code, format!("io: {:?}", error.kind()))
    }
}

impl fmt::Display for AppError {
    /// 诊断格式只给内部使用，避免默认 Display 被误写进 HTTP 响应。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for AppError {}

pub type AppResult<T> = Result<T, AppError>;

/// 对外错误体；字段名与浏览器契约一致，不含诊断文本。
#[derive(Serialize)]
pub struct ErrorBody {
    pub error: ErrorPayload,
}

#[derive(Serialize)]
pub struct ErrorPayload {
    pub code: &'static str,
    pub message: &'static str,
}

impl AppError {
    /// 固定短句与码一一对应，避免把内部诊断泄漏给浏览器。
    pub fn body(&self) -> ErrorBody {
        ErrorBody {
            error: ErrorPayload {
                code: self.code.as_str(),
                message: public_message(self.code),
            },
        }
    }
}

/// 只允许固定的英文短句；页面不会显示它，但抓包/调试时不能出现路径。
pub const fn public_message(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Unauthorized => "unauthorized",
        ErrorCode::SessionExpired => "session expired",
        ErrorCode::InvalidSecret => "invalid secret",
        ErrorCode::Forbidden => "forbidden",
        ErrorCode::NotFound => "not found",
        ErrorCode::DirNotFound => "directory not found",
        ErrorCode::TicketExpired => "ticket expired",
        ErrorCode::TooLarge => "too large",
        ErrorCode::Busy => "busy",
        ErrorCode::Conflict => "conflict",
        ErrorCode::Quota => "storage full",
        ErrorCode::BadRequest => "bad request",
        ErrorCode::PathRejected => "path rejected",
        ErrorCode::Server => "server error",
        ErrorCode::Unavailable => "unavailable",
    }
}
