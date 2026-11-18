//! 随机令牌、时间与摘要等无状态辅助。
//! @author kongweiguang
//!
//! 令牌一律由操作系统 CSPRNG 生成；摘要只用于内存索引，避免在进程内存里长期保存
//! 可直接使用的令牌明文，日志和视图数据都不应出现这些值。

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult, ErrorCode};

/// Unix 秒；会话与票据的到期判断统一用这一时钟，避免各模块各自取时间导致边界不一致。
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

/// 生成 URL 安全的高熵令牌；随机源失败时返回错误而不是退化成可预测值。
pub fn random_token(bytes: usize) -> AppResult<String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer)
        .map_err(|_| AppError::new(ErrorCode::Server, "system random source unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

/// 令牌在内存中只保存摘要；即使进程内存被读取也无法直接复用会话。
pub fn token_digest(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// 定长比较，避免用 `==` 比较配对密钥时泄漏前缀匹配长度。
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    left.len() == right.len() && left.ct_eq(right).into()
}
