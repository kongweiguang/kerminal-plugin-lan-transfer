//! Sidecar 入口装配：协议路由、视图方法注册与进程级收尾。
//! @author kongweiguang
//!
//! 单独成模块而不是留在 `main.rs`，是为了让集成测试能用内存管道驱动真实的握手、
//! 动作与关闭流程；二进制入口只负责把标准输入输出接进来。
//!
//! 协议保留方法 `initialize`/`initialized`/`shutdown` 由 SDK 自己处理：注册它们会让
//! `Router::method` 直接返回 `invalid or duplicate method`，进程在握手前就退出。
//! SDK 的 `shutdown` 只负责回复宿主并结束读取循环，不会关闭插件自己的监听端口，
//! 因此回收必须由本模块在循环退出后执行。

use std::{io, sync::Arc};

use kerminal_plugin_sdk::{Context, Error, Router};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;

use crate::{
    config::{PLUGIN_ID, PLUGIN_VERSION},
    rpc::ViewRegistry,
    service::Controller,
};

/// 视图方法收到的完整调用：SDK 上下文与参数分开保存，避免每个闭包重复声明参数顺序。
/// 上下文必须透传：复制链接要经它向宿主发起 `clipboard.write`，否则动作会静默降级成
/// 「只更新界面、不写剪贴板」。
struct Call {
    context: Context,
    params: Value,
}

/// 组装路由；只注册视图方法。注册期错误立即返回，让进程在握手前失败，
/// 而不是等到某个动作被调用时才暴露。
pub fn build_router(views: Arc<ViewRegistry>) -> io::Result<Router> {
    let mut router = Router::new(PLUGIN_ID, PLUGIN_VERSION);
    register(
        &mut router,
        views.clone(),
        "view.open",
        move |views, call, cancel| async move { views.open(call.params, cancel).await },
    )?;
    register(
        &mut router,
        views.clone(),
        "view.action",
        move |views, call, cancel| async move { views.action(&call.context, call.params, cancel).await },
    )?;
    register(
        &mut router,
        views.clone(),
        "view.snapshot",
        move |views, call, _cancel| async move {
            let _ = call;
            views.snapshot_state()
        },
    )?;
    register(
        &mut router,
        views,
        "view.close",
        move |views, call, _cancel| async move { views.close(call.params).await },
    )?;
    Ok(router)
}

/// 在给定流上运行协议循环，并在循环结束后真正回收服务。
///
/// SDK 对 `shutdown` 的语义只是「取消进行中的请求、回复宿主并结束读取」，并不了解
/// 插件自己的监听端口与临时状态；宿主收到回复后只留约两秒宽限期等待进程退出。
/// 因此回收必须紧跟循环结束执行，且不能把「已回复」当成「已回收」：
/// 只有循环正常结束且 `controller.shutdown()` 确认完成时才返回 `Ok`。
/// 参数取 `&Arc<Controller>` 是因为视图注册表需要共享所有权，而调用方本就用 `Arc` 持有控制器。
pub async fn serve<R, W>(controller: &Arc<Controller>, input: R, output: W) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // 视图注册表需要独立所有权（协议回调在 SDK 内部并发执行），因此这里克隆控制器
    // 而不是借用：控制器的内部状态本身就是共享的，克隆只增加引用计数。
    let views = Arc::new(ViewRegistry::new(Arc::clone(controller)));
    let router = build_router(views)?;
    let result = router.run_with_io(input, output).await;
    // 正常 shutdown、输入 EOF、协议错误与写端关闭都会走到这里；四条路径都必须尝试回收，
    // 否则停止后仍可能留下监听端口或未撤销的会话。
    let reclaimed = controller.shutdown().await;
    combine_outcome(result, reclaimed)
}

/// 把「协议循环结果」与「服务是否确认回收」合并成进程退出语义。
///
/// 拆成独立函数是为了让四条分支都能被直接断言：超时只在真实监听任务不响应时出现，
/// 无法在集成测试里稳定复现，因此这里单独覆盖映射，而不是假装跑过超时场景。
pub fn combine_outcome(result: io::Result<()>, reclaimed: bool) -> io::Result<()> {
    match (result, reclaimed) {
        (Ok(()), true) => Ok(()),
        (Ok(()), false) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "shutdown timed out",
        )),
        (Err(error), true) => Err(error),
        // 两条失败原因都要保留：宿主需要知道协议已失败，也不能误以为资源已回收。
        (Err(error), false) => Err(io::Error::new(
            error.kind(),
            format!("{error}; shutdown timed out"),
        )),
    }
}

/// 统一注册视图方法；注册期错误直接让进程启动失败，而不是运行到某个动作才暴露。
fn register<F, Fut>(
    router: &mut Router,
    views: Arc<ViewRegistry>,
    method: &'static str,
    handler: F,
) -> io::Result<()>
where
    F: Fn(Arc<ViewRegistry>, Call, CancellationToken) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Value, Error>> + Send + 'static,
{
    router
        .method(method, move |context, params, cancel| {
            let views = Arc::clone(&views);
            handler(views, Call { context, params }, cancel)
        })
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.message))
}
