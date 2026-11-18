//! 局域网文件传输 Sidecar 库入口。
//! @author kongweiguang
//!
//! 拆出库目标是让 HTTP 契约、路径安全与票据逻辑能被集成测试直接调用；
//! 二进制入口只负责装配 SDK 路由和进程级清理。

pub mod config;
pub mod entry;
pub mod error;
pub mod fs_root;
pub mod http;
pub mod l10n;
pub mod net;
pub mod path;
pub mod rpc;
pub mod secret;
pub mod service;
pub mod session;
pub mod state;
pub mod ticket;
