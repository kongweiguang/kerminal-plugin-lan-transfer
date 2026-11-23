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
        inner.expires_at = Some(expires_at);
        inner.started_at = Some(now_unix());
        inner.error = None;
        Ok(())
    }

    /// 启动失败：保留错误文案与选中的地址，让用户可以直接重试。
    pub fn mark_failed(&self, message: String) -> AppResult<()> {
        let mut inner = self.lock()?;
        inner.phase = Phase::Failed;
        inner.address = None;
        inner.link = None;
        inner.qr = None;
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
        Ok(true)
    }

    /// 停止完成：清空地址、链接与二维码；统计保留到本次会话结束，便于用户回看。
    pub fn mark_idle(&self) -> AppResult<()> {
        let mut inner = self.lock()?;
        inner.phase = Phase::Idle;
        inner.address = None;
        inner.link = None;
        inner.qr = None;
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
}
