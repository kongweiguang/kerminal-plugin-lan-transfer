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
    l10n::{Key, Locale},
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
                    // 统计推送只携带统计字段，不能把整份快照写回去：一次在途的
                    // 统计更新可能基于生命周期动作之前的快照，整份补丁会把刚完成的
                    // 启动/停止或复制结果覆盖回旧值。统计之外的字段由动作回执负责。
                    let patch = stats_patch(&snapshot);
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
        Ok(json!({
            "data": data(&snapshot, self.controller.retains_running_resources())
        }))
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
        // 但动作失败时宿主不会提交任何补丁（协议错误没有 data），因此调用方仍需
        // 单独收到状态补丁，否则它的窗口会停在动作前的旧状态。例如停止失败后状态
        // 已是 `Failed`，调用方却仍显示「运行中」，用户会以为服务还在正常提供。
        let caller = instance_id(&params).ok();
        if cancel.is_cancelled() {
            return Err(Error::new(-32002, "cancelled"));
        }
        // 复制等待宿主确认时可能遇到另一个窗口停止或重启共享。独立分派可以
        // 避免迟到的全量回执覆盖其他窗口已经提交的生命周期状态。
        if action == "copy-link" {
            return self.copy_link(context, caller.as_deref(), cancel).await;
        }
        // 动作前的投影用于判断失败是否真的改变了可见状态；只有变化时才推送给
        // 调用方，避免把「参数无效」这类没有状态变化的结果写成一次假更新。
        let before = data(
            &self.snapshot()?,
            self.controller.retains_running_resources(),
        );
        // 记下最近一次动作的上下文：统计推送任务只在 HTTP 完成时被唤醒，
        // 那时没有活跃请求可用，只能复用这个句柄。
        *self.context.lock().await = Some(context.clone());
        let outcome = self.dispatch(action, &payload).await;
        let snapshot = self.snapshot()?;
        let mut patch = data(&snapshot, self.controller.retains_running_resources());
        let changed = patch != before;
        // 启动/停止会更换或撤销链接，必须让所有窗口重置复制按钮；其他成功动作
        // （例如只改地址或复制本身）不应重置其他窗口的确认，因此广播前去掉
        // 这个只属于单个实例的字段。
        let lifecycle = matches!(action, "start" | "stop");
        let mut broadcast_patch = patch.clone();
        if outcome.is_ok() && !lifecycle {
            omit_copy_label(&mut broadcast_patch);
        }
        // 成功动作继续向其他窗口广播；失败动作只有确实改变状态时才广播，
        // 避免一次纯校验失败把相同快照重复推给所有窗口。
        if outcome.is_ok() || changed {
            self.broadcast(context, caller.as_deref(), broadcast_patch)
                .await;
        }
        match outcome {
            Ok(()) => Ok(json!({ "patch": patch })),
            Err(failure) => {
                // 调用方的动作回执只有错误，没有补丁；把校验或生命周期错误写进
                // 视图数据，面板才能显示本地化的具体原因。根滚动声明的
                // `x-kerminal-action-error` 指向 `$error`，宿主据此只展示一次，
                // 不再叠加通用错误提示；不支持该扩展的宿主仍按原协议错误回退。
                let has_inline = failure.inline.is_some();
                if let Some(message) = failure.inline
                    && let Some(object) = patch.as_object_mut()
                {
                    object.insert("error".into(), Value::String(message));
                    object.insert("show_error".into(), Value::Bool(true));
                }
                // 状态变化或带内联文案时都要单独推给调用方：校验失败通常不改
                // 阶段，但仍然需要清空旧提示并显示新原因。
                if (changed || has_inline)
                    && let Some(caller) = caller.as_deref()
                {
                    let _ = context
                        .notify("view.update", json!({ "instance": caller, "patch": patch }))
                        .await;
                }
                Err(failure.error)
            }
        }
    }

    /// 复制链接走独立动作路径：只确认调用窗口的按钮，绝不广播全量快照。
    ///
    /// ACK 是宿主能力结果，只证明文本已经交给系统剪贴板；在等待期间另一窗口可以
    /// 停止或重启服务，所以必须在 ACK 后重新取快照，并同时比较链接与代次标识，
    /// 才能确认剪贴板里仍是当前链接。业务失败只在调用窗口写局部错误；宿主错误
    /// 保持协议码与原文，既不伪装成业务错误，也不写进插件的内联错误行。
    async fn copy_link(
        &self,
        context: &Context,
        caller: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let before = self.snapshot()?;
        let failure_key = if let Ok(link) = copyable_link(&before) {
            context
                .request("clipboard.write", json!({ "text": link }), cancel.clone())
                .await?;
            if cancel.is_cancelled() {
                return Err(Error::new(-32002, "cancelled"));
            }
            let after = self.snapshot()?;
            if copyable_link(&after).is_ok_and(|current| current == link)
                && before.started_at == after.started_at
                && before.expires_at == after.expires_at
            {
                return Ok(
                    json!({ "patch": { "copy_label": Locale::detect().text(Key::CopyDone) } }),
                );
            }
            Key::ErrorCopyLinkChanged
        } else {
            Key::ErrorNotRunning
        };

        let failure = ActionFailure::business(ERROR_NOT_RUNNING, failure_key);
        // 与协议错误使用同一文案，让支持内联错误的宿主抑制重复提示。
        if let Some(caller) = caller {
            let patch = json!({
                "error": failure.error.message,
                "show_error": true,
                "copy_label": Locale::detect().text(Key::CopyAction),
            });
            let _ = context
                .notify("view.update", json!({ "instance": caller, "patch": patch }))
                .await;
        }
        Err(failure.error)
    }

    /// 执行单个动作；失败时同时保留协议错误与可选的面板内联文案。
    ///
    /// 拆出来是为了让 `action` 统一在「成功或失败」之后广播快照：失败分支若提前
    /// `return Err`，其他窗口就收不到状态变化。协议错误决定宿主回退行为，内联
    /// 文案只用于面板展示，两者不能互相替代。
    async fn dispatch(
        &self,
        action: &str,
        payload: &Value,
    ) -> std::result::Result<(), ActionFailure> {
        match action {
            "start" => {
                // 只拒绝真正空的目录：Linux/macOS 上目录名可以合法地以空格开头或结尾，
                // 文件选择器返回的原路径必须逐字使用，trim 会把用户选的目录悄悄换成另一个。
                // 缺字段/类型错误属于线上形状问题（-32602）；空字符串是用户可修正的
                // 输入内容问题，使用私有输入码并在面板内联说明。
                let path = match payload.get("root_path") {
                    Some(Value::String(path)) if path.is_empty() => {
                        return Err(ActionFailure::validation(
                            crate::l10n::Key::ErrorRootMissing,
                        ));
                    }
                    Some(Value::String(path)) => path.as_str(),
                    Some(_) => {
                        return Err(ActionFailure::protocol(Error::new(
                            -32602,
                            "invalid root_path",
                        )));
                    }
                    None => {
                        return Err(ActionFailure::protocol(Error::new(-32602, "missing root")));
                    }
                };
                // 宿主只在 Enter 时提交输入框；用户改完监听地址直接点「启动」时，
                // 新地址只存在于本次动作数据里。因此这里必须把快照中的地址交给
                // 控制器，在同一段生命周期操作里校验并采用，而不是依赖此前是否
                // 发过 `set-address`——否则会按旧地址绑定，界面与真实监听不一致。
                // 字段缺失时沿用已选地址；字段存在但为空或格式错误则明确拒绝，
                // 避免输入框显示空白却悄悄用旧地址启动。
                let address = match payload.get("selected_address") {
                    Some(Value::String(value)) => {
                        let value = value.trim();
                        if value.is_empty() {
                            return Err(ActionFailure::validation(
                                crate::l10n::Key::ErrorAddressInvalid,
                            ));
                        }
                        Some(value)
                    }
                    Some(_) => {
                        return Err(ActionFailure::protocol(Error::new(
                            -32602,
                            "invalid selected_address",
                        )));
                    }
                    None => None,
                };
                self.controller
                    .start_with_address(path.into(), address)
                    .await
                    .map_err(ActionFailure::app)?;
            }
            "stop" => {
                self.controller.stop().await.map_err(ActionFailure::app)?;
            }
            // 显式提交监听地址：宿主把文本写进视图草稿后，只有 Enter 会触发
            // `on_submit`，失焦不会；因此这条动作只覆盖「用户按 Enter」的情形，
            // 其余情形由 `start` 直接读取动作快照里的地址兜底。
            "set-address" => {
                let Some(Value::String(value)) = payload.get("selected_address") else {
                    return Err(ActionFailure::protocol(Error::new(
                        -32602,
                        "missing address",
                    )));
                };
                self.controller
                    .state
                    .select_address(value)
                    .map_err(ActionFailure::app)?;
            }
            // `copy-link` 由 `action` 在广播/快照流程之前单独分派，不能在这里补回
            // 通用分派分支：否则会让宿主剪贴板请求重新进入全量快照路径。
            other => {
                return Err(ActionFailure::protocol(Error::new(
                    -32602,
                    format!("unknown action {other}"),
                )));
            }
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

/// 动作失败的两条通道：协议错误决定宿主回退与调用方语义，内联文案只用于面板。
///
/// 宿主的 `view.action` 错误没有补丁，若只返回协议错误，面板只能显示通用提示。
/// 用户可见失败同时投影到视图数据，并让业务错误消息与 `$error` 逐字一致：宿主
/// 只在两者相等时抑制通用提示，因此不能一个用英文诊断、另一个用本地化文案。
struct ActionFailure {
    error: Error,
    inline: Option<String>,
}

impl ActionFailure {
    /// 宿主能力或协议层错误：保留原始码与消息，不伪造面板文案。
    fn protocol(error: Error) -> Self {
        Self {
            error,
            inline: None,
        }
    }

    /// 插件业务错误：协议码走稳定映射，协议消息与面板文案使用同一本地化文本。
    fn app(error: crate::error::AppError) -> Self {
        let message = crate::l10n::Locale::detect()
            .text(crate::service::error_key(&error))
            .to_owned();
        Self {
            error: Error::new(error_code(&error), message.clone()),
            inline: Some(message),
        }
    }

    /// 私有业务错误：指定稳定码，协议消息与内联文案使用同一本地化文本。
    fn business(code: i32, key: crate::l10n::Key) -> Self {
        let message = crate::l10n::Locale::detect().text(key).to_owned();
        Self {
            error: Error::new(code, message.clone()),
            inline: Some(message),
        }
    }

    /// 输入内容校验失败：使用私有输入码，协议消息与内联文案完全相同。
    ///
    /// 只有字段类型正确但内容不可用（空目录、无效/非本机地址）才走这里；
    /// 缺失字段或错误类型属于线上形状问题，仍由 `protocol` 返回 -32602。
    fn validation(key: crate::l10n::Key) -> Self {
        Self::business(ERROR_INPUT, key)
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

// 插件私有错误码必须落在协议保留的业务区间（-32199..=-32100）。宿主把这些码
// 连同插件文案一起归类为业务错误；若借用 -32001/-32003 等保留码，宿主会显示
// 「权限不足」「请求过多」等完全无关的提示。
const ERROR_NOT_RUNNING: i32 = -32100;
const ERROR_UNAVAILABLE: i32 = -32101;
const ERROR_BUSY: i32 = -32102;
const ERROR_PATH_REJECTED: i32 = -32103;
/// 输入形状合法但内容不可用（空目录、地址格式错误或不属于本机）。
///
/// 这类错误需要面板内联说明；宿主把业务错误文案与 `x-kerminal-action-error`
/// 指向的 `$error` 比对后抑制重复提示，因此协议消息必须与视图文案逐字一致。
const ERROR_INPUT: i32 = -32104;
/// 其他用户可见的生命周期/内部失败；同样用业务码承载本地化文案。
const ERROR_INTERNAL: i32 = -32105;

/// 取当前可复制的链接；只有 `Running` 阶段允许。
///
/// 停止流程先进入 `Stopping`，等待监听任务退出期间快照仍保留旧链接；若只看
/// `link.is_some()`，用户会在这个窗口复制到一个马上失效的地址。复制以真实
/// 运行阶段为准，`link` 只作为阶段正确时的内容来源。
fn copyable_link(snapshot: &Snapshot) -> Result<String> {
    if snapshot.phase != crate::state::Phase::Running {
        return Err(Error::new(ERROR_NOT_RUNNING, "not running"));
    }
    snapshot
        .link
        .clone()
        .ok_or_else(|| Error::new(ERROR_NOT_RUNNING, "not running"))
}

/// 去掉只属于单个实例的复制按钮文案。
///
/// 非生命周期的跨窗口广播不能重置各窗口自己的「已复制」；统计推送更是只发
/// `stats`，不会携带这个字段。只有启动、停止或失败状态更新才应恢复默认文案。
fn omit_copy_label(patch: &mut Value) {
    if let Some(object) = patch.as_object_mut() {
        object.remove("copy_label");
    }
}

/// 统计推送的专用补丁：只含统计文案，不重放快照中的生命周期字段。
///
/// 统计任务可能在某个动作之前读到旧快照；若发送整份数据，迟到的统计补丁会把
/// 刚完成的启动/停止、错误或复制结果覆盖回旧值。统计之外的字段只由动作回执
/// 和状态变化推送负责。
fn stats_patch(snapshot: &Snapshot) -> Value {
    json!({ "stats": stats_text(snapshot.uploaded, snapshot.downloaded) })
}

/// 快照到视图数据的映射；字段名即视图绑定名，改名字要同步 `resources/views/panel.json`。
///
/// `retains_running_resources` 由控制器提供：`Failed`/`Idle` 只有在仍持有运行或
/// 遗留句柄时才显示停止入口，否则目录缺失这类启动失败会多出一个无意义的 Stop。
pub fn data(snapshot: &Snapshot, retains_running_resources: bool) -> Value {
    // 编辑区只在空闲或失败时出现：启动、运行、停止期间改写目录或地址会与真实
    // 监听不一致；失败态保留编辑入口，用户才能修正上一次的输入。
    let editable = matches!(
        snapshot.phase,
        crate::state::Phase::Idle | crate::state::Phase::Failed
    );
    // 启动中继续显示正在启动的 Start，而不是让 Stop 抢位；停止中只显示正在
    // 停止的 Stop。运行中始终可停止；空闲/失败态只有在控制器仍保留运行或
    // 遗留句柄时才给停止入口，以保留回收路径而避免普通启动失败多出 Stop。
    let can_start = matches!(
        snapshot.phase,
        crate::state::Phase::Idle | crate::state::Phase::Starting | crate::state::Phase::Failed
    );
    let can_stop = matches!(
        snapshot.phase,
        crate::state::Phase::Running | crate::state::Phase::Stopping
    ) || (matches!(
        snapshot.phase,
        crate::state::Phase::Idle | crate::state::Phase::Failed
    ) && retains_running_resources);
    // 运行中的范围提示只描述实际可访问范围，不用「扫码」暗示手机一定能打开回环。
    let scope = snapshot
        .address
        .as_deref()
        .and_then(|address| address.parse::<std::net::SocketAddr>().ok())
        .map(|address| {
            if address.ip().is_loopback() {
                crate::l10n::Key::ScopeLocalOnly
            } else {
                crate::l10n::Key::ScopeLan
            }
        })
        .map(|key| crate::l10n::Locale::detect().text(key).to_owned())
        .unwrap_or_default();
    json!({
        "root_path": snapshot.root_path,
        // `root_path` 是宿主可编辑的双向绑定，宿主会保护未提交草稿；运行中的
        // 展示必须读取独立的只读字段，否则另一个窗口里未提交的旧目录会盖住
        // 实际共享的规范路径。`shared_path` 只做展示：去掉 Windows canonicalize
        // 产生的 `\\?\` / `\\?\UNC\` 前缀，原始路径继续用于生命周期与持久化。
        "shared_path": snapshot.root_path.as_deref().map(display_shared_path),
        "running": snapshot.phase == crate::state::Phase::Running,
        "starting": snapshot.phase == crate::state::Phase::Starting,
        "stopping": snapshot.phase == crate::state::Phase::Stopping,
        // 错误行只在失败时占位；否则空文本仍会占一行高度，让面板随状态跳动。
        "failed": snapshot.phase == crate::state::Phase::Failed,
        "editable": editable,
        // 启动在主按钮上；停止是否出现由控制器是否仍有可回收资源决定。
        "can_start": can_start,
        "can_stop": can_stop,
        "address": snapshot.address,
        // 绑定回环时只说明「仅本机」，不暗示其他设备可以打开。
        "scope": scope,
        // `link` 含一次性配对密钥，仅供剪贴板动作和集成测试读取；面板不把它
        // 当作普通文本展示，避免密钥被截图或旁人看到。
        "link": snapshot.link,
        "root_name": snapshot.root_name,
        "qr": snapshot.qr,
        "selected_address": snapshot.selected_address,
        "addresses_hint": addresses_text(&snapshot.addresses),
        "status": snapshot.status,
        "error": snapshot.error,
        // 错误行默认只在失败态出现；动作校验失败不会改变阶段，但会把
        // `show_error` 与 `error` 一起推进调用方补丁，让同一行显示具体原因。
        // 这样 Running/Stopping 不会因过时输入被改写成 Failed。
        "show_error": snapshot.phase == crate::state::Phase::Failed,
        "stats": stats_text(snapshot.uploaded, snapshot.downloaded),
        "expires_at": snapshot.expires_at,
        // 复制按钮文案是每个窗口自己的视图状态：复制成功后才切换成「已复制」，
        // 不使用额外提示行，也不会把二维码向下挤动。
        "copy_label": crate::l10n::Locale::detect()
            .text(crate::l10n::Key::CopyAction)
            .to_owned(),
    })
}

/// 处理 Windows canonicalize 产生的 verbatim 前缀，仅供界面展示。
///
/// 仅去掉盘符长路径与 UNC 长路径的 verbatim 前缀，其余命名空间原样保留。
///
/// `\\?\C:\dir` 与 `\\?\UNC\server\share` 是内核长路径形式，直接显示既难读又
/// 会让用户误以为需要手动输入；但 `\\?\Volume{GUID}\`、`\\?\GLOBALROOT\` 等
/// 设备命名空间去掉前缀后会变成看似相对路径的误导文本，因此通用剥离只接受
/// 「ASCII 盘符 + `:\`」这一确定形态。原始 `root_path` 仍保留在状态与持久化中，
/// 不能把展示用字符串写回生命周期字段。
fn display_shared_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        if bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'\\'
        {
            return rest.to_owned();
        }
    }
    path.to_owned()
}

/// 可用地址提示：视图没有列表插值能力，只能显示 Sidecar 拼好的整句，
/// 否则用户得靠猜才能填对监听地址。
///
/// 即使已经列出局域网地址也保留回环限制：回环仍是可选监听地址，用户可能主动
/// 填入并误以为其他设备能打开。回环说明另起一行，避免和地址列表挤成带分号的
/// 一整段，长列表换行时也不会出现行首标点。
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
        "{}{}\n{}",
        locale.text(crate::l10n::Key::AddressHint),
        lan.join(", "),
        locale.text(crate::l10n::Key::AddressHintLocalOnly)
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
///
/// 插件自身状态必须使用业务码：宿主预留的 -32001/-32003 分别表示权限不足与
/// 过载，用它们表达「未运行」「忙」会让用户看到完全错误的原因。输入校验与
/// 其他用户可见失败也落在业务区间，宿主才能把协议文案与内联文案做精确比对。
fn map_error(error: crate::error::AppError) -> Error {
    Error::new(error_code(&error), error.detail)
}

/// 应用错误到稳定业务码；文案由调用方决定是英文诊断还是本地化界面文案。
fn error_code(error: &crate::error::AppError) -> i32 {
    match error.code {
        crate::error::ErrorCode::Busy => ERROR_BUSY,
        // 地址格式/归属校验属于输入内容问题，不是 JSON 参数形状错误；用私有
        // 输入码让宿主按业务错误展示，而不是提示「参数无效」。
        crate::error::ErrorCode::BadRequest => ERROR_INPUT,
        // 目录是用户输入里合法但不可共享的目标，不属于协议参数形状错误；
        // 用业务码分类，宿主才不会把它归成「参数无效」的诊断。
        crate::error::ErrorCode::PathRejected => ERROR_PATH_REJECTED,
        crate::error::ErrorCode::Unavailable => ERROR_UNAVAILABLE,
        _ => ERROR_INTERNAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{AppError, ErrorCode};

    /// 复制必须以真实运行阶段为准：`Stopping` 期间快照仍可能保留旧链接，
    /// 但监听任务正在退出，交给用户的地址随时失效。构造最小快照固定这条边界。
    #[test]
    fn copy_link_requires_running_phase() {
        let link = "http://127.0.0.1:43210/#secret";
        assert_eq!(
            copyable_link(&test_snapshot(crate::state::Phase::Running, Some(link))).unwrap(),
            link
        );
        assert_eq!(
            copyable_link(&test_snapshot(crate::state::Phase::Stopping, Some(link)))
                .unwrap_err()
                .code,
            ERROR_NOT_RUNNING
        );
        assert_eq!(
            copyable_link(&test_snapshot(crate::state::Phase::Running, None))
                .unwrap_err()
                .code,
            ERROR_NOT_RUNNING
        );
    }

    /// 只填复制判定需要的字段；其余用空值，避免测试依赖真实网卡或文件系统。
    fn test_snapshot(phase: crate::state::Phase, link: Option<&str>) -> Snapshot {
        Snapshot {
            phase,
            root_path: None,
            address: None,
            link: link.map(str::to_owned),
            root_name: None,
            qr: None,
            selected_address: String::new(),
            addresses: Vec::new(),
            status: String::new(),
            error: None,
            uploaded: 0,
            downloaded: 0,
            started_at: None,
            expires_at: None,
        }
    }

    /// 只填统计推送需要的字段；阶段固定运行中，避免依赖真实网卡或文件系统。
    fn test_snapshot_with_counts(uploaded: u64, downloaded: u64) -> Snapshot {
        let mut snapshot = test_snapshot(crate::state::Phase::Running, None);
        snapshot.uploaded = uploaded;
        snapshot.downloaded = downloaded;
        snapshot
    }

    /// 复制按钮文案属于单个窗口：非生命周期广播必须只去掉这一个键，
    /// 否则后台一次上传就会把用户刚看到的「已复制」清掉；其他字段仍保留。
    #[test]
    fn copy_label_is_omitted_from_shared_updates() {
        let mut patch = json!({
            "copy_label": "已复制",
            "stats": "上传 1 个文件",
        });
        omit_copy_label(&mut patch);
        assert!(patch.get("copy_label").is_none());
        assert_eq!(patch["stats"], "上传 1 个文件");

        // 非对象补丁不应 panic：协议层防御性处理，保持原值。
        let mut scalar = json!(null);
        omit_copy_label(&mut scalar);
        assert!(scalar.is_null());
    }

    /// 统计推送必须是纯统计补丁：在途的旧快照不能借整份数据覆盖生命周期
    /// 或复制结果，否则用户会看到刚启动的窗口又跳回未启动。
    #[test]
    fn stats_patch_contains_only_stats() {
        let patch = stats_patch(&test_snapshot_with_counts(3, 5));
        let object = patch.as_object().unwrap();
        assert_eq!(object.keys().collect::<Vec<_>>(), vec!["stats"]);
        let stats = patch["stats"].as_str().unwrap();
        assert!(stats.contains('3') && stats.contains('5'), "stats: {stats}");
    }

    /// 输入类错误必须映射到具体文案键：格式错误与非本机地址的修正方式不同。
    #[test]
    fn validation_errors_have_specific_localized_keys() {
        use crate::l10n::Key;
        assert_eq!(
            crate::service::error_key(&AppError::new(ErrorCode::BadRequest, "invalid address")),
            Key::ErrorAddressInvalid
        );
        assert_eq!(
            crate::service::error_key(&AppError::new(
                ErrorCode::BadRequest,
                "address not on this host"
            )),
            Key::ErrorAddressNotLocal
        );
        assert_eq!(
            crate::service::error_key(&AppError::new(ErrorCode::Busy, "service busy")),
            Key::ErrorBusy
        );
        assert_eq!(
            crate::service::error_key(&AppError::new(ErrorCode::Server, "stop timed out")),
            Key::ErrorStopTimeout
        );

        let failure = ActionFailure::validation(Key::ErrorRootMissing);
        assert_eq!(failure.error.code, ERROR_INPUT);
        let inline = failure.inline.expect("inline validation text");
        assert!(!inline.is_empty());
        // 宿主按「业务错误消息 == 内联文案」抑制重复提示，两者必须逐字一致。
        assert_eq!(failure.error.message, inline);
    }

    /// 插件私有码必须避开宿主保留分类：`-32001` 在宿主里是权限不足，
    /// 用它表达「未运行」会把普通状态误导成权限故障。
    #[test]
    fn business_codes_avoid_reserved_classifications() {
        let reserved = [
            -32700, -32600, -32601, -32602, -32603, -32001, -32002, -32003, -32004, -32005,
        ];
        for code in [
            ERROR_NOT_RUNNING,
            ERROR_UNAVAILABLE,
            ERROR_BUSY,
            ERROR_PATH_REJECTED,
            ERROR_INPUT,
            ERROR_INTERNAL,
        ] {
            assert!((-32199..=-32100).contains(&code), "code {code}");
            assert!(!reserved.contains(&code), "code {code}");
        }
        assert_eq!(
            map_error(AppError::new(ErrorCode::Unavailable, "bind failed")).code,
            ERROR_UNAVAILABLE
        );
        assert_eq!(
            map_error(AppError::new(ErrorCode::Busy, "busy")).code,
            ERROR_BUSY
        );
        assert_eq!(
            map_error(AppError::new(ErrorCode::PathRejected, "root missing")).code,
            ERROR_PATH_REJECTED
        );
        assert_eq!(
            map_error(AppError::new(ErrorCode::BadRequest, "invalid address")).code,
            ERROR_INPUT
        );
        assert_eq!(
            map_error(AppError::new(ErrorCode::Server, "stop timed out")).code,
            ERROR_INTERNAL
        );

        // 业务错误消息必须与内联文案一致，宿主才能据此抑制通用错误提示。
        for error in [
            AppError::new(ErrorCode::Unavailable, "bind failed"),
            AppError::new(ErrorCode::BadRequest, "invalid address"),
            AppError::new(ErrorCode::Server, "stop timed out"),
        ] {
            let failure = ActionFailure::app(error);
            let inline = failure.inline.expect("localized inline text");
            assert_eq!(failure.error.message, inline);
        }
    }
}
