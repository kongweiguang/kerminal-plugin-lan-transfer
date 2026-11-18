//! 配对密钥与短期会话令牌。
//! @author kongweiguang
//!
//! 设计原因：
//! - 配对密钥只出现在启动时生成的一次性链接 fragment 中，服务端只保存摘要；
//!   这样即使浏览器把链接同步到别处，也无法从服务端内存反推出可用密钥。
//! - 会话令牌同样是随机值，服务端只存摘要；令牌不写日志、不进视图数据、不落盘。
//! - 会话在停止、过期或服务换代时统一撤销，避免旧令牌在新服务上继续可用。

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::{
    config::SESSION_TTL_SECS,
    error::{AppError, AppResult, ErrorCode},
    secret::{constant_time_eq, now_unix, random_token, token_digest},
};

/// 一次服务运行内的认证状态。`generation` 随每次启动递增，
/// 用于让旧视图和旧票据在重启后自动失效。
#[derive(Clone)]
pub struct Sessions {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    generation: u64,
    pairing_digest: String,
    expires_at: u64,
    /// 摘要 → 会话到期时间；只存摘要，不存令牌原文。
    tokens: HashMap<String, u64>,
}

impl Sessions {
    /// 为新一次服务运行生成配对密钥与到期时间；返回给视图的是明文链接，
    /// 这里只保留摘要。
    pub fn start(generation: u64) -> AppResult<(Self, String)> {
        let secret = random_token(32)?;
        let expires_at = now_unix() + SESSION_TTL_SECS;
        Ok((
            Self {
                inner: Arc::new(Mutex::new(Inner {
                    generation,
                    pairing_digest: token_digest(&secret),
                    expires_at,
                    tokens: HashMap::new(),
                })),
            },
            secret,
        ))
    }

    /// 当前运行代次；票据与会话都用它判断是否属于本次服务。
    pub fn generation(&self) -> u64 {
        self.inner.lock().map(|inner| inner.generation).unwrap_or(0)
    }

    /// 会话到期时间；启动时直接读取，避免为了拿到期时间先签发一个无人使用的令牌。
    pub fn expires_at(&self) -> u64 {
        self.inner
            .lock()
            .map(|inner| inner.expires_at)
            .unwrap_or_else(|_| now_unix())
    }

    /// 用配对密钥换取会话令牌；错误只区分「密钥不对」与「已过期」，
    /// 不透露密钥长度或前缀。
    pub fn pair(&self, secret: &str) -> AppResult<(String, u64)> {
        let mut inner = self.lock()?;
        let now = now_unix();
        if now >= inner.expires_at {
            return Err(AppError::new(ErrorCode::SessionExpired, "pairing expired"));
        }
        let digest = token_digest(secret);
        if !constant_time_eq(digest.as_bytes(), inner.pairing_digest.as_bytes()) {
            return Err(AppError::new(ErrorCode::InvalidSecret, "invalid secret"));
        }
        let token = random_token(32)?;
        // 每次配对签发独立令牌：同一链接被两台设备打开时互不影响，也能单独撤销。
        let expires_at = inner.expires_at;
        inner.tokens.insert(token_digest(&token), expires_at);
        Ok((token, expires_at))
    }

    /// 校验 Bearer 令牌；返回剩余到期时间供 `GET /api/session` 使用。
    pub fn verify(&self, token: &str) -> AppResult<u64> {
        let mut inner = self.lock()?;
        let now = now_unix();
        let digest = token_digest(token);
        let Some(expires_at) = inner.tokens.get(&digest).copied() else {
            return Err(AppError::new(ErrorCode::Unauthorized, "unknown token"));
        };
        if now >= expires_at {
            inner.tokens.remove(&digest);
            return Err(AppError::new(ErrorCode::SessionExpired, "session expired"));
        }
        Ok(expires_at)
    }

    /// 服务停止时清空全部令牌与配对密钥；返回后任何旧令牌都不可再用。
    pub fn revoke_all(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.tokens.clear();
            inner.expires_at = 0;
            inner.pairing_digest.clear();
        }
    }

    /// 互斥锁中毒意味着认证状态不可信，宁可让请求失败也不能继续放行。
    fn lock(&self) -> AppResult<std::sync::MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| AppError::new(ErrorCode::Server, "session state unavailable"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 正确密钥换取令牌后可校验，错误密钥必须被拒绝。
    #[test]
    fn pairs_with_matching_secret_only() {
        let (sessions, secret) = Sessions::start(1).unwrap();
        assert!(sessions.pair("wrong-secret").is_err());
        let (token, expires_at) = sessions.pair(&secret).unwrap();
        assert_eq!(sessions.verify(&token).unwrap(), expires_at);
    }

    /// 撤销后旧令牌立即失效，避免停止服务后仍可访问。
    #[test]
    fn revoke_invalidates_tokens() {
        let (sessions, secret) = Sessions::start(1).unwrap();
        let (token, _) = sessions.pair(&secret).unwrap();
        sessions.revoke_all();
        assert!(sessions.verify(&token).is_err());
        assert!(sessions.pair(&secret).is_err());
    }
}
