//! HTTP 服务装配与安全中间件。
//! @author kongweiguang
//!
//! 路由刻意保持最小：静态页面、配对、会话、列表、上传、下载票据与票据下载。
//! 所有响应都经过同一层安全头，避免某个处理器漏设而让页面被嵌入第三方框架。

pub mod assets;
pub mod auth;
pub mod download;
pub mod files;
pub mod upload;

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Router,
    extract::{Request, State as AxumState},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use tokio::{net::TcpListener, sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;

use crate::{
    error::{AppError, ErrorCode},
    fs_root::ShareRoot,
    session::Sessions,
    state::State,
    ticket::Tickets,
};

/// 一次服务代次的取消信号与在途请求计数。
///
/// 为什么需要独立于 `State` 的代次句柄：`State::is_running()` 是全局字段，服务停止后
/// 重新启动会再次变成 `true`，于是上一代仍在运行的处理器会误以为自己仍然有效——
/// 典型后果是旧上传在重启后继续写盘、旧下载继续发送文件。把「取消」绑定到每代句柄，
/// 处理器只认自己这一代的信号，重启后的新代不会为旧请求「续命」。
///
/// `inflight` 计数是停止流程「已撤销在途请求」的唯一证据：外层 `axum::serve` 的
/// `JoinHandle` 被 abort 只会丢弃监听 future，它内部 `spawn` 的连接任务会被 detach
/// 并继续运行；只看外层句柄就宣称回收等于把「已脱离管理」当成「已结束」。
#[derive(Clone)]
pub struct RunHandle {
    generation: u64,
    cancel: CancellationToken,
    inflight: Arc<InFlight>,
}

struct InFlight {
    /// 当前仍在处理（含响应体仍在流式发送）的请求数。
    active: Mutex<usize>,
    /// 归零时唤醒停止流程；用 `Notify` 而不是轮询，避免引入睡眠等待。
    idle: tokio::sync::Notify,
}

impl RunHandle {
    /// 为一次启动创建代次句柄；代次号与 `Sessions`/`Tickets` 使用同一个值，
    /// 便于日志与测试把三者对上。
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            cancel: CancellationToken::new(),
            inflight: Arc::new(InFlight {
                active: Mutex::new(0),
                idle: tokio::sync::Notify::new(),
            }),
        }
    }

    /// 当前代次。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 本代是否已被要求停止；处理器在长操作之间用它判断是否应提前失败。
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// 取消本代；幂等。停止流程在撤销会话、关闭监听之前调用。
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// 供响应体流式发送使用的取消 future；流在下一块数据处结束。
    pub fn cancelled_owned(&self) -> tokio_util::sync::WaitForCancellationFutureOwned {
        self.cancel.clone().cancelled_owned()
    }

    /// 登记一个在途请求；守卫 drop 时归还计数并在归零时唤醒等待者。
    ///
    /// 计数包含「响应体尚未发送完」的阶段：下载处理器把守卫移进响应体流，
    /// 因此停止流程不会在文件仍在外发时报告已撤销。
    pub fn enter(&self) -> InFlightGuard {
        {
            let mut active = self
                .inflight
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *active = active.saturating_add(1);
        }
        InFlightGuard { run: self.clone() }
    }

    /// 当前在途请求数；只用于诊断与测试断言。
    pub fn active(&self) -> usize {
        *self
            .inflight
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 在绝对截止时间内等待在途请求归零；返回是否确认归零。
    ///
    /// 先 `enable()` 再检查计数，避免「检查为 1 → 请求刚好结束并通知 → 才开始等待」
    /// 这一丢唤醒窗口导致无谓超时。超时后返回当前计数是否为 0，调用方据此决定
    /// 保留句柄还是清空槽位，不能把「没等到」当成「已归零」。
    pub async fn wait_idle(&self, deadline: Instant) -> bool {
        loop {
            let notified = self.inflight.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active() == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.active() == 0;
            }
        }
    }
}

/// 在途请求守卫；drop 即归还计数，客户端断开、panic 或 future 被取消都会走到这里。
pub struct InFlightGuard {
    run: RunHandle,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let remaining = {
            let mut active = self
                .run
                .inflight
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *active = active.saturating_sub(1);
            *active
        };
        if remaining == 0 {
            self.run.inflight.idle.notify_waiters();
        }
    }
}

/// `Host` 头的校验策略，必须与本次实际绑定方式一致。
///
/// 单地址监听沿用逐字比较，允许回环绑定时使用 `localhost`。通配监听则只接受
/// 启动时冻结的本机 IPv4 字面量加实际端口：这样浏览器可以用任意一块本机网卡
/// 打开页面，但 `Host: attacker.example`、`0.0.0.0` 与未分配的地址仍然失败；
/// 运行期不再枚举网卡，避免同一个服务在两次请求之间突然改变放行集合。
#[derive(Clone, Debug)]
pub enum HostPolicy {
    Single(SocketAddr),
    Wildcard { port: u16, allowed: Arc<[Ipv4Addr]> },
}

impl HostPolicy {
    /// 具体地址监听：完全沿用原有的 Host 校验语义。
    pub fn single(bound: SocketAddr) -> Self {
        Self::Single(bound)
    }

    /// 通配监听：只接受冻结的本机 IPv4 集合与实际端口。
    pub fn wildcard(port: u16, allowed: Vec<Ipv4Addr>) -> Self {
        Self::Wildcard {
            port,
            allowed: allowed.into(),
        }
    }

    /// 本次请求的 `Host` 是否被允许；缺少或不可解析的 Host 一律失败关闭。
    fn allows(&self, host: Option<&HeaderValue>) -> bool {
        match self {
            Self::Single(bound) => host_allowed(bound, host),
            Self::Wildcard { port, allowed } => wildcard_host_allowed(*port, allowed, host),
        }
    }
}

/// HTTP 处理器共享的上下文；`root` 在服务运行期间只读，停止时随整个服务一起丢弃。
pub struct AppContext {
    pub state: Arc<State>,
    pub sessions: Sessions,
    pub tickets: Tickets,
    pub root: ShareRoot,
    /// 本次监听的 Host 校验策略；通配监听与具体地址监听在这里分流。
    pub hosts: HostPolicy,
    /// 本代取消信号与在途计数；见 `RunHandle`。
    pub run: RunHandle,
    /// 统计变更信号；HTTP 完成上传/下载后递增，视图监听任务据此推送 `view.update`。
    ///
    /// 用 `watch` 而不是「直接持有视图注册表」：HTTP 层不该依赖 RPC 层，而 `watch`
    /// 只传一个修订号，视图任务自己在唤醒后取快照，不会因为补丁构造失败而丢计数。
    pub stats: watch::Sender<u64>,
    /// 并发上传计数，避免一次请求把磁盘写满。
    pub uploads: AtomicUsize,
    /// 配对尝试的固定窗口计数（窗口起点、次数）；放在上下文里是因为配对接口无状态，
    /// 不能把计数挂在连接上，否则攻击者换连接即可绕过。
    pub pairing: Mutex<(u64, u32)>,
}

impl AppContext {
    /// 是否允许本次配对尝试；窗口与上限由 `auth::attempt_allowed` 统一维护。
    pub fn take_pair_attempt(&self) -> bool {
        match self.pairing.lock() {
            Ok(mut slot) => crate::http::auth::attempt_allowed(&mut slot),
            Err(_) => false,
        }
    }

    /// 记录一次成功提交的上传，并唤醒视图推送统计。
    ///
    /// 计数与广播必须在同一入口完成：只在状态里加一而不通知，面板会一直显示旧数字，
    /// 直到用户下次点击动作才刷新；分开写在多个调用点也容易漏掉其中一处。
    pub fn count_upload(&self) {
        self.state.count_upload();
        self.bump_stats();
    }

    /// 记录一次开始的下载，并唤醒视图推送统计。
    pub fn count_download(&self) {
        self.state.count_download();
        self.bump_stats();
    }

    /// 统计修订号加一；接收端只关心「变过」，不需要具体值。
    fn bump_stats(&self) {
        self.stats.send_modify(|revision| {
            *revision = revision.wrapping_add(1);
        });
    }
}

/// 在已绑定的监听器上启动 HTTP 服务；调用方先绑定并读出真实端口，
/// 这样 `Host`/`Origin` 校验使用的端口与内核实际监听端口必然一致。
///
/// 返回 `JoinHandle<Result<(), AppError>>` 而不是 `JoinHandle<()>`：监听任务自身的
/// 失败必须能被停止流程区分出来。此前用 `let _ = axum::serve(...)` 吞掉返回值，
/// 任务异常结束时状态仍显示运行中，认证也继续有效——用户看到的「正在运行」
/// 和实际不可用是两回事。
pub async fn serve(
    context: Arc<AppContext>,
    listener: TcpListener,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> tokio::task::JoinHandle<Result<(), AppError>> {
    let app = router(Arc::clone(&context));
    tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown.await;
            })
            .await;
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                // 监听失败后先取消本代：仍在运行的连接任务（axum 内部 spawn 后 detach
                // 的那些）会在下一个检查点看到取消信号并自行失败退出，而不是继续
                // 读写磁盘。撤销会话与票据紧随其后，避免「已失败但旧令牌仍可用」。
                context.run.cancel();
                context.sessions.revoke_all();
                context.tickets.clear();
                // 状态只在仍为 `Running` 时落到 `Failed`：若停止流程已经把状态改成
                // `Stopping`，这里再写会覆盖用户已成功的停止。`mark_failed_if_running`
                // 把「检查 + 写入」放在同一把锁内，避免判断与写入之间被插入。
                let message = crate::l10n::Locale::detect()
                    .text(crate::l10n::Key::ErrorInternal)
                    .to_owned();
                let _ = context.state.mark_failed_if_running(message);
                // 错误仍经任务返回值传给控制器：停止流程需要区分「任务内错误」与
                // 「任务被 panic/取消」，不能只依赖已写入的状态。
                eprintln!("lan-transfer http server stopped: {error}");
                Err(AppError::new(ErrorCode::Server, "http server failed"))
            }
        }
    })
}

/// 组装路由。
///
/// 只挂一层中间件：`Host` 校验、状态变更的 `Origin` 校验与安全头都在同一条路径上。
/// axum 的 `layer` 后添加者位于外层，如果分成两层（先安全头、后 Host），
/// Host 拒绝会直接返回而不经过安全头，与「所有响应都带安全头」的承诺矛盾；
/// 单层中间件不存在这种顺序歧义。
pub fn router(context: Arc<AppContext>) -> Router {
    Router::new()
        .route("/", get(assets::index))
        .route("/app.css", get(assets::css))
        .route("/app.js", get(assets::js))
        .route("/api/pair", post(auth::pair))
        .route("/api/session", get(auth::session))
        .route("/api/list", get(files::list))
        .route("/api/upload", post(upload::upload))
        .route("/api/download-ticket", post(download::ticket))
        .route("/download/{token}", get(download::download))
        .layer(middleware::from_fn_with_state(context.clone(), guard))
        .with_state(context)
}

/// 统一请求门禁：先校验 `Host` 与 `Origin`，再交给处理器，最后给所有响应补安全头。
///
/// 门禁放在中间件而不是各处理器里，是因为「每个处理器记得调用 origin 校验」已经被
/// 证明会漏：上传处理器此前只做鉴权，合法令牌加跨站 Origin 仍可写文件。
/// 只读方法（`GET`/`HEAD`）允许缺少 `Origin`（浏览器原生下载不会带），
/// 但存在时同样必须与实际 `Host` 精确同源。
async fn guard(
    AxumState(context): AxumState<Arc<AppContext>>,
    request: Request,
    next: Next,
) -> Response {
    let host = request.headers().get(header::HOST).cloned();
    let origin = request.headers().get(header::ORIGIN).cloned();
    let method = request.method().clone();
    let mut response = if !context.hosts.allows(host.as_ref()) {
        error_response(AppError::new(ErrorCode::BadRequest, "host rejected"))
    } else if !origin_allowed(host.as_ref(), &method, origin.as_ref()) {
        error_response(AppError::new(ErrorCode::Forbidden, "origin rejected"))
    } else if !context.state.is_running() || context.run.is_cancelled() {
        // 运行态门禁集中在中间件而不是各处理器：`auth::pair` 之前没有运行态检查，
        // 停止后的配对请求会创建新会话票据，形成「服务已停、令牌仍可签发」的窗口。
        // 同时检查代次取消信号：`is_running` 是全局字段，重启后立即变回 true，
        // 旧代请求必须只认自己这一代的取消信号，不能被新代「续命」。
        error_response(AppError::new(ErrorCode::Unavailable, "service stopped"))
    } else {
        // 在途计数覆盖整个 handler 执行期；下载响应体的流式发送阶段由处理器把
        // 守卫移进响应体流（见 `download::download`），因此这里返回响应头后守卫
        // 提前释放不会让停止流程把「文件仍在外发」误判为「已回收」。
        let in_flight = context.run.enter();
        let response = next.run(request).await;
        drop(in_flight);
        response
    };
    security_headers(response.headers_mut());
    response
}

/// 统一安全头：禁止被任何页面嵌入（点击劫持），禁止缓存敏感响应，
/// 并关闭 MIME 嗅探与 Referer 泄漏。
///
/// `frame-ancestors` 只能通过 HTTP 响应头生效，HTML `<meta>` 里的同名指令会被浏览器
/// 忽略；因此这里在服务端设置，而不是依赖页面自身声明。
fn security_headers(headers: &mut axum::http::HeaderMap) {
    // 页面已有一份 meta CSP 限制脚本与连接来源；HTTP 头只追加防嵌入与基线限制，
    // 两份策略会同时生效并取交集，这里刻意不重复声明 script-src 等指令，避免两处
    // 维护同一组来源而出现难以定位的冲突。
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'; base-uri 'none'; form-action 'none'"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

/// 精确校验 `Host` 头，拒绝 DNS rebinding 与通过其他主机名访问。
///
/// 按 `IpAddr` 分支构造期望文本：IPv4 是 `ip:port`，IPv6 必须带方括号
/// `[ip]:port`。此前对两种地址都用 `format!("{ip}:{port}")`，IPv6 会得到无方括号的
/// `::1:8080` 并被直接接受——这既不符合 HTTP 的 authority 语法，也让方括号校验
/// 形同虚设。现在 IPv6 只接受带方括号形式。
///
/// 主机名比较不区分大小写：`Host` 是大小写不敏感的 authority，浏览器或用户完全
/// 可能发出 `LOCALHOST:8080`；这里若做逐字比较，会在同源校验之前就误拒合法请求。
/// 只对 `localhost` 这个名字放宽，回环 IP 与绑定地址仍逐字匹配，不引入其他别名。
/// 缺失 `Host` 时按 HTTP/1.1 要求直接拒绝，而不是放行。
fn host_allowed(bound: &SocketAddr, host: Option<&HeaderValue>) -> bool {
    let Some(host) = host.and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let port = bound.port();
    let ip = bound.ip();
    // 期望文本由地址族决定，避免把 IPv6 拼成无方括号的非法 authority。
    let expected = match ip {
        std::net::IpAddr::V4(ip) => format!("{ip}:{port}"),
        std::net::IpAddr::V6(ip) => format!("[{ip}]:{port}"),
    };
    if host == expected {
        return true;
    }
    // 回环访问允许用 localhost，但只在本机绑定时生效；否则会扩大攻击面。
    // 只对 `localhost` 这一个名字做大小写不敏感比较，不把其他别名加进来。
    ip.is_loopback()
        && (host.eq_ignore_ascii_case(&format!("localhost:{port}"))
            || host == format!("127.0.0.1:{port}")
            || host == format!("[::1]:{port}"))
}

/// 通配监听的 Host 校验：只接受启动时冻结的本机 IPv4 字面量与实际端口。
///
/// 不接受 `localhost` 或其他主机名：通配监听覆盖多块网卡，主机名无法证明
/// 请求来自本机，也必须避免 DNS rebinding 把任意域名解析到本机后通过 Host
/// 校验。IPv6 也不在此分支：显式通配请求只针对 IPv4（`0.0.0.0`），IPv6 的
/// 单地址监听仍走 `host_allowed` 的方括号精确匹配。
fn wildcard_host_allowed(port: u16, allowed: &[Ipv4Addr], host: Option<&HeaderValue>) -> bool {
    let Some(host) = host.and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let Ok(address) = host.parse::<SocketAddr>() else {
        return false;
    };
    let SocketAddr::V4(address) = address else {
        return false;
    };
    address.port() == port && allowed.contains(address.ip())
}

/// 状态变更请求必须与实际 `Host` 精确同源；只有 `GET`/`HEAD` 允许缺少 `Origin`，
/// 存在时同样校验。
///
/// 缺省分支用显式只读白名单而不是「排除 POST/PUT/PATCH/DELETE」：路由只注册了
/// `get`/`post`，也没有 CORS 预检处理器，用黑名单会让 `OPTIONS`、`PROPFIND` 之类
/// 未注册方法在缺少 `Origin` 时也穿过门禁，默认放行未列举的方法。白名单把
/// 「未列出的方法」统一按拒绝处理，将来新增方法必须显式修改这里，不会静默继承豁免。
///
/// 关键点是比较「本请求的 Host」而不是「绑定地址的所有别名」：`Host: 127.0.0.1:P`
/// 配 `Origin: http://localhost:P` 虽然都指向本机，却属于不同源，按同源策略必须拒绝。
/// 这样用户仍可以用任一已绑定别名打开页面，但每个请求内部必须自洽。
///
/// `Host`/`Origin` 只要有一侧无法解析为可见 ASCII 文本就拒绝（`to_str` 失败即
/// 非 UTF-8/含不可见字节）：这类值不可能是浏览器给出的同源 Origin，也无法与
/// 已校验的 `Host` 做可信比较。此前把 `Option<Result<&str>>` 当成 `Option<&str>`
/// 解构，编译期就暴露了类型错误；修复时必须保持「解析失败 = 失败关闭」，不能
/// 用 `unwrap_or_default` 之类回退成空串，否则空串比较可能把畸形头放行。
pub fn origin_allowed(
    host: Option<&HeaderValue>,
    method: &Method,
    origin: Option<&HeaderValue>,
) -> bool {
    let Some(origin) = origin else {
        // 缺少 Origin 只对只读方法放行；它们仍须票据或 Bearer 鉴权。
        return matches!(*method, Method::GET | Method::HEAD);
    };
    let (Ok(origin), Some(Ok(host))) = (origin.to_str(), host.map(HeaderValue::to_str)) else {
        return false;
    };
    // 只接受 `http://` 且不带路径、查询、用户信息；出现这些成分说明不是浏览器给出的
    // 简单同源 Origin，直接拒绝。
    let Some(authority) = origin.strip_prefix("http://") else {
        return false;
    };
    if authority.is_empty()
        || authority.contains(['/', '?', '#', '@'])
        || authority.chars().any(char::is_control)
    {
        return false;
    }
    // Host 头大小写不敏感；端口必须逐字相同（`Host` 已由 host_allowed 校验过端口）。
    authority.eq_ignore_ascii_case(host)
}

/// 错误响应统一序列化为 `{error:{code,message}}`；不包含路径、令牌或系统错误原文。
pub fn error_response(error: AppError) -> Response {
    let status =
        StatusCode::from_u16(error.code.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, axum::Json(error.body())).into_response()
}

/// JSON 成功响应；显式设置内容类型，避免被当作可嗅探的未知类型。
pub fn json_response<T: serde::Serialize>(value: T) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        axum::Json(value),
    )
        .into_response()
}

/// 计数型并发限制：超过上限返回 `busy`，不排队占用内存。
pub fn try_acquire_upload(context: &AppContext) -> Result<UploadPermit<'_>, AppError> {
    let current = context.uploads.fetch_add(1, Ordering::AcqRel);
    if current >= crate::config::MAX_CONCURRENT_UPLOADS {
        context.uploads.fetch_sub(1, Ordering::AcqRel);
        return Err(AppError::new(ErrorCode::Busy, "upload limit"));
    }
    Ok(UploadPermit { context })
}

/// 上传并发占位的 RAII 守卫；请求取消或 panic 都会归还计数。
pub struct UploadPermit<'a> {
    context: &'a AppContext,
}

impl Drop for UploadPermit<'_> {
    fn drop(&mut self) {
        self.context.uploads.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 统一给下载响应设置附件头；文件名只允许由服务端路径最后一段派生。
pub fn attachment_header(name: &str) -> HeaderValue {
    // 只保留安全字符，杜绝头注入；非 ASCII 名通过 RFC 5987 编码传递。
    let fallback: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let encoded = percent_encode(name);
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// RFC 5987 百分号编码；只保留非保留字符，其余按 UTF-8 字节编码。
fn percent_encode(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            output.push(*byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    /// 单地址策略的测试入口；封装构造，避免每个断言重复写枚举分支。
    fn single_allows(bound: &SocketAddr, host: Option<&HeaderValue>) -> bool {
        HostPolicy::single(*bound).allows(host)
    }

    /// 回环绑定允许 `localhost` 与三种回环写法；端口必须精确匹配，
    /// 否则同一主机上的其他服务端口也能访问本服务。
    #[test]
    fn loopback_allows_localhost_only_on_loopback_bindings() {
        let loopback: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        for host in ["127.0.0.1:8080", "localhost:8080", "[::1]:8080"] {
            assert!(
                single_allows(&loopback, Some(&HeaderValue::from_static(host))),
                "{host}"
            );
        }
        for host in ["127.0.0.1:8081", "localhost:8081", "example.com:8080", ""] {
            let value = HeaderValue::from_str(host).unwrap();
            assert!(!single_allows(&loopback, Some(&value)), "{host}");
        }
        // 缺少 Host 头必须拒绝，而不是当成「本机访问」放行。
        assert!(!single_allows(&loopback, None));
        // Host 是大小写不敏感的 authority：浏览器可能发出 `LOCALHOST:port`，
        // 不能因为逐字比较在第一层就误拒合法请求（同源层本来就按不区分大小写比较）。
        assert!(single_allows(
            &loopback,
            Some(&HeaderValue::from_static("LOCALHOST:8080"))
        ));
        assert!(single_allows(
            &loopback,
            Some(&HeaderValue::from_static("LocalHost:8080"))
        ));
        // 大小写放宽只针对 `localhost`，不能把其他主机名也变成等价别名。
        assert!(!single_allows(
            &loopback,
            Some(&HeaderValue::from_static("LOCALHOST.LOCAL:8080"))
        ));
    }

    /// 绑定到局域网地址时不接受 `localhost`：否则同网段的攻击者可以用
    /// `localhost` 触发 DNS rebinding 之外的别名访问。
    #[test]
    fn lan_binding_rejects_loopback_aliases() {
        let lan: SocketAddr = "192.168.1.10:8080".parse().unwrap();
        assert!(single_allows(
            &lan,
            Some(&HeaderValue::from_static("192.168.1.10:8080"))
        ));
        for host in ["localhost:8080", "127.0.0.1:8080", "[::1]:8080"] {
            let value = HeaderValue::from_str(host).unwrap();
            assert!(!single_allows(&lan, Some(&value)), "{host}");
        }
    }

    /// IPv6 绑定必须用方括号形式匹配；不带方括号的写法不能被当成同一主机。
    ///
    /// 这条用例曾与实现矛盾：旧代码用 `format!("{ip}:{port}")` 生成期望值，
    /// 对 IPv6 得到无方括号的 `::1:8080` 并直接接受，测试因此必失败。
    /// 现在期望文本按地址族构造，IPv6 只接受 `[ip]:port`。
    #[test]
    fn ipv6_host_requires_brackets() {
        let bound: SocketAddr = "[::1]:8080".parse().unwrap();
        assert!(single_allows(
            &bound,
            Some(&HeaderValue::from_static("[::1]:8080"))
        ));
        assert!(!single_allows(
            &bound,
            Some(&HeaderValue::from_static("::1:8080"))
        ));
        // 端口不一致同样拒绝：方括号不能替代端口校验。
        assert!(!single_allows(
            &bound,
            Some(&HeaderValue::from_static("[::1]:8081"))
        ));
        assert_eq!(bound.ip(), IpAddr::V6("::1".parse().unwrap()));

        // 非回环 IPv6（局域网绑定）只接受自身带方括号形式，不接受 localhost。
        let lan: SocketAddr = "[fd00::1]:8080".parse().unwrap();
        assert!(single_allows(
            &lan,
            Some(&HeaderValue::from_static("[fd00::1]:8080"))
        ));
        assert!(!single_allows(
            &lan,
            Some(&HeaderValue::from_static("fd00::1:8080"))
        ));
        assert!(!single_allows(
            &lan,
            Some(&HeaderValue::from_static("localhost:8080"))
        ));
    }

    /// 通配监听只接受启动时冻结的本机 IPv4 与实际端口；`0.0.0.0`、主机名、
    /// 其他设备地址和错误端口都必须失败，避免把「监听所有网卡」变成放行所有 Host。
    #[test]
    fn wildcard_host_accepts_only_frozen_local_ipv4_with_exact_port() {
        let policy = HostPolicy::wildcard(
            8080,
            vec![Ipv4Addr::LOCALHOST, Ipv4Addr::new(192, 168, 1, 10)],
        );
        for host in ["192.168.1.10:8080", "127.0.0.1:8080"] {
            assert!(
                policy.allows(Some(&HeaderValue::from_static(host))),
                "{host}"
            );
        }
        for host in [
            "192.168.1.10:8081",
            "192.168.1.11:8080",
            "0.0.0.0:8080",
            "localhost:8080",
            "attacker.example:8080",
            "[::1]:8080",
            "",
        ] {
            let value = HeaderValue::from_str(host).unwrap();
            assert!(!policy.allows(Some(&value)), "{host}");
        }
        // 缺少 Host 头没有可比对的 authority，必须失败关闭。
        assert!(!policy.allows(None));
    }

    /// 通配监听的同源校验仍以本次请求的 Host 为准：Origin 必须逐字匹配
    /// 已通过白名单的 Host，不能用另一块本机网卡或任意域名替代。
    #[test]
    fn wildcard_origin_must_match_the_validated_host() {
        let host = HeaderValue::from_static("192.168.1.10:8080");
        assert!(
            HostPolicy::wildcard(
                8080,
                vec![Ipv4Addr::LOCALHOST, Ipv4Addr::new(192, 168, 1, 10)]
            )
            .allows(Some(&host))
        );
        assert!(origin_allowed(
            Some(&host),
            &Method::POST,
            Some(&HeaderValue::from_static("http://192.168.1.10:8080"))
        ));
        assert!(!origin_allowed(
            Some(&host),
            &Method::POST,
            Some(&HeaderValue::from_static("http://127.0.0.1:8080"))
        ));
        assert!(!origin_allowed(
            Some(&host),
            &Method::POST,
            Some(&HeaderValue::from_static("http://attacker.example:8080"))
        ));
        // 状态变更请求缺少 Origin 同样拒绝。
        assert!(!origin_allowed(Some(&host), &Method::POST, None));
    }

    /// 状态变更方法缺少 Origin 一律拒绝；只读方法允许缺少，但存在时同样要精确同源。
    #[test]
    fn origin_rules_split_by_method() {
        let host = HeaderValue::from_static("127.0.0.1:8080");
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(!origin_allowed(Some(&host), &method, None), "{method}");
        }
        assert!(origin_allowed(Some(&host), &Method::GET, None));
        assert!(origin_allowed(Some(&host), &Method::HEAD, None));

        let good = HeaderValue::from_static("http://127.0.0.1:8080");
        let bad = HeaderValue::from_static("http://127.0.0.1:8081");
        assert!(origin_allowed(Some(&host), &Method::GET, Some(&good)));
        assert!(!origin_allowed(Some(&host), &Method::GET, Some(&bad)));
        // `https` 与 `http` 不同源，即便主机端口一致也必须拒绝。
        assert!(!origin_allowed(
            Some(&host),
            &Method::POST,
            Some(&HeaderValue::from_static("https://127.0.0.1:8080"))
        ));
        // 带路径、查询或用户信息的 Origin 不是浏览器给出的同源值，一律拒绝。
        for origin in [
            "http://127.0.0.1:8080/",
            "http://127.0.0.1:8080/?x=1",
            "http://user@127.0.0.1:8080",
        ] {
            assert!(
                !origin_allowed(
                    Some(&host),
                    &Method::POST,
                    Some(&HeaderValue::from_str(origin).unwrap())
                ),
                "{origin}"
            );
        }
    }

    /// 同源比较必须针对本次请求的 `Host`，而不是绑定地址的所有别名：
    /// `Host: 127.0.0.1:P` 配 `Origin: http://localhost:P` 虽然都指向本机，
    /// 但属于不同源，按同源策略必须拒绝；反过来也一样。
    #[test]
    fn origin_must_match_the_request_host_not_any_alias() {
        let ip_host = HeaderValue::from_static("127.0.0.1:8080");
        let localhost_host = HeaderValue::from_static("localhost:8080");
        let ip_origin = HeaderValue::from_static("http://127.0.0.1:8080");
        let localhost_origin = HeaderValue::from_static("http://localhost:8080");

        assert!(origin_allowed(
            Some(&ip_host),
            &Method::POST,
            Some(&ip_origin)
        ));
        assert!(origin_allowed(
            Some(&localhost_host),
            &Method::POST,
            Some(&localhost_origin)
        ));
        assert!(!origin_allowed(
            Some(&ip_host),
            &Method::POST,
            Some(&localhost_origin)
        ));
        assert!(!origin_allowed(
            Some(&localhost_host),
            &Method::POST,
            Some(&ip_origin)
        ));
        // Host 大小写不敏感，但端口必须一致。
        assert!(origin_allowed(
            Some(&HeaderValue::from_static("LOCALHOST:8080")),
            &Method::POST,
            Some(&localhost_origin)
        ));
        assert!(!origin_allowed(
            Some(&HeaderValue::from_static("localhost:8081")),
            &Method::POST,
            Some(&localhost_origin)
        ));
        // 缺少 Host 时没有任何可比对的源，必须拒绝。
        assert!(!origin_allowed(None, &Method::POST, Some(&ip_origin)));
    }

    /// IPv6 绑定必须用方括号形式，且 Origin 与 Host 的方括号写法要一致。
    #[test]
    fn ipv6_origin_requires_matching_brackets() {
        let host = HeaderValue::from_static("[::1]:8080");
        assert!(origin_allowed(
            Some(&host),
            &Method::POST,
            Some(&HeaderValue::from_static("http://[::1]:8080"))
        ));
        assert!(!origin_allowed(
            Some(&host),
            &Method::POST,
            Some(&HeaderValue::from_static("http://::1:8080"))
        ));
    }

    /// 附件头只保留安全字符并给出 RFC 5987 编码；中文名不能破坏头部语法。
    #[test]
    fn attachment_header_escapes_names() {
        let value = attachment_header("报告 \"2026\".txt");
        let text = value.to_str().unwrap();
        assert!(text.starts_with("attachment;"));
        // 只有包裹 fallback 文件名的那一对引号；名字内部的引号必须被替换掉。
        assert_eq!(text.matches('"').count(), 2);
        assert!(text.contains("filename*=UTF-8''"));
        // 非 ASCII 字符必须被百分号编码，不能原样写进头部。
        assert!(text.is_ascii());
        // 头部注入字符（CR/LF）不能出现在结果里。
        assert!(!text.contains('\r') && !text.contains('\n'));
    }
}
