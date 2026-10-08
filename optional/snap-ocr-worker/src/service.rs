//! 当前用户会话后台 OCR 服务：管道/托盘/热键独立于 JchTools 主窗口。
//! 统一后台随主包交付，识别由共享 Xberg 代理承接（XB-20～XB-25）；
//! 仅资产与显式设置落盘；截图、裁剪与识别结果只在内存与剪贴板。

#![cfg(windows)]

mod protocol;
mod tray;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use slint::ComponentHandle;

use crate::capture_win::BgrImage;
use crate::result_window::{ProgressWindow, ResultWindowHandle, SettingsWindow};
use crate::shared_xberg::{ClientError, SharedXbergClient, SnapshotState};

/// 单次识别错误（O-30 分类：取消 / 超时 / 推理失败 / 子进程退出；消息不含图像内容）。
#[derive(Debug, Clone)]
pub(crate) enum OcrError {
    /// 用户取消：结果窗即刻恢复；共享 Xberg 保持运行，服务随后检查模型状态，
    /// 仍就绪则复用，否则按 O-13 重载。
    Cancelled,
    /// 识别请求超时：仅结束本次请求，连接不可复用；共享 Xberg 不被终止，
    /// 模型降级为错误并保留重试入口，避免自动循环重试挂起请求。
    TimedOut(String),
    /// 共享推理进程已退出：连接死亡、模型不可再复用，须降级为错误并保留重试
    /// 入口（O-13）；与单次 [`OcrError::Backend`] 失败（模型保持就绪）分类处理。
    ProcessExited(String),
    /// 单次推理失败（模型仍可复用）。
    Backend(String),
    /// 共享代理通信失败，当前客户端不可安全复用。
    Communication(String),
    /// 模型或其资产在运行中失效，保留重新加载入口。
    ModelFailure(String),
}

impl std::fmt::Display for OcrError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(formatter, "用户取消识别"),
            // 各失败变体的消息都已是完整用户可读文案（ClientError::Display）。
            Self::TimedOut(message)
            | Self::ProcessExited(message)
            | Self::Backend(message)
            | Self::Communication(message)
            | Self::ModelFailure(message) => {
                write!(formatter, "{message}")
            }
        }
    }
}

/// 加载失败分类（O-13：未初始化与错误分别有对应的界面入口）。
#[derive(Debug, Clone)]
pub(crate) enum LoadFailure {
    /// 推理组件未安装或未配置：需要主界面初始化（或开发期环境变量覆盖）。
    NotConfigured(String),
    /// 组件在位但启动/预热失败：保留「重新加载模型」重试入口，不联网。
    Failed(String),
}

impl LoadFailure {
    fn message(&self) -> &str {
        match self {
            Self::NotConfigured(message) | Self::Failed(message) => message,
        }
    }
}

pub(crate) enum Command {
    Pipe(Value, mpsc::SyncSender<Value>),
    CaptureRequested,
    Image(BgrImage, (i32, i32, i32, i32)),
    CancelledSelection,
    CaptureFailed(String),
    ModelLoaded(Result<(), LoadFailure>),
    OcrFinished(Result<Option<String>, OcrError>, (i32, i32, i32, i32)),
    WorkerStopped,
    WorkerStopFailed(String),
    HotkeyUnavailable(String),
    OpenMain,
    OpenSettings,
    InitializeAssets,
    RetryModel,
    CancelRecognition,
    CloseSettings,
    ForceExitRequested,
    ForceExitFinished(Result<(), String>),
    ToggleAutostart,
    ExitRequested,
    ExitDecision(bool),
    /// 托盘热键替换的迟到回执（S8-01）：保存流程进入待确认后由等待线程派发。
    HotkeyReplaced(Result<(), String>),
    /// 等待线程的有界时限耗尽仍未收到托盘回执（S8-01 自愈路径）。
    HotkeyReceiptTimedOut,
}

/// 等待托盘迟到回执的热键上下文（S8-01）。
enum PendingHotkey {
    /// 主保存流程在等「注册新热键」的回执：Ok 继续完成保存（开机启动 +
    /// 设置文件，与同步路径同一收尾语义）；Err 上报失败（托盘 Replace 的
    /// 失败路径不改托盘状态，旧键仍生效，无需回滚）。
    Saving {
        hotkey: String,
        old_hotkey: String,
        old_autostart: bool,
        autostart: bool,
    },
    /// 回滚路径在等「恢复旧热键」的回执：Ok 即完成；Err 把内存值纠正回
    /// 实际仍生效的新键。
    Restoring { new_hotkey: String },
}

enum Work {
    Load,
    Recognize(BgrImage, (i32, i32, i32, i32)),
    Stop,
}

const WORKER_STOP_ATTEMPTS: usize = 3;

fn stop_background_with_retry<F>(mut control: F) -> Result<(), String>
where
    F: FnMut() -> Result<Value, String>,
{
    let mut last_error = None;
    for attempt in 0..WORKER_STOP_ATTEMPTS {
        match control() {
            Ok(state) if state["running"] == false => return Ok(()),
            Ok(_) => last_error = Some("共享后台仍在运行".to_string()),
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < WORKER_STOP_ATTEMPTS {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    Err(last_error.unwrap_or_else(|| "共享后台停止未完成".to_string()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// 内存默认值 + 可选警告（加载失败各分支共用同一构造，警告文案逐字保留）。
    fn defaults(root: &Path, warning: Option<&str>) -> Self {
        Self {
            hotkey: "Ctrl+Alt+O".into(),
            main_exe: read_launcher(root),
            warning: warning.map(str::to_string),
        }
    }
    fn load(root: &Path) -> Self {
        let path = root.join("settings.json");
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::defaults(root, None);
            }
            Err(_) => {
                return Self::defaults(
                    root,
                    Some("截图设置文件无法读取，正在使用内存默认值；原文件未覆盖"),
                );
            }
        };
        let Ok(value) = serde_json::from_slice::<Value>(&raw) else {
            return Self::defaults(
                root,
                Some("截图设置文件损坏，正在使用内存默认值；原文件未覆盖"),
            );
        };
        let Some(hotkey) = value.get("hotkey").and_then(Value::as_str) else {
            return Self::defaults(root, Some("截图设置格式无效，原文件未覆盖"));
        };
        if tray::parse_hotkey(hotkey).is_err() {
            return Self::defaults(root, Some("保存的截图热键无效，原文件未覆盖"));
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

/// 截图服务使用应用级 SQLite 保存的共享 Xberg 目录，不提供环境变量覆盖。
fn xberg_component_dir() -> Result<PathBuf, LoadFailure> {
    crate::xberg_settings::required().map_err(LoadFailure::NotConfigured)
}

/// 组件在位校验（XB-09：运行时只检查场景所需成员是否存在，不校验摘要）：
/// `xberg.exe` + `models/snapshot-ocr` 三个模型文件 + `onnxruntime.dll`。
fn verify_component(dir: &Path) -> Result<(), LoadFailure> {
    let required = [
        dir.join("xberg.exe"),
        dir.join("models").join("snapshot-ocr").join("det.onnx"),
        dir.join("models").join("snapshot-ocr").join("rec.onnx"),
        dir.join("models")
            .join("snapshot-ocr")
            .join("dict")
            .join("dict.txt"),
        dir.join("onnxruntime.dll"),
    ];
    for path in &required {
        if !path.is_file() {
            return Err(LoadFailure::Failed(format!(
                "推理组件不完整：缺少 {}",
                path.strip_prefix(dir).unwrap_or(path).display()
            )));
        }
    }
    Ok(())
}

/// 共享运行时的缺失资产错误会附带用户提供的 Xberg 目录；服务提示不暴露该路径。
fn redact_user_path(message: &str, directory: &Path) -> String {
    let path = directory.display().to_string();
    if path.is_empty() {
        message.to_owned()
    } else {
        message.replace(&path, "共享推理目录")
    }
}

const SNAP_ASSET_MANIFEST: &str = include_str!("../../../resources/snap-ocr-assets.json");
const SNAP_FONT_MEMBER: &str = "fonts/NotoSansMonoCJKsc-Regular.otf";
const SNAP_FONT_LICENSE: &str = "fonts/LICENSE-noto-ofl.txt";

fn verify_manifest_file(
    bytes: &[u8],
    size: u64,
    expected_sha256: &str,
    label: &str,
) -> Result<(), LoadFailure> {
    if bytes.len() as u64 != size {
        return Err(LoadFailure::Failed(format!("{label}大小校验失败")));
    }
    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(LoadFailure::Failed(format!("{label}清单摘要无效")));
    }
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(LoadFailure::Failed(format!("{label}摘要校验失败")));
    }
    Ok(())
}

fn verify_font_assets(_root: &Path) -> Result<(), LoadFailure> {
    let manifest: Value = serde_json::from_str(SNAP_ASSET_MANIFEST)
        .map_err(|_| LoadFailure::Failed("截图字体清单损坏".into()))?;
    let assets = manifest["assets"]
        .as_array()
        .ok_or_else(|| LoadFailure::Failed("截图字体清单缺少资产".into()))?;
    let font = assets
        .iter()
        .find(|asset| asset["id"] == "noto-sans-mono-cjk-sc")
        .ok_or_else(|| LoadFailure::Failed("截图字体清单缺少字体条目".into()))?;
    if font["license"]["license"] != "OFL-1.1"
        || font["license"]["component"].as_str().is_none()
        || font["license"]["source"].as_str().is_none()
    {
        return Err(LoadFailure::Failed("截图字体许可清单无效".into()));
    }
    let member = font["members"]
        .as_array()
        .and_then(|members| {
            members
                .iter()
                .find(|member| member["install_path"] == SNAP_FONT_MEMBER)
        })
        .ok_or_else(|| LoadFailure::Failed("截图字体清单缺少字体文件".into()))?;
    let size = member["size_bytes"]
        .as_u64()
        .ok_or_else(|| LoadFailure::Failed("截图字体大小清单无效".into()))?;
    let sha256 = member["sha256"]
        .as_str()
        .ok_or_else(|| LoadFailure::Failed("截图字体摘要清单无效".into()))?;
    verify_manifest_file(
        crate::result_window::EMBEDDED_FONT,
        size,
        sha256,
        "截图字体",
    )?;

    let license = assets
        .iter()
        .find(|asset| asset["id"] == "noto-cjk-ofl-license")
        .ok_or_else(|| LoadFailure::Failed("截图字体清单缺少许可文件".into()))?;
    if license["license"]["license"] != "OFL-1.1"
        || license["license"]["component"].as_str().is_none()
        || license["license"]["source"].as_str().is_none()
    {
        return Err(LoadFailure::Failed("截图字体许可条目无效".into()));
    }
    let _license_path = license["install_path"]
        .as_str()
        .filter(|path| *path == SNAP_FONT_LICENSE)
        .ok_or_else(|| LoadFailure::Failed("截图字体许可路径无效".into()))?;
    let license_size = license["size_bytes"]
        .as_u64()
        .ok_or_else(|| LoadFailure::Failed("截图字体许可大小清单无效".into()))?;
    let license_sha256 = license["sha256"]
        .as_str()
        .ok_or_else(|| LoadFailure::Failed("截图字体许可摘要清单无效".into()))?;
    verify_manifest_file(
        include_bytes!("../../../resources/fonts/LICENSE-noto-ofl.txt"),
        license_size,
        license_sha256,
        "截图字体许可文件",
    )
}

/// 连接共享 Xberg 客户端并完成预热（模型懒加载发生在首个识别请求，
/// 预热图触发加载后 `snapshot_state` 才会是 ready，O-13）。
fn start_inference(root: &Path) -> Result<SharedXbergClient, LoadFailure> {
    verify_font_assets(root)?;
    let component_dir = xberg_component_dir()?;
    verify_component(&component_dir)?;
    crate::xberg_runtime::validate_assets(&component_dir, "snapshot")
        .map_err(|error| LoadFailure::Failed(redact_user_path(&error, &component_dir)))?;
    let mut client = SharedXbergClient::connect(&component_dir);
    warm_up(&mut client)?;
    Ok(client)
}

/// 预热失败的分类文案：优先用响应的结构化 `error_kind`（与 Xberg
/// `snapshot_ocr.rs` 的取值全集对齐：`asset_invalid` / `input_invalid` /
/// `no_text` / `cancelled` / `internal`，SNAP-15）——`asset_invalid` 是模型/
/// 资产加载失败（SNAP-05）；真实 Xberg 的错误文本是英文（如 "snapshot model
/// asset missing"），旧实现只按中文子串匹配必然落空。`kind` 缺失（旧版 Xberg
/// 或无 `error_kind` 的失败响应）时回退到既有「模型」子串启发式兼容。
fn warm_up_failure(kind: Option<&str>, message: &str) -> String {
    match kind {
        Some("asset_invalid") => format!("推理模型加载失败：{message}"),
        // kind 缺失时回退中文「模型」子串启发式（兼容旧版 Xberg）。
        None if message.contains("模型") => format!("推理模型加载失败：{message}"),
        // 其余 kind 与不含「模型」的回退场景都按组件预热失败分类。
        Some(_) | None => format!("推理组件预热失败：{message}"),
    }
}

/// 预热：向常驻子进程发一张 1×1 白图，触发 Xberg 侧模型懒加载，并确认通道
/// 状态进入 ready（O-13 预热行为；无文字图片是成功响应）。
fn warm_up(client: &mut SharedXbergClient) -> Result<(), LoadFailure> {
    let white = BgrImage::from_vec(1, 1, vec![255, 255, 255])
        .map_err(|_| LoadFailure::Failed("预热图像无效".into()))?;
    let png = white.png_bytes().map_err(LoadFailure::Failed)?;
    let cancel = AtomicBool::new(false);
    match client.recognize(&png, &cancel) {
        Ok(_) => {}
        Err(ClientError::Backend { message, kind }) => {
            return Err(LoadFailure::Failed(warm_up_failure(
                kind.as_deref(),
                &message,
            )));
        }
        Err(error) => return Err(LoadFailure::Failed(error.to_string())),
    }
    match client.snapshot_state() {
        Ok(SnapshotState::Ready) => Ok(()),
        Ok(SnapshotState::Error(message)) => {
            Err(LoadFailure::Failed(format!("推理模型加载失败：{message}")))
        }
        Ok(state) => Err(LoadFailure::Failed(format!(
            "推理组件未进入就绪状态（{}）",
            state.as_str()
        ))),
        Err(error) => Err(LoadFailure::Failed(error.to_string())),
    }
}

/// ClientError → OcrError 的分类映射：超时是连接级故障，与子进程退出同路径
/// 降级（O-13/XB-08），文案直接给出「已终止挂起进程 + 重试入口」的指引；其余
/// 分类按既有语义透传。
fn ocr_error(error: ClientError) -> OcrError {
    match error {
        ClientError::Cancelled => OcrError::Cancelled,
        ClientError::Timeout => {
            OcrError::TimedOut("当前截图识别超时；共享引擎及其他功能保持运行".into())
        }
        error @ ClientError::ProcessExited(_) => OcrError::ProcessExited(error.to_string()),
        ClientError::Backend {
            message,
            kind: Some(kind),
        } if kind == "asset_invalid" => OcrError::ModelFailure(message),
        ClientError::Backend {
            message,
            kind: Some(kind),
        } if kind == "shared_runtime" => OcrError::Communication(message),
        ClientError::Io(message) => OcrError::Communication(message),
        error => OcrError::Backend(error.to_string()),
    }
}

/// 识别一张裁剪图：内存 PNG 编码后交给 Xberg 子进程，取回布局文本。
fn recognize(
    client: &mut SharedXbergClient,
    image: &BgrImage,
    cancel: &AtomicBool,
) -> Result<Option<String>, OcrError> {
    let png = image.png_bytes().map_err(OcrError::Backend)?;
    match client.recognize(&png, cancel) {
        Ok(Some(text)) if text.trim().is_empty() => Ok(None),
        Ok(text) => Ok(text),
        Err(error) => Err(ocr_error(error)),
    }
}

/// 重载决策（S8-02）：探测结果为「引擎仍就绪」时免重预热直接复用；探测
/// 不可得（未配置/通信失败）或状态非就绪都走完整重载。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReloadPlan {
    /// 引擎报告就绪：校验本服务字体后重建客户端，不重复预热模型。
    Reuse,
    /// 完整加载（资产校验 + 连接 + 预热，O-13）。
    Full,
}
fn reload_plan(probe: Option<&Result<SnapshotState, ClientError>>) -> ReloadPlan {
    match probe {
        Some(Ok(SnapshotState::Ready)) => ReloadPlan::Reuse,
        Some(Ok(_) | Err(_)) | None => ReloadPlan::Full,
    }
}

fn reuse_inference(root: &Path, component_dir: &Path) -> Result<SharedXbergClient, LoadFailure> {
    verify_font_assets(root)?;
    Ok(SharedXbergClient::connect(component_dir))
}

fn worker(
    root: &Path,
    receiver: &mpsc::Receiver<Work>,
    events: &mpsc::Sender<Command>,
    cancel: &AtomicBool,
) {
    let mut client: Option<SharedXbergClient> = None;
    let mut engine_pid = None;
    let mut verified_root = None;
    loop {
        let work = match receiver.recv_timeout(Duration::from_secs(3)) {
            Ok(work) => work,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let alive = (|| {
                    let component = crate::xberg_settings::required()?;
                    if verified_root.as_ref() != Some(&component) {
                        crate::xberg_runtime::validate_assets(&component, "engine")
                            .map_err(|error| redact_user_path(&error, &component))?;
                        verified_root = Some(component.clone());
                    }
                    crate::xberg_runtime::request(
                        &component,
                        json!({"command":"keepalive"}),
                        Duration::from_secs(15),
                        &AtomicBool::new(false),
                    )
                    .and_then(crate::xberg_runtime::checked)
                })();
                match alive {
                    Ok(value) => {
                        // S8-04：保活只比较 jchtools_xberg_pid——broker 的 keepalive
                        // 响应（主包 src/xberg_runtime_windows.rs）仅含 jchtools_xberg_pid
                        // 与 broker 协议字段，没有引擎启动时间或代次编号；Windows 的
                        // PID 复用因此可能掩盖引擎代次变化、漏一次重建预热（后果
                        // 有限：下一次识别因模型未热而变慢）。补齐需要 Xberg/broker
                        // 侧在响应中暴露引擎启动时间或代次，属跨模块配合项，本轮
                        // 不强行实现：以本注释说明边界与后果（SNAP2TEXT 附录 C
                        // 「后台生命周期」不受影响，模型常驻语义由引擎侧保证）。
                        let pid = value["jchtools_xberg_pid"].as_u64();
                        if pid == engine_pid {
                            continue;
                        }
                        engine_pid = pid;
                        Work::Load
                    }
                    Err(error) => {
                        client.take();
                        engine_pid = None;
                        let _ = events.send(Command::ModelLoaded(Err(LoadFailure::Failed(error))));
                        continue;
                    }
                }
            }
        };
        match work {
            Work::Load => {
                // 重试入口（O-13）与取消恢复共用。共享引擎为协作取消（XB-14）：
                // 取消不再终止引擎、模型仍常驻，先探测 snapshot 状态；仍就绪则
                // 免完整重预热直接复用（S8-02——旧实现无条件重载预热是直连子
                // 进程时代「取消即杀进程」的遗留语义），探测不可用或非就绪才走
                // 完整加载（校验 + 连接 + 预热）。
                let probe = crate::xberg_settings::required().ok().map(|dir| {
                    let mut probe_client = SharedXbergClient::connect(&dir);
                    probe_client.snapshot_state()
                });
                match reload_plan(probe.as_ref()) {
                    ReloadPlan::Reuse => {
                        // 探测刚确认已配置；此处失败只可能是竞态，按未配置报错
                        // 走 O-13 的初始化入口。
                        match crate::xberg_settings::required() {
                            Ok(dir) => {
                                let outcome = reuse_inference(root, &dir);
                                let status = outcome.as_ref().map(|_| ()).map_err(Clone::clone);
                                client = outcome.ok();
                                let _ = events.send(Command::ModelLoaded(status));
                            }
                            Err(reason) => {
                                client.take();
                                let _ = events.send(Command::ModelLoaded(Err(
                                    LoadFailure::NotConfigured(reason),
                                )));
                            }
                        }
                    }
                    ReloadPlan::Full => {
                        client.take();
                        let outcome = start_inference(root);
                        let status = outcome.as_ref().map(|_| ()).map_err(Clone::clone);
                        client = outcome.ok();
                        let _ = events.send(Command::ModelLoaded(status));
                    }
                }
            }
            Work::Recognize(image, work) => {
                let outcome = if let Some(model) = client.as_mut() {
                    recognize(model, &image, cancel)
                } else {
                    Err(OcrError::Backend("模型未就绪".into()))
                };
                match &outcome {
                    // XB-17：终态只重置本场景客户端，不结束共享引擎。
                    // 未确认结束的任务仍由代理占用本场景，防止重复提交。
                    Err(
                        OcrError::Cancelled
                        | OcrError::TimedOut(_)
                        | OcrError::ProcessExited(_)
                        | OcrError::Communication(_)
                        | OcrError::ModelFailure(_),
                    ) => {
                        drop(client.take());
                    }
                    Ok(_) | Err(OcrError::Backend(_)) => {}
                }
                let _ = events.send(Command::OcrFinished(outcome, work));
            }
            Work::Stop => {
                drop(client.take());
                // GUI 任务已结束，截图调用已返回；请求代理排空剩余业务响应并退出。
                match stop_background_with_retry(|| crate::xberg_runtime::background_control(true))
                {
                    Ok(()) => {
                        let _ = events.send(Command::WorkerStopped);
                        break;
                    }
                    Err(error) => {
                        let _ = events.send(Command::WorkerStopFailed(error));
                    }
                }
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
    // SAFETY: 预定义键 HKCU 与 NUL 结尾宽字符串路径；key 出参指向本栈变量，由本函数关闭。
    let opened =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, 0x0001, &raw mut key) };
    if opened == 2 {
        return Ok(None);
    }
    if opened != 0 {
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
    if code == 2 {
        // SAFETY: key 由上文打开，本分支关闭后不再使用。
        unsafe { RegCloseKey(key) };
        return Ok(None);
    }
    if code != 0 || ty != 1 || size > 32768 {
        // SAFETY: 同上，key 仅在此关闭一次。
        unsafe { RegCloseKey(key) };
        return Err("开机启动项类型无效".into());
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
    // SAFETY: HKCU 预定义键与 NUL 结尾宽字符串；key 出参指向本栈变量，由本函数关闭。
    if unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            std::ptr::null_mut(),
            0,
            0x20006,
            std::ptr::null(),
            &raw mut key,
            std::ptr::null_mut(),
        )
    } != 0
    {
        return Err("无法打开当前用户开机启动项".into());
    }
    let status = if let Some((bytes, length)) = value.as_ref() {
        // SAFETY: key 为刚创建/打开的句柄；bytes 为 NUL 结尾宽字符串数据，长度按字节计。
        unsafe { RegSetValueExW(key, name.as_ptr(), 0, 1, bytes.as_ptr().cast(), *length) }
    } else {
        // SAFETY: key 有效；删除本功能条目，返回 2（值不存在）按成功处理。
        let code = unsafe { RegDeleteValueW(key, name.as_ptr()) };
        if code == 2 {
            0
        } else {
            code
        }
    };
    // SAFETY: 写入/删除已完成，key 关闭后不再使用。
    unsafe { RegCloseKey(key) };
    if status != 0 {
        return Err("开机启动设置更新失败".into());
    }
    Ok(())
}
fn autostart_enabled() -> bool {
    matches!(
        autostart_value(),
        Ok(AutostartValue::Current | AutostartValue::Stale)
    )
}

/// 开机启动 Run 值的归类（S8-05）：只认「命令行可解析且属于本服务」的值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutostartValue {
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
fn classify_autostart(raw: Option<&str>, current_exe: &Path) -> AutostartValue {
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

fn autostart_value() -> Result<AutostartValue, String> {
    let current = std::env::current_exe().map_err(|_| "无法定位截图服务程序".to_string())?;
    Ok(classify_autostart(run_value()?.as_deref(), &current))
}

struct Service {
    root: PathBuf,
    commands: mpsc::Sender<Command>,
    self_weak: std::rc::Weak<std::cell::RefCell<Service>>,
    settings: Settings,
    tray: tray::TrayHandle,
    work: mpsc::Sender<Work>,
    cancel: Arc<AtomicBool>,
    cancel_generation: Arc<AtomicU64>,
    model: ModelState,
    model_error: Option<String>,
    pending_image: Option<(BgrImage, (i32, i32, i32, i32))>,
    busy: bool,
    result: Option<ResultWindowHandle>,
    old_result_visible: bool,
    progress: Option<ProgressWindow>,
    settings_window: Option<SettingsWindow>,
    last_theme_sync: Instant,
    exit_pending: Option<Instant>,
    exit_confirming: Option<Instant>,
    exit_stop_sent: bool,
    stop_retry_at: Option<Instant>,
    pending_hotkey: Option<PendingHotkey>,
    gui_tasks: std::collections::HashMap<u32, bool>,
}

fn selection_cancel_is_current(
    cancel: &AtomicBool,
    generation_token: &AtomicU64,
    generation: u64,
) -> bool {
    generation_token.load(Ordering::Acquire) == generation && cancel.load(Ordering::Acquire)
}

impl Service {
    fn status(&self) -> Value {
        // S8-05：损坏的 Run 值不得计入已启用（保持 bool 口径兼容主界面），
        // 并以独立字段呈现损坏状态。
        let autostart = autostart_value().unwrap_or(AutostartValue::Absent);
        json!({"ok":true,"exiting":self.exit_pending.is_some(),"shared_xberg_protocol":2,"background_service_protocol":1,"model":self.model.as_str(),"task":if self.busy {"recognizing"} else {"idle"},
            "hotkey":self.settings.hotkey,"autostart":matches!(autostart, AutostartValue::Current | AutostartValue::Stale),
            "autostart_corrupt":autostart == AutostartValue::Corrupt,
            "error":self.model_error.as_ref().or(self.settings.warning.as_ref())})
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
            crate::result_window::apply_progress_theme(&window);
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
        crate::result_window::apply_progress_theme(window);
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
    fn request_selection_cancel(
        cancel: Arc<AtomicBool>,
        generation_token: Arc<AtomicU64>,
        generation: u64,
    ) {
        if crate::capture_win::cancel_selection() {
            return;
        }
        // 退出请求可能与托盘的 Trigger 消息竞态：遮罩尚未创建时，单次投递
        // 会落空。短暂重试只发送 WM_DONE，不读取或保存截图内容。
        std::thread::spawn(move || {
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(10));
                // 新任务会复位取消标志，旧取消重试不得关闭新任务的选区。
                if !selection_cancel_is_current(&cancel, &generation_token, generation) {
                    break;
                }
                if crate::capture_win::cancel_selection() {
                    break;
                }
            }
        });
    }
    fn cancel_recognition(&mut self) {
        if !self.busy {
            return;
        }
        self.cancel.store(true, Ordering::Release);
        // 框选窗口运行在托盘线程的消息循环中；取消请求必须显式投递到
        // 当前选区，否则退出/取消只能等用户再次操作遮罩窗口。
        let generation = self.cancel_generation.load(Ordering::Acquire);
        Self::request_selection_cancel(
            Arc::clone(&self.cancel),
            Arc::clone(&self.cancel_generation),
            generation,
        );
        if self.pending_image.take().is_some() {
            self.busy = false;
            self.hide_progress();
            self.restore_old();
            self.tray.notice("已取消识别");
        } else if let Some(window) = self.progress.as_ref() {
            window.set_cancelling(true);
        }
    }
    fn open_main(&self, settings: bool) {
        if let Some(exe) = self.settings.main_exe.as_ref().filter(|p| p.is_file()) {
            let mut command = std::process::Command::new(exe);
            if settings {
                command.arg("--settings");
            }
            if command.spawn().is_err() {
                self.tray.notice("无法打开 JchTools 主界面");
            }
        } else {
            self.tray.notice("主程序位置已改变，请重新打开 JchTools");
        }
    }
    fn open_settings(&mut self) {
        if self.exit_pending.is_none()
            && self.settings.main_exe.as_ref().is_some_and(|p| p.is_file())
        {
            self.open_main(true);
            return;
        }
        if let Some(window) = self.settings_window.as_ref() {
            if !window.window().is_visible() {
                window.set_draft_key("".into());
                window.set_autostart_draft(window.get_autostart_enabled());
            }
            crate::result_window::apply_settings_theme(window);
            let _ = window.show();
            return;
        }
        let Ok(window) = SettingsWindow::new() else {
            self.tray.notice("截图设置窗口创建失败");
            return;
        };
        crate::result_window::apply_settings_theme(&window);
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
        // 热键录制主键归一化（O-14）：Shift+符号反向映射在 Rust 侧实现并单测，
        // slint 的录制处理经此回调取回原键（与主程序 gui.rs 同表）。
        window.on_normalize_shift_key(|shift, key| {
            crate::result_window::normalize_shift_key(shift, &key).into()
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
    fn refresh_theme_if_due(&mut self) {
        if self.last_theme_sync.elapsed() < Duration::from_secs(2) {
            return;
        }
        if let Some(window) = self.settings_window.as_ref() {
            crate::result_window::apply_settings_theme(window);
        }
        if let Some(window) = self.progress.as_ref() {
            crate::result_window::apply_progress_theme(window);
        }
        if let Some(window) = self.result.as_ref() {
            window.refresh_theme();
        }
        self.last_theme_sync = Instant::now();
    }
    fn refresh_settings(&mut self) {
        self.refresh_theme_if_due();
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
            let (enabled, corrupt) = match autostart_value() {
                Ok(AutostartValue::Current | AutostartValue::Stale) => (true, false),
                Ok(AutostartValue::Corrupt) => (false, true),
                Ok(AutostartValue::Absent) | Err(_) => (false, false),
            };
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
            // 设置警告与模型错误都有时以「；」连接，单一存在时原文上屏；
            // 损坏的开机启动值单独追加说明（S8-05：明确报告，不当已启用）。
            let mut status = [
                self.settings.warning.as_deref(),
                self.model_error.as_deref(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("；");
            if corrupt {
                if !status.is_empty() {
                    status.push('；');
                }
                status.push_str(
                    "开机启动项已损坏（未当作已启用，也不会自动改写）；重新保存设置可清除",
                );
            }
            window.set_status_message(status.into());
        }
    }
    /// 进入「后台加载模型」状态：置 Loading、清错误并通知推理线程装载；
    /// 线程已退出时降级为 Error（合同文案两处共用，防改一漏一）。
    fn begin_model_load(&mut self) {
        self.model = ModelState::Loading;
        self.model_error = None;
        if self.work.send(Work::Load).is_err() {
            self.model = ModelState::Error;
            self.model_error = Some("推理线程已退出，请重新启动截图服务".into());
        }
    }
    fn retry(&mut self) {
        if self.exit_pending.is_some()
            || self.model == ModelState::Loading
            || self.model == ModelState::Ready
        {
            return;
        }
        self.begin_model_load();
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
        if hotkey_changed {
            match self.tray.replace(old_hotkey.to_owned()) {
                tray::HotkeyReceipt::Done(Ok(())) => {
                    old_hotkey.clone_into(&mut self.settings.hotkey);
                }
                tray::HotkeyReceipt::Done(Err(_)) => {
                    new_hotkey.clone_into(&mut self.settings.hotkey);
                    let _ = write!(
                        detail,
                        "；原热键已无法恢复，当前仍使用 {new_hotkey}；请重新保存设置"
                    );
                }
                tray::HotkeyReceipt::Late(receiver) => {
                    // S8-01：恢复旧键的替换仍在托盘队列中，超时不是失败终态；
                    // 内存先按旧键记，迟到回执到达后再纠正（Restoring）。
                    old_hotkey.clone_into(&mut self.settings.hotkey);
                    self.pending_hotkey = Some(PendingHotkey::Restoring {
                        new_hotkey: new_hotkey.to_owned(),
                    });
                    self.await_hotkey_receipt(receiver);
                    detail.push_str("；原热键恢复仍在等待托盘确认");
                }
            }
        } else {
            old_hotkey.clone_into(&mut self.settings.hotkey);
        }
        self.settings.warning = Some(detail.clone());
        self.refresh_settings();
        detail
    }
    /// 托起一条迟到回执的等待线程：30 秒上限内到达则派发 [`Command::HotkeyReplaced`]，
    /// 超时派发 [`Command::HotkeyReceiptTimedOut`] 触发自愈（S8-01）。
    fn await_hotkey_receipt(&self, receiver: mpsc::Receiver<Result<(), String>>) {
        let commands = self.commands.clone();
        std::thread::spawn(move || {
            let outcome = receiver.recv_timeout(Duration::from_secs(30));
            let command = match outcome {
                Ok(result) => Command::HotkeyReplaced(result),
                Err(_) => Command::HotkeyReceiptTimedOut,
            };
            let _ = commands.send(command);
        });
    }
    fn save_settings(&mut self, hotkey: &str, enabled: bool) -> Result<(), String> {
        if self.busy || self.exit_pending.is_some() {
            return Err("请等待当前截图任务结束后保存设置".into());
        }
        if self.pending_hotkey.is_some() {
            return Err("上一次热键设置仍在等待托盘确认，请稍后再保存".into());
        }
        tray::parse_hotkey(hotkey)?;
        let old_hotkey = self.settings.hotkey.clone();
        let old_autostart = matches!(
            autostart_value()?,
            AutostartValue::Current | AutostartValue::Stale
        );
        let hotkey_changed = old_hotkey != hotkey;
        if hotkey_changed {
            match self.tray.replace(hotkey.to_owned()) {
                tray::HotkeyReceipt::Done(Ok(())) => {
                    hotkey.clone_into(&mut self.settings.hotkey);
                }
                // 托盘在时限内明确回执失败：Replace 的失败路径不改托盘状态，
                // 原样把原因交回保存入口。
                tray::HotkeyReceipt::Done(Err(reason)) => return Err(reason),
                // S8-01：超时不是失败终态——排队替换仍会在托盘线程恢复后执行。
                // 进入待确认状态，保存收尾顺延到迟到回执；期间不写设置文件，
                // 内存热键保持旧值，保证「回执失败/超时」时重启仍是旧键。
                tray::HotkeyReceipt::Late(receiver) => {
                    self.pending_hotkey = Some(PendingHotkey::Saving {
                        hotkey: hotkey.to_owned(),
                        old_hotkey,
                        old_autostart,
                        autostart: enabled,
                    });
                    self.await_hotkey_receipt(receiver);
                    self.refresh_settings();
                    return Ok(());
                }
            }
        }
        self.commit_settings(hotkey, &old_hotkey, hotkey_changed, old_autostart, enabled)
    }
    /// 保存收尾（热键已生效后）：开机启动与设置文件，任一失败走回滚。
    /// S8-01 起同步路径与迟到回执成功路径共用，保持主流程语义不变。
    fn commit_settings(
        &mut self,
        hotkey: &str,
        old_hotkey: &str,
        hotkey_changed: bool,
        old_autostart: bool,
        enabled: bool,
    ) -> Result<(), String> {
        let autostart_changed = old_autostart != enabled;
        if autostart_changed {
            if let Err(reason) = set_autostart(enabled) {
                return Err(self.rollback_settings(
                    old_hotkey,
                    hotkey,
                    hotkey_changed,
                    Some(old_autostart),
                    reason,
                ));
            }
        }
        if let Err(reason) = self.settings.save(&self.root) {
            return Err(self.rollback_settings(
                old_hotkey,
                hotkey,
                hotkey_changed,
                autostart_changed.then_some(old_autostart),
                reason,
            ));
        }
        self.refresh_settings();
        Ok(())
    }
    /// 迟到回执的裁决（S8-01）：按在途上下文 reconcile 到真实终态；
    /// `None` 表示等待线程的有界时限也耗尽（托盘长时间未泵消息）。
    fn finish_hotkey_receipt(&mut self, outcome: Option<Result<(), String>>) {
        match self.pending_hotkey.take() {
            Some(PendingHotkey::Saving {
                hotkey,
                old_hotkey,
                old_autostart,
                autostart,
            }) => match outcome {
                Some(Ok(())) => {
                    hotkey.clone_into(&mut self.settings.hotkey);
                    // 保存入口早已向界面返回成功；收尾失败经托盘通知上报。
                    if let Err(reason) =
                        self.commit_settings(&hotkey, &old_hotkey, true, old_autostart, autostart)
                    {
                        self.tray.notice(reason);
                    }
                }
                Some(Err(reason)) => {
                    // 托盘明确回执失败：Replace 失败路径不改托盘状态、旧键仍
                    // 生效；内存值与设置文件本就未动，无需回滚，直接上报。
                    self.settings.warning = Some(reason.clone());
                    self.refresh_settings();
                    self.tray.notice(reason);
                }
                None => {
                    // 回执超时：排队中的新键替换仍可能执行，不能当成功也不能
                    // 当失败——自愈：把旧键再排队（FIFO 排在新键之后，托盘恢复
                    // 泵消息后最终回到旧键），内存与文件保持旧键一致。
                    self.tray.replace_detached(old_hotkey.clone());
                    let detail =
                        format!("快捷键确认超时，已自动排队恢复 {old_hotkey}；请稍后重试保存");
                    self.settings.warning = Some(detail.clone());
                    self.refresh_settings();
                    self.tray.notice(detail);
                }
            },
            Some(PendingHotkey::Restoring { new_hotkey, .. }) => {
                if matches!(outcome, Some(Ok(()))) {
                    // 恢复成功：清除等待期提示（Late 分支写入的「仍在等待托盘
                    // 确认」不能残留误导），内存旧键即为终态。
                    self.settings.warning = Some("原快捷键已恢复".to_string());
                    self.refresh_settings();
                } else {
                    // 恢复失败或未确认：托盘实际仍持新键，内存值纠正为新键。
                    new_hotkey.clone_into(&mut self.settings.hotkey);
                    let detail =
                        format!("原快捷键恢复未完成，当前仍使用 {new_hotkey}；请重新保存设置");
                    self.settings.warning = Some(detail.clone());
                    self.refresh_settings();
                    self.tray.notice(detail);
                }
            }
            None => {
                // 无在途上下文的孤立回执（理论不可达，防御）：失败时按当前
                // 内存键自愈重排，避免未知状态漂移。
                if let Some(Err(reason)) = outcome {
                    tracing::warn!(
                        kind = "hotkey_receipt_orphan",
                        "热键回执无对应上下文：{reason}"
                    );
                    let current = self.settings.hotkey.clone();
                    self.tray.replace_detached(current);
                }
            }
        }
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
        // S9-11：截图期间隐藏主程序窗口按完整路径比对，登记值变化须同步内存
        // 与共享登记——先更新 settings 再登记；只登记旧值会让首次 attach 的
        // 会话内隐藏失效，并让 open_main/open_settings 误报「主程序位置已改变」。
        self.settings.main_exe = Some(path);
        crate::capture_win::register_main_exe(self.settings.main_exe.as_deref());
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
        self.cancel_generation.fetch_add(1, Ordering::AcqRel);
        self.cancel.store(false, Ordering::Release);
        self.busy = true;
        self.hide_old();
        self.tray.trigger();
        self.refresh_settings();
    }
    fn request_exit(&mut self) {
        if self.exit_pending.is_some() || self.exit_confirming.is_some() {
            return;
        }
        self.exit_confirming = Some(Instant::now());
        let commands = self.commands.clone();
        // 确认框不阻塞服务事件循环：GUI 心跳仍可更新在途任务和取消结果。
        std::thread::spawn(move || {
            let _ = commands.send(Command::ExitDecision(confirm_background_exit()));
        });
    }
    fn begin_exit(&mut self) {
        self.cancel_recognition();
        self.exit_pending = Some(Instant::now());
        if self.busy || self.model == ModelState::Loading {
            self.tray
                .notice("正在等待当前任务安全结束；退出完成前后台仍保持响应");
            self.open_settings();
        }
    }
    fn stop_worker(&mut self) {
        if self.exit_stop_sent
            || self
                .exit_pending
                .is_none_or(|at| at.elapsed() < Duration::from_secs(2))
        {
            return;
        }
        if self.stop_retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        self.stop_retry_at = None;
        self.gui_tasks.retain(|pid, _| gui_process_alive(*pid));
        if self.gui_tasks.values().any(|busy| *busy) {
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
            "ping" | "get-state" => {
                if let (Some(pid), Some(busy)) = (
                    request["gui_pid"]
                        .as_u64()
                        .and_then(|id| u32::try_from(id).ok()),
                    request["gui_busy"].as_bool(),
                ) {
                    self.gui_tasks.insert(pid, busy);
                }
                Ok(())
            }
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
                if self.exit_pending.is_some() || self.cancel.load(Ordering::Acquire) {
                    self.busy = false;
                    self.pending_image = None;
                    self.hide_progress();
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
                // P-10：截图链路失败落盘（此前只进托盘提示，磁盘日志无迹可循）。
                tracing::warn!(kind = "capture_failed", "截图失败");
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
                    Err(failure) => {
                        // P-10：模型加载失败落盘（未初始化属配置缺失，记 WARN 即可）。
                        tracing::warn!(
                            unconfigured = matches!(failure, LoadFailure::NotConfigured(_)),
                            "截图模型加载失败"
                        );
                        self.model = match failure {
                            LoadFailure::NotConfigured(_) => ModelState::Uninitialized,
                            LoadFailure::Failed(_) => ModelState::Error,
                        };
                        self.model_error = Some(failure.message().to_owned());
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
                    Err(OcrError::Cancelled) => {
                        // 取消使结果窗立即恢复（O-19）。共享引擎为协作取消
                        // （XB-14）：取消不再终止引擎、模型仍常驻；此处仍请求后台
                        // 重载，由 worker 侧先用 snapshot_state 探测，仍就绪则免
                        // 重预热（S8-02），期间的新截图按 O-13「加载中允许截图并
                        // 提示等待模型就绪」排队。
                        self.restore_old();
                        self.tray.notice("已取消识别");
                        // 防御性复位：清掉本请求遗留的取消标志，确保后续
                        // 加载/预热周期不被它影响（当前 warm_up 使用函数内
                        // 局部标志、不读取服务共享的 self.cancel，且产生
                        // pending_image 的路径必先经 request_capture 复位；
                        // 此复位为纵深防御，并非修复已知竞态）。
                        self.cancel.store(false, Ordering::Release);
                        self.begin_model_load();
                    }
                    Err(OcrError::TimedOut(reason)) => {
                        // 识别超时：挂起进程已被 worker 线程终止，连接死亡，
                        // 与子进程退出同路径降级为错误并保留重试入口（O-13）；
                        // 不像取消那样自动重载——超时是故障而非用户意图，避免
                        // 对挂起环境循环重试。
                        tracing::warn!(kind = "timeout", "截图识别失败");
                        self.restore_old();
                        self.tray.notice(reason.as_str());
                        self.model = ModelState::Error;
                        self.model_error = Some(reason);
                    }
                    Err(OcrError::ProcessExited(reason)) => {
                        // 子进程死亡（被误关黑窗、崩溃等）：连接不可复用，模型
                        // 从就绪降级为错误并保留重试入口（O-13/O-30），不得继续
                        // 冒称就绪导致重试按钮失效。
                        tracing::warn!(kind = "process_exited", "截图识别失败");
                        self.restore_old();
                        self.tray.notice("推理子进程已退出；可在设置中重试加载模型");
                        self.model = ModelState::Error;
                        self.model_error = Some(reason);
                    }
                    Err(OcrError::Backend(reason)) => {
                        // 单次推理失败只结束本任务，已预热的模型保持就绪（O-13）。
                        tracing::warn!(kind = "backend", "截图识别失败");
                        self.restore_old();
                        self.tray.notice(format!("OCR 识别失败：{reason}"));
                    }
                    Err(OcrError::ModelFailure(reason)) => {
                        tracing::warn!(kind = "asset_invalid", "截图模型资产失效");
                        self.restore_old();
                        self.tray.notice("截图模型资产失效；可在设置中重试加载模型");
                        self.model = ModelState::Error;
                        self.model_error = Some(reason);
                    }
                    Err(OcrError::Communication(reason)) => {
                        tracing::warn!(kind = "communication", "截图推理通信失败");
                        self.restore_old();
                        self.tray.notice("截图推理通信失败；可在设置中重试加载模型");
                        self.model = ModelState::Error;
                        self.model_error = Some(reason);
                    }
                }
            }
            Command::HotkeyUnavailable(reason) => {
                self.settings.warning = Some(reason);
            }
            Command::OpenMain => self.open_main(false),
            Command::OpenSettings => self.open_settings(),
            Command::InitializeAssets => {
                if let Some(exe) = self.settings.main_exe.as_ref().filter(|p| p.is_file()) {
                    if std::process::Command::new(exe)
                        .arg("--settings")
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
                    self.gui_tasks.retain(|pid, _| gui_process_alive(*pid));
                    if self.gui_tasks.values().any(|busy| *busy) {
                        self.tray
                            .notice("其他界面任务仍在处理，等待安全停止后才能强制退出截图推理");
                    } else {
                        let commands = self.commands.clone();
                        std::thread::spawn(move || {
                            let result = crate::xberg_runtime::force_background_exit()
                                .and_then(crate::xberg_runtime::checked)
                                .map(|_| ());
                            let _ = commands.send(Command::ForceExitFinished(result));
                        });
                    }
                }
            }
            Command::ForceExitFinished(result) => {
                if let Err(error) = result {
                    self.tray.notice(error);
                }
            }
            Command::ToggleAutostart => {
                // S8-01 复审：pending 回执收尾（commit_settings）会按保存时的
                // 旧意图回写自启开关；pending 期间拒绝切换，避免迟到回执把
                // 用户刚做的开关覆盖回去。
                if self.pending_hotkey.is_some() {
                    self.tray
                        .notice("快捷键设置等待确认期间暂不能切换开机启动，请稍后再试");
                } else if let Err(reason) = set_autostart(!autostart_enabled()) {
                    self.tray.notice(reason);
                }
            }
            Command::HotkeyReplaced(outcome) => self.finish_hotkey_receipt(Some(outcome)),
            Command::HotkeyReceiptTimedOut => self.finish_hotkey_receipt(None),
            Command::ExitRequested => self.request_exit(),
            Command::ExitDecision(confirmed) => {
                self.exit_confirming = None;
                if confirmed {
                    self.begin_exit();
                }
            }
            Command::WorkerStopped => {
                // P-10：推理 worker 停止是重要状态切换，落盘留痕。
                tracing::info!("推理 worker 已停止");
                self.complete_exit();
                return;
            }
            Command::WorkerStopFailed(error) => {
                tracing::warn!(kind = "worker_stop_failed", "推理 worker 停止未完成");
                self.exit_stop_sent = false;
                self.stop_retry_at = Some(Instant::now() + Duration::from_secs(1));
                self.tray
                    .notice("后台停止暂未完成，将继续重试；仍可使用强制退出");
                let _ = error;
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
    // SAFETY: GMEM_MOVEABLE(2) 按字节长度分配；失败返回 null，由本函数检查。
    let handle = unsafe { GlobalAlloc(2, size) };
    if handle.is_null() {
        return Err("剪贴板内存分配失败".into());
    }
    // SAFETY: handle 为刚分配的可移动内存句柄；锁定失败返回 null。
    let data = unsafe { GlobalLock(handle) };
    if data.is_null() {
        // SAFETY: 分配成功但锁定失败，所有权仍在手，由本函数释放。
        unsafe { GlobalFree(handle) };
        return Err("剪贴板内存访问失败".into());
    }
    // SAFETY: 源为存活宽字符串切片，目标为锁定内存且按 u16 计数不越界。
    unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), data.cast::<u16>(), wide.len()) };
    // SAFETY: 解锁刚锁定的句柄；解锁后 data 指针不再使用。
    unsafe { GlobalUnlock(handle) };
    // SAFETY: 以 null 关联当前线程；失败即被占用，句柄仍由本函数释放。
    if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
        // SAFETY: 尚未交给剪贴板的所有权重归本函数。
        unsafe { GlobalFree(handle) };
        return Err("剪贴板被其他程序占用".into());
    }
    // SAFETY: 清空剪贴板为设置数据的前置步骤。
    let emptied = unsafe { EmptyClipboard() != 0 };
    // SAFETY: 句柄数据已填好；成功后所有权移交系统，失败时由本函数释放。
    let copied = emptied && !(unsafe { SetClipboardData(13, handle) }).is_null();
    // SAFETY: 关闭剪贴板结束本线程占用。
    unsafe { CloseClipboard() };
    if !copied {
        // SAFETY: 设置失败时系统未接管句柄，由本函数释放。
        unsafe { GlobalFree(handle) };
        return Err("复制到剪贴板失败".into());
    }
    Ok(())
}

fn gui_process_alive(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    // SAFETY: 只请求受限查询权限；pid 来自共享配置，句柄仅在本函数内使用并关闭。
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut code = 0;
    // SAFETY: 句柄刚打开且仅查询退出码；code 为本栈出参。
    let alive = unsafe { GetExitCodeProcess(handle, &raw mut code) } != 0 && code == 259;
    // SAFETY: 查询完成即关闭句柄，此后不再使用。
    unsafe { CloseHandle(handle) };
    alive
}
fn confirm_background_exit() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_YESNO,
    };
    let text = protocol::wide(
        "退出后台服务将停止当前任务、截图及 Xberg。正在处理的内容会在安全边界结束。确定退出？",
    );
    let title = protocol::wide("退出 JchTools 后台服务");
    // SAFETY: 同步系统确认框，宽字符串在调用期间有效。
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
        ) == IDYES
    }
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
    // S8-05：仅当 Run 值形状有效但路径已迁移/过期（Stale——用户此前主动开启
    // 的值）时刷新为本机路径；损坏值（空串/不可解析/他程序）不据此开启、不
    // 重写，只经 status()/设置页报告，绝不擅自开启开机启动。
    if matches!(autostart_value()?, AutostartValue::Stale) {
        set_autostart(true)?;
    }
    // S9-11：截图期间隐藏主程序窗口改按完整进程路径比对（basename 撞名的
    // 其他实例不隐藏），登记当前已知的主程序路径。
    crate::capture_win::register_main_exe(settings.main_exe.as_deref());
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
        cancel_generation: Arc::new(AtomicU64::new(0)),
        model: ModelState::Loading,
        model_error: None,
        pending_image: None,
        busy: false,
        result: None,
        old_result_visible: false,
        progress: None,
        settings_window: None,
        last_theme_sync: Instant::now(),
        exit_pending: None,
        exit_confirming: None,
        exit_stop_sent: false,
        stop_retry_at: None,
        pending_hotkey: None,
        gui_tasks: std::collections::HashMap::new(),
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
                {
                    let mut current = service.borrow_mut();
                    if current.exit_pending.is_some()
                        && !current.busy
                        && current.model != ModelState::Loading
                    {
                        current.stop_worker();
                    }
                    current.refresh_theme_if_due();
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
    use super::{tray, Command, ModelState, OcrError, PendingHotkey, Service, Settings, Work};
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    /// 构造可直接喂 [`Command::OcrFinished`] 的最小服务（模型就绪、任务进行中），
    /// 返回服务与 Work 接收端（供断言后台重载指令）。
    fn ready_busy_service(root: &Path) -> (Service, mpsc::Receiver<Work>) {
        let (commands, _) = mpsc::channel();
        let (work, work_rx) = mpsc::channel();
        let service = Service {
            root: root.to_path_buf(),
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
            cancel_generation: Arc::new(AtomicU64::new(0)),
            model: ModelState::Ready,
            model_error: None,
            pending_image: None,
            busy: true,
            result: None,
            old_result_visible: false,
            progress: None,
            settings_window: None,
            last_theme_sync: Instant::now(),
            exit_pending: None,
            exit_confirming: None,
            exit_stop_sent: false,
            stop_retry_at: None,
            pending_hotkey: None,
            gui_tasks: std::collections::HashMap::new(),
        };
        (service, work_rx)
    }

    // 覆盖 XB-08/O-19（E-3 回归）：取消走「终止进程」路径后，推理连接已死亡，
    // 服务必须在取消结束的同一时刻触发后台重载（置回加载中并发 Work::Load），
    // 让取消后的下一次截图按 O-13「加载中允许截图」等待模型，而不是把新请求
    // 排在已被放弃的旧请求之后。
    #[test]
    fn cancelled_recognition_triggers_model_reload() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (mut service, work_rx) = ready_busy_service(temp.path());
        service.handle(Command::OcrFinished(
            Err(OcrError::Cancelled),
            (0, 0, 20, 20),
        ));
        assert!(!service.busy, "取消后当前任务应立即结束");
        assert_eq!(
            service.model,
            ModelState::Loading,
            "取消终止子进程后必须触发重载（XB-08：旧连接已死，O-13：重载排队）"
        );
        assert_eq!(service.status()["model"], "loading");
        assert!(
            matches!(work_rx.recv_timeout(Duration::from_secs(1)), Ok(Work::Load)),
            "应向推理线程发出 Work::Load 重新 spawn 子进程"
        );
        Ok(())
    }

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
            cancel_generation: Arc::new(AtomicU64::new(0)),
            model: ModelState::Ready,
            model_error: None,
            pending_image: None,
            busy: true,
            result: None,
            old_result_visible: false,
            progress: None,
            settings_window: None,
            last_theme_sync: Instant::now(),
            exit_pending: None,
            exit_confirming: None,
            exit_stop_sent: false,
            stop_retry_at: None,
            pending_hotkey: None,
            gui_tasks: std::collections::HashMap::new(),
        };
        service.handle(Command::OcrFinished(
            Err(OcrError::Backend("一次性推理错误".into())),
            (0, 0, 20, 20),
        ));
        assert_eq!(service.model, ModelState::Ready);
        assert!(service.model_error.is_none());
        assert!(!service.busy);
        assert_eq!(service.status()["model"], "ready");
        Ok(())
    }

    #[test]
    fn asset_invalid_downgrades_ready_model_and_reenables_retry(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (mut service, _work_rx) = ready_busy_service(temp.path());
        service.handle(Command::OcrFinished(
            Err(OcrError::ModelFailure("模型资产失效".into())),
            (0, 0, 20, 20),
        ));
        assert_eq!(service.model, ModelState::Error);
        assert!(service.model_error.is_some());
        assert!(!service.busy);
        service.handle(Command::RetryModel);
        assert_eq!(service.model, ModelState::Loading);
        Ok(())
    }

    #[test]
    fn communication_failure_downgrades_ready_model_and_reenables_retry(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (mut service, _work_rx) = ready_busy_service(temp.path());
        service.handle(Command::OcrFinished(
            Err(OcrError::Communication("共享代理响应线程退出".into())),
            (0, 0, 20, 20),
        ));
        assert_eq!(service.model, ModelState::Error);
        assert!(service.model_error.is_some());
        service.handle(Command::RetryModel);
        assert_eq!(service.model, ModelState::Loading);
        Ok(())
    }

    #[test]
    fn cancelled_image_arriving_late_does_not_start_recognition(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (mut service, work_rx) = ready_busy_service(temp.path());
        service.cancel.store(true, Ordering::Release);
        let image = crate::capture_win::BgrImage::from_vec(8, 8, vec![0; 8 * 8 * 3])
            .map_err(std::io::Error::other)?;
        service.handle(Command::Image(image, (0, 0, 8, 8)));
        assert!(!service.busy);
        assert!(matches!(work_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        Ok(())
    }

    #[test]
    fn stop_background_retries_are_bounded_on_error() {
        let mut calls = 0;
        let result = super::stop_background_with_retry(|| {
            calls += 1;
            Err("代理不可用".to_string())
        });
        assert!(result.is_err());
        assert_eq!(calls, super::WORKER_STOP_ATTEMPTS);
    }

    #[test]
    // 覆盖 O-05/O-21、XB-25：新安装没有字体缓存也能离线就绪。
    fn font_readiness_works_without_external_cache() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        assert!(
            super::verify_font_assets(temp.path()).is_ok(),
            "内置字体不得要求先初始化缓存"
        );
        assert!(
            temp.path().read_dir()?.next().is_none(),
            "字体检查不得生成缓存文件"
        );
        Ok(())
    }

    #[test]
    fn font_readiness_rejects_corrupt_embedded_assets() {
        assert!(matches!(
            super::verify_manifest_file(b"bad", 4, &"00".repeat(32), "截图字体"),
            Err(super::LoadFailure::Failed(message)) if message.contains("大小")
        ));
        assert!(matches!(
            super::verify_manifest_file(b"bad", 3, &"00".repeat(32), "截图字体"),
            Err(super::LoadFailure::Failed(message)) if message.contains("摘要")
        ));
    }

    // 覆盖 O-09/O-13：共享模型已就绪不豁免本服务字体资产校验。
    #[test]
    fn reuse_uses_verified_embedded_font_without_cache() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let outcome = super::reuse_inference(temp.path(), temp.path());
        assert!(
            outcome.is_ok(),
            "复用常驻模型应直接校验内置字体，无需外部缓存"
        );
        Ok(())
    }

    #[test]
    fn stale_selection_cancel_generation_cannot_cancel_new_task() {
        let cancel = AtomicBool::new(true);
        let generation = AtomicU64::new(2);
        assert!(super::selection_cancel_is_current(&cancel, &generation, 2));
        generation.store(3, Ordering::Release);
        cancel.store(false, Ordering::Release);
        assert!(!super::selection_cancel_is_current(&cancel, &generation, 2));
    }

    // 覆盖 O-13：未配置与失败的加载结果分别映射到未初始化与错误状态，
    // 两者都保留各自的界面入口（初始化 / 重试），不冒称就绪。
    #[test]
    fn load_failures_map_to_distinct_model_states() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (commands, _commands_rx) = mpsc::channel();
        let (work, _work_rx) = mpsc::channel();
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
            cancel_generation: Arc::new(AtomicU64::new(0)),
            model: ModelState::Loading,
            model_error: None,
            pending_image: None,
            busy: false,
            result: None,
            old_result_visible: false,
            progress: None,
            settings_window: None,
            last_theme_sync: Instant::now(),
            exit_pending: None,
            exit_confirming: None,
            exit_stop_sent: false,
            stop_retry_at: None,
            pending_hotkey: None,
            gui_tasks: std::collections::HashMap::new(),
        };
        service.handle(Command::ModelLoaded(Err(
            super::LoadFailure::NotConfigured("推理组件未配置".into()),
        )));
        assert_eq!(service.model, ModelState::Uninitialized);
        assert!(service
            .model_error
            .as_deref()
            .is_some_and(|reason| reason.contains("推理组件未配置")));

        // 重试入口把状态置回加载中（Work::Load 保留在通道里由真实线程消费）。
        service.handle(Command::RetryModel);
        assert_eq!(service.model, ModelState::Loading);
        service.handle(Command::ModelLoaded(Err(super::LoadFailure::Failed(
            "推理组件不完整：缺少 models/snapshot-ocr/rec.onnx".into(),
        ))));
        assert_eq!(service.model, ModelState::Error);
        assert_eq!(service.status()["model"], "error");
        Ok(())
    }

    // 覆盖 O-13/O-30（E-2 回归）：识别中推理子进程退出属于连接死亡，必须把
    // 模型从就绪降级为错误并保留重试入口；对照用例
    // inference_failure_does_not_unload_ready_model 证明单次 Backend 失败不降级，
    // 两个分类不得互相吞并。
    #[test]
    fn process_exit_downgrades_ready_model_and_reenables_retry(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (mut service, _work_rx) = ready_busy_service(temp.path());
        service.handle(Command::OcrFinished(
            Err(OcrError::ProcessExited(
                "推理子进程已退出（退出码 3）".into(),
            )),
            (0, 0, 20, 20),
        ));
        assert_eq!(
            service.model,
            ModelState::Error,
            "子进程死亡后模型必须降级，不得继续冒称就绪"
        );
        assert!(service
            .model_error
            .as_deref()
            .is_some_and(|reason| reason.contains("推理子进程已退出")));
        assert!(!service.busy);
        assert_eq!(service.status()["model"], "error");
        // Error 状态下重试入口恢复可用（O-13）：retry 把状态置回加载中。
        service.handle(Command::RetryModel);
        assert_eq!(service.model, ModelState::Loading);
        Ok(())
    }

    // 覆盖 SNAP-05/SNAP-15（修复1回归）：预热失败分类优先读结构化 error_kind。
    // 真实 Xberg 错误文本是英文（"snapshot model asset missing"），旧实现只按
    // 中文「模型」子串匹配必然落空——kind=asset_invalid（模型/资产加载失败）
    // 必须命中「模型加载失败」类文案；kind 缺失时回退子串启发式保持兼容。
    #[test]
    fn warm_up_failure_classifies_by_error_kind() {
        // kind 命中：英文消息也必须分类为模型加载失败（修复前落入「组件预热失败」）。
        let by_kind = super::warm_up_failure(Some("asset_invalid"), "snapshot model asset missing");
        assert!(
            by_kind.contains("推理模型加载失败"),
            "asset_invalid 应分类为模型加载失败，实际 {by_kind}"
        );
        assert!(
            by_kind.contains("snapshot model asset missing"),
            "原始错误摘要应保留，实际 {by_kind}"
        );
        // 其余 kind（input_invalid/cancelled/internal 等）：组件预热失败。
        for kind in ["input_invalid", "cancelled", "internal"] {
            let message = super::warm_up_failure(Some(kind), "boom");
            assert!(
                message.contains("推理组件预热失败"),
                "kind={kind} 应分类为组件预热失败，实际 {message}"
            );
        }
        // kind 缺失回退子串启发式（兼容旧版 Xberg）：中文「模型」仍命中。
        let legacy = super::warm_up_failure(None, "模型文件损坏");
        assert!(legacy.contains("推理模型加载失败"), "实际 {legacy}");
        // kind 缺失且英文消息：预热失败（修复前的既行为，非 kind 场景不改变）。
        let fallback = super::warm_up_failure(None, "warmup crashed");
        assert!(fallback.contains("推理组件预热失败"), "实际 {fallback}");
    }

    // 覆盖 O-13/O-30（修复2回归，服务侧）：识别超时是连接级故障，模型从就绪
    // 降级为错误并保留重试入口；与用户取消（自动后台重载）区分——超时不得
    // 自动发 Work::Load 循环重试挂起的推理环境。
    #[test]
    fn recognition_timeout_downgrades_model_and_reenables_retry(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (mut service, work_rx) = ready_busy_service(temp.path());
        service.handle(Command::OcrFinished(
            Err(OcrError::TimedOut(
                "当前截图识别超时；共享引擎及其他功能保持运行".into(),
            )),
            (0, 0, 20, 20),
        ));
        assert!(!service.busy, "超时后当前任务应立即结束");
        assert_eq!(
            service.model,
            ModelState::Error,
            "超时是故障：模型必须降级为错误，不得冒称就绪"
        );
        assert!(service
            .model_error
            .as_deref()
            .is_some_and(|reason| reason.contains("识别超时")));
        assert_eq!(service.status()["model"], "error");
        // 与取消的自动重载区分：超时不自动发 Work::Load（对照用例
        // cancelled_recognition_triggers_model_reload 断言取消会发）。
        assert!(
            matches!(work_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "超时不得自动触发后台重载（故障降级，非用户意图）"
        );
        // Error 状态下重试入口恢复可用（O-13）：retry 把状态置回加载中。
        service.handle(Command::RetryModel);
        assert_eq!(service.model, ModelState::Loading);
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

    // 覆盖 S8-01：托盘线程在 2 秒时限内未回执（测试句柄永不泵消息，等价于
    // 托盘线程阻塞 >2s）时，保存不得立即报「快捷键更新超时」失败终态——
    // 排队操作在托盘线程恢复后仍会执行，立即报失败会造成「保存失败、实际
    // 热键已变、重启回旧值」。
    #[test]
    fn timeout_hotkey_save_is_not_reported_failure() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (mut service, _work_rx) = ready_busy_service(temp.path());
        service.busy = false;
        let outcome = service.save_settings("Ctrl+Shift+PageDown", false);
        assert!(outcome.is_ok(), "超时未回执不得立即报失败终态：{outcome:?}");
        assert!(
            service.pending_hotkey.is_some(),
            "超时后必须进入待确认状态等待迟到回执"
        );
        assert_eq!(
            service.settings.hotkey, "Ctrl+Alt+O",
            "回执到达前内存热键保持旧值（保证失败/超时路径重启一致）"
        );
        // 模拟托盘线程在 >2s 后完成注册并回执成功：reconcile 为真实终态
        // （成功），继续完成保存收尾（设置文件落地、内存与文件一致）。
        service.handle(Command::HotkeyReplaced(Ok(())));
        assert_eq!(
            service.settings.hotkey, "Ctrl+Shift+PageDown",
            "迟到成功回执应按成功处理"
        );
        assert!(service.pending_hotkey.is_none());
        assert!(service.settings.warning.is_none(), "成功收尾后不得残留警告");
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(temp.path().join("settings.json"))?)?;
        assert_eq!(saved["hotkey"], "Ctrl+Shift+PageDown");
        assert_eq!(service.status()["hotkey"], "Ctrl+Shift+PageDown");
        Ok(())
    }

    // 覆盖 S8-01：迟到回执为真失败（如快捷键冲突）时按失败上报——内存与
    // 设置文件保持旧热键（托盘 Replace 的失败路径不改托盘状态，无需回滚）。
    #[test]
    fn delayed_hotkey_failure_reports_and_keeps_old() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (mut service, _work_rx) = ready_busy_service(temp.path());
        service.busy = false;
        service.pending_hotkey = Some(PendingHotkey::Saving {
            hotkey: "Ctrl+Shift+PageDown".into(),
            old_hotkey: "Ctrl+Alt+O".into(),
            old_autostart: false,
            autostart: false,
        });
        service.handle(Command::HotkeyReplaced(Err(
            "快捷键冲突；原快捷键保持有效".into()
        )));
        assert_eq!(service.settings.hotkey, "Ctrl+Alt+O");
        assert!(
            service
                .settings
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("快捷键冲突")),
            "迟到失败必须上报真实原因"
        );
        assert!(service.pending_hotkey.is_none());
        assert!(
            !temp.path().join("settings.json").exists(),
            "失败路径不得落盘新热键"
        );
        Ok(())
    }

    // 覆盖 S8-01：等待线程的有界时限也耗尽（托盘长时间未泵消息）时的自愈——
    // 旧键被重新排队（FIFO 排在仍在等待的新键替换之后），内存与文件保持旧键，
    // 上报「确认超时」而非「更新失败」。
    #[test]
    fn hotkey_receipt_final_timeout_self_heals_to_old() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (mut service, _work_rx) = ready_busy_service(temp.path());
        service.busy = false;
        service.pending_hotkey = Some(PendingHotkey::Saving {
            hotkey: "Ctrl+Shift+PageDown".into(),
            old_hotkey: "Ctrl+Alt+O".into(),
            old_autostart: false,
            autostart: false,
        });
        service.handle(Command::HotkeyReceiptTimedOut);
        assert_eq!(
            service.settings.hotkey, "Ctrl+Alt+O",
            "回执最终超时后内存保持旧键（文件未写，重启一致）"
        );
        assert!(
            service
                .settings
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("快捷键确认超时")),
            "自愈路径上报确认超时与已排队恢复"
        );
        assert!(service.pending_hotkey.is_none());
        Ok(())
    }

    // 覆盖 S8-02：协作取消（XB-14）后引擎仍就绪时免重预热——重载决策只认
    // snapshot_state=Ready 为可复用；未配置/探测失败/非就绪仍完整重载。
    #[test]
    fn reload_plan_reuses_only_ready_engine() {
        use super::reload_plan;
        use crate::shared_xberg::{ClientError, SnapshotState};
        assert_eq!(
            reload_plan(Some(&Ok(SnapshotState::Ready))),
            super::ReloadPlan::Reuse,
            "引擎仍就绪时取消后不得重复预热模型（S8-02）；字体仍需校验（O-09）"
        );
        let uninitialized = Ok(SnapshotState::Uninitialized);
        let loading = Ok(SnapshotState::Loading);
        let errored = Ok(SnapshotState::Error("上次加载失败".into()));
        let unreachable = Err(ClientError::Io("代理不可用".into()));
        for probe in [
            Some(&uninitialized),
            Some(&loading),
            Some(&errored),
            Some(&unreachable),
            None,
        ] {
            assert_eq!(
                reload_plan(probe),
                super::ReloadPlan::Full,
                "非就绪/探测失败必须完整重载（O-13）"
            );
        }
    }

    // 覆盖 S8-05：Run 值归类——空串/不可解析/他程序/缺参数都是损坏，不得
    // 当作已启用（否则启动时会静默重写、擅自重开自启动）；形状有效但路径
    // 迁移/过期（Stale）仍按已启用处理（用户此前主动开启，启动时刷新路径）。
    #[test]
    fn autostart_value_classification_rejects_corrupt_entries() {
        use super::{classify_autostart, AutostartValue};
        let current = std::path::Path::new(r"C:\app\worker\v1\snap-ocr-worker.exe");
        assert_eq!(classify_autostart(None, current), AutostartValue::Absent);
        assert_eq!(
            classify_autostart(Some(""), current),
            AutostartValue::Corrupt,
            "空串是损坏值，不是已启用（S8-05）"
        );
        assert_eq!(
            classify_autostart(Some("垃圾值"), current),
            AutostartValue::Corrupt
        );
        assert_eq!(
            classify_autostart(
                Some(r#""C:\Windows\System32\notepad.exe" --service--autostart"#),
                current
            ),
            AutostartValue::Corrupt,
            "指向他程序（basename 不符）是损坏值"
        );
        assert_eq!(
            classify_autostart(Some(r#""C:\app\worker\v1\snap-ocr-worker.exe""#), current),
            AutostartValue::Corrupt,
            "缺 --service--autostart 参数不可判为本服务的值"
        );
        assert_eq!(
            classify_autostart(
                Some(r#""C:\app\worker\v1\snap-ocr-worker.exe" --service--autostart"#),
                current
            ),
            AutostartValue::Current
        );
        assert_eq!(
            classify_autostart(
                Some(r#""C:\app\worker\v9\snap-ocr-worker.exe" --service--autostart"#),
                current
            ),
            AutostartValue::Stale,
            "形状有效的旧安装路径按「已启用待刷新」处理"
        );
        // 损坏值永远不是 Stale：启动时的刷新分支（仅 Stale 触发）不会改写它。
        assert_ne!(
            classify_autostart(Some(""), current),
            AutostartValue::Stale,
            "损坏值不得触发启动重写（S8-05）"
        );
    }
    #[test]
    fn xberg_validation_errors_hide_the_user_directory() {
        let directory = Path::new(r"C:\Users\example\Documents\xberg");
        let error = format!(
            "Xberg 资产 models/snapshot-ocr/rec.onnx 缺失（目录 {}）",
            directory.display()
        );
        assert_eq!(
            super::redact_user_path(&error, directory),
            "Xberg 资产 models/snapshot-ocr/rec.onnx 缺失（目录 共享推理目录）"
        );
    }

    // 覆盖 O-22：在独立非交互 window station 的剪贴板复制并读回，
    // 不读取、清空或恢复当前用户 station 的任何剪贴板格式。
    #[test]
    fn copy_all_round_trips_on_isolated_clipboard() {
        const CHILD_FLAG: &str = "JCHTOOLS_ISOLATED_CLIPBOARD_TEST_CHILD";
        use std::ffi::c_void;
        use std::ptr::{null, null_mut};

        #[link(name = "user32")]
        extern "system" {
            fn GetProcessWindowStation() -> *mut c_void;
            fn CreateWindowStationW(
                name: *const u16,
                flags: u32,
                access: u32,
                security: *const c_void,
            ) -> *mut c_void;
            fn SetProcessWindowStation(station: *mut c_void) -> i32;
            fn CloseWindowStation(station: *mut c_void) -> i32;
            fn GetThreadDesktop(thread: u32) -> *mut c_void;
            fn SetThreadDesktop(desktop: *mut c_void) -> i32;
            fn CloseDesktop(desktop: *mut c_void) -> i32;
            fn CreateDesktopW(
                name: *const u16,
                device: *const u16,
                mode: *const c_void,
                flags: u32,
                access: u32,
                security: *const c_void,
            ) -> *mut c_void;
            fn CreateWindowExW(
                ex: u32,
                class: *const u16,
                title: *const u16,
                style: u32,
                x: i32,
                y: i32,
                width: i32,
                height: i32,
                parent: *mut c_void,
                menu: *mut c_void,
                instance: *mut c_void,
                param: *mut c_void,
            ) -> *mut c_void;
            fn DestroyWindow(window: *mut c_void) -> i32;
            fn GetClipboardData(format: u32) -> *mut c_void;
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GlobalSize(memory: *mut c_void) -> usize;
            fn GetCurrentThreadId() -> u32;
        }
        struct IsolatedClipboard {
            previous: *mut c_void,
            station: *mut c_void,
            previous_desktop: *mut c_void,
            desktop: *mut c_void,
            window: *mut c_void,
        }
        impl Drop for IsolatedClipboard {
            fn drop(&mut self) {
                if !self.window.is_null() {
                    // SAFETY: 本测试线程创建并独占隐藏窗口，销毁一次。
                    unsafe { DestroyWindow(self.window) };
                }
                // SAFETY: previous_desktop 保留系统原句柄；隐藏窗口已销毁。
                let restored_desktop = unsafe { SetThreadDesktop(self.previous_desktop) };
                assert_ne!(restored_desktop, 0, "必须恢复线程原 desktop");
                if !self.desktop.is_null() {
                    // SAFETY: 测试 desktop 已不再关联当前线程，关闭独占句柄。
                    unsafe { CloseDesktop(self.desktop) };
                }
                // SAFETY: previous 为进入测试前的有效 station，仍由系统持有。
                let restored = unsafe { SetProcessWindowStation(self.previous) };
                assert_ne!(restored, 0, "必须恢复进程原 window station");
                // SAFETY: 已恢复原 station，关闭本测试创建的独占 station。
                unsafe { CloseWindowStation(self.station) };
            }
        }
        if std::env::var_os(CHILD_FLAG).as_deref() != Some(std::ffi::OsStr::new("1")) {
            // station 是进程级状态，必须用仅运行此用例的独立测试进程，
            // 避免完整测试集并行时改变其他线程的窗口与剪贴板环境。
            let output =
                std::process::Command::new(std::env::current_exe().expect("必须定位当前测试程序"))
                    .args([
                        "--exact",
                        "service::tests::copy_all_round_trips_on_isolated_clipboard",
                        "--test-threads=1",
                    ])
                    .env(CHILD_FLAG, "1")
                    .output()
                    .expect("必须启动独立剪贴板测试进程");
            assert!(
                output.status.success(),
                "隔离剪贴板子进程失败：{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // SAFETY: 只查询进程当前 station；不取得所有权。
        let previous = unsafe { GetProcessWindowStation() };
        assert!(!previous.is_null());
        // SAFETY: 无参数只读查询当前线程 ID。
        let thread = unsafe { GetCurrentThreadId() };
        // SAFETY: 查询线程当前 desktop，不取得所有权。
        let previous_desktop = unsafe { GetThreadDesktop(thread) };
        assert!(!previous_desktop.is_null());
        // SAFETY: 空名字创建独立非交互 station；访问仅用于本进程窗口/剪贴板。
        let station = unsafe { CreateWindowStationW(null(), 0, 0x037f, null()) };
        assert!(!station.is_null(), "无法创建隔离 clipboard station");
        let mut isolated = IsolatedClipboard {
            previous,
            station,
            previous_desktop,
            desktop: null_mut(),
            window: null_mut(),
        };
        // SAFETY: station 为本测试刚创建的有效句柄，尚无测试窗口。
        let selected = unsafe { SetProcessWindowStation(station) };
        assert_ne!(selected, 0, "无法切换到隔离 clipboard station");
        let desktop_name = super::protocol::wide("JchToolsClipboardRegression");
        // SAFETY: 在已切换的独立 station 中创建测试 desktop，字符串有效；
        // DESKTOP_ALL_ACCESS(0x01ff) 用于本线程隐藏窗口及消息队列。
        isolated.desktop =
            unsafe { CreateDesktopW(desktop_name.as_ptr(), null(), null(), 0, 0x01ff, null()) };
        assert!(!isolated.desktop.is_null(), "无法创建隔离 desktop");
        // SAFETY: 当前测试线程尚无窗口或hook，可绑定刚创建的有效 desktop。
        let selected_desktop = unsafe { SetThreadDesktop(isolated.desktop) };
        assert_ne!(selected_desktop, 0, "无法切换到隔离 desktop");
        let class = super::protocol::wide("STATIC");
        let title = super::protocol::wide("JchTools clipboard regression");
        // SAFETY: 系统 STATIC 类，NUL 终止字符串，隐藏窗口无父级和额外参数。
        isolated.window = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                0x8000_0000,
                0,
                0,
                1,
                1,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        assert!(!isolated.window.is_null(), "无法创建隔离剪贴板 owner 窗口");
        let text = "截图 OCR 合成文本  ABC\n  缩进 😀";
        let copied = super::copy_text(text);
        assert!(copied.is_ok(), "复制全部必须成功：{copied:?}");
        // SAFETY: clipboard 属于已切换的独立 station，窗口是本线程有效 owner。
        let opened = unsafe { super::OpenClipboard(isolated.window) };
        assert_ne!(opened, 0, "无法读取隔离剪贴板");
        // SAFETY: 本线程已打开隔离剪贴板，查询 CF_UNICODETEXT。
        let memory = unsafe { GetClipboardData(13) };
        let actual = if memory.is_null() {
            None
        } else {
            // SAFETY: CF_UNICODETEXT 使用全局内存，剪贴板保持打开期间有效。
            let size = unsafe { GlobalSize(memory) };
            // SAFETY: 同一有效全局内存句柄，锁定后只读。
            let data = unsafe { super::GlobalLock(memory) };
            if data.is_null() {
                None
            } else {
                // SAFETY: size 为该块字节容量，按完整 u16 单元只读，不越界。
                let units = unsafe { std::slice::from_raw_parts(data.cast::<u16>(), size / 2) };
                let length = units
                    .iter()
                    .position(|unit| *unit == 0)
                    .unwrap_or(units.len());
                let text = String::from_utf16(&units[..length]).ok();
                // SAFETY: 解锁刚锁定的剪贴板内存，不释放系统所有物。
                unsafe { super::GlobalUnlock(memory) };
                text
            }
        };
        // SAFETY: 配对关闭本线程打开的隔离剪贴板。
        unsafe { super::CloseClipboard() };
        assert_eq!(actual.as_deref(), Some(text), "剪贴板正文必须逐字符保持");
    }
}
