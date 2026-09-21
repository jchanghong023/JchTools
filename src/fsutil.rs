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
    if is_link(&metadata) || !metadata.is_file() {
        bail!("不是普通文件：{}", path.display());
    }
    let modified_ns = match metadata.modified()?.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).context("文件时间超出范围")?,
        Err(e) => -i64::try_from(e.duration().as_nanos()).context("文件时间超出范围")?,
    };
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
        identity,
        links,
    })
}
pub fn open_stable_read(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1); // FILE_SHARE_READ; deny writes/deletes while hashing.
    }
    Ok(options.open(path)?)
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
pub fn unique_target(root: &Path, requested: &Path) -> Result<PathBuf> {
    let parent = requested.parent().context("目标没有父目录")?;
    let stem = requested
        .file_stem()
        .and_then(|v| v.to_str())
        .context("无效文件名")?;
    let ext = requested
        .extension()
        .and_then(|v| v.to_str())
        .map(|v| format!(".{v}"))
        .unwrap_or_default();
    for index in 1u64..=1_000_000 {
        let name = suffixed_candidate(stem, &ext, index);
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
