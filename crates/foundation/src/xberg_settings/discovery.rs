// 目录发现：测试覆盖、随包目录定位与 Windows 注册表安装位置（installed_directory）。
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub(super) fn test_directory_override(
    enabled: bool,
    read: impl Fn(&str) -> Option<OsString>,
) -> Result<Option<PathBuf>, String> {
    if enabled {
        if let Some(path) = read("JCHTOOLS_TEST_STATE_DIR") {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("测试状态目录必须为绝对路径".into());
            }
            return Ok(Some(path));
        }
        // 现有隔离测试不应读写真实用户配置。
        for key in [
            "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT",
            "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT",
        ] {
            if let Some(path) = read(key).map(PathBuf::from) {
                if !path.is_absolute() {
                    return Err("测试资产目录必须为绝对路径".into());
                }
                return Ok(Some(path.join("app-settings")));
            }
        }
    }
    Ok(None)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum BundleOverride {
    Unchanged,
    Isolated(Option<PathBuf>),
}

/// 测试隔离同时覆盖随包目录和注册表；生产策略不读取测试环境变量。
pub(super) fn test_bundle_override(
    enabled: bool,
    read: impl Fn(&str) -> Option<OsString>,
) -> Result<BundleOverride, String> {
    if !enabled {
        return Ok(BundleOverride::Unchanged);
    }
    if let Some(path) = read("JCHTOOLS_TEST_BUNDLED_XBERG_DIR") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("测试随包 Xberg 目录必须为绝对路径".into());
        }
        return Ok(BundleOverride::Isolated(Some(path)));
    }
    if test_directory_override(true, read)?.is_some() {
        return Ok(BundleOverride::Isolated(None));
    }
    Ok(BundleOverride::Unchanged)
}

pub(super) fn valid_runtime_directory(path: &Path) -> Result<bool, String> {
    if !path.is_absolute() {
        return Ok(false);
    }
    let exists = |member: &Path, directory: bool| -> Result<bool, String> {
        match std::fs::metadata(member) {
            Ok(metadata) => Ok(if directory {
                metadata.is_dir()
            } else {
                metadata.is_file()
            }),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(format!("检查 Xberg 运行目录失败：{error}")),
        }
    };
    Ok(exists(path, true)? && exists(&path.join("xberg.exe"), false)?)
}

pub(super) fn discover_bundle(
    executable: &Path,
    windows_apps: Option<&Path>,
    installed_directory: impl FnOnce() -> Result<Option<PathBuf>, String>,
) -> Result<Option<PathBuf>, String> {
    let parent = executable.parent().ok_or("无法定位当前程序所在目录")?;
    let sibling = parent.join("xberg");
    if valid_runtime_directory(&sibling)? {
        return Ok(Some(sibling));
    }
    // 仅当前用户真实 WindowsApps 中的独立 jchocr 可读取安装器登记。
    // 解析目录身份，不能让任意同名目录或目录链接扩大入口范围。
    let is_command = executable
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("jchocr.exe"));
    let is_windows_apps = is_command
        && windows_apps.is_some_and(|directory| {
            match (parent.canonicalize(), directory.canonicalize()) {
                (Ok(actual), Ok(expected)) => actual == expected,
                _ => false,
            }
        });
    if !is_windows_apps {
        return Ok(None);
    }
    if let Some(installed) = installed_directory()? {
        let bundled = installed.join("xberg");
        if valid_runtime_directory(&bundled)? {
            return Ok(Some(bundled));
        }
    }
    Ok(None)
}

#[cfg(windows)]
fn installed_directory() -> Result<Option<PathBuf>, String> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
    };
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY,
    };
    let key: Vec<u16> = "Software\\JchTools".encode_utf16().chain(Some(0)).collect();
    let value: Vec<u16> = "InstallDir".encode_utf16().chain(Some(0)).collect();
    let flags = RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY;
    let mut bytes = 0;
    // SAFETY: 键名和值名均以 NUL 结尾，第一次调用只读取所需缓冲区长度。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut bytes,
        )
    };
    if matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        return Err(format!(
            "读取 JchTools 安装目录注册表失败：系统错误 {status}"
        ));
    }
    if bytes == 0 || bytes % 2 != 0 {
        return Err("JchTools 安装目录注册表字符串无效".into());
    }
    let mut buffer = vec![0_u16; bytes as usize / 2];
    // SAFETY: 缓冲区按注册表返回的字节数分配，API 不会写出该容量。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            flags,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &raw mut bytes,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "读取 JchTools 安装目录注册表失败：系统错误 {status}"
        ));
    }
    buffer.truncate(bytes as usize / 2);
    while buffer.last() == Some(&0) {
        buffer.pop();
    }
    if buffer.is_empty() || buffer.contains(&0) {
        return Err("JchTools 安装目录注册表字符串无效".into());
    }
    let path = PathBuf::from(OsString::from_wide(&buffer));
    if !path.is_absolute() {
        return Err("JchTools 安装目录注册表不是绝对路径".into());
    }
    Ok(Some(path))
}

#[cfg(not(windows))]
fn installed_directory() -> Result<Option<PathBuf>, String> {
    Ok(None)
}

/// 优先发现便携/安装目录内的运行时；WindowsApps CLI 再读取安装器登记的目录。
pub fn bundled_directory() -> Result<Option<PathBuf>, String> {
    if let BundleOverride::Isolated(isolated) =
        test_bundle_override(cfg!(any(test, feature = "test-hooks")), |key| {
            std::env::var_os(key)
        })?
    {
        return match isolated {
            Some(path) if valid_runtime_directory(&path)? => Ok(Some(path)),
            _ => Ok(None),
        };
    }
    let executable =
        std::env::current_exe().map_err(|error| format!("定位当前程序失败：{error}"))?;
    let windows_apps = directories_next::BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().join("Microsoft/WindowsApps"));
    discover_bundle(&executable, windows_apps.as_deref(), installed_directory)
}
