//! 下载票据与票据下载。
//! @author kongweiguang
//!
//! 票据只在 POST 时用 Bearer 签发；随后浏览器用同源 URL 直接下载，不带 Authorization，
//! 这样大文件由浏览器原生下载、可显示进度，也不会把令牌写进下载历史。
//! 票据本身绑定单个文件、当前服务代次和短期到期时间，不能用于其他接口。

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
// `take_until` 由 StreamExt 提供：下载响应体必须在本代取消时结束，缺少该 trait
// 方法会直接编译失败，而不是在停止时留下继续外发的流。
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio_util::io::ReaderStream;

use crate::{
    error::{AppError, ErrorCode},
    http::{AppContext, attachment_header, auth::authenticate, error_response, json_response},
    path::VirtualPath,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TicketRequest {
    #[serde(rename = "fileId")]
    file_id: String,
}

#[derive(Serialize)]
pub struct TicketResponse {
    url: String,
}

/// `POST /api/download-ticket`：把文件 id 换成短期票据 URL。
///
/// 同源校验由中间件统一完成；处理器只负责鉴权、路径校验与签发票据。
pub async fn ticket(
    State(context): State<Arc<AppContext>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(error) = authenticate(&context, &headers) {
        return error_response(error);
    }
    let request: TicketRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(_) => return error_response(AppError::new(ErrorCode::BadRequest, "invalid body")),
    };
    let path = match VirtualPath::parse(&request.file_id) {
        Ok(path) => path,
        Err(error) => return error_response(error),
    };
    // 只为真实存在的普通文件签发票据；否则票据会把「探测文件是否存在」变成离线能力。
    if let Err(error) = context.root.open_file(&path) {
        return error_response(remap_file_error(error));
    }
    match context.tickets.issue(&path) {
        Ok((token, _)) => json_response(TicketResponse {
            url: format!("/download/{token}"),
        }),
        Err(error) => error_response(error),
    }
}

/// `GET /download/{token}`：校验票据后流式发送文件。
///
/// 支持标准单区间 Range（浏览器兼容的最低要求）：`bytes=start-end`、`bytes=start-`、
/// `bytes=-suffix` 返回 206 与 `Content-Range`；越界起点返回 416 与 `bytes */size`；
/// 语法非法、多区间或未知单位按 RFC 7233 允许的方式忽略并整份返回 200。HEAD 由
/// axum 的 GET 路由兜底，头部与状态和 GET 一致、响应体被剥掉。只承诺单次请求内的
/// 浏览器续传，不承诺跨会话断点续传；票据失效后由用户重新获取票据从头下载。
pub async fn download(
    State(context): State<Arc<AppContext>>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response {
    // 停止期间（`Stopping`）也要立刻拒绝：停止流程会先撤销会话与票据再关闭监听，
    // 但已经建立的连接可能仍在处理中；只把 `Running` 视为可服务，才能保证
    // 「停止成功后旧票据不能再用」不是靠时序侥幸成立。
    if !context.state.is_running() || context.run.is_cancelled() {
        return error_response(AppError::new(ErrorCode::Unavailable, "service stopped"));
    }
    let path = match context
        .tickets
        .resolve(&token, context.sessions.generation())
    {
        Ok(path) => path,
        Err(error) => return error_response(error),
    };
    let opened = match context.root.open_file(&path) {
        Ok(opened) => opened,
        Err(error) => return error_response(remap_file_error(error)),
    };
    let name = path.display_name().unwrap_or("download").to_owned();
    let size = opened.size;
    let range = parse_range(&headers, size);
    // 越界区间必须明确 416：如果仍返回 200 整份内容，下载器会按「续传成功」把
    // 整份字节追加到已下载部分，拼出损坏文件。`Content-Range: bytes */size`
    // 是通知客户端「当前长度下该区间不可满足」的标准方式。
    if matches!(range, RangeRequest::Unsatisfiable) {
        let mut response = (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{size}"))
                    .unwrap_or(HeaderValue::from_static("bytes */0")),
            )],
            Body::empty(),
        )
            .into_response();
        response
            .headers_mut()
            .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, attachment_header(&name));
        return response;
    }
    let (status, offset, length, content_range) = match range {
        RangeRequest::Full => (StatusCode::OK, 0u64, size, None),
        RangeRequest::Partial { start, end } => (
            StatusCode::PARTIAL_CONTENT,
            start,
            end.saturating_sub(start).saturating_add(1),
            Some(format!("bytes {start}-{end}/{size}")),
        ),
        RangeRequest::Unsatisfiable => unreachable!("handled above"),
    };
    // 已打开的同步句柄交给 tokio 包装，避免重新按路径打开（那会引入新的竞态窗口）。
    let mut file = tokio::fs::File::from_std(opened.file);
    // 区间起点通过同一句柄 seek 得到：不重新打开文件，避免路径被替换后偏移
    // 落到另一个对象上。seek 失败按内部错误返回，不静默发送错位字节。
    if offset != 0 && let Err(error) = file.seek(SeekFrom::Start(offset)).await {
        return error_response(AppError::io(error));
    }
    // 按本次响应的字节长度截断：Range 只 seek 到起点而不限制读取会把文件剩余部分
    // 全部发出，与 Content-Length 不符，客户端会拼出损坏文件。这里用 AsyncReadExt
    // 的 `take`（按字节）而不是 StreamExt 的 `take`（按块计数）。
    let file = file.take(length);
    // 在途守卫移进响应体流：axum 在处理器返回响应头之后才开始流式发送文件，
    // 守卫若留在这里会在发送开始前就被 drop，停止流程会把「文件仍在外发」
    // 误判成「请求已结束」。守卫随流一起 drop（完成、取消或客户端断开都会触发）。
    let in_flight = context.run.enter();
    let inner = ReaderStream::new(file).take_until(context.run.cancelled_owned());
    let stream = GuardedStream {
        inner: Box::pin(inner),
        _in_flight: in_flight,
    };
    context.state.count_download();
    let mut response = (
        status,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            ),
            (
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&length.to_string())
                    .unwrap_or(HeaderValue::from_static("0")),
            ),
            (header::ACCEPT_RANGES, HeaderValue::from_static("bytes")),
        ],
        Body::from_stream(stream),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, attachment_header(&name));
    if let Some(value) = content_range {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&value).unwrap_or(HeaderValue::from_static("bytes 0-0/0")),
        );
    }
    response
}

/// 单区间请求的解析结果。
enum RangeRequest {
    /// 没有 Range 头、语法非法或使用了不支持的多区间/未知单位：忽略并整份返回。
    /// RFC 7233 允许服务端忽略不可理解或非法的 Range，这比猜测客户端意图更安全。
    Full,
    /// 可满足的闭区间，`end` 已按文件长度收敛。
    Partial { start: u64, end: u64 },
    /// 起点不小于文件长度（或空文件的后缀请求）：必须返回 416。
    Unsatisfiable,
}

/// 解析 `Range: bytes=...`；只接受单区间，浏览器与下载器实际只用这种形式。
///
/// 后缀区间（`bytes=-N`）按「最后 N 字节」解释：`N >= size` 时退化为整份文件，
/// `N == 0` 视为不可满足（RFC 7233 明确 0 长度的 suffix-range 不可满足）。
/// 空文件的任何区间都不可满足。非法语法（缺 `-`、起点大于终点、非数字）一律
/// 忽略而不是 416：客户端可能只是带了一个我们不该解释的头，误判成 416 会让
/// 它把一次本可成功的下载当成失败。
fn parse_range(headers: &HeaderMap, size: u64) -> RangeRequest {
    let Some(value) = headers.get(header::RANGE) else {
        return RangeRequest::Full;
    };
    // 非 ASCII 的头无法按字节区间解释，按非法忽略。
    let Ok(text) = value.to_str() else {
        return RangeRequest::Full;
    };
    let Some(spec) = text.strip_prefix("bytes=") else {
        return RangeRequest::Full;
    };
    if spec.is_empty() || spec.contains(',') {
        // 多区间需要 multipart/byteranges 响应，浏览器下载不会使用；整份返回
        // 仍满足「内容正确」，且避免实现只覆盖一半的 multipart 边界。
        return RangeRequest::Full;
    }
    let Some((start, end)) = spec.split_once('-') else {
        return RangeRequest::Full;
    };
    if start.is_empty() {
        // 后缀区间：`bytes=-N`。
        let Ok(suffix) = end.parse::<u64>() else {
            return RangeRequest::Full;
        };
        if suffix == 0 || size == 0 {
            return RangeRequest::Unsatisfiable;
        }
        return RangeRequest::Partial {
            start: size.saturating_sub(suffix),
            end: size - 1,
        };
    }
    let Ok(start) = start.parse::<u64>() else {
        return RangeRequest::Full;
    };
    if start >= size {
        return RangeRequest::Unsatisfiable;
    }
    if end.is_empty() {
        return RangeRequest::Partial {
            start,
            end: size - 1,
        };
    }
    let Ok(end) = end.parse::<u64>() else {
        return RangeRequest::Full;
    };
    if end < start {
        // `bytes=7-4` 是非法语法；按 RFC 允许的方式忽略，不当成「空区间」。
        return RangeRequest::Full;
    }
    RangeRequest::Partial {
        start,
        // 客户端请求超出文件末尾时，服务端按实际长度截断（RFC 7233）。
        end: end.min(size - 1),
    }
}

/// 文件不存在与被拒绝要分开：前者用户可刷新，后者说明请求越界。
fn remap_file_error(error: AppError) -> AppError {
    match error.code {
        ErrorCode::NotFound => AppError::new(ErrorCode::NotFound, "file not found"),
        _ => error,
    }
}

/// 下载响应体流：正常转发文件内容，停止（本代取消）时立即结束。
///
/// 组合方式：`take_until(cancelled_owned())` 让取消信号到来后的下一次 poll 直接
/// 结束流（axum 会向客户端截断响应）；`InFlightGuard` 挂在同一结构里，保证
/// 「流仍在发送」与「在途计数非零」严格同步，停止流程的 `wait_idle` 不会在
/// 文件外发中途拿到假的归零信号。
///
/// 内层用 `Box::pin` 保存：`take_until` 内部会钉住取消 future，直接放进字段后
/// 再手工投影既容易把 `Pin` 约束写错，也无法在字段上安全地取 `&mut`。装箱只发生
/// 一次，换来的是无 unsafe 的 `Stream` 实现；`GuardedStream` 因此保持 `Unpin`，
/// 可以直接在 `Pin<&mut Self>` 上访问字段。
struct GuardedStream {
    inner: std::pin::Pin<
        Box<dyn Stream<Item = std::io::Result<axum::body::Bytes>> + Send + 'static>,
    >,
    _in_flight: crate::http::InFlightGuard,
}

impl Stream for GuardedStream {
    type Item = std::io::Result<axum::body::Bytes>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // 内层已经封装了「文件结束 / 取消」两种终止条件，这里只做一层转发。
        self.inner.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 `Range` 头并解析；返回类型化结果便于逐项断言。
    fn range(value: &str, size: u64) -> RangeRequest {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_str(value).unwrap());
        parse_range(&headers, size)
    }

    /// 单区间三种写法与边界收敛：闭区间、开放结尾、后缀，以及超出末尾的截断。
    ///
    /// 这些是浏览器与下载器实际会发出的形式；只要有一项解释错误，续传就会拼出
    /// 错误字节，所以逐项固定而不是只测一个例子。
    #[test]
    fn parses_browser_single_ranges() {
        assert!(matches!(
            range("bytes=4-7", 16),
            RangeRequest::Partial { start: 4, end: 7 }
        ));
        assert!(matches!(
            range("bytes=4-", 16),
            RangeRequest::Partial { start: 4, end: 15 }
        ));
        // 末尾超出实际长度：按 RFC 截断到 size - 1，而不是 416。
        assert!(matches!(
            range("bytes=4-100", 16),
            RangeRequest::Partial { start: 4, end: 15 }
        ));
        // 后缀区间：最后 4 字节。
        assert!(matches!(
            range("bytes=-4", 16),
            RangeRequest::Partial { start: 12, end: 15 }
        ));
        // 后缀长度不小于文件：整份文件。
        assert!(matches!(
            range("bytes=-100", 16),
            RangeRequest::Partial { start: 0, end: 15 }
        ));
        assert!(matches!(
            range("bytes=0-0", 16),
            RangeRequest::Partial { start: 0, end: 0 }
        ));
    }

    /// 不可满足区间：起点不小于长度、空文件、零长度后缀。
    #[test]
    fn flags_unsatisfiable_ranges() {
        assert!(matches!(range("bytes=16-", 16), RangeRequest::Unsatisfiable));
        assert!(matches!(
            range("bytes=99-100", 16),
            RangeRequest::Unsatisfiable
        ));
        assert!(matches!(range("bytes=-0", 16), RangeRequest::Unsatisfiable));
        assert!(matches!(range("bytes=0-", 0), RangeRequest::Unsatisfiable));
    }

    /// 无法安全解释的请求按 RFC 允许的方式忽略，返回完整内容而不是 416：
    /// 误判成 416 会让一次本可成功的下载被客户端当作失败重试。
    #[test]
    fn ignores_unusable_range_headers() {
        for value in [
            "items=0-1",     // 未知单位
            "bytes=",        // 空集合
            "bytes=abc-def", // 非数字
            "bytes=7-4",     // 起点大于终点
            "bytes=0-1,4-5", // 多区间：需要 multipart，浏览器下载不会用
            "bytes=-",       // 缺少长度
        ] {
            assert!(
                matches!(range(value, 16), RangeRequest::Full),
                "{value} must be ignored"
            );
        }
        // 没有 Range 头也是整份返回。
        assert!(matches!(parse_range(&HeaderMap::new(), 16), RangeRequest::Full));
        // 非 ASCII 头无法解释，同样忽略。
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_bytes(b"bytes=\xff").unwrap());
        assert!(matches!(parse_range(&headers, 16), RangeRequest::Full));
    }
}
