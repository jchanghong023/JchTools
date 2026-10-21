//! 当前用户会话后台 OCR 服务：管道/托盘/热键独立于 JchTools 主窗口。
//! 统一后台随主包交付，识别由共享 Xberg 代理承接（XB-20～XB-25）；
//! 仅资产与显式设置落盘；截图、裁剪与识别结果只在内存与剪贴板。

mod autostart;
mod clipboard;
mod inference;
mod protocol;
mod settings;
mod settings_flow;
mod tasks;
mod tray;
mod windows_ui;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde_json::Value;

use autostart::{autostart_enabled, autostart_value, set_autostart, AutostartValue};
use inference::worker;
use settings::{root, save_launcher, Settings};

use crate::capture_win::BgrImage;
use crate::result_window::{ProgressWindow, ResultWindowHandle, SettingsWindow};

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

impl OcrError {
    fn diagnostic_kind(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::TimedOut(_) => "timeout",
            Self::ProcessExited(_) => "process_exited",
            Self::Backend(_) => "backend",
            Self::Communication(_) => "communication",
            Self::ModelFailure(_) => "asset_invalid",
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
    Pipe(Value, mpsc::SyncSender<Value>, tracing::Span),
    CaptureRequested,
    Image(BgrImage, (i32, i32, i32, i32)),
    CancelledSelection,
    CaptureFailed(String),
    ModelLoaded(Result<(), LoadFailure>),
    OcrFinished(Result<Option<String>, OcrError>, (i32, i32, i32, i32)),
    WorkerStopped(tracing::Span),
    WorkerStopFailed(tracing::Span),
    TrayStopped,
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
    /// 等待托盘回执超过提示时限；恢复操作仍须保留通道等待明确终态。
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
        old_autostart: AutostartValue,
        autostart: bool,
    },
    /// 回滚路径在等「恢复旧热键」的回执：Ok 即完成；Err 把内存值纠正回
    /// 实际仍生效的新键。
    Restoring { new_hotkey: String },
}

enum Work {
    Load(tracing::Span),
    Recognize(BgrImage, (i32, i32, i32, i32), tracing::Span),
    Stop,
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

/// 截图任务的活动阶段；等待模型与已交付推理均属于 Processing。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivityPhase {
    Idle,
    /// Trigger 已派发，等待框选的终态回执。
    Selecting,
    /// 已收到截图，等待模型或推理完成；托盘退出不能终结此阶段。
    Processing,
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
    phase: ActivityPhase,
    diagnostic_task: Option<(tracing::Span, Instant)>,
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

/// 主窗口关闭不会调用此函数的退出；托盘退出才释放后台常驻模型。
pub fn run_service(autostart: bool) -> Result<(), String> {
    let span = crate::logging::operation_span("snap_ocr_service", "run");
    let _entered = span.enter();
    let started = Instant::now();
    tracing::info!(event = "ocr_service_started", autostart, "截图后台服务启动");
    let outcome = run_service_inner(autostart);
    if outcome.is_ok() {
        tracing::info!(
            event = "ocr_service_stopped",
            elapsed_ms = crate::logging::elapsed_ms(started),
            "截图后台服务已结束"
        );
    } else {
        tracing::error!(
            event = "ocr_service_failed",
            error_type = "service",
            elapsed_ms = crate::logging::elapsed_ms(started),
            "截图后台服务未正常完成"
        );
    }
    outcome
}

fn service_failure(error: String, stage: &'static str) -> String {
    tracing::error!(
        event = "ocr_service_stage_failed",
        stage,
        error_type = "initialization",
        "截图服务阶段失败"
    );
    error
}

fn run_service_inner(autostart: bool) -> Result<(), String> {
    let Some(_instance) =
        protocol::claim_instance().map_err(|error| service_failure(error, "claim_instance"))?
    else {
        use std::io::Write;
        tracing::debug!(event = "ocr_service_reused", "已有截图服务，转发设置请求");
        let pipe_name = protocol::pipe_name()
            .map_err(|error| service_failure(error, "existing_pipe_identity"))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(mut pipe) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&pipe_name)
            {
                pipe.write_all(b"{\"command\":\"open-settings\"}\n")
                    .map_err(|_| {
                        service_failure(
                            "已有截图服务，但无法打开其设置".to_string(),
                            "existing_pipe_write",
                        )
                    })?;
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(service_failure(
                    "已有截图服务，但控制管道不可用".into(),
                    "existing_pipe_connect",
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    // 登录自启代表新一轮用户登录；只有取得单实例后才解除上次退出抑制。
    // 普通启动与后台保活必须继续尊重用户在本会话内的明确退出。
    if autostart {
        crate::xberg_runtime::resume_background()
            .map_err(|error| service_failure(error, "autostart_resume_background"))?;
    }
    // F37：测试在任何托盘、热键或桌面窗口创建前观察真实启动许可。
    #[cfg(test)]
    if tests::REVIEW_F37_STARTUP_CHECKPOINT.with(std::cell::Cell::get) {
        return Err("review_f37 startup checkpoint".into());
    }
    crate::capture_win::enable_per_monitor_v2().map_err(|error| service_failure(error, "dpi"))?;
    let root = root().map_err(|error| service_failure(error, "state_root"))?;
    let mut settings = Settings::load(&root);
    let args: Vec<String> = std::env::args().collect();
    if let Some(index) = args.iter().position(|arg| arg == "--main-exe") {
        if let Some(path) = args
            .get(index + 1)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_file())
        {
            save_launcher(&root, &path).map_err(|error| service_failure(error, "save_launcher"))?;
            settings.main_exe = Some(path);
        }
    }
    // S8-05：仅当 Run 值形状有效但路径已迁移/过期（Stale——用户此前主动开启
    // 的值）时刷新为本机路径；损坏值（空串/不可解析/他程序）不据此开启、不
    // 重写，只经 status()/设置页报告，绝不擅自开启开机启动。
    if matches!(
        autostart_value().map_err(|error| service_failure(error, "autostart_read"))?,
        AutostartValue::Stale
    ) {
        set_autostart(true).map_err(|error| service_failure(error, "autostart_refresh"))?;
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
    let worker_span = tracing::Span::current();
    std::thread::spawn(move || {
        let _entered = worker_span.enter();
        worker(&worker_root, &work_rx, &worker_events, &worker_cancel);
    });
    let tray = tray::start(tx.clone(), settings.hotkey.clone(), autostart)
        .map_err(|error| service_failure(error, "tray"))?;
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
        phase: ActivityPhase::Idle,
        diagnostic_task: None,
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
    let pipe_span = tracing::Span::current();
    std::thread::spawn(move || {
        let _entered = pipe_span.enter();
        protocol::serve(&ipc, &ready_tx);
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| service_failure("截图控制管道启动超时".to_string(), "pipe_ready_timeout"))?
        .map_err(|error| service_failure(error, "pipe_start"))?;
    if service
        .work
        .send(Work::Load(tracing::Span::current()))
        .is_err()
    {
        tracing::warn!(
            event = "ocr_load_dispatch_failed",
            stage = "startup_worker_dispatch",
            error_type = "disconnected",
            "截图模型初始加载请求无法发送"
        );
    }
    tracing::info!(event = "ocr_service_ready", "截图后台服务已就绪");
    let service = std::rc::Rc::new(std::cell::RefCell::new(service));
    service.borrow_mut().self_weak = std::rc::Rc::downgrade(&service);
    let timer = slint::Timer::default();
    {
        let service = service.clone();
        let timer_span = tracing::Span::current();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(40),
            move || {
                let _entered = timer_span.enter();
                while let Ok(command) = rx.try_recv() {
                    service.borrow_mut().handle(command);
                }
                {
                    let mut current = service.borrow_mut();
                    if current.exit_pending.is_some()
                        && current.phase == ActivityPhase::Idle
                        && current.model != ModelState::Loading
                    {
                        current.stop_worker();
                    }
                    current.release_closed_result();
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
    slint::run_event_loop_until_quit()
        .map_err(|_| service_failure("截图服务窗口事件循环失败".to_string(), "event_loop"))
}

#[cfg(all(test, feature = "test-hooks"))]
#[path = "../../../tests/common/engine_session.rs"]
mod engine_session;

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
mod tests;
