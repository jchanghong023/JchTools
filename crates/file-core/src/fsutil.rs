use crate::model::Snapshot;
use anyhow::{bail, Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};

fn log_filesystem_result<T>(
    operation: &'static str,
    started: std::time::Instant,
    result: &Result<T>,
) {
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    match result {
        Ok(_) => tracing::debug!(
            event = "filesystem_operation_completed",
            operation,
            elapsed_ms,
            "文件系统操作完成"
        ),
        Err(error) => {
            let io = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<std::io::Error>());
            tracing::error!(
                event = "filesystem_operation_failed",
                operation,
                elapsed_ms,
                error_type = if io.is_some() {
                    "io"
                } else {
                    "filesystem_boundary"
                },
                error_code = io.and_then(std::io::Error::raw_os_error),
                "文件系统操作失败"
            );
        }
    }
}
pub fn validate_component(name: &str) -> Result<()> {
    // Windows 禁止尾随 ASCII 空格/点；合法 Unicode 空白由可选名称规范化规则处理。
    if name.is_empty() || name == "." || name == ".." || name.ends_with(['.', ' ']) {
        bail!("不安全或不兼容 Windows 的名称：{name:?}");
    }
    if name
        .chars()
        .any(|c| c <= '\u{1f}' || "<>:\"/\\|?*".contains(c))
    {
        bail!("文件名包含 Windows 不支持的字符：{name:?}");
    }
    // 官方保留名清单为 COM1-9 / LPT1-9（COM0/LPT0 并非保留名，可正常创建），
    // 这里按官方清单拒绝，不做额外扩大，避免误拒用户磁盘上真实存在的合法文件。
    let stem = name.split('.').next().unwrap_or("").to_uppercase();
    if ["CON", "PRN", "AUX", "NUL", "CLOCK$"].contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.chars().count() == 4
            && stem
                .chars()
                .last()
                .is_some_and(|c| "123456789¹²³".contains(c)))
    {
        bail!("Windows 保留文件名：{name}");
    }
    if name.encode_utf16().count() > 255 {
        bail!("单个文件名超过 255 个 UTF-16 单元");
    }
    Ok(())
}
pub fn safe_relative(raw: &str) -> Result<PathBuf> {
    if raw.starts_with('/') || raw.starts_with('\\') {
        bail!("拒绝压缩包绝对路径：{raw:?}");
    }
    let normalized = raw.replace('\\', "/");
    let mut out = PathBuf::new();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        validate_component(part)?;
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        bail!("空文件路径");
    }
    Ok(out)
}
pub fn path_string(path: &Path) -> Result<String> {
    Ok(path
        .to_str()
        .context("路径含不能无损表示为 UTF-8 的字符，已拒绝处理")?
        .to_string())
}
pub fn relative_string(root: &Path, path: &Path) -> Result<String> {
    Ok(path_string(path.strip_prefix(root).context("路径不在选定目录内")?)?.replace('\\', "/"))
}
/// Windows 序数忽略大小写比较键，保留 UTF-16 单元顺序；非 Windows 平台原样返回。
/// 相对路径比较前一律先经本函数折叠，「比较前先折叠」这一不变量只有这一处实现。
pub fn fold_rel(name: &str) -> String {
    #[cfg(windows)]
    {
        let mut key = String::new();
        for unit in name.encode_utf16().map(ordinal_fold_unit) {
            // 字符串键需保持 UTF-16 单元的顺序，包括代理项；将代理项映射到其间隔
            // 区域，再把更大的 BMP 单元整体平移，使 String 排序等价于单元序数排序。
            let code_point = u32::from(unit);
            let code_point = if code_point >= 0xD800 {
                code_point + 0x800
            } else {
                code_point
            };
            if let Some(character) = char::from_u32(code_point) {
                key.push(character);
            }
        }
        key
    }
    #[cfg(not(windows))]
    {
        name.to_string()
    }
}

#[cfg(windows)]
fn ordinal_fold_unit(unit: u16) -> u16 {
    if unit < 0x80 {
        return if (u16::from(b'a')..=u16::from(b'z')).contains(&unit) {
            unit - 32
        } else {
            unit
        };
    }
    if (0xD800..=0xDFFF).contains(&unit) {
        return unit;
    }
    // CompareStringOrdinal 使用逐 UTF-16 单元的大写映射；这些希腊字母的简单
    // 单元映射与 Rust 全大写结果不同，按操作系统的序数比较行为折叠。
    match unit {
        0x1F80..=0x1F87 | 0x1F90..=0x1F97 | 0x1FA0..=0x1FA7 => return unit + 0x08,
        0x1FB3 => return 0x1FBC,
        0x1FC3 => return 0x1FCC,
        0x1FF3 => return 0x1FFC,
        _ => {}
    }
    let Some(mut uppercase) = char::from_u32(u32::from(unit)).map(char::to_uppercase) else {
        return unit;
    };
    let Some(mapped) = uppercase.next() else {
        return unit;
    };
    if uppercase.next().is_some() || mapped.len_utf16() != 1 {
        return unit;
    }
    u16::try_from(u32::from(mapped)).unwrap_or(unit)
}

pub fn is_link(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}
/// 只检查目录边界，不遍历 Git 工作树内部；`.git` 文件与目录均保护整树。
pub fn is_git_root(path: &Path) -> Result<bool> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        // 路径不存在或不是目录（如解压流程里按折叠键重建的成员路径）：
        // 与旧口径（path/.git 的 symlink_metadata NotFound → 无边界）一致，
        // 视为无 .git 边界而不是任务失败。
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("无法检查 Git 目录边界：{}", path.display()));
        }
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("无法检查 Git 目录边界：{}", path.display()))?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(".git"))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// H-06：沿所选根的祖先到卷根检查直接目录项；任一祖先直接含 `.git` 说明所选根
/// 位于 Git 项目内部，两工具都必须拒绝整次处理（不拆散项目子树）。
pub fn root_inside_git_project(root: &Path) -> Result<()> {
    let mut current = root.parent();
    while let Some(dir) = current {
        if is_git_root(dir)? {
            bail!(
                "所选目录位于 Git 项目（{}）内部：为保护项目完整，本次处理不执行；请选择项目外的目录",
                dir.display()
            );
        }
        current = dir.parent();
    }
    Ok(())
}
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let p = safe_relative(rel)?;
    let mut current = root.to_path_buf();
    for part in p.components() {
        if !matches!(part, Component::Normal(_)) {
            bail!("非法路径组件");
        }
        current.push(part.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(meta) if is_link(&meta) => bail!(
                "拒绝符号链接 / junction / reparse point：{}",
                current.display()
            ),
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e).with_context(|| format!("无法检查 {}", current.display())),
        }
    }
    Ok(current)
}
/// S-04：所选根本身或其任一上级组件是符号链接/junction/reparse point 时拒绝开始。
/// 检查必须作用于用户给出的原始路径（canonicalize 之前）——规范化会把链接解析成
/// 目标路径，reparse 身份随之丢失，链接边界就再也检不出来，两工具会沿 canonicalize
/// 结果处理链接目标树。逐组件（含根本身）用 symlink_metadata 判定并复用 [`is_link`]；
/// 不读取链接目标、不搬移、不删除链接本身。
pub fn ensure_plain_entry(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if !matches!(component, Component::Normal(_)) {
            // 盘符前缀、根分隔符与 `.`/`..` 组件不是可判定的目录项，跳过；
            // 路径最终是否存在、是否为目录交给 normalize_root 报告。
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(meta) if is_link(&meta) => bail!(
                "所选路径或其上级包含符号链接/junction（{}）；已按 S-04 拒绝开始：不跟随链接、不搬移或删除链接本身，请直接选择实际目录",
                current.display()
            ),
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => {
                return Err(e).with_context(|| format!("无法检查路径边界：{}", current.display()));
            }
        }
    }
    Ok(())
}
/// 工作区容器冲突只按现有不覆盖命名生成规范正整数后缀；不认可其他相似名称。
fn is_extraction_workspace_container(name: &str) -> bool {
    if name.eq_ignore_ascii_case(".jchtools-work") {
        return true;
    }
    let Some((base, suffix)) = name.rsplit_once(" (") else {
        return false;
    };
    let Some(number) = suffix.strip_suffix(')') else {
        return false;
    };
    base.eq_ignore_ascii_case(".jchtools-work")
        && !number.is_empty()
        && !number.starts_with('0')
        && number.bytes().all(|byte| byte.is_ascii_digit())
}

/// X-08：仅识别现有格式的真实解压工作区，不把同名用户目录当作工具所有物。
/// 识别不授予删除权限；清理仍须持有本次生成的所有权标记。
pub fn is_extraction_workspace(path: &Path) -> Result<bool> {
    use std::io::Read;

    if !path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .is_some_and(is_extraction_workspace_container)
    {
        return Ok(false);
    }
    let Some(owner) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };
    let mut canonical = [0u8; 36];
    if owner.len() != 36
        || !uuid::Uuid::parse_str(owner)
            .is_ok_and(|value| value.hyphenated().encode_lower(&mut canonical) == owner)
    {
        return Ok(false);
    }
    let directory = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("无法检查解压工作区目录"),
    };
    if is_link(&directory) || !directory.is_dir() {
        return Ok(false);
    }
    let marker = path.join("OWNER");
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("无法检查解压工作区所有权标记"),
    };
    anyhow::ensure!(
        !is_link(&metadata) && metadata.is_file(),
        "可疑解压工作区的所有权标记不是普通文件，拒绝进入"
    );
    if metadata.len() != 36 {
        return Ok(false);
    }
    let mut recorded_owner = String::with_capacity(36);
    File::open(&marker)
        .context("无法读取解压工作区所有权标记")?
        .take(37)
        .read_to_string(&mut recorded_owner)
        .context("无法读取解压工作区所有权标记")?;
    if recorded_owner != owner {
        return Ok(false);
    }
    let content = match fs::symlink_metadata(path.join("content")) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("无法检查解压工作区内容目录"),
    };
    anyhow::ensure!(
        !is_link(&content),
        "可疑解压工作区的内容目录为链接，拒绝进入"
    );
    Ok(content.is_dir())
}

/// S-05 受保护目录解析的纯函数核心（测试注入用，不触碰真实环境变量）：
/// 缺失或无法规范化都报错——保护信息不可用时必须拒绝开始，不得静默失去系统目录保护。
fn resolve_protected_root(raw: Option<&std::ffi::OsStr>) -> Result<PathBuf> {
    let raw = raw.context(
        "无法确定 Windows 系统目录（SystemRoot 未设置）；为避免误处理系统文件，拒绝开始处理",
    )?;
    fs::canonicalize(raw).with_context(|| {
        format!(
            "无法访问 Windows 系统目录（{}）；为避免误处理系统文件，拒绝开始处理",
            Path::new(raw).display()
        )
    })
}
/// 读取并规范化操作系统报告的实际 Windows 安装目录，不信任可改写的环境变量。
#[cfg(windows)]
pub fn protected_root() -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;

    let query = |buffer: &mut [u16]| -> Result<usize> {
        let capacity = u32::try_from(buffer.len()).context("Windows 系统目录长度超出范围")?;
        // SAFETY: 缓冲区可写且容量与传入长度一致；系统调用不会保留指针。
        let length = unsafe { GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), capacity) };
        if length == 0 {
            return Err(std::io::Error::last_os_error()).context("无法确定实际 Windows 系统目录");
        }
        usize::try_from(length).context("Windows 系统目录长度超出范围")
    };
    let mut buffer = [0u16; 260];
    let length = query(&mut buffer)?;
    let directory = if length < buffer.len() {
        std::ffi::OsString::from_wide(&buffer[..length])
    } else {
        let mut extended = vec![
            0u16;
            length
                .checked_add(1)
                .context("Windows 系统目录长度超出范围")?
        ];
        let length = query(&mut extended)?;
        anyhow::ensure!(length < extended.len(), "无法完整读取实际 Windows 系统目录");
        std::ffi::OsString::from_wide(&extended[..length])
    };
    resolve_protected_root(Some(&directory))
}
/// S-05 的参数化核心：规范化根，并给出「根包含受保护目录时需整树剪枝的子树」。
/// - 根位于受保护目录内（含等于）：拒绝整次任务；
/// - 根是受保护目录的严格上层：不拒绝，返回 `(根, Some(受保护目录))`，由扫描、
///   确认框清点与收尾清理按该子树统一剪枝并提示（S-05 第二句）；
/// - 其余：返回 `(根, None)`。`protected` 为 None 表示没有受保护目录信息（非 Windows）。
pub fn normalize_root_with(
    path: &Path,
    protected: Option<&Path>,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let root = fs::canonicalize(path).context("无法访问目标目录")?;
    if !root.is_dir() {
        bail!("请选择普通目录");
    }
    #[cfg(not(windows))]
    if root.parent().is_none() {
        // 非产品平台维持原有根目录边界；Windows 盘根按 S-05 排除系统子树。
        bail!("不允许直接整理根目录");
    }
    // S-05 只授权拒绝 Windows 安装目录及其后代；Program Files、用户目录、
    // 盘根不自动扩大进拒绝清单，仍受其余范围与权限规则约束。
    if let Some(protected) = protected {
        if root.starts_with(protected) {
            bail!("不允许整理 Windows 系统目录");
        }
    }
    // X-08：直接选择真实标记工作区或其后代，也不能绕过扫描范围排除。
    // 只在候选容器命名族内增加必要 Git 检查，避免在 Git 树内读取 OWNER 内容；
    // 普通根不增加重复 Git 扫描，预检仍按原有 H-06 闸门执行。
    for directory in root.ancestors() {
        if !directory
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(is_extraction_workspace_container)
        {
            continue;
        }
        root_inside_git_project(directory)?;
        anyhow::ensure!(
            !is_git_root(directory)?,
            "所选目录位于 Git 项目内；按 H-06 本次处理不执行"
        );
        anyhow::ensure!(
            !is_extraction_workspace(directory)?,
            "所选目录位于带所有权标记的解压工作区内；本次处理不执行，请先将待处理资料移出工作区"
        );
    }
    let protected_prune = protected
        .filter(|protected| protected.starts_with(&root))
        .map(Path::to_path_buf);
    Ok((root, protected_prune))
}
pub fn normalize_root(path: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let protected = protected_root()?;
        Ok(normalize_root_with(path, Some(&protected))?.0)
    }
    #[cfg(not(windows))]
    {
        normalize_root_with(path, None).map(|(root, _)| root)
    }
}
pub fn snapshot(path: &Path) -> Result<Snapshot> {
    let metadata = fs::symlink_metadata(path)?;
    snapshot_with(path, &metadata)
}
/// 与 [`snapshot`] 相同，复用调用方已取得的元数据检查普通文件与链接边界。
/// Windows 同一次句柄查询取得大小、时间、标识与链接数，避免硬链接目录项的陈旧时间；
/// Unix 继续复用目录枚举元数据，不增加扫描或元数据调用。
pub fn snapshot_with(path: &Path, metadata: &fs::Metadata) -> Result<Snapshot> {
    if is_link(metadata) || !metadata.is_file() {
        bail!("不是普通文件：{}", path.display());
    }
    #[cfg(unix)]
    let (size, modified_ns, created_ns, identity, links) = {
        use std::os::unix::fs::MetadataExt;
        let modified_ns = match metadata.modified()?.duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_nanos()).context("文件时间超出范围")?,
            Err(e) => -i64::try_from(e.duration().as_nanos()).context("文件时间超出范围")?,
        };
        // 创建时间随移动忠实保留；系统/文件系统不提供或超出范围时为 None。
        let created_ns =
            metadata
                .created()
                .ok()
                .and_then(|time| match time.duration_since(UNIX_EPOCH) {
                    Ok(delta) => i64::try_from(delta.as_nanos()).ok(),
                    Err(error) => i64::try_from(error.duration().as_nanos())
                        .ok()
                        .map(|value| -value),
                });
        (
            metadata.len(),
            modified_ns,
            created_ns,
            format!("{}:{}", metadata.dev(), metadata.ino()),
            metadata.nlink(),
        )
    };
    #[cfg(windows)]
    let (size, modified_ns, created_ns, identity, links) = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let file = File::open(path)?;
        // SAFETY: BY_HANDLE_FILE_INFORMATION 是纯 POD 结构，全零是合法初值（无必须非零的字段）。
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: file 是刚打开的有效句柄；调用只向 info 写入，不保留指针出本作用域。
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) } == 0 {
            return Err(std::io::Error::last_os_error()).context("读取 Windows 文件标识失败");
        }
        // FILETIME 为 1601 起算的 100ns 单位；在 i128 中先减纪元再乘，
        // 保留 1970 前的负时间，mtime 越界报错，ctime 越界仍按原口径为 None。
        const EPOCH_DELTA_100NS: i128 = 116_444_736_000_000_000;
        let [modified_ns, created_ns] = [info.ftLastWriteTime, info.ftCreationTime].map(|time| {
            let units = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
            i64::try_from((i128::from(units) - EPOCH_DELTA_100NS) * 100)
        });
        (
            (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
            modified_ns.context("文件时间超出范围")?,
            created_ns.ok(),
            format!(
                "{}:{}:{}",
                info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
            ),
            u64::from(info.nNumberOfLinks),
        )
    };
    Ok(Snapshot {
        size,
        modified_ns,
        created_ns,
        identity,
        links,
    })
}
/// User-file moves are strictly no-replace and never copy data across volumes.
pub fn rename_noreplace(source: &Path, target: &Path) -> Result<()> {
    let span = jchtools_foundation::logging::operation_span("fsutil", "rename_noreplace");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::debug!(event = "filesystem_rename_started", "不覆盖改名开始");
    let result: Result<()> = (|| {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
            let s: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
            let t: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: s/t 都是以 NUL 结尾的 UTF-16 缓冲区；MoveFileExW 只在调用期间读取这两个指针。
            if unsafe { MoveFileExW(s.as_ptr(), t.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("移动失败（不会覆盖或跨卷复制）");
            }
            Ok(())
        }
        #[cfg(target_os = "linux")]
        {
            use std::{ffi::CString, os::unix::ffi::OsStrExt};
            let s = CString::new(source.as_os_str().as_bytes())?;
            let t = CString::new(target.as_os_str().as_bytes())?;
            // SAFETY: s/t 是合法 CString（路径不含 NUL，构造失败会提前返回）；
            // renameat2 按 libc 约定传 AT_FDCWD + RENAME_NOREPLACE，内核侧不保留指针。
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    libc::AT_FDCWD,
                    s.as_ptr(),
                    libc::AT_FDCWD,
                    t.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if rc == 0 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            // 老内核/受限 seccomp 可能没有 renameat2：回退到与其它 Unix 相同的硬链接路径，
            // 目标已存在时 hard_link 失败，仍保持不覆盖语义。
            if err.raw_os_error() != Some(libc::ENOSYS) {
                return Err(err).context("不覆盖移动失败");
            }
            fs::hard_link(source, target)?;
            if let Err(e) = fs::remove_file(source) {
                let _ = fs::remove_file(target);
                return Err(e.into());
            }
            Ok(())
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            // Safe file-only fallback. No existing target can be overwritten.
            fs::hard_link(source, target)?;
            if let Err(e) = fs::remove_file(source) {
                let _ = fs::remove_file(target);
                return Err(e.into());
            }
            Ok(())
        }
    })();
    log_filesystem_result("rename", started, &result);
    result
}
/// S-01：不覆盖复制（跨卷移动的落盘核心）。目标已存在时按 `AlreadyExists`
/// 失败；只有 `create_new` 成功后才拥有目标，后续失败只清理这个新建副本。
fn copy_noreplace(source: &Path, target: &Path) -> std::io::Result<()> {
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    let copied = std::io::copy(&mut input, &mut output).and_then(|_| output.sync_all());
    // 即使复制失败也先关闭输出；不能从错误种类推断目标是否由本次创建。
    let result = copied.and(close_file(output));
    if result.is_err() {
        if let Err(cleanup) = fs::remove_file(target) {
            tracing::warn!(
                event = "filesystem_move_cleanup_failed",
                stage = "copy_rollback",
                error_code = cleanup.raw_os_error(),
                error_type = "io",
                "跨卷失败副本清理失败"
            );
        }
    }
    result
}

#[cfg(windows)]
fn close_file(file: File) -> std::io::Result<()> {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::Foundation::CloseHandle;

    let handle = file.into_raw_handle();
    // SAFETY: handle 由 File 转移所有权，本函数只关闭一次且不再访问。
    if unsafe { CloseHandle(handle) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn close_file(file: File) -> std::io::Result<()> {
    drop(file);
    Ok(())
}
/// FILETIME 换算核心（100ns 单位、1601 纪元）：接受相对 UNIX 纪元的偏移
/// （Ok = 1970 之后，Err = 1970 之前的时长）。1601-1970 的负偏移受检折算；
/// 早于 1601-01-01（FILETIME 合法下界）下溢返回 Err 明确报错，不得钳制成 1970
/// 或其他静默值（S-01 忠实移动：时间写不回去就必须如实失败并保留源项）；
/// 超出 u64 上限的远未来值钳制到 u64::MAX（SetFileTime 会拒绝非法值）。
/// 独立成纯函数便于直接测试越界值——SystemTime 在 Windows 上无法表示早于
/// 1601 的时刻（checked_sub 返回 None），越界分支无法经 SystemTime 构造。
#[cfg(windows)]
fn offset_to_filetime(
    offset: Result<std::time::Duration, std::time::Duration>,
) -> Result<windows_sys::Win32::Foundation::FILETIME> {
    use windows_sys::Win32::Foundation::FILETIME;
    const EPOCH_DELTA_100NS: u64 = 116_444_736_000_000_000;
    let units = match offset {
        Ok(delta) => {
            let total = u128::from(EPOCH_DELTA_100NS) + delta.as_nanos() / 100;
            u64::try_from(total).unwrap_or(u64::MAX)
        }
        Err(before) => u128::from(EPOCH_DELTA_100NS)
            .checked_sub(before.as_nanos() / 100)
            .and_then(|units| u64::try_from(units).ok())
            .context("文件时间早于 1601-01-01，超出 FILETIME 可表示范围")?,
    };
    Ok(FILETIME {
        dwLowDateTime: u32::try_from(units % (1u64 << 32)).unwrap_or(0),
        dwHighDateTime: u32::try_from(units >> 32).unwrap_or(0),
    })
}
/// 把 SystemTime 换算为 FILETIME；负偏移（1601-1970）按 [`offset_to_filetime`]
/// 受检折算，越界（早于 1601）返回 Err。
#[cfg(windows)]
fn systemtime_to_filetime(
    time: std::time::SystemTime,
) -> Result<windows_sys::Win32::Foundation::FILETIME> {
    offset_to_filetime(match time.duration_since(UNIX_EPOCH) {
        Ok(delta) => Ok(delta),
        Err(error) => Err(error.duration()),
    })
}
/// S-01：用户文件的最终移动入口。同卷走不覆盖改名（Windows 同卷改名天然保留创建时间，
/// 满足 S-01 忠实移动要求）；确因跨文件系统失败时按「不覆盖完整复制 → 设置创建/修改
/// 时间 → 删除源项」执行，复制、写时间或删除任一失败都保留源项并如实报错。
pub fn move_file_preserving_times(source: &Path, target: &Path) -> Result<()> {
    let span = jchtools_foundation::logging::operation_span("fsutil", "move_file");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::debug!(event = "filesystem_move_started", "保留时间移动开始");
    let result: Result<()> = (|| {
        match rename_noreplace(source, target) {
            Ok(()) => Ok(()),
            Err(error) => {
                let cross_volume = error
                    .root_cause()
                    .downcast_ref::<std::io::Error>()
                    .and_then(std::io::Error::raw_os_error)
                    .is_some_and(|code| {
                        // 17 = Windows ERROR_NOT_SAME_DEVICE；18 = POSIX EXDEV。
                        code == 17 || code == 18
                    });
                if !cross_volume {
                    return Err(error);
                }
                tracing::warn!(
                    event = "filesystem_move_fallback",
                    stage = "cross_volume_copy",
                    error_code = error
                        .chain()
                        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
                        .and_then(std::io::Error::raw_os_error),
                    "跨卷改名不可用，按既有策略复制并保留时间"
                );
                tracing::debug!(
                    event = "filesystem_move_stage_started",
                    stage = "source_metadata",
                    "跨卷移动读取源文件时间信息开始"
                );
                let metadata =
                    fs::symlink_metadata(source).context("无法读取跨卷移动源文件属性")?;
                #[cfg(windows)]
                let created = Some(
                    metadata
                        .created()
                        .context("跨卷移动无法读取源文件创建时间")?,
                );
                #[cfg(not(windows))]
                let created = metadata.created().ok();
                let modified = Some(
                    metadata
                        .modified()
                        .context("跨卷移动无法读取源文件修改时间")?,
                );
                // S-01：复制核心在 create_new 成功后才取得副本清理所有权；
                // 源打开或目标创建失败不能导致调用方删除既有目标。
                tracing::debug!(
                    event = "filesystem_move_stage_started",
                    stage = "copy",
                    "跨卷副本写入开始"
                );
                if let Err(error) = copy_noreplace(source, target) {
                    let context = if error.kind() == std::io::ErrorKind::AlreadyExists {
                        format!("跨卷移动目标已存在，不覆盖既有文件（{}）", target.display())
                    } else {
                        "跨卷复制失败，源文件已保留".to_string()
                    };
                    return Err(anyhow::Error::new(error).context(context));
                }
                tracing::debug!(
                    event = "filesystem_move_stage_started",
                    stage = "restore_times",
                    "跨卷副本时间恢复开始"
                );
                if let Err(error) = set_created_and_modified(target, created, modified) {
                    if let Err(cleanup) = fs::remove_file(target) {
                        tracing::warn!(
                            event = "filesystem_move_cleanup_failed",
                            stage = "times_rollback",
                            error_code = cleanup.raw_os_error(),
                            error_type = "io",
                            "跨卷失败副本清理失败"
                        );
                    }
                    return Err(error.context("跨卷副本时间设置失败，已保留源文件与副本状态"));
                }
                tracing::debug!(
                    event = "filesystem_move_stage_started",
                    stage = "source_delete",
                    "跨卷源文件删除开始"
                );
                fs::remove_file(source)
                    .with_context(|| format!("源删除失败；两份均保留：{}", source.display()))?;
                Ok(())
            }
        }
    })();
    log_filesystem_result("move", started, &result);
    result
}
/// 把创建/修改时间写回文件（S-01 跨卷复制后必须恢复创建时间，保证 C-21 幂等）。
fn set_created_and_modified(
    path: &Path,
    created: Option<std::time::SystemTime>,
    modified: Option<std::time::SystemTime>,
) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, SetFileTime, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING,
        };
        let created_ft = created.map(systemtime_to_filetime).transpose()?;
        let modified_ft = modified.map(systemtime_to_filetime).transpose()?;
        let path16: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: path16 是 NUL 结尾的 UTF-16 缓冲区；句柄在本函数内关闭，指针不出作用域。
        let handle = unsafe {
            CreateFileW(
                path16.as_ptr(),
                FILE_WRITE_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error()).context("打开文件以写回时间失败");
        }
        let creation_ptr = created_ft
            .as_ref()
            .map_or(std::ptr::null(), std::ptr::from_ref::<FILETIME>);
        let modified_ptr = modified_ft
            .as_ref()
            .map_or(std::ptr::null(), std::ptr::from_ref::<FILETIME>);
        // SAFETY: handle 有效；两个指针指向本函数栈上的 FILETIME 或为 NULL（表示不修改）。
        let ok = unsafe { SetFileTime(handle, creation_ptr, std::ptr::null(), modified_ptr) };
        let set_error = if ok == 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };
        // SAFETY: 关闭本函数打开的句柄。
        let close_ok = unsafe { CloseHandle(handle) };
        if let Some(error) = set_error {
            return Err(error).context("写回创建/修改时间失败");
        }
        if close_ok == 0 {
            return Err(std::io::Error::last_os_error()).context("关闭文件时间句柄失败");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let file = fs::OpenOptions::new().write(true).open(path)?;
        if let Some(modified) = modified {
            file.set_modified(modified)?;
        }
        Ok(())
    }
}
/// 设置文件/目录的创建时间（S-01 跨卷复制写回创建时间的同一底层能力；
/// 测试用它伪造创建日期）。
pub fn set_created_time(path: &Path, created: std::time::SystemTime) -> Result<()> {
    set_created_and_modified(path, Some(created), None)
}
/// 返回目标直接父目录是否由本次调用实际创建；既有目录不授予收尾删除所有权。
pub fn ensure_parent(root: &Path, target: &Path) -> Result<bool> {
    let rel = relative_string(root, target)?;
    safe_join(root, &rel)?;
    let parent = target.parent().context("目标没有父目录")?;
    let creation = match fs::create_dir(parent) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(parent.parent().context("目标父目录没有上层目录")?)?;
            fs::create_dir(parent)
        }
        result => result,
    };
    let created = match creation {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    safe_join(root, &rel)?;
    Ok(created)
}
/// 创建目录链本身（`directory` 就是要创建的目录，不再上溯一层）。
/// `ensure_parent` 只创建传入路径的父级，拿目录调用它只会创建到祖父目录：
/// 解压成员合入必须用本函数，否则带子目录的成员会以「系统找不到指定的路径」整包失败。
/// 与 `ensure_parent` 一样对目录链做链接/junction 校验（拒绝把成员写进重定向目录），
/// 但不校验 `directory` 之下的最终名——那是成员的最终文件名，调用方另有落盘兜底。
pub fn ensure_dir(root: &Path, directory: &Path) -> Result<()> {
    let rel = relative_string(root, directory)?;
    if rel.is_empty() {
        return Ok(());
    } // 选定的根目录自身，无需创建
    safe_join(root, &rel)?;
    fs::create_dir_all(directory)?;
    safe_join(root, &rel)?;
    Ok(())
}
/// 生成「stem (N)ext」形式的候选名；整体超过 Windows 单组件上限（255 个 UTF-16
/// 单元）时按「序号标记 → 扩展名 → stem」的优先级截断，而不是让 validate_component
/// 把整次分配搞失败。截断按 UTF-16 单元预算逐字符进行，不会切开代理对；扩展名
/// 截断后剥掉尾部 ASCII 点/空格，保证结果仍是合法组件。ext 自带前导点（可为空串）。
pub fn suffixed_candidate(stem: &str, ext: &str, index: u64) -> String {
    let sep = format!(" ({index})");
    let budget = 255usize.saturating_sub(sep.encode_utf16().count());
    // stem 非空时至少给 stem 保留 1 个单元，避免扩展名占满预算后候选名以序号空格开头。
    let ext_cap = budget.saturating_sub(usize::from(!stem.is_empty()));
    // 只剥掉 Windows 禁止的尾部 ASCII 点/空格；合法 Unicode 空白属于原扩展名。
    let ext: String = truncate_utf16(ext, ext_cap)
        .trim_end_matches(['.', ' '])
        .to_string();
    let budget = budget - ext.encode_utf16().count();
    let cut = truncate_utf16(stem, budget);
    format!("{cut}{sep}{ext}")
}
/// 按 UTF-16 单元上限截断字符串：逐字符累计 len_utf16，不切开代理对。
fn truncate_utf16(text: &str, max_units: usize) -> String {
    let mut out = String::new();
    let mut units = 0usize;
    for ch in text.chars() {
        let need = ch.len_utf16();
        if units + need > max_units {
            break;
        }
        units += need;
        out.push(ch);
    }
    out
}
/// X-10 数字尾卷编号：至少三位 ASCII 数字；超过三位时不得有多余前导零。
/// `.000` 保留为可识别的非法起始编号，合法起始与入口由调用方判定。
pub fn is_numbered_volume_digits(digits: &str) -> bool {
    digits.len() >= 3
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && (digits.len() == 3 || digits.as_bytes()[0] != b'0')
}

/// X-10：规范数字尾卷编号（最少三位补零，`.999` 后自然增长）前只并入最后一个白名单扩展名；
/// `.tar.<流后缀>` 再按附录 B 整体并入一层。
fn is_volume_suffix_component(component: &str) -> bool {
    [
        "gz", "bz2", "xz", "zst", "lzma", "z", "7z", "zip", "rar", "tar", "tgz", "tbz2", "txz",
        "tzst",
    ]
    .iter()
    .any(|kind| component.eq_ignore_ascii_case(kind))
}

/// 附录 B：只有六类压缩流允许与紧邻的 `.tar` 组成复合扩展名。
fn tar_stream_split(stem: &str, extension: &str) -> Option<usize> {
    if !["gz", "bz2", "xz", "zst", "lzma", "z"]
        .iter()
        .any(|kind| extension.eq_ignore_ascii_case(kind))
    {
        return None;
    }
    stem.rfind('.')
        .filter(|&dot| dot > 0 && stem[dot + 1..].eq_ignore_ascii_case("tar"))
}

/// 内段是否为 part rar 的编号段（`part<正整数>`；X-10 / 附录 B）。
fn is_part_rar_component(component: &str) -> bool {
    // 先验证切分点在字符边界上，避免多字节字符处切出 panic。
    component.len() > 4 && component.is_char_boundary(4) && {
        let (head, tail) = component.split_at(4);
        head.eq_ignore_ascii_case("part")
            && tail.bytes().all(|byte| byte.is_ascii_digit())
            && tail.bytes().any(|byte| byte != b'0')
    }
}

/// 分离文件名主体与完整扩展名；扩展名含前导点，直接借用原串。
/// 不可拆分后缀（X-10 / 附录 B）：
/// - 数字尾卷（最少三位 ASCII 数字，超过三位无前导零）只并入前一白名单扩展名及允许的
///   一层 `.tar.<流后缀>`：`x.tar.gz.001` → 主体 `x` + 扩展名 `.tar.gz.001`；
///   非白名单组件不并入（`foo.bar.001` 保持主体 `foo.bar`）。
/// - part rar 的 `.partN.rar` 整体并入扩展名（`资料.part01.rar` → `资料`）。
/// - 普通复合压缩流仍按「`.tar` + 流后缀」并入一层（`资料.tar.gz` → `资料`）。
pub fn split_compound_name(name: &str) -> (&str, &str) {
    let Some(mut split) = name.rfind('.').filter(|&index| index > 0) else {
        return (name, "");
    };
    let (stem, extension) = name.split_at(split);
    if is_numbered_volume_digits(&extension[1..]) {
        if let Some(dot) = stem.rfind('.').filter(|&index| index > 0) {
            let component = &stem[dot + 1..];
            if is_volume_suffix_component(component) {
                split = tar_stream_split(&stem[..dot], component).unwrap_or(dot);
            }
        }
        return name.split_at(split);
    }
    if extension.eq_ignore_ascii_case(".rar") {
        if let Some(dot) = stem.rfind('.').filter(|&index| index > 0) {
            if is_part_rar_component(&stem[dot + 1..]) {
                return name.split_at(dot);
            }
        }
    }
    if let Some(inner_dot) = tar_stream_split(stem, &extension[1..]) {
        split = inner_dot;
    }
    name.split_at(split)
}
pub fn unique_target(root: &Path, requested: &Path) -> Result<PathBuf> {
    let parent = requested.parent().context("目标没有父目录")?;
    let name = requested
        .file_name()
        .and_then(|value| value.to_str())
        .context("无效文件名")?;
    let (stem, ext) = split_compound_name(name);
    let extension_units = ext.encode_utf16().count();
    for index in 1u64..=1_000_000 {
        if extension_units + index.ilog10() as usize + 5 > 255 {
            bail!("原扩展名过长，无法保留扩展名并追加冲突序号");
        }
        let name = suffixed_candidate(stem, ext, index);
        let path = parent.join(name);
        let rel = relative_string(root, &path)?;
        for part in rel.split('/') {
            validate_component(part)?;
        }
        // 符号链接/坏链视为占用并试下一个序号（与 planner::target_will_be_free 对齐），
        // 不得因 safe_join 的链接拒绝而整函数失败。
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(path),
            Err(error) => {
                return Err(error).with_context(|| format!("无法检查目标占用：{}", path.display()));
            }
        }
    }
    bail!(
        "无法为 {} 分配不冲突的名称：已尝试 {stem} (1)…{stem} (1000000) 均已被占用",
        requested.display()
    )
}
pub struct RootGuard(File);
impl RootGuard {
    pub fn acquire(state: &Path) -> Result<Self> {
        let span = jchtools_foundation::logging::operation_span("fsutil", "task_lock");
        let _entered = span.enter();
        let started = std::time::Instant::now();
        tracing::debug!(event = "filesystem_lock_started", "任务锁获取开始");
        let result: Result<Self> = (|| {
            fs::create_dir_all(state)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(state.join("organizer.lock"))?;
            fs2::FileExt::try_lock_exclusive(&file)
                .context("另一个递归解压或目录整理任务正在运行，请先结束它")?;
            Ok(Self(file))
        })();
        log_filesystem_result("lock", started, &result);
        result
    }
}
impl Drop for RootGuard {
    fn drop(&mut self) {
        if let Err(error) = fs2::FileExt::unlock(&self.0) {
            tracing::warn!(
                event = "filesystem_unlock_failed",
                error_type = "io",
                error_code = error.raw_os_error(),
                "任务锁显式释放失败，仍关闭文件句柄"
            );
        }
    }
}

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
mod tests {
    use super::*;

    // 覆盖 X-08 / S-04：名字或单独 OWNER 不是完整工作区；真实标记不授予删除权限。
    #[test]
    fn workspace_detection_requires_matching_owner_and_plain_content_directory() {
        let root = tempfile::tempdir().unwrap();
        let wrapper = root.path().join(".JCHTOOLS-WORK");
        let owner = "11111111-1111-4111-8111-111111111111";
        let workspace = wrapper.join(owner);
        fs::create_dir_all(&workspace).unwrap();
        assert!(!is_extraction_workspace(&wrapper).unwrap());
        assert!(!is_extraction_workspace(&workspace).unwrap());
        fs::write(workspace.join("OWNER"), owner).unwrap();
        assert!(!is_extraction_workspace(&workspace).unwrap(), "缺少 content 的用户目录仍需纳入");
        fs::write(workspace.join("content"), b"user file").unwrap();
        assert!(!is_extraction_workspace(&workspace).unwrap(), "content 普通文件不是工作区");
        fs::remove_file(workspace.join("content")).unwrap();
        fs::create_dir(workspace.join("content")).unwrap();
        assert!(is_extraction_workspace(&workspace).unwrap());
        fs::write(workspace.join("OWNER"), "22222222-2222-4222-8222-222222222222").unwrap();
        assert!(!is_extraction_workspace(&workspace).unwrap(), "标记必须与目录 UUID 一致");
        assert!(workspace.join("OWNER").is_file(), "识别不授予删除授权");
    }

    // 覆盖 X-08 / S-04：规范冲突容器和原容器共用标记协议；
    // 直接选择其根或后代必须拒绝，非规范同名用户目录不被扩大排除。
    #[test]
    fn workspace_namespace_and_selected_root_boundaries_are_exact() {
        let root = tempfile::tempdir().unwrap();
        let owner = "11111111-1111-4111-8111-111111111111";
        for (index, (container, recognized)) in [
            (".jchtools-work", true),
            (".JCHTOOLS-WORK (2)", true),
            (".jchtools-work (0)", false),
            (".jchtools-work (02)", false),
            (".jchtools-work (2) extra", false),
        ].into_iter().enumerate() {
            let workspace = root.path().join(format!("case-{index}")).join(container).join(owner);
            let content = workspace.join("content");
            let nested = content.join("nested");
            fs::create_dir_all(&nested).unwrap();
            fs::write(workspace.join("OWNER"), owner).unwrap();
            assert_eq!(is_extraction_workspace(&workspace).unwrap(), recognized, "{container}");
            for selected in [&workspace, &content, &nested] {
                let result = normalize_root_with(selected, None);
                if recognized {
                    let error = result.expect_err("真实工作区及后代不能直接处理");
                    assert!(format!("{error:#}").contains("解压工作区"), "{error:#}");
                } else {
                    assert!(result.is_ok(), "普通非规范容器不得仅按名字拒绝：{container}");
                }
            }
            assert_eq!(fs::read(workspace.join("OWNER")).unwrap(), owner.as_bytes());
        }
    }

    // 覆盖 H-07 / X-04：直接调用真实候选生成函数，不依赖 GUI 集成测试链接。
    #[test]
    fn suffix_keeps_legal_unicode_extension() {
        for whitespace in ['\u{00a0}', '\u{2003}', '\u{3000}'] {
            let extension = format!(".txt{whitespace}");
            validate_component(&format!("资料{extension}")).unwrap();
            assert_eq!(
                suffixed_candidate("资料", &extension, 1),
                format!("资料 (1){extension}"),
                "合法扩展名中的 Unicode 空白不得被修剪"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn fold_rel_matches_windows_ordinal_case_and_order() {
        assert_ne!(fold_rel("\u{0130}"), fold_rel("i\u{0307}"));
        assert_eq!(fold_rel("\u{1f80}"), fold_rel("\u{1f88}"));
        assert!(
            fold_rel("\u{10000}") < fold_rel("\u{e000}"),
            "折叠键顺序必须保留 Windows UTF-16 序数顺序"
        );
    }

    // 覆盖 X-10, 附录 B（回归：part rar 的 `.partN.rar` 是不可拆分后缀，
    // 冲突改名的序号插在整个后缀之前，如 `资料 (1).part01.rar`）
    #[test]
    fn split_keeps_part_rar_suffix_whole() {
        assert_eq!(
            split_compound_name("资料.part01.rar"),
            ("资料", ".part01.rar")
        );
        assert_eq!(split_compound_name("a.part1.rar"), ("a", ".part1.rar"));
        // 仅含 “.part” 子串的普通包不按 part rar 后缀处理。
        assert_eq!(
            split_compound_name("report.partial.rar"),
            ("report.partial", ".rar")
        );
        // part 段必须带数字；`.partN.zip` 不属于 part rar 族。
        assert_eq!(split_compound_name("x.part.rar"), ("x.part", ".rar"));
        assert_eq!(split_compound_name("x.part01.zip"), ("x.part01", ".zip"));
        // X-10 的 N 是正整数；全零不是 part rar 卷集后缀。
        assert_eq!(split_compound_name("x.part0.rar"), ("x.part0", ".rar"));
        assert_eq!(split_compound_name("x.part000.rar"), ("x.part000", ".rar"));
    }

    // 覆盖 X-10, 附录 B（数字尾卷只并入前一白名单扩展名及一层 tar 压缩流；
    // 主体中碰巧同名的白名单段不得继续并入）
    #[test]
    fn split_merges_whitelisted_components_behind_numbered_tail() {
        assert_eq!(split_compound_name("x.tar.gz.001"), ("x", ".tar.gz.001"));
        assert_eq!(split_compound_name("包.7z.001"), ("包", ".7z.001"));
        assert_eq!(split_compound_name("y.tbz2.001"), ("y", ".tbz2.001"));
        assert_eq!(split_compound_name("foo.bar.001"), ("foo.bar", ".001"));
        assert_eq!(split_compound_name("x.tar.foo.001"), ("x.tar.foo", ".001"));
        assert_eq!(split_compound_name("a.zip.7z.001"), ("a.zip", ".7z.001"));
        assert_eq!(
            split_compound_name("a.tar.tar.gz.001"),
            ("a.tar", ".tar.gz.001")
        );
    }

    // 覆盖 H-07（复合压缩流扩展名整体保留；既有行为不回归）
    #[test]
    fn split_keeps_compound_stream_suffix() {
        assert_eq!(split_compound_name("资料.tar.gz"), ("资料", ".tar.gz"));
        assert_eq!(
            split_compound_name("report.tar.pdf"),
            ("report.tar", ".pdf")
        );
        assert_eq!(
            split_compound_name("report.tar.zip"),
            ("report.tar", ".zip")
        );
        assert_eq!(split_compound_name("report.tar"), ("report", ".tar"));
        assert_eq!(split_compound_name("a.txt"), ("a", ".txt"));
        assert_eq!(split_compound_name("无扩展名"), ("无扩展名", ""));
        assert_eq!(split_compound_name("combo.z01"), ("combo", ".z01"));
    }

    // 覆盖 X-06, X-10（回归：冲突改名把序号插在整组卷后缀之前）
    #[test]
    fn unique_target_inserts_index_before_volume_suffix() {
        let temp = tempfile::tempdir().unwrap();
        let occupied = temp.path().join("资料.part01.rar");
        fs::write(&occupied, b"existing").unwrap();
        let target = unique_target(temp.path(), &occupied).unwrap();
        assert_eq!(
            target.file_name().and_then(|name| name.to_str()).unwrap(),
            "资料 (1).part01.rar"
        );
    }

    #[test]
    fn numbered_volume_growth_split_preserves_complete_canonical_suffix() {
        for (name, expected) in [
            ("包.7z.999", ("包", ".7z.999")),
            ("包.7z.1000", ("包", ".7z.1000")),
            ("包.ZIP.1001", ("包", ".ZIP.1001")),
            ("x.tar.gz.1000", ("x", ".tar.gz.1000")),
            ("a.tar.tar.gz.1001", ("a.tar", ".tar.gz.1001")),
            ("a.zip.7z.1000", ("a.zip", ".7z.1000")),
            ("x.tbz2.18446744073709551616", ("x", ".tbz2.18446744073709551616")),
        ] {
            assert_eq!(split_compound_name(name), expected, "{name}");
        }
        for (name, expected) in [
            ("包.7z.0001", ("包.7z", ".0001")),
            ("包.7z.01000", ("包.7z", ".01000")),
            ("包.7z.1", ("包.7z", ".1")),
            ("包.7z.01", ("包.7z", ".01")),
            ("包.7z.１０００", ("包.7z", ".１０００")),
            ("包.7z.١٠٠٠", ("包.7z", ".١٠٠٠")),
            ("包.7z.1000.bak", ("包.7z.1000", ".bak")),
            ("foo.bar.1000", ("foo.bar", ".1000")),
            ("x.iso.1000", ("x.iso", ".1000")),
        ] {
            assert_eq!(split_compound_name(name), expected, "{name}");
        }
    }

    #[test]
    fn numbered_volume_growth_unique_target_preserves_numbered_suffix() {
        let temp = tempfile::tempdir().unwrap();
        for (name, first, second) in [
            ("资料.7z.1000", "资料 (1).7z.1000", "资料 (2).7z.1000"),
            ("资料.tar.gz.1001", "资料 (1).tar.gz.1001", "资料 (2).tar.gz.1001"),
            ("资料.7z.01000", "资料.7z (1).01000", "资料.7z (2).01000"),
        ] {
            let requested = temp.path().join(name);
            fs::write(&requested, b"existing").unwrap();
            let target = unique_target(temp.path(), &requested).unwrap();
            assert_eq!(target.file_name().and_then(|name| name.to_str()).unwrap(), first);
            fs::write(&target, b"collision").unwrap();
            let target = unique_target(temp.path(), &requested).unwrap();
            assert_eq!(target.file_name().and_then(|name| name.to_str()).unwrap(), second);
        }
    }

    /// F28 夹具：写一个临时文件并返回路径。
    fn f28_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("t.txt");
        fs::write(&file, b"x").unwrap();
        (temp, file)
    }

    /// 1970 纪元到 1601 纪元的换算常数（100ns 单位），即 1601-01-01 相对
    /// UNIX_EPOCH 的纳秒偏移 ÷ 100。
    const F28_DELTA_NS: u64 = 11_644_473_600_000_000_000;

    // 平台门禁原因：验证对象是 Windows FILETIME 语义与 SetFileTime 写回，
    // 非 Windows 分支不写创建时间，断言无意义。
    // 覆盖 S-01, C-21（回归：1969-12-31 的负偏移必须忠实写回；
    // 修复前 to_filetime 对 duration_since 的 Err 分支一律返回 1970-01-01）
    #[cfg(windows)]
    #[test]
    fn f28_pre_epoch_created_time_is_written_faithfully() {
        let (_temp, file) = f28_file();
        let target = UNIX_EPOCH
            .checked_sub(std::time::Duration::from_hours(24))
            .unwrap();
        set_created_time(&file, target).unwrap();
        let back = fs::metadata(&file).unwrap().created().unwrap();
        assert_eq!(
            UNIX_EPOCH.duration_since(back).unwrap(),
            std::time::Duration::from_hours(24),
            "1969-12-31 的创建时间必须原样写回，不得被抹成 1970-01-01"
        );
    }

    // 平台门禁原因：同上，FILETIME 1601 下界是 Windows 语义。
    // 覆盖 S-01, C-21（回归：恰好 1601-01-01 是 FILETIME 合法下界，必须接受）
    #[cfg(windows)]
    #[test]
    fn f28_accepts_exact_1601_boundary() {
        let (_temp, file) = f28_file();
        let boundary = UNIX_EPOCH
            .checked_sub(std::time::Duration::from_nanos(F28_DELTA_NS))
            .unwrap();
        assert!(
            set_created_time(&file, boundary).is_ok(),
            "恰好 1601-01-01 00:00:00 UTC 可表示，必须成功写回"
        );
    }

    // 平台门禁原因：同上。
    // 覆盖 S-01, C-21（回归：早于 1601-01-01 一个 100ns 单位必须显式报错；
    // 修复前负偏移一律被静默钳制成 1970。SystemTime 在 Windows 上无法表示早于
    // 1601 的时刻（checked_sub 返回 None），越界值经纯函数核心直接构造）
    #[cfg(windows)]
    #[test]
    fn f28_rejects_one_tick_before_1601() {
        assert!(
            offset_to_filetime(Err(std::time::Duration::from_nanos(F28_DELTA_NS + 100))).is_err(),
            "早于 1601-01-01 一个 100ns 单位超出 FILETIME 范围，必须报错而非钳制"
        );
    }

    // 平台门禁原因：同上。
    // 覆盖 S-01, C-21（纪元本身 1970-01-01 必须原样写回）
    #[cfg(windows)]
    #[test]
    fn f28_epoch_itself_is_written() {
        let (_temp, file) = f28_file();
        set_created_time(&file, UNIX_EPOCH).unwrap();
        let back = fs::metadata(&file).unwrap().created().unwrap();
        assert_eq!(
            back.duration_since(UNIX_EPOCH).unwrap(),
            std::time::Duration::ZERO
        );
    }

    // 平台门禁原因：同上。
    // 覆盖 S-01, C-21（现代值正偏移路径不回归）
    #[cfg(windows)]
    #[test]
    fn f28_modern_time_is_written() {
        let (_temp, file) = f28_file();
        let modern = UNIX_EPOCH + std::time::Duration::from_secs(1_768_000_000);
        set_created_time(&file, modern).unwrap();
        let back = fs::metadata(&file).unwrap().created().unwrap();
        assert_eq!(
            back.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_768_000_000
        );
    }

    // 平台门禁原因：同上。
    // 覆盖 S-01, C-21（越界值：远早于 1601 必须报错而非钳制）
    #[cfg(windows)]
    #[test]
    fn f28_rejects_far_pre_1601_value() {
        assert!(
            offset_to_filetime(Err(std::time::Duration::from_hours(400 * 366 * 24))).is_err(),
            "远早于 1601 的越界值必须报错而非钳制"
        );
    }

    // 覆盖 S-01（回归：跨卷移动的复制核心对已存在目标必须失败且不截断既有
    // 字节；修复前 fs::copy 以 create+truncate 打开目标，D→C 实测静默覆盖旧
    // 内容后删除源文件）
    #[test]
    fn copy_noreplace_refuses_existing_target_without_truncation() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("src.txt");
        let target = temp.path().join("dst.txt");
        fs::write(&source, b"new content from source").unwrap();
        fs::write(&target, b"existing target bytes").unwrap();
        let error = copy_noreplace(&source, &target).expect_err("目标已存在必须失败");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::AlreadyExists,
            "失败原因必须是目标已存在"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"existing target bytes",
            "既有目标字节不得被截断或改写"
        );
        assert_eq!(
            fs::read(&source).unwrap(),
            b"new content from source",
            "源文件保持原状"
        );
        // 正常路径：目标不存在时完整复制。
        let fresh = temp.path().join("fresh.txt");
        copy_noreplace(&source, &fresh).unwrap();
        assert_eq!(fs::read(&fresh).unwrap(), b"new content from source");
    }

    // 覆盖 S-05（SystemRoot 缺失或指向不可访问路径时必须报错拒绝开始，
    // 不得静默失去系统目录保护——修复前该分支被静默跳过）
    #[test]
    fn resolve_protected_root_errors_when_unavailable() {
        assert!(
            resolve_protected_root(None).is_err(),
            "受保护目录信息缺失必须报错，不得静默放行"
        );
        assert!(
            resolve_protected_root(Some(std::ffi::OsStr::new(r"Z:\不存在的受保护目录"))).is_err(),
            "受保护目录无法规范化必须报错，不得静默放行"
        );
    }

    // 覆盖 S-05（受保护目录参数化：根在其内或即其本身拒绝；根是严格上层时不拒绝、
    // 返回需整树剪枝的子树；无关根正常放行）
    #[test]
    fn normalize_root_with_synthetic_protected_dir() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir_all(root.join("win/System32")).unwrap();
        // 受保护目录 = root/win（位于根内，用于「严格上层」分支）。
        let win = fs::canonicalize(root.join("win")).unwrap();
        let inside = win.join("System32");
        fs::create_dir_all(&inside).unwrap();

        assert!(
            normalize_root_with(&inside, Some(&win)).is_err(),
            "根位于受保护目录内必须拒绝整次任务"
        );
        assert!(
            normalize_root_with(&win, Some(&win)).is_err(),
            "根即受保护目录必须拒绝整次任务"
        );
        let (got, prune) = normalize_root_with(&root, Some(&win)).unwrap();
        assert_eq!(got, fs::canonicalize(&root).unwrap());
        assert_eq!(
            prune.as_deref(),
            Some(win.as_path()),
            "根是严格上层时不拒绝，返回需整树剪枝的子树"
        );
        // 与根无关的受保护目录：不产生剪枝。
        let outside = temp.path().join("elsewhere");
        fs::create_dir_all(&outside).unwrap();
        let outside = fs::canonicalize(&outside).unwrap();
        let (_, prune) = normalize_root_with(&root, Some(&outside)).unwrap();
        assert!(prune.is_none(), "受保护目录不在根内时不产生剪枝");
    }
}
