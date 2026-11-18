//! 内嵌静态页面。资源在编译期打包进 Sidecar，运行期不读磁盘、不访问网络，
//! 因此断网、目录被移动或用户改错工作目录都不会让页面 404。
//!
//! 页面在编译期嵌入二进制，因此打包时不需要把 `web/` 复制到包根；代价是页面内容
//! 随二进制版本固定，修改页面必须重新构建。`web/` 目前由另一执行者维护，页面与
//! Sidecar 的联调结论要以重新构建后的产物为准，不能只凭源码目录判断。
//! @author kongweiguang

use axum::{
    body::Body,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

/// 页面主体；`web/**` 由浏览器执行者维护，这里只读引用。
const INDEX: &str = include_str!("../../web/index.html");
const CSS: &str = include_str!("../../web/app.css");
const JS: &str = include_str!("../../web/app.js");

/// 首页：明确声明 UTF-8 与 HTML 类型，避免 Windows 默认代码页导致中文乱码。
pub async fn index() -> Response {
    static_asset(INDEX, "text/html; charset=utf-8")
}

pub async fn css() -> Response {
    static_asset(CSS, "text/css; charset=utf-8")
}

pub async fn js() -> Response {
    static_asset(JS, "text/javascript; charset=utf-8")
}

/// 静态资源共用同一响应构造；内容哈希随构建变化，但这里仍用 `no-store`，
/// 避免浏览器在服务重启后继续使用旧页面与旧契约。
fn static_asset(content: &'static str, content_type: &'static str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        Body::from(content),
    )
        .into_response()
}
