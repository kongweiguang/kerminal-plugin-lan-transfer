//! 服务生命周期：共享根校验、绑定、配对链接、停止与回收。
//! @author kongweiguang
//!
//! 停止与宿主关闭使用同一套绝对截止时间：先撤销会话与票据、再关闭监听、最后等待任务
//! 回收。超时如实返回错误，不把「已发出关闭信号」说成「已回收」。
//!
//! 回收期间运行句柄始终留在控制器的 `running` 槽里，等待通过借用句柄完成：等待点是
//! 取消点，若把 `Running` 移出槽位再等待，future 被 drop 时 `JoinHandle` 只会 detach，
//! 任务从此无人可确认，下一次停止就会在「没有句柄」时伪报成功。
//!
//! 取消路径无法 `await`，因此 `DetachedService::drop` 把已中止的句柄推入同步遗留
//! 列表，而不是尝试借用异步槽位：`try_lock` 失败就等于丢句柄，而丢句柄只会 detach。
//! 遗留列表在后续 `stop`/`start` 中于同一绝对截止时间内逐一确认，确认后才释放。
//!
//! 并发模型：`start` 与 `stop` 共用一把 `lifecycle` 异步锁，覆盖「改状态 → 绑定/回收」
//! 的整段过程。只锁状态字段不够——`start` 在 `begin_start` 之后还要 await 绑定端口，
//! 若此时允许 `stop` 进入，它会看到 `running == None` 而直接报告停止成功，随后
//! `start_inner` 仍会登记句柄并把状态改成运行中，表现为「停止成功后服务又出现」。
//!
//! Windows 上没有目录 fsync，这里不承诺崩溃后的目录项持久性。

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use tokio::{
    sync::{Mutex, oneshot, watch},
    task::JoinHandle,
    time::Instant,
};

use crate::{
    config::{SHUTDOWN_TIMEOUT_SECS, STOP_TIMEOUT_SECS},
    error::{AppError, ErrorCode},
    fs_root::ShareRoot,
    http::{AppContext, serve},
    session::Sessions,
    state::State,
    ticket::Tickets,
};

/// 一次运行的服务句柄；`None` 表示当前未运行。
///
/// 句柄在确认任务退出之前始终留在控制器的 `running` 槽里，而不是移入某个调用栈
/// 局部变量：`JoinHandle` 被 drop 只会 detach，任务继续跑而控制器再也找不到它，
/// 下一次 `stop` 就会在「没有句柄」时伪报成功。
pub struct Running {
    shutdown: Option<oneshot::Sender<()>>,
    /// 监听任务必须带返回值：只有任务自己报告正常结束才算「优雅停止」，
    /// 任务内错误（监听失败）与 `JoinError`（panic/取消）要分别如实上报。
    task: JoinHandle<Result<(), AppError>>,
    sessions: Sessions,
    tickets: Tickets,
    /// 本代取消信号与在途请求计数。停止流程先取消它再等任务退出；
    /// 「在途请求已撤销」只能由 `wait_idle` 在预算内确认，外层任务的
    /// `JoinHandle` 结束不等于内部 spawn 的连接任务已结束。
    run: crate::http::RunHandle,
    /// 本控制器是否已对该任务发出过 `abort`。停止超时后中止、下次再确认时
    /// `JoinError` 必然是「被取消」；记下这个标记才能把它与真正的 panic 区分开，
    /// 既不会把强制停止说成优雅停止，也不会把预期的取消误报成新故障。
    aborted: bool,
}

/// 一次回收等待的结果。
///
/// 把「确认已退出」与「窗口内没退出」分成不同类型，是因为后者必须保留句柄：
/// 只要句柄还在控制器的槽里，下一次停止或启动就能继续确认；一旦把句柄移出槽位
/// 再等待，等待点被取消时 `JoinHandle` 的 Drop 会 detach 任务，控制器再也找不到它，
/// 下一次停止就会在「没有句柄」时伪报成功。
enum Reclaim {
    /// 任务已退出且自身报告正常结束。
    Reclaimed,
    /// 任务已退出但自身报告监听错误：资源随任务释放，服务属于异常结束。
    Failed(AppError),
    /// 任务以 panic 或非本次中止导致的取消结束。
    Abnormally(String),
    /// 任务是被本控制器 `abort` 掉的：已确认退出，但不是优雅停止。
    Aborted,
    /// 截止时间内没退出时，句柄必须留在槽里，由后续流程继续确认。
    Pending,
}

impl Reclaim {
    /// 是否已确认任务退出（无论优雅、异常还是被中止）。
    ///
    /// 只有确认退出才允许清空控制器的槽位：清空等于宣称「控制器不再需要这个任务」，
    /// 而未确认时任务可能仍在监听，清空后就再也找不到它。
    fn confirmed(&self) -> bool {
        !matches!(self, Self::Pending)
    }
}

impl Running {
    /// 请求监听任务关闭；只发一次。
    ///
    /// `shutdown` 取走后置空，重复的停止/启动流程不会对同一通道反复发送；
    /// 已经发过关闭信号的运行实例只需要继续等待任务退出。
    fn request_shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    /// 在绝对截止时间内等待任务退出，全程只借用句柄。
    ///
    /// 借用 `&mut self` 而不是把 `JoinHandle` 取出来等待：等待是取消点，
    /// 若调用方的 future 在此被 drop（SDK 取消 `view.action`），句柄仍在控制器的
    /// 槽里，后续流程可以继续确认回收。由本控制器发出过 `abort` 的任务会以
    /// 「取消」结束，这属于强制停止，单独标记出来交给调用方如实报告，不当成 panic。
    async fn reclaim(&mut self, deadline: Instant) -> Reclaim {
        match tokio::time::timeout_at(deadline, &mut self.task).await {
            Ok(Ok(Ok(()))) => Reclaim::Reclaimed,
            Ok(Ok(Err(error))) => Reclaim::Failed(error),
            Ok(Err(error)) if error.is_cancelled() && self.aborted => Reclaim::Aborted,
            Ok(Err(error)) => Reclaim::Abnormally(error.to_string()),
            Err(_) => Reclaim::Pending,
        }
    }

    /// 撤销本代：先取消代次信号（处理器与响应体流在下一个检查点退出），
    /// 再撤销令牌并请求监听关闭。顺序不能反：关闭信号只终止 accept 循环，
    /// 已建立连接的处理器只认代次取消信号；先关监听再取消，中间窗口里
    /// 在途请求既不退出也不被计数取消，等待时间会被无谓拉长。
    fn revoke(&mut self) {
        self.run.cancel();
        self.sessions.revoke_all();
        self.tickets.clear();
        self.request_shutdown();
    }

    /// 确认在途请求已在截止时间内全部结束；未确认返回 `false`，由调用方决定
    /// 中止任务并保留句柄。这是「已撤销在途请求」的唯一证据来源：优雅关闭只
    /// 终止 accept 循环，不等待已 spawn 的连接任务，不能替代这里的确认。
    async fn wait_inflight_idle(&self, deadline: Instant) -> bool {
        self.run.wait_idle(deadline).await
    }
}

/// 已启动但尚未登记的监听任务。
///
/// 存在的理由：`serve` 一旦返回，监听任务就已经 `spawn` 到运行时；若此后 `start`
/// 的 future 被取消（SDK 取消 `view.action`）或状态写入失败，句柄还没进入
/// `self.running`，普通返回路径不会执行，任务会永久脱离管理并继续对外服务。
/// 守卫在 drop 时撤销会话、请求关闭并中止任务；`into_running` 是唯一解除武装的出口。
///
/// 取消路径不能 `await`，因此不能把句柄登记进异步槽位再确认。这里改为把已中止的
/// 句柄放进**独立的中止遗留列表**：`std::sync::Mutex` 的锁只覆盖一次 push，
/// 不会被其它异步流程长期持有，drop 里必定取得；后续 `stop`/`start` 取走列表、
/// 在共享截止时间内等待并分类。这样无论锁不变量是否成立，`JoinHandle` 都不会
/// 被静默丢弃（丢弃只会 detach，任务继续跑而控制器再也找不到它）。
struct DetachedService {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), AppError>>>,
    sessions: Sessions,
    tickets: Tickets,
    /// 本代取消信号；取消路径无法 `await`，drop 里只能同步调用 `cancel`。
    run: crate::http::RunHandle,
    /// 取消时若状态已经写成 `Running`（取消发生在 `mark_running` 与登记句柄之间），
    /// 必须把状态改成可见失败，否则界面会一直显示「运行中」而句柄从未存在。
    state: Arc<State>,
    /// 控制器的中止遗留列表；drop 把已中止的句柄放进去供后续流程确认。
    orphaned: Arc<std::sync::Mutex<Vec<Running>>>,
    armed: bool,
}

impl DetachedService {
    fn new(
        shutdown: oneshot::Sender<()>,
        task: JoinHandle<Result<(), AppError>>,
        sessions: Sessions,
        tickets: Tickets,
        run: crate::http::RunHandle,
        state: Arc<State>,
        orphaned: Arc<std::sync::Mutex<Vec<Running>>>,
    ) -> Self {
        Self {
            shutdown: Some(shutdown),
            task: Some(task),
            sessions,
            tickets,
            run,
            state,
            orphaned,
            armed: true,
        }
    }

    /// 把监听任务移交为 `Running`，但守卫保持武装：调用方必须在同一段不含 await
    /// 的临界区内把 `Running` 写进控制器槽位，再调用 `disarm`。
    ///
    /// 这样「句柄已离开守卫、尚未登记」之间没有取消点：若在等槽位锁时被取消，
    /// 守卫仍持有任务，drop 会撤销会话并中止它；若守卫在这里就解除武装，
    /// 取消会留下一个既不在槽里、也无人中止的监听任务。
    fn into_running(&mut self) -> Option<Running> {
        let task = self.task.take()?;
        Some(Running {
            shutdown: self.shutdown.take(),
            task,
            sessions: self.sessions.clone(),
            tickets: self.tickets.clone(),
            run: self.run.clone(),
            aborted: false,
        })
    }

    /// 句柄已经登记进控制器，之后由停止流程负责回收。
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DetachedService {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // 取消路径无法 await：先取消代次信号（在途请求在下一个检查点退出），
        // 让旧令牌立刻失效，再请求关闭并中止任务。
        // 中止由运行时保证最终被 drop，监听端口随之释放；这里不声称已确认回收，
        // 调用方会因取消而收到错误，不会看到「启动成功」。
        self.run.cancel();
        self.sessions.revoke_all();
        self.tickets.clear();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
            // 已中止的句柄必须保留，交给后续流程确认退出。这里不尝试借用异步槽位：
            // drop 里不能 await，`try_lock` 失败就等于丢句柄，而丢句柄只会 detach，
            // 任务继续跑却再也无人能确认——下一次停止会在「没有句柄」时伪报成功。
            // 改为推入独立的同步遗留列表：push 不等待任何异步锁，必然成功；
            // 列表由 `stop`/`start` 在共享截止时间内取出并逐一分类，确认后才释放。
            // 已完成的任务不再入列：它的输出已经确定，没有可确认的资源。
            if !task.is_finished()
                && let Ok(mut orphaned) = self.orphaned.lock()
            {
                orphaned.push(Running {
                    shutdown: None,
                    task,
                    sessions: self.sessions.clone(),
                    tickets: self.tickets.clone(),
                    run: self.run.clone(),
                    aborted: true,
                });
            }
        }
        // 取消发生在 `mark_running` 之后时，状态已经是 `Running` 但没有句柄；
        // 这里把它落成可见失败，避免界面永久显示「运行中」。
        let message = crate::l10n::Locale::detect()
            .text(crate::l10n::Key::ErrorInternal)
            .to_owned();
        let _ = self.state.mark_failed_if_running(message);
    }
}

/// 启动阶段的取消守卫：future 被 drop 时把仍为 `Starting` 的状态收回空闲。
struct StartGuard {
    state: Arc<State>,
}

impl StartGuard {
    fn new(state: Arc<State>) -> Self {
        Self { state }
    }
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        self.state.mark_idle_if_starting();
    }
}

/// 停止阶段的取消守卫：future 被 drop 时把仍为 `Stopping` 的状态落成可见失败。
///
/// 停止流程会先取走运行句柄并撤销会话，若此时被取消，监听任务已经脱离控制器，
/// 面板却会永远停在「正在停止…」，且后续 `stop` 因 `Stopping` 被当作忙而无法重试。
/// 落成 `Failed` 至少让用户看到失败并能重新启动或停止。
struct StopGuard {
    state: Arc<State>,
}

impl StopGuard {
    fn new(state: Arc<State>) -> Self {
        Self { state }
    }
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        let message = crate::l10n::Locale::detect()
            .text(crate::l10n::Key::ErrorInternal)
            .to_owned();
        self.state.mark_failed_if_stopping(message);
    }
}

/// Sidecar 进程级控制器；视图方法都通过它读写状态，避免界面成为唯一状态源。
pub struct Controller {
    pub state: Arc<State>,
    /// 当前运行句柄；确认退出前不移出槽位（见模块注释）。
    running: Arc<Mutex<Option<Running>>>,
    /// 取消路径留下的已中止句柄。`std::sync::Mutex` 只保护一次 push/取出，
    /// 不参与异步等待，因此 `DetachedService::drop` 里必定能登记成功；
    /// 后续 `stop`/`start` 取出后在共享截止时间内确认退出，确认后才释放。
    orphaned: Arc<std::sync::Mutex<Vec<Running>>>,
    generation: Mutex<u64>,
    /// 统计修订号通道；HTTP 完成上传/下载后递增，视图推送任务据此刷新面板。
    /// 由控制器持有而不是每代重建：统计是本次进程的累计值，重启后视图仍应看到
    /// 同一份累计数字；接收端只把修订号当「有变化」的信号，不关心具体值。
    stats: watch::Sender<u64>,
    /// 串行化 `start`/`stop`；见模块注释，状态锁本身不足以防止「停止后又复活」。
    lifecycle: Mutex<()>,
}

impl Controller {
    pub fn new() -> Arc<Self> {
        let (stats, _receiver) = watch::channel(0u64);
        Arc::new(Self {
            state: State::new(),
            running: Arc::new(Mutex::new(None)),
            orphaned: Arc::new(std::sync::Mutex::new(Vec::new())),
            generation: Mutex::new(0),
            stats,
            lifecycle: Mutex::new(()),
        })
    }

    /// 统计修订号通道的克隆；视图推送任务用它订阅变更。
    pub fn stats(&self) -> watch::Sender<u64> {
        self.stats.clone()
    }

    /// 启动服务（沿用当前已选监听地址）。整段过程持有 `lifecycle`，因此并发的
    /// `stop` 要么先完成（此时 `begin_start` 会发现状态不是空闲并拒绝启动），
    /// 要么等启动结束后再正常停止。
    pub async fn start(&self, root_path: PathBuf) -> Result<String, AppError> {
        self.start_with_address(root_path, None).await
    }

    /// 启动服务并可选地采用调用方给出的监听地址。
    ///
    /// 为什么必须由 `start` 自己接收地址：宿主只在 Enter 提交输入框，用户改完地址
    /// 直接点「启动」时，草稿只出现在动作数据里，不会先发 `set-address`。
    /// 若这里忽略它，服务会按旧地址绑定，界面显示与实际监听不一致——用户可能以为
    /// 只共享给了回环，实际却绑到了局域网地址。
    ///
    /// 地址校验和进入启动态在同一状态锁内完成，防止其他面板覆盖本次请求。
    /// 地址无效时保留原状态，避免用户误以为服务正在按这次选择启动。
    pub async fn start_with_address(
        &self,
        root_path: PathBuf,
        address: Option<&str>,
    ) -> Result<String, AppError> {
        let _lifecycle = self.lifecycle.lock().await;
        // 先过状态机再记录目录：此前在 busy 判断之前就写 `root_path`，运行中或
        // 地址无效的启动请求会把面板显示的共享根改成新路径，而实际监听仍绑定
        // 旧目录句柄——用户看到「已共享 B」，文件却来自 A。只有 `begin_start`
        // 真正准入后才记录，失败或忙的请求不再产生可见副作用。
        let ip = self.state.begin_start_with_address(address)?;
        self.state.remember_root(&root_path);
        // 取消守卫：SDK 可以在 `view.action` 中途取消并直接 drop future，此时下面的
        // `Err` 分支不会执行，状态会永久停在「正在启动…」。守卫在 drop 时把仍为
        // `Starting` 的状态收回空闲；正常成功/失败路径不会命中该条件。
        let start_guard = StartGuard::new(Arc::clone(&self.state));
        // `begin_start` 已保证状态机只允许从空闲/失败进入启动中；上一轮遗留的
        // `running` 句柄由 `start_inner` 在打开共享根之后统一回收，这里不重复处理。
        // 启动预算与停止共用同一上限：启动期间的回收旧句柄、绑定端口都在这个
        // 截止时间内完成，超时如实失败，而不是无限期挂着。
        let deadline = Instant::now() + Duration::from_secs(STOP_TIMEOUT_SECS);
        let result = self.start_inner(ip, root_path, deadline).await;
        if let Err(error) = &result {
            let message = crate::l10n::Locale::detect()
                .text(error_key(error))
                .to_owned();
            let _ = self.state.mark_failed(message);
        }
        drop(start_guard);
        result
    }

    /// 启动细节：先打开并校验共享根，再分配代次、生成密钥、绑定端口。
    ///
    /// 顺序固定为「先失败在最便宜的一步」：目录不可用时不会白白生成会话或占用端口。
    /// 二维码在启动监听任务之前生成：二维码失败时监听器随局部变量一起释放，
    /// 不会留下一个已经对外服务、但状态显示为失败的端口。
    ///
    /// `deadline` 是本次启动的绝对预算，只约束回收旧句柄这一步：重新启动常常紧接在
    /// 一次超时停止之后，旧任务可能仍未退出；若这里再等一轮固定窗口，总耗时就会
    /// 叠加到用户可见的「启动」操作上。等待旧任务用与停止相同的截止时间，超时就
    /// 中止旧任务、保留句柄并如实失败，让后续停止或启动继续确认。
    async fn start_inner(
        &self,
        ip: IpAddr,
        root_path: PathBuf,
        deadline: Instant,
    ) -> Result<String, AppError> {
        let root = open_root(&root_path)?;
        // 取消路径留下的已中止句柄必须先确认退出：它们既不在运行槽里，也没有其它
        // 流程会等待。与运行槽一样在共享截止时间内确认，未确认就如实失败，
        // 不允许在旧监听可能仍存活时启动新服务。已确认的句柄随之释放。
        //
        // 确认的是「之前那次取消的强制中止」，不是本次启动失败；启动可以继续，
        // 但不能把这次中止说成优雅回收——它只影响上一轮运行的历史，不影响新会话。
        self.reclaim_orphaned(deadline).await?;
        // 上一次运行可能在运行期失败（状态已落 `Failed`，但句柄仍登记着），也可能
        // 停止超时后把未确认中止的句柄放了回来。`begin_start` 允许从 `Failed` 重新
        // 启动，因此这里必须先回收旧句柄：否则下面登记新句柄会直接覆盖它，旧任务
        // 若仍在监听就会与新服务并存，且再也无法被停止流程找到。
        //
        // 回收期间句柄仍留在槽里，等待通过锁守卫借用：这个等待是取消点，若把
        // `Running` 移出槽位再等待，future 被 drop 时 `JoinHandle` 会被丢弃（只
        // detach），旧任务从此无人可确认。只有确认退出才清空槽位；未确认时返回
        // 错误并保留句柄，让后续停止/启动继续确认。
        {
            let mut slot = self.running.lock().await;
            // 先在同一表达式里借用作等待，再用返回的结果决定是否清空槽位：
            // `if let Some(stale) = slot.as_mut() { ... *slot = None }` 会让
            // `Option<&mut ..>` 的临时借用存活到整个块结束，块内再写槽位无法通过
            // 借用检查；分成「取结果」和「改槽位」两步既清晰又没有该限制。
            let outcome = match slot.as_mut() {
                Some(stale) => {
                    // 先撤销代次（取消信号 → 令牌 → 关闭监听），再中止任务：
                    // 中止前若还有未确认的在途请求，用同一截止时间等待其结束；
                    // 等不到就把任务标记为已中止，句柄留在槽里由后续流程确认。
                    // 这里不能跳过等待直接中止：那会让仍在写盘的上传被硬切，
                    // 磁盘上的结果不可断言。
                    stale.revoke();
                    if !stale.wait_inflight_idle(deadline).await {
                        stale.task.abort();
                        stale.aborted = true;
                    }
                    stale.reclaim(deadline).await
                }
                None => Reclaim::Reclaimed,
            };
            if !outcome.confirmed() {
                return Err(AppError::new(
                    ErrorCode::Busy,
                    "previous service still reclaiming",
                ));
            }
            *slot = None;
        }
        let root_label = root_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("share")
            .to_owned();
        // 每次启动递增代次，旧会话与旧票据随之失效；锁在取到值后立即释放，
        // 避免后面的网络绑定持有锁导致停止请求排队。
        let generation = {
            let mut generation = self.generation.lock().await;
            *generation = generation.wrapping_add(1);
            *generation
        };
        let (sessions, secret) = Sessions::start(generation)?;
        let tickets = Tickets::new(generation);
        let expires_at = sessions.expires_at();
        let (shutdown, receiver) = oneshot::channel();
        // 先绑定系统分配的端口，再把真实端口写进上下文，保证 Host/Origin 校验
        // 使用的端口就是内核实际监听的端口。
        let listener = tokio::net::TcpListener::bind(SocketAddr::new(ip, 0))
            .await
            .map_err(|_| AppError::new(ErrorCode::Unavailable, "bind failed"))?;
        let bound = listener
            .local_addr()
            .map_err(|_| AppError::new(ErrorCode::Server, "local address unavailable"))?;
        // 通配监听（0.0.0.0）不能把绑定地址直接写进链接：浏览器无法打开
        // `http://0.0.0.0:port`。这里在真正启动时重新枚举一次网卡，用同一份样本
        // 挑选具体 IPv4 生成链接与二维码，并冻结成 Host 白名单。面板创建后网络
        // 可能已经变化，沿用创建时的旧列表会生成打不开的链接；白名单随后固定，
        // 运行中换网卡需要重启服务。单地址监听保持原有逐字校验，不因为引入通配
        // 分支而放宽任何具体地址的 Host/Origin 检查。
        let (advertised, hosts) = if ip == IpAddr::V4(Ipv4Addr::UNSPECIFIED) {
            let addresses = crate::net::list_addresses();
            let ipv4 = crate::net::advertised_ipv4(&addresses);
            let allowed = crate::net::ipv4_allowlist(&addresses);
            let advertised = SocketAddr::new(IpAddr::V4(ipv4), bound.port());
            (
                advertised,
                crate::http::HostPolicy::wildcard(bound.port(), allowed),
            )
        } else {
            (bound, crate::http::HostPolicy::single(bound))
        };
        let link = format!("http://{advertised}/#{secret}");
        let qr = render_qr(&link)?;
        // 统计通道由控制器持有，跨代共享：每次上传/下载完成只递增修订号，
        // 视图推送任务（首个 `view.open` 启动）订阅并刷新面板。
        let stats = self.stats();
        let run = crate::http::RunHandle::new(generation);
        let context = Arc::new(AppContext {
            state: Arc::clone(&self.state),
            sessions: sessions.clone(),
            tickets: tickets.clone(),
            root,
            hosts,
            run: run.clone(),
            stats,
            uploads: std::sync::atomic::AtomicUsize::new(0),
            pairing: std::sync::Mutex::new((0, 0)),
        });
        let task = serve(Arc::clone(&context), listener, receiver).await;
        // 从这里开始监听任务已经存在。用守卫接管它：此后任何提前返回或被取消都会
        // 撤销会话并中止任务，不会留下脱离管理的后台端口。
        let mut service = DetachedService::new(
            shutdown,
            task,
            sessions,
            tickets,
            run,
            Arc::clone(&self.state),
            Arc::clone(&self.orphaned),
        );
        // 监听任务可能在我们登记句柄之前就失败（例如端口被并发关闭）。此时不能把
        // 状态覆盖成运行中，也不能把句柄登记下来；先确认任务状态，失败就直接返回。
        // 任务自己的错误信息要保留，避免用户只看到笼统的「服务失败」。
        let finished = match service.task.as_mut() {
            Some(task) if task.is_finished() => match task.await {
                Ok(Ok(())) => Some("http server exited early".to_owned()),
                Ok(Err(error)) => Some(error.detail),
                Err(error) => Some(format!("http server ended abnormally: {error}")),
            },
            _ => None,
        };
        if let Some(detail) = finished {
            return Err(AppError::new(ErrorCode::Server, detail));
        }
        if let Err(error) =
            self.state
                .mark_running(advertised, link.clone(), root_label, qr, expires_at)
        {
            // 状态写入失败时不能让已经启动的监听任务脱离管理：守卫 drop 时会撤销
            // 会话、请求关闭并中止任务。
            return Err(error);
        }
        // 监听任务在登记之后仍可能自行失败（例如运行期 accept/IO 错误）。控制器此时
        // 不再主动等待它，因此任务自身会在状态仍为 `Running` 时落到 `Failed`
        // （见 `http::serve`）；停止流程把状态改成 `Stopping` 后该回调会被忽略，
        // 不会覆盖用户成功的停止。
        // 先拿到槽位锁，再交接句柄：`into_running` 之后不能有 await 点。
        // 若先交接再等锁，等待期间 future 被取消会把局部 `Running` 丢掉，
        // `JoinHandle` 的 Drop 只 detach，任务从此既不在槽里也无人中止；
        // 反过来先等锁时守卫仍持有任务，取消会撤销会话并中止它。
        let mut slot = self.running.lock().await;
        let Some(running) = service.into_running() else {
            // 句柄缺失说明启动流程被破坏；不能留下「状态运行中但没有句柄」的局面。
            drop(slot);
            self.mark_stop_failed();
            return Err(AppError::new(ErrorCode::Server, "service handle lost"));
        };
        *slot = Some(running);
        service.disarm();
        drop(slot);
        // 任务可能在 `mark_running` 与登记之间就已经失败并把状态改成 `Failed`；
        // 此时不能返回成功，否则界面会先显示运行中再跳成失败。
        if !self.state.is_running() {
            // 回收期间同样借用槽位里的句柄；未确认退出时它留在槽里，不丢句柄。
            // 这里复用启动预算的绝对截止时间，不再额外延长。与上面一样把
            // 「取回收结果」和「改槽位」分成两步，避免在 `if let` 块内重写槽位。
            let mut slot = self.running.lock().await;
            let outcome = match slot.as_mut() {
                Some(running) => {
                    // 同上：先撤销代次，再在共享预算内确认在途请求归零；
                    // 未确认才中止，不把「已发出关闭信号」当成「请求已结束」。
                    running.revoke();
                    if !running.wait_inflight_idle(deadline).await {
                        running.task.abort();
                        running.aborted = true;
                    }
                    running.reclaim(deadline).await
                }
                None => Reclaim::Reclaimed,
            };
            if outcome.confirmed() {
                *slot = None;
            }
            drop(slot);
            return Err(AppError::new(ErrorCode::Server, "http server failed"));
        }
        Ok(link)
    }

    /// 用户发起的停止：默认预算见 `STOP_TIMEOUT_SECS`。
    pub async fn stop(&self) -> Result<(), AppError> {
        self.stop_with_timeout(Duration::from_secs(STOP_TIMEOUT_SECS))
            .await
    }

    /// 停止服务：撤销认证 → 关闭监听 → 等待回收；超时或任务异常都如实报错。
    ///
    /// 整段持有 `lifecycle`，因此不会与 `start` 交叉：`stop` 要么在启动开始前完成
    /// （随后 `start` 看到非空闲状态而拒绝），要么等启动完成后再停止它。
    ///
    /// 句柄在确认任务退出之前一直留在 `self.running` 槽里，等待通过 `MutexGuard`
    /// 借用句柄（`&mut running.task`），而不是把 `Running` 移出槽位：这个等待点是
    /// 取消点，若 future 被 drop，移出的 `JoinHandle` 会被 Drop 掉（只 detach），
    /// 控制器再也无法确认任务，下一次停止就会在「没有句柄」时伪报成功。
    /// 只有确认退出后才清空槽位。
    async fn stop_with_timeout(&self, budget: Duration) -> Result<(), AppError> {
        // 绝对截止时间从进入本函数就开始计时，并且同时覆盖「等生命周期锁」与
        // 「等监听任务退出」两段。若只给任务等待计时，一次正在进行的 `start`
        // 就会让宿主关闭无限期排队，宽限期形同虚设。
        let deadline = Instant::now() + budget;
        let _lifecycle = match tokio::time::timeout_at(deadline, self.lifecycle.lock()).await {
            Ok(guard) => guard,
            Err(_) => {
                // 还没拿到锁，无法安全改动状态或取走句柄：如实报告超时，
                // 让调用方（宿主）按失败处理，而不是伪报已停止。
                return Err(AppError::new(ErrorCode::Server, "stop timed out"));
            }
        };
        // 槽位锁从判定到等待全程持有：锁本身只是短暂的临界区，真正耗时的等待
        // 通过借用 `&mut running.task` 完成，不会把锁交给别的任务。这样做的好处是
        // 「判空 → 撤销认证 → 等待回收 → 清空槽位」是一个原子序列，取消发生在任何
        // 位置都不会让句柄离开槽位；若拆成两次加锁，取消窗口里槽位状态会短暂与
        // 判定结果不一致。
        let mut slot = self.running.lock().await;
        // 取消路径留下的已中止句柄也属于「控制器仍持有、尚未确认退出」的资源；
        // 判定是否进入停止流程时必须把它们算进来，否则空闲态 + 遗留句柄会被
        // `begin_stop(false)` 当成「没有资源」直接伪报成功。
        let has_orphans = self.has_orphaned();
        let has_handle = slot.is_some();
        let should_stop = self.state.begin_stop(has_handle || has_orphans)?;
        if !should_stop {
            return Ok(());
        }
        // 从这里起状态已是 `Stopping`，而下面的等待是一个可被取消的 await 点。
        // 取消守卫保证被 drop 时不会永久停在「正在停止…」；它只在作用域结束时使用，
        // 因此用下划线前缀避免未使用变量告警。
        let _stop_guard = StopGuard::new(Arc::clone(&self.state));
        // 先确认取消路径留下的已中止句柄，再处理运行槽。它们与运行槽共享同一
        // 绝对截止时间：先处理的资源不会让后面的等待获得额外预算。这里把统一的
        // 遗留错误映射成停止语义，避免用户看到与「停止」无关的启动/忙碌文案。
        let confirmed_orphans = match self.reclaim_orphaned(deadline).await {
            Ok(confirmed) => confirmed,
            Err(error) => {
                drop(slot);
                self.mark_stop_failed();
                return Err(AppError::new(
                    error.code,
                    if error.code == ErrorCode::Busy {
                        "stop timed out".to_owned()
                    } else {
                        error.detail
                    },
                ));
            }
        };
        if slot.is_none() {
            // 没有运行句柄：可能从未成功启动，也可能上一次停止已确认任务退出并
            // 清空了槽位。遗留句柄已在上面确认退出；由于整段持有 `lifecycle`，
            // 这里不存在「启动进行中但句柄尚未登记」的窗口。
            //
            // 但若刚刚确认的是被 abort 的遗留任务，这仍不是「用户停止了一个正常
            // 运行的服务」：它属于强制中止，必须如实报告，不能因为槽位为空就显示
            // 成功。没有遗留任务时才是真正的空操作。
            drop(slot);
            if confirmed_orphans > 0 {
                self.mark_stop_failed();
                return Err(AppError::new(ErrorCode::Server, "service task was aborted"));
            }
            self.state.mark_idle()?;
            return Ok(());
        }
        // 判空与借用之间没有其他写入者（生命周期锁 + 槽位锁都在本函数手里），
        // 因此这里必然有句柄；仍然用错误返回而不是 `unwrap`，避免结构变动时崩溃。
        let running = slot
            .as_mut()
            .ok_or_else(|| AppError::new(ErrorCode::Server, "service handle lost"))?;
        // 撤销次序与启动回收路径一致：先取消代次信号（在途请求在检查点退出、
        // 响应体流截断），再撤销令牌，最后请求关闭监听。随后在共享预算内确认
        // 在途请求归零，再等待任务退出；优雅关闭只终止 accept 循环，不能替代
        // 「在途请求已撤销」的确认。
        running.revoke();
        let inflight_idle = running.wait_inflight_idle(deadline).await;
        if !inflight_idle {
            // 预算内仍有请求在写盘或发送响应体：中止监听任务让运行时回收它们，
            // 但不能宣称「已撤销」——已开始的磁盘提交不可取消，结果如实保留给
            // 下一次停止确认。句柄留在槽里，本次停止按超时失败上报。
            running.task.abort();
            running.aborted = true;
        }
        let outcome = running.reclaim(deadline).await;
        match outcome {
            // 任务在截止时间内退出，且自身报告正常：这才是「已回收」。但若本轮
            // 还确认了之前被 abort 的遗留任务，整体仍不是「用户停止了一个正常
            // 运行的服务」，必须如实报告强制中止，不能把它抹平成成功。
            Reclaim::Reclaimed => {
                *slot = None;
                drop(slot);
                if confirmed_orphans > 0 {
                    self.mark_stop_failed();
                    return Err(AppError::new(ErrorCode::Server, "service task was aborted"));
                }
                self.state.mark_idle()?;
                Ok(())
            }
            // 任务正常结束但内部报错（监听失败）：资源已随任务释放，但服务是异常
            // 结束的，不能显示为「用户停止成功」，否则会掩盖真实故障。
            Reclaim::Failed(error) => {
                *slot = None;
                drop(slot);
                self.mark_stop_failed();
                Err(error)
            }
            // 任务以 panic 或非本次中止导致的取消结束：已退出，但把这种情况当成
            // 「停止成功」会掩盖真实故障，因此单独报错。
            Reclaim::Abnormally(detail) => {
                *slot = None;
                drop(slot);
                self.mark_stop_failed();
                Err(AppError::new(
                    ErrorCode::Server,
                    format!("service task ended abnormally: {detail}"),
                ))
            }
            // 任务是被本控制器在之前某次超时停止中 abort 掉的：已确认退出，但这是
            // 强制路径，不能报告成优雅停止；也不能复用「stop timed out」，
            // 否则调用方无法区分「这次又超时」和「确认了之前的中止」。
            Reclaim::Aborted => {
                *slot = None;
                drop(slot);
                self.mark_stop_failed();
                Err(AppError::new(ErrorCode::Server, "service task was aborted"))
            }
            // 超过共享预算：请求关闭没能在预算内完成。可以发 abort，但不得为了确认
            // 而延长到预算之外——宿主宽限期是硬约束。未确认退出时句柄留在槽里并
            // 标记已中止，下一次停止仍能继续确认，而不是因为槽位为空伪报成功。
            Reclaim::Pending => {
                running.task.abort();
                running.aborted = true;
                drop(slot);
                self.mark_stop_failed();
                Err(AppError::new(ErrorCode::Server, "stop timed out"))
            }
        }
    }

    /// 进程退出前尽力停止；返回是否确认已回收。
    ///
    /// 宿主收到 `shutdown` 回复后只留约两秒宽限期就会强杀进程，因此这里用比用户停止
    /// 更短的预算，尽量在宽限期内完成回收。返回 `false` 表示没有确认回收：
    /// 进程仍会以失败退出，宿主据退出码与日志判断，而不是被伪报的成功误导。
    pub async fn shutdown(&self) -> bool {
        self.stop_with_timeout(Duration::from_secs(SHUTDOWN_TIMEOUT_SECS))
            .await
            .is_ok()
    }

    /// 停止失败统一落到可见的 `Failed`，保留用户可操作的重试入口。
    fn mark_stop_failed(&self) {
        let _ = self.state.mark_failed(
            crate::l10n::Locale::detect()
                .text(crate::l10n::Key::ErrorInternal)
                .to_owned(),
        );
    }

    /// 是否仍保留需要停止入口回收的运行资源。
    ///
    /// `Failed`/`Idle` 不一定还有句柄：目录缺失或绑定失败时没有任何监听任务，
    /// 界面不应继续显示 Stop；但停止失败、取消启动或超时可能留下未确认退出的
    /// 句柄，此时必须保留停止入口。槽位锁被占用时按「仍有资源」处理：宁可多给
    /// 一次可回收的停止入口，也不要在无法判断时把恢复路径藏起来。
    pub fn retains_running_resources(&self) -> bool {
        let running = match self.running.try_lock() {
            Ok(slot) => slot.is_some(),
            Err(_) => return true,
        };
        running || self.has_orphaned()
    }

    /// 是否仍有取消路径留下的未确认句柄；锁中毒按「有」处理，宁可进入停止流程
    /// 并如实失败，也不在存在未知资源时报告空闲。
    fn has_orphaned(&self) -> bool {
        self.orphaned
            .lock()
            .map(|list| !list.is_empty())
            .unwrap_or(true)
    }

    /// 确认取消路径留下的已中止句柄；返回确认退出的数量。
    ///
    /// 遗留列表由 `DetachedService::drop` 推入（同步锁，push 必成功）。这里逐个
    /// 取出并在共享截止时间内等待：确认的句柄随守卫释放，未确认的由守卫在 drop 时
    /// 放回列表，让后续停止/启动继续确认，而不是丢掉句柄假装回收。
    ///
    /// 逐个取出而不是整体 `take`，是因为本函数本身也是取消点：整体取出后若在
    /// 等待中被取消，局部 `Vec<Running>` 会被 drop，`JoinHandle` 全部 detach，
    /// 又回到「无人可确认」。单条守卫保证任何中断都只影响当前这一条，其余仍在列表里。
    ///
    /// 这些任务在取消时已被 abort，`reclaim` 会把它们归类为强制中止；这里只负责
    /// 确认退出与计数，分类由调用方按「已确认中止不是优雅停止」如实上报。
    async fn reclaim_orphaned(&self, deadline: Instant) -> Result<usize, AppError> {
        let mut confirmed = 0usize;
        loop {
            let running = match self.orphaned.lock() {
                Ok(mut list) => list.pop(),
                Err(_) => {
                    return Err(AppError::new(
                        ErrorCode::Server,
                        "orphaned service registry unavailable",
                    ));
                }
            };
            let Some(running) = running else {
                return Ok(confirmed);
            };
            // 守卫在未确认退出时把句柄放回列表；确认后由 `release` 解除武装。
            let mut pending = OrphanReclaim {
                orphaned: Arc::clone(&self.orphaned),
                running: Some(running),
            };
            let running = pending
                .running
                .as_mut()
                .ok_or_else(|| AppError::new(ErrorCode::Server, "orphaned service handle lost"))?;
            // 旧令牌在取消时已撤销；这里再撤销一次是幂等的，且能覆盖「取消发生在
            // 会话登记之前」的极端顺序。代次取消同样幂等，且必须重放：遗留任务
            // 虽然已被 abort，但它内层 spawn 的连接任务仍可能存活，只有取消信号
            // 能让处理器与响应体流在下一个检查点退出。
            running.revoke();
            if !running.wait_inflight_idle(deadline).await {
                return Err(AppError::new(
                    ErrorCode::Busy,
                    "previous service still reclaiming",
                ));
            }
            if !running.reclaim(deadline).await.confirmed() {
                // 未确认：守卫 drop 会把句柄放回列表，下一轮停止/启动继续确认。
                return Err(AppError::new(
                    ErrorCode::Busy,
                    "previous service still reclaiming",
                ));
            }
            pending.release();
            confirmed += 1;
        }
    }
}

/// 单条遗留句柄的回收守卫。
///
/// 为什么需要它：`reclaim` 的等待是取消点，若此时调用方 future 被 drop，局部
/// `Running` 会被丢弃，`JoinHandle` 只会 detach，任务从此无人可确认。守卫在
/// drop 时把未确认的句柄放回遗留列表，让下一次停止/启动还能继续确认。
struct OrphanReclaim {
    orphaned: Arc<std::sync::Mutex<Vec<Running>>>,
    running: Option<Running>,
}

impl OrphanReclaim {
    /// 已确认退出：解除武装，句柄随本结构一起释放。
    fn release(&mut self) {
        self.running = None;
    }
}

impl Drop for OrphanReclaim {
    fn drop(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        // 锁中毒时无处可放：此时只能让句柄随 drop 释放，但这条路径已经在
        // `reclaim_orphaned` 的锁失败分支里被如实上报，不会伪报成功。
        if let Ok(mut list) = self.orphaned.lock() {
            list.push(running);
        }
    }
}

/// 打开共享根：拒绝文件、链接与不可安全打开的目录，并把原因映射成可见错误。
fn open_root(path: &Path) -> Result<ShareRoot, AppError> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(AppError::new(ErrorCode::PathRejected, "not a directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AppError::new(ErrorCode::PathRejected, "root missing"));
        }
        Err(_) => return Err(AppError::new(ErrorCode::PathRejected, "root unavailable")),
    }
    ShareRoot::open(path)
}

/// 生成无静区二维码矩阵；边长必须满足宿主校验的 `21 + 4n` 形式。
fn render_qr(link: &str) -> Result<String, AppError> {
    let code = qrcode::QrCode::new(link.as_bytes())
        .map_err(|_| AppError::new(ErrorCode::Server, "qr encode failed"))?;
    let width = code.width();
    let colors = code.to_colors();
    let mut matrix = String::with_capacity((width + 1) * width);
    for row in 0..width {
        for column in 0..width {
            matrix.push(match colors[row * width + column] {
                qrcode::Color::Dark => '1',
                qrcode::Color::Light => '0',
            });
        }
        matrix.push('\n');
    }
    Ok(matrix)
}

/// 错误到文案键的映射；只覆盖用户能据此采取行动的原因，其余归为内部错误。
///
/// 共享目录的三种失败（不存在、不是目录、不可安全打开）处置方式不同：前两者用户
/// 换一个目录即可，第三种要解释为什么被拒绝，因此按 `open_root` 的固定诊断文本区分，
/// 而不是统一显示「不可共享」让用户无从下手。
///
/// 视图动作层也复用这条映射，把错误以本地化文本写进面板内联错误行；协议错误码与
/// 英文诊断仍由 `map_error` 保留，二者不互相替代。
pub(crate) fn error_key(error: &AppError) -> crate::l10n::Key {
    use crate::l10n::Key;
    // 停止超时可能由 Busy 的遗留回收失败改写而来；必须先看稳定诊断文本，
    // 否则会被当成普通忙碌/内部错误，界面失去「重试停止」的提示。
    if error.detail == "stop timed out" {
        return Key::ErrorStopTimeout;
    }
    match error.code {
        ErrorCode::PathRejected => match error.detail.as_str() {
            "root missing" => Key::ErrorRootMissing,
            "not a directory" => Key::ErrorRootNotDirectory,
            _ => Key::ErrorRootRejected,
        },
        ErrorCode::BadRequest => match error.detail.as_str() {
            // 解析失败与「不是本机地址」是两种修正方式，必须给出不同文案。
            "address not on this host" => Key::ErrorAddressNotLocal,
            _ => Key::ErrorAddressInvalid,
        },
        ErrorCode::Unavailable => Key::ErrorBind,
        ErrorCode::Busy => Key::ErrorBusy,
        _ => Key::ErrorInternal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把监听地址固定到回环：测试不应该把临时共享目录暴露到局域网。
    /// 本机没有非回环网卡时首选地址已经是回环，此时重复设置仍然合法（状态为 Idle）。
    fn bind_loopback(controller: &Controller) {
        controller.state.select_address("127.0.0.1").unwrap();
    }

    /// 停止一个从未启动的控制器必须是幂等的：宿主可能重复请求关闭，
    /// 重复请求既不能报错，也不能把状态改成「已停止」之外的任何值。
    /// 注意这里不能证明「真实任务已回收」，只证明空闲态的停止路径不产生副作用。
    #[tokio::test]
    async fn stop_without_start_is_idempotent() {
        let controller = Controller::new();
        assert!(controller.stop().await.is_ok());
        assert!(controller.stop().await.is_ok());
        assert_eq!(
            controller.state.snapshot().unwrap().phase,
            crate::state::Phase::Idle
        );
    }

    /// 未运行时关闭进程也必须成功：没有监听任务就没有可回收资源，
    /// 不能因为没有运行过就返回超时，让宿主误判为进程泄漏。
    #[tokio::test]
    async fn shutdown_without_start_reports_reclaimed() {
        let controller = Controller::new();
        assert!(controller.shutdown().await);
    }

    /// 启动时给出的地址无效必须明确失败，且不能退回旧地址：
    /// 退回会让用户以为只共享给了回环，实际却绑到了之前的局域网地址。
    #[tokio::test]
    async fn start_with_invalid_address_fails_without_fallback() {
        let root = tempfile::tempdir().unwrap();
        let share = root.path().join("share");
        std::fs::create_dir(&share).unwrap();
        let controller = Controller::new();
        bind_loopback(&controller);
        let error = controller
            .start_with_address(share.clone(), Some("203.0.113.9"))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::BadRequest);
        let snapshot = controller.state.snapshot().unwrap();
        assert_ne!(snapshot.phase, crate::state::Phase::Running);
        assert!(snapshot.link.is_none());
        // 旧地址仍然保留（用于用户修正），但从未被用来绑定。
        assert_eq!(snapshot.selected_address, "127.0.0.1");
        assert!(controller.shutdown().await);
    }

    /// 直接带地址启动（模拟「改完地址直接点启动」）必须采用动作里的地址，
    /// 而不是之前的状态；绑定结果与用户看到的地址一致。
    #[tokio::test]
    async fn start_with_address_uses_action_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let share = root.path().join("share");
        std::fs::create_dir(&share).unwrap();
        let controller = Controller::new();
        // 初始选中回环；动作里再次给出同一地址，验证「不先发 set-address 直接 start」
        // 这条路径本身可用（宿主只在 Enter 时提交输入框）。
        bind_loopback(&controller);
        let link = controller
            .start_with_address(share.clone(), Some("127.0.0.1"))
            .await
            .unwrap();
        assert!(link.starts_with("http://127.0.0.1:"), "link: {link}");
        let snapshot = controller.state.snapshot().unwrap();
        assert_eq!(snapshot.selected_address, "127.0.0.1");
        assert_eq!(snapshot.phase, crate::state::Phase::Running);
        assert!(controller.shutdown().await);
    }

    /// 目录不存在时启动必须落到可见的 `Failed`，且不留下运行句柄；
    /// 这是「失败也保留路径供用户修正」的状态契约。
    #[tokio::test]
    async fn start_with_missing_root_reports_failed() {
        let controller = Controller::new();
        // 在临时目录下取一个未创建的子路径：每次运行都唯一，
        // 不会因为上次遗留的同名目录让「不存在」这个前提失效。
        let base = tempfile::tempdir().unwrap();
        let missing = base.path().join("missing");
        assert!(controller.start(missing.clone()).await.is_err());
        let snapshot = controller.state.snapshot().unwrap();
        assert_eq!(snapshot.phase, crate::state::Phase::Failed);
        assert!(snapshot.error.is_some());
        assert!(snapshot.address.is_none() && snapshot.link.is_none());
        assert_eq!(snapshot.root_path.as_deref(), missing.to_str());
        assert!(controller.shutdown().await);
    }

    /// 通配启动必须把具体本机 IPv4 写进链接与展示地址，绝不能把 `0.0.0.0`
    /// 交给对端；停止与重试仍保留用户显式选择的通配值，但每次重新生成链接。
    #[tokio::test]
    async fn wildcard_start_uses_concrete_link_and_survives_retry() {
        let root = tempfile::tempdir().unwrap();
        let share = root.path().join("share");
        std::fs::create_dir(&share).unwrap();
        let controller = Controller::new();
        // 显式选择通配地址；默认值不能自动变成通配。
        controller.state.select_address("0.0.0.0").unwrap();
        let link = controller.start(share.clone()).await.unwrap();
        let snapshot = controller.state.snapshot().unwrap();
        assert_eq!(snapshot.phase, crate::state::Phase::Running);
        assert_eq!(snapshot.selected_address, "0.0.0.0");
        assert!(snapshot.qr.is_some());
        let authority = link
            .strip_prefix("http://")
            .and_then(|link| link.split('/').next())
            .expect("link authority");
        let host = authority.rsplit_once(':').map(|(host, _)| host).unwrap();
        // 链接必须使用真实可打开的具体地址，通配语义只存在于绑定层。
        assert_ne!(host, "0.0.0.0", "link must not expose the wildcard address");
        assert_eq!(snapshot.address.as_deref(), Some(authority));

        controller.stop().await.unwrap();
        assert_eq!(
            controller.state.snapshot().unwrap().selected_address,
            "0.0.0.0",
            "stop must keep the explicit wildcard selection for retry"
        );
        let second = controller.start(share).await.unwrap();
        // 不比较密钥原文；完整链接不同只输出固定说明，避免把一次性密钥带进日志。
        assert!(second != link, "restart must issue a new link");
        let snapshot = controller.state.snapshot().unwrap();
        assert_eq!(snapshot.selected_address, "0.0.0.0");
        let authority = second
            .strip_prefix("http://")
            .and_then(|link| link.split('/').next())
            .expect("second link authority");
        assert_eq!(snapshot.address.as_deref(), Some(authority));
        assert!(controller.shutdown().await);
    }

    /// 启动成功后再停止必须回到 `Idle` 并清空链接、二维码与地址；
    /// 统计保留是刻意的，便于用户回看本次会话的传输量。
    #[tokio::test]
    async fn start_then_stop_clears_share_state() {
        let root = tempfile::tempdir().unwrap();
        let share = root.path().join("share");
        std::fs::create_dir(&share).unwrap();
        let controller = Controller::new();
        // 只监听回环，避免测试期间把共享目录暴露到局域网；首选地址若已是回环
        // 就不能再改选，因为运行中改地址会被拒绝。
        bind_loopback(&controller);
        let link = controller.start(share.clone()).await.unwrap();
        assert!(link.starts_with("http://127.0.0.1:"), "link: {link}");
        let running = controller.state.snapshot().unwrap();
        assert_eq!(running.phase, crate::state::Phase::Running);
        assert!(running.address.is_some() && running.qr.is_some());
        // 绑定 0 端口后地址必须带真实端口，否则分享链接不可用。
        assert!(
            running
                .address
                .as_deref()
                .is_some_and(|value| !value.ends_with(":0"))
        );

        controller.stop().await.unwrap();
        let idle = controller.state.snapshot().unwrap();
        assert_eq!(idle.phase, crate::state::Phase::Idle);
        assert!(idle.address.is_none() && idle.link.is_none() && idle.qr.is_none());
        assert!(idle.started_at.is_none() && idle.expires_at.is_none());
        // 停止后再次关闭仍必须报告已回收，避免宿主把幂等关闭当成泄漏。
        assert!(controller.shutdown().await);
    }

    /// 运行中重复启动必须被拒绝且不改变当前运行状态；否则第二次启动会覆盖
    /// 第一次的会话与票据，让已打开的浏览器页面突然失效。
    #[tokio::test]
    async fn second_start_while_running_is_busy() {
        let root = tempfile::tempdir().unwrap();
        let share = root.path().join("share");
        std::fs::create_dir(&share).unwrap();
        let controller = Controller::new();
        bind_loopback(&controller);
        let first = controller.start(share.clone()).await.unwrap();
        let error = controller.start(share).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Busy);
        let snapshot = controller.state.snapshot().unwrap();
        assert_eq!(snapshot.phase, crate::state::Phase::Running);
        assert_eq!(snapshot.link.as_deref(), Some(first.as_str()));
        assert!(controller.shutdown().await);
    }

    /// 并发 `start` + `stop` 必须收敛到「停止之后不再复活」。
    ///
    /// 调度是确定的：先让 `start` 进入启动阶段（观察到状态离开空闲），再发起 `stop`。
    /// 此时 `start` 正持有生命周期锁并等待绑定端口，`stop` 只能在它结束后进入；
    /// 若没有这把锁，`stop` 会看到「还没有运行句柄」而直接报告成功，随后 `start`
    /// 把状态改成运行中——正是要防的复活。最终状态必须是空闲，且不能有运行链接。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_stop_cannot_leave_service_running() {
        let root = tempfile::tempdir().unwrap();
        let share = root.path().join("share");
        std::fs::create_dir(&share).unwrap();
        let controller = Controller::new();
        bind_loopback(&controller);

        let starter = {
            let controller = Arc::clone(&controller);
            let share = share.clone();
            tokio::spawn(async move { controller.start(share).await })
        };
        // 等到启动确实开始（状态离开空闲）再发起停止，避免「停止先完成、启动后成功」
        // 这种合法但会让断言失去意义的顺序。
        let mut observed = false;
        for _ in 0..10_000 {
            if controller.state.snapshot().unwrap().phase != crate::state::Phase::Idle {
                observed = true;
                break;
            }
            if starter.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(observed, "start must leave the idle phase before stop");

        let stopper = {
            let controller = Arc::clone(&controller);
            tokio::spawn(async move { controller.stop().await })
        };
        let _ = starter.await.unwrap();
        let stopped = stopper.await.unwrap();
        assert!(stopped.is_ok(), "stop must succeed: {stopped:?}");

        let snapshot = controller.state.snapshot().unwrap();
        assert_eq!(snapshot.phase, crate::state::Phase::Idle, "{snapshot:?}");
        assert!(snapshot.link.is_none() && snapshot.address.is_none());
        assert!(controller.shutdown().await);
    }

    /// 停止期间不接受新的启动：`stop` 成功后紧接着的 `start` 必须从空闲态重新开始，
    /// 而不是复用上一次的代次或旧会话。
    #[tokio::test]
    async fn restart_after_stop_uses_new_generation() {
        let root = tempfile::tempdir().unwrap();
        let share = root.path().join("share");
        std::fs::create_dir(&share).unwrap();
        let controller = Controller::new();
        bind_loopback(&controller);
        let first = controller.start(share.clone()).await.unwrap();
        controller.stop().await.unwrap();
        let second = controller.start(share).await.unwrap();
        // 链接里的密钥每次启动都必须不同，否则旧链接会在重启后继续可用。
        assert_ne!(first, second, "link must not be reused across restarts");
        assert_eq!(
            controller.state.snapshot().unwrap().phase,
            crate::state::Phase::Running
        );
        assert!(controller.shutdown().await);
    }

    /// 启动 future 被取消时不能留下「正在启动」或脱离管理的监听任务。
    ///
    /// 这里直接断言两个守卫的 drop 语义，而不是在集成层复现「SDK 中途取消」：
    /// 绑定端口与生成二维码都是同步或立即就绪的，无法稳定停在某个 await 点上制造取消，
    /// 靠 sleep 或多次 yield 的用例会在快机器上偶发通过、在慢机器上偶发失败。
    /// 取消路径的真实触发点是 `view.action` 被 SDK 取消；本用例只证明守卫本身
    /// 会收回状态、撤销令牌并中止任务，不声称已验证 SDK 的取消时序。
    #[tokio::test]
    async fn cancellation_guards_release_state_and_task() {
        let state = State::new();
        state.select_address("127.0.0.1").unwrap();
        state.begin_start().unwrap();
        {
            let _guard = StartGuard::new(Arc::clone(&state));
        }
        assert_eq!(
            state.snapshot().unwrap().phase,
            crate::state::Phase::Idle,
            "dropped start guard must not leave the panel in Starting"
        );

        // 守卫接管一个永不完成的监听任务：drop 后必须请求关闭并中止任务，
        // 且撤销会话与票据，避免取消后旧令牌仍然可用。
        let (sessions, secret) = Sessions::start(1).unwrap();
        let tickets = Tickets::new(1);
        let (token, _) = sessions.pair(&secret).unwrap();
        let (ticket, _) = tickets
            .issue(&crate::path::VirtualPath::parse("root/note.txt").unwrap())
            .unwrap();
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            // 守卫不发送关闭信号时任务一直等待，用来证明 drop 确实中止了它；
            // 收到信号则正常结束，避免用永不返回的 future 制造不可达代码。
            let _ = receiver.await;
            Ok::<(), AppError>(())
        });
        let orphaned = Arc::new(std::sync::Mutex::new(Vec::new()));
        // 守卫现在必须持有本代 RunHandle：drop 时先取消在途请求再中止任务，
        // 测试若省略它就无法证明取消路径真的切断了代次信号。
        let run = crate::http::RunHandle::new(1);
        {
            let _guard = DetachedService::new(
                shutdown,
                task,
                sessions.clone(),
                tickets.clone(),
                run,
                state.clone(),
                Arc::clone(&orphaned),
            );
        }
        // 守卫在 drop 时发送了关闭信号并中止任务：会话必须立刻失效。
        assert!(
            sessions.verify(&token).is_err(),
            "cancelled start must revoke issued sessions"
        );
        assert!(
            tickets.resolve(&ticket, 1).is_err(),
            "cancelled start must clear issued tickets"
        );
        // 已中止但尚未确认退出的句柄必须留在遗留列表里，不能被丢弃：
        // 这是「取消后仍可确认回收」的结构性保证。
        assert_eq!(
            orphaned.lock().unwrap().len(),
            1,
            "cancelled start must hand the aborted task to the controller"
        );
    }

    /// 受控监听任务与控制器：任务先等关闭信号，再等测试显式释放，最后正常返回。
    ///
    /// 为什么不用真实 `serve`：需要精确制造「停止等待中被取消」与「超时后任务仍未
    /// 退出」两种时序，真实监听器无法稳定停在指定状态，靠 sleep 的用例会偶发。
    /// 这里全部由通道驱动，时序确定，也不占用真实端口。
    struct Harness {
        controller: Arc<Controller>,
        sessions: Sessions,
        token: String,
        tickets: Tickets,
        ticket: String,
        release: Option<oneshot::Sender<()>>,
    }

    impl Harness {
        /// `running` 为真时状态预置为运行中，否则预置为失败态（用于验证重新启动
        /// 时回收旧句柄的路径）；两种状态都预置一个真实句柄。任务会先响应关闭
        /// 请求，再等测试调用 `finish`：这样「等待点被取消」和「超时」都能停在
        /// 确定位置，而不是靠调度运气。
        async fn new(running: bool) -> Self {
            let controller = Controller::new();
            if running {
                controller.state.begin_start().unwrap();
                let bound: SocketAddr = "127.0.0.1:1234".parse().unwrap();
                controller
                    .state
                    .mark_running(bound, "link".into(), "share".into(), "qr".into(), 10)
                    .unwrap();
            } else {
                controller
                    .state
                    .mark_failed("previous failure".into())
                    .unwrap();
            }
            let (sessions, secret) = Sessions::start(1).unwrap();
            let (token, _) = sessions.pair(&secret).unwrap();
            let tickets = Tickets::new(1);
            let (ticket, _) = tickets
                .issue(&crate::path::VirtualPath::parse("root/note.txt").unwrap())
                .unwrap();
            let (shutdown, receiver) = oneshot::channel::<()>();
            let (release, released) = oneshot::channel::<()>();
            let task = tokio::spawn(async move {
                // 收到关闭请求后仍不退出：等待测试释放，才能让「等待点被取消」和
                // 「超时」发生在确定的位置。
                let _ = receiver.await;
                let _ = released.await;
                Ok::<(), AppError>(())
            });
            *controller.running.lock().await = Some(Running {
                shutdown: Some(shutdown),
                task,
                sessions: sessions.clone(),
                tickets: tickets.clone(),
                // 停止流程会通过代次句柄取消在途请求并等待归零；测试句柄必须携带
                // 真实 RunHandle，否则无法覆盖 revoke 与 wait_inflight_idle 的路径。
                run: crate::http::RunHandle::new(1),
                aborted: false,
            });
            Self {
                controller,
                sessions,
                token,
                tickets,
                ticket,
                release: Some(release),
            }
        }

        /// 让任务正常返回；未释放时任务一直挂在等待点上。
        fn finish(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
        }

        async fn has_handle(&self) -> bool {
            self.controller.running.lock().await.is_some()
        }

        /// 等到槽位里的任务确实结束（中止已生效），再让测试继续。
        ///
        /// `abort` 只发信号，任务真正退出是调度器稍后完成的事件；固定 sleep 既
        /// 会拖慢测试也可能在慢机器上偶发失败。这里用 `yield_now` 轮询，等到任务
        /// 自己报告结束后再断言，避免把「已中止但还没跑完」当成「已确认回收」。
        async fn wait_task_finished(&self) {
            for _ in 0..1000 {
                {
                    let mut slot = self.controller.running.lock().await;
                    if let Some(running) = slot.as_mut()
                        && running.task.is_finished()
                    {
                        return;
                    }
                }
                tokio::task::yield_now().await;
            }
            panic!("task did not finish after abort");
        }
    }

    /// 把 future 轮询一次并断言它挂起，用来在确定位置模拟 SDK 取消 `view.action`。
    ///
    /// 只轮询一次就丢弃，等价于「执行到第一个 await 点被取消」，不需要 sleep。
    /// 用 UFCS 调用 `Future::poll` 是为了不把 trait 引入测试模块的其余代码。
    fn poll_once_pending<F: std::future::Future>(future: std::pin::Pin<&mut F>) {
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(future, &mut context).is_pending());
    }

    /// 停止等待中被取消：句柄必须仍在控制器里，下一次停止能真正确认回收。
    ///
    /// 这是「取消后不能伪成功」的核心回归：旧实现把 `Running` 移入 stop 的局部
    /// 变量，取消时 `JoinHandle` 被 Drop（只 detach），槽位为空，下一次停止会
    /// 直接报告成功而任务可能仍在监听。
    #[tokio::test]
    async fn cancelled_stop_keeps_handle_and_next_stop_confirms() {
        let mut harness = Harness::new(true).await;
        assert!(harness.sessions.verify(&harness.token).is_ok());

        let mut stop = Box::pin(harness.controller.stop_with_timeout(Duration::from_secs(5)));
        poll_once_pending(stop.as_mut());
        // 取消前已经撤销认证：旧令牌不能继续用。
        assert!(
            harness.sessions.verify(&harness.token).is_err(),
            "stop must revoke sessions before waiting"
        );
        drop(stop);

        assert!(
            harness.has_handle().await,
            "cancelled stop must keep the handle in the controller"
        );
        // 状态是 Stopping 时取消：必须落成可见失败，不能永远停在「正在停止…」。
        assert_eq!(
            harness.controller.state.snapshot().unwrap().phase,
            crate::state::Phase::Failed,
            "cancelled stop must not leave the panel in Stopping"
        );

        // 任务现在正常退出；下一次停止必须确认回收并清空槽位。
        harness.finish();
        harness
            .controller
            .stop_with_timeout(Duration::from_secs(5))
            .await
            .unwrap();
        assert!(!harness.has_handle().await);
        assert_eq!(
            harness.controller.state.snapshot().unwrap().phase,
            crate::state::Phase::Idle
        );
        assert!(harness.tickets.resolve(&harness.ticket, 1).is_err());
    }

    /// 停止超时：不得越过共享预算继续等，也不得丢句柄或伪报成功。
    ///
    /// 断言耗时明显小于固定的一秒确认窗口，用于区分「在截止时间后再多等一轮
    /// 确认」的旧行为；第二次停止必须能确认已中止的任务，而不是因为槽位为空
    /// 报告成功。
    #[tokio::test]
    async fn stop_timeout_keeps_handle_without_extending_budget() {
        let harness = Harness::new(true).await;
        let started = Instant::now();
        let error = harness
            .controller
            .stop_with_timeout(Duration::from_millis(50))
            .await
            .unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(error.detail, "stop timed out");
        assert!(
            elapsed < Duration::from_millis(900),
            "stop must not wait past its budget: {elapsed:?}"
        );
        assert!(
            harness.has_handle().await,
            "unconfirmed stop must keep the handle"
        );

        // 任务已被 abort；等它真正结束，第二次停止才能确认退出。第二次仍如实报告
        // 「强制中止」，不能把 abort 说成优雅停止。
        harness.wait_task_finished().await;
        let error = harness
            .controller
            .stop_with_timeout(Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(error.detail, "service task was aborted");
        assert!(!harness.has_handle().await);
        assert_eq!(
            harness.controller.state.snapshot().unwrap().phase,
            crate::state::Phase::Failed
        );
    }

    /// 重新启动时回收旧句柄的过程中被取消：旧句柄不能丢，后续停止仍能确认。
    ///
    /// 旧实现把 `Running` 从槽位取出后再等待，取消会让 `JoinHandle` 被 Drop，
    /// 旧任务从此无人可确认。这里用失败态预置一个已挂起的旧句柄，取消启动后
    /// 必须仍能在槽里看到它，并且下一次停止能确认它真正退出。
    #[tokio::test]
    async fn cancelled_start_keeps_stale_handle() {
        let harness = Harness::new(false).await;
        let root = tempfile::tempdir().unwrap();
        let share = root.path().join("share");
        std::fs::create_dir(&share).unwrap();

        let mut start = Box::pin(
            harness
                .controller
                .start_with_address(share, Some("127.0.0.1")),
        );
        poll_once_pending(start.as_mut());
        drop(start);

        assert!(
            harness.has_handle().await,
            "cancelled start must keep the stale handle for later confirmation"
        );
        assert_eq!(
            harness.controller.state.snapshot().unwrap().phase,
            crate::state::Phase::Idle,
            "cancelled start must not leave the panel in Starting"
        );

        // 取消启动只保证句柄不丢，并不会顺手中止旧任务：回收等待被 drop 后，
        // 旧任务仍在槽里运行。第一次停止用短预算验证「超时不越过预算、保留句柄」，
        // 它会在预算耗尽后 abort；第二次停止才能确认「强制中止」并清空槽位。
        let error = harness
            .controller
            .stop_with_timeout(Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(error.detail, "stop timed out");
        assert!(
            harness.has_handle().await,
            "timed-out stop must keep the stale handle"
        );
        harness.wait_task_finished().await;
        let error = harness
            .controller
            .stop_with_timeout(Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(error.detail, "service task was aborted");
        assert!(!harness.has_handle().await);
        // 任务已被中止，取消掉的启动没有产生新链接。
        assert!(harness.controller.state.snapshot().unwrap().link.is_none());
    }

    /// 取消登记前留下的已中止句柄必须由后续停止确认，不能在空闲态伪报成功。
    ///
    /// 旧实现在 `DetachedService::drop` 里 `try_lock` 失败或槽位已占用时直接丢弃
    /// `JoinHandle`，任务只被 detach；控制器既没有句柄、也没有遗留记录，下一次
    /// 停止会在 `begin_stop(false)` 直接返回成功，而监听任务可能仍在运行。
    /// 这里直接构造守卫：它在 drop 时把句柄推入遗留列表；随后停止必须进入停止流程，
    /// 在截止时间内确认该任务并如实报告「强制中止」，而不是报告空闲。
    #[tokio::test]
    async fn orphaned_cancelled_start_is_confirmed_by_stop() {
        let state = State::new();
        state.select_address("127.0.0.1").unwrap();
        let (sessions, _secret) = Sessions::start(1).unwrap();
        let tickets = Tickets::new(1);
        let (shutdown, _receiver) = oneshot::channel();
        // 永不结束的任务：只有 abort 能让它退出，模拟取消发生在登记之前。
        let task = tokio::spawn(async {
            std::future::pending::<()>().await;
            Ok::<(), AppError>(())
        });
        let controller = Controller::new();
        // 与生产路径一致：守卫需要本代 RunHandle 才能在 drop 时取消在途请求；
        // 省略它会让「取消后旧令牌失效」只验证到一半。
        let run = crate::http::RunHandle::new(1);
        {
            let _guard = DetachedService::new(
                shutdown,
                task,
                sessions,
                tickets,
                run,
                Arc::clone(&controller.state),
                Arc::clone(&controller.orphaned),
            );
        }
        assert!(
            controller.has_orphaned(),
            "dropped guard must retain the aborted task"
        );

        // 等待 abort 生效后再停止：停止流程需要在截止时间内确认退出。
        for _ in 0..1000 {
            if !controller.has_orphaned() {
                break;
            }
            if controller
                .orphaned
                .lock()
                .unwrap()
                .iter()
                .all(|running| running.task.is_finished())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        // 这里允许两种真实结果：任务已经结束则停止确认并报告「强制中止」；
        // 若仍未结束则停止必须在预算内如实超时。两者都不能是「成功且无资源」。
        let outcome = controller.stop_with_timeout(Duration::from_secs(5)).await;
        match outcome {
            Err(error) => {
                assert!(
                    matches!(
                        error.detail.as_str(),
                        "service task was aborted" | "stop timed out"
                    ),
                    "unexpected stop error: {error:?}"
                );
                // 超时路径必须把未确认的句柄留在列表里，供下一次停止继续确认；
                // 只有确认中止后列表才允许为空。两种结果都不能丢句柄。
                if error.detail == "stop timed out" {
                    assert!(
                        controller.has_orphaned(),
                        "timed-out stop must keep the unconfirmed handle"
                    );
                }
            }
            Ok(()) => panic!("orphaned task must never be reported as a clean stop"),
        }
        // 确认中止后列表必须清空；这里补一次停止把仍留着的句柄确认掉，避免测试
        // 结束后任务仍在后台等待调度器回收。
        if controller.has_orphaned() {
            for _ in 0..1000 {
                if controller
                    .orphaned
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|running| running.task.is_finished())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let _ = controller.stop_with_timeout(Duration::from_secs(5)).await;
        }
        assert!(
            !controller.has_orphaned(),
            "confirmed stop must not leave unmanaged handles"
        );
    }
}
