//! 配对与会话接口，以及 Bearer 鉴权提取。
//! @author kongweiguang
//!
//! 配对是唯一不需要令牌的写接口，因此额外要求精确同源并做全局尝试限流：
//! 密钥本身是高熵随机值，限流的作用是防止被反复撞击时白白消耗 CPU 与日志空间。

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::{
    error::{AppError, ErrorCode},
    http::{AppContext, error_response, json_response},
    secret::now_unix,
};

/// 配对请求体；`deny_unknown_fields` 让多余字段直接失败，避免页面与服务端字段漂移。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairRequest {
    secret: String,
}

/// 配对响应；`expiresAt` 是 Unix 秒，页面据此显示剩余时间。
#[derive(Serialize)]
pub struct PairResponse {
    token: String,
    #[serde(rename = "expiresAt")]
    expires_at: u64,
}

/// 会话状态响应。
#[derive(Serialize)]
pub struct SessionResponse {
    #[serde(rename = "expiresAt")]
    expires_at: u64,
}

/// `POST /api/pair`：用 fragment 中的密钥换取短期会话令牌。
///
/// 同源校验由统一中间件完成，处理器不再重复读取 `Origin`/`Host`；少一个未使用的
/// 提取参数，也避免以后误以为处理器自己承担了来源校验。
pub async fn pair(State(context): State<Arc<AppContext>>, body: String) -> Response {
    if !context.take_pair_attempt() {
        return error_response(AppError::new(ErrorCode::Busy, "too many pairing attempts"));
    }
    let request: PairRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(_) => {
            return error_response(AppError::new(ErrorCode::BadRequest, "invalid body"));
        }
    };
    match context.sessions.pair(&request.secret) {
        Ok((token, expires_at)) => json_response(PairResponse { token, expires_at }),
        Err(error) => error_response(error),
    }
}

/// `GET /api/session`：确认会话仍有效并返回到期时间；页面用它判断是否要重新配对。
///
/// 这里也要求服务处于运行中：停止期间监听可能还没关闭，已建立的连接仍会打进来，
/// 此时返回有效会话会让页面以为服务可用。
pub async fn session(State(context): State<Arc<AppContext>>, headers: HeaderMap) -> Response {
    if !context.state.is_running() {
        return error_response(AppError::new(ErrorCode::Unavailable, "service stopped"));
    }
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return error_response(error),
    };
    match context.sessions.verify(token) {
        Ok(expires_at) => json_response(SessionResponse { expires_at }),
        Err(error) => error_response(error),
    }
}

/// 从 `Authorization: Bearer` 提取令牌；只接受恰好一个 Bearer 方案，
/// 不允许空令牌或额外参数，避免把解析歧义变成鉴权绕过。
pub fn bearer(headers: &HeaderMap) -> Result<&str, AppError> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::new(ErrorCode::Unauthorized, "missing token"))?;
    let token = value
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty() && !token.contains(' '))
        .ok_or_else(|| AppError::new(ErrorCode::Unauthorized, "invalid authorization"))?;
    Ok(token)
}

/// 供处理器复用的鉴权：同时校验令牌有效性与服务仍在运行。
pub fn authenticate(context: &AppContext, headers: &HeaderMap) -> Result<u64, AppError> {
    if !context.state.is_running() {
        return Err(AppError::new(ErrorCode::Unavailable, "service stopped"));
    }
    let token = bearer(headers)?;
    context.sessions.verify(token)
}

/// 空响应；配对失败等分支只需要状态码。
pub fn empty(status: StatusCode) -> Response {
    status.into_response()
}

/// 配对尝试限流窗口；固定窗口足够简单且不可被单请求放大。
pub(crate) fn attempt_allowed(slot: &mut (u64, u32)) -> bool {
    const WINDOW_SECS: u64 = 60;
    const MAX_ATTEMPTS: u32 = 30;
    let now = now_unix();
    if now.saturating_sub(slot.0) >= WINDOW_SECS {
        *slot = (now, 1);
        return true;
    }
    if slot.1 >= MAX_ATTEMPTS {
        return false;
    }
    slot.1 += 1;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    /// 只接受恰好一个 `Bearer <token>`；大小写、空令牌、额外参数与多个方案
    /// 都必须拒绝，避免把解析歧义变成鉴权绕过。
    #[test]
    fn bearer_accepts_exactly_one_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc"),
        );
        assert_eq!(bearer(&headers).unwrap(), "abc");

        for value in [
            "bearer abc",
            "Bearer ",
            "Bearer",
            "Bearer a b",
            "Basic abc",
            "Bearer abc Bearer def",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            assert!(bearer(&headers).is_err(), "{value}");
        }
        assert!(bearer(&HeaderMap::new()).is_err());
    }

    /// 固定窗口：窗口内前 N 次放行，第 N+1 次拒绝；窗口滑过后重新放行。
    /// 这里用同一秒内的大量调用覆盖上限，不依赖真实等待；窗口重置只断言
    /// 计数被重置，不断言具体时间戳（跨越秒边界时会多 1 秒）。
    #[test]
    fn attempt_window_limits_then_resets() {
        let mut slot = (now_unix(), 0);
        for _ in 0..30 {
            assert!(attempt_allowed(&mut slot));
        }
        assert!(!attempt_allowed(&mut slot));
        // 窗口起点早于 60 秒即视为新窗口，计数重新从 1 开始。
        let mut stale = (now_unix().saturating_sub(60), 30);
        assert!(attempt_allowed(&mut stale));
        assert_eq!(stale.1, 1);
    }
}
