//! `POST /api/upload`：单文件流式上传、暂存与不覆盖提交。
//! @author kongweiguang
//!
//! 语义边界：
//! - 请求体 100% 只表示字节已收到，不等于文件已提交；只有暂存写完、刷盘、改名成功
//!   并读回校验后才返回 2xx。
//! - 提交前失败返回明确错误；提交开始后失败无法断言「未提交」，因此不再返回成功，
//!   但也不会谎称可以安全重试。
//! - 文件名只来自查询串并按同一套路径段规则校验，绝不参与拼接绝对路径。

use std::{io::ErrorKind, sync::Arc};

use axum::{
    body::Body,
    extract::{Query, State},
    http::HeaderMap,
    response::Response,
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::{
    config::MAX_UPLOAD_BYTES,
    error::{AppError, ErrorCode},
    http::{AppContext, auth::authenticate, error_response, json_response, try_acquire_upload},
    path::{VirtualPath, validate_upload_name},
};

#[derive(Deserialize)]
pub struct UploadQuery {
    #[serde(default)]
    dir: Option<String>,
    name: String,
}

#[derive(Serialize)]
pub struct UploadResponse {
    name: String,
    size: u64,
}

/// 上传接口。鉴权与路径校验都在读取请求体之前完成，避免为非法请求白白写入磁盘。
pub async fn upload(
    State(context): State<Arc<AppContext>>,
    headers: HeaderMap,
    Query(query): Query<UploadQuery>,
    body: Body,
) -> Response {
    // 同源由中间件统一校验（见 `http::guard`），这里只做鉴权与参数校验；
    // 两处都写会让「哪个才是权威」变得含糊，也容易被后续修改漏掉一处。
    if let Err(error) = authenticate(&context, &headers) {
        return error_response(error);
    }
    let dir = match VirtualPath::parse(query.dir.as_deref().unwrap_or("root")) {
        Ok(dir) => dir,
        Err(error) => return error_response(error),
    };
    let name = match validate_upload_name(&query.name) {
        Ok(name) => name,
        Err(error) => return error_response(error),
    };
    let _permit = match try_acquire_upload(&context) {
        Ok(permit) => permit,
        Err(error) => return error_response(error),
    };
    match store(&context, &dir, &name, body).await {
        Ok(response) => {
            context.state.count_upload();
            json_response(response)
        }
        Err(error) => error_response(error),
    }
}

/// 真正写盘：暂存 → 校验 → 不覆盖提交。
///
/// 清理语义（这是本函数最容易出错的部分）：
/// - 暂存文件的清理只通过**创建时的父目录句柄**执行，绝不按虚拟路径重新打开目录：
///   目录在请求期间被替换时，重新打开会删到另一个目录里的同名文件。
/// - 清理放在 RAII 守卫里，因此客户端断开导致 handler future 被直接 drop 时也会执行；
///   只在 `Err` 分支清理会漏掉这条路径，留下永久隐藏的暂存文件。
/// - 一旦开始提交就解除守卫：提交结果未知时保留暂存文件供人工排查，不盲删、不重试。
/// - 提交以创建时返回的写句柄为源对象（Windows 直接改名句柄，Linux 走
///   `/proc/self/fd`）：同目录并发写入者替换同名暂存文件也不能改变提交内容。
async fn store(
    context: &AppContext,
    dir: &VirtualPath,
    name: &str,
    body: Body,
) -> Result<UploadResponse, AppError> {
    let staging = context.root.create_staging(dir)?;
    // 守卫持有父目录句柄、暂存名与写句柄的独立克隆；`disarm()` 之后不再删除。
    // 身份克隆必须在写句柄交出去之前完成：清理时要证明「目录项仍是本请求创建的
    // 那个文件对象」，只能拿创建时同一 inode 的句柄来比较，事后按名重开没有意义。
    let identity = staging
        .file
        .try_clone()
        .map_err(|error| AppError::io(error))?;
    let mut guard = StagingGuard::new(&staging.dir, staging.staging.clone(), identity);
    // 同步句柄转成 tokio 文件才能用异步读写；底层仍是同一个打开的文件对象。
    let mut file = tokio::fs::File::from_std(staging.file);
    let size = match write_body(&mut file, body, context).await {
        Ok(size) => size,
        Err(error) => return Err(error),
    };
    // 刷盘在提交之前完成：提交后的目录项持久性由平台决定（Windows 无目录 fsync），
    // 但至少保证文件内容已经交给内核，而不是留在进程缓冲里。
    if let Err(error) = file.flush().await {
        return Err(map_write_error(error));
    }
    if let Err(error) = file.sync_all().await {
        return Err(map_write_error(error));
    }
    // 刷盘完成后、提交开始前的最后一个取消检查点。提交一旦开始就没有「撤销」
    // 语义：Windows 的改名与 Linux 的 linkat 都是内核里不可中断的单一操作，
    // 停止流程只能在提交返回后如实记录结果，不能宣称把它撤销了。
    if !context.state.is_running() || context.run.is_cancelled() {
        return Err(AppError::new(ErrorCode::Unavailable, "service stopped"));
    }
    // 交回标准句柄供平台提交使用：Windows 用它做改名源，Linux 用它定位
    // `/proc/self/fd`。`into_std` 会先完成在途操作，避免句柄状态不确定。
    let file = file.into_std().await;
    // 提交开始后无法断言「未提交」：失败时保留暂存文件供人工排查，
    // 不盲目删除，也不自动重试（自动重试可能产生重复文件）。
    guard.disarm();
    // 提交以创建时的父目录句柄为接收者，写句柄按值传入：句柄是唯一权威的目录与
    // 源对象，调用方无法再额外传一个「可能不是同一个」的目录参数。
    let final_name = staging.dir.commit_staging(file, &staging.staging, name)?;
    Ok(UploadResponse {
        name: final_name,
        size,
    })
}

/// 暂存文件清理守卫。
///
/// 之所以不用 `?` + 显式清理：`?` 的每个分支都要记得清理，而 future 被 drop 时
/// 一个分支都不会执行。Drop 里用平台句柄删除，与异步运行时无关，也不会阻塞 UI
/// （只删除本插件创建的小文件）。
struct StagingGuard<'a> {
    /// 创建暂存文件时的父目录句柄。借用而不是克隆：句柄生命周期覆盖整个请求，
    /// 借用可以保证守卫不会比它指向的目录活得更久。
    handle: &'a crate::fs_root::DirHandle,
    staging: String,
    /// 写句柄的身份克隆。清理时用它做身份比对：同目录并发写入者可能把暂存名
    /// 换成自己的文件，按名删除会误删别人的文件；比对 dev/ino（或卷序列号+
    /// 文件索引）一致才删除，不一致时保留材料并如实记录。
    identity: std::fs::File,
    armed: bool,
}

impl<'a> StagingGuard<'a> {
    fn new(handle: &'a crate::fs_root::DirHandle, staging: String, identity: std::fs::File) -> Self {
        Self {
            handle,
            staging,
            identity,
            armed: true,
        }
    }

    /// 提交即将开始：结果未知，保留材料而不是删除。
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingGuard<'_> {
    fn drop(&mut self) {
        // Linux 匿名暂存没有名字（`staging` 为空），不满足前缀条件即跳过：
        // 它的释放完全由写句柄的 Drop 负责，不需要也不能按名删除。
        if !self.armed || !self.staging.starts_with(crate::config::TEMP_PREFIX) {
            return;
        }
        // 失败只影响遗留的隐藏暂存文件，不能在这里 panic（panic 发生在 drop 中会让
        // 整个请求任务异常）。身份不匹配时 `remove_staging_if_owned` 返回 false，
        // 目录项保留——误删别人的文件比多留一个隐藏暂存文件严重得多；这里无法
        // 引入日志设施，残余材料由固定前缀与「同一父目录句柄」两条约束保证可识别。
        let _ = self
            .handle
            .remove_staging_if_owned(&self.staging, &self.identity);
    }
}

/// 流式写入并累计字节数；超过上限立即中止，避免先写满磁盘再报错。
///
/// 取消检查点刻意放在三处：每个非空数据块之前（长上传在传输中途被停止时尽快退出）、
/// 空请求体到达时（零字节上传也必须被停止拒绝，否则「没有数据」会被当成无事发生）、
/// 以及流结束后（EOF 到 flush/commit 之间仍有一段真实的磁盘工作时间）。EOF 之后的
/// 检查返回与流中途相同的错误：调用方据此走守卫清理路径，保留暂存材料。
async fn write_body(
    file: &mut tokio::fs::File,
    body: Body,
    context: &AppContext,
) -> Result<u64, AppError> {
    use futures_util::StreamExt as _;
    let mut stream = body.into_data_stream();
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        if !context.state.is_running() || context.run.is_cancelled() {
            return Err(AppError::new(ErrorCode::Unavailable, "service stopped"));
        }
        let chunk = chunk.map_err(|_| AppError::new(ErrorCode::BadRequest, "body read failed"))?;
        if chunk.is_empty() {
            continue;
        }
        total = total.saturating_add(chunk.len() as u64);
        if total > MAX_UPLOAD_BYTES {
            return Err(AppError::new(ErrorCode::TooLarge, "upload too large"));
        }
        file.write_all(&chunk).await.map_err(map_write_error)?;
    }
    // EOF 之后再查一次：从最后一个数据块到此刻之间，停止请求随时可能到达；
    // 不检查会让「已收到停止信号」的上传继续进入刷盘与提交阶段。
    if !context.state.is_running() || context.run.is_cancelled() {
        return Err(AppError::new(ErrorCode::Unavailable, "service stopped"));
    }
    Ok(total)
}

/// 写入错误映射：磁盘满与配额统一成 `quota`，其余保留类型化错误。
fn map_write_error(error: std::io::Error) -> AppError {
    match error.kind() {
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => {
            AppError::new(ErrorCode::Quota, "storage full")
        }
        ErrorKind::FileTooLarge => AppError::new(ErrorCode::TooLarge, "file too large"),
        other => AppError::new(ErrorCode::Server, format!("write failed: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_root::ShareRoot;
    use std::io::Write as _;

    /// 在给定目录里放一个带固定前缀的命名暂存文件，供守卫清理用例使用。
    ///
    /// Linux 生产路径优先匿名暂存（没有目录项），无法验证「按名清理」；这里直接
    /// 写一个命名文件来固定守卫的删除语义。匿名暂存由写句柄 Drop 释放，不经过
    /// 守卫，属于另一条（更简单的）回收路径。返回句柄以便测试把守卫与真实
    /// inode 绑定（生产路径用的是写句柄的身份克隆）。
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    fn named_staging(share: &std::path::Path) -> (String, std::fs::File) {
        let name = format!("{}.test", crate::config::TEMP_PREFIX);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(share.join(&name))
            .unwrap();
        file.write_all(b"staged").unwrap();
        file.sync_all().unwrap();
        let identity = file.try_clone().unwrap();
        (name, identity)
    }

    /// 守卫必须在「尚未提交」时删除暂存文件，且只删除带固定前缀的名字；
    /// 这是客户端断开导致 future 被 drop 时的唯一清理路径。
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn guard_removes_staging_when_dropped() {
        let root = tempfile::tempdir().unwrap();
        let share = root.path().join("share");
        std::fs::create_dir(&share).unwrap();
        let root_handle = ShareRoot::open(&share).unwrap();
        let dir = root_handle
            .open_dir(&crate::path::VirtualPath::root())
            .unwrap();
        let (staging, identity) = named_staging(&share);
        assert!(share.join(&staging).exists());
        {
            let _guard = StagingGuard::new(&dir, staging.clone(), identity);
        }
        assert!(!share.join(&staging).exists(), "staging must be removed");
    }

    /// 提交开始后解除守卫：结果未知时保留暂存文件供人工排查，不能盲删。
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn guard_keeps_staging_after_disarm() {
        let root = tempfile::tempdir().unwrap();
        let share = root.path().join("share");
        std::fs::create_dir(&share).unwrap();
        let root_handle = ShareRoot::open(&share).unwrap();
        let dir = root_handle
            .open_dir(&crate::path::VirtualPath::root())
            .unwrap();
        let (staging, identity) = named_staging(&share);
        {
            let mut guard = StagingGuard::new(&dir, staging.clone(), identity);
            guard.disarm();
        }
        assert!(share.join(&staging).exists(), "staging must be preserved");
    }

    /// 清理必须使用创建时的父目录句柄，而不是按虚拟路径重新打开：
    /// 目录在请求期间被替换时，重新打开会删到另一个目录里的同名文件。
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn guard_removes_from_original_directory_after_replacement() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        std::fs::create_dir(&first).unwrap();
        let handle = ShareRoot::open(&first).unwrap();
        let dir = handle.open_dir(&crate::path::VirtualPath::root()).unwrap();
        let (staging, identity) = named_staging(&first);

        // Windows 不允许目录内仍有打开的文件句柄时重命名该目录（MoveFileEx 返回
        // ACCESS_DENIED），因此先释放身份句柄；重命名后按新路径重新打开同一文件，
        // 身份比较仍然成立，守卫依旧必须使用创建时的目录句柄删除。
        drop(identity);
        // 用同名的新目录替换原目录：新目录里放一个同名文件作为「错误目标」。
        let moved = root.path().join("moved");
        std::fs::rename(&first, &moved).unwrap();
        std::fs::create_dir(&first).unwrap();
        std::fs::write(first.join(&staging), b"decoy").unwrap();
        let identity = std::fs::File::open(moved.join(&staging)).unwrap();

        {
            let _guard = StagingGuard::new(&dir, staging.clone(), identity);
        }
        // 原目录（已被改名）里的暂存文件被删除；新目录里的同名文件不受影响。
        assert!(
            !moved.join(&staging).exists(),
            "original staging must be removed"
        );
        assert!(
            first.join(&staging).exists(),
            "replacement must be untouched"
        );
    }

    /// 暂存名被同目录并发写入者换掉时，守卫必须保留诱饵而不是按名误删；
    /// 这是身份绑定清理的判别性回归（Windows 验证同一句柄删除，Linux 验证
    /// dev+ino 比对，平台差异由各自的 `remove_staging_if_owned` 承担）。
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn guard_spares_staging_name_swapped_by_local_writer() {
        let root = tempfile::tempdir().unwrap();
        let share = root.path().join("share");
        std::fs::create_dir(&share).unwrap();
        let root_handle = ShareRoot::open(&share).unwrap();
        let dir = root_handle
            .open_dir(&crate::path::VirtualPath::root())
            .unwrap();
        let (staging, identity) = named_staging(&share);
        // 把暂存目录项挪走，再用同名诱饵占位；身份句柄仍指向我们的 inode。
        std::fs::rename(share.join(&staging), share.join("moved")).unwrap();
        std::fs::write(share.join(&staging), b"theirs").unwrap();

        {
            let _guard = StagingGuard::new(&dir, staging.clone(), identity);
        }
        assert_eq!(
            std::fs::read(share.join(&staging)).unwrap(),
            b"theirs",
            "decoy must survive identity-bound cleanup"
        );
        assert_eq!(
            std::fs::read(share.join("moved")).unwrap(),
            b"staged",
            "staging material must be preserved for diagnosis"
        );
    }
}
