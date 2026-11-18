//! Origin/Host 同源契约回归：覆盖同源、不同源、非 UTF-8 头与缺失 Origin 的读写边界。
//! @author kongweiguang
//!
//! 放在独立集成测试目标里，是因为 `origin_allowed` 是跨站写入的唯一门禁：
//! 任何「解析失败就放行」的回归都应在公开 API 层被固定住，而不是只靠
//! `src/http/mod.rs` 内部单测。这里不启动监听，也不依赖浏览器，直接调用纯函数，
//! 因此能在唯一 Cargo 队列释放后由 `cargo test --test origin_contract` 快速复核。

use axum::http::{HeaderValue, Method};
use kerminal_plugin_lan_transfer::http::origin_allowed;

/// 同源请求在读写方法上都应放行；读写分别对应浏览器的 `GET`/`HEAD` 与状态变更。
///
/// 同源判断必须区分大小写不敏感的 authority 与逐字端口，这里同时固定住
/// 正常拼写和大小写变体，避免未来改成逐字比较时误伤合法请求。
#[test]
fn same_origin_is_allowed_for_read_and_write() {
    let host = HeaderValue::from_static("127.0.0.1:8080");
    let origin = HeaderValue::from_static("http://127.0.0.1:8080");

    for method in [Method::GET, Method::HEAD, Method::POST, Method::PUT] {
        assert!(
            origin_allowed(Some(&host), &method, Some(&origin)),
            "{method}"
        );
    }

    // authority 大小写不敏感：`LOCALHOST` 与 `localhost` 属于同一源。
    let upper_host = HeaderValue::from_static("LOCALHOST:8080");
    let lower_origin = HeaderValue::from_static("http://localhost:8080");
    assert!(origin_allowed(
        Some(&upper_host),
        &Method::POST,
        Some(&lower_origin)
    ));
}

/// 不同源即使指向同一台机器也必须拒绝：端口、主机名、协议任一不同都不同源。
///
/// 这是 CSRF 防护的核心，读方法携带 Origin 时同样不能豁免，否则跨站页面
/// 可以借用户浏览器读取列表接口。
#[test]
fn cross_origin_is_rejected_for_read_and_write() {
    let host = HeaderValue::from_static("127.0.0.1:8080");
    let cases = [
        // 端口不同。
        "http://127.0.0.1:8081",
        // 主机名不同（与 Host 是不同源，即便都指向本机）。
        "http://localhost:8080",
        // 协议不同。
        "https://127.0.0.1:8080",
        // 非 http 协议。
        "file://127.0.0.1:8080",
        // 带路径、查询或用户信息的 Origin 不是浏览器同源值。
        "http://127.0.0.1:8080/",
        "http://127.0.0.1:8080/?x=1",
        "http://user@127.0.0.1:8080",
    ];
    for origin in cases {
        let origin = HeaderValue::from_str(origin).unwrap();
        for method in [Method::GET, Method::POST] {
            assert!(
                !origin_allowed(Some(&host), &method, Some(&origin)),
                "{origin:?} {method}"
            );
        }
    }
}

/// `Host` 不是可见 ASCII（非 UTF-8/含不可见字节）时必须失败关闭。
///
/// 这类头不可能来自浏览器同源 Origin，也无法与 Origin 做可信比较；
/// 一旦回退成空串比较，畸形 Host 就可能被当作同源放行。
/// `0xFF` 是 `HeaderValue` 允许承载但不构成合法 UTF-8 的原始字节，同时
/// 多字节 UTF-8（`é`）也会被 `to_str` 按「非可见 ASCII」拒绝；两种都固定住。
#[test]
fn non_utf8_host_fails_closed() {
    let origin = HeaderValue::from_static("http://127.0.0.1:8080");
    for raw in [
        HeaderValue::from_bytes(b"127.0.0.1:8080\xFF").unwrap(),
        HeaderValue::from_bytes(b"127.0.0.1:8080\xC3\xA9").unwrap(),
    ] {
        assert!(raw.to_str().is_err(), "fixture must be unparsable");
        for method in [Method::GET, Method::HEAD, Method::POST] {
            assert!(
                !origin_allowed(Some(&raw), &method, Some(&origin)),
                "{method}"
            );
        }
    }
}

/// `Origin` 不是可见 ASCII 时同样拒绝；读取方向也不能因为方法只读而放宽。
#[test]
fn non_utf8_origin_fails_closed() {
    let host = HeaderValue::from_static("127.0.0.1:8080");
    for raw in [
        HeaderValue::from_bytes(b"http://127.0.0.1:8080\xFF").unwrap(),
        HeaderValue::from_bytes(b"http://127.0.0.1:8080\xC3\xA9").unwrap(),
    ] {
        assert!(raw.to_str().is_err(), "fixture must be unparsable");
        for method in [Method::GET, Method::HEAD, Method::POST, Method::DELETE] {
            assert!(
                !origin_allowed(Some(&host), &method, Some(&raw)),
                "{method}"
            );
        }
    }
}

/// 缺失 Origin：只有 `GET`/`HEAD` 放行（浏览器原生下载不带 Origin），其余一律拒绝。
///
/// 路由只注册 `get`/`post`，也没有 CORS 预检处理器，因此 `OPTIONS` 与自定义方法
/// 没有「缺少 Origin 也合法」的既有路径；这里把它们固定为拒绝，防止实现退回
/// 「排除 POST/PUT/PATCH/DELETE」的黑名单后，未列举方法静默获得豁免。
/// 只读放行不等于无鉴权——对应接口仍要求票据或 Bearer；这里只固定门禁层的
/// 方法边界，避免把「缺 Origin」误当成所有请求都安全的信号。
#[test]
fn missing_origin_allows_reads_only() {
    let host = HeaderValue::from_static("127.0.0.1:8080");

    for method in [Method::GET, Method::HEAD] {
        assert!(origin_allowed(Some(&host), &method, None), "{method}");
    }
    for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
        assert!(!origin_allowed(Some(&host), &method, None), "{method}");
    }
    // 未注册方法与 CORS 预检不在白名单里：没有 Origin 也不能穿过门禁。
    for method in [
        Method::OPTIONS,
        Method::TRACE,
        Method::CONNECT,
        Method::from_bytes(b"PROPFIND").unwrap(),
    ] {
        assert!(!origin_allowed(Some(&host), &method, None), "{method}");
    }

    // 缺少 Host 时没有任何可比对的源，即使 Origin 合法也必须拒绝。
    let origin = HeaderValue::from_static("http://127.0.0.1:8080");
    assert!(!origin_allowed(None, &Method::POST, Some(&origin)));
    assert!(!origin_allowed(None, &Method::GET, Some(&origin)));
}

/// 未注册方法即使携带同源 Origin 也不获得额外豁免：Origin 校验只决定同源，
/// 路由层是否支持该方法是另一回事；这里确认门禁不会因方法名而放宽 Host 校验。
#[test]
fn same_origin_does_not_bypass_host_requirement_for_other_methods() {
    let origin = HeaderValue::from_static("http://127.0.0.1:8080");
    for method in [
        Method::OPTIONS,
        Method::TRACE,
        Method::from_bytes(b"PROPFIND").unwrap(),
    ] {
        assert!(!origin_allowed(None, &method, Some(&origin)), "{method}");
    }
}
