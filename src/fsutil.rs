use crate::model::Snapshot;
use anyhow::{bail, Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};

pub fn validate_component(name: &str) -> Result<()> {
    // Windows 拒绝尾随空格/点；其他 Unicode 空白（如全角空格、NBSP）同样会被部分
    // 文件系统与工具视为尾随空白，一并保守拒绝。
    if name.is_empty()
        || name == "."
        || name == ".."
        || name
            .chars()
            .last()
            .is_some_and(|c| c == '.' || c.is_whitespace())
    {
        bail!("不安全或不兼容 Windows 的名称：{name:?}");
    }
    if name
        .chars()
        .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
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
    match fs::symlink_metadata(path.join(".git")) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("无法检查 Git 目录边界：{}", path.display()))
        }
    }
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
pub fn normalize_root(path: &Path) -> Result<PathBuf> {
    let root = fs::canonicalize(path).context("无法访问目标目录")?;
    if !root.is_dir() || root.parent().is_none() {
        bail!("请选择普通目录，不允许直接整理整个磁盘根目录");
    }
    #[cfg(windows)]
    {
        // 组件数 <= 2 同时覆盖盘根与 UNC 共享根：UNC 形态的 server\share 被吸收进
        // Prefix（\\server\share 为 2 组件，canonicalize 后的 \\?\UNC\server\share 甚至
        // 只有 1 个组件），因此共享根（含 \\srv\d$ 管理共享）与整盘根同样被拒绝。
        if root.components().count() <= 2 {
            bail!("不允许整理磁盘根目录");
        }
        for var in [
            "SystemRoot",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramData",
        ] {
            if let Some(protected) = std::env::var_os(var) {
                if let Ok(protected) = fs::canonicalize(protected) {
                    if root.starts_with(&protected) {
                        bail!("不允许整理 Windows / 程序安装 / 系统数据目录");
                    }
                }
            }
        }
    }
    Ok(root)
}
pub fn snapshot(path: &Path) -> Result<Snapshot> {
    let metadata = fs::symlink_metadata(path)?;
    snapshot_with(path, &metadata)
}
/// 与 [`snapshot`] 相同，但复用调用方已取得的元数据（如目录枚举随条目带回的
/// symlink 元数据，Windows 上不产生额外系统调用），只为标识与硬链接数补开一次句柄。
pub fn snapshot_with(path: &Path, metadata: &fs::Metadata) -> Result<Snapshot> {
    if is_link(metadata) || !metadata.is_file() {
        bail!("不是普通文件：{}", path.display());
    }
    let modified_ns = match metadata.modified()?.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).context("文件时间超出范围")?,
        Err(e) => -i64::try_from(e.duration().as_nanos()).context("文件时间超出范围")?,
    };
    // 创建时间随移动忠实保留（S-01 跨卷复制同口径）；系统/文件系统不提供时为 None。
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
    #[cfg(unix)]
    let (identity, links) = {
        use std::os::unix::fs::MetadataExt;
        (
            format!("{}:{}", metadata.dev(), metadata.ino()),
            metadata.nlink(),
        )
    };
    #[cfg(windows)]
    let (identity, links) = {
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
        (
            format!(
                "{}:{}:{}",
                info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
            ),
            u64::from(info.nNumberOfLinks),
        )
    };
    Ok(Snapshot {
        size: metadata.len(),
        modified_ns,
        created_ns,
        identity,
        links,
    })
}
/// User-file moves are strictly no-replace and never copy data across volumes.
pub fn rename_noreplace(source: &Path, target: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
        let s: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let t: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: s/t 都是以 NUL 结尾的 UTF-16 缓冲区；MoveFileExW 只在调用期间读取这两个指针。
        if unsafe { MoveFileExW(s.as_ptr(), t.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
            return Err(std::io::Error::last_os_error()).context("移动失败（不会覆盖或跨卷复制）");
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
}
/// S-01：用户文件的最终移动入口。同卷走不覆盖改名（Windows 同卷改名天然保留创建时间，
/// 满足 S-01 忠实移动要求）；确因跨文件系统失败时按「不覆盖完整复制 → 设置创建/修改
/// 时间 → 删除源项」执行，复制、写时间或删除任一失败都保留源项并如实报错。
pub fn move_file_preserving_times(source: &Path, target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    let created = metadata.created().ok();
    let modified = metadata.modified().ok();
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
            // fs::copy 不会覆盖已存在目标的打开语义由调用方保证（规划期已预留目标名，
            // 执行用不覆盖兜底再核一次），复制失败不删除源项。
            if let Err(error) = fs::copy(source, target) {
                let _ = fs::remove_file(target);
                return Err(anyhow::Error::new(error).context("跨卷复制失败，源文件已保留"));
            }
            if let Err(error) = set_created_and_modified(target, created, modified) {
                let _ = fs::remove_file(target);
                return Err(error.context("跨卷副本时间设置失败，已保留源文件与副本状态"));
            }
            fs::remove_file(source)
                .with_context(|| format!("源删除失败；两份均保留：{}", source.display()))?;
            Ok(())
        }
    }
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
        let to_filetime = |time: std::time::SystemTime| -> FILETIME {
            const EPOCH_DELTA_100NS: u64 = 116_444_736_000_000_000;
            // 时间换算为 100ns 单位；越界值钳制到 u64::MAX（SetFileTime 会拒绝非法值）。
            let units = match time.duration_since(std::time::UNIX_EPOCH) {
                Ok(delta) => {
                    let total = u128::from(EPOCH_DELTA_100NS) + delta.as_nanos() / 100;
                    u64::try_from(total).unwrap_or(u64::MAX)
                }
                Err(_) => EPOCH_DELTA_100NS,
            };
            FILETIME {
                dwLowDateTime: u32::try_from(units % (1u64 << 32)).unwrap_or(0),
                dwHighDateTime: u32::try_from(units >> 32).unwrap_or(0),
            }
        };
        let created_ft = created.map(to_filetime);
        let modified_ft = modified.map(to_filetime);
        let creation_ptr = created_ft
            .as_ref()
            .map_or(std::ptr::null(), std::ptr::from_ref::<FILETIME>);
        let modified_ptr = modified_ft
            .as_ref()
            .map_or(std::ptr::null(), std::ptr::from_ref::<FILETIME>);
        // SAFETY: handle 有效；两个指针指向本函数栈上的 FILETIME 或为 NULL（表示不修改）。
        let ok = unsafe { SetFileTime(handle, creation_ptr, std::ptr::null(), modified_ptr) };
        // SAFETY: 关闭本函数打开的句柄。
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error()).context("写回创建/修改时间失败");
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
pub fn ensure_parent(root: &Path, target: &Path) -> Result<()> {
    let rel = relative_string(root, target)?;
    safe_join(root, &rel)?;
    let parent = target.parent().context("目标没有父目录")?;
    fs::create_dir_all(parent)?;
    safe_join(root, &rel)?;
    Ok(())
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
/// 截断后剥掉尾部点/空白，保证结果仍是合法组件。ext 自带前导点（可为空串）。
pub fn suffixed_candidate(stem: &str, ext: &str, index: u64) -> String {
    let sep = format!(" ({index})");
    let budget = 255usize.saturating_sub(sep.encode_utf16().count());
    // stem 非空时至少给 stem 保留 1 个单元，避免扩展名占满预算后候选名以序号空格开头。
    let ext_cap = budget.saturating_sub(usize::from(!stem.is_empty()));
    // 扩展名截断后剥掉尾部点与任意 Unicode 空白，与 validate_component 的尾随判定同口径。
    let ext: String = truncate_utf16(ext, ext_cap)
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace())
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
/// X-10 / 附录 B 不可拆分后缀的白名单组件：编号尾卷 `.NNN` 向左逐段并入时，
/// 只认 X-01 白名单扩展名与 `.tar.<流后缀>` 组合的各段（含 tgz 等别名）。
fn is_volume_suffix_component(component: &str) -> bool {
    [
        "gz", "bz2", "xz", "zst", "lzma", "z", "7z", "zip", "rar", "tar", "tgz", "tbz2", "txz",
        "tzst",
    ]
    .iter()
    .any(|kind| component.eq_ignore_ascii_case(kind))
}

/// 内段是否为 part rar 的编号段（`part<数字>`，至少一位数字；X-10 / 附录 B）。
fn is_part_rar_component(component: &str) -> bool {
    // 先验证切分点在字符边界上，避免多字节字符处切出 panic。
    component.len() > 4 && component.is_char_boundary(4) && {
        let (head, tail) = component.split_at(4);
        head.eq_ignore_ascii_case("part") && tail.bytes().all(|byte| byte.is_ascii_digit())
    }
}

/// 分离文件名主体与完整扩展名；扩展名含前导点，直接借用原串。
/// 不可拆分后缀（X-10 / 附录 B）：
/// - 数字尾卷 `.NNN`（恰好三位 ASCII 数字）向左逐段并入白名单后缀组件：
///   `x.tar.gz.001` → 主体 `x` + 扩展名 `.tar.gz.001`；非白名单组件不并入
///   （`foo.bar.001` 保持主体 `foo.bar`）。
/// - part rar 的 `.partN.rar` 整体并入扩展名（`资料.part01.rar` → `资料`）。
/// - 普通复合压缩流仍按「`.tar` + 流后缀」并入一层（`资料.tar.gz` → `资料`）。
pub fn split_compound_name(name: &str) -> (&str, &str) {
    let Some(mut split) = name.rfind('.').filter(|&index| index > 0) else {
        return (name, "");
    };
    let (stem, extension) = name.split_at(split);
    let numbered_tail =
        extension.len() == 4 && extension.as_bytes()[1..].iter().all(u8::is_ascii_digit);
    if numbered_tail {
        // 向左逐段并入白名单组件；遇到非白名单组件或抵达主体即停。
        let mut current = stem;
        while let Some(dot) = current.rfind('.').filter(|&index| index > 0) {
            if !is_volume_suffix_component(&current[dot + 1..]) {
                break;
            }
            current = &current[..dot];
            split = dot;
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
    if let Some(inner_dot) = stem.rfind('.').filter(|&index| index > 0) {
        let inner = &stem[inner_dot + 1..];
        if inner.eq_ignore_ascii_case("tar") {
            split = inner_dot;
        }
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
        if let Err(error) = fs::symlink_metadata(&path) {
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(path);
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
        fs::create_dir_all(state)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(state.join("organizer.lock"))?;
        fs2::FileExt::try_lock_exclusive(&file)
            .context("另一个目录整理任务正在运行，请先结束它")?;
        Ok(Self(file))
    }
}
impl Drop for RootGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    // 覆盖 X-10, 附录 B（回归：数字尾卷 `.NNN` 向左逐段并入白名单后缀组件；
    // 非白名单组件不得并入）
    #[test]
    fn split_merges_whitelisted_components_behind_numbered_tail() {
        assert_eq!(split_compound_name("x.tar.gz.001"), ("x", ".tar.gz.001"));
        assert_eq!(split_compound_name("包.7z.001"), ("包", ".7z.001"));
        assert_eq!(split_compound_name("y.tbz2.001"), ("y", ".tbz2.001"));
        assert_eq!(split_compound_name("foo.bar.001"), ("foo.bar", ".001"));
        assert_eq!(split_compound_name("x.tar.foo.001"), ("x.tar.foo", ".001"));
    }

    // 覆盖 H-07（复合压缩流扩展名整体保留；既有行为不回归）
    #[test]
    fn split_keeps_compound_stream_suffix() {
        assert_eq!(split_compound_name("资料.tar.gz"), ("资料", ".tar.gz"));
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
}
