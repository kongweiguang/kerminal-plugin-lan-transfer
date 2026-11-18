//! 下载票据：短期、绑定单个文件与会话代次。
//! @author kongweiguang
//!
//! 票据不落盘、不进日志，只保存在内存里。设计上允许同一票据被 HEAD 与 GET 各用一次、
//! 也允许浏览器重试，因此不做「用一次即失效」：真正的边界是短期到期、单文件绑定和
//! 会话代次。这样既避免预检/重试误伤，也不会把票据变成可长期使用的旁路。

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::{
    config::TICKET_TTL_SECS,
    error::{AppError, AppResult, ErrorCode},
    path::VirtualPath,
    secret::{now_unix, random_token},
};

/// 票据表；键是随机票据串本身，因为票据只在短时间内存活且需要按 URL 路径精确查找。
#[derive(Clone)]
pub struct Tickets {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    generation: u64,
    entries: HashMap<String, Ticket>,
}

#[derive(Clone)]
struct Ticket {
    path: VirtualPath,
    expires_at: u64,
}

impl Tickets {
    /// 每次服务启动重建票据表，旧代次的票据自然不存在。
    pub fn new(generation: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                generation,
                entries: HashMap::new(),
            })),
        }
    }

    /// 为单个文件签发票据；路径已在调用方校验过属于共享根。
    pub fn issue(&self, path: &VirtualPath) -> AppResult<(String, u64)> {
        let token = random_token(24)?;
        let expires_at = now_unix() + TICKET_TTL_SECS;
        let mut inner = self.lock()?;
        // 顺手清理过期票据，避免长时间运行时表无限增长。
        let now = now_unix();
        inner.entries.retain(|_, ticket| ticket.expires_at > now);
        inner.entries.insert(
            token.clone(),
            Ticket {
                path: path.clone(),
                expires_at,
            },
        );
        Ok((token, expires_at))
    }

    /// 按票据取回绑定路径；代次不匹配或过期都返回 `ticket_expired`，
    /// 不区分「从未存在」和「已失效」，避免成为票据探测预言机。
    pub fn resolve(&self, token: &str, generation: u64) -> AppResult<VirtualPath> {
        let inner = self.lock()?;
        let ticket = inner
            .entries
            .get(token)
            .filter(|_| inner.generation == generation)
            .ok_or_else(|| AppError::new(ErrorCode::TicketExpired, "ticket expired"))?;
        if now_unix() >= ticket.expires_at {
            return Err(AppError::new(ErrorCode::TicketExpired, "ticket expired"));
        }
        Ok(ticket.path.clone())
    }

    /// 服务停止时清空，避免停止后仍能凭旧票据读取文件。
    pub fn clear(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.entries.clear();
        }
    }

    fn lock(&self) -> AppResult<std::sync::MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| AppError::new(ErrorCode::Server, "ticket state unavailable"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 票据只能解析出签发时的文件，且换一个代次后立即失效。
    #[test]
    fn ticket_binds_path_and_generation() {
        let tickets = Tickets::new(7);
        let path = VirtualPath::parse("root/a.txt").unwrap();
        let (token, _) = tickets.issue(&path).unwrap();
        assert_eq!(tickets.resolve(&token, 7).unwrap(), path);
        assert!(tickets.resolve(&token, 8).is_err());
        assert!(tickets.resolve("unknown", 7).is_err());
    }

    /// 同一票据可以被 HEAD 与 GET 各用一次，浏览器重试也不会失效；
    /// 真正的边界是到期、单文件绑定与会话代次，而不是一次性使用。
    #[test]
    fn ticket_is_reusable_until_expiry() {
        let tickets = Tickets::new(3);
        let path = VirtualPath::parse("root/a.txt").unwrap();
        let (token, _) = tickets.issue(&path).unwrap();
        assert_eq!(tickets.resolve(&token, 3).unwrap(), path);
        assert_eq!(tickets.resolve(&token, 3).unwrap(), path);
    }

    /// 服务停止时清空票据：即使状态被重新标记为运行，旧票据也不能复活。
    #[test]
    fn clear_invalidates_every_ticket() {
        let tickets = Tickets::new(4);
        let path = VirtualPath::parse("root/a.txt").unwrap();
        let (token, _) = tickets.issue(&path).unwrap();
        tickets.clear();
        assert!(tickets.resolve(&token, 4).is_err());
    }

    /// 过期票据必须被拒绝，且错误码与「从未存在」一致，
    /// 避免把票据表变成可探测的预言机。
    #[test]
    fn expired_ticket_reports_same_error_as_unknown() {
        let tickets = Tickets::new(5);
        let path = VirtualPath::parse("root/a.txt").unwrap();
        let (token, _) = tickets.issue(&path).unwrap();
        {
            let mut inner = tickets.inner.lock().unwrap();
            let entry = inner.entries.get_mut(&token).unwrap();
            entry.expires_at = 0;
        }
        let expired = tickets.resolve(&token, 5).unwrap_err();
        let unknown = tickets.resolve("missing", 5).unwrap_err();
        assert_eq!(expired.code, ErrorCode::TicketExpired);
        assert_eq!(expired.code, unknown.code);
        assert_eq!(expired.detail, unknown.detail);
    }
}
