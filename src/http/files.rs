//! `GET /api/list`：虚拟相对路径下的目录枚举与分页。
//! @author kongweiguang
//!
//! 分页游标是「上次返回的最后一项名字」，而不是索引：目录在两次请求之间发生变化时，
//! 索引会跳项或重复；按名字续读只可能漏掉被删除的项，不会把同一文件重复返回给
//! 页面（页面仍会按 id 去重）。

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::Response,
};
use serde::{Deserialize, Serialize};

use crate::{
    config::PAGE_SIZE,
    error::{AppError, ErrorCode},
    fs_root::EntryKind,
    http::{AppContext, auth::authenticate, error_response, json_response},
    path::VirtualPath,
};

/// 查询参数；`dir` 缺省为根，`cursor` 是上一页最后一项名字。
#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Serialize)]
pub struct Breadcrumb {
    id: String,
    name: String,
}

#[derive(Serialize)]
pub struct Entry {
    id: String,
    name: String,
    kind: &'static str,
    size: Option<u64>,
    modified: Option<u64>,
}

#[derive(Serialize)]
pub struct ListResponse {
    breadcrumbs: Vec<Breadcrumb>,
    entries: Vec<Entry>,
    #[serde(rename = "nextCursor")]
    next_cursor: Option<String>,
}

/// 列表接口：先鉴权，再解析路径，最后枚举。错误码区分「目录不存在」与「路径被拒绝」，
/// 因为前者可由用户刷新列表解决，后者说明请求本身越界。
pub async fn list(
    State(context): State<Arc<AppContext>>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    if let Err(error) = authenticate(&context, &headers) {
        return error_response(error);
    }
    let dir = match VirtualPath::parse(query.dir.as_deref().unwrap_or("root")) {
        Ok(dir) => dir,
        Err(error) => return error_response(error),
    };
    let entries = match context.root.read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) => return error_response(remap_dir_error(error)),
    };
    // 目录里可能存在本插件无法安全表示的名字（例如 Linux 上合法的 `a:b.txt` 或尾空格，
    // 它们会被路径段规则拒绝）。这类条目必须整条过滤，而不是带一个空 id 发给页面：
    // 页面把空 id 视为协议错误，会让整个目录都打不开。过滤发生在排序与分页之前，
    // 游标因此只指向真正返回过的名字，翻页不会卡在被隐藏的条目上。
    let mut visible = entries
        .into_iter()
        .filter_map(|entry| {
            let id = entry_id(&dir, &entry.name)?;
            Some((entry, id))
        })
        .collect::<Vec<_>>();
    // 名称排序放在服务端，保证分页游标与页面显示顺序一致；客户端再排序会破坏续读。
    visible.sort_by(|(left, _), (right, _)| {
        kind_rank(left.kind)
            .cmp(&kind_rank(right.kind))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
    let start = match query.cursor.as_deref() {
        Some(cursor) if !cursor.is_empty() => visible
            .iter()
            .position(|(entry, _)| entry.name == cursor)
            .map(|index| index + 1)
            .unwrap_or(0),
        _ => 0,
    };
    let page = visible
        .iter()
        .skip(start)
        .take(PAGE_SIZE)
        .map(|(entry, id)| Entry {
            id: id.clone(),
            name: entry.name.clone(),
            kind: match entry.kind {
                EntryKind::File => "file",
                EntryKind::Directory => "directory",
            },
            size: entry.size,
            modified: entry.modified,
        })
        .collect::<Vec<_>>();
    let next_cursor = visible
        .get(start + page.len())
        .map(|(entry, _)| entry.name.clone());
    json_response(ListResponse {
        breadcrumbs: breadcrumbs(&dir),
        entries: page,
        next_cursor,
    })
}

/// 为目录项生成不透明 id；名字无法通过路径段规则时返回 `None`，由调用方整条过滤。
///
/// 这里绝不退回空串或猜测名字：空 id 会让页面把整份列表判为协议错误，
/// 而「改名后展示」会指向一个不存在的文件，两种做法都比隐藏该条目更糟。
fn entry_id(dir: &VirtualPath, name: &str) -> Option<String> {
    dir.join(name).ok().map(|path| path.id())
}

/// 面包屑首项必须是固定 `root`，页面据此拒绝来自其他实现的响应。
fn breadcrumbs(dir: &VirtualPath) -> Vec<Breadcrumb> {
    let mut crumbs = vec![Breadcrumb {
        id: crate::path::ROOT_ID.to_owned(),
        name: "/".to_owned(),
    }];
    let mut current = VirtualPath::root();
    for segment in dir.segments() {
        if let Ok(next) = current.join(segment) {
            crumbs.push(Breadcrumb {
                id: next.id(),
                name: segment.clone(),
            });
            current = next;
        }
    }
    crumbs
}

/// 目录在前、文件在后；页面按同一顺序渲染，避免排序规则分散在两处。
fn kind_rank(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::Directory => 0,
        EntryKind::File => 1,
    }
}

/// 目录不存在时返回 `dir_not_found`，让页面区分「路径越界」与「目录被移走」。
fn remap_dir_error(error: AppError) -> AppError {
    if matches!(error.code, ErrorCode::NotFound) {
        AppError::new(ErrorCode::DirNotFound, "directory not found")
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 能通过路径段规则的名字必须得到与 `root/…` 形式一致的 id；
    /// 这是页面回传 id 时能被服务端重新解析的前提。
    #[test]
    fn entry_id_round_trips_supported_names() {
        let dir = VirtualPath::parse("root/docs").unwrap();
        // 点开头的名字属于隐藏项，按策略不共享，因此不出现在这个列表里。
        for name in ["note.txt", "报告 2026.txt", "a-b_c.txt", "a.b.txt"] {
            let id = entry_id(&dir, name).unwrap_or_else(|| panic!("{name} must be supported"));
            assert_eq!(id, format!("root/docs/{name}"));
            assert_eq!(VirtualPath::parse(&id).unwrap().id(), id);
        }
    }

    /// 无法安全表示的名字必须被过滤（返回 `None`），而不是生成空 id；
    /// 页面把空 id 视为协议错误，会让整个目录都打不开。
    ///
    /// 这些名字在 Linux/macOS 上合法、在 Windows 上通常无法创建，因此本测试只断言
    /// 纯函数行为，不依赖具体平台能否创建它们。
    #[test]
    fn entry_id_rejects_names_the_rules_cannot_express() {
        let dir = VirtualPath::root();
        for name in [
            "a:b.txt",    // 冒号在 Windows 上是 ADS 分隔符
            "trailing ",  // 尾空格会被 Win32 规范化
            "trailing.",  // 尾点同上
            r"a\b.txt",   // 反斜杠是 Windows 路径分隔符
            "CON",        // Windows 保留设备名
            ".hidden",    // 隐藏项按策略不共享
            ".klt-tmp-1", // 内部暂存前缀
            "a\u{1}b",    // 控制字符
        ] {
            assert_eq!(entry_id(&dir, name), None, "{name}");
        }
    }
}
