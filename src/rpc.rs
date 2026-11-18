//! SDK 视图方法：打开/动作/快照/关闭与主动更新。
//! @author kongweiguang
//!
//! 视图只是控制面：所有状态来自 `Controller`，关闭视图不停止服务；重新打开时
//! `view.open` 直接返回当前快照，而不是依赖界面缓存。

use std::sync::Arc;

use kerminal_plugin_sdk::{Context, Error, Result};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    config::{VIEW_ID, VIEW_STATE_VERSION},
    service::Controller,
    state::Snapshot,
};

/// 视图实例注册表：同一视图可能被打开多次（不同窗口），主动更新需要逐个通知。
///
/// 构造通过 `for_controller`：注册表反向持有控制器（SDK 协议回调需要它读写状态），
/// 控制器侧只保留弱引用，避免 `Arc` 互相持有造成进程退出前无法释放的环。
pub struct ViewRegistry {
    controller: Arc<Controller>,
    instances: tokio::sync::Mutex<Vec<String>>,
    /// 最近一次动作携带的 SDK 上下文。统计推送任务（HTTP 完成后触发）需要一个
    /// 能把 `view.update` 发回宿主的句柄，而 `view.open` 在协议层拿不到上下文；
    /// 只有 `view.action` 能提供。因此统计广播在「至少发生过一次动作」之后才生效，
    /// 这一限制如实记录，不用占位数据假装已经推送。
    context: tokio::sync::Mutex<Option<Context>>,
    /// 统计推送任务只启动一次的标记；`open` 可并发进入，用 `Once` 保证唯一。
    stats_task: std::sync::Once,
}

impl ViewRegistry {
    /// 创建注册表；控制器由注册表持有，统计推送任务直接持有注册表的强引用。
    ///
    /// 为什么不让控制器反向持有注册表：注册表的生命周期只覆盖协议循环，
    /// 控制器若长期持有它（即使弱引用）会引入构造期的双向依赖；统计任务在
    /// 首个 `view.open` 时以 `Arc<ViewRegistry>` 自持，进程退出时随任务结束。
    pub fn new(controller: Arc<Controller>) -> Self {
        Self {
            controller,
            instances: tokio::sync::Mutex::new(Vec::new()),
            context: tokio::sync::Mutex::new(None),
            stats_task: std::sync::Once::new(),
        }
    }

    /// 统计推送任务：控制器统计修订号变化后向已打开实例推送当前快照。
    ///
    /// 只在首个 `view.open` 时启动一次：任务与实例列表共存亡（实例为空时跳过推送），
    /// 不随代次重启。上下文取注册表里最近一次 `view.action` 保存的句柄；从未发生过
    /// 动作时保持静默——HTTP 传输仍正常计数，只是面板统计要等下一次动作回执刷新，
    /// 这比伪造一个不可用的上下文更诚实。
    async fn spawn_stats_watcher(self: &Arc<Self>) {
        self.stats_task.call_once(|| {
            let registry = Arc::clone(self);
            tokio::spawn(async move {
                let mut receiver = registry.controller.stats().subscribe();
                loop {
                    if receiver.changed().await.is_err() {
                        // 发送端（控制器）已销毁：进程正在退出，任务随之结束。
                        return;
                    }
                    // 推送前取当前快照：连续多次变更合并为一次最新值推送，
                    // 既减少通道占用，也避免把过时的中间计数写给面板。
                    let Ok(snapshot) = registry.controller.state.snapshot() else {
                        return;
                    };
                    let patch = data(&snapshot);
                    let instances = registry.instances.lock().await.clone();
                    let Some(context) = registry.context.lock().await.clone() else {
                        // 还没有任何动作提供过上下文：保持静默，等下次变更再试。
                        continue;
                    };
                    for instance in instances {
                        let _ = context
                            .notify(
                                "view.update",
                                json!({ "instance": instance, "patch": patch }),
                            )
                            .await;
                    }
                }
            });
        });
    }

    /// `view.open`：登记实例并返回完整数据；恢复的旧状态只用于选中地址等非敏感字段。
    pub async fn open(self: &Arc<Self>, params: Value, cancel: CancellationToken) -> Result<Value> {
        let instance = instance_id(&params)?;
        let view = params
            .get("view")
            .and_then(Value::as_str)
            .unwrap_or(VIEW_ID);
        if view != VIEW_ID {
            return Err(Error::new(-32602, "unknown view"));
        }
        // 取消检查放在登记之前：已取消的打开请求不应留下一个永远不会被关闭的
        // 实例，否则后续每次广播都会尝试向一个不存在的窗口推送。登记之后的
        // 取消同样要注销（见下方），两条路径都覆盖。
        if cancel.is_cancelled() {
            return Err(Error::new(-32002, "cancelled"));
        }
        self.spawn_stats_watcher().await;
        {
            let mut instances = self.instances.lock().await;
            if !instances.iter().any(|existing| existing == &instance) {
                instances.push(instance.clone());
            }
        }
        // 恢复地址选择：只接受当前仍存在的本机地址，旧状态里的其他字段一律忽略，
        // 避免把上一次运行的链接或令牌带回新会话。
        if let Some(state) = params.get("state") {
            if let Some(address) = state.get("selected_address").and_then(Value::as_str) {
                let _ = self.controller.state.select_address(address);
            }
            // 目录路径只是本地输入草稿，回填失败不影响打开视图；真正校验发生在启动时。
            // 必须走带运行态门禁的入口：运行中重开一个携带旧目录的视图时，无条件
            // 写回会让面板显示旧目录 B，而实际共享仍是启动时打开的 A——用户据此
            // 判断共享范围会出错。
            if let Some(path) = state.get("root_path").and_then(Value::as_str) {
                let _ = self
                    .controller
                    .state
                    .remember_root_if_idle(std::path::Path::new(path));
            }
        }
        let snapshot = self.snapshot()?;
        if cancel.is_cancelled() {
            // 迟到取消：注销刚登记的实例，避免注册表里留下无主窗口。
            let mut instances = self.instances.lock().await;
            instances.retain(|existing| existing != &instance);
            return Err(Error::new(-32002, "cancelled"));
        }
        Ok(json!({ "data": data(&snapshot) }))
    }

    /// `view.action`：返回补丁而不是整份数据，让宿主的合并语义与大小上限保持一致。
    ///
    /// 需要宿主能力（复制链接）的动作必须走 `context.request`：视图补丁只会写回数据，
    /// 不会替插件完成剪贴板写入，否则界面会显示「已复制」而系统剪贴板仍是旧内容。
    pub async fn action(
        &self,
        context: &Context,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let action = params
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::new(-32602, "missing action"))?;
        // 局部变量不能与下方 `data()` 同名，否则 `data(&snapshot)` 会被解析成值调用。
        let payload = params.get("data").cloned().unwrap_or(Value::Null);
        // 发起动作的实例由动作回执更新；广播时排除它，避免同一补丁被应用两次。
        let caller = instance_id(&params).ok();
        if cancel.is_cancelled() {
            return Err(Error::new(-32002, "cancelled"));
        }
        // 记下最近一次动作的上下文：统计推送任务只在 HTTP 完成时被唤醒，
        // 那时没有活跃请求可用，只能复用这个句柄。
        *self.context.lock().await = Some(context.clone());
        let outcome = self
            .dispatch(context, action, &payload, cancel.clone())
            .await;
        // 失败也要广播：其他已打开窗口若仍显示旧状态（例如某个窗口发起的启动失败），
        // 它们不会收到动作回执，只能靠广播看到「启动失败 / 已停止」。只在成功时
        // 广播会让多窗口状态长期不一致。广播内容是当前快照，不含错误细节之外的
        // 敏感信息（快照本身已有 `error` 字段用于显示）。
        let snapshot = self.snapshot()?;
        let patch = data(&snapshot);
        self.broadcast(context, caller.as_deref(), patch.clone())
            .await;
        match outcome {
            Ok(()) => Ok(json!({ "patch": patch })),
            Err(error) => Err(error),
        }
    }

    /// 执行单个动作；成功时返回 `Ok(())`，失败如实返回协议错误。
    ///
    /// 拆出来是为了让 `action` 统一在「成功或失败」之后广播快照：失败分支若提前
    /// `return Err`，其他窗口就收不到状态变化。
    async fn dispatch(
        &self,
        context: &Context,
        action: &str,
        payload: &Value,
        cancel: CancellationToken,
    ) -> Result<()> {
        match action {
            "start" => {
                // 只拒绝真正空的输入：Linux/macOS 上目录名可以合法地以空格开头或结尾，
                // 文件选择器返回的原路径必须逐字使用，trim 会把用户选的目录悄悄换成另一个。
                let path = payload
                    .get("root_path")
                    .and_then(Value::as_str)
                    .filter(|path| !path.is_empty())
                    .ok_or_else(|| Error::new(-32602, "missing root"))?;
                // 宿主只在 Enter 时提交输入框；用户改完监听地址直接点「启动」时，
                // 新地址只存在于本次动作数据里。因此这里必须把快照中的地址交给
                // 控制器，在同一段生命周期操作里校验并采用，而不是依赖此前是否
                // 发过 `set-address`——否则会按旧地址绑定，界面与真实监听不一致。
                let address = payload
                    .get("selected_address")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                self.controller
                    .start_with_address(path.into(), address)
                    .await
                    .map_err(map_error)?;
            }
            "stop" => {
                self.controller.stop().await.map_err(map_error)?;
            }
            // 显式提交监听地址：宿主把文本写进视图草稿后，只有 Enter 会触发
            // `on_submit`，失焦不会；因此这条动作只覆盖「用户按 Enter」的情形，
            // 其余情形由 `start` 直接读取动作快照里的地址兜底。
            "set-address" => {
                let value = payload
                    .get("selected_address")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::new(-32602, "missing address"))?;
                self.controller
                    .state
                    .select_address(value)
                    .map_err(map_error)?;
            }
            "copy-link" => {
                let snapshot = self.snapshot()?;
                let link = snapshot
                    .link
                    .ok_or_else(|| Error::new(-32001, "not running"))?;
                // 只把一次性链接写进剪贴板；失败如实上报，不返回成功让界面误以为已复制。
                context
                    .request("clipboard.write", json!({ "text": link }), cancel.clone())
                    .await?;
                // 回执由外层统一构造为当前快照补丁；这里不再单独返回只含 link 的补丁，
                // 避免两条返回路径让「失败也广播」的逻辑分叉。
            }
            other => return Err(Error::new(-32602, format!("unknown action {other}"))),
        }
        Ok(())
    }

    /// `view.snapshot`：只返回可恢复的非敏感字段与状态版本。
    pub fn snapshot_state(&self) -> Result<Value> {
        let snapshot = self.snapshot()?;
        Ok(json!({
            "state_version": VIEW_STATE_VERSION,
            "state": {
                "selected_address": snapshot.selected_address,
                "root_path": snapshot.root_path,
            }
        }))
    }

    /// `view.close`：只注销实例，不停止服务（关闭视图不等于停止共享）。
    pub async fn close(&self, params: Value) -> Result<Value> {
        let instance = instance_id(&params)?;
        let mut instances = self.instances.lock().await;
        instances.retain(|existing| existing != &instance);
        self.snapshot_state()
    }

    /// 服务状态变化后向其他打开实例推送补丁；失败只记录，不影响主流程。
    ///
    /// `skip` 是本次动作的发起实例：宿主已用动作回执更新它，重复推送只会多占一份
    /// 补丁预算，并可能在回执到达前把旧值写回去。
    pub async fn broadcast(&self, context: &Context, skip: Option<&str>, patch: Value) {
        let instances = self.instances.lock().await.clone();
        for instance in instances {
            if skip == Some(instance.as_str()) {
                continue;
            }
            let _ = context
                .notify(
                    "view.update",
                    json!({ "instance": instance, "patch": patch }),
                )
                .await;
        }
    }

    fn snapshot(&self) -> Result<Snapshot> {
        self.controller.state.snapshot().map_err(map_error)
    }
}

/// 实例标识由宿主生成；缺失时拒绝，不能用空串代替。
fn instance_id(params: &Value) -> Result<String> {
    params
        .get("instance")
        .and_then(Value::as_str)
        .filter(|instance| !instance.is_empty() && instance.len() <= 256)
        .map(str::to_owned)
        .ok_or_else(|| Error::new(-32602, "missing instance"))
}

/// 快照到视图数据的映射；字段名即视图绑定名，改名字要同步 `resources/views/panel.json`。
pub fn data(snapshot: &Snapshot) -> Value {
    json!({
        "phase": snapshot.phase,
        "root_path": snapshot.root_path,
        "running": snapshot.phase == crate::state::Phase::Running,
        "starting": snapshot.phase == crate::state::Phase::Starting,
        "stopping": snapshot.phase == crate::state::Phase::Stopping,
        // 错误行只在失败时占位；否则空文本仍会占一行高度，让面板随状态跳动。
        "failed": snapshot.phase == crate::state::Phase::Failed,
        "address": snapshot.address,
        "link": snapshot.link,
        "root_name": snapshot.root_name,
        "qr": snapshot.qr,
        "selected_address": snapshot.selected_address,
        "addresses": snapshot.addresses,
        "addresses_hint": addresses_text(&snapshot.addresses),
        "status": snapshot.status,
        "error": snapshot.error,
        "uploaded": snapshot.uploaded,
        "downloaded": snapshot.downloaded,
        "stats": stats_text(snapshot.uploaded, snapshot.downloaded),
        "expires_at": snapshot.expires_at,
    })
}

/// 可用地址提示：视图没有列表插值能力，只能显示 Sidecar 拼好的整句，
/// 否则用户得靠猜才能填对监听地址。
fn addresses_text(addresses: &[String]) -> String {
    let locale = crate::l10n::Locale::detect();
    let lan = addresses
        .iter()
        .filter(|address| {
            address
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| !ip.is_loopback())
        })
        .cloned()
        .collect::<Vec<_>>();
    if lan.is_empty() {
        return locale
            .text(crate::l10n::Key::AddressHintLoopback)
            .to_owned();
    }
    format!(
        "{}{}",
        locale.text(crate::l10n::Key::AddressHint),
        lan.join(", ")
    )
}

/// 传输计数文案：视图只能绑定字符串，因此由 Sidecar 计算好整句，避免界面自己拼语言。
fn stats_text(uploaded: u64, downloaded: u64) -> String {
    let locale = crate::l10n::Locale::detect();
    if uploaded == 0 && downloaded == 0 {
        return locale.text(crate::l10n::Key::StatsEmpty).to_owned();
    }
    locale
        .text(crate::l10n::Key::Stats)
        .replace("{up}", &uploaded.to_string())
        .replace("{down}", &downloaded.to_string())
}

/// 应用错误转协议错误：只保留稳定码与短句，不把诊断细节送进宿主界面。
fn map_error(error: crate::error::AppError) -> Error {
    let code = match error.code {
        crate::error::ErrorCode::Busy => -32003,
        crate::error::ErrorCode::BadRequest | crate::error::ErrorCode::PathRejected => -32602,
        crate::error::ErrorCode::Unavailable => -32001,
        _ => -32603,
    };
    Error::new(code, error.detail)
}
