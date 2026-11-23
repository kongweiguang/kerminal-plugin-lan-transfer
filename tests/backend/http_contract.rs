//! HTTP 契约回归：安全头、Host/Origin、鉴权、路径拒绝、上传不覆盖与票据边界。
//! @author kongweiguang
//!
//! 这些测试直接驱动真实路由（不启动监听），因此能覆盖中间件顺序与响应头，
//! 而不依赖真实浏览器；真实浏览器与真实局域网验收另行记录。

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};

use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use kerminal_plugin_lan_transfer::{
    fs_root::ShareRoot,
    http::{AppContext, HostPolicy, RunHandle, router},
    session::Sessions,
    state::State,
    ticket::Tickets,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

/// 测试夹具：独立临时共享根 + 已标记运行的状态，绝不触碰真实配置或用户目录。
///
/// 保留 `context` 是为了让「服务已停止」这类状态变更测试直接驱动同一份上下文，
/// 而不必为了改状态再启动一个真实监听端口。
struct Fixture {
    app: axum::Router,
    context: Arc<AppContext>,
    bound: SocketAddr,
    secret: String,
    _root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let bound: SocketAddr = "127.0.0.1:43210".parse().unwrap();
        Self::build(bound, HostPolicy::single(bound))
    }

    /// 通配夹具：只替换 Host 策略，不真正绑定 `0.0.0.0`。
    ///
    /// 自动测试若真的监听所有网卡，会短暂把临时共享暴露到局域网，并可能触发
    /// 开发机防火墙提示；这里用同一套中间件与夹具验证白名单语义，真实内核
    /// HTTP 由使用回环监听器的用例单独覆盖。
    fn wildcard() -> Self {
        let bound: SocketAddr = "192.168.1.10:43210".parse().unwrap();
        Self::wildcard_for(bound)
    }

    /// 通配策略夹具；`bound` 是请求使用的 Host 与状态展示地址，端口必须与
    /// 白名单端口一致，才能复现真实启动时的精确端口校验。
    fn wildcard_for(bound: SocketAddr) -> Self {
        let allowed = vec![Ipv4Addr::LOCALHOST, Ipv4Addr::new(192, 168, 1, 10)];
        Self::build(bound, HostPolicy::wildcard(bound.port(), allowed))
    }

    fn build(bound: SocketAddr, hosts: HostPolicy) -> Self {
        let root = TempDir::new().unwrap();
        // 共享根名字不能以点开头（隐藏目录按规则拒绝），因此使用临时目录下的显式子目录。
        let share = root.path().join("share");
        std::fs::create_dir(&share).unwrap();
        let state = State::new();
        let (sessions, secret) = Sessions::start(1).unwrap();
        // `mark_running` 只接受 `Starting` 来源，先走一次真实启动转换；
        // 否则夹具会依赖一条生产路径不存在（也不该存在）的状态跳转。
        state.begin_start().unwrap();
        state
            .mark_running(
                bound,
                format!("http://{bound}/#{secret}"),
                "share".into(),
                String::new(),
                4_000_000_000,
            )
            .unwrap();
        let (stats, _receiver) = tokio::sync::watch::channel(0u64);
        let context = Arc::new(AppContext {
            state,
            sessions,
            tickets: Tickets::new(1),
            root: ShareRoot::open(&share).unwrap(),
            hosts,
            // 单代次、未取消：夹具直接用同一份上下文驱动停止路径。
            run: RunHandle::new(1),
            stats,
            uploads: std::sync::atomic::AtomicUsize::new(0),
            pairing: std::sync::Mutex::new((0, 0)),
        });
        Self {
            app: router(Arc::clone(&context)),
            context,
            bound,
            secret,
            _root: root,
        }
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, headers, body)
    }

    /// 获取有效 Bearer 令牌；配对必须带精确同源 Origin。
    async fn token(&self) -> String {
        let body = format!("{{\"secret\":\"{}\"}}", self.secret);
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/pair")
            .header(header::HOST, self.bound.to_string())
            .header(header::ORIGIN, format!("http://{}", self.bound))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, _, bytes) = self.send(request).await;
        assert_eq!(status, StatusCode::OK, "pairing must succeed");
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["token"].as_str().unwrap().to_owned()
    }

    /// 上传一个文件并换取票据 URL；HEAD/Range/停止用例都需要「已存在且可下载」的文件，
    /// 抽出来避免每个用例重复三段请求拼装。
    async fn upload_and_ticket(&self, token: &str, name: &str, body: &[u8]) -> String {
        let upload = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/upload?dir=root&name={}", urlencode(name)))
            .header(header::HOST, self.bound.to_string())
            .header(header::ORIGIN, format!("http://{}", self.bound))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(body.to_vec()))
            .unwrap();
        let (status, _, bytes) = self.send(upload).await;
        assert_eq!(status, StatusCode::OK, "upload failed: {bytes:?}");
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/download-ticket")
            .header(header::HOST, self.bound.to_string())
            .header(header::ORIGIN, format!("http://{}", self.bound))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(format!("{{\"fileId\":\"root/{name}\"}}")))
            .unwrap();
        let (status, _, bytes) = self.send(request).await;
        assert_eq!(status, StatusCode::OK, "ticket failed: {bytes:?}");
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["url"].as_str().unwrap().to_owned()
    }

    fn get(&self, uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header(header::HOST, self.bound.to_string())
            .body(Body::empty())
            .unwrap()
    }
}

/// 页面响应必须带防嵌入头；`frame-ancestors` 只能通过 HTTP 头生效，
/// 因此这里验证的是服务端响应，而不是 index.html 的 meta 声明。
#[tokio::test]
async fn page_sets_anti_framing_headers() {
    let fixture = Fixture::new();
    let (status, headers, body) = fixture.send(fixture.get("/")).await;
    assert_eq!(status, StatusCode::OK);
    let csp = headers
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(csp.contains("frame-ancestors 'none'"), "csp: {csp}");
    assert_eq!(
        headers
            .get(header::X_FRAME_OPTIONS)
            .and_then(|value| value.to_str().ok()),
        Some("DENY")
    );
    assert!(!body.is_empty());
}

/// 伪造 Host 必须被拒绝，防止 DNS rebinding 把请求指向本服务。
#[tokio::test]
async fn rejects_foreign_host() {
    let fixture = Fixture::new();
    let request = Request::builder()
        .uri("/api/session")
        .header(header::HOST, "attacker.example:43210")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = fixture.send(request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// 通配监听只接受冻结的本机 IPv4 与实际端口；其他本机网段地址、通配地址、
/// 主机名和错误端口都不能穿过第一道门禁。被拒绝的响应仍带安全头。
#[tokio::test]
async fn wildcard_host_accepts_only_frozen_local_ipv4_and_exact_port() {
    let fixture = Fixture::wildcard();
    for host in ["192.168.1.10:43210", "127.0.0.1:43210"] {
        let request = Request::builder()
            .uri("/")
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            fixture.send(request).await.0,
            StatusCode::OK,
            "Host {host} must be accepted"
        );
    }
    for host in [
        "192.168.1.10:43211",
        "192.168.1.11:43210",
        "0.0.0.0:43210",
        "localhost:43210",
        "attacker.example:43210",
        "[::1]:43210",
    ] {
        let request = Request::builder()
            .uri("/")
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap();
        let (status, headers, _) = fixture.send(request).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "Host {host} must be rejected"
        );
        assert_eq!(
            headers
                .get(header::X_FRAME_OPTIONS)
                .and_then(|value| value.to_str().ok()),
            Some("DENY"),
            "rejected Host {host} must still carry security headers"
        );
    }
    // 缺少 Host 没有可比对的 authority，必须失败关闭。
    let request = Request::builder().uri("/").body(Body::empty()).unwrap();
    assert_eq!(fixture.send(request).await.0, StatusCode::BAD_REQUEST);
}

/// 通配监听的 Origin 必须与本次请求已通过白名单的 Host 精确同源；
/// 另一个本机 IPv4 也是不同源，不能用「都在本机」替代同源校验。
#[tokio::test]
async fn wildcard_origin_must_match_the_request_host() {
    let fixture = Fixture::wildcard();
    let body = format!("{{\"secret\":\"{}\"}}", fixture.secret);
    let cases = [
        (
            "192.168.1.10:43210",
            Some("http://192.168.1.10:43210"),
            StatusCode::OK,
        ),
        (
            "192.168.1.10:43210",
            Some("http://127.0.0.1:43210"),
            StatusCode::FORBIDDEN,
        ),
        (
            "192.168.1.10:43210",
            Some("http://attacker.example:43210"),
            StatusCode::FORBIDDEN,
        ),
        ("192.168.1.10:43210", None, StatusCode::FORBIDDEN),
        (
            "127.0.0.1:43210",
            Some("http://127.0.0.1:43210"),
            StatusCode::OK,
        ),
    ];
    for (host, origin, expected) in cases {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri("/api/pair")
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        let request = builder.body(Body::from(body.clone())).unwrap();
        assert_eq!(
            fixture.send(request).await.0,
            expected,
            "Host {host} Origin {origin:?}"
        );
    }
}

/// 通配策略在真实回环监听上仍按同一套中间件工作：具体本机 IPv4 Host 可以
/// 打开页面与完成同源配对，任意 Host 返回 400，跨源写请求返回 403。
///
/// 监听器只绑定回环，避免自动测试把临时共享暴露到局域网；通配策略的 Host
/// 白名单仍包含一个合成局域网地址，用于证明校验依据是 Host 而不是连接地址。
#[tokio::test]
async fn wildcard_policy_serves_real_http_only_for_local_host() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let bound: SocketAddr = format!("192.168.1.10:{port}").parse().unwrap();
    let fixture = Fixture::wildcard_for(bound);
    let app = fixture.app.clone();
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let allowed = raw_http(
        port,
        &format!("GET / HTTP/1.1\r\nHost: 192.168.1.10:{port}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(
        allowed.starts_with("HTTP/1.1 200 OK"),
        "allowed Host must reach the page: {allowed}"
    );
    let loopback = raw_http(
        port,
        &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(
        loopback.starts_with("HTTP/1.1 200 OK"),
        "loopback IPv4 must be accepted: {loopback}"
    );
    let rejected = raw_http(
        port,
        &format!("GET / HTTP/1.1\r\nHost: attacker.example:{port}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(
        rejected.starts_with("HTTP/1.1 400 Bad Request"),
        "foreign Host must be rejected: {rejected}"
    );

    let body = format!("{{\"secret\":\"{}\"}}", fixture.secret);
    let cross = raw_http(
        port,
        &format!(
            "POST /api/pair HTTP/1.1\r\nHost: 192.168.1.10:{port}\r\nOrigin: http://attacker.example\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(
        cross.starts_with("HTTP/1.1 403 Forbidden"),
        "cross-origin pairing must be rejected: {cross}"
    );
    let same = raw_http(
        port,
        &format!(
            "POST /api/pair HTTP/1.1\r\nHost: 192.168.1.10:{port}\r\nOrigin: http://192.168.1.10:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(
        same.starts_with("HTTP/1.1 200 OK"),
        "same-origin pairing must succeed: {same}"
    );

    server.abort();
}

/// 发一条原始 HTTP/1.1 请求并读完响应；只用于回环真实监听用例，
/// 不依赖任何浏览器或外部网络。
async fn raw_http(port: u16, request: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

/// 状态变更请求缺少或伪造 Origin 一律拒绝，避免跨站表单触发写操作。
#[tokio::test]
async fn pairing_requires_same_origin() {
    let fixture = Fixture::new();
    let body = format!("{{\"secret\":\"{}\"}}", fixture.secret);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/pair")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, "http://attacker.example")
        .body(Body::from(body.clone()))
        .unwrap();
    let (status, _, _) = fixture.send(request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/pair")
        .header(header::HOST, fixture.bound.to_string())
        .body(Body::from(body))
        .unwrap();
    let (status, _, _) = fixture.send(request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// 未鉴权访问受保护接口必须返回 401 且错误体只有码与固定短句。
#[tokio::test]
async fn protected_routes_require_bearer() {
    let fixture = Fixture::new();
    let (status, _, bytes) = fixture.send(fixture.get("/api/list?dir=root")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["error"]["code"], "unauthorized");
    assert!(value["error"]["message"].as_str().unwrap().len() < 64);
}

/// 路径逃逸在解析阶段即被拒绝，绝不交给文件系统。
#[tokio::test]
async fn list_rejects_traversal() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    for dir in ["root/../etc", "root/.hidden", "root/a%5Cb", "C:/windows"] {
        let request = Request::builder()
            .uri(format!("/api/list?dir={dir}"))
            .header(header::HOST, fixture.bound.to_string())
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = fixture.send(request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{dir} must be rejected");
    }
}

/// 同名上传默认不覆盖：第二次提交自动改名，两个文件都存在。
#[tokio::test]
async fn upload_never_overwrites() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let mut names = Vec::new();
    for _ in 0..2 {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/upload?dir=root&name=note.txt")
            .header(header::HOST, fixture.bound.to_string())
            .header(header::ORIGIN, format!("http://{}", fixture.bound))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from("hello"))
            .unwrap();
        let (status, _, bytes) = fixture.send(request).await;
        assert_eq!(status, StatusCode::OK, "upload failed: {bytes:?}");
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["size"], 5);
        names.push(value["name"].as_str().unwrap().to_owned());
    }
    assert_eq!(names[0], "note.txt");
    assert_eq!(names[1], "note (1).txt");
}

/// 上传名字同样走路径段校验；隐藏名与暂存前缀不可写。
#[tokio::test]
async fn upload_rejects_restricted_names() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    for name in [".hidden", ".klt-tmp-x", "CON", "a/b"] {
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/upload?dir=root&name={}", urlencode(name)))
            .header(header::HOST, fixture.bound.to_string())
            .header(header::ORIGIN, format!("http://{}", fixture.bound))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from("x"))
            .unwrap();
        let (status, _, _) = fixture.send(request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{name} must be rejected");
    }
}

/// 票据只能下载签发票据时的那一个文件，且不能用于其他 API。
#[tokio::test]
async fn ticket_is_scoped_to_one_file() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let upload = Request::builder()
        .method(Method::POST)
        .uri("/api/upload?dir=root&name=a.txt")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, format!("http://{}", fixture.bound))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("payload"))
        .unwrap();
    assert_eq!(fixture.send(upload).await.0, StatusCode::OK);

    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/download-ticket")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, format!("http://{}", fixture.bound))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("{\"fileId\":\"root/a.txt\"}"))
        .unwrap();
    let (status, _, bytes) = fixture.send(request).await;
    assert_eq!(status, StatusCode::OK);
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let url = value["url"].as_str().unwrap().to_owned();
    assert!(url.starts_with("/download/"));

    let (status, headers, body) = fixture.send(fixture.get(&url)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"payload");
    assert!(
        headers
            .get(header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("attachment"))
    );

    // 票据 URL 不构成通用凭据：换成列表接口必须仍然 401。
    let (status, _, _) = fixture.send(fixture.get("/api/list?dir=root")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// 查询串里的中文文件名必须能按 UTF-8 百分号编码正确落盘。
#[tokio::test]
async fn upload_accepts_utf8_name() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let name = "报告 2026.txt";
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/upload?dir=root&name={}", urlencode(name)))
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, format!("http://{}", fixture.bound))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("data"))
        .unwrap();
    let (status, _, bytes) = fixture.send(request).await;
    assert_eq!(status, StatusCode::OK, "{bytes:?}");
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["name"], name);
}

/// 最小百分号编码，仅供测试构造查询串。
fn urlencode(value: &str) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            output.push(*byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

/// HEAD 由 axum 的 GET 路由兜底，但响应体必须被剥掉、头部与 GET 一致。
///
/// 浏览器与下载管理器常在真正 GET 之前发 HEAD 探测长度与类型；如果 HEAD 返回 405
/// 或缺少 `Content-Length`，下载器会退回整份 GET 或直接报错。这里同时固定
/// `Accept-Ranges: bytes`：下载支持标准单区间续传。
#[tokio::test]
async fn head_on_download_mirrors_get_headers_without_body() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let url = fixture
        .upload_and_ticket(&token, "head.txt", b"head-body")
        .await;

    let (status, get_headers, get_body) = fixture.send(fixture.get(&url)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(get_body, b"head-body");

    let request = Request::builder()
        .method(Method::HEAD)
        .uri(&url)
        .header(header::HOST, fixture.bound.to_string())
        .body(Body::empty())
        .unwrap();
    let (status, head_headers, head_body) = fixture.send(request).await;
    assert_eq!(status, StatusCode::OK);
    assert!(head_body.is_empty(), "HEAD must not carry a body");
    // axum 在剥离 HEAD 响应体之前用 body 的 size hint 补 `Content-Length`，
    // 因此这里比较的是「长度一致」而不是「头部逐字相同」。
    assert_eq!(
        head_headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()),
        get_headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
    );
    assert_eq!(
        head_headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()),
        Some("9")
    );
    assert_eq!(
        head_headers.get(header::CONTENT_TYPE),
        get_headers.get(header::CONTENT_TYPE)
    );
    assert_eq!(
        head_headers.get(header::CONTENT_DISPOSITION),
        get_headers.get(header::CONTENT_DISPOSITION)
    );
    assert_eq!(
        head_headers
            .get(header::ACCEPT_RANGES)
            .and_then(|value| value.to_str().ok()),
        Some("bytes")
    );
    // 安全头必须同样出现在 HEAD 上，否则探测请求会看到不同的安全策略。
    assert_eq!(
        head_headers
            .get(header::X_FRAME_OPTIONS)
            .and_then(|value| value.to_str().ok()),
        Some("DENY")
    );
}

/// 标准单区间 Range 的端到端语义：206、`Content-Range`、部分长度与精确字节。
///
/// 这些断言固定浏览器续传依赖的契约。若状态码或 `Content-Range` 与内容不一致，
/// 下载器会把整份或错位字节拼进已下载文件，产生静默损坏——因此逐项比对字节，
/// 而不只检查状态码。
#[tokio::test]
async fn range_requests_return_exact_partial_content() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let payload = b"0123456789abcdef";
    let url = fixture
        .upload_and_ticket(&token, "range.txt", payload)
        .await;

    // 闭区间：bytes=4-7 → "4567"。
    let request = Request::builder()
        .uri(&url)
        .header(header::HOST, fixture.bound.to_string())
        .header(header::RANGE, "bytes=4-7")
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = fixture.send(request).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, b"4567");
    assert_eq!(
        headers
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok()),
        Some("bytes 4-7/16")
    );
    assert_eq!(
        headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()),
        Some("4")
    );
    assert_eq!(
        headers
            .get(header::ACCEPT_RANGES)
            .and_then(|value| value.to_str().ok()),
        Some("bytes")
    );

    // 开放结尾：bytes=10- → 尾部 6 字节。
    let request = Request::builder()
        .uri(&url)
        .header(header::HOST, fixture.bound.to_string())
        .header(header::RANGE, "bytes=10-")
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = fixture.send(request).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, b"abcdef");
    assert_eq!(
        headers
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok()),
        Some("bytes 10-15/16")
    );

    // 后缀：bytes=-4 → 最后 4 字节。
    let request = Request::builder()
        .uri(&url)
        .header(header::HOST, fixture.bound.to_string())
        .header(header::RANGE, "bytes=-4")
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = fixture.send(request).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, b"cdef");
    assert_eq!(
        headers
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok()),
        Some("bytes 12-15/16")
    );

    // 末尾超出长度：按实际长度截断，而不是 416。
    let request = Request::builder()
        .uri(&url)
        .header(header::HOST, fixture.bound.to_string())
        .header(header::RANGE, "bytes=14-999")
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = fixture.send(request).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, b"ef");
    assert_eq!(
        headers
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok()),
        Some("bytes 14-15/16")
    );
}

/// 超出文件长度的起点必须 416 + `Content-Range: bytes */size`。
///
/// 若这里退回 200 整份内容，严格按 RFC 处理的下载器会把整份字节拼到已有进度之后，
/// 得到损坏文件；明确 416 才能让客户端放弃或重新取票据。
#[tokio::test]
async fn unsatisfiable_range_is_rejected_with_416() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let payload = b"0123456789abcdef";
    let url = fixture
        .upload_and_ticket(&token, "range-416.txt", payload)
        .await;

    for value in ["bytes=16-", "bytes=99-100", "bytes=-0"] {
        let request = Request::builder()
            .uri(&url)
            .header(header::HOST, fixture.bound.to_string())
            .header(header::RANGE, value)
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) = fixture.send(request).await;
        assert_eq!(
            status,
            StatusCode::RANGE_NOT_SATISFIABLE,
            "{value}: {body:?}"
        );
        assert!(body.is_empty(), "{value} must not carry a body");
        assert_eq!(
            headers
                .get(header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok()),
            Some("bytes */16"),
            "{value}"
        );
    }
}

/// 非法或无法解释的 Range 按 RFC 允许的方式忽略：200 整份内容，而不是 416。
///
/// 误把「客户端带了一个我们不认识的区间」判成不可满足，会让下载器放弃本可成功的
/// 下载；整份返回仍保证内容正确。
#[tokio::test]
async fn unusable_range_is_ignored_and_returns_full_body() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let payload = b"0123456789abcdef";
    let url = fixture
        .upload_and_ticket(&token, "range-ignore.txt", payload)
        .await;

    for value in [
        "items=0-1",
        "bytes=",
        "bytes=abc-def",
        "bytes=7-4",
        "bytes=0-1,4-5",
    ] {
        let request = Request::builder()
            .uri(&url)
            .header(header::HOST, fixture.bound.to_string())
            .header(header::RANGE, value)
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) = fixture.send(request).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(body, payload, "{value}");
        assert!(headers.get(header::CONTENT_RANGE).is_none(), "{value}");
    }
}

/// HEAD 带 Range 时必须与 GET 的区间语义一致（状态与头部一致，响应体为空）。
///
/// 下载管理器常先 HEAD 探测再决定从哪一段续传；若 HEAD 返回 200 而 GET 返回 206，
/// 客户端会按错误的总长度规划请求。
#[tokio::test]
async fn head_with_range_matches_get_semantics_without_body() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let payload = b"0123456789abcdef";
    let url = fixture
        .upload_and_ticket(&token, "range-head.txt", payload)
        .await;

    let request = Request::builder()
        .method(Method::HEAD)
        .uri(&url)
        .header(header::HOST, fixture.bound.to_string())
        .header(header::RANGE, "bytes=4-7")
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = fixture.send(request).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert!(body.is_empty(), "HEAD must not carry a body");
    assert_eq!(
        headers
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok()),
        Some("bytes 4-7/16")
    );
    assert_eq!(
        headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()),
        Some("4")
    );
}

/// 票据在服务停止后必须失效：`GET /download/{token}` 不能继续读文件。
///
/// 停止路径会撤销会话与票据并清空运行状态；这里直接驱动同一份上下文，
/// 确认处理器按状态拒绝，而不是依赖监听端口关闭后的连接失败。
#[tokio::test]
async fn download_is_rejected_after_stop() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let url = fixture
        .upload_and_ticket(&token, "stopped.txt", b"data")
        .await;
    assert_eq!(fixture.send(fixture.get(&url)).await.0, StatusCode::OK);

    fixture.context.state.mark_idle().unwrap();
    let (status, _, body) = fixture.send(fixture.get(&url)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"]["code"], "unavailable");

    // 票据本身也随停止清空：即使状态被重新标记为运行，旧票据也不能复活。
    // `mark_running` 只允许从 `Starting` 进入，因此先走一次启动转换，避免
    // 「空闲态直接写运行中」这条本就不合法的路径掩盖真实断言。
    fixture.context.tickets.clear();
    fixture.context.state.begin_start().unwrap();
    fixture
        .context
        .state
        .mark_running(
            fixture.bound,
            String::new(),
            "share".into(),
            String::new(),
            4_000_000_000,
        )
        .unwrap();
    let (status, _, body) = fixture.send(fixture.get(&url)).await;
    assert_eq!(status, StatusCode::GONE);
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"]["code"], "ticket_expired");
}

/// 状态变更接口在停止后同样拒绝，且拒绝发生在解析请求体与写盘之前。
#[tokio::test]
async fn upload_and_ticket_are_rejected_after_stop() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    fixture.context.state.mark_idle().unwrap();

    let upload = Request::builder()
        .method(Method::POST)
        .uri("/api/upload?dir=root&name=after-stop.txt")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, format!("http://{}", fixture.bound))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("late"))
        .unwrap();
    let (status, _, _) = fixture.send(upload).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let ticket = Request::builder()
        .method(Method::POST)
        .uri("/api/download-ticket")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, format!("http://{}", fixture.bound))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("{\"fileId\":\"root/after-stop.txt\"}"))
        .unwrap();
    let (status, _, _) = fixture.send(ticket).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // 停止状态没有写盘副作用：共享目录里不应出现这次上传的名字。
    let share = fixture._root.path().join("share");
    assert!(!share.join("after-stop.txt").exists());
}

/// 配对在停止后也必须拒绝，避免停止瞬间仍签发可用令牌。
#[tokio::test]
async fn pairing_is_rejected_after_stop() {
    let fixture = Fixture::new();
    fixture.context.state.mark_idle().unwrap();
    let body = format!("{{\"secret\":\"{}\"}}", fixture.secret);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/pair")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, format!("http://{}", fixture.bound))
        .body(Body::from(body))
        .unwrap();
    let (status, _, _) = fixture.send(request).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// 并发上传占满时必须立即返回 `busy`，而不是排队占内存；
/// 占位释放后同一请求必须能成功，证明 RAII 守卫没有泄漏计数。
#[tokio::test]
async fn upload_concurrency_limit_is_enforced_and_released() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let limit = kerminal_plugin_lan_transfer::config::MAX_CONCURRENT_UPLOADS;

    // 直接把计数推到上限，模拟已有若干上传正在进行。
    fixture
        .context
        .uploads
        .store(limit, std::sync::atomic::Ordering::Release);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/upload?dir=root&name=busy.txt")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, format!("http://{}", fixture.bound))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("x"))
        .unwrap();
    let (status, _, body) = fixture.send(request).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"]["code"], "busy");
    // 被拒绝的请求不能写盘，也不能把计数留在上限之上。
    assert!(!fixture._root.path().join("share").join("busy.txt").exists());
    assert_eq!(
        fixture
            .context
            .uploads
            .load(std::sync::atomic::Ordering::Acquire),
        limit
    );

    fixture
        .context
        .uploads
        .store(0, std::sync::atomic::Ordering::Release);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/upload?dir=root&name=busy.txt")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, format!("http://{}", fixture.bound))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("x"))
        .unwrap();
    assert_eq!(fixture.send(request).await.0, StatusCode::OK);
    // 成功路径必须归还占位，否则下一次上传会被自己的计数挡住。
    assert_eq!(
        fixture
            .context
            .uploads
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

/// 配对尝试在固定窗口内达到上限后返回 `busy`；这是唯一免令牌的写接口，
/// 限流失效就等于允许离线爆破。
#[tokio::test]
async fn pairing_attempts_are_rate_limited() {
    let fixture = Fixture::new();
    // 直接推进窗口计数：真实等待 60 秒不适合作为测试时长。
    {
        let mut slot = fixture.context.pairing.lock().unwrap();
        *slot = (kerminal_plugin_lan_transfer::secret::now_unix(), u32::MAX);
    }
    let body = format!("{{\"secret\":\"{}\"}}", fixture.secret);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/pair")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, format!("http://{}", fixture.bound))
        .body(Body::from(body))
        .unwrap();
    let (status, _, bytes) = fixture.send(request).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["error"]["code"], "busy");
}

/// 目录里混有「本插件不支持的名字」时，列表必须只返回可安全访问的条目，
/// 且每一项的 id 都能被服务端重新解析——不能出现空 id 让整份列表变成协议错误。
///
/// 平台差异：`a:b.txt`、尾空格、尾点在 Linux/macOS 上是合法文件名，Windows 上
/// 通常无法创建。测试按平台分别准备能创建的名字，断言的是「创建成功的那些要么
/// 出现在列表里且 id 可解析，要么被过滤掉」，不假设某个名字一定能创建。
#[tokio::test]
async fn list_filters_names_that_cannot_be_represented() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let share = fixture._root.path().join("share");
    std::fs::write(share.join("normal.txt"), b"ok").unwrap();
    std::fs::create_dir(share.join("folder")).unwrap();

    let mut created = Vec::new();
    #[cfg(unix)]
    for name in ["a:b.txt", "trailing ", "trailing.", r"a\b.txt"] {
        if std::fs::write(share.join(name), b"x").is_ok() {
            created.push(name);
        }
    }
    // Windows 上这些名字基本无法创建；仍尝试一次，创建成功就必须被过滤。
    #[cfg(windows)]
    for name in ["a:b.txt", "trailing."] {
        if std::fs::write(share.join(name), b"x").is_ok() {
            created.push(name);
        }
    }

    let request = Request::builder()
        .uri("/api/list?dir=root")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let (status, _, bytes) = fixture.send(request).await;
    assert_eq!(status, StatusCode::OK, "{bytes:?}");
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let entries = value["entries"].as_array().unwrap();

    let mut names = Vec::new();
    for entry in entries {
        let id = entry["id"].as_str().expect("id must be a string");
        // 空 id 会让页面把整份列表判为协议错误；这里逐项证明不会出现。
        assert!(!id.is_empty(), "entry id must never be empty: {entry:?}");
        // id 必须能被服务端重新解析成同一目录下的同一名字。
        let path = kerminal_plugin_lan_transfer::path::VirtualPath::parse(id)
            .unwrap_or_else(|_| panic!("id must round-trip: {id}"));
        assert_eq!(
            path.segments().len(),
            1,
            "id must stay in the listed dir: {id}"
        );
        names.push(entry["name"].as_str().unwrap().to_owned());
    }
    assert!(names.contains(&"normal.txt".to_owned()));
    assert!(names.contains(&"folder".to_owned()));
    // 创建成功的不支持名字必须被隐藏，而不是以空 id 或改名形式出现。
    for name in &created {
        assert!(
            !names.iter().any(|value| value == name),
            "{name} must be hidden"
        );
    }
    // 目录只有两项正常内容时，被隐藏的条目不能产生多余的下一页游标。
    assert!(value["nextCursor"].is_null(), "{value:?}");
}

/// 隐藏条目被过滤后，分页游标仍然只指向真实返回过的名字：
/// 用上一页最后一项作为 cursor 必须能继续读到下一页，不会因为隐藏条目卡住。
#[tokio::test]
async fn list_cursor_skips_hidden_entries() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let share = fixture._root.path().join("share");
    for index in 0..3 {
        std::fs::write(share.join(format!("file-{index}.txt")), b"x").unwrap();
    }
    // 点开头的名字在 Windows 与 Unix 上都能创建，因此这里能真实制造「被过滤的条目」，
    // 而不是只在注释里假设它们存在。
    std::fs::write(share.join(".hidden"), b"x").unwrap();
    std::fs::write(share.join(".klt-tmp-1"), b"x").unwrap();

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let uri = match &cursor {
            Some(value) => format!("/api/list?dir=root&cursor={}", urlencode(value)),
            None => "/api/list?dir=root".to_owned(),
        };
        let request = Request::builder()
            .uri(&uri)
            .header(header::HOST, fixture.bound.to_string())
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let (status, _, bytes) = fixture.send(request).await;
        assert_eq!(status, StatusCode::OK, "{bytes:?}");
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for entry in value["entries"].as_array().unwrap() {
            seen.push(entry["name"].as_str().unwrap().to_owned());
        }
        match value["nextCursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
        assert!(seen.len() <= 8, "pagination must terminate");
    }
    assert_eq!(
        seen,
        vec![
            "file-0.txt".to_owned(),
            "file-1.txt".to_owned(),
            "file-2.txt".to_owned()
        ]
    );
}

/// 所有非 GET/HEAD 请求都由中间件统一校验同源，处理器不可能漏掉：
/// 合法令牌但缺少 Origin 的上传必须被拒绝，且磁盘上不留下任何文件。
#[tokio::test]
async fn upload_without_origin_is_rejected_before_writing() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/upload?dir=root&name=no-origin.txt")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("data"))
        .unwrap();
    let (status, _, body) = fixture.send(request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"]["code"], "forbidden");
    let share = fixture._root.path().join("share");
    assert!(!share.join("no-origin.txt").exists());
    assert!(
        std::fs::read_dir(&share).unwrap().next().is_none(),
        "rejected upload must not create staging files"
    );
}

/// 跨站 Origin 即使带合法令牌也不能上传：这是 CSRF 防护的实际断言，
/// 而不是只测一个辅助函数。
#[tokio::test]
async fn upload_with_foreign_origin_is_rejected() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/upload?dir=root&name=csrf.txt")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, "http://attacker.example")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("data"))
        .unwrap();
    let (status, _, _) = fixture.send(request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!fixture._root.path().join("share").join("csrf.txt").exists());
    // 跨站 Origin 也不能用于签发下载票据或配对，处理器不再各自重复校验。
    let ticket = Request::builder()
        .method(Method::POST)
        .uri("/api/download-ticket")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, "http://attacker.example")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("{\"fileId\":\"root/csrf.txt\"}"))
        .unwrap();
    assert_eq!(fixture.send(ticket).await.0, StatusCode::FORBIDDEN);
}

/// 同源比较针对本次请求的 Host：`Host: 127.0.0.1:P` 配 `Origin: http://localhost:P`
/// 虽然都指向本机，却不是同源，必须拒绝。
#[tokio::test]
async fn cross_loopback_alias_origin_is_rejected() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/upload?dir=root&name=alias.txt")
        .header(header::HOST, format!("127.0.0.1:{}", fixture.bound.port()))
        .header(
            header::ORIGIN,
            format!("http://localhost:{}", fixture.bound.port()),
        )
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("data"))
        .unwrap();
    let (status, _, body) = fixture.send(request).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body:?}");
    assert!(
        !fixture
            ._root
            .path()
            .join("share")
            .join("alias.txt")
            .exists()
    );

    // 同一别名自洽时（Host 与 Origin 都用 localhost）必须放行：
    // 用户可以用任一已绑定别名打开页面，限制的是请求内部的一致性。
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/upload?dir=root&name=alias.txt")
        .header(header::HOST, format!("localhost:{}", fixture.bound.port()))
        .header(
            header::ORIGIN,
            format!("http://localhost:{}", fixture.bound.port()),
        )
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("data"))
        .unwrap();
    assert_eq!(fixture.send(request).await.0, StatusCode::OK);
}

/// `Host` 是大小写不敏感的 authority：`LOCALHOST:P` 配同端口 Origin 必须放行，
/// 而大小写不同的其他主机名不能被当成 `localhost`。
///
/// 这条用例覆盖门禁两层的配合：第一层 `host_allowed` 曾对 `localhost` 逐字比较，
/// 会在同源校验之前就误拒合法请求；两层必须使用同一套大小写语义。
#[tokio::test]
async fn localhost_host_is_case_insensitive() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let port = fixture.bound.port();

    for host in [format!("LOCALHOST:{port}"), format!("LocalHost:{port}")] {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/upload?dir=root&name=case.txt")
            .header(header::HOST, &host)
            .header(header::ORIGIN, format!("http://localhost:{port}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from("data"))
            .unwrap();
        assert_eq!(
            fixture.send(request).await.0,
            StatusCode::OK,
            "Host: {host} must be accepted"
        );
    }

    // 大小写放宽只针对 `localhost`：同端口下其他主机名仍是伪造 Host。
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/upload?dir=root&name=case.txt")
        .header(header::HOST, format!("LOCALHOST.LOCAL:{port}"))
        .header(header::ORIGIN, format!("http://LOCALHOST.LOCAL:{port}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("data"))
        .unwrap();
    assert_eq!(fixture.send(request).await.0, StatusCode::BAD_REQUEST);

    // 即使 Host 大小写合法，Origin 指向别的主机也必须拒绝（同源仍需精确匹配）。
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/upload?dir=root&name=case.txt")
        .header(header::HOST, format!("LOCALHOST:{port}"))
        .header(header::ORIGIN, format!("http://127.0.0.1:{port}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from("data"))
        .unwrap();
    assert_eq!(fixture.send(request).await.0, StatusCode::FORBIDDEN);
}

/// 只读例外：票据下载的 GET/HEAD 允许缺少 Origin，但带了就必须与 Host 一致。
#[tokio::test]
async fn ticket_download_allows_missing_origin_but_validates_present_one() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let url = fixture
        .upload_and_ticket(&token, "get.txt", b"payload")
        .await;

    // 缺少 Origin：允许（浏览器原生下载不会带）。
    assert_eq!(fixture.send(fixture.get(&url)).await.0, StatusCode::OK);

    // 同源 Origin：允许。
    let request = Request::builder()
        .uri(&url)
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, format!("http://{}", fixture.bound))
        .body(Body::empty())
        .unwrap();
    assert_eq!(fixture.send(request).await.0, StatusCode::OK);

    // 跨源 Origin：拒绝，即使票据有效。
    let request = Request::builder()
        .uri(&url)
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, "http://attacker.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(fixture.send(request).await.0, StatusCode::FORBIDDEN);
}

/// Host 被拒绝时响应仍必须带全部安全头：单层中间件保证门禁失败也走同一段收尾。
#[tokio::test]
async fn rejected_host_response_still_carries_security_headers() {
    let fixture = Fixture::new();
    let request = Request::builder()
        .uri("/")
        .header(header::HOST, "attacker.example:43210")
        .body(Body::empty())
        .unwrap();
    let (status, headers, _) = fixture.send(request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        headers
            .get(header::X_FRAME_OPTIONS)
            .and_then(|value| value.to_str().ok()),
        Some("DENY")
    );
    assert!(
        headers
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("frame-ancestors 'none'"))
    );
    assert_eq!(
        headers
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .and_then(|value| value.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        headers
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
}

/// Origin 被拒绝时同样带安全头，避免「被拒响应」成为唯一没有防嵌入头的路径。
#[tokio::test]
async fn rejected_origin_response_still_carries_security_headers() {
    let fixture = Fixture::new();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/pair")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::ORIGIN, "http://attacker.example")
        .body(Body::from("{}"))
        .unwrap();
    let (status, headers, _) = fixture.send(request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        headers
            .get(header::X_FRAME_OPTIONS)
            .and_then(|value| value.to_str().ok()),
        Some("DENY")
    );
}

/// 停止期间（`Stopping`）即使监听尚未关闭，也必须拒绝会话检查与下载：
/// 页面据此判断服务不可用，而不是拿着旧令牌继续操作。
#[tokio::test]
async fn stopping_phase_rejects_session_and_download() {
    let fixture = Fixture::new();
    let token = fixture.token().await;
    let url = fixture
        .upload_and_ticket(&token, "stopping.txt", b"data")
        .await;

    fixture.context.state.begin_stop(true).unwrap();

    let request = Request::builder()
        .uri("/api/session")
        .header(header::HOST, fixture.bound.to_string())
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = fixture.send(request).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (status, _, _) = fixture.send(fixture.get(&url)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
