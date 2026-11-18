//! Windows 平台实现：NtCreateFile + OBJ_DONT_REPARSE，枚举与提交都不回退到路径。
//! @author kongweiguang
//!
//! 这里刻意不用 `cap-std`：其 `Dir::entries`/`rename` 在 3.4.x 会先把句柄还原成
//! 路径再重新解析，一旦目录中含 junction 或并发替换，安全性就无法证明。因此全部
//! 操作都直接以父句柄为 `RootDirectory` 调用 NT API，且统一带 `OBJ_DONT_REPARSE`。

use std::{
    ffi::OsStr,
    fs::File,
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, HandleOrInvalid, OwnedHandle},
    },
    path::Path,
    ptr,
};

use windows_sys::Wdk::{
    Foundation::OBJECT_ATTRIBUTES,
    Storage::FileSystem::{
        FILE_CREATE, FILE_DIRECTORY_FILE, FILE_DISPOSITION_INFORMATION, FILE_NON_DIRECTORY_FILE,
        FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
        FileDispositionInformation, FileRenameInformation, NtCreateFile, NtSetInformationFile,
    },
};
use windows_sys::Win32::{
    Foundation::{HANDLE, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, RtlNtStatusToDosError},
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_ID_BOTH_DIR_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY,
        FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_TRAVERSE, FILE_WRITE_DATA, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx, SYNCHRONIZE,
    },
    System::IO::IO_STATUS_BLOCK,
};

use super::{DirEntryInfo, DirHandle, EntryKind, OpenedFile, RootIdentity};
use crate::config::TEMP_PREFIX;

/// 目录访问只申请列目录与读取属性；创建/改名/删除都在目标子项句柄上单独申请权限，
/// 避免共享根句柄长期持有删除权限。
///
/// `SYNCHRONIZE` 不是多余权限：`FILE_SYNCHRONOUS_IO_NONALERT` 要求句柄具备同步访问，
/// 否则 `NtCreateFile` 直接返回 `STATUS_INVALID_PARAMETER`。
/// 创建子项需要对父目录持有 `FILE_ADD_FILE`，相对句柄打开也需要 `FILE_TRAVERSE`；
/// 缺少这些权限时 `NtCreateFile` 以 `STATUS_ACCESS_DENIED` 失败，而不是在目标上失败。
const DIR_ACCESS: u32 = FILE_LIST_DIRECTORY
    | FILE_READ_ATTRIBUTES
    | FILE_TRAVERSE
    | FILE_ADD_FILE
    | FILE_ADD_SUBDIRECTORY
    | SYNCHRONIZE;

/// 打开共享根；根必须是真实目录，不能是链接或重解析点。
pub fn open_ambient_dir(path: &Path) -> io::Result<DirHandle> {
    let handle = open_absolute_dir(path)?;
    Ok(DirHandle { handle })
}

/// 直接对绝对路径调用 NtCreateFile，只用于启动时打开根。
///
/// 关键点：**不做 `canonicalize`**。`std::fs::canonicalize` 会跟随根自身的 junction
/// 或符号链接，先解析到目标再打开，等于把「根是链接」这个事实抹掉，
/// 与 `ShareRoot::open` 拒绝根重解析点的承诺冲突。这里改用 `std::path::absolute`
/// （只拼接当前目录，不解析链接），再交给内核带 `OBJ_DONT_REPARSE` 解析：
/// 路径中任何一段是重解析点都会让内核直接失败，不存在「检查后被替换」的窗口。
///
/// `NtCreateFile` 的绝对路径必须位于对象管理器命名空间（`\??\C:\...`），
/// 直接传 `C:\...` 会被当作相对根目录的名字而失败。
fn open_absolute_dir(path: &Path) -> io::Result<OwnedHandle> {
    let absolute = std::path::absolute(path)?;
    let wide = nt_path(&absolute)?.encode_wide().collect::<Vec<u16>>();
    let mut name = unicode_string(&wide);
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: ptr::null_mut(),
        ObjectName: &mut name,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let handle = nt_open(
        0,
        &attributes,
        DIR_ACCESS,
        FILE_OPEN,
        FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
    )?;
    // 内核在 OBJ_DONT_REPARSE 下已会拒绝解析重解析点；这里再按属性复核一次，
    // 避免将来有人删掉该标志时「根是链接」被静默接受。
    let file = file_from(handle);
    let info = handle_info(&file)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "root is a reparse point",
        ));
    }
    Ok(file.into())
}

/// 以父目录句柄为根打开子目录；OBJ_DONT_REPARSE 让内核拒绝任何重解析点。
pub fn open_dir_child(parent: &DirHandle, name: &str) -> io::Result<DirHandle> {
    let wide = OsStr::new(name).encode_wide().collect::<Vec<u16>>();
    let mut unicode = unicode_string(&wide);
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.handle.as_raw_handle() as HANDLE,
        ObjectName: &mut unicode,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let handle = nt_open(
        0,
        &attributes,
        DIR_ACCESS,
        FILE_OPEN,
        FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
    )?;
    Ok(DirHandle { handle })
}

/// 复制目录句柄：用 `NtCreateFile` + 空对象名从原句柄重新打开同一目录对象。
///
/// 不能改用 `DuplicateHandle`：它返回同一个文件对象的另一个句柄，而
/// `GetFileInformationByHandleEx` 的枚举位置保存在文件对象内部，两个句柄会共享
/// 同一个光标，并发列表会互相推进或重置枚举位置。`ReOpenFile` 本应提供独立文件
/// 对象，但在本机 Windows 11 26300 上对目录句柄稳定返回 `ACCESS_DENIED`（同样的
/// 访问权限对文件句柄正常），会让列表、上传暂存与并发枚举全部失败。`NtCreateFile`
/// 把 `RootDirectory` 指向原句柄、对象名留空，内核直接重开该目录对象：不解析任何
/// 路径、不跟随重解析点，并得到独立文件对象与独立枚举光标。
pub fn clone_dir(dir: &DirHandle) -> io::Result<DirHandle> {
    let mut name = windows_sys::Win32::Foundation::UNICODE_STRING {
        Length: 0,
        MaximumLength: 0,
        Buffer: ptr::null_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: dir.handle.as_raw_handle() as HANDLE,
        ObjectName: &mut name,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let handle = nt_open(
        0,
        &attributes,
        DIR_ACCESS,
        FILE_OPEN,
        FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
    )?;
    Ok(DirHandle { handle })
}

/// 枚举目录：使用句柄相对 `FileIdBothDirectoryInfo`，绝不把句柄还原成路径。
///
/// 两个必须分开处理的状态：
/// - 首次调用带 `FileIdBothDirectoryRestartInfo`，把枚举位置重置到目录开头；
///   否则同一个句柄的第二次读取会从上次位置继续，结果取决于此前的调用顺序。
/// - `NextEntryOffset == 0` 只表示「本批最后一个条目」，不表示目录结束；只有内核以
///   `ERROR_NO_MORE_FILES` 结束才算读完。此前把「本批末尾」当成「目录末尾」，
///   超过单次 64 KiB 缓冲的目录会被静默截断。
///
/// 类常量必须取自 `Win32::Storage::FileSystem`（`FILE_INFO_BY_HANDLE_CLASS`，
/// `RestartInfo = 11`、`Info = 10`）。此前误用 `Wdk` 的
/// `FileIdBothDirectoryInformation`（`FILE_INFORMATION_CLASS = 37`）：两者都是 `i32`
/// 别名，传错值不会触发类型错误，只会在运行时被 `GetFileInformationByHandleEx`
/// 当成未知类拒绝或返回别的结构，因此这类错误只能靠真实目录枚举测试发现。
///
/// 每次调用都由 `ShareRoot::open_dir` 经 `clone_dir` 重开独立文件对象：`NtCreateFile`
/// 以原句柄为 `RootDirectory`、对象名留空，内核直接重开同一目录对象。这样既不会把
/// 句柄还原成路径再解析（目录被 junction 或并发替换时可能打开到另一个对象），也不会
/// 像 `DuplicateHandle` 那样共享同一个文件对象和枚举光标，并发读取因此各自持有独立
/// 枚举位置（原因详见 `clone_dir`）。
///
/// 边界说明：`GetFileInformationByHandleEx` 成功时不返回实际填充字节数，因此只能
/// 按整块缓冲校验记录与名字范围。内核只写完整记录，最后一条记录的 `NextEntryOffset`
/// 为 0，名字紧随其后；真正能证明「读到批末」的仍是 `ERROR_NO_MORE_FILES`。
/// 若内核返回的记录本身被破坏（偏移回指、奇数名字长度、越界），这里返回
/// `InvalidData`，不继续解析也不把它当成目录结束。
///
/// 缓冲会在多批之间复用，因此批末之后可能残留上一批的字节；解析严格沿
/// `NextEntryOffset` 链前进并在链尾（偏移为 0）停止，不会读到这些残留。
/// `ERROR_MORE_DATA` 只在单条记录都放不下时出现（64 KiB 远大于 255 个 UTF-16
/// 单元的名字上限），这里按错误上报而不是猜测已写入范围。
pub fn read_entries(dir: &DirHandle) -> io::Result<Vec<DirEntryInfo>> {
    // 用 `u64` 分配缓冲：`FILE_ID_BOTH_DIR_INFO` 含 `i64` 字段，要求 8 字节对齐。
    // 用 `Vec<u8>` 再强转成结构体引用在技术上未定义行为（分配只保证 1 字节对齐），
    // 在 x86 上「碰巧可用」会掩盖问题，换平台或优化级别就可能出错。
    let mut buffer = vec![0u64; DIRECTORY_BUFFER_BYTES / 8];
    let mut entries = Vec::new();
    // 显式标注为 `FILE_INFO_BY_HANDLE_CLASS`，避免将来再次把 Wdk 的
    // `FILE_INFORMATION_CLASS` 常量混进来（两者同为 `i32`，编译器无法区分）。
    let mut class: FILE_INFO_BY_HANDLE_CLASS = FileIdBothDirectoryRestartInfo;
    loop {
        let ok = unsafe {
            GetFileInformationByHandleEx(
                dir.handle.as_raw_handle() as HANDLE,
                class,
                buffer.as_mut_ptr() as *mut _,
                DIRECTORY_BUFFER_BYTES as u32,
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            // ERROR_NO_MORE_FILES（18）才是「目录已读完」的权威信号。
            if error.raw_os_error() == Some(18) {
                break;
            }
            return Err(error);
        }
        // 后续批次沿用普通枚举类，从内核记录的位置继续。
        class = FileIdBothDirectoryInfo;
        parse_batch(&buffer, DIRECTORY_BUFFER_BYTES, &mut entries)?;
        if super::exceeds_entry_limit(entries.len(), crate::config::MAX_DIR_ENTRIES) {
            // 超过上限必须报错而不是截断：截断后的列表会被页面当成完整目录，
            // 用户找不到文件却没有任何提示。错误映射到 `too_large`，界面可提示缩小范围。
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "directory has too many entries",
            ));
        }
    }
    Ok(entries)
}

/// 单次枚举缓冲区大小；必须是 8 的倍数，保证按 `u64` 分配后仍能容纳完整结构体。
const DIRECTORY_BUFFER_BYTES: usize = 64 * 1024;

/// 解析一批 `FILE_ID_BOTH_DIR_INFO` 记录。
///
/// 关键点：`FILE_ID_BOTH_DIR_INFO.FileName` 在 windows-sys 里声明为 `[u16; 1]`，
/// 它只是变长名字的**起始占位**。如果直接借这个字段（`&info.FileName`），切片长度
/// 永远是 1，所有多字符文件名都会被静默截成第一个 UTF-16 单元（中文名甚至变成
/// 半个代理对）。名字必须按 `FileNameLength` 从记录之后读取。
///
/// 所有偏移都相对本批有效字节检查：记录头、名字与下一条偏移越界都按数据损坏返回
/// 可见错误，绝不越界读取，也不把损坏当成「目录已读完」。读取一律按字节复制，
/// 不假设内核返回的记录起点对齐。
fn parse_batch(
    buffer: &[u64],
    buffer_bytes: usize,
    entries: &mut Vec<DirEntryInfo>,
) -> io::Result<()> {
    // 任何调用方给出的窗口都不能超过真实分配，否则下面的边界检查本身就会越界。
    let buffer_bytes = buffer_bytes.min(buffer.len() * std::mem::size_of::<u64>());
    let base = buffer.as_ptr() as *const u8;
    let header = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
    let mut offset = 0usize;
    // 名字先复制到独立缓冲再解码：即使内核返回的记录起点不是 8 字节对齐，
    // 也不会对未对齐地址构造 `u16` 切片（那是未定义行为）。
    // 记录头只按 `header` 字节复制到零初始化结构体：直接 `read_unaligned` 整个
    // 结构体会连带读入 `FileName` 占位与尾部填充，可能在批末越过有效字节。
    let mut name = Vec::new();
    let mut info = FILE_ID_BOTH_DIR_INFO::default();
    loop {
        // 记录头必须完整落在本批有效字节内。
        if offset + header > buffer_bytes {
            return Err(corrupt("directory entry header out of range"));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                base.add(offset),
                &mut info as *mut FILE_ID_BOTH_DIR_INFO as *mut u8,
                header,
            );
        }
        let name_bytes = info.FileNameLength as usize;
        if name_bytes % 2 != 0 {
            return Err(corrupt("directory entry name length is odd"));
        }
        let name_end = offset + header + name_bytes;
        if name_end > buffer_bytes {
            return Err(corrupt("directory entry name out of range"));
        }
        name.clear();
        name.extend((0..name_bytes / 2).map(|index| unsafe {
            std::ptr::read_unaligned(base.add(offset + header + index * 2) as *const u16)
        }));
        match String::from_utf16(&name) {
            Ok(name) => {
                if let Some(entry) = classify(
                    &name,
                    info.FileAttributes,
                    info.EndOfFile,
                    info.LastWriteTime,
                ) {
                    entries.push(entry);
                }
            }
            // 非法 UTF-16 无法无损传给浏览器，按不可见处理；这不是记录损坏，
            // 与越界偏移区分开，避免把个别怪名字升级成整份列表失败。
            Err(_) => {}
        }
        if info.NextEntryOffset == 0 {
            return Ok(());
        }
        let next = info.NextEntryOffset as usize;
        // 下一条必须前进到本记录之后且仍在批内；否则继续解析只会读到上一条
        // 名字里的字节或批外内存，必须当成损坏立即失败。
        if next < header + name_bytes || offset + next > buffer_bytes {
            return Err(corrupt("next entry offset out of range"));
        }
        offset += next;
    }
}

/// 统一的「内核返回结构不可信」错误；用 `InvalidData` 让上层映射为可见失败。
fn corrupt(detail: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail)
}

/// 过滤隐藏项、暂存前缀、`.`/`..` 与非普通文件；隐藏属性同样不共享。
fn classify(name: &str, attributes: u32, size: i64, last_write: i64) -> Option<DirEntryInfo> {
    if name.is_empty() || name == "." || name == ".." || crate::path::is_restricted_segment(name) {
        return None;
    }
    if attributes & FILE_ATTRIBUTE_HIDDEN != 0 || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return None;
    }
    let modified = filetime_to_unix(last_write);
    if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Some(DirEntryInfo {
            name: name.to_owned(),
            kind: EntryKind::Directory,
            size: None,
            modified,
        });
    }
    Some(DirEntryInfo {
        name: name.to_owned(),
        kind: EntryKind::File,
        size: u64::try_from(size).ok(),
        modified,
    })
}

/// 打开普通文件：拒绝目录、链接/重解析点，并用硬链接数拒绝共享内容。
///
/// 硬链接会让「共享目录里的文件」同时出现在共享根之外，因此把 `NumberOfLinks > 1`
/// 视为不可共享；这比只检查路径更可靠，因为链接的创建发生在路径检查之后。
pub fn open_file_child(parent: &DirHandle, name: &str) -> io::Result<OpenedFile> {
    let handle = open_child(
        parent,
        name,
        FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        FILE_NON_DIRECTORY_FILE,
    )?;
    let file = file_from(handle);
    let info = handle_info(&file)?;
    if info.nNumberOfLinks != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "hard linked file",
        ));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "reparse point",
        ));
    }
    Ok(OpenedFile {
        size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        modified: filetime_to_unix(
            ((info.ftLastWriteTime.dwHighDateTime as i64) << 32)
                | info.ftLastWriteTime.dwLowDateTime as i64,
        ),
        file,
    })
}

/// 创建唯一暂存文件；带 FILE_CREATE 的 NtCreateFile 在名字冲突时失败，不会覆盖。
pub fn create_staging(dir: &DirHandle) -> io::Result<(String, File)> {
    for attempt in 0..32u32 {
        let name = format!("{TEMP_PREFIX}{}", super::random_token(attempt));
        let wide = OsStr::new(&name).encode_wide().collect::<Vec<u16>>();
        let mut unicode = unicode_string(&wide);
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: dir.handle.as_raw_handle() as HANDLE,
            ObjectName: &mut unicode,
            Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
            SecurityDescriptor: ptr::null(),
            SecurityQualityOfService: ptr::null(),
        };
        match nt_open(
            0,
            &attributes,
            FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | DELETE | SYNCHRONIZE,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
        ) {
            Ok(handle) => return Ok((name, file_from(handle))),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "staging name exhausted",
    ))
}

/// 不覆盖提交：源对象是创建暂存文件时返回的**写句柄**，而不是按名字重新打开。
///
/// 为什么必须用句柄绑定源：按名字定位暂存文件时，同目录的并发本地写入者若在
/// 「定位」与「改名」之间把同名的暂存文件换掉，提交的就会是别人的字节——
/// 暂存名不可预测只提高攻击成本，不构成身份证明。改名请求直接作用在写句柄
/// 指向的对象上，目标名按父目录句柄相对解析，源一侧完全不经路径查找，
/// 任何同目录替换都影响不到本次提交。
///
/// 这里不调用 `FlushFileBuffers` 或声称目录持久化：Windows 没有目录 fsync，
/// 崩溃后的目录项一致性由文件系统决定，实现不对其作承诺。
pub fn commit_staging(
    dir: &DirHandle,
    staging_file: File,
    _staging: &str,
    desired: &str,
) -> io::Result<String> {
    for candidate in super::rename_candidates(desired) {
        match rename_handle(dir, staging_file.as_raw_handle(), &candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    // 全冲突时写句柄仍归调用方，由上层清理策略决定保留还是删除，这里不吞掉句柄。
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "rename candidates exhausted",
    ))
}

/// 对句柄本身执行改名：源是传入的暂存写句柄，目标按父目录句柄相对解析。
///
/// 身份绑定的关键在源一侧：`NtSetInformationFile` 改名的就是 `FileHandle` 参数
/// 指向的那个对象，不再按名字重新打开，并发写入者替换同名暂存文件影响不到它。
/// 目标一侧仍要给出名字；`RootDirectory` 传父目录句柄时内核按句柄相对解析，
/// 不经过共享根之外的路径，目标冲突以 `AlreadyExists` 失败。
///
/// 传给内核的长度按字段偏移构造，并以 `size_of::<FILE_RENAME_INFORMATION>()` 为下限：
/// 结构体里的 `FileName: [u16; 1]` 只是变长名字的占位，`size_of` 还包含尾部填充。
/// 单字符目标名按「偏移 + 2」只有 22 字节，实测内核以 `ERROR_BAD_LENGTH`（24）拒绝；
/// 因此长度至少取结构体大小，更长名字仍按「偏移 + `FileNameLength`」传，不额外补齐。
/// `single_unit_name_commit_is_accepted_and_never_overwrites` 固定这一边界。
///
/// `ReplaceIfExists` 与 `Flags` 是同一个 4 字节 union 的两种视图：`FileRenameInformation`
/// 类只读其中的 BOOLEAN。这里用 `Flags: 0` 一次写满整个 union，与「不覆盖」同义，
/// 同时不把 union 剩余字节留给未初始化值或缓冲区清零来兜底。没有观察到覆盖行为，
/// 这里只是消除未定义初始化，不声称存在已复现的覆盖漏洞。
fn rename_handle(
    dir: &DirHandle,
    source: std::os::windows::io::RawHandle,
    to: &str,
) -> io::Result<()> {
    let wide = OsStr::new(to).encode_wide().collect::<Vec<u16>>();
    let name_offset = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    // 单字符名按偏移只有 22 字节，内核要求至少结构体大小（24）；取下限后更长名字
    // 仍按实际名字长度传，不额外补齐。
    let total = (name_offset + wide.len() * 2).max(std::mem::size_of::<FILE_RENAME_INFORMATION>());
    let mut buffer = vec![0u64; total.div_ceil(8)];
    let base = buffer.as_mut_ptr() as *mut u8;
    unsafe {
        ptr::write(
            base as *mut FILE_RENAME_INFORMATION,
            FILE_RENAME_INFORMATION {
                Anonymous: windows_sys::Wdk::Storage::FileSystem::FILE_RENAME_INFORMATION_0 {
                    Flags: 0,
                },
                RootDirectory: dir.handle.as_raw_handle() as HANDLE,
                FileNameLength: (wide.len() * 2) as u32,
                FileName: [0],
            },
        );
        ptr::copy_nonoverlapping(wide.as_ptr(), base.add(name_offset) as *mut u16, wide.len());
    }
    let mut status = IO_STATUS_BLOCK::default();
    let nt = unsafe {
        NtSetInformationFile(
            source as HANDLE,
            &mut status,
            base as *const _,
            total as u32,
            FileRenameInformation,
        )
    };
    if nt < 0 {
        return Err(nt_error(nt));
    }
    Ok(())
}

/// 删除暂存文件；用 FILE_DISPOSITION_INFORMATION 走句柄，避免按路径再解析。
pub fn remove_child(dir: &DirHandle, name: &str) -> io::Result<()> {
    let handle = open_child(dir, name, DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE, 0)?;
    let info = FILE_DISPOSITION_INFORMATION { DeleteFile: true };
    let mut status = IO_STATUS_BLOCK::default();
    let nt = unsafe {
        NtSetInformationFile(
            handle.as_raw_handle() as HANDLE,
            &mut status,
            &info as *const _ as *const _,
            std::mem::size_of::<FILE_DISPOSITION_INFORMATION>() as u32,
            FileDispositionInformation,
        )
    };
    if nt < 0 {
        return Err(nt_error(nt));
    }
    Ok(())
}

/// 仅当目录项仍指向 `owned` 的文件对象时才删除它。
///
/// Windows 上可以做到「先验证、后删除」都落在同一个句柄上：先按名打开一个带
/// `DELETE` 权限的句柄并比较文件索引与卷序列号，确认是本插件创建的对象后再对
/// **这个句柄**（而不是重新按名定位）设置删除标记。即使目录项随后被并发替换，
/// 被删除的也只是当初验证过的那个对象，不会误删替换者的文件。
pub fn remove_staging_if_owned(dir: &DirHandle, name: &str, owned: &File) -> io::Result<bool> {
    let ours = handle_info(owned)?;
    // 目录项已不存在按「无可清理对象」处理而不是报错：与 Unix 实现语义一致，
    // 也让守卫的 Drop 路径不会把「已被清理」当成异常残留。
    let handle = match open_child(dir, name, DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE, 0) {
        Ok(handle) => handle,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let file = file_from(handle);
    let candidate = handle_info(&file)?;
    if candidate.dwVolumeSerialNumber != ours.dwVolumeSerialNumber
        || candidate.nFileIndexHigh != ours.nFileIndexHigh
        || candidate.nFileIndexLow != ours.nFileIndexLow
    {
        // 名字已被换成本地写入者的文件：保持原样，由调用方决定是否上报。
        return Ok(false);
    }
    let info = FILE_DISPOSITION_INFORMATION { DeleteFile: true };
    let mut status = IO_STATUS_BLOCK::default();
    let nt = unsafe {
        NtSetInformationFile(
            file.as_raw_handle() as HANDLE,
            &mut status,
            &info as *const _ as *const _,
            std::mem::size_of::<FILE_DISPOSITION_INFORMATION>() as u32,
            FileDispositionInformation,
        )
    };
    if nt < 0 {
        return Err(nt_error(nt));
    }
    Ok(true)
}

/// 目录身份：卷序列号 + 文件索引。
pub fn dir_identity(dir: &DirHandle) -> io::Result<RootIdentity> {
    let info = handle_info_file(dir)?;
    Ok(RootIdentity::new(
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

/// 打开目录内子项并统一附加同步/重解析点选项；调用方只决定权限与文件类型。
fn open_child(dir: &DirHandle, name: &str, access: u32, kind: u32) -> io::Result<OwnedHandle> {
    let wide = OsStr::new(name).encode_wide().collect::<Vec<u16>>();
    let mut unicode = unicode_string(&wide);
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: dir.handle.as_raw_handle() as HANDLE,
        ObjectName: &mut unicode,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    nt_open(
        0,
        &attributes,
        access,
        FILE_OPEN,
        kind | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
    )
}

/// 统一 NtCreateFile 调用并把 NTSTATUS 转成 io::Error。
fn nt_open(
    allocation: u64,
    attributes: &OBJECT_ATTRIBUTES,
    access: u32,
    disposition: u32,
    options: u32,
) -> io::Result<OwnedHandle> {
    let mut handle: HANDLE = ptr::null_mut();
    let mut status = IO_STATUS_BLOCK::default();
    // NtCreateFile 要求 `LARGE_INTEGER` 指针，这里显式转换并保持局部变量存活到调用结束。
    let allocation = allocation as i64;
    let nt = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            attributes,
            &mut status,
            &allocation as *const i64,
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            disposition,
            options,
            ptr::null(),
            0,
        )
    };
    if nt < 0 {
        return Err(nt_error(nt));
    }
    // 内核成功时必然写入有效句柄；仍用 HandleOrInvalid 复核，避免把空值当句柄使用。
    let handle = unsafe { HandleOrInvalid::from_raw_handle(handle as _) };
    handle
        .try_into()
        .map_err(|_| io::Error::other("invalid handle"))
}

/// NTSTATUS 直接放进 `io::Error` 会得到 `Uncategorized`，上层无法区分「不存在」
/// 「已存在」「拒绝访问」。统一经 `RtlNtStatusToDosError` 转成 Win32 错误码，
/// 让 `ErrorKind` 与 `AppError::io` 的映射保持一致。
fn nt_error(status: i32) -> io::Error {
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(code as i32)
}

/// 把已绝对化（但未解析链接）的路径转成对象管理器路径。
///
/// 这里刻意不调用 `canonicalize`：它会跟随路径里的 junction/符号链接，
/// 让「根是链接」这一事实在打开之前就被抹掉。
fn nt_path(path: &Path) -> io::Result<std::ffi::OsString> {
    let text = path.as_os_str().to_string_lossy();
    const DOS_PREFIX: &str = "\\\\?\\";
    const NT_PREFIX: &str = "\\??\\";
    // `std::path::absolute` 在 Windows 上可能给出 verbatim 前缀（`\\?\C:\...`）；
    // 对象管理器只认 `\??\`，两者只在前缀上不同。
    let rest = text.strip_prefix(DOS_PREFIX).unwrap_or(&text);
    if rest.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty path"));
    }
    // UNC 路径在对象管理器里是 `\??\UNC\server\share`，直接拼 `\??\\server\...`
    // 会被解释成设备名而失败。
    if let Some(unc) = rest.strip_prefix("\\\\") {
        return Ok(std::ffi::OsString::from(format!("{NT_PREFIX}UNC\\{unc}")));
    }
    Ok(std::ffi::OsString::from(format!("{NT_PREFIX}{rest}")))
}

/// 把 OwnedHandle 交给 File，让读写与关闭都走标准库的生命周期管理。
fn file_from(handle: OwnedHandle) -> File {
    File::from(handle)
}

/// UNICODE_STRING 借用调用栈上的 UTF-16 缓冲，生命周期只覆盖本次调用。
fn unicode_string(wide: &[u16]) -> windows_sys::Win32::Foundation::UNICODE_STRING {
    windows_sys::Win32::Foundation::UNICODE_STRING {
        Length: (wide.len() * 2) as u16,
        MaximumLength: (wide.len() * 2) as u16,
        Buffer: wide.as_ptr() as *mut _,
    }
}

/// 提取文件句柄信息；失败时返回原始 IO 错误。
fn handle_info(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

/// DirHandle 只持有句柄，借用同一条路径复用文件信息查询。
fn handle_info_file(dir: &DirHandle) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let ok = unsafe { GetFileInformationByHandle(dir.handle.as_raw_handle() as HANDLE, &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

/// Windows FILETIME（100ns，自 1601）转 Unix 秒；异常值返回 None。
fn filetime_to_unix(value: i64) -> Option<u64> {
    if value <= 0 {
        return None;
    }
    let seconds = value / 10_000_000 - 11_644_473_600;
    u64::try_from(seconds).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// 真实目录里的长 ASCII、中文与代理对文件名必须完整读出并可再打开。
    ///
    /// 这条用例直接固定住 `FILE_ID_BOTH_DIR_INFO.FileName` 是 `[u16; 1]` 占位这一
    /// 陷阱：任何「按字段切片」的实现都只会得到首字符，而 `open_dir_child`/
    /// `open_file_child` 会因名字对不上而失败。名字长度都超过 1 个 UTF-16 单元，
    /// 其中代理对（emoji）还要求名字没有被按 UTF-16 单元截断。
    #[test]
    fn long_and_unicode_names_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let handle = open_ambient_dir(directory.path()).unwrap();
        let long_ascii = format!("{}.txt", "a".repeat(120));
        let chinese = "中文报告-2026.txt";
        let emoji = "😀-summary.txt";
        for name in [long_ascii.as_str(), chinese, emoji] {
            std::fs::write(directory.path().join(name), b"x").unwrap();
        }
        let mut names = read_entries(&handle)
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        names.sort();
        let mut expected = vec![long_ascii.clone(), chinese.to_owned(), emoji.to_owned()];
        expected.sort();
        assert_eq!(names, expected);

        // 列表里的名字必须能直接用于句柄相对打开，证明读到的不是被截断的前缀。
        for name in [long_ascii.as_str(), chinese, emoji] {
            let opened =
                open_file_child(&handle, name).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(opened.size, 1, "{name}");
        }
    }

    /// 超过单批 64 KiB 缓冲的目录必须完整枚举，且重复读取得到同一集合。
    ///
    /// 每条记录约 100+ 字节，600 个条目足以跨越多批；若把「本批末尾」当成目录末尾，
    /// 这里会稳定地少返回条目。重复读取还验证 RestartInfo 真正重置了枚举位置，
    /// 而不是沿用上一次的光标。
    #[test]
    fn multi_batch_listing_is_complete_and_repeatable() {
        let directory = tempfile::tempdir().unwrap();
        let handle = open_ambient_dir(directory.path()).unwrap();
        let total = 600usize;
        for index in 0..total {
            std::fs::write(
                directory
                    .path()
                    .join(format!("entry-{index:04}-{}.txt", "x".repeat(40))),
                b"x",
            )
            .unwrap();
        }
        let first = read_entries(&handle).unwrap();
        let second = read_entries(&handle).unwrap();
        assert_eq!(first.len(), total, "multi-batch listing must not truncate");
        assert_eq!(second.len(), total);
        let names = |entries: &[DirEntryInfo]| {
            let mut names = entries
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        assert_eq!(names(&first), names(&second));
        // 最后一批之后的条目也必须包含在内，不能只验证前 64 KiB。
        assert!(
            names(&first)
                .iter()
                .any(|name| name.starts_with("entry-0599-"))
        );
    }

    /// 构造一批手工记录，验证解析器按 `FileNameLength` 读取变长名字而不是按字段切片。
    ///
    /// 这里直接调用 `parse_batch`，因此不依赖内核；记录布局按 Win32 文档：
    /// 固定部分 + 紧邻的 UTF-16 名字，`NextEntryOffset` 指向下一条起点。
    #[test]
    fn parser_reads_variable_length_names() {
        let mut buffer = vec![0u64; 64];
        let bytes = buffer.len() * 8;
        let base = buffer.as_mut_ptr() as *mut u8;
        let header = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        let names = ["alpha.txt", "中文名字.txt", "😀.txt"];
        let mut offset = 0usize;
        for (index, name) in names.iter().enumerate() {
            let wide = name.encode_utf16().collect::<Vec<u16>>();
            let record_len = (header + wide.len() * 2).div_ceil(8) * 8;
            let info = unsafe { &mut *(base.add(offset) as *mut FILE_ID_BOTH_DIR_INFO) };
            info.FileAttributes = 0;
            info.FileNameLength = (wide.len() * 2) as u32;
            info.NextEntryOffset = if index + 1 == names.len() {
                0
            } else {
                record_len as u32
            };
            unsafe {
                std::ptr::copy_nonoverlapping(
                    wide.as_ptr(),
                    base.add(offset + header) as *mut u16,
                    wide.len(),
                );
            }
            offset += record_len;
        }
        let mut entries = Vec::new();
        parse_batch(&buffer, bytes, &mut entries).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            names
        );
    }

    /// 损坏的记录必须返回可见错误，不能越界读取或静默当成目录结束。
    #[test]
    fn parser_rejects_corrupt_offsets() {
        // 缓冲必须容纳完整记录头，否则每条用例都会因「头越界」失败，失去判别力。
        let mut buffer = vec![0u64; 64];
        let bytes = buffer.len() * 8;
        let base = buffer.as_mut_ptr() as *mut u8;
        let header = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        assert!(bytes > header + 8, "test buffer must hold a record header");
        let mut entries = Vec::new();

        // 名字长度超过本批有效字节。
        let info = unsafe { &mut *(base as *mut FILE_ID_BOTH_DIR_INFO) };
        info.FileNameLength = 4096;
        info.NextEntryOffset = 0;
        assert!(parse_batch(&buffer, bytes, &mut entries).is_err());

        // 下一条偏移指回记录内部，继续解析会读到名字里的字节。
        let info = unsafe { &mut *(base as *mut FILE_ID_BOTH_DIR_INFO) };
        info.FileNameLength = 2;
        info.NextEntryOffset = 1;
        assert!(parse_batch(&buffer, bytes, &mut entries).is_err());

        // 名字长度为奇数，无法构成合法 UTF-16 序列。
        let info = unsafe { &mut *(base as *mut FILE_ID_BOTH_DIR_INFO) };
        info.FileNameLength = 3;
        info.NextEntryOffset = 0;
        assert!(parse_batch(&buffer, bytes, &mut entries).is_err());

        // 记录头本身超出本批有效字节（用 0 字节窗口模拟短读）。
        assert!(parse_batch(&buffer, header - 1, &mut entries).is_err());
    }

    /// 根目录本身是 junction 时必须拒绝打开，而不是解析到目标后继续服务。
    /// 这条用例需要创建 junction（`mklink /J` 不需要管理员权限）。
    ///
    /// 未在无 junction 权限或非 NTFS 的环境下验证；创建失败时跳过并说明原因，
    /// 不把「没跑」说成「已通过」。
    #[test]
    fn junction_root_is_rejected() {
        let base = tempfile::tempdir().unwrap();
        let target = base.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = base.path().join("link");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !status.is_ok_and(|status| status.success()) {
            eprintln!("skipping: cannot create a junction in this environment");
            return;
        }
        assert!(
            open_ambient_dir(&link).is_err(),
            "a junction root must not be opened"
        );
    }

    /// 枚举类必须取自 Win32 的 `FILE_INFO_BY_HANDLE_CLASS`：`RestartInfo = 11`、
    /// `Info = 10`。
    ///
    /// 这条断言固定住「Wdk 的 `FileIdBothDirectoryInformation = 37` 混进来」这一类
    /// 回归：两个常量都是 `i32` 别名，传错值不会产生类型错误，只有数值断言能在
    /// 不依赖内核行为的情况下立即发现。真实枚举另由下面的多批/并发用例覆盖。
    #[test]
    fn enumeration_classes_match_win32_api() {
        assert_eq!(FileIdBothDirectoryRestartInfo, 11);
        assert_eq!(FileIdBothDirectoryInfo, 10);
    }

    /// 对目录句柄发起一次枚举并解析本批结果。
    ///
    /// 生产路径的 `read_entries` 每次都以 `RestartInfo` 开头，无法观察两个句柄是否
    /// 共享游标；测试需要按调用方指定的枚举类单独推进一侧，才能让「另一方从哪里
    /// 继续」成为可断言的差异。helper 只存在于测试模块，不改变生产 API。
    fn read_one_batch(
        dir: &DirHandle,
        class: FILE_INFO_BY_HANDLE_CLASS,
    ) -> std::io::Result<Vec<DirEntryInfo>> {
        let mut buffer = vec![0u64; DIRECTORY_BUFFER_BYTES / 8];
        let ok = unsafe {
            GetFileInformationByHandleEx(
                dir.handle.as_raw_handle() as HANDLE,
                class,
                buffer.as_mut_ptr() as *mut _,
                DIRECTORY_BUFFER_BYTES as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut entries = Vec::new();
        parse_batch(&buffer, DIRECTORY_BUFFER_BYTES, &mut entries)?;
        Ok(entries)
    }

    /// 克隆出的目录句柄必须各自持有独立枚举游标。
    ///
    /// 旧用例只连续调用三次 `read_entries`：每次都以 `RestartInfo` 重置位置，即使
    /// 两个句柄共享同一个文件对象也会各自从头返回，无法识别共享游标。这里改为顺序
    /// 发起单批枚举：先让 `first` 从 `RestartInfo` 读一批、再推进一批，然后让从未读过
    /// 的 `second` 直接发普通 `Info` 请求。若两者共享文件对象，`second` 会从 `first`
    /// 停下的位置继续（或直接 `ERROR_NO_MORE_FILES`）；只有独立文件对象才会重新返回
    /// 目录开头。调用是顺序的，不依赖线程调度，结果可稳定复现。
    #[test]
    fn concurrent_listings_do_not_share_cursor() {
        let directory = tempfile::tempdir().unwrap();
        let handle = open_ambient_dir(directory.path()).unwrap();
        // 名字固定 56 个 UTF-16 单元、共 900 条：按 64 KiB 批大小至少跨三批，
        // 保证「推进两批」之后仍有下一批可供比较，不把批数不足误当成游标隔离。
        let total = 900usize;
        for index in 0..total {
            std::fs::write(
                directory
                    .path()
                    .join(format!("shared-{index:04}-{}.txt", "y".repeat(40))),
                b"x",
            )
            .unwrap();
        }
        let names = |entries: &[DirEntryInfo]| {
            let mut names = entries
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        assert_eq!(read_entries(&handle).unwrap().len(), total);

        let first = clone_dir(&handle).unwrap();
        let second = clone_dir(&handle).unwrap();

        let first_batch = read_one_batch(&first, FileIdBothDirectoryRestartInfo).unwrap();
        let first_next = read_one_batch(&first, FileIdBothDirectoryInfo).unwrap();
        assert!(!first_batch.is_empty());
        assert!(!first_next.is_empty());
        assert_ne!(
            first_batch[0].name, first_next[0].name,
            "advancing the first handle must move its own cursor"
        );

        // `second` 从未读过；独立游标意味着它的第一批必须与 `first` 的第一批相同。
        let second_first = read_one_batch(&second, FileIdBothDirectoryInfo).unwrap();
        assert_eq!(
            names(&second_first),
            names(&first_batch),
            "a cloned handle must start at the directory head, not where the other handle stopped"
        );

        // 两边继续推进时仍各自沿自己的序列前进，互不跳批。
        let first_third = read_one_batch(&first, FileIdBothDirectoryInfo).unwrap();
        let second_next = read_one_batch(&second, FileIdBothDirectoryInfo).unwrap();
        assert!(
            !first_third.is_empty(),
            "test directory must span at least three batches"
        );
        assert_eq!(names(&second_next), names(&first_next));
        assert_ne!(names(&first_third), names(&first_next));
    }

    /// 暂存提交回归：名字必须写在 `FileName` 的真实偏移上，且同名第二次提交自动改名。
    ///
    /// 这条测试固定住曾经的错误码 123（`STATUS_OBJECT_NAME_INVALID`）：按 `size_of`
    /// 追加名字会让内核读到错位的 UTF-16，测试能直接暴露该类布局回归。
    /// 提交直接以写句柄为源对象：这是上传身份绑定在同一回归里的落点。
    #[test]
    fn staging_commit_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_ambient_dir(dir.path()).unwrap();
        let (name, mut file) = create_staging(&handle).unwrap();
        file.write_all(b"hello").unwrap();
        file.sync_all().unwrap();
        assert_eq!(
            commit_staging(&handle, file, &name, "note.txt").unwrap(),
            "note.txt"
        );

        let (second, mut file) = create_staging(&handle).unwrap();
        file.write_all(b"world").unwrap();
        // 提交需要写句柄本身作为改名源，不能先 drop：句柄是身份绑定的唯一依据。
        assert_eq!(
            commit_staging(&handle, file, &second, "note.txt").unwrap(),
            "note (1).txt"
        );
    }

    /// 单 UTF-16 单元的目标名必须能真实提交，且同名仍按「不覆盖」自动改名。
    ///
    /// 单字符名按字段偏移只有 22 字节，小于 `FILE_RENAME_INFORMATION` 的 `size_of`
    /// （24，含占位与尾部填充）；内核实测以 `ERROR_BAD_LENGTH`（24）拒绝 22 字节，
    /// 因此 `rename_handle` 把长度下限设为结构体大小。该用例固定这一边界，同时保留
    /// 同名第二次提交自动改名、原文件内容不变的断言。
    #[test]
    fn single_unit_name_commit_is_accepted_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_ambient_dir(dir.path()).unwrap();
        let (staging, mut file) = create_staging(&handle).unwrap();
        file.write_all(b"first").unwrap();
        file.sync_all().unwrap();
        assert_eq!(commit_staging(&handle, file, &staging, "a").unwrap(), "a");
        assert_eq!(std::fs::read(dir.path().join("a")).unwrap(), b"first");

        let (second, mut file) = create_staging(&handle).unwrap();
        file.write_all(b"second").unwrap();
        file.sync_all().unwrap();
        assert_eq!(
            commit_staging(&handle, file, &second, "a").unwrap(),
            "a (1)"
        );
        assert_eq!(std::fs::read(dir.path().join("a")).unwrap(), b"first");
        assert_eq!(std::fs::read(dir.path().join("a (1)")).unwrap(), b"second");
    }

    /// 并发替换同名暂存文件不能改变提交的内容：这是上传身份绑定的判别性回归。
    ///
    /// 攻击模型：同目录有本地写入者，在本插件「创建暂存 → 写入 → 提交」的窗口里
    /// 把同名的暂存文件换掉。这里先把暂存目录项改名（我们的写句柄仍指向原 inode），
    /// 再在原来的暂存名处放一个内容不同的文件。按名字重新定位源文件的提交会提交
    /// 诱饵内容；句柄绑定的提交只认当初那个写句柄，提交内容不受影响。
    #[test]
    fn commit_submits_handle_contents_despite_name_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_ambient_dir(dir.path()).unwrap();
        let (name, mut file) = create_staging(&handle).unwrap();
        file.write_all(b"ours").unwrap();
        file.sync_all().unwrap();

        // 把暂存目录项挪走，再用同名诱饵占位；写句柄仍指向我们写入的 inode。
        let moved = dir.path().join("moved-staging");
        std::fs::rename(dir.path().join(&name), &moved).unwrap();
        std::fs::write(dir.path().join(&name), b"theirs").unwrap();

        assert_eq!(
            commit_staging(&handle, file, &name, "note.txt").unwrap(),
            "note.txt"
        );
        // 提交后的名字必须仍是本插件写入的字节；诱饵文件还留在原暂存名下。
        assert_eq!(std::fs::read(dir.path().join("note.txt")).unwrap(), b"ours");
        assert_eq!(std::fs::read(dir.path().join(&name)).unwrap(), b"theirs");
    }
}
