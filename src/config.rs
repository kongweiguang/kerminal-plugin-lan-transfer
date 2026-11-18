//! 局域网文件传输 Sidecar 的固定配置与限制。
//! @author kongweiguang
//!
//! 这里集中放置身份、上限和内部暂存前缀：清单、协议握手、HTTP 路由和视图必须使用同一组
//! 值，否则会出现「清单声明 0.1.0、握手报另一个版本」这类只能重启才暴露的错位。

/// 与 `kerminal-plugin.json` 的 id 完全一致；SDK 握手会拒绝任何不一致的自报身份。
pub const PLUGIN_ID: &str = "kongweiguang.lan-transfer";
/// 构建脚本通过环境变量注入清单版本；本地 cargo run 回退到清单当前版本。
pub const PLUGIN_VERSION: &str = match option_env!("KERMINAL_PLUGIN_VERSION") {
    Some(version) => version,
    None => "0.1.0",
};
/// 原生视图键，必须与清单 `views` 的键一致。
pub const VIEW_ID: &str = "lan.transfer.panel";
/// 视图状态版本；变更状态形状时递增，宿主据此拒绝旧草稿。
pub const VIEW_STATE_VERSION: u32 = 1;

/// 单页列表条目数：过大会让一次 RPC 补丁超过宿主 1 MiB 限制。
pub const PAGE_SIZE: usize = 200;
/// 单目录最大条目数。超过时列举直接失败并映射为 `too_large`：
/// 截断后的列表会被页面当成完整目录，用户找不到文件却没有任何提示。
pub const MAX_DIR_ENTRIES: usize = 20_000;
/// 单条路径最大层级，避免用极深目录放大逐段打开的成本。
pub const MAX_PATH_COMPONENTS: usize = 64;
/// 单个文件名字节上限；与常见文件系统限制一致。
pub const MAX_NAME_BYTES: usize = 255;
/// 单次上传字节上限（8 GiB）；超过立即拒绝，避免写满磁盘。
pub const MAX_UPLOAD_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// 同时进行的上传数量；与页面自身并发（2）相比留出余量，但不允许无限并发。
pub const MAX_CONCURRENT_UPLOADS: usize = 4;
/// 会话有效期（秒）；到期后必须重新打开分享链接。
pub const SESSION_TTL_SECS: u64 = 12 * 60 * 60;
/// 下载票据有效期（秒）；允许浏览器先 HEAD 再 GET 并重试。
pub const TICKET_TTL_SECS: u64 = 300;
/// 内部暂存前缀；列表和直接访问都会拒绝以它开头的名字。
pub const TEMP_PREFIX: &str = ".klt-tmp-";
/// 用户点击「停止」时等待监听任务回收的绝对上限。
///
/// 比宿主关闭宽限（约 2 秒）宽松：用户停止不需要赶在宿主强杀之前完成，
/// 宁可多等几秒也要拿到「任务真的退出」这一确认。
pub const STOP_TIMEOUT_SECS: u64 = 5;
/// 进程退出（宿主 `shutdown`）时的回收预算。
///
/// 宿主收到 `shutdown` 回复后约 2 秒就会强杀，因此这里留出余量：既要给撤销会话、
/// 关闭监听留时间，又必须在宽限期内结束。无法在预算内确认回收时如实返回失败，
/// 由宿主按退出码判断，而不是伪报成功。
pub const SHUTDOWN_TIMEOUT_SECS: u64 = 1;
/// 允许绑定的回环地址也保留，但界面默认选择第一个非回环 IPv4。
pub const LOOPBACK_HINT: &str = "127.0.0.1";
