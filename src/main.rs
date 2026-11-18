//! Sidecar 二进制入口：把标准输入输出接到协议循环，并把失败原因写到标准错误。
//! @author kongweiguang
//!
//! 标准输出只承载有界 NDJSON 协议帧，所有诊断都写标准错误；这样监督器不会把
//! 日志误当成协议消息，也不会因为日志量导致帧解析失败。

use std::process::ExitCode;

use kerminal_plugin_lan_transfer::{entry::serve, service::Controller};

/// 入口只做装配与退出码映射；协议、视图方法与回收顺序都在 `entry` 模块里，
/// 便于集成测试用内存管道覆盖同一段代码。
#[tokio::main]
async fn main() -> ExitCode {
    let controller = Controller::new();
    match serve(&controller, tokio::io::stdin(), tokio::io::stdout()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lan-transfer sidecar stopped: {error}");
            ExitCode::FAILURE
        }
    }
}
