use crate::model::Snapshot;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::{fs::{self, File, OpenOptions}, io::Write, path::{Component, Path, PathBuf}, time::UNIX_EPOCH};

pub fn validate_component(name: &str) -> Result<()> {
    // Windows 拒绝尾随空格/点；其他 Unicode 空白（如全角空格、NBSP）同样会被部分
    // 文件系统与工具视为尾随空白，一并保守拒绝。
    if name.is_empty() || name == "." || name == ".."
        || name.chars().last().is_some_and(|c| c == '.' || c.is_whitespace()) {
        bail!("不安全或不兼容 Windows 的名称：{name:?}");
    }
    if name.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)) {
        bail!("文件名包含 Windows 不支持的字符：{name:?}");
    }
    // 官方保留名清单为 COM1-9 / LPT1-9（COM0/LPT0 并非保留名，可正常创建），
    // 这里按官方清单拒绝，不做额外扩大，避免误拒用户磁盘上真实存在的合法文件。
    let stem = name.split('.').next().unwrap_or("").to_uppercase();
    if ["CON", "PRN", "AUX", "NUL", "CLOCK$"].contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.chars().count() == 4
            && stem.chars().last().is_some_and(|c| "123456789¹²³".contains(c))) {
        bail!("Windows 保留文件名：{name}");
    }
    if name.encode_utf16().count() > 255 { bail!("单个文件名超过 255 个 UTF-16 单元"); }
    Ok(())
}
pub fn safe_relative(raw: &str) -> Result<PathBuf> {
    if raw.starts_with('/') || raw.starts_with('\\') { bail!("拒绝压缩包绝对路径：{raw:?}"); }
    let normalized = raw.replace('\\', "/");
    let mut out = PathBuf::new();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." { continue; }
        validate_component(part)?;
        out.push(part);
    }
    if out.as_os_str().is_empty() { bail!("空文件路径"); }
    Ok(out)
}
pub fn path_string(path: &Path) -> Result<String> {
    Ok(path.to_str().context("路径含不能无损表示为 UTF-8 的字符，已拒绝处理")?.to_string())
}
pub fn relative_string(root: &Path, path: &Path) -> Result<String> {
    Ok(path_string(path.strip_prefix(root).context("路径不在选定目录内")?)?.replace('\\', "/"))
}
pub fn is_link(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() { return true; }
    #[cfg(windows)] {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 { return true; }
    }
    false
}
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let p = safe_relative(rel)?;
    let mut current = root.to_path_buf();
    for part in p.components() {
        if !matches!(part, Component::Normal(_)) { bail!("非法路径组件"); }
        current.push(part.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(meta) if is_link(&meta) => bail!("拒绝符号链接 / junction / reparse point：{}", current.display()),
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e).with_context(|| format!("无法检查 {}", current.display())),
        }
    }
    Ok(current)
}
pub fn normalize_root(path: &Path) -> Result<PathBuf> {
    let root = fs::canonicalize(path).context("无法访问目标目录")?;
    if !root.is_dir() || root.parent().is_none() { bail!("请选择普通目录，不允许直接整理整个磁盘根目录"); }
    #[cfg(windows)] {
        if root.components().count() <= 2 { bail!("不允许整理磁盘根目录"); }
        for var in ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)", "ProgramData"] {
            if let Some(protected) = std::env::var_os(var) {
                if let Ok(protected) = fs::canonicalize(protected) {
                    if root.starts_with(&protected) { bail!("不允许整理 Windows / 程序安装 / 系统数据目录"); }
                }
            }
        }
    }
    Ok(root)
}
pub fn snapshot(path: &Path) -> Result<Snapshot> {
    let metadata = fs::symlink_metadata(path)?;
    if is_link(&metadata) || !metadata.is_file() { bail!("不是普通文件：{}", path.display()); }
    let modified_ns = match metadata.modified()?.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).context("文件时间超出范围")?,
        Err(e) => -i64::try_from(e.duration().as_nanos()).context("文件时间超出范围")?,
    };
    #[cfg(unix)]
    let (identity, links) = {
        use std::os::unix::fs::MetadataExt;
        (format!("{}:{}", metadata.dev(), metadata.ino()), metadata.nlink())
    };
    #[cfg(windows)]
    let (identity, links) = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
        let file = File::open(path)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error()).context("读取 Windows 文件标识失败");
        }
        (format!("{}:{}:{}", info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow), info.nNumberOfLinks as u64)
    };
    Ok(Snapshot { size: metadata.len(), modified_ns, identity, links })
}
pub fn unchanged(path: &Path, expected: &Snapshot) -> Result<()> {
    let actual = snapshot(path)?;
    // Link counts can change when another selected alias is removed; identity/content metadata must not.
    if actual.size != expected.size || actual.modified_ns != expected.modified_ns || actual.identity != expected.identity {
        bail!("扫描后文件已发生变化，已跳过：{}", path.display());
    }
    Ok(())
}
pub fn open_stable_read(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)] {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1); // FILE_SHARE_READ; deny writes/deletes while hashing.
    }
    Ok(options.open(path)?)
}
/// User-file moves are strictly no-replace and never copy data across volumes.
pub fn rename_noreplace(source: &Path, target: &Path) -> Result<()> {
    #[cfg(windows)] {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
        let s: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let t: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe { MoveFileExW(s.as_ptr(), t.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
            return Err(std::io::Error::last_os_error()).context("移动失败（不会覆盖或跨卷复制）");
        }
    }
    #[cfg(target_os = "linux")] {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let s = CString::new(source.as_os_str().as_bytes())?;
        let t = CString::new(target.as_os_str().as_bytes())?;
        let rc = unsafe { libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, s.as_ptr(), libc::AT_FDCWD, t.as_ptr(), libc::RENAME_NOREPLACE) };
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
        if let Err(e) = fs::remove_file(source) { let _ = fs::remove_file(target); return Err(e.into()); }
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "linux")))] {
        // Safe file-only fallback. No existing target can be overwritten.
        fs::hard_link(source, target)?;
        if let Err(e) = fs::remove_file(source) { let _ = fs::remove_file(target); return Err(e.into()); }
    }
    Ok(())
}
pub fn ensure_parent(root: &Path, target: &Path) -> Result<()> {
    let rel = relative_string(root, target)?;
    safe_join(root, &rel)?;
    let parent = target.parent().context("目标没有父目录")?;
    fs::create_dir_all(parent)?;
    safe_join(root, &rel)?;
    Ok(())
}
pub fn unique_target(root: &Path, requested: &Path) -> Result<PathBuf> {
    let parent = requested.parent().context("目标没有父目录")?;
    let stem = requested.file_stem().and_then(|v| v.to_str()).context("无效文件名")?;
    let ext = requested.extension().and_then(|v| v.to_str());
    for index in 1u64..=1_000_000 {
        let name = match ext { Some(ext) => format!("{stem} ({index}).{ext}"), None => format!("{stem} ({index})") };
        let path = parent.join(name);
        let rel = relative_string(root, &path)?;
        for part in rel.split('/') { validate_component(part)?; }
        // 符号链接/坏链视为占用并试下一个序号（与 planner::target_will_be_free 对齐），
        // 不得因 safe_join 的链接拒绝而整函数失败。
        match fs::symlink_metadata(&path) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(path),
            Err(_) => continue,
        }
    }
    bail!("无法为 {} 分配不冲突的名称：已尝试 {stem} (1)…{stem} (1000000) 均已被占用", requested.display())
}
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("配置路径缺少父目录")?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?; file.sync_all()?; drop(file);
        #[cfg(windows)] {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
            let a: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
            let b: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(not(windows))] {
            fs::rename(&tmp, path)?;
            // 与 Windows MOVEFILE_WRITE_THROUGH 对齐：目录项落盘后再返回。
            if let Ok(dir) = File::open(parent) { let _ = dir.sync_all(); }
        }
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&tmp); }
    result
}
pub struct RootGuard(File);
impl RootGuard {
    pub fn acquire(state: &Path) -> Result<Self> {
        fs::create_dir_all(state)?;
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false)
            .open(state.join("organizer.lock"))?;
        fs2::FileExt::try_lock_exclusive(&file).context("另一个目录整理任务正在运行，请先结束它")?;
        Ok(Self(file))
    }
}
impl Drop for RootGuard {
    fn drop(&mut self) { let _ = fs2::FileExt::unlock(&self.0); }
}
