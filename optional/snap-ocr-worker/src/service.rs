//! 当前用户会话后台 OCR 服务：管道/托盘/热键独立于 JchTools 主窗口。
//! 仅资产与显式设置落盘；截图、裁剪与识别结果只在内存与剪贴板。

#![cfg(windows)]

mod protocol;
mod tray;

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use slint::ComponentHandle;
use snap_ocr_core::layout::build_layout;
use snap_ocr_core::pipeline::{
    assemble_spans, detect_candidates, recognize_records, OcrBackend, PipelineError,
};

use crate::image_ops::{self, BgrImage};
use crate::pipeline_backend::{build_record, WorkerOcrBackend};
use crate::result_window::{ProgressWindow, ResultWindowHandle, SettingsWindow};

pub(crate) enum Command {
    Pipe(Value, mpsc::SyncSender<Value>),
    CaptureRequested,
    Image(BgrImage, (i32, i32, i32, i32)),
    CancelledSelection,
    CaptureFailed(String),
    ModelLoaded(Result<(), String>),
    OcrFinished(Result<Option<String>, PipelineError>, (i32, i32, i32, i32)),
    WorkerStopped,
    OpenSettings,
    InitializeAssets,
    RetryModel,
    CancelRecognition,
    CloseSettings,
    ForceExitRequested,
    ToggleAutostart,
    ExitRequested,
}

enum Work {
    Load,
    Recognize(BgrImage, (i32, i32, i32, i32)),
    Stop,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModelState {
    Uninitialized,
    Loading,
    Ready,
    Error,
}
impl ModelState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Uninitialized => "uninitialized",
            Self::Loading => "loading",
            Self::Ready => "ready",
            Self::Error => "error",
        }
    }
}

struct Settings {
    hotkey: String,
    main_exe: Option<PathBuf>,
    warning: Option<String>,
}
impl Settings {
    fn load(root: &Path) -> Self {
        let defaults = || Self {
            hotkey: "Ctrl+Alt+O".into(),
            main_exe: read_launcher(root),
            warning: None,
        };
        let path = root.join("settings.json");
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return defaults(),
            Err(_) => {
                let mut settings = defaults();
                settings.warning =
                    Some("截图设置文件无法读取，正在使用内存默认值；原文件未覆盖".into());
                return settings;
            }
        };
        let Ok(value) = serde_json::from_slice::<Value>(&raw) else {
            let mut settings = defaults();
            settings.warning = Some("截图设置文件损坏，正在使用内存默认值；原文件未覆盖".into());
            return settings;
        };
        let Some(hotkey) = value.get("hotkey").and_then(Value::as_str) else {
            let mut settings = defaults();
            settings.warning = Some("截图设置格式无效，原文件未覆盖".into());
            return settings;
        };
        if tray::parse_hotkey(hotkey).is_err() {
            let mut settings = defaults();
            settings.warning = Some("保存的截图热键无效，原文件未覆盖".into());
            return settings;
        }
        Self {
            hotkey: hotkey.to_string(),
            main_exe: read_launcher(root),
            warning: None,
        }
    }
    fn save(&mut self, root: &Path) -> Result<(), String> {
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
fn read_launcher(root: &Path) -> Option<PathBuf> {
    let raw = fs::read_to_string(root.join("launcher.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("main_exe")?
        .as_str()
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
}
fn save_launcher(root: &Path, path: &Path) -> Result<(), String> {
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

fn root() -> Result<PathBuf, String> {
    if cfg!(debug_assertions) {
        if let Some(path) = std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
        {
            return Ok(path);
        }
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

fn verified(root: &Path, relative: &str, size: u64, expected: &str) -> Result<PathBuf, String> {
    let path = root.join(relative);
    let mut file = File::open(&path).map_err(|_| format!("资产 {relative} 未安装"))?;
    if file
        .metadata()
        .map_err(|_| "资产元数据不可读".to_string())?
        .len()
        != size
    {
        return Err(format!("资产 {relative} 尺寸不匹配"));
    }
    let mut digest = Sha256::new();
    let mut block = vec![0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut block)
            .map_err(|_| "资产读取失败".to_string())?;
        if count == 0 {
            break;
        }
        digest.update(&block[..count]);
    }
    if format!("{:x}", digest.finalize()) != expected {
        return Err(format!("资产 {relative} 校验失败"));
    }
    Ok(path)
}

fn load_backend(root: &Path) -> Result<WorkerOcrBackend, String> {
    let det = verified(
        root,
        "models/PP-OCRv6_small_det/inference.onnx",
        9_891_707,
        "3914f972d833af87d23bb2338bd09238f978a48f3c4dbb8e1a4ee26a93869940",
    )?;
    let rec = verified(
        root,
        "models/PP-OCRv6_small_rec/inference.onnx",
        21_148_338,
        "3e3def686ac9a1676b59bc9749ad896263d8f68b53f352060774de359a2e23ed",
    )?;
    let dict = verified(
        root,
        "models/PP-OCRv6_small_rec/dict.txt",
        74947,
        "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d",
    )?;
    verified(
        root,
        "fonts/NotoSansMonoCJKsc-Regular.otf",
        16_393_784,
        "ec04cc376b34887cedbdf84074e2e226ed2761eeabdcb9173fc1dd7bfd153ef7",
    )?;
    let dll = verified(
        root,
        "worker/v0.1.1/onnxruntime.dll",
        15_809_848,
        "18370c375f07357fa5874344a9d9ac17e6b6fe1eb18b1dd209d79483b4470257",
    )?;
    std::env::set_var("ORT_DYLIB_PATH", dll);
    let backend = WorkerOcrBackend::load_for_service(&det, &rec, &dict, 10)
        .map_err(|_| "推理模型加载失败".to_string())?;
    // 验证两条模型会话已能执行一次推理，而不是仅检查 ONNX 文件存在。
    let white = BgrImage::from_vec(64, 64, vec![255u8; 64 * 64 * 3])
        .map_err(|_| "预热图像无效".to_string())?;
    backend
        .detect(&white)
        .map_err(|_| "检测模型预热失败".to_string())?;
    let rec_image = BgrImage::from_vec(64, 32, vec![255u8; 64 * 32 * 3])
        .map_err(|_| "预热图像无效".to_string())?;
    backend
        .recognize(&[rec_image])
        .map_err(|_| "识别模型预热失败".to_string())?;
    Ok(backend)
}

fn recognize(
    backend: &WorkerOcrBackend,
    image: &BgrImage,
    cancel: &AtomicBool,
) -> Result<Option<String>, PipelineError> {
    let width = u32::try_from(image.width())
        .map_err(|_| PipelineError::Backend("图像宽度超出检测模型范围".into()))?;
    let height = u32::try_from(image.height())
        .map_err(|_| PipelineError::Backend("图像高度超出检测模型范围".into()))?;
    let candidates = detect_candidates(backend, image, width, height, Some(cancel))?;
    let mut records = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if cancel.load(Ordering::Acquire) {
            return Err(PipelineError::Cancelled);
        }
        if let Ok(crop) = image_ops::warp_perspective_cubic_replicate(image, candidate.quad()) {
            records.push(build_record(candidate, crop));
        }
    }
    recognize_records(backend, &mut records, Some(cancel))?;
    let spans = assemble_spans(&records, Some(cancel))?;
    if spans.is_empty() {
        return Ok(None);
    }
    let text = build_layout(&spans).text;
    if cancel.load(Ordering::Acquire) {
        return Err(PipelineError::Cancelled);
    }
    if text.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

fn worker(
    root: &Path,
    receiver: &mpsc::Receiver<Work>,
    events: &mpsc::Sender<Command>,
    cancel: &AtomicBool,
) {
    let mut model = None;
    while let Ok(work) = receiver.recv() {
        match work {
            Work::Load => {
                drop(model.take());
                let outcome = load_backend(root);
                let status = outcome.as_ref().map(|_| ()).map_err(Clone::clone);
                model = outcome.ok();
                let _ = events.send(Command::ModelLoaded(status));
            }
            Work::Recognize(image, work) => {
                let outcome = if let Some(model) = model.as_ref() {
                    recognize(model, &image, cancel)
                } else {
                    Err(PipelineError::Backend("模型未就绪".into()))
                };
                let _ = events.send(Command::OcrFinished(outcome, work));
            }
            Work::Stop => {
                drop(model.take());
                let _ = events.send(Command::WorkerStopped);
                break;
            }
        }
    }
}

#[link(name = "advapi32")]
extern "system" {
    fn RegCreateKeyExW(
        key: usize,
        path: *const u16,
        reserved: u32,
        class: *mut u16,
        options: u32,
        access: u32,
        security: *const std::ffi::c_void,
        result: *mut usize,
        disposition: *mut u32,
    ) -> i32;
    fn RegOpenKeyExW(
        key: usize,
        path: *const u16,
        options: u32,
        access: u32,
        result: *mut usize,
    ) -> i32;
    fn RegSetValueExW(
        key: usize,
        name: *const u16,
        reserved: u32,
        kind: u32,
        data: *const u8,
        size: u32,
    ) -> i32;
    fn RegDeleteValueW(key: usize, name: *const u16) -> i32;
    fn RegQueryValueExW(
        key: usize,
        name: *const u16,
        reserved: *mut u32,
        kind: *mut u32,
        data: *mut u8,
        size: *mut u32,
    ) -> i32;
    fn RegCloseKey(key: usize) -> i32;
}
// Win32 predefined HKEY_CURRENT_USER is the sign-extended 32-bit pseudo-handle 0x80000001.
const HKEY_CURRENT_USER: usize = (-2_147_483_647_isize).cast_unsigned();
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE: &str = "JchToolsSnapOcr";
fn run_value() -> Result<Option<String>, String> {
    let mut key = 0usize;
    let path = protocol::wide(RUN_KEY);
    let name = protocol::wide(VALUE);
    unsafe {
        let opened = RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, 0x0001, &raw mut key);
        if opened == 2 {
            return Ok(None);
        }
        if opened != 0 {
            return Err("无法读取当前用户开机启动项".into());
        }
        let mut ty = 0u32;
        let mut size = 0u32;
        let code = RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &raw mut ty,
            std::ptr::null_mut(),
            &raw mut size,
        );
        if code == 2 {
            RegCloseKey(key);
            return Ok(None);
        }
        if code != 0 || ty != 1 || size > 32768 {
            RegCloseKey(key);
            return Err("开机启动项类型无效".into());
        }
        let mut data = vec![0u8; size as usize];
        let status = RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &raw mut ty,
            data.as_mut_ptr(),
            &raw mut size,
        );
        RegCloseKey(key);
        if status != 0 {
            return Err("无法读取当前用户开机启动项".into());
        }
        let units = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .take_while(|unit| *unit != 0)
            .collect::<Vec<_>>();
        Ok(Some(String::from_utf16_lossy(&units)))
    }
}
fn set_autostart(enabled: bool) -> Result<(), String> {
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
    let mut key = 0usize;
    let path = protocol::wide(RUN_KEY);
    let name = protocol::wide(VALUE);
    // Windows Run 项仅修改本功能的值，不动其他应用。
    unsafe {
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            std::ptr::null_mut(),
            0,
            0x20006,
            std::ptr::null(),
            &raw mut key,
            std::ptr::null_mut(),
        ) != 0
        {
            return Err("无法打开当前用户开机启动项".into());
        }
        let status = if let Some((bytes, length)) = value.as_ref() {
            RegSetValueExW(key, name.as_ptr(), 0, 1, bytes.as_ptr().cast(), *length)
        } else {
            let code = RegDeleteValueW(key, name.as_ptr());
            if code == 2 {
                0
            } else {
                code
            }
        };
        RegCloseKey(key);
        if status != 0 {
            return Err("开机启动设置更新失败".into());
        }
        Ok(())
    }
}
fn autostart_enabled() -> bool {
    run_value().ok().flatten().is_some()
}

struct Service {
    root: PathBuf,
    commands: mpsc::Sender<Command>,
    self_weak: std::rc::Weak<std::cell::RefCell<Service>>,
    settings: Settings,
    tray: tray::TrayHandle,
    work: mpsc::Sender<Work>,
    cancel: Arc<AtomicBool>,
    model: ModelState,
    model_error: Option<String>,
    pending_image: Option<(BgrImage, (i32, i32, i32, i32))>,
    busy: bool,
    result: Option<ResultWindowHandle>,
    old_result_visible: bool,
    progress: Option<ProgressWindow>,
    settings_window: Option<SettingsWindow>,
    exit_pending: Option<Instant>,
    exit_stop_sent: bool,
}
impl Service {
    fn status(&self) -> Value {
        json!({"ok":true,"model":self.model.as_str(),"task":if self.busy {"recognizing"} else {"idle"},
            "hotkey":self.settings.hotkey,"autostart":autostart_enabled(),"error":self.model_error.as_ref().or(self.settings.warning.as_ref())})
    }
    fn hide_old(&mut self) {
        self.old_result_visible = self
            .result
            .as_ref()
            .is_some_and(ResultWindowHandle::is_visible);
        if self.old_result_visible {
            if let Some(result) = &self.result {
                if let Some(window) = result.as_weak().upgrade() {
                    let _ = window.hide();
                }
            }
        }
    }
    fn restore_old(&mut self) {
        if self.old_result_visible {
            if let Some(result) = &self.result {
                if let Some(window) = result.as_weak().upgrade() {
                    let _ = window.show();
                }
            }
            self.old_result_visible = false;
        }
    }
    fn show_progress(&mut self, stage: &str) -> Result<(), String> {
        if self.progress.is_none() {
            let window = ProgressWindow::new().map_err(|_| "识别进度窗口创建失败".to_string())?;
            let weak = self.self_weak.clone();
            window.on_cancel_requested(move || {
                if let Some(service) = weak.upgrade() {
                    service.borrow_mut().cancel_recognition();
                }
            });
            let weak = self.self_weak.clone();
            window.window().on_close_requested(move || {
                if let Some(service) = weak.upgrade() {
                    service.borrow_mut().cancel_recognition();
                }
                slint::CloseRequestResponse::KeepWindowShown
            });
            self.progress = Some(window);
        }
        let window = self
            .progress
            .as_ref()
            .ok_or_else(|| "识别进度窗口不可用".to_string())?;
        window.set_stage(stage.into());
        window.set_cancelling(false);
        window
            .show()
            .map_err(|_| "识别进度窗口显示失败".to_string())
    }
    fn hide_progress(&self) {
        if let Some(window) = self.progress.as_ref() {
            let _ = window.hide();
        }
    }
    fn cancel_recognition(&mut self) {
        if !self.busy {
            return;
        }
        self.cancel.store(true, Ordering::Release);
        if self.pending_image.take().is_some() {
            self.busy = false;
            self.hide_progress();
            self.restore_old();
            self.tray.notice("已取消识别");
        } else if let Some(window) = self.progress.as_ref() {
            window.set_cancelling(true);
        }
    }
    fn open_settings(&mut self) {
        if let Some(window) = self.settings_window.as_ref() {
            if !window.window().is_visible() {
                window.set_draft_key("".into());
                window.set_autostart_draft(window.get_autostart_enabled());
            }
            let _ = window.show();
            return;
        }
        let Ok(window) = SettingsWindow::new() else {
            self.tray.notice("截图设置窗口创建失败");
            return;
        };
        let weak = self.self_weak.clone();
        window.on_save_settings(move |hotkey, enabled| {
            let Some(service) = weak.upgrade() else {
                return false;
            };
            let mut service = service.borrow_mut();
            match service.save_settings(&hotkey, enabled) {
                Ok(()) => true,
                Err(reason) => {
                    service.tray.notice(reason);
                    false
                }
            }
        });
        let sender = self.commands.clone();
        window.on_retry_model(move || {
            let _ = sender.send(Command::RetryModel);
        });
        let sender = self.commands.clone();
        window.on_initialize_assets(move || {
            let _ = sender.send(Command::InitializeAssets);
        });
        let sender = self.commands.clone();
        window.on_cancel_recognition(move || {
            let _ = sender.send(Command::CancelRecognition);
        });
        let sender = self.commands.clone();
        window.on_exit_service(move || {
            let _ = sender.send(Command::ExitRequested);
        });
        let sender = self.commands.clone();
        window.on_force_exit_service(move || {
            let _ = sender.send(Command::ForceExitRequested);
        });
        let sender = self.commands.clone();
        window.on_close_requested(move || {
            let _ = sender.send(Command::CloseSettings);
        });
        let sender = self.commands.clone();
        window.window().on_close_requested(move || {
            let _ = sender.send(Command::CloseSettings);
            slint::CloseRequestResponse::KeepWindowShown
        });
        self.settings_window = Some(window);
        self.refresh_settings();
        if let Some(window) = self.settings_window.as_ref() {
            let _ = window.show();
        }
    }
    fn refresh_settings(&self) {
        if let Some(window) = self.settings_window.as_ref() {
            window.set_hotkey_label(self.settings.hotkey.as_str().into());
            window.set_model_status(
                match self.model {
                    ModelState::Uninitialized => "资产尚未初始化",
                    ModelState::Loading => "正在加载并预热模型…",
                    ModelState::Ready => "模型已就绪",
                    ModelState::Error => "模型加载失败",
                }
                .into(),
            );
            let enabled = autostart_enabled();
            if window.get_autostart_draft() == window.get_autostart_enabled() {
                window.set_autostart_draft(enabled);
            }
            window.set_autostart_enabled(enabled);
            window.set_recognizing(self.busy);
            window.set_model_loading(self.model == ModelState::Loading);
            window.set_needs_initialization(matches!(
                self.model,
                ModelState::Uninitialized | ModelState::Error
            ));
            window.set_exit_waiting(self.exit_pending.is_some());
            window.set_force_exit_available(
                self.exit_pending
                    .is_some_and(|at| at.elapsed() >= Duration::from_secs(10)),
            );
            let status = match (
                self.settings.warning.as_deref(),
                self.model_error.as_deref(),
            ) {
                (Some(warning), Some(model_error)) => format!("{warning}；{model_error}"),
                (Some(warning), None) => warning.to_owned(),
                (None, Some(model_error)) => model_error.to_owned(),
                (None, None) => String::new(),
            };
            window.set_status_message(status.into());
        }
    }
    fn retry(&mut self) {
        if self.exit_pending.is_some()
            || self.model == ModelState::Loading
            || self.model == ModelState::Ready
        {
            return;
        }
        self.model = ModelState::Loading;
        self.model_error = None;
        if self.work.send(Work::Load).is_err() {
            self.model = ModelState::Error;
            self.model_error = Some("推理线程已退出，请重新启动截图服务".into());
        }
        self.refresh_settings();
    }
    fn rollback_settings(
        &mut self,
        old_hotkey: &str,
        new_hotkey: &str,
        hotkey_changed: bool,
        old_autostart: Option<bool>,
        reason: String,
    ) -> String {
        let mut detail = reason;
        if let Some(enabled) = old_autostart {
            if set_autostart(enabled).is_err() {
                detail.push_str("；开机启动未能恢复，请检查当前开关状态");
            }
        }
        if hotkey_changed && self.tray.replace(old_hotkey.to_owned()).is_err() {
            new_hotkey.clone_into(&mut self.settings.hotkey);
            let _ = write!(
                detail,
                "；原热键已无法恢复，当前仍使用 {new_hotkey}；请重新保存设置"
            );
        } else {
            old_hotkey.clone_into(&mut self.settings.hotkey);
        }
        self.settings.warning = Some(detail.clone());
        self.refresh_settings();
        detail
    }
    fn save_settings(&mut self, hotkey: &str, enabled: bool) -> Result<(), String> {
        if self.busy || self.exit_pending.is_some() {
            return Err("请等待当前截图任务结束后保存设置".into());
        }
        tray::parse_hotkey(hotkey)?;
        let old_hotkey = self.settings.hotkey.clone();
        let old_autostart = run_value()?.is_some();
        let hotkey_changed = old_hotkey != hotkey;
        let autostart_changed = old_autostart != enabled;
        if hotkey_changed {
            self.tray.replace(hotkey.to_owned())?;
            hotkey.clone_into(&mut self.settings.hotkey);
        }
        if autostart_changed {
            if let Err(reason) = set_autostart(enabled) {
                return Err(self.rollback_settings(
                    &old_hotkey,
                    hotkey,
                    hotkey_changed,
                    Some(old_autostart),
                    reason,
                ));
            }
        }
        if let Err(reason) = self.settings.save(&self.root) {
            return Err(self.rollback_settings(
                &old_hotkey,
                hotkey,
                hotkey_changed,
                autostart_changed.then_some(old_autostart),
                reason,
            ));
        }
        self.refresh_settings();
        Ok(())
    }
    fn attach_main_exe(&mut self, raw: &str) -> Result<(), String> {
        let path = PathBuf::from(raw);
        if !path.is_absolute()
            || !path.is_file()
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("JchTools.exe"))
        {
            return Err("主程序路径无效".into());
        }
        save_launcher(&self.root, &path)?;
        self.settings.main_exe = Some(path);
        Ok(())
    }
    fn request_capture(&mut self) {
        if self.exit_pending.is_some() {
            self.tray.notice("截图服务正在退出");
            return;
        }
        if self.busy {
            self.tray.notice("正在识别");
            return;
        }
        self.cancel.store(false, Ordering::Release);
        self.busy = true;
        self.hide_old();
        self.tray.trigger();
        self.refresh_settings();
    }
    fn request_exit(&mut self) {
        if self.exit_pending.is_some() {
            return;
        }
        self.cancel_recognition();
        self.exit_pending = Some(Instant::now());
        if self.busy || self.model == ModelState::Loading {
            self.tray
                .notice("正在等待当前推理调用结束；超过 10 秒可在设置中选择强制退出");
        } else {
            self.stop_worker();
        }
    }
    fn stop_worker(&mut self) {
        if self.exit_stop_sent {
            return;
        }
        self.exit_stop_sent = true;
        if self.work.send(Work::Stop).is_err() {
            self.complete_exit();
        }
    }
    fn complete_exit(&mut self) {
        self.tray.stop();
        if let Some(result) = self.result.take() {
            result.hide_and_clear();
        }
        self.hide_progress();
        let _ = slint::quit_event_loop();
    }
    fn pipe(&mut self, request: &Value) -> Value {
        let action = request.get("command").and_then(Value::as_str).unwrap_or("");
        let outcome = match action {
            "ping" | "get-state" => Ok(()),
            "open-settings" => {
                self.open_settings();
                Ok(())
            }
            "save-settings" => match (
                request.get("hotkey").and_then(Value::as_str),
                request.get("autostart").and_then(Value::as_bool),
            ) {
                (Some(hotkey), Some(enabled)) => self.save_settings(hotkey, enabled),
                _ => Err("缺少有效快捷键或开机启动开关".into()),
            },
            "attach-main-exe" => request
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "缺少主程序路径".to_string())
                .and_then(|path| self.attach_main_exe(path)),
            "retry-load" => {
                self.retry();
                Ok(())
            }
            "capture" => {
                self.request_capture();
                Ok(())
            }
            "cancel" => {
                self.cancel_recognition();
                Ok(())
            }
            "shutdown" => {
                self.request_exit();
                Ok(())
            }
            _ => Err("未知的截图服务命令".into()),
        };
        self.refresh_settings();
        match outcome {
            Ok(()) => self.status(),
            Err(reason) => json!({"ok":false,"error":reason}),
        }
    }
    fn handle(&mut self, command: Command) {
        match command {
            Command::Pipe(request, response) => {
                let _ = response.send(self.pipe(&request));
            }
            Command::CaptureRequested => self.request_capture(),
            Command::Image(image, work) => {
                if self.exit_pending.is_some() {
                    self.busy = false;
                    self.restore_old();
                } else {
                    match self.model {
                        ModelState::Loading => {
                            if let Err(reason) = self.show_progress("等待模型就绪…") {
                                self.busy = false;
                                self.restore_old();
                                self.tray.notice(reason);
                            } else {
                                self.pending_image = Some((image, work));
                            }
                        }
                        ModelState::Ready => {
                            self.cancel.store(false, Ordering::Release);
                            if let Err(reason) = self.show_progress("正在识别…") {
                                self.busy = false;
                                self.restore_old();
                                self.tray.notice(reason);
                            } else if self.work.send(Work::Recognize(image, work)).is_err() {
                                self.busy = false;
                                self.hide_progress();
                                self.restore_old();
                                self.tray.notice("推理进程已退出");
                            }
                        }
                        ModelState::Uninitialized | ModelState::Error => {
                            self.busy = false;
                            self.restore_old();
                            self.tray.notice("模型未就绪，请初始化或在设置中重试加载");
                        }
                    }
                }
            }
            Command::CancelledSelection => {
                self.busy = false;
                self.hide_progress();
                self.restore_old();
            }
            Command::CaptureFailed(reason) => {
                self.busy = false;
                self.hide_progress();
                self.restore_old();
                self.tray.notice(reason);
            }
            Command::ModelLoaded(outcome) => {
                match outcome {
                    Ok(()) => {
                        if let Err(reason) =
                            crate::result_window::register_fonts(&self.root.join("fonts"))
                        {
                            self.model = ModelState::Error;
                            self.model_error = Some(reason.to_string());
                        } else {
                            self.model = ModelState::Ready;
                            self.model_error = None;
                            if let Some((image, work)) = self.pending_image.take() {
                                if self.cancel.load(Ordering::Acquire) {
                                    self.busy = false;
                                    self.hide_progress();
                                    self.restore_old();
                                } else {
                                    if let Some(window) = self.progress.as_ref() {
                                        window.set_stage("正在识别…".into());
                                    }
                                    if self.work.send(Work::Recognize(image, work)).is_err() {
                                        self.busy = false;
                                        self.hide_progress();
                                        self.restore_old();
                                        self.tray.notice("推理进程已退出");
                                    }
                                }
                            }
                        }
                    }
                    Err(reason) => {
                        self.model = if reason.contains("未安装") {
                            ModelState::Uninitialized
                        } else {
                            ModelState::Error
                        };
                        self.model_error = Some(reason);
                    }
                }
                if self.model != ModelState::Ready && self.pending_image.take().is_some() {
                    self.busy = false;
                    self.hide_progress();
                    self.restore_old();
                    self.tray.notice(format!(
                        "截图组件加载失败：{}",
                        self.model_error.as_deref().unwrap_or("未知错误")
                    ));
                }
            }
            Command::OcrFinished(outcome, work) => {
                self.busy = false;
                self.hide_progress();
                if self.exit_pending.is_some() {
                    self.stop_worker();
                    return;
                }
                match outcome {
                    Ok(Some(text)) => {
                        match ResultWindowHandle::show_for_monitor(&text, work, |text| {
                            copy_text(text)
                        }) {
                            Ok(window) => {
                                if let Some(old) = self.result.take() {
                                    old.hide_and_clear();
                                }
                                self.result = Some(window);
                                self.old_result_visible = false;
                            }
                            Err(reason) => {
                                self.restore_old();
                                self.tray.notice(reason.to_string());
                            }
                        }
                    }
                    Ok(None) => {
                        self.restore_old();
                        self.tray.notice("选区内未识别到文字");
                    }
                    Err(PipelineError::Cancelled) => {
                        self.restore_old();
                        self.tray.notice("已取消识别");
                    }
                    Err(_) => {
                        self.restore_old();
                        self.tray.notice("OCR 推理失败，请重试截图");
                    }
                }
            }
            Command::OpenSettings => self.open_settings(),
            Command::InitializeAssets => {
                if let Some(exe) = self.settings.main_exe.as_ref().filter(|p| p.is_file()) {
                    if std::process::Command::new(exe)
                        .arg("--snap-ocr-settings")
                        .spawn()
                        .is_err()
                    {
                        self.tray.notice("无法打开 JchTools 以初始化组件");
                    }
                } else {
                    self.tray
                        .notice("主程序位置已改变；请先打开 JchTools 再初始化组件");
                }
            }
            Command::RetryModel => self.retry(),
            Command::CancelRecognition => {
                self.cancel_recognition();
            }
            Command::CloseSettings => {
                if let Some(window) = self.settings_window.as_ref() {
                    window.set_draft_key("".into());
                    window.set_autostart_draft(window.get_autostart_enabled());
                    let _ = window.hide();
                }
            }
            Command::ForceExitRequested => {
                if self
                    .exit_pending
                    .is_some_and(|at| at.elapsed() >= Duration::from_secs(10))
                {
                    // 只有用户显式选择强制退出才终止当前可能仍阻塞的 ONNX 调用。
                    std::process::exit(0);
                }
            }
            Command::ToggleAutostart => {
                if let Err(reason) = set_autostart(!autostart_enabled()) {
                    self.tray.notice(reason);
                }
            }
            Command::ExitRequested => self.request_exit(),
            Command::WorkerStopped => {
                self.complete_exit();
                return;
            }
        }
        if self.exit_pending.is_some() && !self.busy && self.model != ModelState::Loading {
            self.stop_worker();
        }
        self.refresh_settings();
    }
}

#[link(name = "user32")]
extern "system" {
    fn OpenClipboard(hwnd: *mut std::ffi::c_void) -> i32;
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(format: u32, handle: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn CloseClipboard() -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn GlobalAlloc(flags: u32, size: usize) -> *mut std::ffi::c_void;
    fn GlobalLock(handle: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn GlobalUnlock(handle: *mut std::ffi::c_void) -> i32;
    fn GlobalFree(handle: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}
fn copy_text(text: &str) -> Result<(), String> {
    let wide = protocol::wide(text);
    let size = wide
        .len()
        .checked_mul(2)
        .ok_or_else(|| "文本过长无法复制".to_string())?;
    unsafe {
        let handle = GlobalAlloc(2, size);
        if handle.is_null() {
            return Err("剪贴板内存分配失败".into());
        }
        let data = GlobalLock(handle);
        if data.is_null() {
            GlobalFree(handle);
            return Err("剪贴板内存访问失败".into());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), data.cast::<u16>(), wide.len());
        GlobalUnlock(handle);
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            GlobalFree(handle);
            return Err("剪贴板被其他程序占用".into());
        }
        let copied = EmptyClipboard() != 0 && !SetClipboardData(13, handle).is_null();
        CloseClipboard();
        if !copied {
            GlobalFree(handle);
            return Err("复制到剪贴板失败".into());
        }
    }
    Ok(())
}

/// 主窗口关闭不会调用此函数的退出；托盘退出才释放后台常驻模型。
pub fn run_service(autostart: bool) -> Result<(), String> {
    let Some(_instance) = protocol::claim_instance()? else {
        use std::io::Write;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(mut pipe) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(protocol::pipe_name())
            {
                pipe.write_all(b"{\"command\":\"open-settings\"}\n")
                    .map_err(|_| "已有截图服务，但无法打开其设置".to_string())?;
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("已有截图服务，但控制管道不可用".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    crate::capture_win::enable_per_monitor_v2()?;
    let root = root()?;
    let mut settings = Settings::load(&root);
    let args: Vec<String> = std::env::args().collect();
    if let Some(index) = args.iter().position(|arg| arg == "--main-exe") {
        if let Some(path) = args
            .get(index + 1)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_file())
        {
            save_launcher(&root, &path)?;
            settings.main_exe = Some(path);
        }
    }
    // 已启用时更新移动过的 worker 路径，但绝不擅自开启开机启动。
    if autostart_enabled() {
        set_autostart(true)?;
    }
    let (tx, rx) = mpsc::channel::<Command>();
    let (work_tx, work_rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_events = tx.clone();
    let worker_cancel = cancel.clone();
    let worker_root = root.clone();
    std::thread::spawn(move || worker(&worker_root, &work_rx, &worker_events, &worker_cancel));
    let tray = tray::start(tx.clone(), settings.hotkey.clone(), autostart)?;
    let service = Service {
        root,
        settings,
        tray,
        commands: tx.clone(),
        self_weak: std::rc::Weak::new(),
        work: work_tx,
        cancel,
        model: ModelState::Loading,
        model_error: None,
        pending_image: None,
        busy: false,
        result: None,
        old_result_visible: false,
        progress: None,
        settings_window: None,
        exit_pending: None,
        exit_stop_sent: false,
    };
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let ipc = tx.clone();
    std::thread::spawn(move || protocol::serve(&ipc, &ready_tx));
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "截图控制管道启动超时".to_string())??;
    let _ = service.work.send(Work::Load);
    let service = std::rc::Rc::new(std::cell::RefCell::new(service));
    service.borrow_mut().self_weak = std::rc::Rc::downgrade(&service);
    let timer = slint::Timer::default();
    {
        let service = service.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(40),
            move || {
                while let Ok(command) = rx.try_recv() {
                    service.borrow_mut().handle(command);
                }
                let current = service.borrow();
                if current
                    .exit_pending
                    .is_some_and(|at| at.elapsed() >= Duration::from_secs(10))
                {
                    if let Some(window) = current.settings_window.as_ref() {
                        window.set_force_exit_available(true);
                    }
                }
            },
        );
    }
    // 无可见窗时仍继续驻留：只由托盘「退出」显式退出。
    slint::run_event_loop_until_quit().map_err(|_| "截图服务窗口事件循环失败".to_string())
}

#[cfg(test)]
mod tests {
    use super::{tray, Command, ModelState, Service, Settings};
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use std::sync::{mpsc, Arc};

    use snap_ocr_core::pipeline::PipelineError;

    // 覆盖 O-13/O-20/O-30：单次推理错误只结束该任务，已预热的模型仍供下次截图。
    #[test]
    fn inference_failure_does_not_unload_ready_model() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (commands, _) = mpsc::channel();
        let (work, _) = mpsc::channel();
        let mut service = Service {
            root: temp.path().to_path_buf(),
            commands,
            self_weak: std::rc::Weak::new(),
            settings: Settings {
                hotkey: "Ctrl+Alt+O".into(),
                main_exe: None,
                warning: None,
            },
            tray: tray::TrayHandle::for_test(),
            work,
            cancel: Arc::new(AtomicBool::new(false)),
            model: ModelState::Ready,
            model_error: None,
            pending_image: None,
            busy: true,
            result: None,
            old_result_visible: false,
            progress: None,
            settings_window: None,
            exit_pending: None,
            exit_stop_sent: false,
        };
        service.handle(Command::OcrFinished(
            Err(PipelineError::Backend("一次性推理错误".into())),
            (0, 0, 20, 20),
        ));
        assert!(service.model == ModelState::Ready);
        assert!(service.model_error.is_none());
        assert!(!service.busy);
        assert_eq!(service.status()["model"], "ready");
        Ok(())
    }

    // 覆盖 O-15：损坏的设置只进入内存默认值，不被启动或读取过程静默覆盖；
    // 用户明确保存以后才用新设置替换。
    #[test]
    fn corrupt_settings_survive_load_until_explicit_save() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = tempfile::tempdir()?;
        let file = temp.path().join("settings.json");
        fs::write(&file, b"{broken config")?;
        let mut settings = Settings::load(temp.path());
        assert_eq!(settings.hotkey, "Ctrl+Alt+O");
        assert!(settings.warning.is_some());
        assert_eq!(fs::read(&file)?, b"{broken config");
        settings.hotkey = "Ctrl+Shift+PageDown".into();
        settings.save(temp.path())?;
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&file)?)?;
        assert_eq!(value["hotkey"], "Ctrl+Shift+PageDown");
        Ok(())
    }
}
