//! 注册表开机自启动项（HKCU Run）读写与归类（O-15 / S8-05）。
//! 从 `service.rs` 按内聚职责原样搬移；行为与注释保持不变。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::protocol;

use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
    System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_WRITE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    },
};
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
pub(super) const VALUE: &str = "JchToolsSnapOcr";

// 测试自启只写非 Run 子键，避免资产探测和单测更改用户登录启动项。
pub(super) fn autostart_registry_path() -> Result<String, String> {
    if cfg!(any(test, feature = "test-hooks")) {
        if let Some(root) = std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT") {
            let root = PathBuf::from(root);
            if !root.is_absolute() {
                return Err("测试截图资产目录必须为绝对路径".into());
            }
            let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
            return Ok(format!(
                r"Software\JchTools\Tests\SnapOcr\{}",
                hex::encode(digest)
            ));
        }
        if cfg!(test) {
            return Ok(format!(
                r"Software\JchTools\Tests\SnapOcr\unit-{}",
                std::process::id()
            ));
        }
    }
    Ok(RUN_KEY.to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StoredAutostartValue {
    Absent,
    Corrupt,
    Command(String),
}

fn run_value() -> Result<StoredAutostartValue, String> {
    read_autostart_value(&autostart_registry_path()?)
}

pub(super) fn read_autostart_value(registry_path: &str) -> Result<StoredAutostartValue, String> {
    let mut key: HKEY = std::ptr::null_mut();
    let path = protocol::wide(registry_path);
    let name = protocol::wide(VALUE);
    // SAFETY: 预定义键 HKCU 与 NUL 结尾宽字符串路径；key 出参指向本栈变量，由本函数关闭。
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &raw mut key,
        )
    };
    if opened == ERROR_FILE_NOT_FOUND {
        return Ok(StoredAutostartValue::Absent);
    }
    if opened != ERROR_SUCCESS {
        return Err("无法读取当前用户开机启动项".into());
    }
    let mut ty = 0u32;
    let mut size = 0u32;
    // SAFETY: key 为刚打开的 HKCU Run 子键句柄；类型/大小指针指向本栈变量，本次不取数据。
    let code = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &raw mut ty,
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        // SAFETY: key 由上文打开，本分支关闭后不再使用。
        unsafe { RegCloseKey(key) };
        return Ok(StoredAutostartValue::Absent);
    }
    if code != ERROR_SUCCESS {
        // SAFETY: 同上，key 仅在此关闭一次。
        unsafe { RegCloseKey(key) };
        return Err("开机启动项类型无效".into());
    }
    if ty != REG_SZ || size > 32768 {
        // O-15：已存在但类型/大小无效的值只报告损坏，读取不改写，
        // 也不阻止托盘与热键启动；真实注册表查询失败仍由上方报错。
        // SAFETY: key 由上文打开，本分支关闭后不再使用。
        unsafe { RegCloseKey(key) };
        return Ok(StoredAutostartValue::Corrupt);
    }
    let mut data = vec![0u8; size as usize];
    // SAFETY: data 按首查大小分配；写回的 size 不超过分配长度，仍以字节为单位。
    let status = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &raw mut ty,
            data.as_mut_ptr(),
            &raw mut size,
        )
    };
    // SAFETY: 第二次查询结束（无论成败）即关闭 key，与原实现一致。
    unsafe { RegCloseKey(key) };
    if status != ERROR_SUCCESS {
        return Err("无法读取当前用户开机启动项".into());
    }
    let units = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|unit| *unit != 0)
        .collect::<Vec<_>>();
    Ok(StoredAutostartValue::Command(String::from_utf16_lossy(
        &units,
    )))
}
pub(super) fn set_autostart(enabled: bool) -> Result<(), String> {
    set_autostart_value(&autostart_registry_path()?, enabled)
}

pub(super) fn set_autostart_value(registry_path: &str, enabled: bool) -> Result<(), String> {
    let command = if enabled {
        let exe = std::env::current_exe().map_err(|_| "无法定位截图服务程序".to_string())?;
        format!("\"{}\" --service--autostart", exe.display())
    } else {
        String::new()
    };
    if command.contains('\0') {
        return Err("开机启动路径无效".into());
    }
    let value = if enabled {
        let bytes = protocol::wide(&command);
        let length = bytes
            .len()
            .checked_mul(2)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| "开机启动路径过长".to_string())?;
        Some((bytes, length))
    } else {
        None
    };
    let mut key: HKEY = std::ptr::null_mut();
    let path = protocol::wide(registry_path);
    let name = protocol::wide(VALUE);
    // Windows Run 项仅修改本功能的值，不动其他应用。
    // SAFETY: HKCU 预定义键与 NUL 结尾宽字符串；key 出参指向本栈变量，由本函数关闭。
    if unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            std::ptr::null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null(),
            &raw mut key,
            std::ptr::null_mut(),
        )
    } != ERROR_SUCCESS
    {
        return Err("无法打开当前用户开机启动项".into());
    }
    let status = if let Some((bytes, length)) = value.as_ref() {
        // SAFETY: key 为刚创建/打开的句柄；bytes 为 NUL 结尾宽字符串数据，长度按字节计。
        unsafe {
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                bytes.as_ptr().cast(),
                *length,
            )
        }
    } else {
        // SAFETY: key 有效；删除本功能条目，返回 2（值不存在）按成功处理。
        let code = unsafe { RegDeleteValueW(key, name.as_ptr()) };
        if code == ERROR_FILE_NOT_FOUND {
            ERROR_SUCCESS
        } else {
            code
        }
    };
    // SAFETY: 写入/删除已完成，key 关闭后不再使用。
    unsafe { RegCloseKey(key) };
    if status != ERROR_SUCCESS {
        return Err("开机启动设置更新失败".into());
    }
    Ok(())
}
pub(super) fn autostart_enabled() -> bool {
    matches!(
        autostart_value(),
        Ok(AutostartValue::Current | AutostartValue::Stale)
    )
}

/// 开机启动 Run 值的归类（S8-05）：只认「命令行可解析且属于本服务」的值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AutostartValue {
    /// 值不存在：未启用。
    Absent,
    /// 命令行可解析且路径即当前程序：有效启用。
    Current,
    /// 命令行可解析、basename 与当前程序一致，但路径是旧安装位置（移动或
    /// 版本升级后）：视为「已启用但待刷新」，启动时按既有语义刷新为本机
    /// 路径——这是用户此前主动开启的值，不属于损坏。
    Stale,
    /// 值存在但不可解析（空串/无引号/缺 `--service--autostart` 参数/指向
    /// 其他程序）：损坏。不得当作已启用，也不得在启动时据此重写——外部
    /// 残留的损坏值曾导致启动时「静默重开自启动」。
    Corrupt,
}

/// 显式保存必须落实开关；损坏的本功能启动项不能仅因「显示关闭」而跳过删除。
pub(super) fn autostart_update_required(previous: AutostartValue, enabled: bool) -> bool {
    previous == AutostartValue::Corrupt
        || matches!(previous, AutostartValue::Current | AutostartValue::Stale) != enabled
}

/// 解析 Run 值命令行里的 EXE 路径：形状必须是 `"绝对路径" --service--autostart`。
fn parse_autostart_command(raw: &str, exe_basename: &str) -> Option<PathBuf> {
    let rest = raw.strip_prefix('"')?;
    let (path, args) = rest.split_once('"')?;
    if args != " --service--autostart" {
        return None;
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return None;
    }
    let matches_basename = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(exe_basename));
    matches_basename.then_some(path)
}

/// Run 原始值 + 当前程序路径 → 归类（纯函数，供单测）。
pub(super) fn classify_autostart(raw: Option<&str>, current_exe: &Path) -> AutostartValue {
    let Some(raw) = raw else {
        return AutostartValue::Absent;
    };
    let Some(base) = current_exe
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return AutostartValue::Corrupt;
    };
    let Some(exe) = parse_autostart_command(raw, &base) else {
        return AutostartValue::Corrupt;
    };
    let same = exe.to_string_lossy().to_ascii_lowercase()
        == current_exe.to_string_lossy().to_ascii_lowercase();
    if same {
        AutostartValue::Current
    } else {
        AutostartValue::Stale
    }
}

pub(super) fn autostart_value() -> Result<AutostartValue, String> {
    let current = std::env::current_exe().map_err(|_| "无法定位截图服务程序".to_string())?;
    Ok(match run_value()? {
        StoredAutostartValue::Absent => AutostartValue::Absent,
        StoredAutostartValue::Corrupt => AutostartValue::Corrupt,
        StoredAutostartValue::Command(command) => classify_autostart(Some(&command), &current),
    })
}
