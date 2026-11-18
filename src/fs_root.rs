//! 共享根句柄与平台安全打开。
//! @author kongweiguang
//!
//! 安全模型：启动时打开共享根并长期持有目录句柄；之后所有访问都从该句柄逐段打开，
//! 每一段都要求「最后一段不是链接/重解析点」。绝不使用 `canonicalize` 加字符串前缀
//! 比较——那只能证明「调用时」的解析结果，无法阻止目录在检查后被替换（TOCTOU）。
//!
//! 根被替换时，已持有的句柄仍指向原目录，因此服务不会转向新目标；但实现**不会**
//! 在运行期重新校验路径身份，`RootIdentity` 只用于诊断。换句话说「根被换成别处」
//! 的后果是继续服务原目录，而不是立即失败——这是钉住原根的取舍，不是承诺。
//!
//! 平台能力差异（这是实现事实，不是承诺）：
//! - Windows：使用 `NtCreateFile` 相对父句柄打开，并带 `OBJ_DONT_REPARSE`，由内核在
//!   解析路径时拒绝任何重解析点，避免「打开后再 lstat」的窗口。枚举、创建、重命名与
//!   删除同样以句柄为根，不把句柄还原成路径字符串。Windows 不提供目录 fsync，崩溃后
//!   的目录项持久性由文件系统决定，这里不声称已保证。
//! - Linux：逐段 `openat` + `O_NOFOLLOW | O_DIRECTORY`；最终文件 `O_NONBLOCK` 后再
//!   `fstat`，拒绝 FIFO/设备；上传暂存优先匿名 `O_TMPFILE`，提交经 `/proc/self/fd/<fd>`
//!   链接到目标名，源对象由 fd 绑定，不覆盖由 `EEXIST` 保证。
//! - macOS：`renameatx_np(RENAME_EXCL)` 提供不覆盖提交；逐段 `openat` + `O_NOFOLLOW`。
//!   macOS 没有 `O_TMPFILE` 与 fd 链接入口，提交按暂存名解析源对象，身份无法绑定。
//! - 其他 Unix：保守拒绝不覆盖提交，明确返回错误而不是用「先 stat 再 rename」冒充原子性。

use std::{fs::File, io, path::Path};

use crate::{
    config::TEMP_PREFIX,
    error::{AppError, AppResult},
    path::{VirtualPath, is_restricted_segment},
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "fs_root/unix.rs"]
mod platform;
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
#[path = "fs_root/unsupported.rs"]
mod platform;
#[cfg(windows)]
#[path = "fs_root/windows.rs"]
mod platform;

/// 目录项类型；只暴露页面需要的两种，其余一律拒绝。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
}

/// 枚举出的单个条目，名称已通过隐藏项与暂存前缀过滤。
#[derive(Clone, Debug)]
pub struct DirEntryInfo {
    pub name: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    /// Unix 秒；平台不提供修改时间时为 `None`。
    pub modified: Option<u64>,
}

/// 已安全打开的普通文件；句柄绑定到具体 inode/文件对象，不受后续路径替换影响。
pub struct OpenedFile {
    pub file: File,
    pub size: u64,
    pub modified: Option<u64>,
}

/// 已创建但尚未提交的暂存资源包。
///
/// 把父目录句柄、写句柄与暂存名捆绑在一起，是因为三者在提交与清理时必须同源：
/// 分开后调用方可能在句柄存活前丢失名字，或在句柄失效后仍按名字操作——后者正是
/// 并发写入者场景下「提交了别的字节」的来源。
pub struct CreatedStaging {
    /// 创建暂存文件时的父目录句柄；提交与取消清理都以它为根。
    pub dir: DirHandle,
    /// 暂存文件的独占写句柄；是提交时身份绑定的源对象，必须存活到提交完成。
    pub file: File,
    /// 暂存名；仅用于 Linux 兜底路径与错误诊断。
    pub staging: String,
}

/// 共享根。持有打开时的目录句柄：根被替换或改成链接后旧句柄仍指向原目录，
/// 因此服务不会悄悄转向新目标。运行期不重查路径身份（见模块注释），
/// 这一点由设计取舍决定，而不是由未实现的校验保证。
pub struct ShareRoot {
    root: DirHandle,
    /// 启动时记录的根身份（卷序列号+文件索引，或 dev+ino），用于启动校验与诊断。
    identity: RootIdentity,
}

impl ShareRoot {
    /// 打开共享根：拒绝链接/重解析点，并拒绝隐藏或内部暂存目录作为根。
    pub fn open(path: &Path) -> AppResult<Self> {
        if path.as_os_str().is_empty() {
            return Err(AppError::new(
                crate::error::ErrorCode::PathRejected,
                "empty root",
            ));
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_restricted_segment)
        {
            return Err(AppError::new(
                crate::error::ErrorCode::PathRejected,
                "root is hidden or internal",
            ));
        }
        let root = DirHandle::open_ambient(path).map_err(|error| {
            AppError::new(
                crate::error::ErrorCode::PathRejected,
                format!("root open failed: {:?}", error.kind()),
            )
        })?;
        let identity = root.identity()?;
        Ok(Self { root, identity })
    }

    /// 根身份用于诊断；不参与权限判断，避免把「同一路径」当成「同一目录」。
    pub fn identity(&self) -> &RootIdentity {
        &self.identity
    }

    /// 逐段打开目录；任一段是链接/重解析点或越界都失败。
    pub fn open_dir(&self, path: &VirtualPath) -> AppResult<DirHandle> {
        if path.is_restricted() {
            return Err(AppError::rejected());
        }
        let mut current = self.root.try_clone()?;
        for segment in path.segments() {
            current = current.open_dir_child(segment)?;
        }
        Ok(current)
    }

    /// 枚举目录；只返回普通文件与目录，隐藏项和内部暂存一律过滤。
    pub fn read_dir(&self, path: &VirtualPath) -> AppResult<Vec<DirEntryInfo>> {
        let dir = self.open_dir(path)?;
        dir.entries()
    }

    /// 打开下载目标：必须位于共享根内、是普通文件、不是硬链接共享内容。
    pub fn open_file(&self, path: &VirtualPath) -> AppResult<OpenedFile> {
        if path.is_restricted() {
            return Err(AppError::rejected());
        }
        let (parent, name) = self.split_parent(path)?;
        parent.open_file_child(name)
    }

    /// 为上传在目标目录创建暂存文件；返回提交与清理所需的全部资源。
    ///
    /// 返回父目录句柄而不是让调用方稍后按路径重新打开：提交与取消清理都必须绑定
    /// 到创建时的目录对象，否则目录被替换后会操作到另一个目录。返回的写句柄必须
    /// 保持存活直到提交完成：Windows 直接以它作为改名的源对象，Linux 匿名暂存
    /// 靠它定位 `/proc/self/fd`，提前丢弃会失去身份绑定。
    pub fn create_staging(&self, dir: &VirtualPath) -> AppResult<CreatedStaging> {
        let handle = self.open_dir(dir)?;
        let (staging, file) = handle.create_staging_file()?;
        Ok(CreatedStaging {
            dir: handle,
            file,
            staging,
        })
    }

    /// 删除暂存文件；只删除带固定前缀的名字。
    ///
    /// 生产路径**不使用**它：上传取消时必须用创建暂存文件时的父目录句柄删除，
    /// 按虚拟路径重新打开会在目录被替换时删到另一个目录里的同名文件
    /// （见 `http::upload::StagingGuard`）。这里保留给测试验证「前缀之外的名字
    /// 不会被删除」这一约束，不要把它当成取消清理的正确入口。
    pub fn discard_staging(&self, dir: &VirtualPath, staging: &str) {
        if !staging.starts_with(TEMP_PREFIX) {
            return;
        }
        if let Ok(handle) = self.open_dir(dir) {
            let _ = handle.remove_child(staging);
        }
    }

    /// 定位文件的父目录与文件名；根不能作为文件打开。
    fn split_parent<'a>(&self, path: &'a VirtualPath) -> AppResult<(DirHandle, &'a str)> {
        let name = path
            .segments()
            .last()
            .map(String::as_str)
            .ok_or_else(AppError::rejected)?;
        let parent = self.open_dir(&path.parent())?;
        Ok((parent, name))
    }
}

/// 根身份；不同平台字段不同，但都只用于诊断与「根未被替换」的粗粒度判断。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootIdentity {
    pub first: u64,
    pub second: u64,
}

impl RootIdentity {
    /// 统一构造，避免平台分支各自拼装。
    pub fn new(first: u64, second: u64) -> Self {
        Self { first, second }
    }
}

/// 平台目录句柄包装；所有子操作都以句柄为根。
pub struct DirHandle {
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
    #[cfg(unix)]
    file: File,
}

impl DirHandle {
    /// 从可信绝对路径打开共享根；只允许启动时使用，运行期必须逐段从根句柄打开。
    pub fn open_ambient(path: &Path) -> io::Result<Self> {
        platform::open_ambient_dir(path)
    }

    /// 复制句柄；不复制路径，因此克隆不会放宽边界。
    pub fn try_clone(&self) -> AppResult<Self> {
        platform::clone_dir(self).map_err(AppError::io)
    }

    /// 打开子目录，要求它是真实目录而非链接/重解析点。
    pub fn open_dir_child(&self, name: &str) -> AppResult<Self> {
        platform::open_dir_child(self, name).map_err(AppError::io)
    }

    /// 枚举目录项；隐藏项、内部暂存和非普通文件由平台层过滤。
    pub fn entries(&self) -> AppResult<Vec<DirEntryInfo>> {
        platform::read_entries(self).map_err(AppError::io)
    }

    /// 打开普通文件用于下载。
    pub fn open_file_child(&self, name: &str) -> AppResult<OpenedFile> {
        platform::open_file_child(self, name).map_err(AppError::io)
    }

    /// 在目录内创建唯一暂存文件。
    pub fn create_staging_file(&self) -> AppResult<(String, File)> {
        platform::create_staging(self).map_err(AppError::io)
    }

    /// 不覆盖提交暂存文件，必要时自动改名。
    ///
    /// 接收者是**创建暂存文件时的父目录句柄**，而不是共享根：提交与取消清理都必须
    /// 绑定到同一个目录对象，否则目录在请求期间被替换后会提交到另一个目录。写句柄
    /// 按值传入并由平台层直接作为改名源，是「提交的就是本次写入的字节」的唯一依据；
    /// 按暂存名重新定位在并发本地写入者面前无法证明这一点。
    ///
    /// 这里不接收 `&self` 形式的独立目录参数，是为了让调用方无法把「目录句柄」与
    /// 「共享根」混用：方法接收者就是唯一权威的目录，少一个可能传错的参数。
    pub fn commit_staging(&self, file: File, staging: &str, desired: &str) -> AppResult<String> {
        platform::commit_staging(self, file, staging, desired).map_err(AppError::io)
    }

    /// 删除目录内子项；仅用于本插件创建的暂存文件。
    pub fn remove_child(&self, name: &str) -> AppResult<()> {
        platform::remove_child(self, name).map_err(AppError::io)
    }

    /// 仅当目录项仍指向 `owned` 所代表的文件对象时删除它；返回是否真的删除。
    ///
    /// 为什么不能只按名删除：提交用句柄绑定保证了「提交的是本次写入的字节」，
    /// 但取消清理若仍按名 `unlink`/`Disposition`，同目录并发写入者把暂存名换成
    /// 别的文件后，我们会删掉别人的文件。这里先按名打开、比较文件身份，只有仍是
    /// 我们创建的对象才删除；名字已被替换时保持原样并返回 `false`。
    ///
    /// Windows 的删除作用在「刚按名打开并已验证身份」的句柄上，因此身份校验之后
    /// 即使名字再被替换，被删除的仍是我们验证过的那个对象。Linux/macOS 没有
    /// 「按 fd 删除」的接口，校验与 `unlinkat` 之间仍存在被替换的窗口——这是
    /// 尽力而为的清理，不是安全保证，已在 README/SECURITY 中如实标注。
    pub fn remove_staging_if_owned(&self, name: &str, owned: &File) -> AppResult<bool> {
        platform::remove_staging_if_owned(self, name, owned).map_err(AppError::io)
    }

    /// 目录身份；用于「根是否仍是同一目录」的诊断。
    pub fn identity(&self) -> AppResult<RootIdentity> {
        platform::dir_identity(self).map_err(AppError::io)
    }
}

/// 目录条目上限判定：恰好达到上限仍然接受，超过一条即失败。
///
/// 抽成纯函数是为了能用小数字做判别性测试：真实两万条目目录的端到端用例成本过高，
/// 而「恰好上限」与「超过上限」的边界语义正是这里唯一需要固定的规则。
/// 两个平台都必须用它，避免一处写成 `>=`、另一处写成 `>` 导致行为分叉。
pub(super) fn exceeds_entry_limit(count: usize, limit: usize) -> bool {
    count > limit
}

/// 生成不覆盖候选名：原名，随后 `stem (n).ext`。两个平台共用同一策略，
/// 避免同名冲突时 Windows 与 Unix 产出不同名字导致用户看到两套行为。
pub(super) fn rename_candidates(desired: &str) -> Vec<String> {
    let mut names = vec![desired.to_owned()];
    let (stem, extension) = match desired.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (desired, None),
    };
    for index in 1..64 {
        let name = match extension {
            Some(extension) => format!("{stem} ({index}).{extension}"),
            None => format!("{stem} ({index})"),
        };
        if name.len() > crate::config::MAX_NAME_BYTES {
            break;
        }
        names.push(name);
    }
    names
}

/// 暂存名随机后缀。getrandom 失败时退回「时间戳 + 进程内递增序号」，
/// 仍由平台层的独占创建保证唯一性；这里不把失败当成致命错误，因为暂存名只是内部
/// 实现细节，不会暴露给浏览器，也不会参与任何权限判断。
pub(super) fn random_token(attempt: u32) -> String {
    // 同一时间戳内的重试必须给出不同候选名，否则会在独占创建上无谓地连续失败。
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        bytes.copy_from_slice(&((nanos as u64) ^ sequence.rotate_left(17)).to_le_bytes());
    }
    format!("{:016x}{attempt:02x}", u64::from_le_bytes(bytes))
}

// 平台实现只覆盖 Windows、Linux 与 macOS；其他 Unix 的 `Unsupported` 分支没有
// 可用的文件操作，这些用例在那里只会得到「平台不支持」，因此不纳入测试目标。
#[cfg(all(test, any(windows, target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::{io::Write as _, path::PathBuf};

    /// 准备共享根；同时返回真实目录路径，因为 `ShareRoot` 刻意不暴露路径
    /// （暴露路径会鼓励按路径访问，破坏句柄模型）。
    fn share() -> (tempfile::TempDir, PathBuf, ShareRoot) {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("share");
        std::fs::create_dir(&directory).unwrap();
        let handle = ShareRoot::open(&directory).unwrap();
        (root, directory, handle)
    }

    /// 根名是隐藏项或内部暂存前缀时直接拒绝：否则整个共享根都会被列表规则过滤掉，
    /// 用户只会看到「目录不存在」而不知道原因。
    #[test]
    fn rejects_restricted_root_names() {
        let root = tempfile::tempdir().unwrap();
        for name in [".hidden", ".klt-tmp-root"] {
            let path = root.path().join(name);
            std::fs::create_dir(&path).unwrap();
            assert_eq!(
                ShareRoot::open(&path).err().map(|error| error.code),
                Some(crate::error::ErrorCode::PathRejected),
                "{name}"
            );
        }
        assert!(ShareRoot::open(&root.path().join("missing")).is_err());
    }

    /// 受限名字在解析阶段就无法表示成 `VirtualPath`，因此根本到不了文件系统；
    /// 共享根里的 `.hidden` 与暂存文件因此既列不出来也打不开。
    #[test]
    fn restricted_names_cannot_be_represented_or_opened() {
        let (_root, directory, share) = share();
        std::fs::write(directory.join(".hidden"), b"secret").unwrap();
        std::fs::write(directory.join(".klt-tmp-abc"), b"secret").unwrap();
        for name in [".hidden", ".klt-tmp-abc"] {
            assert!(
                VirtualPath::parse(&format!("root/{name}")).is_err(),
                "{name} must not parse"
            );
            assert!(
                VirtualPath::from_segments(vec![name.to_owned()]).is_err(),
                "{name} must not be constructible"
            );
        }
        // 列表里也不出现，确认不是「能解析但打开失败」这种半开状态。
        let names = share
            .read_dir(&VirtualPath::root())
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        assert!(
            names.is_empty(),
            "hidden entries must be invisible: {names:?}"
        );
    }

    /// 目录不能被当作文件下载：否则页面会得到一个「0 字节文件」而不是明确的拒绝。
    #[test]
    fn directory_is_not_downloadable() {
        let (_root, directory, share) = share();
        std::fs::create_dir(directory.join("folder")).unwrap();
        let path = VirtualPath::parse("root/folder").unwrap();
        assert!(share.open_file(&path).is_err());
    }

    /// 硬链接会让共享目录里的文件同时出现在根之外，必须按「非普通共享内容」拒绝。
    /// 硬链接在 Windows 与 Unix 上都不需要管理员权限，因此这条断言跨平台有效。
    #[test]
    fn hard_linked_file_is_rejected() {
        let (root, directory, share) = share();
        let outside = root.path().join("outside.txt");
        std::fs::write(&outside, b"secret").unwrap();
        std::fs::hard_link(&outside, directory.join("linked.txt")).unwrap();
        let path = VirtualPath::parse("root/linked.txt").unwrap();
        assert!(share.open_file(&path).is_err());
    }

    /// 符号链接（Unix）不能被跟随；Windows 的 junction 需要额外权限，
    /// 改由硬链接用例覆盖「跨边界共享内容」这一类。
    #[cfg(unix)]
    #[test]
    fn symlink_is_not_followed() {
        let (root, directory, share) = share();
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, directory.join("escape")).unwrap();
        assert!(
            share
                .open_dir(&VirtualPath::parse("root/escape").unwrap())
                .is_err()
        );
        std::os::unix::fs::symlink(outside.join("secret.txt"), directory.join("file-link"))
            .unwrap();
        assert!(
            share
                .open_file(&VirtualPath::parse("root/file-link").unwrap())
                .is_err()
        );
    }

    /// 列表必须过滤隐藏项与暂存文件，且只返回普通文件与目录；
    /// 这是「共享目录里的内部状态不可见」的直接证据。
    #[test]
    fn listing_hides_internal_entries() {
        let (_root, directory, share) = share();
        std::fs::write(directory.join("visible.txt"), b"x").unwrap();
        std::fs::write(directory.join(".hidden"), b"x").unwrap();
        std::fs::write(directory.join(".klt-tmp-1234"), b"x").unwrap();
        std::fs::create_dir(directory.join("folder")).unwrap();
        let mut names = share
            .read_dir(&VirtualPath::root())
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec!["folder".to_owned(), "visible.txt".to_owned()]);
    }

    /// 暂存提交不覆盖，且只删除带固定前缀的暂存名：调用方传入其他名字时
    /// 不能变成「删除共享目录里任意文件」的能力。
    #[test]
    fn staging_commit_and_discard_are_scoped() {
        let (_root, directory, share) = share();
        let CreatedStaging {
            dir,
            mut file,
            staging,
        } = share.create_staging(&VirtualPath::root()).unwrap();
        file.write_all(b"payload").unwrap();
        file.sync_all().unwrap();
        assert_eq!(
            dir.commit_staging(file, &staging, "note.txt").unwrap(),
            "note.txt"
        );

        std::fs::write(directory.join("keep.txt"), b"keep").unwrap();
        share.discard_staging(&VirtualPath::root(), "keep.txt");
        assert!(directory.join("keep.txt").exists());

        // Linux 上暂存可能是匿名文件（没有目录项）；此时没有可删除的暂存名，
        // 按名清理本来就不适用，跳过而不是拿空名字去 join 目录本身。
        let CreatedStaging { file, staging, .. } =
            share.create_staging(&VirtualPath::root()).unwrap();
        if staging.is_empty() {
            return;
        }
        // Windows 的 delete-on-close 只有在最后一个句柄关闭后才移除目录项；
        // 暂存写句柄仍打开时按名删除只会留下 pending 状态。测试必须显式释放它，
        // 才能观察 discard_staging 是否真的删除了目录项，而不是把 pending 当成失败。
        drop(file);
        share.discard_staging(&VirtualPath::root(), &staging);
        assert!(!directory.join(&staging).exists());
    }

    /// 候选名必须在扩展名前递增，并遵守名字长度上限；两个平台共用这一策略，
    /// 否则同名冲突时 Windows 与 Unix 会产出不同的文件名。
    #[test]
    fn rename_candidates_follow_one_strategy() {
        assert_eq!(
            rename_candidates("note.txt")[..3],
            [
                "note.txt".to_owned(),
                "note (1).txt".to_owned(),
                "note (2).txt".to_owned()
            ]
        );
        assert_eq!(rename_candidates("README")[1], "README (1)");
        let long = format!("{}.txt", "a".repeat(crate::config::MAX_NAME_BYTES - 4));
        assert!(
            rename_candidates(&long)
                .iter()
                .all(|name| name.len() <= crate::config::MAX_NAME_BYTES)
        );
    }

    /// 条目上限的边界语义：恰好上限接受，超过一条失败。用 `limit = 2` 做判别，
    /// 不需要真的创建两万个文件，也不会因为平台差异（符号链接过滤）影响结论。
    #[test]
    fn entry_limit_is_inclusive() {
        assert!(!exceeds_entry_limit(0, 2));
        assert!(!exceeds_entry_limit(2, 2));
        assert!(exceeds_entry_limit(3, 2));
    }

    /// 重复列举同一目录必须得到相同结果：Windows 上每次调用都用
    /// `FileIdBothDirectoryRestartInfo` 重置枚举位置，否则第二次读取会从上次位置继续，
    /// 结果取决于调用顺序（并发读取也会互相推进光标）。
    #[test]
    fn repeated_listing_is_stable() {
        let (_root, directory, share) = share();
        for index in 0..8 {
            std::fs::write(directory.join(format!("f{index}.txt")), b"x").unwrap();
        }
        let first = share.read_dir(&VirtualPath::root()).unwrap();
        let second = share.read_dir(&VirtualPath::root()).unwrap();
        let names = |entries: &[DirEntryInfo]| {
            let mut names = entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
            names.sort();
            names
        };
        assert_eq!(names(&first), names(&second));
        assert_eq!(
            first.len(),
            8,
            "listing must not be truncated by a batch boundary"
        );
    }

    /// 同一目录重复打开身份必须一致，不同目录必须不同；否则「根未被替换」的诊断
    /// 会在每次启动时误报，或者反过来永远检测不到替换。
    #[test]
    fn root_identity_distinguishes_directories() {
        let (root, _directory, share) = share();
        let other = root.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let second = ShareRoot::open(&other).unwrap();
        assert_ne!(share.identity(), second.identity());
        let again = ShareRoot::open(&root.path().join("share")).unwrap();
        assert_eq!(share.identity(), again.identity());
    }
}
