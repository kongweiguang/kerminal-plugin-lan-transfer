//! 其他 Unix 的保守实现：只实现可以证明安全的只读能力，其余一律拒绝。
//! @author kongweiguang
//!
//! 这些平台没有 `renameat2`/`renameatx_np` 级别的原子不覆盖提交，也没有与
//! Linux/macOS 同等的逐段句柄验证测试覆盖。按设计文档要求「无法证明安全时拒绝该操作」，
//! 这里直接返回 `Unsupported`，而不是用「先 stat 再 rename」冒充原子性。

use super::{DirEntryInfo, DirHandle, OpenedFile, RootIdentity};
use std::{fs::File, io, path::Path};

/// 无法证明安全的平台直接拒绝启动，避免共享根在未知语义下被当作已隔离。
pub fn open_ambient_dir(_path: &Path) -> io::Result<DirHandle> {
    Err(unsupported())
}

pub fn clone_dir(_dir: &DirHandle) -> io::Result<DirHandle> {
    Err(unsupported())
}

pub fn open_dir_child(_parent: &DirHandle, _name: &str) -> io::Result<DirHandle> {
    Err(unsupported())
}

pub fn read_entries(_dir: &DirHandle) -> io::Result<Vec<DirEntryInfo>> {
    Err(unsupported())
}

pub fn open_file_child(_parent: &DirHandle, _name: &str) -> io::Result<OpenedFile> {
    Err(unsupported())
}

pub fn create_staging(_dir: &DirHandle) -> io::Result<(String, File)> {
    Err(unsupported())
}

/// 这些平台没有可证明安全的提交路径，连按名改名也不提供：直接拒绝，
/// 不把「先检查再改名」冒充原子不覆盖。参数与支持的平台保持一致，便于上层统一调用。
pub fn commit_staging(
    _dir: &DirHandle,
    _staging_file: File,
    _staging: &str,
    _desired: &str,
) -> io::Result<String> {
    Err(unsupported())
}

pub fn remove_child(_dir: &DirHandle, _name: &str) -> io::Result<()> {
    Err(unsupported())
}

/// 保守平台没有可用的目录项操作，身份绑定清理同样拒绝；`Ok(false)` 会让调用方
/// 误以为「名字已换，保留材料」是正常结果，掩盖平台不支持的事实。
pub fn remove_staging_if_owned(
    _dir: &DirHandle,
    _name: &str,
    _owned: &File,
) -> io::Result<bool> {
    Err(unsupported())
}

pub fn dir_identity(_dir: &DirHandle) -> io::Result<RootIdentity> {
    Err(unsupported())
}

/// 固定错误种类，让上层把它映射成可见的「平台不支持」而不是内部错误。
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "platform lacks verified handle-relative file operations",
    )
}
