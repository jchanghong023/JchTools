//! 截图服务的状态根定位与 settings.json / launcher.json 持久化。
//! 从 `service.rs` 按内聚职责原样搬移；行为与注释保持不变。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{json, Value};

use super::tray;

pub(super) struct Settings {
    pub(super) hotkey: String,
    pub(super) main_exe: Option<PathBuf>,
    pub(super) warning: Option<String>,
}
impl Settings {
    /// 内存默认值 + 可选警告（加载失败各分支共用同一构造，警告文案逐字保留）。
    fn defaults(root: &Path, warning: Option<&str>) -> Self {
        Self {
            hotkey: "Ctrl+Alt+O".into(),
            main_exe: read_launcher(root),
            warning: warning.map(str::to_string),
        }
    }
    pub(super) fn load(root: &Path) -> Self {
        let started = Instant::now();
        tracing::debug!(
            component = "snap_ocr_service",
            event = "ocr_settings_load_started",
            "开始加载截图设置"
        );
        let path = root.join("settings.json");
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    component = "snap_ocr_service",
                    event = "ocr_settings_defaulted",
                    stage = "read",
                    result = "not_found",
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "截图设置未保存，使用默认设置"
                );
                return Self::defaults(root, None);
            }
            Err(_) => {
                tracing::warn!(
                    component = "snap_ocr_service",
                    event = "ocr_settings_defaulted",
                    stage = "read",
                    error_type = "io",
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "截图设置无法读取，使用内存默认设置"
                );
                return Self::defaults(
                    root,
                    Some("截图设置文件无法读取，正在使用内存默认值；原文件未覆盖"),
                );
            }
        };
        let Ok(value) = serde_json::from_slice::<Value>(&raw) else {
            tracing::warn!(
                component = "snap_ocr_service",
                event = "ocr_settings_defaulted",
                stage = "decode",
                error_type = "invalid_json",
                elapsed_ms = crate::logging::elapsed_ms(started),
                "截图设置损坏，使用内存默认设置"
            );
            return Self::defaults(
                root,
                Some("截图设置文件损坏，正在使用内存默认值；原文件未覆盖"),
            );
        };
        let Some(hotkey) = value.get("hotkey").and_then(Value::as_str) else {
            tracing::warn!(
                component = "snap_ocr_service",
                event = "ocr_settings_defaulted",
                stage = "schema",
                error_type = "missing_hotkey",
                elapsed_ms = crate::logging::elapsed_ms(started),
                "截图设置格式无效，使用内存默认设置"
            );
            return Self::defaults(root, Some("截图设置格式无效，原文件未覆盖"));
        };
        if tray::parse_hotkey(hotkey).is_err() {
            tracing::warn!(
                component = "snap_ocr_service",
                event = "ocr_settings_defaulted",
                stage = "hotkey",
                error_type = "invalid_hotkey",
                elapsed_ms = crate::logging::elapsed_ms(started),
                "截图热键配置无效，使用内存默认设置"
            );
            return Self::defaults(root, Some("保存的截图热键无效，原文件未覆盖"));
        }
        tracing::debug!(
            component = "snap_ocr_service",
            event = "ocr_settings_load_completed",
            elapsed_ms = crate::logging::elapsed_ms(started),
            result = "loaded",
            "截图设置已加载"
        );
        Self {
            hotkey: hotkey.to_string(),
            main_exe: read_launcher(root),
            warning: None,
        }
    }
    pub(super) fn save(&mut self, root: &Path) -> Result<(), String> {
        fs::create_dir_all(root).map_err(|_| "无法创建截图设置目录".to_string())?;
        let target = root.join("settings.json");
        let staged = root.join("settings.json.new");
        let bytes = serde_json::to_vec(&json!({"hotkey":self.hotkey}))
            .map_err(|_| "无法编码截图设置".to_string())?;
        fs::write(&staged, bytes).map_err(|_| "无法保存截图设置".to_string())?;
        if fs::rename(&staged, &target).is_err() {
            // Windows rename 不覆盖已有文件；先原子替换目标文件。
            if replace_file(&staged, &target).is_err() {
                return Err("无法更新截图设置文件".into());
            }
        }
        self.warning = None;
        Ok(())
    }
}

/// 最近一次主程序路径仅供托盘「初始化」唤起 GUI；独立于用户设置。
/// 设置损坏时不覆盖原文件，单独保存此启动指针。
pub(super) fn read_launcher(root: &Path) -> Option<PathBuf> {
    let raw = fs::read_to_string(root.join("launcher.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("main_exe")?
        .as_str()
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
}
pub(super) fn save_launcher(root: &Path, path: &Path) -> Result<(), String> {
    fs::create_dir_all(root).map_err(|_| "无法更新主程序路径".to_string())?;
    let target = root.join("launcher.json");
    let staged = root.join("launcher.json.new");
    let bytes = serde_json::to_vec(&json!({"main_exe":path.display().to_string()}))
        .map_err(|_| "无法编码主程序路径".to_string())?;
    fs::write(&staged, bytes).map_err(|_| "无法更新主程序路径".to_string())?;
    if fs::rename(&staged, &target).is_err() {
        replace_file(&staged, &target)?;
    }
    Ok(())
}
#[link(name = "kernel32")]
extern "system" {
    fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
}
fn replace_file(from: &Path, to: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    let source: Vec<u16> = from
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let target: Vec<u16> = to
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: 两个宽字符串带 NUL；REPLACE_EXISTING 同卷原子替换，不暴露半份设置。
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 1) } == 0 {
        return Err("设置文件替换失败".into());
    }
    Ok(())
}

pub(super) fn root() -> Result<PathBuf, String> {
    if cfg!(any(test, feature = "test-hooks")) {
        if let Some(path) = std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
        {
            return Ok(path);
        }
    }
    if std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("JchTools.exe")))
        .is_some_and(|p| p.is_file())
    {
        return crate::xberg_settings::state_dir().map(|p| p.join("snap-ocr"));
    }
    // 主程序由 ProjectDirs 决定用户状态目录，不能在 worker 中重新拼 LOCALAPPDATA：
    // Windows 上它实际位于 JchTools/data/snap-ocr。worker 总是从已校验的
    // <资产根>/worker/<版本>/snap-ocr-worker.exe 启动，因此由自身路径回溯。
    let executable = std::env::current_exe().map_err(|_| "无法定位截图服务程序".to_string())?;
    let version_dir = executable
        .parent()
        .ok_or_else(|| "截图服务安装路径无效".to_string())?;
    let worker_dir = version_dir
        .parent()
        .filter(|path| path.file_name() == Some(std::ffi::OsStr::new("worker")))
        .ok_or_else(|| "截图服务未安装在资产目录内".to_string())?;
    worker_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "截图服务资产根目录无效".to_string())
}
