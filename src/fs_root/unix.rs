//! Unix 平台实现：逐段 `openat` + `O_NOFOLLOW | O_DIRECTORY`，不覆盖提交优先用
//! 「匿名文件 + linkat」绑定写句柄，macOS 退化为按名改名；不支持原子不覆盖的
//! Unix 明确拒绝而不是退化。
//! @author kongweiguang

use std::{fs::File, io, os::fd::AsFd, path::Path};

use rustix::{
    fs::{AtFlags, FileType, Mode, OFlags, fstat, openat, statat, unlinkat},
    io::Errno,
};

// 提交能力按平台分开导入：Linux 走 fd 链接，macOS 走按名不覆盖改名。
// 合在一起会在另一平台产生未使用导入告警，掩盖真正的遗漏。
#[cfg(target_os = "linux")]
use rustix::fs::linkat;
#[cfg(not(target_os = "linux"))]
use rustix::fs::{RenameFlags, renameat_with};

#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

use super::{DirEntryInfo, DirHandle, EntryKind, OpenedFile, RootIdentity};
use crate::config::TEMP_PREFIX;

/// 打开共享根：要求是目录且不是符号链接。
pub fn open_ambient_dir(path: &Path) -> io::Result<DirHandle> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = openat(rustix::fs::CWD, path, flags, Mode::empty()).map_err(errno)?;
    Ok(DirHandle {
        file: File::from(fd),
    })
}

/// 打开子目录；`O_NOFOLLOW` 让链接本身以 ELOOP 失败，不会被跟随。
pub fn open_dir_child(parent: &DirHandle, name: &str) -> io::Result<DirHandle> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = openat(parent.file.as_fd(), name, flags, Mode::empty()).map_err(errno)?;
    Ok(DirHandle {
        file: File::from(fd),
    })
}

/// 复制目录句柄；dup 保留同一文件描述，不重新解析路径。
pub fn clone_dir(dir: &DirHandle) -> io::Result<DirHandle> {
    let fd = rustix::io::dup(dir.file.as_fd()).map_err(errno)?;
    Ok(DirHandle {
        file: File::from(fd),
    })
}

/// 枚举目录：用 rustix 的 `Dir::read_from` 读取句柄本身，再逐项 no-follow 取元数据。
///
/// `read_from` 内部会重新 `openat(fd, ".")` 得到独立文件描述，因此不会与调用方共享
/// 目录流位置；单个条目的 `statat` 失败（并发删除）按跳过处理，不让整页列表失败。
pub fn read_entries(dir: &DirHandle) -> io::Result<Vec<DirEntryInfo>> {
    read_entries_limited(dir, crate::config::MAX_DIR_ENTRIES)
}

/// 带显式上限的枚举实现；生产路径固定传 `MAX_DIR_ENTRIES`。
///
/// 把上限做成参数是为了能用小数字在真实目录上验证「恰好上限通过、超一条失败」：
/// 若只保留常量，测试要么先造两万个文件（慢且受文件系统限制），要么只能测纯函数
/// 而漏掉「检查位置在 push 之前」这一真实缺陷。
fn read_entries_limited(dir: &DirHandle, limit: usize) -> io::Result<Vec<DirEntryInfo>> {
    let mut reader = rustix::fs::Dir::read_from(dir.file.as_fd()).map_err(errno)?;
    let mut entries = Vec::new();
    loop {
        let Some(entry) = reader.read() else { break };
        let entry = entry.map_err(errno)?;
        let name = match std::str::from_utf8(entry.file_name().to_bytes()) {
            Ok(name) => name,
            // 非 UTF-8 名字无法无损传给浏览器，按不可见处理而不是猜测编码。
            Err(_) => continue,
        };
        if name == "." || name == ".." || crate::path::is_restricted_segment(name) {
            continue;
        }
        let metadata = match statat(dir.file.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let file_type = FileType::from_raw_mode(metadata.st_mode);
        if file_type == FileType::Symlink {
            continue;
        }
        let modified = u64::try_from(metadata.st_mtime).ok();
        if file_type == FileType::Directory {
            entries.push(DirEntryInfo {
                name: name.to_owned(),
                kind: EntryKind::Directory,
                size: None,
                modified,
            });
        } else if file_type == FileType::RegularFile {
            entries.push(DirEntryInfo {
                name: name.to_owned(),
                kind: EntryKind::File,
                size: u64::try_from(metadata.st_size).ok(),
                modified,
            });
        }
        // 上限按「已收录的条目数」计，且必须在收录之后判定：此前写成
        // `len > max` 却放在 push 之前，收录第 max + 1 条时 len 仍是 max，
        // 判定不成立，恰好放行 max + 1 条。现在与 Windows 一致：收录完再看
        // 已收录数，`max` 条通过、第 `max + 1` 条立即失败，不截断冒充完整目录。
        // 上限由参数传入，是为了能用小数字跑真实目录枚举测试。
        if super::exceeds_entry_limit(entries.len(), limit) {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "directory has too many entries",
            ));
        }
    }
    Ok(entries)
}

/// 打开普通文件：`O_NONBLOCK` 避免阻塞在 FIFO，`fstat` 后拒绝非普通文件与硬链接。
pub fn open_file_child(parent: &DirHandle, name: &str) -> io::Result<OpenedFile> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = openat(parent.file.as_fd(), name, flags, Mode::empty()).map_err(errno)?;
    let metadata = fstat(fd.as_fd()).map_err(errno)?;
    let file_type = FileType::from_raw_mode(metadata.st_mode);
    if file_type != FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "not a regular file",
        ));
    }
    if metadata.st_nlink != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "hard linked file",
        ));
    }
    Ok(OpenedFile {
        file: File::from(fd),
        size: u64::try_from(metadata.st_size).unwrap_or(0),
        modified: u64::try_from(metadata.st_mtime).ok(),
    })
}

/// 创建暂存文件：Linux 优先匿名 `O_TMPFILE`，其余平台回退到独占命名的暂存文件。
///
/// 匿名文件没有目录项，同目录的并发写入者无法「按名字」替换它；提交时再以
/// `/proc/self/fd/<fd>` 把这个已打开的对象链接到目标名，身份由 fd 保证。
/// `O_TMPFILE` 在部分文件系统（旧内核、网络文件系统等）不可用，此时回退到
/// 命名暂存：提交仍走 `/proc/self/fd`，创建期多出的目录项即使被替换，`linkat`
/// 链接的仍是本进程持有的 inode，提交内容依然正确。
pub fn create_staging(dir: &DirHandle) -> io::Result<(String, File)> {
    #[cfg(target_os = "linux")]
    if let Some(created) = create_anonymous(dir)? {
        return Ok(created);
    }
    create_named_staging(dir)
}

/// 独占命名的暂存文件；`O_EXCL` 保证不会复用已有名字。
///
/// 单独拆出是为了让测试能强制走命名路径：Linux 生产路径优先匿名，若测试复用
/// `create_staging` 就永远构造不出「按名替换」的攻击场景，用例会失去判别力。
fn create_named_staging(dir: &DirHandle) -> io::Result<(String, File)> {
    for attempt in 0..32u32 {
        let name = format!("{TEMP_PREFIX}{}", super::random_token(attempt));
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match openat(
            dir.file.as_fd(),
            name.as_str(),
            flags,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(fd) => return Ok((name, File::from(fd))),
            Err(Errno::EXIST) => continue,
            Err(error) => return Err(errno(error)),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "staging name exhausted",
    ))
}

/// 尝试创建匿名暂存文件；平台或文件系统不支持时返回 `None` 让调用方回退。
///
/// 只有「不支持」类错误才回退：权限、配额、只读等错误必须如实上报，否则会把
/// 真实故障伪装成「平台不支持」，让用户看到错误的处置建议。
#[cfg(target_os = "linux")]
fn create_anonymous(dir: &DirHandle) -> io::Result<Option<(String, File)>> {
    let flags = OFlags::WRONLY | OFlags::TMPFILE | OFlags::CLOEXEC;
    match openat(dir.file.as_fd(), ".", flags, Mode::RUSR | Mode::WUSR) {
        Ok(fd) => Ok(Some((String::new(), File::from(fd)))),
        Err(Errno::OPNOTSUPP | Errno::NOSYS | Errno::INVAL) => Ok(None),
        Err(error) => Err(errno(error)),
    }
}

/// 不覆盖提交：源对象是创建暂存时返回的**写句柄**，而不是按名字重新定位。
///
/// 这是上传身份绑定的落点。按名字 `renameat` 时，同目录的并发写入者若在
/// 「创建/写入」与「提交」之间把同名暂存文件换掉，提交的就是别人的字节；
/// 暂存名不可预测只提高成本，不构成身份证明。
///
/// - Linux：`linkat(AT_FDCWD, "/proc/self/fd/<fd>", dirfd, candidate, 0)` 把
///   已打开 inode 以 `candidate` 链接进目录，随后仅在暂存目录项仍指向同一 inode 时
///   删除它；目标已存在时 `linkat` 以 `EEXIST` 失败，等价于不覆盖。若目标文件系统
///   不提供 procfs，链接会失败，此时不降级成按名改名，而是如实报错。
/// - macOS：没有 `O_TMPFILE` 与 fd 链接入口，只能按名 `renameatx_np(RENAME_EXCL)`；
///   这仍保证不覆盖，但无法证明「提交的是本次写入的字节」，已在文档中如实标注。
pub fn commit_staging(
    dir: &DirHandle,
    staging_file: File,
    staging: &str,
    desired: &str,
) -> io::Result<String> {
    for candidate in super::rename_candidates(desired) {
        match commit_one(dir, &staging_file, staging, &candidate) {
            Ok(()) => {
                // 链接成功后暂存目录项（若存在）仍需删除；匿名文件没有名字，跳过。
                // 但不能无条件按名删除：同目录并发写入者可能已把暂存名换成别的文件，
                // 按名 unlink 会删掉别人的文件。只有目录项仍指向我们写入的 inode
                // 时才删除，身份判断与提交本身用同一份 fstat 结果。
                if !staging.is_empty() {
                    remove_staging_if_same(dir, &staging_file, staging);
                }
                return Ok(candidate);
            }
            Err(Errno::EXIST) => continue,
            Err(error) => return Err(errno(error)),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "rename candidates exhausted",
    ))
}

/// Linux：删除仍属于本次上传的暂存目录项；名字已被替换时保持原样。
///
/// 为什么不能直接按名 `unlinkat`：提交用 fd 绑定保证了「提交的是本次写入的字节」，
/// 但清理若仍按名删除，同目录并发写入者把暂存名换成别的文件后，我们会删掉别人的
/// 文件——身份绑定在清理这一步失效。这里先比较 `st_dev + st_ino`，只有目录项仍
/// 指向我们持有的 inode 才删除。
///
/// 残留窗口（如实记录，不假装已消除）：`statat` 与 `unlinkat` 之间仍可能被替换，
/// 因此这是「尽力而为的清理」，不是安全保证。它只影响共享目录里的隐藏暂存名，
/// 不改变提交内容与不覆盖语义；真正需要证明的提交身份已由 fd 绑定保证。
#[cfg(target_os = "linux")]
fn remove_staging_if_same(dir: &DirHandle, staging_file: &File, staging: &str) {
    let _ = remove_staging_if_owned(dir, staging, staging_file);
}

/// 非 Linux 平台：提交用按名改名，成功后暂存名已不存在，没有可清理的目录项。
#[cfg(not(target_os = "linux"))]
fn remove_staging_if_same(_dir: &DirHandle, _staging_file: &File, _staging: &str) {}

/// Linux 提交单个候选名：以 `/proc/self/fd/<fd>` 链接已打开的暂存 inode。
///
/// `linkat` 默认不跟随符号链接：procfs 里的 fd 条目是符号链接，因此必须带
/// `AT_SYMLINK_FOLLOW` 才会链接到真实 inode。目标已存在时内核返回 `EEXIST`，
/// 天然满足「不覆盖」；这里不做「先 stat 再链接」的检查，那会重新引入竞态。
#[cfg(target_os = "linux")]
fn commit_one(
    dir: &DirHandle,
    staging_file: &File,
    _staging: &str,
    candidate: &str,
) -> Result<(), Errno> {
    // `/proc` 未挂载或 fd 条目不可用时，`linkat` 以 ENOENT 失败。这里不把它
    // 改写成其它错误码：ENOENT 也可能来自「目标目录在提交期间被删除」，两者
    // 无法只凭 errno 区分，强行改写会掩盖真实故障。无论哪种，提交都失败关闭，
    // 不会降级成按名改名（那会重新引入「提交了别人的字节」的竞态）。
    let source = format!("/proc/self/fd/{}", staging_file.as_raw_fd());
    linkat(
        rustix::fs::CWD,
        source.as_str(),
        dir.file.as_fd(),
        candidate,
        AtFlags::SYMLINK_FOLLOW,
    )
}

/// macOS 及其它 Unix 没有 fd 链接入口，只能按名 `RENAME_NOREPLACE` 提交。
///
/// 不覆盖仍由内核保证，但源对象按名字解析，无法证明「提交的就是本次写入的字节」；
/// 该平台的这一限制在 `SECURITY.md` 中如实标注，不在此处伪装成 fd 绑定。
#[cfg(not(target_os = "linux"))]
fn commit_one(
    dir: &DirHandle,
    _staging_file: &File,
    staging: &str,
    candidate: &str,
) -> Result<(), Errno> {
    renameat_with(
        dir.file.as_fd(),
        staging,
        dir.file.as_fd(),
        candidate,
        RenameFlags::NOREPLACE,
    )
}

/// 删除暂存文件；不带 `REMOVEDIR`，因此只可能删除非目录项。
pub fn remove_child(dir: &DirHandle, name: &str) -> io::Result<()> {
    unlinkat(dir.file.as_fd(), name, AtFlags::empty()).map_err(errno)
}

/// 仅当目录项仍指向 `owned` 的 inode 时删除；返回是否删除。
///
/// 这里按「先打开、再比较、最后删除」执行：先以 `O_NOFOLLOW` 打开当前目录项，
/// 用 `fstat` 比较 `st_dev`/`st_ino` 与持有句柄是否一致，确认是我们的文件才
/// 按名删除。名字被替换时保留目录项并返回 `false`，由调用方如实记录残留。
///
/// Unix 没有「按 fd 删除」的接口，比较与 `unlinkat` 之间仍有一个可能被替换的窗口；
/// 这是尽力而为的清理，不是安全保证。共享目录里的隐藏暂存名与提交内容无关，
/// 提交身份由 `commit_staging` 的 fd 绑定（Linux）或按名不覆盖改名（macOS）负责。
pub fn remove_staging_if_owned(dir: &DirHandle, name: &str, owned: &File) -> io::Result<bool> {
    if name.is_empty() {
        // 匿名暂存没有目录项，关闭写句柄即释放；这里没有可删除的对象。
        return Ok(false);
    }
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let fd = match openat(dir.file.as_fd(), name, flags, Mode::empty()) {
        Ok(fd) => fd,
        // 目录项已不存在：没有可清理的对象。
        Err(Errno::NOENT) => return Ok(false),
        Err(error) => return Err(errno(error)),
    };
    let ours = fstat(owned.as_fd()).map_err(errno)?;
    let candidate = fstat(fd.as_fd()).map_err(errno)?;
    if candidate.st_dev != ours.st_dev || candidate.st_ino != ours.st_ino {
        // 名字已被换成本地写入者的文件：保持原样。
        return Ok(false);
    }
    drop(fd);
    unlinkat(dir.file.as_fd(), name, AtFlags::empty()).map_err(errno)?;
    Ok(true)
}

/// 目录身份：dev + ino。
///
/// `dev_t` 宽度平台不同（Linux 是 `u64`，macOS 是 `i32`），而 `RootIdentity` 统一用
/// `u64` 承载。这里按平台分别取值：Linux 直接用原值，macOS 先按位转成无符号再扩展，
/// 避免 `as u64` 对负值做符号扩展后与真实设备号不一致。该字段只用于诊断与
/// 「根是否仍是同一目录」的粗粒度比较，不参与任何权限判断。
pub fn dir_identity(dir: &DirHandle) -> io::Result<RootIdentity> {
    let metadata = fstat(dir.file.as_fd()).map_err(errno)?;
    #[cfg(target_os = "linux")]
    let device = metadata.st_dev;
    #[cfg(target_os = "macos")]
    let device = metadata.st_dev as u32 as u64;
    Ok(RootIdentity::new(device, metadata.st_ino))
}

/// 把 rustix 的 Errno 转成 std io::Error，保留错误种类供上层映射。
fn errno(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// 真实目录上的条目上限语义：恰好 `limit` 条通过，第 `limit + 1` 条立即失败。
    ///
    /// 这是 `len > max` 放在 push 之前那一缺陷的判别性回归：旧实现会在第
    /// `limit + 1` 条时看到 `len == limit` 而放行，列表因此多出一条。用 `limit = 3`
    /// 在真实目录上枚举，不依赖先造两万个文件，也不靠纯函数断言代替真实读取路径。
    #[test]
    fn real_directory_entry_limit_is_exact() {
        let directory = tempfile::tempdir().unwrap();
        let handle = open_ambient_dir(directory.path()).unwrap();
        for index in 0..3 {
            std::fs::write(directory.path().join(format!("f{index}.txt")), b"x").unwrap();
        }
        assert_eq!(read_entries_limited(&handle, 3).unwrap().len(), 3);
        std::fs::write(directory.path().join("f3.txt"), b"x").unwrap();
        let error = read_entries_limited(&handle, 3).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        // 生产入口仍按固定上限工作：4 个条目远低于 20000，必须完整返回。
        assert_eq!(read_entries(&handle).unwrap().len(), 4);
    }

    /// Linux 的提交必须绑定写句柄：同目录并发替换同名暂存文件不能改变提交内容。
    ///
    /// 攻击模型：本插件创建暂存后，本地写入者把同名目录项换掉。fd 绑定提交经
    /// `/proc/self/fd/<fd>` 链接我们自己的 inode，诱饵内容不会被提交。
    /// 这里显式使用命名暂存，保证「按名替换」这个前提成立；匿名文件本身没有
    /// 目录项，不存在该攻击面（另有用例覆盖）。
    #[test]
    fn fd_bound_commit_ignores_replaced_staging_name() {
        let directory = tempfile::tempdir().unwrap();
        let handle = open_ambient_dir(directory.path()).unwrap();
        let (name, mut file) = create_named_staging(&handle).unwrap();
        file.write_all(b"ours").unwrap();
        file.sync_all().unwrap();
        // 把暂存目录项挪走，再用同名诱饵占位；写句柄仍指向我们写入的 inode。
        std::fs::rename(directory.path().join(&name), directory.path().join("moved")).unwrap();
        std::fs::write(directory.path().join(&name), b"theirs").unwrap();

        assert_eq!(
            commit_staging(&handle, file, &name, "note.txt").unwrap(),
            "note.txt"
        );
        assert_eq!(
            std::fs::read(directory.path().join("note.txt")).unwrap(),
            b"ours",
            "commit must submit the bytes written through our handle"
        );
        assert_eq!(
            std::fs::read(directory.path().join(&name)).unwrap(),
            b"theirs",
            "replacement must be untouched"
        );
    }

    /// 匿名暂存没有目录项，提交必须链接本进程持有的 inode 并清理临时状态。
    #[test]
    fn anonymous_staging_commits_and_leaves_no_staging_name() {
        let directory = tempfile::tempdir().unwrap();
        let handle = open_ambient_dir(directory.path()).unwrap();
        let (name, mut file) = create_anonymous(&handle)
            .unwrap()
            .expect("linux should support anonymous staging on tmpfs/ext4");
        file.write_all(b"anonymous").unwrap();
        file.sync_all().unwrap();
        assert!(
            name.is_empty(),
            "anonymous staging must have no directory name"
        );
        assert_eq!(
            commit_staging(&handle, file, &name, "note.txt").unwrap(),
            "note.txt"
        );
        assert_eq!(
            std::fs::read(directory.path().join("note.txt")).unwrap(),
            b"anonymous"
        );
        // 目录里只应留下提交后的文件，没有可见的暂存残留。
        let visible = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !name.starts_with('.'))
            .collect::<Vec<_>>();
        assert_eq!(visible, vec!["note.txt".to_owned()]);
    }

    /// 身份绑定清理的判别性回归：名字被换后必须保留诱饵，不按名误删。
    ///
    /// 旧清理路径直接按暂存名 `unlinkat`，本地写入者把同名目录项换成自己的文件后
    /// 会被误删。这里分别验证两个方向：同名诱饵占位时保持原样返回 `false`；名字
    /// 仍指向持有 inode 时才真正删除并返回 `true`。残留窗口（stat 与 unlink 之间）
    /// 无法在进程内确定性制造，按文档如实标注，不在这里伪装成已消除。
    #[test]
    fn owned_cleanup_spares_replaced_name_and_removes_same_inode() {
        let directory = tempfile::tempdir().unwrap();
        let handle = open_ambient_dir(directory.path()).unwrap();
        let (name, mut file) = create_named_staging(&handle).unwrap();
        file.write_all(b"ours").unwrap();
        file.sync_all().unwrap();

        // 用另一个自己的文件占用暂存名，模拟同目录并发写入者的替换。
        std::fs::rename(directory.path().join(&name), directory.path().join("moved")).unwrap();
        std::fs::write(directory.path().join(&name), b"decoy").unwrap();
        assert!(
            !remove_staging_if_owned(&handle, &name, &file).unwrap(),
            "replaced name must be kept"
        );
        assert_eq!(
            std::fs::read(directory.path().join(&name)).unwrap(),
            b"decoy",
            "decoy must survive identity-bound cleanup"
        );
        assert_eq!(
            std::fs::read(directory.path().join("moved")).unwrap(),
            b"ours",
            "original staging material must be preserved"
        );

        // 目录项仍指向持有 inode 时（此处即 moved 的原名路径）：恢复原名后应删除。
        std::fs::remove_file(directory.path().join(&name)).unwrap();
        std::fs::rename(directory.path().join("moved"), directory.path().join(&name)).unwrap();
        assert!(remove_staging_if_owned(&handle, &name, &file).unwrap());
        assert!(
            !directory.path().join(&name).exists(),
            "same-inode entry must be removed"
        );

        // 目录项已不存在：按「无可清理对象」返回 false，而不是报错。
        assert!(!remove_staging_if_owned(&handle, &name, &file).unwrap());
    }
}
