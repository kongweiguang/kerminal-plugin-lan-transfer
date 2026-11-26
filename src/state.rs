//! 运行状态机与视图快照。
//! @author kongweiguang
//!
//! 状态只由 Sidecar 维护：视图关闭再打开时向 Sidecar 拉取当前状态，而不是把界面
//! 当作唯一状态源。所有转换都在同一把锁内完成，避免「停止中又被启动」这类交叉。

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
};

use serde::Serialize;

use crate::{
    config::WILDCARD_ADDRESS,
    error::{AppResult, ErrorCode},
    l10n::{Key, Locale},
    net::ListenAddress,
    secret::now_unix,
};

/// 显式状态机。绑定失效不会静默换地址：要么回到 `Idle`，要么停在 `Failed`
/// 等用户重新启动，避免界面显示与实际监听不一致。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Starting,
    Running,
    Stopping,
    Failed,
}

/// 视图地址下拉中的一个选项。
///
/// `value` 会写回 `selected_address`，因此必须是可解析的监听值；`label` 只负责
/// 让用户辨认网卡与用途，可包含接口名或本地化说明，不参与任何校验。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AddressOption {
    pub label: String,
    pub value: String,
}

/// 运行态平铺地址列表中的一行。
///
/// `ip` 是行唯一键，用于行点击切换二维码与行尾复制动作；`address` 用于等宽展示，
/// `group` 承载「局域网 / 本机」分组标签，`hint` 保留网卡名作为悬停提示。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AddressItem {
    pub ip: String,
    pub address: String,
    pub group: String,
    pub hint: String,
}

/// 通配监听本次启动冻结的运行时信息。
///
/// 只保存在内存里：端口、配对密钥与 Host 白名单都以启动那一刻为准，切换展示
/// 地址不重新绑定、不换密钥。停止、失败与下一次启动前必须清空，避免旧密钥
/// 经任何快照路径回到宿主或日志。
#[derive(Clone, Debug)]
pub struct WildcardRuntime {
    /// 本次实际绑定的端口；切换地址时链接与二维码必须沿用同一个端口。
    pub port: u16,
    /// 本次运行的配对密钥；切换地址不得更换，否则已打开的链接会立即失效。
    pub secret: String,
    /// 当前对外展示的 IPv4。
    pub active_ip: Ipv4Addr,
    /// 与 `allowed` 一一对应的下拉选项，标签里带端口。
    pub options: Vec<AddressOption>,
    /// 与 `allowed` 一一对应的平铺地址行（局域网在前、本机在后），用于运行态直接展示并逐行复制。
    pub items: Vec<AddressItem>,
    /// 与 `HostPolicy::wildcard` 同源的 IPv4 白名单。
    pub allowed: Vec<Ipv4Addr>,
}

/// 对外快照；字段名即视图绑定名，增删字段需同步视图文件与状态版本。
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub phase: Phase,
    /// 上次选择（或尝试启动）的共享目录绝对路径。它只用于回填本地面板的目录输入框，
    /// 不参与浏览器请求解析；保存它是为了避免每次打开面板都要重新选择。
    pub root_path: Option<String>,
    /// 运行中时的监听地址（含系统分配端口）。
    pub address: Option<String>,
    /// 一次性配对链接；只在本次运行有效，停止后清空。
    pub link: Option<String>,
    /// 共享目录展示名；不含绝对路径。
    pub root_name: Option<String>,
    /// 二维码矩阵（`1`/`0` 行文本，无静区）；由视图以只读绑定读取。
    pub qr: Option<String>,
    /// 已选监听地址的文本形式，供输入框绑定。
    pub selected_address: String,
    pub addresses: Vec<String>,
    /// 下拉选项；包含具体本机地址、回环与显式全接口 IPv4。
    pub address_options: Vec<AddressOption>,
    /// 可直接展示的状态行与错误行（已本地化）。
    pub status: String,
    pub error: Option<String>,
    /// 仅在运行中的通配监听为 true；非运行态不得泄露本次启动的网络信息。
    pub wildcard: bool,
    /// `wildcard` 的反向布尔量，供视图在两种地址展示形态之间切换。
    pub single_address: bool,
    /// 当前对外展示的 IPv4；非通配运行态为空串。
    pub active_ip: String,
    /// 通配运行期的地址下拉；其他阶段为空。
    pub active_ip_options: Vec<AddressOption>,
    /// 运行期的平铺地址列表（单网卡 1 行，全网卡列出全部白名单 IPv4）；非运行态为空。
    pub address_items: Vec<AddressItem>,
    pub uploaded: u64,
    pub downloaded: u64,
    pub started_at: Option<u64>,
    /// 会话到期时间（Unix 秒），用于视图显示剩余时间。
    pub expires_at: Option<u64>,
}

/// 服务运行期数据；会话与票据随每次启动重建，因此这里只保留展示所需字段。
pub struct State {
    inner: Mutex<Inner>,
}

struct Inner {
    phase: Phase,
    root_path: Option<String>,
    address: Option<SocketAddr>,
    link: Option<String>,
    root_name: Option<String>,
    qr: Option<String>,
    selected_address: String,
    addresses: Vec<ListenAddress>,
    error: Option<String>,
    /// 通配运行的冻结信息；只有 `Running` 且以 `0.0.0.0` 监听时才存在。
    wildcard_runtime: Option<WildcardRuntime>,
    uploaded: u64,
    downloaded: u64,
    started_at: Option<u64>,
    expires_at: Option<u64>,
}

impl State {
    /// 初始化状态；地址列表在状态创建时枚举一次，供下拉选项与具体地址校验
    /// 使用，避免每次刷新都触发网卡枚举。通配启动不依赖这份列表：`service`
    /// 会在真正绑定时重新采样，让链接与 Host 白名单反映当时的网络。
    pub fn new() -> Arc<Self> {
        let addresses = crate::net::list_addresses();
        let selected = addresses
            .first()
            .map(|address| address.ip.to_string())
            .unwrap_or_else(|| crate::config::LOOPBACK_HINT.to_owned());
        Arc::new(Self {
            inner: Mutex::new(Inner {
                phase: Phase::Idle,
                root_path: None,
                address: None,
                link: None,
                root_name: None,
                qr: None,
                selected_address: selected,
                addresses,
                error: None,
                wildcard_runtime: None,
                uploaded: 0,
                downloaded: 0,
                started_at: None,
                expires_at: None,
            }),
        })
    }

    /// 读取快照；锁中毒时返回明确的内部错误，不伪造空状态。
    pub fn snapshot(&self) -> AppResult<Snapshot> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| crate::error::AppError::new(ErrorCode::Server, "state unavailable"))?;
        let runtime = inner.wildcard_runtime.as_ref();
        // 网络信息只在「运行中的通配监听」这一种情况下可见：空闲、启动中、
        // 停止中与失败态都不携带端口/密钥/白名单，避免旧值经视图补丁泄露。
        let wildcard = inner.phase == Phase::Running && runtime.is_some();
        Ok(Snapshot {
            phase: inner.phase,
            root_path: inner.root_path.clone(),
            address: inner.address.map(|address| address.to_string()),
            link: inner.link.clone(),
            root_name: inner.root_name.clone(),
            qr: inner.qr.clone(),
            selected_address: inner.selected_address.clone(),
            addresses: inner
                .addresses
                .iter()
                .map(|address| address.ip.to_string())
                .collect(),
            address_options: address_options(&inner.addresses),
            status: status_text(inner.phase).to_owned(),
            error: inner.error.clone(),
            wildcard,
            single_address: !wildcard,
            // 运行期网络信息与 `wildcard` 同生共死：`Stopping` 时句柄仍在回收，
            // 但链接即将失效，继续展示会诱导用户复制一个马上不可用的地址。
            active_ip: if wildcard {
                runtime
                    .map(|runtime| runtime.active_ip.to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            },
            active_ip_options: if wildcard {
                runtime
                    .map(|runtime| runtime.options.clone())
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
            address_items: if wildcard {
                runtime
                    .map(|runtime| runtime.items.clone())
                    .unwrap_or_default()
            } else if inner.phase == Phase::Running {
                inner
                    .address
                    .map(|bound| vec![single_address_item(&inner.addresses, bound)])
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
            uploaded: inner.uploaded,
            downloaded: inner.downloaded,
            started_at: inner.started_at,
            expires_at: inner.expires_at,
        })
    }

    /// 修改选中地址；只有空闲或失败状态允许改，运行中改地址必须显式停止，
    /// 否则界面显示与实际绑定会不一致。
    ///
    /// 地址文本是用户输入的 IP，前后空白没有语义（与目录路径不同），因此这里
    /// 去掉空白后再校验并保存；`start` 从动作快照读取地址时也做同样处理。
    /// 显式 `0.0.0.0` 是允许的全接口 IPv4 请求；其他不属于本机的地址仍拒绝。
    pub fn select_address(&self, value: &str) -> AppResult<()> {
        let mut inner = self.lock()?;
        apply_address(&mut inner, value, false)
    }

    /// 从宿主保存的视图状态恢复地址选择。
    ///
    /// 与用户动作不同，宿主持久化的草稿可能来自上一进程。全接口监听会显著扩大
    /// 暴露范围，因此不能因为草稿里有 `0.0.0.0` 就在新运行期自动选中；只有本次
    /// 运行期已经由用户显式选过通配地址时才接受同值恢复。具体地址仍按原规则
    /// 校验，失效或非本机地址会被忽略而不影响视图打开。
    pub fn restore_address(&self, value: &str) -> AppResult<()> {
        let mut inner = self.lock()?;
        apply_address(&mut inner, value, true)
    }

    /// 记录本次尝试的共享目录。即使启动失败也保留：用户更可能需要修正刚才那条路径，
    /// 而不是从空输入框重新选择。值只在本进程与宿主视图状态内使用。
    pub fn remember_root(&self, path: &std::path::Path) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.root_path = path.to_str().map(str::to_owned);
        }
    }

    /// 仅在空闲/失败态记录共享目录；运行或停止中不改写显示路径。
    ///
    /// 用于「重新打开视图时恢复上次保存的目录」：视图可能打开在服务运行之后，
    /// 而它携带的目录只是历史快照。运行中改写 `root_path` 会让面板显示 B，
    /// 实际共享仍是启动时打开的 A 目录句柄——显示与事实背离，用户据此判断
    /// 共享范围会出错。非空闲时返回 `false`，由调用方保留现值并如实提示。
    pub fn remember_root_if_idle(&self, path: &std::path::Path) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        if !matches!(inner.phase, Phase::Idle | Phase::Failed) {
            return false;
        }
        inner.root_path = path.to_str().map(str::to_owned);
        true
    }

    /// 无显式地址的调用沿用当前选择，避免测试或内部调用需要复制默认值逻辑。
    pub fn begin_start(&self) -> AppResult<IpAddr> {
        self.begin_start_with_address(None)
    }

    /// 地址校验和进入启动态共享状态锁，避免其他窗口的旧草稿覆盖本次启动选择。
    pub(crate) fn begin_start_with_address(&self, address: Option<&str>) -> AppResult<IpAddr> {
        let mut inner = self.lock()?;
        if !matches!(inner.phase, Phase::Idle | Phase::Failed) {
            return Err(crate::error::AppError::new(ErrorCode::Busy, "service busy"));
        }
        if let Some(address) = address {
            apply_address(&mut inner, address, false)?;
        }
        let ip = inner
            .selected_address
            .parse::<std::net::IpAddr>()
            .map_err(|_| crate::error::AppError::new(ErrorCode::BadRequest, "invalid address"))?;
        inner.phase = Phase::Starting;
        // 进入启动中必须清掉上一次运行的展示字段与时间戳：失败态可能保留着旧链接
        // 或旧的开始时间，界面在「正在启动」期间不应继续显示上一次的地址。
        inner.address = None;
        inner.link = None;
        inner.qr = None;
        // 上一次运行冻结的端口、密钥与白名单不能延续到本次启动：新监听会重新
        // 分配端口和密钥，旧值留在内存里只会成为误导性的过期事实。
        inner.wildcard_runtime = None;
        inner.expires_at = None;
        inner.started_at = None;
        inner.error = None;
        Ok(ip)
    }

    /// 启动成功：记录绑定地址、链接、二维码与会话到期时间。
    ///
    /// 只允许从 `Starting` 进入：监听任务可能在 `mark_running` 之前就失败并把状态
    /// 写成 `Failed`（见 `mark_failed_if_running`），此时再写 `Running` 会掩盖失败，
    /// 让界面显示「运行中」而端口已经不可用。错误码用 `Server` 而不是 `Busy`：
    /// 这不是用户操作冲突，界面应显示内部错误并提供重试，而不是提示「正在切换状态」。
    pub fn mark_running(
        &self,
        address: SocketAddr,
        link: String,
        root_name: String,
        qr: String,
        expires_at: u64,
    ) -> AppResult<()> {
        self.mark_running_share(address, link, root_name, qr, expires_at, None)
    }

    /// 通配监听启动成功：除常规展示字段外，记录本次冻结的端口、密钥与白名单。
    ///
    /// 与单地址启动共用同一段状态校验，避免两条路径对「是否仍在 `Starting`」
    /// 产生不同判断；差异只在是否携带运行期网络信息。
    pub fn mark_running_wildcard(
        &self,
        address: SocketAddr,
        link: String,
        root_name: String,
        qr: String,
        expires_at: u64,
        runtime: WildcardRuntime,
    ) -> AppResult<()> {
        self.mark_running_share(address, link, root_name, qr, expires_at, Some(runtime))
    }

    /// 启动成功的共同实现；`runtime` 为 `None` 表示单地址监听。
    fn mark_running_share(
        &self,
        address: SocketAddr,
        link: String,
        root_name: String,
        qr: String,
        expires_at: u64,
        runtime: Option<WildcardRuntime>,
    ) -> AppResult<()> {
        let mut inner = self.lock()?;
        if inner.phase != Phase::Starting {
            return Err(crate::error::AppError::new(
                ErrorCode::Server,
                "start superseded",
            ));
        }
        inner.phase = Phase::Running;
        inner.address = Some(address);
        inner.link = Some(link);
        inner.root_name = Some(root_name);
        inner.qr = Some(qr);
        inner.wildcard_runtime = runtime;
        inner.expires_at = Some(expires_at);
        inner.started_at = Some(now_unix());
        inner.error = None;
        Ok(())
    }

    /// 在本次通配运行冻结的白名单内切换对外展示的 IPv4。
    ///
    /// 只改 `address`/`link`/`qr` 与选中值，不重新绑定端口、不更换密钥，因此
    /// 已建立的会话、票据与在途传输都不受影响。地址必须来自启动时与
    /// `HostPolicy::wildcard` 同源的白名单：否则面板会展示一个 Host 校验根本
    /// 不接受的地址，二维码扫出来也是打不开的链接。
    pub fn select_active_ip(&self, value: &str) -> AppResult<()> {
        let mut inner = self.lock()?;
        if inner.phase != Phase::Running {
            return Err(crate::error::AppError::new(
                ErrorCode::Unavailable,
                "not running",
            ));
        }
        let ip = value
            .trim()
            .parse::<Ipv4Addr>()
            .map_err(|_| crate::error::AppError::new(ErrorCode::BadRequest, "invalid address"))?;
        let (port, secret) = {
            let runtime = inner.wildcard_runtime.as_ref().ok_or_else(|| {
                crate::error::AppError::new(ErrorCode::BadRequest, "not wildcard")
            })?;
            if !runtime.allowed.contains(&ip) {
                return Err(crate::error::AppError::new(
                    ErrorCode::BadRequest,
                    "address not in active addresses",
                ));
            }
            if runtime.active_ip == ip {
                return Ok(());
            }
            (runtime.port, runtime.secret.clone())
        };
        // 先构造新值再写回：二维码生成失败时保持原地址可用，不会留下
        // 「active_ip 已换、qr 仍是旧值」的半更新状态。
        let link = format!("http://{ip}:{port}/#{secret}");
        let qr = crate::service::render_qr(&link)?;
        if let Some(runtime) = inner.wildcard_runtime.as_mut() {
            runtime.active_ip = ip;
        }
        inner.address = Some(SocketAddr::new(IpAddr::V4(ip), port));
        inner.link = Some(link);
        inner.qr = Some(qr);
        Ok(())
    }

    /// 启动失败：保留错误文案与选中的地址，让用户可以直接重试。
    pub fn mark_failed(&self, message: String) -> AppResult<()> {
        let mut inner = self.lock()?;
        inner.phase = Phase::Failed;
        inner.address = None;
        inner.link = None;
        inner.qr = None;
        inner.wildcard_runtime = None;
        inner.expires_at = None;
        inner.error = Some(message);
        Ok(())
    }

    /// 进入停止中；返回是否需要真正关闭监听。
    ///
    /// `has_handle` 表示控制器仍持有运行句柄。取消或失败的启动可能把状态收回
    /// `Idle`/`Failed`，却留下一个已请求中止但尚未确认退出的任务；此时即使状态是
    /// `Idle` 也必须进入停止流程并如实等待回收，否则「没有句柄就没有资源」的假设
    /// 会变成一次伪报成功。`Failed` 同样必须进入：它正是「上一次启动/停止失败，
    /// 句柄可能仍未确认退出」的可见状态。
    pub fn begin_stop(&self, has_handle: bool) -> AppResult<bool> {
        let mut inner = self.lock()?;
        if inner.phase == Phase::Idle && !has_handle {
            return Ok(false);
        }
        if inner.phase == Phase::Stopping {
            return Err(crate::error::AppError::new(ErrorCode::Busy, "service busy"));
        }
        inner.phase = Phase::Stopping;
        // 进入停止即撤销运行期网络信息：链接与二维码从这一刻起不再可信，
        // 视图也不应继续提供地址切换或复制入口。
        inner.wildcard_runtime = None;
        Ok(true)
    }

    /// 停止完成：清空地址、链接与二维码；统计保留到本次会话结束，便于用户回看。
    pub fn mark_idle(&self) -> AppResult<()> {
        let mut inner = self.lock()?;
        inner.phase = Phase::Idle;
        inner.address = None;
        inner.link = None;
        inner.qr = None;
        inner.wildcard_runtime = None;
        inner.expires_at = None;
        inner.started_at = None;
        Ok(())
    }

    /// 监听任务异常结束时把可见状态落到 `Failed`，但只在仍为 `Running` 或 `Starting`
    /// 时生效。
    ///
    /// 不能无条件调用 `mark_failed`：停止流程会先把状态改成 `Stopping`，若失败回调
    /// 此时再写一次，就会把用户已经成功的停止覆盖成故障。`Starting` 必须包含在内：
    /// 监听任务可能在 `mark_running` 之前就失败，若此时忽略，任务随即退出，而
    /// `mark_running` 又把状态写成运行中，最终留下「运行中但端口已死」的假象。
    /// 覆盖后由启动流程在登记句柄时检查任务是否已结束并回收。
    pub fn mark_failed_if_running(&self, message: String) -> AppResult<()> {
        let mut inner = self.lock()?;
        if !matches!(inner.phase, Phase::Running | Phase::Starting) {
            return Ok(());
        }
        // 撤销与 `Starting` 同一次启动登记下来的展示字段（链接、二维码、绑定地址），
        // 否则失败态会残留一个看起来可用的地址；`root_name`/`root_path` 保留给用户修正。
        inner.phase = Phase::Failed;
        inner.address = None;
        inner.link = None;
        inner.qr = None;
        inner.wildcard_runtime = None;
        inner.expires_at = None;
        inner.started_at = None;
        inner.error = Some(message);
        Ok(())
    }

    /// 启动被取消时把仍处于 `Starting` 的状态收回 `Idle`。
    ///
    /// 为什么需要：SDK 可以在 `view.action` 执行中途取消请求并 drop future，
    /// 此时 `start` 的 `Err` 分支不会执行，状态会永久停在「正在启动…」。
    /// 只处理 `Starting`，避免把其他流程正在接管的阶段（`Stopping`/`Running`）覆盖。
    /// 这里刻意不返回 `Result`：它会在 drop 路径调用，不能 panic 也不能吞掉调用方的
    /// 主要错误；锁中毒时保持原状，由后续停止流程处理。
    pub fn mark_idle_if_starting(&self) {
        if let Ok(mut inner) = self.inner.lock()
            && inner.phase == Phase::Starting
        {
            inner.phase = Phase::Idle;
            inner.address = None;
            inner.link = None;
            inner.qr = None;
            inner.wildcard_runtime = None;
            inner.expires_at = None;
            inner.started_at = None;
        }
    }

    /// 停止流程被取消时把仍处于 `Stopping` 的状态落到可见失败。
    ///
    /// 为什么需要：`stop` 在等待监听任务退出时是一个 await 点，SDK 取消
    /// `view.action` 会直接 drop future，`mark_idle`/`mark_stop_failed` 都不会执行，
    /// 面板会永久停在「正在停止…」，之后的停止请求也会被 `begin_stop` 当成忙拒绝。
    /// 只处理 `Stopping`：正常路径在返回前已经写好 `Idle` 或 `Failed`，不会被覆盖。
    pub fn mark_failed_if_stopping(&self, message: String) {
        if let Ok(mut inner) = self.inner.lock()
            && inner.phase == Phase::Stopping
        {
            inner.phase = Phase::Failed;
            inner.address = None;
            inner.link = None;
            inner.qr = None;
            inner.wildcard_runtime = None;
            inner.expires_at = None;
            inner.started_at = None;
            inner.error = Some(message);
        }
    }

    /// 记录一次成功提交的上传。
    pub fn count_upload(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.uploaded = inner.uploaded.saturating_add(1);
        }
    }

    /// 记录一次开始的下载；只统计服务端已开始发送的文件。
    pub fn count_download(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.downloaded = inner.downloaded.saturating_add(1);
        }
    }

    /// 是否处于运行中；HTTP 处理器据此拒绝已停止服务的残余请求。
    pub fn is_running(&self) -> bool {
        self.inner
            .lock()
            .map(|inner| inner.phase == Phase::Running)
            .unwrap_or(false)
    }

    fn lock(&self) -> AppResult<std::sync::MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| crate::error::AppError::new(ErrorCode::Server, "state unavailable"))
    }
}

/// 校验并写入选中地址；`restoring` 区分用户显式动作与宿主保存草稿的恢复。
///
/// 两者共用同一套本机地址校验，避免恢复路径绕过「不属于本机」检查。差异只在
/// 通配地址：宿主草稿不能把新运行期自动切到全接口；用户在本运行期已经显式
/// 选过时，恢复同值只是幂等确认。
fn apply_address(inner: &mut Inner, value: &str, restoring: bool) -> AppResult<()> {
    if matches!(
        inner.phase,
        Phase::Starting | Phase::Running | Phase::Stopping
    ) {
        return Err(crate::error::AppError::new(ErrorCode::Busy, "service busy"));
    }
    let value = value.trim();
    let parsed = value
        .parse::<IpAddr>()
        .map_err(|_| crate::error::AppError::new(ErrorCode::BadRequest, "invalid address"))?;
    let wildcard = parsed == IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    if wildcard && restoring && inner.selected_address != WILDCARD_ADDRESS {
        // 不做任何提示：宿主草稿只是历史输入，不是用户此刻的动作；视图仍显示
        // 当前具体地址，用户需要全接口时必须重新选择或输入。
        return Ok(());
    }
    if !wildcard && !inner.addresses.iter().any(|address| address.ip == parsed) {
        return Err(crate::error::AppError::new(
            ErrorCode::BadRequest,
            "address not on this host",
        ));
    }
    inner.selected_address = parsed.to_string();
    // 不在这里清 `error`：失败说明通常指向目录或绑定原因，用户只是修正地址
    // 时不应该把原失败原因抹掉，否则面板会只剩「操作失败」而没有任何依据。
    // 下一次启动会由 `begin_start` 清空旧错误并生成新的结果。
    Ok(())
}

/// 生成视图下拉选项：具体地址在前，回环与显式全接口 IPv4 依次在后。
///
/// 接口名能让用户在多个网卡之间辨认用途；回环与通配地址使用本地化说明，
/// 因为它们是固定的语义选项，而不是某块网卡的名字。通配地址始终排在最后，
/// 避免成为默认项，也避免把「监听所有网卡」混进普通本机地址列表中间。
fn address_options(addresses: &[ListenAddress]) -> Vec<AddressOption> {
    let locale = Locale::detect();
    let mut options = addresses
        .iter()
        .map(|address| {
            let label = match address.ip {
                IpAddr::V4(ip) if ip.is_loopback() => {
                    locale.text(Key::AddressOptionLoopback).to_owned()
                }
                _ if !address.label.is_empty() && address.label != "loopback" => {
                    format!("{} · {}", address.ip, address.label)
                }
                _ => address.ip.to_string(),
            };
            AddressOption {
                label,
                value: address.ip.to_string(),
            }
        })
        .collect::<Vec<_>>();
    options.push(AddressOption {
        label: locale.text(Key::AddressOptionAllInterfaces).to_owned(),
        value: WILDCARD_ADDRESS.to_owned(),
    });
    options
}

/// 生成通配运行期的地址下拉选项。
///
/// `allowed` 必须与 `HostPolicy::wildcard` 使用同一份白名单，顺序也直接沿用
/// `list_addresses` 的排序（物理网卡、虚拟网卡、回环），因此下拉里出现的地址
/// 就是 Host 校验会接受的地址，且默认第一项不会落在 WSL/Docker 等虚拟网卡上。
/// 标签固定带端口，用户切换后能立刻确认二维码与复制链接指向哪个监听端点。
pub fn active_ip_options(
    addresses: &[ListenAddress],
    allowed: &[Ipv4Addr],
    port: u16,
) -> Vec<AddressOption> {
    let locale = Locale::detect();
    allowed
        .iter()
        .map(|ip| {
            let label = if ip.is_loopback() {
                locale.text(Key::ActiveIpLoopback).to_owned()
            } else {
                addresses
                    .iter()
                    .find(|address| address.ip == IpAddr::V4(*ip))
                    .map(|address| address.label.clone())
                    .filter(|label| !label.is_empty() && label != "loopback")
                    .unwrap_or_else(|| ip.to_string())
            };
            AddressOption {
                label: format!("{ip}:{port} · {label}"),
                value: ip.to_string(),
            }
        })
        .collect()
}

/// 生成通配运行期的平铺地址行：局域网地址在前（物理网卡优先），本机回环在后。
///
/// 与 `active_ip_options` 共享同一份启动冻结白名单；每行自带 `group`（局域网 / 本机）
/// 与 `hint`（网卡名），面板可按参考样式平铺展示并支持逐行单点复制。
///
/// @author kongweiguang
pub fn active_ip_items(
    addresses: &[ListenAddress],
    allowed: &[Ipv4Addr],
    port: u16,
) -> Vec<AddressItem> {
    let locale = Locale::detect();
    allowed
        .iter()
        .map(|ip| {
            let group = if ip.is_loopback() {
                locale.text(Key::GroupLocal)
            } else {
                locale.text(Key::GroupLan)
            }
            .to_owned();
            let hint = if ip.is_loopback() {
                locale.text(Key::ActiveIpLoopback).to_owned()
            } else {
                addresses
                    .iter()
                    .find(|address| address.ip == IpAddr::V4(*ip))
                    .map(|address| address.label.clone())
                    .filter(|label| !label.is_empty() && label != "loopback")
                    .unwrap_or_default()
            };
            AddressItem {
                ip: ip.to_string(),
                address: format!("{ip}:{port}"),
                group,
                hint,
            }
        })
        .collect()
}

/// 生成单地址运行期的平铺地址行，使单网卡与全网卡共用同一套列表呈现与复制交互。
///
/// @author kongweiguang
fn single_address_item(addresses: &[ListenAddress], bound: SocketAddr) -> AddressItem {
    let locale = Locale::detect();
    let loopback = bound.ip().is_loopback();
    let group = if loopback {
        locale.text(Key::GroupLocal)
    } else {
        locale.text(Key::GroupLan)
    }
    .to_owned();
    let hint = if loopback {
        locale.text(Key::ActiveIpLoopback).to_owned()
    } else {
        addresses
            .iter()
            .find(|address| address.ip == bound.ip())
            .map(|address| address.label.clone())
            .filter(|label| !label.is_empty() && label != "loopback")
            .unwrap_or_default()
    };
    AddressItem {
        ip: bound.ip().to_string(),
        address: bound.to_string(),
        group,
        hint,
    }
}

/// 状态行文案；与 `l10n::Key` 保持同一套措辞。
fn status_text(phase: Phase) -> &'static str {
    let locale = crate::l10n::Locale::detect();
    use crate::l10n::Key;
    match phase {
        Phase::Idle => locale.text(Key::StatusIdle),
        Phase::Starting => locale.text(Key::StatusStarting),
        Phase::Running => locale.text(Key::StatusRunning),
        Phase::Stopping => locale.text(Key::StatusStopping),
        Phase::Failed => locale.text(Key::StatusFailed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    /// 构造一个空闲状态并强制把选中地址设为回环；本机地址列表因环境而异，
    /// 测试不能假设一定存在某块网卡。
    fn idle() -> Arc<State> {
        let state = State::new();
        state.select_address("127.0.0.1").unwrap();
        state
    }

    /// 启动失败后必须能从 `Failed` 直接重试，而不是要求先做一次多余的停止；
    /// 这是用户修正目录后点「启动」的实际路径。
    #[test]
    fn failed_state_can_restart() {
        let state = idle();
        state.begin_start().unwrap();
        state.mark_failed("boom".into()).unwrap();
        let snapshot = state.snapshot().unwrap();
        assert_eq!(snapshot.phase, Phase::Failed);
        assert_eq!(snapshot.error.as_deref(), Some("boom"));
        assert!(snapshot.address.is_none() && snapshot.link.is_none());
        // 失败态再次启动合法，并清掉上一次的错误行。
        state.begin_start().unwrap();
        assert_eq!(state.snapshot().unwrap().phase, Phase::Starting);
        assert!(state.snapshot().unwrap().error.is_none());
    }

    /// 运行中重复启动与重复停止都必须被拒绝；交叉操作会让界面显示与实际监听不一致。
    #[test]
    fn running_state_rejects_duplicate_transitions() {
        let state = idle();
        state.begin_start().unwrap();
        let bound: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        state
            .mark_running(bound, "link".into(), "share".into(), "qr".into(), 10)
            .unwrap();
        assert!(state.begin_start().is_err());
        assert!(state.begin_stop(true).unwrap());
        assert_eq!(state.snapshot().unwrap().phase, Phase::Stopping);
        // 停止中既不能再启动，也不能再次进入停止。
        assert!(state.begin_start().is_err());
        assert!(state.begin_stop(true).is_err());
        state.mark_idle().unwrap();
        assert_eq!(state.snapshot().unwrap().phase, Phase::Idle);
        // 已空闲时停止是空操作，返回 false 表示无需关闭监听。
        assert!(!state.begin_stop(false).unwrap());
        // 但空闲态若控制器仍持有未确认退出的句柄，必须进入停止流程等待回收，
        // 否则「空闲即无资源」会让停止直接成功返回。
        assert!(state.begin_stop(true).unwrap());
        state.mark_idle().unwrap();
        // 失败态同样可能有未确认退出的句柄（取消或超时的启动），不能因为
        // 不是 `Idle` 就跳过；停止必须进入等待并如实报告结果。
        state.mark_failed("previous failure".into()).unwrap();
        assert!(state.begin_stop(true).unwrap());
        state.mark_idle().unwrap();
    }

    /// 运行中不允许改监听地址：改地址必须先停止，否则链接指向的端口与界面显示不符。
    #[test]
    fn address_cannot_change_while_busy() {
        let state = idle();
        state.begin_start().unwrap();
        let bound: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        state
            .mark_running(bound, "link".into(), "share".into(), "qr".into(), 10)
            .unwrap();
        assert_eq!(
            state.select_address("127.0.0.1").unwrap_err().code,
            ErrorCode::Busy
        );
        // 不在本机的地址即使空闲也拒绝，避免启动时才发现无法绑定。
        state.mark_idle().unwrap();
        assert_eq!(
            state.select_address("203.0.113.9").unwrap_err().code,
            ErrorCode::BadRequest
        );
        // 非法文本同样拒绝，不静默回退到旧值。
        assert!(state.select_address("not-an-ip").is_err());
    }

    /// 失败态修正监听地址时不能清掉原失败说明：用户只是改地址，并不代表
    /// 目录或绑定故障的原因已经解决；面板仍需显示上一次的具体原因。
    #[test]
    fn failed_explanation_survives_address_edit() {
        let state = idle();
        state.begin_start().unwrap();
        state.mark_failed("无法绑定该地址与端口".into()).unwrap();
        state.select_address("127.0.0.1").unwrap();
        let snapshot = state.snapshot().unwrap();
        assert_eq!(snapshot.phase, Phase::Failed);
        assert_eq!(snapshot.error.as_deref(), Some("无法绑定该地址与端口"));
    }

    /// 显式全接口选择必须在本运行期的失败重试与停止后保留；否则用户每次
    /// 重试都要重新打开下拉，且可能误以为服务已退回某个具体地址。
    #[test]
    fn wildcard_selection_survives_failed_retry_and_stop() {
        let state = State::new();
        state.select_address("0.0.0.0").unwrap();
        assert_eq!(
            state.begin_start().unwrap(),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
        state.mark_failed("bind failed".into()).unwrap();
        assert_eq!(
            state.begin_start().unwrap(),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            "failure retry must keep the explicit wildcard request"
        );
        state.begin_stop(true).unwrap();
        state.mark_idle().unwrap();
        let snapshot = state.snapshot().unwrap();
        assert_eq!(snapshot.selected_address, "0.0.0.0");
        // 链接与 Host 白名单现在由启动时的同一次网卡样本生成，相关断言在
        // `service` 与 `net` 的测试里覆盖；这里只关心选择本身是否被保留。
    }

    /// 宿主保存的旧草稿不能把新运行期自动切到全接口；只有本运行期已显式
    /// 选择过通配地址时，恢复同值才是允许的幂等操作。
    #[test]
    fn stale_wildcard_is_not_restored_without_explicit_selection() {
        let state = State::new();
        let before = state.snapshot().unwrap().selected_address;
        assert_ne!(before, "0.0.0.0", "default must stay concrete");
        state.restore_address("0.0.0.0").unwrap();
        assert_eq!(state.snapshot().unwrap().selected_address, before);

        state.select_address("0.0.0.0").unwrap();
        state.restore_address("0.0.0.0").unwrap();
        assert_eq!(state.snapshot().unwrap().selected_address, "0.0.0.0");

        // 恢复路径仍拒绝不属于本机的具体地址；视图打开会忽略该错误。
        assert!(state.restore_address("203.0.113.9").is_err());
    }

    /// 启动一旦采纳动作地址，其他窗口的选择或恢复都不得改写本次绑定范围。
    #[test]
    fn start_pins_the_action_address_until_the_phase_changes() {
        let state = idle();
        let ip = state.begin_start_with_address(Some("0.0.0.0")).unwrap();
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(
            state.select_address("127.0.0.1").unwrap_err().code,
            ErrorCode::Busy
        );
        assert_eq!(
            state.restore_address("127.0.0.1").unwrap_err().code,
            ErrorCode::Busy
        );
        assert_eq!(state.snapshot().unwrap().selected_address, "0.0.0.0");
    }

    /// 下拉标签必须注明具体 IP 和回环范围；通配放在末尾，避免默认扩大监听范围。
    #[test]
    fn address_options_include_loopback_and_wildcard_last() {
        let state = idle();
        let options = state.snapshot().unwrap().address_options;
        assert!(!options.is_empty());
        assert!(options.iter().all(|option| !option.label.is_empty()));
        assert_eq!(
            options.last().map(|option| option.value.as_str()),
            Some("0.0.0.0")
        );
        let loopback = options
            .iter()
            .find(|option| option.value == "127.0.0.1")
            .expect("loopback option");
        assert!(loopback.label.contains("127.0.0.1"));
        assert!(options.last().unwrap().label.contains("IPv4"));
    }

    /// 停止后必须清空链接、二维码、地址与时间戳；统计保留用于回看本次会话。
    #[test]
    fn idle_clears_share_state_but_keeps_counters() {
        let state = idle();
        state.begin_start().unwrap();
        let bound: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        state
            .mark_running(bound, "link".into(), "share".into(), "qr".into(), 10)
            .unwrap();
        state.count_upload();
        state.count_download();
        state.count_download();
        assert!(state.is_running());
        state.begin_stop(true).unwrap();
        assert!(!state.is_running());
        state.mark_idle().unwrap();
        let snapshot = state.snapshot().unwrap();
        assert!(snapshot.link.is_none() && snapshot.qr.is_none() && snapshot.address.is_none());
        assert!(snapshot.started_at.is_none() && snapshot.expires_at.is_none());
        assert_eq!(snapshot.uploaded, 1);
        assert_eq!(snapshot.downloaded, 2);
        // 选中地址用于下次启动，停止后仍保留。
        assert_eq!(
            snapshot.selected_address,
            IpAddr::V4(Ipv4Addr::LOCALHOST).to_string()
        );
    }

    /// 监听任务失败在 `Starting`/`Running` 时落到 `Failed`；停止中或已空闲都不能
    /// 被覆盖，否则用户已经成功的停止会显示成故障，或空闲态凭空出现错误行。
    /// 同时验证 `mark_running` 不能从 `Failed` 复活服务。
    #[test]
    fn runtime_failure_only_applies_while_running() {
        let state = idle();
        // 空闲态：忽略，不制造错误行。
        state.mark_failed_if_running("boom".into()).unwrap();
        assert_eq!(state.snapshot().unwrap().phase, Phase::Idle);

        state.begin_start().unwrap();
        let bound: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        state
            .mark_running(bound, "link".into(), "share".into(), "qr".into(), 10)
            .unwrap();
        state.mark_failed_if_running("boom".into()).unwrap();
        let snapshot = state.snapshot().unwrap();
        assert_eq!(snapshot.phase, Phase::Failed);
        assert_eq!(snapshot.error.as_deref(), Some("boom"));
        assert!(snapshot.link.is_none() && snapshot.qr.is_none() && snapshot.address.is_none());

        // 停止中：忽略，避免覆盖停止流程正在处理的状态。
        state.begin_start().unwrap();
        state
            .mark_running(bound, "link".into(), "share".into(), "qr".into(), 10)
            .unwrap();
        state.begin_stop(true).unwrap();
        state.mark_failed_if_running("boom".into()).unwrap();
        assert_eq!(state.snapshot().unwrap().phase, Phase::Stopping);
        state.mark_idle().unwrap();

        // 启动中失败（监听任务在 `mark_running` 之前退出）：必须保留失败，
        // 之后的 `mark_running` 不能再把服务写成运行中。
        state.begin_start().unwrap();
        state.mark_failed_if_running("early exit".into()).unwrap();
        assert_eq!(state.snapshot().unwrap().phase, Phase::Failed);
        assert!(
            state
                .mark_running(bound, "link".into(), "share".into(), "qr".into(), 10)
                .is_err()
        );
        assert_eq!(state.snapshot().unwrap().phase, Phase::Failed);
    }

    /// 计数饱和而不是回绕：长期运行后计数溢出不能变成 0，让界面显示「没有传输」。
    #[test]
    fn counters_saturate() {
        let state = idle();
        {
            let mut inner = state.inner.lock().unwrap();
            inner.uploaded = u64::MAX;
        }
        state.count_upload();
        assert_eq!(state.snapshot().unwrap().uploaded, u64::MAX);
    }

    /// 构造一个通配运行态；白名单与选项使用固定地址，避免测试依赖真实网卡。
    fn wildcard_state() -> Arc<State> {
        let state = State::new();
        state.select_address("0.0.0.0").unwrap();
        state.begin_start().unwrap();
        let runtime = WildcardRuntime {
            port: 65395,
            secret: "Ab3xY9_qLmN0pQrS".into(),
            active_ip: Ipv4Addr::new(192, 168, 1, 10),
            options: vec![
                AddressOption {
                    label: "192.168.1.10:65395 · Ethernet".into(),
                    value: "192.168.1.10".into(),
                },
                AddressOption {
                    label: "127.0.0.1:65395 · 仅本机".into(),
                    value: "127.0.0.1".into(),
                },
            ],
            items: vec![
                AddressItem {
                    ip: "192.168.1.10".into(),
                    address: "192.168.1.10:65395".into(),
                    group: "局域网".into(),
                    hint: "Ethernet".into(),
                },
                AddressItem {
                    ip: "127.0.0.1".into(),
                    address: "127.0.0.1:65395".into(),
                    group: "本机".into(),
                    hint: "仅本机".into(),
                },
            ],
            allowed: vec![Ipv4Addr::new(192, 168, 1, 10), Ipv4Addr::LOCALHOST],
        };
        state
            .mark_running_wildcard(
                "192.168.1.10:65395".parse().unwrap(),
                "http://192.168.1.10:65395/#Ab3xY9_qLmN0pQrS".into(),
                "share".into(),
                "qr-old".into(),
                10,
                runtime,
            )
            .unwrap();
        state
    }

    /// 切换展示 IP 必须复用同一端口与密钥，并同步更新地址、链接和二维码；
    /// 阶段保持 `Running`，证明没有重启监听或断开会话。
    #[test]
    fn select_active_ip_switches_link_and_qr_in_place() {
        let state = wildcard_state();
        let before = state.snapshot().unwrap();
        assert!(before.wildcard && !before.single_address);
        assert_eq!(before.active_ip, "192.168.1.10");
        assert_eq!(before.active_ip_options.len(), 2);
        assert_eq!(before.address.as_deref(), Some("192.168.1.10:65395"));
        let started_at = before.started_at;
        let expires_at = before.expires_at;

        state.select_active_ip("127.0.0.1").unwrap();
        let after = state.snapshot().unwrap();
        assert_eq!(after.phase, Phase::Running);
        assert_eq!(after.active_ip, "127.0.0.1");
        assert_eq!(after.address.as_deref(), Some("127.0.0.1:65395"));
        assert_eq!(
            after.link.as_deref(),
            Some("http://127.0.0.1:65395/#Ab3xY9_qLmN0pQrS"),
            "port and secret must be reused"
        );
        assert!(after.qr.is_some());
        assert_ne!(after.qr, before.qr, "qr must follow the new link");
        assert_eq!(after.started_at, started_at);
        assert_eq!(after.expires_at, expires_at);
    }

    /// 白名单外、非法格式、单地址模式与未运行状态都必须拒绝，不能悄悄显示
    /// 一个 Host 校验不接受的地址。
    #[test]
    fn select_active_ip_rejects_outside_frozen_allowlist() {
        let state = wildcard_state();
        assert_eq!(
            state.select_active_ip("203.0.113.9").unwrap_err().code,
            ErrorCode::BadRequest
        );
        assert_eq!(
            state.select_active_ip("not-an-ip").unwrap_err().code,
            ErrorCode::BadRequest
        );
        // 非法输入不能改变当前展示地址。
        assert_eq!(state.snapshot().unwrap().active_ip, "192.168.1.10");

        state.begin_stop(true).unwrap();
        state.mark_idle().unwrap();
        assert_eq!(
            state.select_active_ip("127.0.0.1").unwrap_err().code,
            ErrorCode::Unavailable
        );

        // 单地址监听没有运行期白名单，切换动作必须拒绝而不是静默成功。
        let single = idle();
        single.begin_start().unwrap();
        single
            .mark_running(
                "127.0.0.1:1234".parse().unwrap(),
                "link".into(),
                "share".into(),
                "qr".into(),
                10,
            )
            .unwrap();
        assert_eq!(
            single.select_active_ip("127.0.0.1").unwrap_err().code,
            ErrorCode::BadRequest
        );
    }

    /// 停止后不得留下端口、密钥、白名单或选项；快照只能看到回退后的选中输入值。
    #[test]
    fn stop_clears_wildcard_runtime() {
        let state = wildcard_state();
        state.begin_stop(true).unwrap();
        let stopping = state.snapshot().unwrap();
        assert!(!stopping.wildcard);
        assert!(stopping.single_address);
        assert!(stopping.active_ip.is_empty());
        assert!(stopping.active_ip_options.is_empty());
        state.mark_idle().unwrap();
        let idle = state.snapshot().unwrap();
        assert!(idle.active_ip.is_empty());
        assert!(idle.active_ip_options.is_empty());
        assert!(idle.address.is_none() && idle.qr.is_none());
        // 用户显式选择的全接口输入仍保留，供下次启动使用。
        assert_eq!(idle.selected_address, "0.0.0.0");
    }

    /// 选项顺序沿用白名单顺序，标签必须带端口；回环使用本地化「仅本机」。
    #[test]
    fn active_ip_options_follow_allowlist_and_include_port() {
        let addresses = vec![
            ListenAddress {
                ip: "192.168.1.10".parse().unwrap(),
                label: "Ethernet".into(),
            },
            ListenAddress {
                ip: "127.0.0.1".parse().unwrap(),
                label: "loopback".into(),
            },
        ];
        let allowed = vec![Ipv4Addr::new(192, 168, 1, 10), Ipv4Addr::LOCALHOST];
        let options = active_ip_options(&addresses, &allowed, 65395);
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].value, "192.168.1.10");
        assert_eq!(options[0].label, "192.168.1.10:65395 · Ethernet");
        assert_eq!(options[1].value, "127.0.0.1");
        assert!(options[1].label.contains("127.0.0.1:65395"));
        assert!(
            options[1]
                .label
                .contains(Locale::detect().text(Key::ActiveIpLoopback))
        );
    }
}
