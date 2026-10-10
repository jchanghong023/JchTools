//! GUI 组装层：Slint 界面的状态、回调整体在此实现；`main.rs` 只是薄壳入口。
//! 同步回调集中在 `wire_sync`，便于无头测试装配后直接断言界面状态。
/// Slint 生成代码（target/**/out/app.rs）不做 unwrap/可达性审查：机器生成、
/// 修复无意义且计数随 UI 改版大幅波动；业务代码保持 unwrap/expect 全面禁止。
/// todo 豁免同因：slint 1.17.1 生成器在 embed_component 等内部桩里无条件发射
/// todo 宏，无生成器配置可关闭；仅作用于本生成模块，不覆盖业务代码。
#[allow(clippy::unwrap_used)]
#[allow(clippy::todo)]
#[allow(unreachable_pub)]
mod generated_ui {
    slint::include_modules!();
}
pub use generated_ui::*;

use crate::{
    acp_api::{runtime as acp_runtime, settings as acp_settings, ServiceConfig, ServiceStatus},
    config::{Config, DeleteChoice},
    control::{Context, Control, Event, PlanSnapshot},
    db::Database,
    engine,
    git_tools::{self, GitShared, BACKOFF_UNIT},
    markdown::{self, FormatGroup},
    markdown_assets, md_tools,
    model::{bytes, ActionKind, Summary},
    registry, snap_ocr_assets,
};
use anyhow::Result;
use serde::Deserialize;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
#[cfg(windows)]
use std::io::{BufRead, BufReader, Write};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

/// OCR 命令由单个后台线程串行执行；心跳和主动操作共享同一连接语义。
enum SnapCommand {
    Ensure,
    Request(serde_json::Value),
}

enum SnapMessage {
    Readiness(u64, Result<(), String>),
    Progress(u64, String),
    Initialized(u64, Result<(), String>),
    Service(Result<serde_json::Value, String>, bool),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AcpOperation {
    Ensure,
    Save,
    Apply,
    Stop,
}

impl AcpOperation {
    fn text(self) -> &'static str {
        match self {
            Self::Ensure => "连接",
            Self::Save => "保存",
            Self::Apply => "应用并重启",
            Self::Stop => "退出",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AcpRequest {
    id: u64,
    operation: AcpOperation,
}

impl AcpRequest {
    const CONNECT: Self = Self {
        id: 0,
        operation: AcpOperation::Ensure,
    };
}

enum AcpCommand {
    Ensure,
    Save {
        executable: String,
        arguments: String,
        port: String,
        stopped: bool,
    },
    Apply,
    Stop,
}

impl AcpCommand {
    fn operation(&self) -> AcpOperation {
        match self {
            Self::Ensure => AcpOperation::Ensure,
            Self::Save { .. } => AcpOperation::Save,
            Self::Apply => AcpOperation::Apply,
            Self::Stop => AcpOperation::Stop,
        }
    }
}

enum AcpMessage {
    Snapshot(u64, std::result::Result<ServiceStatus, String>),
    Completed(AcpRequest, std::result::Result<ServiceStatus, String>),
}

/// AH-06/AH-07：IPC、SQLite、启动和安全收尾均不进入界面线程。
/// 丢弃本句柄只停止观察，不取消后台操作或关闭 Agent。
struct AcpObserver {
    commands: mpsc::Sender<(AcpRequest, AcpCommand)>,
    stops: mpsc::Sender<(AcpRequest, AcpCommand)>,
    explicitly_stopped: Arc<std::sync::atomic::AtomicBool>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}
impl Drop for AcpObserver {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
}

fn run_acp_commands(
    receiver: &mpsc::Receiver<(AcpRequest, AcpCommand)>,
    sender: &mpsc::Sender<AcpMessage>,
    closed: &std::sync::atomic::AtomicBool,
    explicitly_stopped: &std::sync::atomic::AtomicBool,
) {
    loop {
        let (request, command) = match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if closed.load(Ordering::Acquire) {
                    break;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if explicitly_stopped.load(Ordering::Acquire)
            && matches!(command, AcpCommand::Ensure | AcpCommand::Apply)
        {
            // 确认退出后，尚未开始的自动连接/应用不得重开本 GUI 的服务。
            continue;
        }
        let result = match command {
            AcpCommand::Ensure => acp_runtime::ensure_started(),
            AcpCommand::Save {
                executable,
                arguments,
                port,
                stopped,
            } => (|| {
                let config = ServiceConfig {
                    executable,
                    arguments: acp_settings::parse_arguments(&arguments)?,
                    port: acp_settings::parse_port(&port)?,
                };
                acp_settings::validate_config(&config)?;
                if stopped || explicitly_stopped.load(Ordering::Acquire) {
                    // 当前 GUI 主动退出后只持久保存，不隐式重开服务（AH-10）。
                    acp_settings::save_config(&config)?;
                    acp_runtime::status()
                } else {
                    acp_runtime::save_config(&config)
                }
            })(),
            AcpCommand::Apply => acp_runtime::apply_and_restart(),
            AcpCommand::Stop => acp_runtime::stop(),
        };
        if sender
            .send(AcpMessage::Completed(
                request,
                result.map_err(|error| error.to_string()),
            ))
            .is_err()
        {
            break;
        }
    }
}

fn start_acp_observer(state: &Rc<RefCell<State>>, ensure: bool) {
    let mut state = state.borrow_mut();
    if let Some(observer) = &state.acp_observer {
        if ensure && !state.acp_explicit_stopped {
            let _ = observer
                .commands
                .send((AcpRequest::CONNECT, AcpCommand::Ensure));
        }
        return;
    }
    let (commands, receiver) = mpsc::channel();
    let (stops, stop_receiver) = mpsc::channel();
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let explicitly_stopped = Arc::new(std::sync::atomic::AtomicBool::new(
        state.acp_explicit_stopped,
    ));
    // Stop 拥有独立控制线程和 IPC 连接，不排在等待排空的 Apply 后面。
    for receiver in [receiver, stop_receiver] {
        let worker_closed = closed.clone();
        let worker_stopped = explicitly_stopped.clone();
        let sender = state.acp_sender.clone();
        std::thread::spawn(move || {
            run_acp_commands(&receiver, &sender, &worker_closed, &worker_stopped);
        });
    }
    let observation_generation = state.acp_observation_generation.clone();
    let observer_closed = closed.clone();
    let sender = state.acp_sender.clone();
    std::thread::spawn(move || {
        while !observer_closed.load(Ordering::Acquire) {
            // 在读取开始时取代际；保存完成之后才送达的旧读取仍属于旧代际。
            let generation = observation_generation.load(Ordering::Acquire);
            let snapshot = acp_runtime::status().map_err(|error| error.to_string());
            if sender
                .send(AcpMessage::Snapshot(generation, snapshot))
                .is_err()
            {
                break;
            }
            // 短等待使关闭观察及时生效；不阻塞 GUI，不触发 ensure/restart。
            for _ in 0..5 {
                if observer_closed.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
    if ensure && !state.acp_explicit_stopped {
        let _ = commands.send((AcpRequest::CONNECT, AcpCommand::Ensure));
    }
    state.acp_observer = Some(AcpObserver {
        commands,
        stops,
        explicitly_stopped,
        closed,
    });
}

fn request_acp_action(ui: &AppWindow, state: &Rc<RefCell<State>>, command: AcpCommand, text: &str) {
    start_acp_observer(state, false);
    let operation = command.operation();
    let mut state = state.borrow_mut();
    state.acp_request_id += 1;
    let request = AcpRequest {
        id: state.acp_request_id,
        operation,
    };
    let sent = state.acp_observer.as_ref().is_some_and(|observer| {
        if operation == AcpOperation::Stop {
            observer.explicitly_stopped.store(true, Ordering::Release);
            observer.stops.send((request, command)).is_ok()
        } else {
            observer.commands.send((request, command)).is_ok()
        }
    });
    if sent {
        state.acp_pending_request = Some(request);
        // 抢占时即建立观察边界：Stop 前开始读取的快照不得回退退出状态。
        state
            .acp_observation_generation
            .fetch_add(1, Ordering::AcqRel);
        ui.set_acp_apply_inflight(operation == AcpOperation::Apply);
        ui.set_acp_request_pending(true);
        ui.set_acp_operation(text.into());
        ui.set_acp_action_error("".into());
    } else {
        ui.set_acp_action_error("模型服务操作线程已退出，请重新打开工具箱".into());
    }
}

fn can_request_acp_stop(ui: &AppWindow, state: &State) -> bool {
    !state.acp_explicit_stopped
        && ui.get_acp_status_known()
        && !ui.get_acp_service_pid().is_empty()
        && (!ui.get_acp_request_pending()
            || state
                .acp_pending_request
                .is_some_and(|request| request.operation == AcpOperation::Apply))
}

fn acp_config_text(config: Option<&ServiceConfig>) -> String {
    config.map_or_else(
        || "无".into(),
        |config| {
            format!(
                "程序：{}\n参数：{}\n地址：http://127.0.0.1:{}",
                config.executable,
                acp_settings::format_arguments(&config.arguments),
                config.port
            )
        },
    )
}

fn apply_acp_status(ui: &AppWindow, state: &Rc<RefCell<State>>, status: &ServiceStatus) {
    use crate::acp_api::ServicePhase;
    let (phase, text) = match status.phase {
        ServicePhase::Unconfigured => ("unconfigured", "未配置有效的 Agent 启动程序"),
        ServicePhase::Starting => ("starting", "正在启动模型服务并初始化 ACP"),
        ServicePhase::Ready => ("ready", "模型服务已就绪"),
        ServicePhase::Draining => ("draining", "停止接收新请求；等待所有已有请求完成后应用配置"),
        ServicePhase::Stopping => ("stopping", "停止接收新请求；正在取消并等待在途任务安全收尾"),
        ServicePhase::Stopped => ("stopped", "模型服务已停止"),
        ServicePhase::Error => ("error", "模型服务未就绪，请查看错误详情"),
    };
    ui.set_acp_phase(phase.into());
    ui.set_acp_status(text.into());
    ui.set_acp_ready(status.phase == ServicePhase::Ready);
    ui.set_acp_status_known(true);
    ui.set_acp_pending_apply(status.pending_apply() && !state.borrow().acp_explicit_stopped);
    ui.set_acp_service_pid(
        status
            .service_pid
            .map_or_else(String::new, |pid| pid.to_string())
            .into(),
    );
    ui.set_acp_agent_pid(
        status
            .agent_pid
            .map_or_else(String::new, |pid| pid.to_string())
            .into(),
    );
    ui.set_acp_executing(i32::try_from(status.executing).unwrap_or(i32::MAX));
    ui.set_acp_waiting(i32::try_from(status.waiting).unwrap_or(i32::MAX));
    {
        let mut state = state.borrow_mut();
        if let Some(error) = &status.error {
            state.acp_service_error = Some(error.clone());
        } else if status.phase == ServicePhase::Ready {
            // 无后台的 Stopped 轮询不是恢复，不能抹去自动连接的具体失败原因。
            state.acp_service_error = None;
        }
        ui.set_acp_service_error(
            state
                .acp_service_error
                .as_deref()
                .unwrap_or_default()
                .into(),
        );
    }
    ui.set_acp_saved_config(acp_config_text(status.saved_config.as_ref()).into());
    ui.set_acp_running_config(acp_config_text(status.running_config.as_ref()).into());
    if !state.borrow().acp_draft_edited {
        if let Some(config) = &status.saved_config {
            ui.set_acp_executable(config.executable.clone().into());
            ui.set_acp_arguments(acp_settings::format_arguments(&config.arguments).into());
            ui.set_acp_port(config.port.to_string().into());
        }
    }
}

fn apply_acp_messages(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let messages: Vec<_> = state
        .borrow()
        .acp_receiver
        .borrow_mut()
        .try_iter()
        .collect();
    for message in messages {
        let result = match message {
            AcpMessage::Snapshot(generation, result) => {
                if generation
                    != state
                        .borrow()
                        .acp_observation_generation
                        .load(Ordering::Acquire)
                {
                    continue;
                }
                result
            }
            AcpMessage::Completed(request, result) => {
                let mut state = state.borrow_mut();
                if request.operation == AcpOperation::Ensure {
                    // 已退出的 GUI 不接受迟到自动连接的结果。
                    if state.acp_explicit_stopped {
                        continue;
                    }
                } else if state.acp_pending_request != Some(request) {
                    // Apply 被 Stop 抢占后，迟到结果不消费 Stop 门禁或覆盖其终态。
                    continue;
                }
                // 完成回包确立新的观察边界；此前已开始的读取，即使稍后送达也不得上屏。
                state
                    .acp_observation_generation
                    .fetch_add(1, Ordering::AcqRel);
                // 后台自动连接不消费正在保存/应用/退出的 UI 门禁。
                if request.operation != AcpOperation::Ensure {
                    state.acp_pending_request = None;
                    ui.set_acp_apply_inflight(false);
                    let action = request.operation.text();
                    ui.set_acp_request_pending(false);
                    ui.set_acp_operation(
                        if result.is_ok() {
                            format!("{action}已完成")
                        } else {
                            format!("{action}未成功")
                        }
                        .into(),
                    );
                    if request.operation == AcpOperation::Save && result.is_ok() {
                        state.acp_draft_edited = false;
                    }
                    ui.set_acp_action_error(
                        result.as_ref().err().cloned().unwrap_or_default().into(),
                    );
                }
                drop(state);
                result
            }
        };
        match result {
            Ok(status) => apply_acp_status(ui, state, &status),
            Err(error) => {
                state.borrow_mut().acp_service_error = Some(error.clone());
                ui.set_acp_status_known(false);
                ui.set_acp_ready(false);
                ui.set_acp_status("无法读取或连接模型服务".into());
                ui.set_acp_service_error(error.into());
            }
        }
    }
}

fn wire_acp_http(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let edited_state = state.clone();
    ui.on_acp_config_edited(move || {
        edited_state.borrow_mut().acp_draft_edited = true;
    });
    let weak = ui.as_weak();
    let save_state = state.clone();
    ui.on_acp_save_config(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_confirm_kind() != 0 || ui.get_acp_request_pending() {
            return;
        }
        let stopped = save_state.borrow().acp_explicit_stopped;
        save_state.borrow_mut().acp_draft_edited = true;
        request_acp_action(
            &ui,
            &save_state,
            AcpCommand::Save {
                executable: ui.get_acp_executable().to_string(),
                arguments: ui.get_acp_arguments().to_string(),
                port: ui.get_acp_port().to_string(),
                stopped,
            },
            "正在校验并保存配置…",
        );
    });
    let weak = ui.as_weak();
    let apply_state = state.clone();
    ui.on_acp_apply_and_restart(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_confirm_kind() != 0
            || ui.get_acp_request_pending()
            || !ui.get_acp_status_known()
            || !ui.get_acp_pending_apply()
            || apply_state.borrow().acp_explicit_stopped
        {
            return;
        }
        request_acp_action(
            &ui,
            &apply_state,
            AcpCommand::Apply,
            "正在停止接新请求，等待全部已有请求完成后应用并重启…",
        );
    });
    let weak = ui.as_weak();
    let stop_state = state.clone();
    ui.on_acp_request_stop(move || {
        let Some(ui) = weak.upgrade() else { return; };
        if ui.get_confirm_kind() != 0 || !can_request_acp_stop(&ui, &stop_state.borrow()) {
            return;
        }
        ui.set_confirm_pending(false);
        ui.set_acknowledge(false);
        ui.set_confirm_text("确认退出独立模型服务？\n将停止接收新请求，取消在途模型请求并等待安全收尾，再释放本应用拥有的 Agent、终端及连接。不自动强杀。\n截图、Xberg 及其他工具不受影响。\n当前工具箱不会自动重启模型服务；重新打开工具箱后按已保存配置自动启动。".into());
        ui.set_confirm_kind(5);
    });
}

/// 当前注册工具：决定规则分区集合、流程与状态文案。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tool {
    Extract,
    Organizer,
    Md,
    Git,
    MarkdownConverter,
    /// 截图 OCR（O-01）：第六个注册工具，独立后台服务 + 热键截图。
    SnapOcr,
    AcpHttp,
}
impl Tool {
    fn sections(self) -> &'static [&'static str] {
        match self {
            Tool::Extract => &["解压", "安全与性能"],
            Tool::Organizer => &["去重", "归类", "清理", "安全与性能"],
            // R-01：MD 整理与 Git 工具不设规则面板，分区集合为空。
            Tool::Md | Tool::Git | Tool::MarkdownConverter | Tool::SnapOcr | Tool::AcpHttp => &[],
        }
    }
    fn from_id(id: &str) -> Option<Self> {
        match id {
            "recursive-extract" => Some(Tool::Extract),
            "directory-organizer" => Some(Tool::Organizer),
            "md-organizer" => Some(Tool::Md),
            "git-tools" => Some(Tool::Git),
            "markdown-converter" => Some(Tool::MarkdownConverter),
            "snap-ocr" => Some(Tool::SnapOcr),
            "acp-http" => Some(Tool::AcpHttp),
            _ => None,
        }
    }
}
/// 当前运行中的任务种类：实时指标与进度口径随工具区分（U-03/U-12）。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum RuntimeMode {
    #[default]
    Organizer,
    Extract,
    Md,
    Git,
    MarkdownConverter,
}
/// 等待覆盖确认后执行的 MD 操作（M-07/M-11）：确认（confirm-kind=4）后由界面重新发起。
#[derive(Clone)]
enum MdPending {
    Merge {
        root: PathBuf,
        recursive: bool,
        output: PathBuf,
    },
    Split {
        input: PathBuf,
        limit: u64,
        out_dir: PathBuf,
    },
}
/// 规则的界面层级：basic 常显，advanced 只在「显示高级选项」打开时出现。
#[derive(Clone, Copy, PartialEq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum Tier {
    #[default]
    Basic,
    Advanced,
}
/// rules.json 未写 tools 时的兜底：两类工具都显示（全部行都显式标注，兜底仅防手误）。
fn default_tools() -> Vec<String> {
    vec!["extract".into(), "organizer".into()]
}
#[derive(Clone, Deserialize)]
struct RuleSpec {
    section: String,
    key: String,
    title: String,
    hint: String,
    kind: String,
    choices: Vec<Vec<String>>,
    #[serde(default)]
    tier: Tier,
    #[serde(default = "default_tools")]
    tools: Vec<String>,
    /// 附录 D：容量项可用 B/KiB/MiB/GiB/TiB 单位输入；None 表示普通整数。
    #[serde(default)]
    unit: Option<String>,
}
struct State {
    config: Config,
    specs: Vec<RuleSpec>,
    /// 附录 D：不能表示为配置数字的编辑草稿，保留到用户修正；不得以旧值执行。
    invalid_rule_inputs: HashMap<String, String>,
    section: String,
    task: Option<PathBuf>,
    control: Option<Arc<Control>>,
    logs: VecDeque<String>,
    /// 转 Markdown 专用日志；转换任务切换到旧工具时不会污染旧工具日志。
    convert_logs: VecDeque<String>,
    page: usize,
    page_starts: Vec<i64>,
    started: Instant,
    close_after: bool,
    pending_selection: usize,
    /// 勾选落库后的依赖重算仍在途；完成前不得确认执行旧计划。
    plan_recompute_inflight: usize,
    /// 依赖重算失败后须重新分析，不能以旧计划继续执行。
    plan_recompute_failed: bool,
    applying: bool,
    /// 递归解压运行中：解压不写整理流程的 read_bytes/completed 计数器，实时指标与
    /// 进度说明必须走单独口径，否则解压期间会显示恒为 0 的整理指标（U-03）。
    extracting: bool,
    planned: u64,
    plan_filter: Option<String>,
    /// 本轮勾选保存中出现过失败：pending 归零时用于决定是否重载计划页
    selection_failed: bool,
    /// 依赖重算结果到达时勾选批次仍在途，行状态变化尚未上屏（C-01：
    /// 依赖改变必须显示给用户）——pending 归零时补一次计划页重载。
    plan_recompute_dirty: bool,
    /// 「显示高级选项」开关：只影响显示，不落盘、不改变任何默认值
    show_advanced: bool,
    /// 当前工具：切工具时同步重置规则分区（P-02 两工具各自只显示相关分区，R-01）。
    tool: Tool,
    /// 当前任务的运行口径（U-03/U-12）：MD/Git 不写整理流程的计数器与文案。
    runtime: RuntimeMode,
    /// MD 整理：等待覆盖确认的挂起操作（M-07/M-11）。
    md_pending: Option<MdPending>,
    /// Git 工具：运行中任务的共享进度（G-13），由事件泵轮询上屏。
    git_shared: Option<Arc<GitShared>>,
    /// 转 Markdown：独立取消信号；不复用目录整理/解压的控制器。
    convert_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// 转 Markdown：当前正在处理的文件相对路径（T-22 实时指标的当前文件口径）；
    /// 空 = 尚未进入单文件处理（扫描/两个文件的间隙）。
    convert_current: String,
    /// 转 Markdown 初始化专用取消信号。
    convert_init_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// 转 Markdown 当前选择的组件预检在途；预检不占用 busy，避免阻塞其它页面。
    convert_preparing: bool,
    /// 预检通过后交给真正转换 worker 的参数。
    convert_pending_options: Option<markdown::Options>,
    /// 转 Markdown 组件检查代际；旧检查结果不得覆盖新初始化/检查状态。
    convert_readiness_generation: u64,
    /// S1-05：当前代的就绪结果在任务运行中被丢弃（busy 时无法上屏）——任务终态
    /// （CONVERTER_DONE/FAIL 收尾）须补查一次，页面不得长期停留过期就绪状态。
    convert_readiness_missed: bool,
    /// 转 Markdown 启动前场景预检代际。
    convert_preflight_generation: u64,
    /// 截图 OCR（O 分区）：可选组件初始化的取消信号（O-06 取消/重试语义）。
    snap_init_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// 截图 OCR 资产检查代际：迟到的旧检查/初始化收尾不得覆盖新状态。
    snap_generation: u64,
    /// 截图 OCR 服务监督线程的命令端（O-11：GUI 只连接/请求，窗口关闭不停止服务）。
    snap_commands: Option<mpsc::Sender<SnapCommand>>,
    snap_foreground_busy: Arc<std::sync::atomic::AtomicBool>,
    snap_sender: mpsc::Sender<SnapMessage>,
    snap_receiver: RefCell<mpsc::Receiver<SnapMessage>>,
    /// AH-07：只拥有 GUI 观察线程，不拥有后台服务生命周期。
    acp_observer: Option<AcpObserver>,
    acp_sender: mpsc::Sender<AcpMessage>,
    acp_receiver: RefCell<mpsc::Receiver<AcpMessage>>,
    /// AH-09：观察读取从开始到操作完成的跨线程代际，不按回包到达顺序判断新旧。
    acp_observation_generation: Arc<AtomicU64>,
    acp_request_id: u64,
    acp_pending_request: Option<AcpRequest>,
    /// AH-08：保留具体连接/后台失败直到真正就绪；无后台轮询不能清空。
    acp_service_error: Option<String>,
    acp_draft_edited: bool,
    acp_explicit_stopped: bool,
    /// 测试注入：覆盖任务状态目录；生产路径为 None，仍走 engine::prepare/apply。
    engine_overrides: Option<EngineTestOverrides>,
    /// 解压确认清点的请求代际：迟到的低代际清点事件不得刷新文案或解除门禁（X-02）。
    extract_generation: u64,
    /// 计划页加载代际（跨线程）：丢弃晚到的旧 filter/page 结果。
    plan_load: Arc<PlanLoadSync>,
    /// 目录/工具变化的反馈意图保留到最新有效快照落地，不随被顶替的请求丢失。
    readiness_status_pending: Cell<bool>,
    /// U-10 失败列表：当前解压任务的任务库目录。引擎在状态目录下创建任务库，启动时
    /// 界面只知状态目录，由 watcher（`watch_extract_task`）发现新目录后回传。
    extract_task: Option<PathBuf>,
    /// 失败列表分页游标（每页首项的事件 id）与请求代际（丢弃迟到的旧结果）。
    fail_page: usize,
    fail_page_starts: Vec<i64>,
    fail_gen: u64,
    /// 失败列表专用通道：Sender 克隆给 watcher/分页 worker，Receiver 由事件泵排空
    /// （Event 枚举属引擎事件协议，不扩展；见 FailChannelMsg）。
    fail_sender: mpsc::Sender<FailChannelMsg>,
    fail_receiver: RefCell<mpsc::Receiver<FailChannelMsg>>,
}
/// 计划页/就绪快照的请求代际：worker 完成后按事件自带代际与当前视图比较，
/// 界面只应用「仍是最新代际」的结果。两个计数器分开：页面与就绪快照是两类独立请求
/// （切工具/目录编辑只重算就绪，不重载页面），互不顶掉对方的结果。
#[derive(Default)]
struct PlanLoadSync {
    /// 最新计划页请求的代际（每次页面加载递增）
    page: AtomicU64,
    /// 最新就绪快照请求的代际（页面请求也带回快照，因此同样递增）
    state: AtomicU64,
}

/// U-10 失败列表的一行数据（worker 取数结果）：一个失败包或 X-10 卷集对应一项。
#[derive(Debug)]
struct FailItem {
    /// 阶段标识：解压失败 / 未完全解开 / 源包清理失败（X-05 单独标明，不冒充坏包）。
    stage: &'static str,
    /// 原路径（相对所选目录）。
    source: String,
    /// 失败原因（任务库事件 reason 字段）。
    reason: String,
    /// 隔离后位置；无隔离记录时为空（界面如实显示「未记录隔离位置」，不编造「已隔离」）。
    target: String,
}

/// 失败列表分页 worker 的取数结果。
#[derive(Debug)]
struct FailPage {
    task: PathBuf,
    generation: u64,
    page: usize,
    items: Vec<FailItem>,
    total: u64,
    has_more: bool,
    /// 本页最后一项的任务库事件 id：下一页的取数游标。
    last_id: i64,
    error: Option<String>,
}

/// 失败列表专用通道的消息。`Event` 枚举属于引擎事件协议（control.rs），这里不扩展它：
/// 解压失败列表的分页结果与任务目录发现走独立通道，由事件泵每拍排空，
/// 与计划页 worker 同一「界面线程不开库、后台分页取数」架构（H-02）。
#[derive(Debug)]
enum FailChannelMsg {
    /// watcher 发现当前解压任务的任务库目录（引擎在状态目录下创建，界面事先不知道路径；
    /// 首项为发起解压时的清点请求代际，迟到的发现按代际整体丢弃）。
    TaskFound(PathBuf, u64),
    /// 分页取数结果。
    Page(FailPage),
}

/// 解压失败列表的取数条件（U-10/X-06/S-07）：
/// - `解压` 阶段的 `失败`（引擎报错）与 `未完全解开`（成员被跳过/排除等）记录：一个
///   失败包或卷集对应一项；
/// - `删除` 阶段的 `失败` 记录：源包清理失败按 X-05 单独标明阶段，不冒充坏包；
/// - `移入解压失败` 记录不单独成项，只作为其后最近一个失败项的隔离后位置归属。
const FAIL_ITEM_PREDICATE: &str =
    "(phase='解压' AND result IN ('失败','未完全解开')) OR (phase='删除' AND result='失败')";

/// 运行中失败列表的自动刷新间隔（U-10「运行中可查看已经发生的失败」）。
const FAIL_REFRESH_MIN: Duration = Duration::from_secs(2);

/// watcher 等待任务库目录出现的超时：引擎在解压开始的最初几步内建库；超时即放弃，
/// 任务收尾（ExtractDone/Failed）会按事件自带的路径刷新，失败列表不会因此缺数据。
const EXTRACT_TASK_WATCH_TIMEOUT: Duration = Duration::from_secs(120);

/// 轮询状态目录的 tasks/ 子目录，发现快照之外的新任务目录即回传（U-10）。
/// 任务库目录由引擎在 extract worker 内部创建、路径随结果事件返回时任务已结束，
/// 运行中查看失败列表必须先发现目录：启动解压前快照既有目录，本函数只做只读
/// 文件系统访问，与分页 worker 一样不进界面线程。
fn watch_extract_task(
    tasks_root: &std::path::Path,
    known: &[PathBuf],
    generation: u64,
    sender: &mpsc::Sender<FailChannelMsg>,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(tasks_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && !known.contains(&path) {
                    let _ = sender.send(FailChannelMsg::TaskFound(path, generation));
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 发起失败列表分页取数：worker 线程开任务库查询（界面线程不开库，H-02），
/// 结果经专用通道由事件泵应用；请求代际递增，迟到的旧结果整体丢弃。
fn request_fail_page(state: &Rc<RefCell<State>>, page: usize) {
    let (task, generation, start, sender) = {
        let mut s = state.borrow_mut();
        let Some(task) = s.extract_task.clone() else {
            return;
        };
        s.fail_gen += 1;
        (
            task,
            s.fail_gen,
            s.fail_page_starts.get(page).copied().unwrap_or(0),
            s.fail_sender.clone(),
        )
    };
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            query_fail_page(&task, start, page, generation)
        }));
        let fail = result
            .unwrap_or_else(|_| Err(anyhow::anyhow!("失败列表加载时后台操作意外退出")))
            .unwrap_or_else(|error| FailPage {
                task: task.clone(),
                generation,
                page,
                items: Vec::new(),
                total: 0,
                has_more: false,
                last_id: 0,
                error: Some(format!("{error:#}")),
            });
        let _ = sender.send(FailChannelMsg::Page(fail));
    });
}

/// 查询一页失败项（U-10）：只在 worker 线程调用。隔离后位置按事件顺序归组——
/// 一个失败包的「移入解压失败」记录紧跟该包的失败记录、且先于下一个失败记录
/// （解压按包串行处理），据此把整组卷的隔离位置合并进同一项。
fn query_fail_page(task: &Path, start: i64, page: usize, generation: u64) -> Result<FailPage> {
    let db = Database::open_existing(task)?;
    let total: i64 = db.conn.query_row(
        &format!("SELECT COUNT(*) FROM events WHERE ({FAIL_ITEM_PREDICATE})"),
        [],
        |row| row.get(0),
    )?;
    // 多取一条探测「还有下一页」，随后截断到 100 行（与计划页每页 100 条同口径）。
    // 注意谓词含 OR，必须整体加括号再与 `id > ?1` 组合，否则 AND 优先级会把游标
    // 条件吞进第二个分支（回归：失败列表翻页失效、隔离位置归组为空）。
    let mut stmt = db.conn.prepare(&format!(
        "SELECT id, phase, result, source, reason FROM events WHERE ({FAIL_ITEM_PREDICATE}) AND id > ?1 ORDER BY id LIMIT 101"
    ))?;
    let query = stmt.query_map(rusqlite::params![start], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut rows = query.collect::<rusqlite::Result<Vec<_>>>()?;
    let has_more = rows.len() > 100;
    rows.truncate(100);
    // 本页末项的隔离记录上界：下一个失败项的事件 id（没有则直到库尾）。
    let upper_bound: i64 = match rows.last() {
        None => 0,
        Some((last_id, ..)) => db
            .conn
            .query_row(
                &format!("SELECT MIN(id) FROM events WHERE ({FAIL_ITEM_PREDICATE}) AND id > ?1"),
                rusqlite::params![last_id],
                |row| row.get::<_, Option<i64>>(0),
            )?
            .unwrap_or(i64::MAX),
    };
    let mut items = Vec::new();
    if !rows.is_empty() {
        let first_id = rows[0].0;
        let mut qstmt = db.conn.prepare(
            "SELECT id, target FROM events WHERE result='移入解压失败' AND id > ?1 AND id < ?2 ORDER BY id",
        )?;
        let quarantine_query = qstmt
            .query_map(rusqlite::params![first_id, upper_bound], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
        let quarantines = quarantine_query.collect::<rusqlite::Result<Vec<_>>>()?;
        for (index, (id, phase, result, source, reason)) in rows.iter().enumerate() {
            // 归组区间 (本项 id, 下一失败项 id)：页内以下一项为界，页末项用 upper_bound。
            let bound = rows
                .get(index + 1)
                .map_or(upper_bound, |(next_id, ..)| *next_id);
            let moved: Vec<&str> = quarantines
                .iter()
                .filter(|(qid, _)| *qid > *id && *qid < bound)
                .map(|(_, target)| target.as_str())
                .collect();
            let target = match moved.as_slice() {
                [] => String::new(),
                [first] => (*first).to_string(),
                [first, ..] => format!("{first}（整组共 {} 个卷已隔离）", moved.len()),
            };
            let stage = if phase == "删除" {
                "源包清理失败（X-05）"
            } else if result == "未完全解开" {
                "未完全解开"
            } else {
                "解压失败"
            };
            items.push(FailItem {
                stage,
                source: source.clone(),
                reason: reason.clone(),
                target,
            });
        }
    }
    Ok(FailPage {
        task: task.to_path_buf(),
        generation,
        page,
        total: u64::try_from(total.max(0)).unwrap_or(0),
        has_more,
        last_id: rows.last().map_or(0, |(id, ..)| *id),
        items,
        error: None,
    })
}

/// U-10：失败列表只保留「本次」解压任务的数据——启动下一次解压任务时清空上一任务的
/// 列表与分页游标并使其在途结果过期（任务结束/取消后、切换工具时不清空）。
fn reset_fail_list(ui: &AppWindow, state: &mut State) {
    state.extract_task = None;
    state.fail_page = 0;
    state.fail_page_starts = vec![0];
    state.fail_gen += 1;
    ui.set_fail_ready(false);
    ui.set_fail_rows(Rc::new(VecModel::from(Vec::<FailRow>::new())).into());
    ui.set_fail_total(0);
    ui.set_fail_prev_enabled(false);
    ui.set_fail_next_enabled(false);
    ui.set_fail_page_label("第 1 页".into());
}
/// 无头 GUI 测试用的引擎注入：把任务库隔离到 tempfile，避免污染真实状态目录。
pub struct EngineTestOverrides {
    pub state_dir: PathBuf,
}

thread_local! {
    /// 事件循环线程上登记的「立即排空事件通道」钩子（见 `wake_event_loop`）。
    /// worker 线程无法直接触碰界面状态（`Rc<RefCell<State>>` 与 `Weak<AppWindow>` 都不是
    /// `Send`），因此唤醒经由 `slint::invoke_from_event_loop` post 一个空闭包，再由本钩子在
    /// 事件循环线程上排空。钩子只在事件循环线程登记，其它线程看到 `None`，`Rc` 不跨线程。
    static EVENT_LOOP_DRAIN: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
}

/// 唤醒事件循环立刻排空事件通道：结果事件不再等最坏 100ms 的轮询（H-02 操作流畅）。
/// 事件循环未运行或已退出时 `invoke_from_event_loop` 返回 Err，忽略即可——事件仍在通道里，
/// 由既有轮询排空，因此不可能丢事件（只可能晚一点上屏）。
fn wake_event_loop() {
    let _ = slint::invoke_from_event_loop(|| {
        // 先取出钩子再调用：钩子自身会触发新的界面刷新，避免重复借用同一个 RefCell。
        let drain = EVENT_LOOP_DRAIN.with(|slot| slot.borrow().clone());
        if let Some(drain) = drain {
            drain();
        }
    });
}

/// 结果事件回传句柄：事件通道 + 事件循环唤醒。
/// 引擎侧（`Context.events`）只接受裸通道、内部实时事件（状态/日志）按既有轮询上屏；
/// 界面自己的 worker 用本句柄回传结果，发送成功即唤醒事件循环，结果上屏不再受轮询周期限制。
#[derive(Clone)]
struct EventSender {
    channel: mpsc::SyncSender<Event>,
}
impl EventSender {
    fn new(channel: mpsc::SyncSender<Event>) -> Self {
        Self { channel }
    }
    /// 通道句柄：交给引擎的 `Context.events`（界面不接管引擎事件的唤醒节奏）。
    fn channel(&self) -> mpsc::SyncSender<Event> {
        self.channel.clone()
    }
    /// 回传结果事件并立即唤醒事件循环；接收端已退出时返回 Err（调用方按既有口径忽略）。
    /// Err 装箱：事件本身最大 300+ 字节，裸 `SendError<Event>` 会让每个 `Result` 都变胖。
    fn send(&self, event: Event) -> Result<(), Box<mpsc::SendError<Event>>> {
        match self.channel.try_send(event) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Disconnected(event)) => {
                return Err(Box::new(mpsc::SendError(event)))
            }
            Err(mpsc::TrySendError::Full(event)) => {
                // 日志占满队列时先请求排空，再等待可靠结果入队，避免仍被轮询周期卡住。
                wake_event_loop();
                self.channel.send(event).map_err(Box::new)?;
            }
        }
        wake_event_loop();
        Ok(())
    }
}
struct WindowDrag {
    origin: (f64, f64),
    press: (f64, f64),
    restoring: bool,
}
/// 光标的屏幕坐标（物理像素）。拖动必须基于屏幕坐标：窗口自身移动不会改变它，因此不会出现
/// “移动窗口 → 局部坐标回跳 → 窗口被拉回去”的反馈抖动。
#[cfg(windows)]
fn pointer_position() -> Option<(f64, f64)> {
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: point 是有效的栈上 POINT；调用只向它写入光标坐标。
    (unsafe { GetCursorPos(&raw mut point) } != 0)
        .then_some((f64::from(point.x), f64::from(point.y)))
}
#[cfg(not(windows))]
fn pointer_position() -> Option<(f64, f64)> {
    None
}
/// 监视器 API 在 windows-sys 的 `Win32_Graphics_Gdi` 特性下，本 crate 未启用该特性，
/// 因此在 gui 内声明最小 FFI（user32），避免改 Cargo.toml。
#[cfg(windows)]
mod win32_monitor {
    use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
    /// MONITORINFO：字段名仅为 Rust 侧标识，ABI 布局由 `repr(C)` 的字段顺序与类型决定，
    /// 因此按 Rust 命名惯例写；Win32 的 `cbSize`/`rcMonitor`/`rcWork`/`dwFlags` 语义见各字段注释。
    #[repr(C)]
    pub(super) struct MonitorInfo {
        /// cbSize：调用方必须填入结构体字节数
        pub cb_size: u32,
        /// rcMonitor：监视器完整矩形
        pub rc_monitor: RECT,
        /// rcWork：监视器工作区矩形（排除任务栏）
        pub rc_work: RECT,
        /// dwFlags：MONITORINFOF_* 标志
        pub dw_flags: u32,
    }
    pub(super) type HMonitor = *mut core::ffi::c_void;
    /// MONITOR_DEFAULTTONEAREST：取包含点/窗口的最近监视器
    pub(super) const MONITOR_DEFAULTTONEAREST: u32 = 2;
    extern "system" {
        pub(super) fn MonitorFromWindow(hwnd: HWND, dw_flags: u32) -> HMonitor;
        pub(super) fn MonitorFromPoint(pt: POINT, dw_flags: u32) -> HMonitor;
        pub(super) fn GetMonitorInfoW(hmonitor: HMonitor, lpmi: *mut MonitorInfo) -> i32;
    }
}
/// 启动时把窗口居中：基于当前/光标所在监视器的工作区计算对称留白，
/// 满足 AGENTS「主窗口启动时居中显示在当前显示器中央」（多显示器下不固定主屏）。
#[cfg(windows)]
fn center_window(window: &slint::Window) {
    use win32_monitor::{
        GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MonitorInfo, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetForegroundWindow};
    // 优先当前前台窗口所在监视器；没有前台窗口时退到光标所在监视器。
    // SAFETY: GetForegroundWindow 只是查询前台窗口句柄，不产生所有权或别名约束。
    let foreground = unsafe { GetForegroundWindow() };
    let monitor = if foreground.is_null() {
        let mut point = POINT { x: 0, y: 0 };
        // SAFETY: point 是有效的栈上 POINT；调用只向它写入光标坐标。
        if unsafe { GetCursorPos(&raw mut point) } == 0 {
            return;
        }
        // SAFETY: point 由 GetCursorPos 刚写入；MONITOR_DEFAULTTONEAREST 只读该值。
        unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) }
    } else {
        // SAFETY: foreground 非空（上方已判空）；调用只读取该句柄。
        unsafe { MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) }
    };
    if monitor.is_null() {
        return;
    }
    // Win32 ABI 要求的 cbSize：结构体仅数十字节，饱和兜底不可能触发。
    let cb_size = u32::try_from(std::mem::size_of::<MonitorInfo>()).unwrap_or(u32::MAX);
    let mut info = MonitorInfo {
        cb_size,
        rc_monitor: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        rc_work: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        dw_flags: 0,
    };
    // 兼容部分声明布局：cb_size 必须正确
    // SAFETY: monitor 是刚取得的有效 HMONITOR；info.cb_size 已按 ABI 要求填好，
    // 调用只向 info 写入监视器信息。
    if unsafe { GetMonitorInfoW(monitor, &raw mut info) } == 0 {
        return;
    }
    let work = info.rc_work;
    let size = window.size();
    // 窗口尺寸远小于 i32 上限；饱和转换仅防御性兜底（显示用途）。
    let width = i32::try_from(size.width).unwrap_or(i32::MAX);
    let height = i32::try_from(size.height).unwrap_or(i32::MAX);
    // 在监视器工作区内居中，并夹回工作区，避免压住任务栏或跑到屏幕外
    let x = work.left + ((work.right - work.left) - width) / 2;
    let y = work.top + ((work.bottom - work.top) - height) / 2;
    let x = x.clamp(work.left, (work.right - width).max(work.left));
    let y = y.clamp(work.top, (work.bottom - height).max(work.top));
    window.set_position(slint::PhysicalPosition::new(x, y));
}
#[cfg(not(windows))]
fn center_window(_window: &slint::Window) {}
/// 读取系统“应用使用浅色主题”设置，供自绘配色在“跟随系统”时保持一致。
#[cfg(windows)]
fn system_dark() -> bool {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let name: Vec<u16> = "AppsUseLightTheme"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut value: u32 = 1;
    // DWORD 缓冲区大小恒为 4，饱和兜底不可能触发。
    let mut size = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(u32::MAX);
    // SAFETY: path/name 都是以 NUL 结尾的 UTF-16 缓冲区；value/size 是配套的
    // DWORD 输出缓冲区（RRF_RT_REG_DWORD 要求大小恰为 4），调用期间指针均有效。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut value).cast::<core::ffi::c_void>(),
            &raw mut size,
        )
    };
    status == 0 && value == 0
}
#[cfg(not(windows))]
fn system_dark() -> bool {
    false
}
/// 选项可带第三项作为该选项的说明；否则回落到规则默认 hint。
/// 用于「选中项改变后，下面那行注释文字跟着变」。
fn hint_for(spec: &RuleSpec, value: &serde_json::Value) -> String {
    if spec.kind == "choice" {
        if let Some(pos) = spec
            .choices
            .iter()
            .position(|c| Some(c[0].as_str()) == value.as_str())
        {
            if let Some(hint) = spec.choices[pos].get(2) {
                return hint.clone();
            }
        }
    }
    spec.hint.clone()
}
fn rule_row(spec: &RuleSpec, data: &serde_json::Value) -> RuleRow {
    let value = &data[&spec.key];
    let checked = value.as_bool().unwrap_or(false);
    RuleRow {
        key: spec.key.clone().into(),
        title: spec.title.clone().into(),
        hint: hint_for(spec, value).into(),
        kind: if spec.kind == "bool" {
            0
        } else if spec.kind == "choice" {
            1
        } else {
            2
        },
        checked,
        index: i32::try_from(
            spec.choices
                .iter()
                .position(|c| Some(c[0].as_str()) == value.as_str())
                .unwrap_or(0),
        )
        .unwrap_or(0),
        options: Rc::new(VecModel::from(
            spec.choices
                .iter()
                .map(|c| SharedString::from(c[1].as_str()))
                .collect::<Vec<_>>(),
        ))
        .into(),
        value: value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned)
            .into(),
    }
}
/// 解析规则数字输入。附录 D：计数/比例项只接受非负十进制整数；容量项（capacity）
/// 额外接受可选的 B/KiB/MiB/GiB/TiB 单位（不区分大小写），换算后必须为整数字节，
/// 否则按非法输入处理（保留输入并阻止开始，不自动截断）。全程整数运算，不做浮点换算。
fn parse_capacity(value: &str, capacity: bool) -> Result<u64, ()> {
    if !capacity {
        return value.parse::<u64>().map_err(|_| ());
    }
    let text = value.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let multiplier: u64 = match unit.trim().to_lowercase().as_str() {
        "" | "b" => 1,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => return Err(()),
    };
    let (int_part, frac_part) = match number.split_once('.') {
        Some((whole, frac)) => (whole, frac),
        None => (number, ""),
    };
    let digits_ok = |text: &str| text.bytes().all(|b| b.is_ascii_digit());
    if !digits_ok(int_part) || !digits_ok(frac_part) {
        return Err(());
    }
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(());
    }
    let whole: u64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| ())?
    };
    let total = whole.checked_mul(multiplier).ok_or(())?;
    if frac_part.is_empty() {
        return Ok(total);
    }
    // 小数部分：frac × 单位 ÷ 10^位数 必须整除，否则不是整数字节。
    let frac: u64 = frac_part.parse().map_err(|_| ())?;
    let digits = u32::try_from(frac_part.len()).map_err(|_| ())?;
    let denom = 10u64.checked_pow(digits).ok_or(())?;
    let scaled = frac.checked_mul(multiplier).ok_or(())?;
    if scaled % denom != 0 {
        return Err(());
    }
    total.checked_add(scaled / denom).ok_or(())
}

/// 规则行是否显示：高级层默认隐藏；联动项在依赖未开启且自身仍是默认值时不显示。
/// 已经改过值的行必须保留可见，否则依赖关掉后用户既看不到该行、也无法把它改回去。
fn rule_visible(spec: &RuleSpec, config: &Config, show_advanced: bool) -> bool {
    if !show_advanced && spec.tier == Tier::Advanced {
        return false;
    }
    match spec.key.as_str() {
        "large_threshold_bytes" => {
            config.large_files || config.large_threshold_bytes != 1024 * 1024 * 1024
        }
        // C-08：各清理类别的删除方式只在对应清理开启时才有意义；已经改过值的行保留可见，
        // 否则类别关掉后用户既看不到该行、也无法把它改回默认。
        "junk_delete" => config.clean_junk || config.junk_delete != DeleteChoice::Global,
        "temp_delete" => config.clean_temp || config.temp_delete != DeleteChoice::Global,
        "zero_delete" => config.clean_zero || config.zero_delete != DeleteChoice::Global,
        _ => true,
    }
}
fn visible_rows(state: &State) -> Result<Vec<RuleRow>> {
    let data = serde_json::to_value(&state.config)?;
    let tool = match state.tool {
        Tool::Extract => "extract",
        Tool::Organizer => "organizer",
        // MD/Git 不设规则面板（R-01）：不会进入规则表过滤，占位即可。
        Tool::Md | Tool::Git | Tool::MarkdownConverter | Tool::SnapOcr | Tool::AcpHttp => "",
    };
    Ok(state
        .specs
        .iter()
        .filter(|s| {
            s.section == state.section
                && s.tools.iter().any(|t| t == tool)
                && rule_visible(s, &state.config, state.show_advanced)
        })
        .map(|spec| {
            let mut row = rule_row(spec, &data);
            if let Some(draft) = state.invalid_rule_inputs.get(&spec.key) {
                row.value = draft.as_str().into();
            }
            row
        })
        .collect())
}
fn rule_rows(state: &State) -> Result<ModelRc<RuleRow>> {
    Ok(Rc::new(VecModel::from(visible_rows(state)?)).into())
}
fn refresh(ui: &AppWindow, state: &State) {
    if let Ok(rows) = rule_rows(state) {
        ui.set_rules(rows);
    }
    ui.set_theme(match state.config.theme.as_str() {
        "light" => 1,
        "dark" => 2,
        _ => 0,
    });
}
fn invalidate(ui: &AppWindow, tool: Tool) {
    // 运行中不得改写状态文案（C-11 如实展示）：规则/目录/工具变化在任务收尾后由
    // Done/Failed/Cancelled 事件按最终状态重算；运行期改写会把「执行中」谎报成
    // 「已结束/已就绪」。ready 在任务启动时已置 false，无需在此重复。
    if ui.get_busy() {
        return;
    }
    ui.set_ready(false);
    if tool == Tool::Extract {
        if ui.get_directory().is_empty() {
            ui.set_status("请选择需要解压的目录".into());
            return;
        }
        if !PathBuf::from(ui.get_directory().as_str()).is_dir() {
            ui.set_status("目录不存在或无法访问，请检查路径".into());
            return;
        }
        ui.set_status("目录已就绪；点「开始解压」后会先弹一次确认".into());
        return;
    }
    // U-11/C-11：MD 整理与 Git 工具没有「分析→计划→执行」流程，状态栏必须用本工具
    // 中性文案，不得串用目录整理的流程词（分析/整理/规则）误导用户。
    if matches!(tool, Tool::Md | Tool::Git) {
        if ui.get_directory().is_empty() {
            ui.set_status("请选择需要处理的目录".into());
            return;
        }
        if !PathBuf::from(ui.get_directory().as_str()).is_dir() {
            ui.set_status("目录不存在或无法访问，请检查路径".into());
            return;
        }
        ui.set_status("目录已就绪".into());
        return;
    }
    if tool == Tool::MarkdownConverter {
        ui.set_status("转 Markdown 可在页面中初始化组件并开始转换".into());
        return;
    }
    if tool == Tool::SnapOcr {
        // O-01：截图 OCR 无目录输入与规则面板，状态栏用本工具中性文案。
        ui.set_status("截图 OCR 可在页面中初始化组件并管理后台服务".into());
        return;
    }
    if tool == Tool::AcpHttp {
        ui.set_status("模型服务在独立后台运行，关闭窗口不停止服务".into());
        return;
    }
    if ui.get_has_task() {
        ui.set_status("规则或目录已改变，请重新分析后再执行".into());
        return;
    }
    if ui.get_directory().is_empty() {
        ui.set_status("请选择需要整理的目录".into());
        return;
    }
    // 手输路径打错时立刻反馈，不要一路绿灯到确认框才由引擎报“无法访问目标目录”。
    if !PathBuf::from(ui.get_directory().as_str()).is_dir() {
        ui.set_status("目录不存在或无法访问，请检查路径".into());
        return;
    }
    ui.set_status("目录已就绪；「分析」只读不改文件，随时可以开始".into());
}
/// 目录输入变化与切换工具共用的就绪同步：已有任务时按「任务 + 配置 + 目录」在后台重算
/// （开任务库与 canonicalize 都在 worker 线程，见 `load_plan_state`），而不是一律 invalidate
/// ——用户可能只是重新输入了同一路径或切去看了一眼另一个工具，不应清掉仍可执行的计划。
/// 结果落地时按请求代际与「当前目录/配置」筛选，并给真实状态文案（C-11 如实展示）。
fn sync_ready_after_directory(ui: &AppWindow, state: &State, out: &EventSender) {
    // 同 invalidate 的 busy 门禁：运行中的状态栏归任务事件管，目录输入与工具切换
    // 不得触发就绪重算把执行中任务改写为「已结束/可执行」。
    if ui.get_busy() {
        return;
    }
    if ui.get_has_task() && state.tool == Tool::Organizer {
        if let Some(task) = state.task.clone() {
            ui.set_ready(false);
            state.readiness_status_pending.set(true);
            load_plan_state(
                out,
                &state.plan_load,
                task,
                ui.get_directory().to_string(),
                PlanQuery::readiness(),
            );
            return;
        }
    }
    invalidate(ui, state.tool);
}
fn apply_theme(ui: &AppWindow, state: &State) {
    ui.set_theme(match state.config.theme.as_str() {
        "light" => 1,
        "dark" => 2,
        _ => 0,
    });
}
/// 只改单行显示，避免整表重建打断 ComboBox/CheckBox（用户点选后文字停在旧值的根因）。
fn patch_rule_row(ui: &AppWindow, key: &str, mutate: impl Fn(&mut RuleRow)) {
    let rules = ui.get_rules();
    let Some(model) = rules.as_any().downcast_ref::<VecModel<RuleRow>>() else {
        return;
    };
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            if row.key.as_str() == key {
                mutate(&mut row);
                model.set_row_data(i, row);
                break;
            }
        }
    }
}
/// 只插入/删除可见性变化的行，不重建整表：整表重建会销毁 ListView 里的控件，
/// 在回调中执行会让刚点过的 ComboBox 文字停在旧值（见 on_rule_bool 上的原注释）。
fn sync_rules(ui: &AppWindow, state: &State) {
    let Ok(desired) = visible_rows(state) else {
        return;
    };
    let rules = ui.get_rules();
    let Some(model) = rules.as_any().downcast_ref::<VecModel<RuleRow>>() else {
        return;
    };
    let mut index = 0usize;
    for row in desired {
        loop {
            match model.row_data(index) {
                None => {
                    model.push(row.clone());
                    break;
                }
                Some(current) if current.key == row.key => break,
                Some(_) => {
                    if model
                        .row_data(index + 1)
                        .is_some_and(|next| next.key == row.key)
                    {
                        model.remove(index);
                        continue;
                    }
                    model.insert(index, row.clone());
                    break;
                }
            }
        }
        index += 1;
    }
    while model.row_count() > index {
        model.remove(index);
    }
}
/// 合并行的影子键：界面上一个开关同时驱动多个配置键，Config 保留细粒度字段
/// 供引擎与历史任务库继续使用，只是界面不再拆开展示。
fn group_keys(key: &str) -> Vec<&str> {
    match key {
        "fix_extension" => vec!["fix_extension", "detect_type"],
        _ => vec![key],
    }
}
fn changed(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    key: &str,
    value: &serde_json::Value,
    rebuild: bool,
) -> bool {
    let tool = state.borrow().tool;
    let mut state = state.borrow_mut();
    let result = (|| {
        for target in group_keys(key) {
            state.config.set_json(target, value.clone())?;
        }
        Ok::<_, anyhow::Error>(())
    })();
    match result {
        Ok(()) => {
            // 立刻反馈规则之间的依赖（例如“修正扩展名”需要先开“检测真实类型”），
            // 不要让用户等到点「开始分析 / 开始解压」才知道配置不成立。
            let validation = match tool {
                Tool::Organizer => state.config.validate_organizer(),
                Tool::Extract => state.config.validate_extract(),
                // 主题设置不属于任一工具的规则面板。
                Tool::Md | Tool::Git | Tool::MarkdownConverter | Tool::SnapOcr | Tool::AcpHttp => {
                    Ok(())
                }
            };
            let validation = invalid_number_error(&state, tool)
                .map_or(validation, |error| Err(anyhow::anyhow!(error)));
            match validation {
                Ok(()) => ui.set_error_text("".into()),
                Err(error) => ui.set_error_text(format!("{error:#}").into()),
            }
            // theme 是纯外观设置，不影响计划内容：不使已生成的计划失效。
            if key != "theme" {
                refresh_scope_notice(ui, &state.config);
                invalidate(ui, tool);
            }
            if rebuild {
                refresh(ui, &state);
            } else if key == "theme" {
                apply_theme(ui, &state);
            }
            true
        }
        Err(error) => {
            ui.set_error_text(format!("{error:#}").into());
            false
        }
    }
}
/// Event::Failed 收尾：任务库仍在时重载摘要与计划页，避免界面停留在失败前的旧数据。
/// 摘要与计划页取自同一次后台开库（`PlanQuery::page(..., summary=true)`），界面线程不开库
/// （H-02：失败收尾也要立刻画出状态，不能被磁盘/网络盘卡住）。
/// 注意 task 必须先用普通 let 从 state 提取：edition 2021 下 if-let scrutinee 的
/// borrow() Ref 临时存活到整个语句结束，块内 borrow_mut 会 BorrowMutError panic
/// ——任务取消/失败且有任务时 100% 触发（回归见 gui_tests）。
fn reload_after_failed(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    // task 先用普通 let 提取后再 if-let：scrutinee 里的 borrow() Ref 不会存活到块内，
    // 块内的 borrow_mut 与共享借用才安全（缺陷模式见回归测试 failed_reload_*）。
    let task = state.borrow().task.clone();
    if let Some(task) = task {
        let (start, page, filter, plan_load) = {
            let s = state.borrow();
            (
                s.page_starts.get(s.page).copied().unwrap_or(0),
                s.page,
                s.plan_filter.clone(),
                Arc::clone(&s.plan_load),
            )
        };
        load_plan_state(
            out,
            &plan_load,
            task,
            ui.get_directory().to_string(),
            PlanQuery::page(start, page, filter, true),
        );
    }
}

/// 计划页请求参数：起始 action id、页码与筛选（`None`=全部）。
struct PlanPageRequest {
    start: i64,
    page: usize,
    filter: Option<String>,
}

/// 计划 worker 的取数口径：页面、就绪快照与摘要都取自同一次开库（界面线程不开库，H-02）。
struct PlanQuery {
    /// `None`=只取就绪快照，不重载计划页（切工具、目录编辑、勾选保存后重算就绪）
    page: Option<PlanPageRequest>,
    /// 连摘要一起取：失败收尾复用同一次开库，界面线程不再自己开库读摘要
    summary: bool,
}
impl PlanQuery {
    /// 只取就绪快照，不重载计划页。
    fn readiness() -> Self {
        Self {
            page: None,
            summary: false,
        }
    }
    /// 取指定页 + 就绪快照；`summary` 为真时同一次开库带回摘要（失败收尾用）。
    fn page(start: i64, page: usize, filter: Option<String>, summary: bool) -> Self {
        Self {
            page: Some(PlanPageRequest {
                start,
                page,
                filter,
            }),
            summary,
        }
    }
}

/// worker 取数结果：页面请求带回页面，只取快照的请求只带回快照；两者都带就绪快照。
enum PlanLoaded {
    Page(PlanSnapshot, Vec<crate::model::Action>),
    State(PlanSnapshot),
}

/// 发起计划页/就绪快照加载：worker 线程开库取数，界面线程只等结果。
/// 目录原文在界面线程读一次随请求带走：快照落地时界面据此判断是否仍对应当前输入，
/// 因此界面线程既不打开任务库，也不做 canonicalize（H-02：网络盘与大目录不卡界面）。
fn load_plan_state(
    out: &EventSender,
    plan_load: &Arc<PlanLoadSync>,
    path: PathBuf,
    directory: String,
    query: PlanQuery,
) {
    let PlanQuery { page, summary } = query;
    let (page_start, page_index, page_filter) = match page {
        Some(request) => (Some(request.start), request.page, request.filter),
        None => (None, 0, None),
    };
    // 两类请求各自递增代际：同一会话里筛选/翻页/工具切换会并发发起多次请求，
    // 晚到的低代际结果不得覆盖当前视图。只取快照的请求不动页面代际（否则会把在途的
    // 页面结果误判为过期，列表永远填不上）。page_gen 仅页面请求分支使用。
    let state_gen = plan_load.state.fetch_add(1, Ordering::AcqRel) + 1;
    let page_gen = page_start
        .map(|_| plan_load.page.fetch_add(1, Ordering::AcqRel) + 1)
        .unwrap_or_default();
    let out = out.clone();
    // 打开的是引擎已生成的任务库：缺失时报错，不静默新建空库。
    std::thread::spawn(move || {
        // 与 async_work 一致地拦截 panic：否则失败事件缺失会让 UI 停在「加载中…」。
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let db = Database::open_existing(&path)?;
            let snapshot = plan_snapshot(&db, &directory, summary);
            match page_start {
                Some(start) => db
                    .actions_page_filtered(start, 101, page_filter.as_deref())
                    .map(|actions| PlanLoaded::Page(snapshot, actions)),
                None => Ok(PlanLoaded::State(snapshot)),
            }
        }))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("计划加载时后台操作意外退出；请重试")));
        let event = match result {
            Ok(PlanLoaded::Page(snapshot, actions)) => Event::PlanPage(
                path,
                actions,
                page_index,
                page_gen,
                page_filter,
                state_gen,
                Some(snapshot),
            ),
            Ok(PlanLoaded::State(snapshot)) => Event::PlanState(path, state_gen, Some(snapshot)),
            Err(error) => {
                if page_start.is_some() {
                    if page_index == 0 {
                        // 筛选切换/任务加载（都从第 0 页开始）失败必须回空页：否则列表残留
                        // 上一筛选甚至上一任务的行，与已切换的胶囊不一致。快照一并缺失：
                        // 界面按不可执行处理（fail-closed），原因由下面的红条给出。
                        let _ = out.send(Event::PlanPage(
                            path.clone(),
                            Vec::new(),
                            page_index,
                            page_gen,
                            page_filter.clone(),
                            state_gen,
                            None,
                        ));
                    }
                    // 失败提示带代际/筛选归属：过期请求（用户已切走筛选/翻页）的失败
                    // 不得把红条误报到当前正确视图上，UI 侧按 gen/filter 决定是否上屏。
                    let _ = out.send(Event::PlanLoadFailed(
                        format!("{error:#}"),
                        page_gen,
                        page_filter,
                    ));
                } else {
                    // 只取就绪快照的请求失败与旧口径一致：不弹红条，按不可执行处理
                    // （状态栏文案仍按就绪状态说明当前无法执行）。
                    let _ = out.send(Event::PlanState(path, state_gen, None));
                }
                return;
            }
        };
        let _ = out.send(event);
    });
}

/// 任务库快照：一次开库读出就绪判定与（可选）摘要所需的全部字段。
/// `canonicalize` 与配置序列化都在 worker 线程完成，界面线程只拿纯数据比较（H-02）。
fn plan_snapshot(db: &Database, directory: &str, with_summary: bool) -> PlanSnapshot {
    // theme 是纯外观设置，不改变计划内容，比较时剔除（界面侧同样剔除后再比较）。
    let config_json = db.config().ok().and_then(|config| {
        let mut value = serde_json::to_value(config).ok()?;
        value.as_object_mut()?.remove("theme");
        Some(value)
    });
    PlanSnapshot {
        status: db.get::<String>("status").ok(),
        config_json,
        root_matches: db.get::<String>("root").is_ok_and(|root| {
            std::fs::canonicalize(Path::new(directory))
                .is_ok_and(|selected| selected == Path::new(&root))
        }),
        requested_directory: directory.to_string(),
        summary: if with_summary {
            db.summary().ok()
        } else {
            None
        },
    }
}

/// 计划就绪四态：Ready=任务在就绪态且配置/目录一致；Changed=规则或目录与计划不一致；
/// Finished=任务已执行/取消/失败等非就绪态；Unavailable=任务库读不到（快照缺失）。
/// 四态决定界面文案与按钮/勾选可用性（C-11 如实展示）。
#[derive(Clone, Copy)]
enum PlanReadyState {
    Ready,
    Changed,
    Finished,
    Unavailable,
}
impl PlanReadyState {
    /// 状态栏文案：不谎报改动，也不把读不到库说成任务已结束。
    fn status_text(self) -> &'static str {
        match self {
            PlanReadyState::Ready => "目录与当前计划一致，可以确认执行",
            PlanReadyState::Changed => "规则或目录已改变，请重新分析后再执行",
            PlanReadyState::Finished => "上次任务已结束；如需再次整理请重新分析",
            PlanReadyState::Unavailable => "无法读取任务库；请重新分析后再执行",
        }
    }
    /// 勾选是否可编辑：只有已结束或读不到库才禁用——这两种情况的勾选必然写不进任务库
    /// （fail-closed：库不可访问时不允许编辑）。
    fn editable(self) -> bool {
        matches!(self, PlanReadyState::Ready | PlanReadyState::Changed)
    }
}

/// 由「worker 快照 + 当前界面配置/目录」判定就绪状态：纯比较，不开库、不做 canonicalize（H-02）。
/// 快照对应的目录输入已不是当前输入、或库内配置/根目录与当前不一致时按「已改变」处理
/// （fail-closed：宁可按未就绪显示，也不凭陈旧数据放行执行）。
fn classify_plan_snapshot(
    snapshot: Option<&PlanSnapshot>,
    directory: &str,
    config: &Config,
) -> PlanReadyState {
    let Some(snapshot) = snapshot else {
        // 任务库不可访问按不可执行处理：目录可能被移动或删除，需重新分析。
        return PlanReadyState::Unavailable;
    };
    // 快照描述的是发起请求时的目录输入：用户之后改了目录（去抖重算尚未返回）时不得用它放行。
    if snapshot.requested_directory != directory {
        return PlanReadyState::Changed;
    }
    // 任务状态是第一道闸；缺失状态不能冒称任务已结束，也不能重新点亮 ready。
    let Some(status) = snapshot.status.as_deref() else {
        return PlanReadyState::Unavailable;
    };
    if status != "ready" {
        return PlanReadyState::Finished;
    }
    let Some(db_config) = snapshot.config_json.as_ref() else {
        return PlanReadyState::Changed;
    };
    let Ok(mut current) = serde_json::to_value(config) else {
        return PlanReadyState::Changed;
    };
    if let Some(object) = current.as_object_mut() {
        object.remove("theme");
    }
    if *db_config == current && snapshot.root_matches {
        PlanReadyState::Ready
    } else {
        PlanReadyState::Changed
    }
}

/// 应用最新有效快照；目录/工具变化的状态反馈由界面持有，任一最新快照都能完成反馈。
fn apply_plan_readiness(ui: &AppWindow, state: &State, snapshot: Option<&PlanSnapshot>) {
    let outcome = classify_plan_snapshot(snapshot, ui.get_directory().as_str(), &state.config);
    ui.set_plan_editable(outcome.editable());
    ui.set_ready(
        matches!(outcome, PlanReadyState::Ready)
            && invalid_number_error(state, Tool::Organizer).is_none(),
    );
    if state.tool == Tool::Organizer && state.readiness_status_pending.replace(false) {
        ui.set_status(outcome.status_text().into());
    }
}
/// 附录 D：仅当前工具实际启用的数字字段参与门禁。
fn invalid_number_error(state: &State, tool: Tool) -> Option<String> {
    let owner = match tool {
        Tool::Extract => "extract",
        Tool::Organizer => "organizer",
        Tool::Md | Tool::Git | Tool::MarkdownConverter | Tool::SnapOcr | Tool::AcpHttp => {
            return None
        }
    };
    state.specs.iter().find_map(|spec| {
        if !state.invalid_rule_inputs.contains_key(&spec.key)
            || !spec.tools.iter().any(|item| item == owner)
            || (spec.key == "large_threshold_bytes" && !state.config.large_files)
        {
            return None;
        }
        Some(format!(
            "{}：{}",
            spec.title,
            if spec.unit.as_deref() == Some("bytes") {
                "需要非负数字，可选单位 B/KiB/MiB/GiB/TiB（换算后须为整数字节且不超出允许范围）"
            } else {
                "需要输入非负整数，且不得超出允许范围"
            }
        ))
    })
}
fn retain_invalid_number(ui: &AppWindow, state: &Rc<RefCell<State>>, key: &str, value: &str) {
    let (tool, error) = {
        let mut s = state.borrow_mut();
        s.invalid_rule_inputs
            .insert(key.to_string(), value.to_string());
        (s.tool, invalid_number_error(&s, s.tool))
    };
    patch_rule_row(ui, key, |row| row.value = value.into());
    invalidate(ui, tool);
    ui.set_error_text(error.unwrap_or_default().into());
}
/// 计划页事件是否可应用：代际须仍是 latest，且 filter 与当前视图一致。
/// page 不再要求与 UI 预置值一致：翻页采用「先加载、成功再提交」，加载期间 state.page 仍是旧页。
fn plan_page_event_accepted(
    event_gen: u64,
    latest: u64,
    event_filter: Option<&str>,
    ui_filter: Option<&str>,
) -> bool {
    event_gen == latest && event_filter == ui_filter
}
/// 执行阶段的进度分母：只统计仍勾选且待执行（selected=1 且 state='pending'）的计划项，
/// 用户取消勾选的项不计入。计数失败时返回 Err，调用方保留全量分母继续执行。
fn count_selected_pending(task: &Path) -> Result<u64> {
    let db = Database::open_existing(task)?;
    let n: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM actions WHERE selected=1 AND state='pending'",
        [],
        |r| r.get(0),
    )?;
    // COUNT(*) 恒非负，max(0) 仅防御损坏库；转换在 64 位平台无损。
    Ok(u64::try_from(n.max(0)).unwrap_or(0))
}
/// 引擎侧后台线程公共骨架：body 产出终态事件；panic 以固定文案转
/// [`Event::Failed`]，引擎错误转 `{error:#}` 文本。发送失败静默放弃
/// （界面事件循环已退出时无处投递）。清点等事件形状不同的站点不适用本骨架。
fn spawn_event_worker(
    out: &EventSender,
    panic_text: &'static str,
    body: impl FnOnce() -> anyhow::Result<Event> + Send + 'static,
) {
    let out = out.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        let event = match result {
            Ok(Ok(event)) => event,
            Ok(Err(error)) => Event::Failed(format!("{error:#}")),
            Err(_) => Event::Failed(panic_text.to_string()),
        };
        let _ = out.send(event);
    });
}
fn start_task(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender, apply: bool) {
    if ui.get_busy() {
        return;
    }
    if state.borrow().pending_selection != 0 || state.borrow().plan_recompute_inflight != 0 {
        ui.set_error_text("计划勾选或依赖重算仍在进行，请稍后再试".into());
        return;
    }
    if apply && state.borrow().plan_recompute_failed {
        ui.set_error_text("计划依赖重算失败，请重新分析后再执行".into());
        return;
    }
    if let Some(error) = invalid_number_error(&state.borrow(), state.borrow().tool) {
        show_error(ui, error);
        return;
    }
    let (configuration, task, tool) = {
        let s = state.borrow();
        (s.config.clone(), s.task.clone(), s.tool)
    };
    let validation = match tool {
        Tool::Organizer => configuration.validate_organizer(),
        Tool::Extract => configuration.validate_extract(),
        Tool::Md | Tool::Git | Tool::MarkdownConverter | Tool::SnapOcr | Tool::AcpHttp => Ok(()),
    };
    if let Err(error) = validation {
        ui.set_error_text(format!("{error:#}").into());
        return;
    }
    let directory = PathBuf::from(ui.get_directory().as_str());
    if apply && task.is_none() {
        ui.set_error_text("还没有可以执行的计划".into());
        return;
    }
    if !apply && !directory.is_dir() {
        ui.set_error_text("目标目录不存在或无法访问，请重新选择目录".into());
        return;
    }
    // S-04：所选根或其上级含符号链接/junction 时在这里就拒绝（与引擎入口同口径），
    // 不进入扫描线程。
    if !apply {
        if let Err(error) = crate::fsutil::ensure_plain_entry(&directory) {
            ui.set_error_text(format!("{error:#}").into());
            return;
        }
    }
    let control = Arc::new(Control::default());
    {
        let mut s = state.borrow_mut();
        s.control = Some(control.clone());
        s.started = Instant::now();
        s.close_after = false;
        s.selection_failed = false;
        s.plan_recompute_inflight = 0;
        s.plan_recompute_failed = false;
        s.plan_recompute_dirty = false;
        s.readiness_status_pending.set(false);
        s.logs.clear();
        s.page = 0;
        s.page_starts = vec![0];
        s.applying = apply;
        s.extracting = false;
        s.runtime = RuntimeMode::Organizer;
    }
    // 先把运行态上屏，再让 worker 做磁盘工作：busy/状态文案必须早于任何等待 I/O 的操作
    // 画出来（H-02：点确认后界面立即有反馈，项目计数不再拖住首帧）。
    ui.set_busy(true);
    ui.set_ready(false);
    ui.set_paused(false);
    ui.set_error_text("".into());
    // 分析（新任务）清掉上一任务的提示；执行是同一任务的收尾阶段，必须保留分析期产生的
    // 提示（H-06 Git 目录处置结果等），否则用户在整个执行阶段都看不到该说明。
    if !apply {
        ui.set_notice_text("".into());
    }
    ui.set_panel(2);
    ui.set_log_text("".into());
    ui.set_status(
        if apply {
            "正在执行已确认的整理计划"
        } else {
            "准备扫描与分析；分析阶段只读，不会改动任何文件"
        }
        .into(),
    );
    // 目录整理不调用解压、不询问解压冲突（C-01/H-03）：引擎只需任务控制与事件通道。
    // 引擎侧只接受裸通道（接口不变）；结果事件由本 worker 用 `out` 回传并唤醒事件循环。
    let context = Context {
        control: control.clone(),
        events: Some(out.channel()),
    };
    // 同一次 GUI 会话里可能先分析再执行：覆盖对象必须可重复使用，不能 take。
    let overrides = state
        .borrow()
        .engine_overrides
        .as_ref()
        .map(|o| EngineTestOverrides {
            state_dir: o.state_dir.clone(),
        });
    spawn_event_worker(
        out,
        "整理线程意外退出；未执行的步骤不会继续",
        move || {
            if apply {
                // apply 分支在函数入口已校验任务库存在；工作线程内不使用 unwrap，
                // 若状态被并发改动则按错误返回，交给事件循环统一呈现。
                let Some(task_path) = task.as_deref() else {
                    anyhow::bail!("内部错误：执行阶段任务库缺失");
                };
                // 执行分母按将实际执行的勾选数修正（U-03）：规划期 planned 是全量，用户可能已
                // 取消部分勾选。计数在 worker 线程完成并写入本次运行的 control——界面线程不为此
                // 打开任务库（H-02），计数失败则保留全量分母，不阻断执行（分母可能偏大但仍收敛）。
                if let Ok(n) = count_selected_pending(task_path) {
                    control.set_planned(n);
                }
                engine::apply(task_path, context)
                    .map(|result| Event::Done(result.directory, result.summary))
            } else {
                let prepare = match &overrides {
                    // 测试注入只需隔离任务库状态目录（回收站注入随 S-02 一并移除）。
                    Some(o) => engine::prepare_at(&directory, configuration, context, &o.state_dir),
                    None => engine::prepare(&directory, configuration, context),
                };
                prepare.map(|result| Event::Ready(result.directory, result.summary))
            }
        },
    );
}
/// 「递归解压」一段式启动（X-02）：一段确认后连续执行到结束；无计划审核环节，
/// 结束事件 ExtractDone 只收尾摘要，不改 ready/has_task（那是目录整理两段式的状态）。
fn start_extract(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    if ui.get_busy() {
        return;
    }
    if let Some(error) = invalid_number_error(&state.borrow(), Tool::Extract) {
        show_error(ui, error);
        return;
    }
    let configuration = {
        let s = state.borrow();
        s.config.clone()
    };
    if let Err(error) = configuration.validate_extract() {
        ui.set_error_text(format!("{error:#}").into());
        return;
    }
    let directory = PathBuf::from(ui.get_directory().as_str());
    if !directory.is_dir() {
        ui.set_error_text("目标目录不存在或无法访问，请重新选择目录".into());
        return;
    }
    let control = Arc::new(Control::default());
    {
        let mut s = state.borrow_mut();
        s.control = Some(control.clone());
        s.started = Instant::now();
        s.close_after = false;
        s.logs.clear();
        s.applying = false;
        s.extracting = true;
        s.runtime = RuntimeMode::Extract;
        s.planned = 0;
    }
    ui.set_busy(true);
    ui.set_ready(false);
    ui.set_paused(false);
    ui.set_error_text("".into());
    ui.set_notice_text("".into());
    ui.set_panel(1);
    ui.set_quarantined_count(0);
    ui.set_log_text("".into());
    // U-10：新解压任务开始——清空上一任务的失败列表与分页游标（在途分页结果一并作废）。
    reset_fail_list(ui, &mut state.borrow_mut());
    // X-05/S-02：成功删原包是破坏性行为，运行期状态栏必须如实说明删除条件与保留条件。
    ui.set_status(
        "正在解压；完整成功解出的原压缩包及分卷会永久删除（不经回收站，不可恢复），失败、未完全解开或取消的包保留，无法完全解开的包移入「解压失败」并记录原因".into(),
    );
    // X-05/X-06/H-07：解压只按成功条件删原包，不删除无关文件、不覆盖、不询问冲突；
    // 引擎侧只需任务控制与事件通道（裸通道接口不变，结果事件由本 worker 用 `out` 回传）。
    let context = Context {
        control: control.clone(),
        events: Some(out.channel()),
    };
    let overrides = state
        .borrow()
        .engine_overrides
        .as_ref()
        .map(|o| EngineTestOverrides {
            state_dir: o.state_dir.clone(),
        });
    spawn_event_worker(
        out,
        "解压线程意外退出；未执行的步骤不会继续",
        move || {
            let result = match overrides {
                Some(overrides) => engine::extract_run_at(
                    &directory,
                    configuration,
                    context,
                    &overrides.state_dir,
                    None,
                ),
                None => engine::extract_run(&directory, configuration, context),
            };
            result.map(|result| Event::ExtractDone(result.directory, result.summary))
        },
    );
    // U-10：失败列表的数据源是本次任务的任务库，而任务库目录由引擎在状态目录下创建、
    // 路径随结果事件返回时任务已结束。启动前快照既有任务目录，watcher 在后台轮询新
    // 目录并回传路径（只读访问，不进界面线程）；失败列表由此在运行中即可查看。
    let state_root = state.borrow().engine_overrides.as_ref().map_or_else(
        || crate::config::state_dir().ok(),
        |o| Some(o.state_dir.clone()),
    );
    if let Some(state_root) = state_root {
        let tasks_root = state_root.join("tasks");
        let known = std::fs::read_dir(&tasks_root)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .collect::<Vec<PathBuf>>()
            })
            .unwrap_or_default();
        let generation = state.borrow().extract_generation;
        let fail_sender = state.borrow().fail_sender.clone();
        std::thread::spawn(move || {
            watch_extract_task(
                &tasks_root,
                &known,
                generation,
                &fail_sender,
                EXTRACT_TASK_WATCH_TIMEOUT,
            );
        });
    }
}
/// 「开始解压」确认框文案（X-02，S-02 的破坏性告知）：数量由后台清点（大目录不阻塞界面），
/// 清点完成前先给占位文案，事件到达后原位更新。文案固定说明去向、完整成功即永久删除原包及
/// 分卷（不可恢复）、失败/未完全解开/取消时保留、已有文件保留、冲突只为新文件自动改名且保持
/// 扩展名、失败包去向与原因可查；不提供删除方式或冲突策略选项。
fn extract_confirm_text(count_text: &str, directory: &str) -> String {
    format!(
        "目标目录：{directory}

将递归解压压缩包（含本次解出的嵌套压缩包），就地解到各包所在位置。
{count_text}
每个包完整解开（全部成员落盘）后，原压缩包及该包的分卷会永久删除，不经回收站、不可恢复；失败、未完全解开或取消的包一律保留。
已有文件一律保留；目标同名时只为新解出的文件自动改文件名（如 资料 (1).txt），扩展名与复合扩展名保持不变。
无法完全解开的包保留并移入「解压失败」子目录，原因记录在日志中；处理期间可随时取消。"
    )
}
fn show_error(ui: &AppWindow, error: impl std::fmt::Display) {
    ui.set_error_text(error.to_string().into());
}
/// 目录选择起点策略；各页面传入自己的当前字段，不共享输入状态。
fn directory_dialog_start(entered: &str) -> Option<PathBuf> {
    let entered = PathBuf::from(entered);
    entered.is_dir().then_some(entered)
}
fn pick_directory(
    ui: &AppWindow,
    title: &str,
    entered: &str,
    apply: impl FnOnce(&AppWindow, PathBuf),
) {
    let mut dialog = rfd::FileDialog::new().set_title(title);
    if let Some(start) = directory_dialog_start(entered) {
        dialog = dialog.set_directory(start);
    }
    if let Some(path) = dialog.pick_folder() {
        apply(ui, path);
    }
}
/// M-08：解析拆分大小输入——正数十进制（可含小数），按 KB（1024）/ MB（1024×1024）
/// 换算后必须为整数字节；非法输入报字段错误，不自动截断或钳制。
fn parse_size_bytes(text: &str, unit_index: i32) -> Result<u64, String> {
    let multiplier: u64 = if unit_index == 1 { 1024 * 1024 } else { 1024 };
    let trimmed = text.trim();
    let (whole, frac) = trimmed
        .split_once('.')
        .map_or((trimmed, ""), |(w, f)| (w, f));
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() && frac.is_empty() {
        return Err("请输入大于 0 的数字（可用小数，换算后须为整数字节）".into());
    }
    if !digits(whole) || !digits(frac) {
        return Err("大小只接受非负十进制数字".into());
    }
    let whole_value: u64 = whole
        .parse()
        .map_err(|_| "数值超出允许的范围".to_string())?;
    let base = whole_value
        .checked_mul(multiplier)
        .ok_or_else(|| "数值超出允许的范围".to_string())?;
    if frac.is_empty() {
        if base == 0 {
            return Err("拆分大小必须大于 0".into());
        }
        return Ok(base);
    }
    if frac.len() > 9 {
        return Err("小数位数过多，换算后不是整数字节".into());
    }
    let frac_value: u64 = frac.parse().map_err(|_| "数值超出允许的范围".to_string())?;
    let digits_count = u32::try_from(frac.len()).unwrap_or(u32::MAX);
    let denom = 10u64
        .checked_pow(digits_count)
        .ok_or_else(|| "数值超出允许的范围".to_string())?;
    let scaled = frac_value
        .checked_mul(multiplier)
        .ok_or_else(|| "数值超出允许的范围".to_string())?;
    if scaled % denom != 0 {
        return Err("换算后不是整数字节（如 0.1 KB = 102.4 字节）".into());
    }
    // 合法性判定看换算后的总量：整数部分为 0 的小数（如 0.5 MB）在汇总后才见真值，
    // 提前判 base==0 会把合法小数输入误拒（回归见 split_size_accepts_integral_fractions）。
    let total = base
        .checked_add(scaled / denom)
        .ok_or_else(|| "数值超出允许的范围".to_string())?;
    if total == 0 {
        return Err("拆分大小必须大于 0".into());
    }
    Ok(total)
}

/// MD 整理合并输出文件名（M-07）：去掉首尾空白，不含路径分隔符，自动补 `.md` 后缀。
fn normalize_output_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("请填写输出文件名".into());
    }
    if name.contains('/') || name.contains('\\') || name.contains(':') {
        return Err("输出文件名不能包含路径分隔符".into());
    }
    if crate::fsutil::validate_component(name).is_err() {
        return Err(format!("输出文件名「{name}」不是合法的 Windows 文件名"));
    }
    if name.to_ascii_lowercase().ends_with(".md") {
        Ok(name.to_owned())
    } else {
        Ok(format!("{name}.md"))
    }
}

/// MD 合并任务的公共运行段：扫描（排除输出自身）→（按 overwrite）合并（M-02~M-07）。
/// 供首次启动（overwrite=false）与覆盖确认后的重跑共用；文件集合在执行时重新扫描。
/// MD 合并/拆分共用的进度回调工厂：FileStarted 更新分母并上屏状态文案，
/// FileCompleted 更新分母与完成计数（U-03/U-12：完成计数只由 FileCompleted 驱动，
/// 单文件任务开局不得显示 1/1）。两处的计数口径只有这一份实现。
fn md_progress_sink<'a>(
    control: &'a Arc<Control>,
    out: &'a EventSender,
    status_text: impl Fn(usize, usize) -> String + 'a,
) -> impl Fn(md_tools::MdProgress) -> anyhow::Result<()> + 'a {
    let out = out.clone();
    move |event| {
        match event {
            md_tools::MdProgress::FileStarted(index, total) => {
                control.set_planned(u64::try_from(total).unwrap_or(0));
                let _ = out.send(Event::Status(status_text(index, total)));
            }
            md_tools::MdProgress::FileCompleted(done, total) => {
                control.set_planned(u64::try_from(total).unwrap_or(0));
                control
                    .completed
                    .store(u64::try_from(done).unwrap_or(0), Ordering::Relaxed);
            }
        }
        Ok(())
    }
}
fn md_merge_run(
    root: &Path,
    recursive: bool,
    output: &Path,
    overwrite: bool,
    control: &Arc<Control>,
    out: &EventSender,
) -> Result<String, String> {
    let entries = md_tools::scan_markdown(root, recursive, Some(output))
        .map_err(|error| format!("{error:#}"))?;
    let _ = out.send(Event::Log(format!(
        "扫描完成：找到 {} 个 .md 文件（{}），输出：{}",
        entries.len(),
        if recursive { "递归" } else { "不递归" },
        output.display()
    )));
    if entries.is_empty() {
        return Ok(format!(
            "所选范围内没有 .md 文件，未生成输出：{}",
            root.display()
        ));
    }
    // M-07：输出已存在 → 交由界面确认（MD_CONFLICT 前缀触发确认框），确认后重跑覆盖
    if !overwrite && output.exists() {
        return Err(format!(
            "MD_CONFLICT 输出文件已存在：{}；确认后将覆盖原内容（不可恢复），返回检查则不做任何写入",
            output.display()
        ));
    }
    let stats = md_tools::merge_markdown_with_events(
        &entries,
        output,
        overwrite,
        control,
        &md_progress_sink(control, out, |index, total| {
            format!("合并中：{index} / {total} 个文件")
        }),
    )
    .map_err(|error| format!("{error:#}"))?;
    Ok(format!(
        "合并完成：{} 个文件按创建时间顺序写入 {}",
        stats.files,
        output.display()
    ))
}

/// MD 拆分任务的公共运行段：规划（UTF-8 安全边界）→ 冲突检测 →（按 overwrite）写出
/// （M-08~M-11）。供首次启动与覆盖确认后的重跑共用。
fn md_split_run(
    input: &Path,
    limit: u64,
    out_dir: &Path,
    overwrite: bool,
    control: &Arc<Control>,
    out: &EventSender,
) -> Result<String, String> {
    let plan = md_tools::plan_splits(input, limit).map_err(|error| format!("{error:#}"))?;
    if plan.bounds.is_empty() {
        return Ok(format!(
            "输入文件为空（0 字节），没有可拆分的内容：{}",
            input.display()
        ));
    }
    let names = md_tools::split_names(
        &input
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        plan.bounds.len(),
    );
    // M-11：仅在未确认覆盖时前置检测冲突；确认覆盖（overwrite=true）后直接写出，
    // 否则旧分片仍存在会让确认框无限复弹、覆盖永远无法完成（回归见
    // md_split_existing_output_requires_confirmation_then_overwrites）。
    if !overwrite {
        let conflicts = md_tools::conflicting_outputs(out_dir, &names);
        if !conflicts.is_empty() {
            let sample: Vec<String> = conflicts
                .iter()
                .take(3)
                .map(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect();
            return Err(format!(
                "MD_CONFLICT {} 个同名分片已存在于输出目录（如 {}）",
                conflicts.len(),
                sample.join("、")
            ));
        }
    }
    md_tools::run_split_with_events(
        input,
        &plan,
        out_dir,
        overwrite,
        control,
        &md_progress_sink(control, out, |index, total| {
            format!("拆分中：{index} / {total} 片")
        }),
    )
    .map_err(|error| format!("{error:#}"))?;
    Ok(format!(
        "拆分完成：{} 片写入 {}（每片不超过 {} 字节）",
        plan.bounds.len(),
        out_dir.display(),
        limit
    ))
}

/// 后台线程统一收尾：Err 前缀 MD_CONFLICT 表示需要覆盖确认（M-07/M-11），
/// 由事件泵转为确认框；其余 Err 按失败收尾。
fn spawn_md_worker(
    out: EventSender,
    body: impl FnOnce() -> Result<String, String> + Send + 'static,
) {
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body))
            .unwrap_or_else(|_| Err("MD 整理后台操作意外退出；未完成的输出请手动检查".to_string()));
        let event = match result {
            Ok(text) => Event::MdDone(text),
            Err(text) if text.starts_with("MD_CONFLICT ") => {
                Event::MdNeedsConfirm(text.trim_start_matches("MD_CONFLICT ").to_string())
            }
            Err(text) => Event::Failed(text),
        };
        let _ = out.send(event);
    });
}

/// 启动 MD 合并任务（M-02~M-07）：同步校验输入后转后台扫描与合并。
fn start_md_merge(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    if ui.get_busy() {
        return;
    }
    let root = PathBuf::from(ui.get_md_input_dir().to_string());
    if root.as_os_str().is_empty() {
        show_error(ui, "请先选择或输入要合并的目录");
        return;
    }
    if !root.is_dir() {
        show_error(ui, "输入目录不存在或无法访问，请重新选择");
        return;
    }
    let name = match normalize_output_name(ui.get_md_output_name().as_str()) {
        Ok(name) => name,
        Err(text) => {
            show_error(ui, text);
            return;
        }
    };
    let out_dir = PathBuf::from(ui.get_md_output_dir().to_string());
    if out_dir.as_os_str().is_empty() {
        show_error(ui, "请填写输出目录（可用「选择目录…」指定）");
        return;
    }
    let recursive = ui.get_md_recursive();
    let output = out_dir.join(&name);
    let control = Arc::new(Control::default());
    {
        let mut s = state.borrow_mut();
        s.control = Some(Arc::clone(&control));
        s.logs.clear();
        // U-09：一次「停止并关闭」只作用于当次任务收尾，启动新任务必须重置；
        // U-03：实时耗时从本次任务起算。
        s.close_after = false;
        s.started = Instant::now();
        s.runtime = RuntimeMode::Md;
        s.md_pending = Some(MdPending::Merge {
            root: root.clone(),
            recursive,
            output: output.clone(),
        });
    }
    ui.set_busy(true);
    ui.set_paused(false);
    ui.set_error_text("".into());
    ui.set_progress(-1.0);
    ui.set_progress_note("".into());
    ui.set_log_text("".into());
    ui.set_status("正在扫描并合并 Markdown 文件；原文件不会被改动".into());
    let worker_out = out.clone();
    spawn_md_worker(worker_out.clone(), move || {
        md_merge_run(&root, recursive, &output, false, &control, &worker_out)
    });
}

/// 启动 MD 拆分任务（M-08~M-11）。
fn start_md_split(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    if ui.get_busy() {
        return;
    }
    let input = PathBuf::from(ui.get_md_split_file().to_string());
    if input.as_os_str().is_empty() {
        show_error(ui, "请先选择要拆分的 .md 文件");
        return;
    }
    if !input.is_file() {
        show_error(ui, "输入文件不存在或不是文件，请重新选择");
        return;
    }
    if !input
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
    {
        show_error(ui, "MD 拆分只接受 .md 文件");
        return;
    }
    let limit = match parse_size_bytes(ui.get_md_split_size().as_str(), ui.get_md_split_unit()) {
        Ok(limit) => limit,
        Err(text) => {
            show_error(ui, text);
            return;
        }
    };
    let out_dir = PathBuf::from(ui.get_md_split_dir().to_string());
    if out_dir.as_os_str().is_empty() {
        show_error(ui, "请填写输出目录（可用「选择目录…」指定）");
        return;
    }
    let control = Arc::new(Control::default());
    {
        let mut s = state.borrow_mut();
        s.control = Some(Arc::clone(&control));
        s.logs.clear();
        // U-09：一次「停止并关闭」只作用于当次任务收尾，启动新任务必须重置；
        // U-03：实时耗时从本次任务起算。
        s.close_after = false;
        s.started = Instant::now();
        s.runtime = RuntimeMode::Md;
        s.md_pending = Some(MdPending::Split {
            input: input.clone(),
            limit,
            out_dir: out_dir.clone(),
        });
    }
    ui.set_busy(true);
    ui.set_paused(false);
    ui.set_error_text("".into());
    ui.set_progress(-1.0);
    ui.set_progress_note("".into());
    ui.set_log_text("".into());
    ui.set_status(format!("正在规划拆分边界（单文件上限 {limit} 字节）").into());
    let worker_out = out.clone();
    spawn_md_worker(worker_out.clone(), move || {
        md_split_run(&input, limit, &out_dir, false, &control, &worker_out)
    });
}

/// 确认框 kind=4 的处理：消费挂起的 MD 操作并以覆盖模式重跑（M-07/M-11）。
/// 生产由 on_confirmed(4) 调用；无头测试直接调用同一函数走完全相同的路径。
fn confirm_md_override(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    let pending = state.borrow_mut().md_pending.take();
    if let Some(pending) = pending {
        ui.set_busy(true);
        ui.set_paused(false);
        ui.set_acknowledge(false);
        ui.set_status("已确认覆盖：正在重新扫描并写入输出".into());
        resume_md_after_confirm(pending, state, out);
    }
}

/// 覆盖确认后的 MD 重跑（M-07/M-11）：重新扫描/规划并写入（overwrite=true）。
fn resume_md_after_confirm(pending: MdPending, state: &Rc<RefCell<State>>, out: &EventSender) {
    let control = Arc::new(Control::default());
    {
        let mut s = state.borrow_mut();
        s.control = Some(Arc::clone(&control));
        // 覆盖确认重跑是一次新的执行段：耗时从重跑起算（U-03）；close_after 理论上
        // 必为 false（为 true 时 MdNeedsConfirm 已直接退出），此处一并重置保持不变量。
        s.close_after = false;
        s.started = Instant::now();
        s.runtime = RuntimeMode::Md;
    }
    let worker_out = out.clone();
    spawn_md_worker(worker_out.clone(), move || match pending {
        MdPending::Merge {
            root,
            recursive,
            output,
        } => md_merge_run(&root, recursive, &output, true, &control, &worker_out),
        MdPending::Split {
            input,
            limit,
            out_dir,
        } => md_split_run(&input, limit, &out_dir, true, &control, &worker_out),
    });
}

/// 启动 Git 逐文件提交并推送任务（G-02~G-16）。
fn start_git(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    if ui.get_busy() {
        return;
    }
    let repo = PathBuf::from(ui.get_git_repo().to_string());
    if repo.as_os_str().is_empty() {
        show_error(ui, "请先选择或输入 Git 项目目录");
        return;
    }
    if !repo.is_dir() {
        show_error(ui, "目录不存在或无法访问，请重新选择");
        return;
    }
    let control = Arc::new(Control::default());
    let shared = Arc::new(GitShared::new());
    {
        let mut s = state.borrow_mut();
        s.control = Some(Arc::clone(&control));
        s.logs.clear();
        // U-09：一次「停止并关闭」只作用于当次任务收尾，启动新任务必须重置；
        // U-03：实时耗时从本次任务起算。
        s.close_after = false;
        s.started = Instant::now();
        s.runtime = RuntimeMode::Git;
        s.git_shared = Some(Arc::clone(&shared));
        s.md_pending = None;
    }
    ui.set_busy(true);
    ui.set_paused(false);
    ui.set_error_text("".into());
    ui.set_notice_text("".into());
    ui.set_progress(-1.0);
    ui.set_progress_note("".into());
    ui.set_log_text("".into());
    ui.set_git_state("检查仓库".into());
    // 新任务预检期间不显示上一仓库的分支、文件与重试进度。
    ui.set_git_branch("".into());
    ui.set_git_upstream("".into());
    ui.set_git_current("".into());
    ui.set_git_stage("扫描".into());
    ui.set_git_retry(0);
    ui.set_git_retry_wait(0);
    ui.set_git_total(0);
    ui.set_git_done(0);
    ui.set_status("正在验证仓库（分支、upstream 与仓库状态）…".into());
    let worker_out = out.clone();
    std::thread::spawn(move || {
        let git = match git_tools::find_git() {
            Ok(git) => git,
            Err(error) => {
                let text = format!("{error:#}");
                if let Ok(mut state) = shared.state.lock() {
                    state.clear();
                    state.push_str("失败");
                }
                if let Ok(mut stage) = shared.stage.lock() {
                    stage.clear();
                    stage.push_str("失败");
                }
                let _ = worker_out.send(Event::GitDone(format!("无法启动 Git 任务：{text}")));
                return;
            }
        };
        let log_out = worker_out.clone();
        let status_out = worker_out.clone();
        let final_text = git_tools::run(
            &git,
            &repo,
            &control,
            &shared,
            &|text: &str| {
                let _ = log_out.send(Event::Log(text.to_string()));
            },
            &|text: &str| {
                let _ = status_out.send(Event::Status(text.to_string()));
            },
            BACKOFF_UNIT,
        );
        let _ = worker_out.send(Event::GitDone(final_text));
    });
}

/// 范围缩小提示的前缀：只有本条提示才在范围恢复默认时被清除，
/// 不影响取消等其它蓝条通知（U-06）。
const SCOPE_NOTICE_PREFIX: &str = "已缩小处理范围";

/// R-03/S-04：用户主动缩小处理范围（关闭递归、排除隐藏/系统资料、填写排除规则）时，
/// 界面必须明确提示范围缩小；Git 排除不受这些开关影响，始终生效。
fn scope_notice(config: &Config) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if !config.recursive {
        parts.push("不含所选目录的子目录");
    }
    if !config.include_hidden {
        parts.push("不含隐藏属性资料");
    }
    if !config.include_system {
        parts.push("不含系统属性资料");
    }
    if !config.exclusions.trim().is_empty() {
        parts.push("已应用排除路径规则");
    }
    (!parts.is_empty()).then(|| {
        format!(
            "{SCOPE_NOTICE_PREFIX}：{}；Git 目录仍始终整树排除。",
            parts.join("、")
        )
    })
}

/// 按当前配置刷新范围缩小提示：缩小就提示，恢复默认（且当前提示正是它）就清除。
fn refresh_scope_notice(ui: &AppWindow, config: &Config) {
    match scope_notice(config) {
        Some(text) => ui.set_notice_text(text.into()),
        None => {
            if ui.get_notice_text().starts_with(SCOPE_NOTICE_PREFIX) {
                ui.set_notice_text("".into());
            }
        }
    }
}

/// C-11：计划行状态的中文显示。未勾选且尚未执行的计划行按勾选实际状态显示
/// 「已取消勾选」，重新勾选恢复「待执行」（勾选即时生效，不必等执行阶段）。
/// 任务库存英文状态（done/skipped/failed，engine 执行后回写），读取链在此翻译成中文；
/// 旧任务库可能已存中文，别名一并保留。终态行不会落入「待执行/已取消勾选」，
/// 与 ui/app.slint 的勾选门禁（只认这两种状态可勾选）保持一致。
fn plan_row_state(selected: bool, stored: &str) -> &str {
    match stored {
        "pending" | "待执行" | "unselected" | "已取消勾选" => {
            if selected {
                "待执行"
            } else {
                "已取消勾选"
            }
        }
        "done" | "已执行" => "已执行",
        "skipped" | "已跳过" => "已跳过",
        "failed" | "执行失败" => "执行失败",
        other => other,
    }
}

/// 按数值 id 在计划模型中定位行并就地修改（首行命中即停）；模型不是预期的
/// VecModel 时静默不动。id 比较只有这一处实现：按数值比较避免每行拼一次
/// id.to_string()（分配次数与行数同阶），也避免按字符串比较时跨任务误匹配
/// 相同 id 的行（action id 是各任务库各自的 rowid）。
fn mutate_plan_row(ui: &AppWindow, id: i64, mutate: impl FnOnce(&mut PlanRow)) -> bool {
    let plans = ui.get_plans();
    let Some(model) = plans.as_any().downcast_ref::<VecModel<PlanRow>>() else {
        return false;
    };
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            if row.id.as_str().parse::<i64>() == Ok(id) {
                mutate(&mut row);
                model.set_row_data(i, row);
                return true;
            }
        }
    }
    false
}

/// 勾选切换时就地更新该行：勾选值与状态显示同时改，避免「取消勾选后仍显示待执行」
/// 到数据库事件回来前的不一致；保存失败时由 SelectionSaved/重载恢复数据库真值。
fn patch_plan_row(ui: &AppWindow, id: i64, selected: bool) {
    mutate_plan_row(ui, id, |row| {
        row.selected = selected;
        row.state = plan_row_state(selected, row.state.as_str()).into();
    });
}

/// 事件循环把日志写进界面环形缓冲的统一入口（U-10/S-07：界面仅保留最近 300 条）。
/// Failed 收尾与普通日志同走此路径，避免失败消息绕过条数上限。
fn push_event_log(logs: &mut VecDeque<String>, text: String) {
    if logs.len() >= 300 {
        logs.pop_front();
    }
    logs.push_back(text);
}

/// 日志面板当前是否可见：「进度与日志」在目录整理页是 panel 2、在递归解压页是 panel 1
/// （ui/app.slint 两处 `if root.panel == …` 的条件元素）。不可见时条件子树根本没实例化，
/// 重建 300 行文本纯属浪费——事件循环据此只在可见时上屏（见轮询尾部的 log_dirty）。
fn log_panel_visible(ui: &AppWindow) -> bool {
    match ui.get_screen() {
        0 => ui.get_panel() == 2,
        2 => ui.get_panel() == 1,
        // MD 整理与 Git 工具页的日志区常驻显示（无面板切换）。
        3..=5 => true,
        _ => false,
    }
}

/// 日志面板的呈现文本：最新一条在最上（U-10 的显示口径与环形缓冲同源）。
/// 逐条 push_str 一次成型，避免「克隆 300 条字符串 → 收集成 Vec → join」的中间分配。
fn log_panel_text(logs: &VecDeque<String>) -> String {
    // 预分配：行长之和 + 分隔换行，避免大字符串反复扩容拷贝。
    let capacity = logs.iter().map(String::len).sum::<usize>() + logs.len().saturating_sub(1);
    let mut text = String::with_capacity(capacity);
    for (index, line) in logs.iter().rev().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        text.push_str(line);
    }
    text
}

/// 恢复侧栏全量工具列表并清空搜索框：从关于页返回或点选工具后调用，
/// 避免搜索过滤残留导致侧栏只剩匹配项。
fn reset_tool_list(ui: &AppWindow) {
    let tools = registry::tools()
        .iter()
        .map(|t| ToolRow {
            id: t.id.into(),
            name: t.name.into(),
            summary: t.summary.into(),
        })
        .collect::<Vec<_>>();
    ui.set_tools(Rc::new(VecModel::from(tools)).into());
    ui.set_tool_search("".into());
}

/// 同步回调装配：规则表、分区/工具导航、主题与输入校验——纯属性/状态操作，
/// 不依赖事件循环，独立成函数以便无头 GUI 测试直接装配后断言。
fn wire_sync(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    {
        let weak = ui.as_weak();
        let state = state.clone();
        // 目录输入变化：无任务时重算很轻（仅本地存在性检查），保持同步反馈；有任务时
        // 就绪重算要开任务库 + canonicalize，交给后台 worker（见 load_plan_state）。
        // 去抖 300ms 只用于「按键停顿后才重算」，不再是为了躲开界面线程上的阻塞 I/O。
        let debounce = std::rc::Rc::new(slint::Timer::default());
        ui.on_root_edited({
            let weak = weak.clone();
            let state = state.clone();
            let debounce = debounce.clone();
            let out = out.clone();
            move || {
                if let Some(ui) = weak.upgrade() {
                    // 编辑目录立即失效执行按钮（旧同步行为的安全属性）：去抖窗口内不得凭
                    // 陈旧 ready 打开确认框；随后去抖重算，目录与计划仍一致时重新点亮。
                    ui.set_ready(false);
                    if !ui.get_has_task() {
                        sync_ready_after_directory(&ui, &state.borrow(), &out);
                        return;
                    }
                    let weak = weak.clone();
                    let state = state.clone();
                    let out = out.clone();
                    debounce.start(
                        slint::TimerMode::SingleShot,
                        Duration::from_millis(300),
                        move || {
                            if let Some(ui) = weak.upgrade() {
                                sync_ready_after_directory(&ui, &state.borrow(), &out);
                            }
                        },
                    );
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_select_tool(move |id| {
            if let Some(ui) = weak.upgrade() {
                // 点回工具时恢复全量列表：搜索过滤不得在切页后残留。
                reset_tool_list(&ui);
                if let Some(tool) = Tool::from_id(id.as_str()) {
                    let screen = match tool {
                        Tool::Extract => 2,
                        Tool::Organizer => 0,
                        Tool::Md => 3,
                        Tool::Git => 4,
                        Tool::MarkdownConverter => 5,
                        Tool::SnapOcr => 6,
                        Tool::AcpHttp => 8,
                    };
                    ui.set_screen(screen);
                    ui.set_active_tool_id(id.clone());
                    // 全局提示属于当前页面；切换工具时不得把转换组件缺失详情
                    // 残留到截图或旧工具页面。
                    ui.set_error_text("".into());
                    ui.set_notice_text("".into());
                    ui.set_convert_detail_text("".into());
                    ui.set_convert_detail_open(false);
                    {
                        let mut s = state.borrow_mut();
                        s.tool = tool;
                        // 切工具回到该工具的第一个规则分区；MD/Git 无规则分区（R-01），
                        // 分区名清空，界面不显示分区胶囊。分区集合随工具变化（R-01）。
                        s.section = tool
                            .sections()
                            .first()
                            .map_or_else(String::new, |first| (*first).to_string());
                    }
                    ui.set_section(0);
                    // 面板页签集合同样随工具变化（目录整理 0–2、递归解压 0–1）：
                    // 共享索引不重置会停在另一工具才有的面板，内容区整块空白（U-11）。
                    ui.set_panel(0);
                    // U-11：MD 整理回到默认子页「合并 MD」，不得停留在拆分子页。
                    ui.set_md_subpage(0);
                    // 转换任务运行中切页再切回时保留实时进度：无条件重置会让长任务
                    // 期间离开再回来的进度条到下一个 CONVERTER_FILE_STARTED 前
                    // 保持不确定。运行判定与 refresh_runtime 同口径（busy 且
                    // runtime=MarkdownConverter）；非运行态保持原重置口径，上次
                    // 任务的残留进度不得带到新会话。
                    if !(ui.get_busy() && state.borrow().runtime == RuntimeMode::MarkdownConverter)
                    {
                        ui.set_convert_progress(-1.0);
                        ui.set_convert_progress_note("".into());
                    }
                    if tool == Tool::MarkdownConverter {
                        ui.set_convert_log_text(
                            log_panel_text(&state.borrow().convert_logs).into(),
                        );
                        start_markdown_readiness(&ui, &state, &out);
                    }
                    if tool == Tool::SnapOcr {
                        // 进入页面即做只读资产复检并连上服务监督线程（O-09/O-11）；
                        // 未初始化、服务缺失都不阻塞其他工具（O-03）。
                        start_snap_readiness(&ui, &state, &out);
                        if state.borrow().snap_commands.is_none() {
                            ensure_snap_supervisor(&state, &out);
                        }
                    }
                    refresh(&ui, &state.borrow());
                    // 切工具不是规则或目录改动（C-10/R-04）：就绪计划按「任务+配置+目录」
                    // 重算保持可执行，与目录重输同口径，而不是一律失效。
                    sync_ready_after_directory(&ui, &state.borrow(), &out);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_convert_selection_changed(move || {
            if let Some(ui) = weak.upgrade() {
                start_markdown_readiness(&ui, &state, &out);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_search_tools(move |query| {
            if let Some(ui) = weak.upgrade() {
                let query = query.to_lowercase();
                let tools = registry::tools()
                    .iter()
                    .filter(|t| {
                        format!("{} {}", t.name, t.summary)
                            .to_lowercase()
                            .contains(&query)
                    })
                    .map(|t| ToolRow {
                        id: t.id.into(),
                        name: t.name.into(),
                        summary: t.summary.into(),
                    })
                    .collect::<Vec<_>>();
                ui.set_tools(Rc::new(VecModel::from(tools)).into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_navigation(move |screen| {
            if let Some(ui) = weak.upgrade() {
                ui.set_screen(screen);
                ui.set_error_text("".into());
                ui.set_notice_text("".into());
                // 「关于」不是工具：清空 active-tool-id，侧栏不高亮任何工具。
                // 返回工具页统一走 select_tool（工具 NavItem 的点击回调），此处不再恢复列表：
                // 该回调在生产中只被「关于」NavItem 以 1 调用，其余分支属不可达路径。
                if screen == 1 || screen == 7 {
                    ui.set_active_tool_id("".into());
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_select_section(move |index| {
            {
                // 分区集合随工具变化（R-01）：递归解压=解压/安全与性能，目录整理=去重/归类/清理/安全与性能。
                let sections = state.borrow().tool.sections();
                if let Some(section) = sections.get(usize::try_from(index.max(0)).unwrap_or(0)) {
                    // UI 与状态用同一个钳制值：负 index 不再出现「状态到解压、高亮停在 -1」的分叉。
                    let index = index.max(0);
                    state.borrow_mut().section = section.to_string();
                    if let Some(ui) = weak.upgrade() {
                        {
                            ui.set_section(index);
                            refresh(&ui, &state.borrow());
                        }
                    }
                }
            }
        });
    }
    // 布尔/下拉：不整表重建 rules。重建会在 selected/toggled 回调里销毁 ListView 里的
    // ComboBox，用户点选后的文字会停在旧值；改为配置写入 + 就地更新该行，
    // 联动行的出现/消失交给 sync_rules 增量插入删除。
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_rule_bool(move |key, value| {
            if let Some(ui) = weak.upgrade() {
                if changed(&ui, &state, key.as_str(), &value.into(), false) {
                    patch_rule_row(&ui, key.as_str(), |row| row.checked = value);
                    sync_rules(&ui, &state.borrow());
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_rule_choice(move |key, index| {
            if let Some(ui) = weak.upgrade() {
                let spec = state
                    .borrow()
                    .specs
                    .iter()
                    .find(|s| s.key == key.as_str())
                    .cloned();
                let selected = spec
                    .as_ref()
                    .and_then(|s| s.choices.get(usize::try_from(index.max(0)).unwrap_or(0)))
                    .map(|c| c[0].clone());
                if let Some(value) = selected {
                    let hint = spec
                        .as_ref()
                        .map(|s| hint_for(s, &serde_json::Value::from(value.as_str())));
                    if changed(&ui, &state, key.as_str(), &value.into(), false) {
                        patch_rule_row(&ui, key.as_str(), |row| {
                            row.index = index;
                            if let Some(hint) = &hint {
                                row.hint = hint.clone().into();
                            }
                        });
                        sync_rules(&ui, &state.borrow());
                    }
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        // 主题选择在「关于」页（应用级设置，不属于目录整理的处理规则）；
        // changed() 对 theme 键会跳过计划失效并直接套用。
        ui.on_select_theme(move |choice| {
            if let Some(ui) = weak.upgrade() {
                let value = match choice {
                    1 => "light",
                    2 => "dark",
                    _ => "system",
                };
                changed(&ui, &state, "theme", &value.into(), false);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_toggle_advanced(move |show| {
            if let Some(ui) = weak.upgrade() {
                state.borrow_mut().show_advanced = show;
                ui.set_show_advanced(show);
                refresh(&ui, &state.borrow());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_rule_text(move |key, value| {
            if let Some(ui) = weak.upgrade() {
                let spec = state
                    .borrow()
                    .specs
                    .iter()
                    .find(|s| s.key == key.as_str())
                    .cloned();
                let numeric = spec.as_ref().is_some_and(|s| s.kind == "number");
                let capacity = spec
                    .as_ref()
                    .is_some_and(|s| s.unit.as_deref() == Some("bytes"));
                let parsed = if numeric {
                    if let Ok(v) = parse_capacity(value.as_str(), capacity) {
                        serde_json::Value::from(v)
                    } else {
                        retain_invalid_number(&ui, &state, key.as_str(), value.as_str());
                        return;
                    }
                } else {
                    serde_json::Value::from(value.to_string())
                };
                if numeric {
                    state.borrow_mut().invalid_rule_inputs.remove(key.as_str());
                }
                if changed(&ui, &state, key.as_str(), &parsed, false) {
                    // 模型行里的 value 是重建列表（切分区、恢复默认规则）时的唯一来源，必须跟着更新，
                    // 否则重建后这一行会拿旧值覆盖刚改好的设置。
                    let canonical = parsed
                        .as_u64()
                        .map_or_else(|| value.to_string(), |v| v.to_string());
                    patch_rule_row(&ui, key.as_str(), |row| {
                        row.value = canonical.clone().into();
                    });
                    // 联动行的可见性也取决于自身的值（如 large_threshold_bytes 改回默认）：
                    // 与 on_rule_choice 同步可见性，增量插删不重建整表。
                    sync_rules(&ui, &state.borrow());
                } else if numeric {
                    retain_invalid_number(&ui, &state, key.as_str(), value.as_str());
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_request_start(move || {
            if let Some(ui) = weak.upgrade() {
                // C-01：分析只读不改文件，无需破坏性确认；只有「执行计划」（kind=2）需要确认。
                start_task(&ui, &state, &out, false);
            }
        });
    }
    {
        // X-02：解压是一段确认。先弹确认框（数量先占位），后台清点压缩包后原位更新文案。
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_request_extract_start(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_busy() {
                    return;
                }
                if let Some(error) = invalid_number_error(&state.borrow(), Tool::Extract) {
                    show_error(&ui, error);
                    return;
                }
                if let Err(error) = state.borrow().config.validate_extract() {
                    show_error(&ui, error);
                    return;
                }
                let directory = PathBuf::from(ui.get_directory().as_str());
                if directory.as_os_str().is_empty() {
                    show_error(&ui, "请先选择需要解压的目录");
                    return;
                }
                if !directory.is_dir() {
                    show_error(&ui, "目标目录不存在或无法访问，请重新选择目录");
                    return;
                }
                // S-04：先在原始路径上检查链接边界（canonicalize 会丢失 reparse 身份），
                // 在打开确认框前拒绝。
                if let Err(error) = crate::fsutil::ensure_plain_entry(&directory) {
                    show_error(&ui, error);
                    return;
                }
                // 与清点同一口径的规范化预检（S-05 受保护目录等）：在这里就拒绝，
                // 不得先打开确认框再等后台清点失败——占位文案下仍可点「确认」。
                if let Err(error) = crate::fsutil::normalize_root(&directory) {
                    show_error(&ui, error);
                    return;
                }
                ui.set_confirm_text(
                    extract_confirm_text("正在清点压缩包…", ui.get_directory().as_str()).into(),
                );
                ui.set_acknowledge(false);
                ui.set_confirm_kind(1);
                // X-02：数量未知（清点进行中）不得允许确认；清点返回后在事件侧解除。
                ui.set_confirm_pending(true);
                let out = out.clone();
                let config = {
                    let s = state.borrow();
                    s.config.clone()
                };
                // 清点请求代际：每次发起清点请求时递增，迟到的低代际事件整体丢弃（X-02）。
                let generation = {
                    let mut s = state.borrow_mut();
                    s.extract_generation += 1;
                    s.extract_generation
                };
                std::thread::spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        engine::count_archives(&directory, &config)
                    }));
                    let event = match result {
                        Ok(Ok(count)) => Event::ExtractCount(generation, Ok(count)),
                        Ok(Err(error)) => {
                            Event::ExtractCount(generation, Err(format!("{error:#}")))
                        }
                        Err(_) => Event::ExtractCount(
                            generation,
                            Err("清点压缩包时后台操作意外退出".into()),
                        ),
                    };
                    let _ = out.send(event);
                });
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_request_apply(move||{if let Some(ui)=weak.upgrade(){
            if !ui.get_ready(){return;}
            ui.set_confirm_text(format!("目标目录：{}\n执行“整理计划”中已勾选的操作；取消勾选的项目不会执行。\n\n{}\n\n{}",ui.get_directory(),state.borrow().config.destructive_warning(),ui.get_summary()).into());
            ui.set_acknowledge(false);ui.set_confirm_kind(2);
        }});
    }
    {
        // U-10：失败列表翻页。页游标存在才发起（翻页按钮只在对应页真实存在时启用）；
        // 采用「先加载、结果落地再提交页码」，失败时按钮保持与列表一致。
        let state = state.clone();
        ui.on_fail_page(move |direction| {
            let next = if direction < 0 {
                state.borrow().fail_page.saturating_sub(1)
            } else {
                state.borrow().fail_page + 1
            };
            let valid = {
                let s = state.borrow();
                s.extract_task.is_some() && next < s.fail_page_starts.len()
            };
            if valid {
                request_fail_page(&state, next);
            }
        });
    }
}

/// 界面事件泵：排空事件通道、上屏日志、刷新实时指标与系统主题。
/// 由两条路径驱动，共用同一套排空/应用逻辑：
/// 1) 100ms 轮询——进度、耗时、主题等周期性刷新（绘制频率因此不超过 10Hz），
///    并作为唤醒不可用时的兜底排空（事件循环未运行/已退出）；
/// 2) 结果事件唤醒——worker 通过 `EventSender` 回传结果后立即唤醒事件循环
///    （见 `wake_event_loop`），结果上屏不再等最坏 100ms（H-02）。
struct UiPump {
    receiver: mpsc::Receiver<Event>,
    state: Rc<RefCell<State>>,
    /// 结果回传句柄：收尾与勾选保存后要重新发起计划页/就绪快照加载。
    out: EventSender,
    /// 上次系统主题轮询：“跟随系统”需要感知运行期的系统深浅色切换；
    /// 轮询注册表成本极低，每 2 秒一次。开机不足 10 秒时减法会下溢 panic，构造时兜底。
    theme_poll: Cell<Instant>,
    /// 上次日志上屏时间：面板是 300 行整段重建，按时间预算限流（见 `LOG_REFRESH_MIN`）。
    log_refreshed: Cell<Instant>,
    /// 日志呈现是否落后于环形缓冲：面板不可见时只置脏（重建 300 行文本浪费），
    /// 打开后的下一次刷新按 state.logs 补齐，保持最新在上的 300 条记录。
    log_dirty: Cell<bool>,
    /// 转换日志呈现是否落后于环形缓冲（S1-03）：CONVERTER_LOG 只置脏，由轮询
    /// 尾部在转换页可见时统一重建——一批 N 条日志只做一次整段重排；收尾与
    /// 校验失败事件仍直接上屏保证立即可见。
    convert_log_dirty: Cell<bool>,
    /// 失败列表当前页的上次自动刷新时间：运行中低频节流刷新（U-10，见 refresh_fail_list）。
    fail_refreshed: Cell<Instant>,
}
/// 300 行可选中文本的整段重排开销显著：运行中日志合批到 2Hz，避免连续重排挤占输入处理。
/// 进度与取消仍走原来的 100ms 刷新；任务收尾日志不等待此间隔。
const LOG_REFRESH_MIN: Duration = Duration::from_millis(500);

impl UiPump {
    fn new(receiver: mpsc::Receiver<Event>, state: Rc<RefCell<State>>, out: EventSender) -> Self {
        Self {
            receiver,
            state,
            out,
            theme_poll: Cell::new(
                Instant::now()
                    .checked_sub(Duration::from_secs(10))
                    .unwrap_or_else(Instant::now),
            ),
            // 首次刷新立即可做：减去最小间隔而不是从现在开始计时。
            log_refreshed: Cell::new(
                Instant::now()
                    .checked_sub(LOG_REFRESH_MIN)
                    .unwrap_or_else(Instant::now),
            ),
            log_dirty: Cell::new(false),
            convert_log_dirty: Cell::new(false),
            fail_refreshed: Cell::new(
                Instant::now()
                    .checked_sub(FAIL_REFRESH_MIN)
                    .unwrap_or_else(Instant::now),
            ),
        }
    }
    /// 一次刷新：主题 → 排空事件 → 日志上屏 → 失败列表 → 实时指标。
    fn run(&self, ui: &AppWindow) {
        self.state.borrow().snap_foreground_busy.store(
            ui.get_busy()
                || ui.get_convert_initializing()
                || ui.get_convert_preparing()
                || ui.get_snap_initializing()
                || ui.get_convert_runtime_saving(),
            Ordering::Release,
        );
        if self.theme_poll.get().elapsed() >= Duration::from_secs(2) {
            self.theme_poll.set(Instant::now());
            ui.set_system_dark(system_dark());
        }
        let terminal = self.drain(ui);
        // 隐藏时只置脏：日志面板没实例化就不重建 300 行文本；面板可见（含从隐藏切回来）
        // 的下一次刷新按环形缓冲上屏，保证打开时看到的是最新 300 条。
        // 整段重排按时间预算限流：结果事件唤醒只让结果尽快上屏/更新指标，不放大日志重排成本；
        // 收尾事件例外——失败原因与最终统计的日志必须立刻可见。
        if self.log_dirty.get()
            && log_panel_visible(ui)
            && (terminal || self.log_refreshed.get().elapsed() >= LOG_REFRESH_MIN)
        {
            self.log_refreshed.set(Instant::now());
            self.log_dirty.set(false);
            ui.set_log_text(log_panel_text(&self.state.borrow().logs).into());
        }
        // 转换日志同口径（S1-03）：面板只在转 Markdown 页实例化，离开页面时整段
        // 重建纯属浪费；置脏后在页面可见的下一次刷新按环形缓冲补齐。
        if self.convert_log_dirty.get() && ui.get_screen() == 5 {
            self.convert_log_dirty.set(false);
            ui.set_convert_log_text(log_panel_text(&self.state.borrow().convert_logs).into());
        }
        self.apply_fail_messages(ui);
        self.apply_snap_messages(ui);
        apply_acp_messages(ui, &self.state);
        self.refresh_fail_list(ui);
        self.refresh_runtime(ui);
    }
    fn apply_snap_messages(&self, ui: &AppWindow) {
        let messages: Vec<SnapMessage> = self
            .state
            .borrow()
            .snap_receiver
            .borrow_mut()
            .try_iter()
            .collect();
        for message in messages {
            match message {
                SnapMessage::Readiness(generation, result) => {
                    if self.state.borrow().snap_generation != generation {
                        continue;
                    }
                    match result {
                        Ok(()) => {
                            ui.set_snap_ready(true);
                            ui.set_snap_asset_status("组件已校验，可离线识别".into());
                            ui.set_snap_asset_detail("".into());
                        }
                        Err(error) => {
                            ui.set_snap_ready(false);
                            ui.set_snap_asset_status("组件未就绪，请前往设置修复".into());
                            ui.set_snap_asset_detail(error.into());
                        }
                    }
                }
                SnapMessage::Progress(generation, progress) => {
                    if self.state.borrow().snap_generation == generation {
                        ui.set_snap_progress(progress.into());
                    }
                }
                SnapMessage::Initialized(generation, result) => {
                    if self.state.borrow().snap_generation != generation {
                        continue;
                    }
                    self.state.borrow_mut().snap_init_cancel = None;
                    ui.set_snap_initializing(false);
                    match result {
                        Ok(()) => {
                            ui.set_snap_ready(true);
                            ui.set_snap_asset_status("组件初始化完成，可离线使用".into());
                            ui.set_snap_asset_detail("".into());
                            ui.set_snap_progress("".into());
                            ensure_snap_supervisor(&self.state, &self.out);
                        }
                        Err(error) => {
                            ui.set_snap_ready(false);
                            if error == "用户取消初始化" {
                                // U-06/S3-04：用户主动取消不是组件故障——中性状态行
                                // 提示即可，不写红色错误、不汇入「组件未就绪」详情。
                                ui.set_snap_asset_status("初始化已取消，可稍后前往设置重试".into());
                                ui.set_snap_asset_detail("".into());
                            } else {
                                ui.set_snap_asset_status("组件初始化未完成，请前往设置重试".into());
                                ui.set_snap_asset_detail(error.clone().into());
                                ui.set_snap_error(error.into());
                            }
                        }
                    }
                    if self.close_after_if_idle(ui) {
                        let _ = slint::quit_event_loop();
                    }
                }
                SnapMessage::Service(result, requested) => {
                    if requested {
                        ui.set_snap_request_pending(false);
                    }
                    match result {
                        Err(error) => {
                            ui.set_snap_connected(false);
                            if error.starts_with("后台已从托盘退出") {
                                ui.set_snap_service_status(error.into());
                                ui.set_snap_error("".into());
                            } else {
                                ui.set_snap_service_status("后台服务未连接，请查看设置页".into());
                                ui.set_snap_error(error.into());
                            }
                        }
                        Ok(value) if value["ok"] != true => {
                            ui.set_snap_connected(true);
                            ui.set_snap_error(
                                value["error"].as_str().unwrap_or("截图服务拒绝请求").into(),
                            );
                            // 请求失败不等于断线；保留之前读出的设置和当前生效热键。
                        }
                        Ok(value) => {
                            self.apply_snap_service_state(ui, &value, requested);
                        }
                    }
                }
            }
        }
    }

    /// 排空失败列表专用通道：任务目录发现与分页结果（U-10）。
    /// 迟到的结果按请求代际与当前解压任务整体丢弃，与计划页事件同一口径。
    fn apply_fail_messages(&self, ui: &AppWindow) {
        let messages: Vec<FailChannelMsg> = self
            .state
            .borrow()
            .fail_receiver
            .borrow_mut()
            .try_iter()
            .collect();
        for message in messages {
            match message {
                FailChannelMsg::TaskFound(path, generation) => {
                    // 过期发现：用户已发起新的解压任务（清点代际已前进）或流程已结束。
                    if self.state.borrow().extract_generation != generation {
                        continue;
                    }
                    {
                        let mut s = self.state.borrow_mut();
                        s.extract_task = Some(path);
                        s.fail_page = 0;
                        s.fail_page_starts = vec![0];
                        s.fail_gen += 1;
                    }
                    // 关联到本次任务：先呈现空列表占位，第 0 页数据落地后填充。
                    ui.set_fail_ready(true);
                    ui.set_fail_rows(Rc::new(VecModel::from(Vec::<FailRow>::new())).into());
                    ui.set_fail_total(0);
                    ui.set_fail_prev_enabled(false);
                    ui.set_fail_next_enabled(false);
                    ui.set_fail_page_label("加载中…".into());
                    request_fail_page(&self.state, 0);
                }
                FailChannelMsg::Page(fail) => {
                    let accepted = {
                        let s = self.state.borrow();
                        s.extract_task.as_ref() == Some(&fail.task) && s.fail_gen == fail.generation
                    };
                    if !accepted {
                        continue;
                    }
                    if let Some(error) = fail.error {
                        ui.set_error_text(error.into());
                        // 取数失败按当前页恢复翻页按钮，避免永久中间态（与计划页同口径）。
                        let (page, page_count) = {
                            let s = self.state.borrow();
                            (s.fail_page, s.fail_page_starts.len())
                        };
                        ui.set_fail_prev_enabled(page > 0);
                        ui.set_fail_next_enabled(page + 1 < page_count);
                        continue;
                    }
                    {
                        let mut s = self.state.borrow_mut();
                        s.fail_page = fail.page;
                        if fail.has_more && s.fail_page_starts.len() <= fail.page + 1 {
                            s.fail_page_starts.push(fail.last_id);
                        }
                    }
                    let rows = fail
                        .items
                        .into_iter()
                        .map(|item| FailRow {
                            stage: item.stage.into(),
                            source: item.source.into(),
                            reason: item.reason.into(),
                            target: item.target.into(),
                        })
                        .collect::<Vec<_>>();
                    ui.set_fail_rows(Rc::new(VecModel::from(rows)).into());
                    // 总数为显示用途，超出 i32 的极端值饱和显示即可。
                    ui.set_fail_total(i32::try_from(fail.total).unwrap_or(i32::MAX));
                    ui.set_fail_prev_enabled(fail.page > 0);
                    ui.set_fail_next_enabled(fail.has_more);
                    ui.set_fail_page_label(
                        format!("第 {} 页 · 每页最多 100 项", fail.page + 1).into(),
                    );
                }
            }
        }
    }
    /// 运行中自动刷新失败列表当前页（U-10「运行中可查看已经发生的失败」）：
    /// 只在解压任务运行且失败列表面板可见时按低频节流发起；空闲与其他页面不取数。
    fn refresh_fail_list(&self, ui: &AppWindow) {
        if ui.get_screen() != 2 || ui.get_panel() != 2 || !ui.get_busy() {
            return;
        }
        let (extracting, has_task) = {
            let s = self.state.borrow();
            (s.runtime == RuntimeMode::Extract, s.extract_task.is_some())
        };
        if !extracting || !has_task || self.fail_refreshed.get().elapsed() < FAIL_REFRESH_MIN {
            return;
        }
        self.fail_refreshed.set(Instant::now());
        let page = self.state.borrow().fail_page;
        request_fail_page(&self.state, page);
    }
    /// 排空事件通道并应用事件；返回本轮是否处理过任务收尾事件（Ready/Done/Failed/ExtractDone），
    /// 供调用方决定是否立即上屏日志（收尾日志是例外，不受刷新预算限制）。
    /// 每次刷新最多处理 256 条：单次回调里的界面工作有上界，其余事件留给下一次处理。
    /// 独立借用下的日志上屏：写环形缓冲并标脏。仅限 borrow_mut 只服务日志的
    /// 调用点；与其他字段共用同一借用的收尾分支不得使用（会扩大借用存活范围）。
    fn push_log(&self, text: String) {
        let mut s = self.state.borrow_mut();
        push_event_log(&mut s.logs, text);
        self.log_dirty.set(true);
    }

    /// SnapMessage::Service 的 Ok 分支：按服务状态快照上屏连接/模型/任务/热键/
    /// 自启动，退出广播时取消全部共享任务（`exiting` 只求值一次，纯读无副作用），
    /// 并按 C'-2 口径维护错误文本。
    fn apply_snap_service_state(&self, ui: &AppWindow, value: &serde_json::Value, requested: bool) {
        let exiting = value["exiting"] == true;
        if exiting {
            if let Some(control) = self.state.borrow().control.as_ref() {
                control.cancel();
            }
            for cancel in [
                &self.state.borrow().convert_cancel,
                &self.state.borrow().convert_init_cancel,
                &self.state.borrow().snap_init_cancel,
            ]
            .into_iter()
            .flatten()
            {
                cancel.store(true, Ordering::Release);
            }
        }
        ui.set_snap_connected(true);
        ui.set_snap_service_status(
            if exiting {
                "正在等待任务安全结束，随后退出后台"
            } else {
                "后台已连接 · 托盘和热键独立运行"
            }
            .into(),
        );
        ui.set_snap_model(value["model"].as_str().unwrap_or("uninitialized").into());
        ui.set_snap_task(value["task"].as_str().unwrap_or("idle").into());
        if let Some(hotkey) = value["hotkey"].as_str() {
            if !ui.get_snap_recording() && ui.get_snap_hotkey_draft() == ui.get_snap_hotkey() {
                ui.set_snap_hotkey_draft(hotkey.into());
            }
            ui.set_snap_hotkey(hotkey.into());
        }
        if let Some(autostart) = value["autostart"].as_bool() {
            if ui.get_snap_autostart_draft() == ui.get_snap_autostart() {
                ui.set_snap_autostart_draft(autostart);
            }
            ui.set_snap_autostart(autostart);
        }
        if requested
            || value["error"]
                .as_str()
                .is_some_and(|error| !error.is_empty())
        {
            ui.set_snap_error(value["error"].as_str().unwrap_or("").into());
        } else if !ui.get_snap_error().is_empty() {
            // C'-2：纯轮询收到正常状态（ok 且无错误）时清除旧错误文本
            //（如识别中的「正在识别」）；只在当前非空时写，避免每 2 秒
            // 空写引发无谓的界面刷新。
            ui.set_snap_error("".into());
        }
    }

    /// Event::Status 中 6 个 CONVERTER_* 协议前缀的解码与上屏（转 Markdown 页专属）；
    /// 命中任一前缀返回 true。前缀互斥，判定顺序与拆分前一致，各分支内的副作用
    /// 顺序原样保留。
    fn apply_converter_status(&self, ui: &AppWindow, text: &str) -> bool {
        if let Some(log) = text.strip_prefix("CONVERTER_LOG|") {
            let mut s = self.state.borrow_mut();
            push_event_log(&mut s.convert_logs, log.to_string());
            // S1-03：与通用日志的 log_dirty 同口径——只置脏，由轮询尾部按面板
            // 可见性统一重建上屏，一批 N 条日志只做一次整段重排。
            self.convert_log_dirty.set(true);
        } else if let Some(rest) = text.strip_prefix("CONVERTER_PREFLIGHT|") {
            let mut fields = rest.splitn(3, '|');
            let generation = fields.next().and_then(|value| value.parse::<u64>().ok());
            let ok = fields.next() == Some("1");
            let message = fields.next().unwrap_or_default().to_string();
            let accepted = generation.is_some_and(|generation| {
                let s = self.state.borrow();
                s.convert_preparing && s.convert_preflight_generation == generation
            });
            if accepted {
                let options = {
                    let mut s = self.state.borrow_mut();
                    s.convert_preparing = false;
                    s.convert_pending_options.take()
                };
                ui.set_convert_preparing(false);
                if ok {
                    if let Some(options) = options {
                        let closing = self.state.borrow().close_after;
                        if closing
                            || ui.get_busy()
                            || ui.get_convert_initializing()
                            || ui.get_convert_runtime_saving()
                        {
                            self.state.borrow_mut().convert_cancel = None;
                            ui.set_convert_status(
                                if closing {
                                    "正在等待其他任务安全结束，随后关闭"
                                } else {
                                    "其他任务或组件操作仍在进行；启动检查已通过，请稍后重新开始"
                                }
                                .into(),
                            );
                        } else {
                            launch_markdown_conversion(ui, &self.state, &self.out, options);
                        }
                    } else {
                        self.state.borrow_mut().convert_cancel = None;
                    }
                } else {
                    self.state.borrow_mut().convert_cancel = None;
                    if message == "用户取消检查" {
                        ui.set_convert_status("检查已取消，可稍后重试".into());
                    } else {
                        ui.set_convert_status("开始前检查失败，请修正后重试".into());
                        if ui.get_screen() == 5 {
                            ui.set_error_text(message.into());
                        } else {
                            ui.set_convert_detail_text(message.into());
                            ui.set_convert_detail_open(false);
                        }
                    }
                }
                if self.close_after_if_idle(ui) {
                    let _ = slint::quit_event_loop();
                }
            }
        } else if let Some(rest) = text.strip_prefix("CONVERTER_READINESS|") {
            let mut fields = rest.splitn(3, '|');
            let generation = fields.next().and_then(|value| value.parse::<u64>().ok());
            let ready = fields.next() == Some("1");
            let message = fields.next().unwrap_or_default();
            let current = generation.is_some_and(|generation| {
                let s = self.state.borrow();
                s.convert_readiness_generation == generation && s.convert_init_cancel.is_none()
            });
            if current && ui.get_busy() {
                // S1-05：当前代就绪结果在任务运行中无法上屏——标记「结果被错过」，
                // 由任务终态（DONE/FAIL 收尾）补查一次，避免页面长期停留过期状态。
                self.state.borrow_mut().convert_readiness_missed = true;
            }
            if current && !ui.get_busy() {
                ui.set_convert_ready(ready);
                if ready {
                    ui.set_convert_detail_text("".into());
                    ui.set_convert_detail_open(false);
                } else if !message.is_empty() {
                    ui.set_convert_detail_text(message.into());
                    ui.set_convert_detail_open(false);
                }
                ui.set_convert_status(
                    if ready {
                        "已安装组件就绪，可离线使用"
                    } else {
                        "可选组件未就绪，请主动初始化"
                    }
                    .into(),
                );
            }
        } else if let Some(progress) = text.strip_prefix("CONVERTER_INIT|") {
            ui.set_convert_status(progress.into());
        } else if let Some(rest) = text.strip_prefix("CONVERTER_STARTED|") {
            ui.set_convert_progress(-1.0);
            ui.set_convert_progress_note("正在转换".into());
            ui.set_convert_metrics(format!("待处理 {rest} 个文件").into());
        } else if let Some(rest) = text.strip_prefix("CONVERTER_FILE_STARTED|") {
            let mut fields = rest.splitn(4, '|');
            let index = fields.next().and_then(|v| v.parse::<usize>().ok());
            let total = fields.next().and_then(|v| v.parse::<usize>().ok());
            let relative = fields.next().unwrap_or_default();
            if let (Some(index), Some(total)) = (index, total) {
                // 先按 f64 求比例再写进度属性（保持 0.0-1.0 钳制）：
                // 分子分母各自 u16 饱和后再相除，会让 >65535 文件的
                // 大批次进度失真为 1.0（仅显示口径，不影响统计）。
                // [quality-baseline approved 2026-10-03] 显示用途转换，经用户裁定保留
                #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                let progress = if total == 0 {
                    -1.0
                } else {
                    (index.saturating_sub(1)) as f64 / total as f64
                }
                .clamp(0.0, 1.0) as f32;
                ui.set_convert_progress(progress);
                ui.set_convert_progress_note(format!("{index} / {total}").into());
                // T-22：记录当前文件，供 100ms 周期刷新拼出「正在处理 X · 耗时 Ns」。
                relative.clone_into(&mut self.state.borrow_mut().convert_current);
                ui.set_convert_metrics(format!("正在处理 {relative}").into());
            }
        } else if let Some(relative) = text.strip_prefix("CONVERTER_FILE_CANCELLED|") {
            let detail = format!("已取消：{relative}；未完成结果不提交");
            ui.set_convert_metrics(detail.clone().into());
            let mut state = self.state.borrow_mut();
            state.convert_current.clear();
            push_event_log(&mut state.convert_logs, detail);
            ui.set_convert_log_text(log_panel_text(&state.convert_logs).into());
        } else if let Some(rest) = text.strip_prefix("CONVERTER_FILE_FINISHED|") {
            let mut fields = rest.splitn(5, '|');
            let partial = fields.next() == Some("1");
            let success = fields.next() == Some("1");
            let relative = fields.next().unwrap_or_default();
            let message = fields.next().unwrap_or_default();
            let detail = format!(
                "{}：{}{}",
                if partial {
                    "部分提取"
                } else if success {
                    "已完成"
                } else {
                    "失败"
                },
                relative,
                if message.is_empty() {
                    String::new()
                } else {
                    format!(" · {message}")
                }
            );
            // 该文件已结束：周期刷新不再把它当作「正在处理」，改报运行耗时。
            self.state.borrow_mut().convert_current.clear();
            ui.set_convert_metrics(detail.clone().into());
            if !success || partial {
                let mut state = self.state.borrow_mut();
                push_event_log(&mut state.convert_logs, detail);
                ui.set_convert_log_text(log_panel_text(&state.convert_logs).into());
            }
        } else {
            return false;
        }
        true
    }

    /// SETTINGS_LOADED（启动快照）与 SETTINGS_READY（设置操作收尾）共用的上屏。
    /// `terminal` 为真表示这是设置操作自己的终态：清掉该操作置起的保存/初始化
    /// 标志，且仅在没有任何运行中任务/前置检查/截图初始化时消费 close_after
    /// （U-09/S1-01：任务运行中到达的收尾事件不得提前退出事件循环，关闭由任务
    /// 终态统一完成，镜像 on_close_requested 的守卫口径）；为假（启动快照）时
    /// 两者都不碰——迟到快照不得抹掉运行中的初始化状态。
    fn apply_settings_snapshot(&self, ui: &AppWindow, payload: &str, terminal: bool) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            return;
        };
        ui.set_settings_custom_dir(value["custom"].as_str().unwrap_or_default().into());
        ui.set_settings_downloaded_dir(value["downloaded"].as_str().unwrap_or_default().into());
        ui.set_settings_source(i32::from(value["downloaded_active"] == true));
        let active = value["active"].as_str().unwrap_or_default();
        ui.set_convert_runtime_dir(active.into());
        ui.set_convert_runtime_confirmed(!active.is_empty());
        ui.set_settings_status(
            if active.is_empty() {
                "请选择已有目录或主动下载 Xberg"
            } else {
                "配置已持久保存；后台自动连接，重启无需重新配置"
            }
            .into(),
        );
        if terminal {
            ui.set_convert_runtime_saving(false);
            ui.set_convert_initializing(false);
            self.state.borrow_mut().convert_init_cancel = None;
        }
        if !active.is_empty() {
            ensure_snap_supervisor(&self.state, &self.out);
            start_markdown_readiness(ui, &self.state, &self.out);
            start_snap_readiness(ui, &self.state, &self.out);
        }
        if terminal {
            // 设置操作自身的标志已在上面清理；统一守卫仍会等待其它任务、转换预检/
            // 初始化及截图初始化完成后再消费 close_after。
            if self.close_after_if_idle(ui) {
                let _ = slint::quit_event_loop();
            }
        }
    }

    /// 仅在所有 GUI 管理的操作均空闲后消费已确认的关闭请求。
    fn close_after_if_idle(&self, ui: &AppWindow) -> bool {
        let idle = !ui.get_busy()
            && !ui.get_convert_initializing()
            && !ui.get_convert_preparing()
            && !ui.get_convert_runtime_saving()
            && !ui.get_snap_initializing();
        idle && std::mem::take(&mut self.state.borrow_mut().close_after)
    }

    fn drain(&self, ui: &AppWindow) -> bool {
        let mut terminal = false;
        // 一轮回调返回后才会绘制，同一轮里只有最后一条状态文案会被看到：逐条 set_status
        // 只是把同一个标签反复标记重绘。暂停态丢弃的判断仍按事件原位置求值（语义不变）；
        // 自带收尾文案的事件（Ready/Done、Failed、ExtractDone）处理前清空缓冲，
        // 保证「后写的收尾文案覆盖先写的状态」与逐条写入时一致。
        let mut pending_status: Option<String> = None;
        for event in self.receiver.try_iter().take(256) {
            match event {
                Event::Status(text) => {
                    if let Some(message) = text.strip_prefix("SETTINGS_PROGRESS|") {
                        ui.set_settings_status(message.into());
                        continue;
                    }

                    if self.apply_converter_status(ui, &text) {
                        continue;
                    }
                    if !ui.get_paused() {
                        pending_status = Some(text);
                    }
                }
                Event::Log(text) => self.push_log(text),
                Event::Ready(path, summary) => {
                    pending_status = None;
                    terminal = true;
                    self.finish_task(ui, path, &summary, true);
                }
                Event::Done(path, summary) => {
                    pending_status = None;
                    terminal = true;
                    self.finish_task(ui, path, &summary, false);
                }
                Event::Failed(error) => {
                    pending_status = None;
                    terminal = true;
                    // 用户点了“取消任务”时不要用红色错误条报同一个消息：取消是预期操作，不是故障。
                    let cancelled = {
                        let s = self.state.borrow();
                        s.control
                            .as_ref()
                            .is_some_and(|control| control.is_cancelled())
                    };
                    {
                        let mut s = self.state.borrow_mut();
                        s.control = None;
                        s.extracting = false;
                        s.runtime = RuntimeMode::Organizer;
                        s.md_pending = None;
                        s.git_shared = None;
                        push_event_log(&mut s.logs, error.clone());
                        self.log_dirty.set(true);
                    }
                    ui.set_busy(false);
                    // 就绪与勾选可编辑性由紧随其后的计划快照按库真值给出（界面线程不开库）；
                    // 收尾期间先按不可执行处理，避免凭运行前的陈旧值放行（fail-closed）。
                    ui.set_ready(false);
                    ui.set_plan_editable(false);
                    ui.set_paused(false);
                    if cancelled {
                        ui.set_notice_text(error.into());
                        ui.set_status(
                            "任务已取消；已完成的操作不会自动回滚，详情见进度与日志".into(),
                        );
                    } else {
                        ui.set_error_text(error.into());
                        ui.set_status(
                            "任务已停止；已完成的操作不会自动回滚，详情见进度与日志".into(),
                        );
                    }
                    ui.set_progress(-1.0);
                    ui.set_progress_note("".into());
                    // 失败/取消后计划与摘要可能已部分变化：从任务库重载摘要与当前计划页
                    // （同一次后台开库），避免界面停留在失败前的旧数据。
                    // task 提取的借用安全见 reload_after_failed 注释。
                    reload_after_failed(ui, &self.state, &self.out);
                    // U-10：解压失败/取消后同样刷新失败列表，保留本次已产生的失败明细
                    // 供查看（仅解压任务存在时；其他工具的失败与该列表无关）。
                    if self.state.borrow().extract_task.is_some() {
                        let page = self.state.borrow().fail_page;
                        request_fail_page(&self.state, page);
                    }
                    if self.close_after_if_idle(ui) {
                        let _ = slint::quit_event_loop();
                    }
                }
                Event::SelectionSaved(path, saved, error) => {
                    let mut s = self.state.borrow_mut();
                    s.pending_selection = s.pending_selection.saturating_sub(1);
                    if let Some(error) = error {
                        ui.set_error_text(error.into());
                        s.selection_failed = true;
                    }
                    // 勾选写入由单线程按用户操作顺序落库；仍有写入在途时，只保留回调中的最新
                    // 即时选择，避免较早完成事件把用户刚取消的勾选重新显示为选中。
                    let mut batch_failed = false;
                    if s.task.as_ref() == Some(&path) {
                        if s.pending_selection == 0 {
                            if let Some((id, selected)) = saved {
                                mutate_plan_row(ui, id, |row| row.selected = selected);
                            }
                        }
                        if s.pending_selection == 0 && !ui.get_busy() {
                            // 失败可能发生在本轮任何一次勾选（不一定最后一个事件），只要
                            // pending 归零且出现过失败，就重载当前页让界面回到数据库真实状态。
                            batch_failed = std::mem::take(&mut s.selection_failed);
                        }
                    }
                    if batch_failed {
                        let start = s.page_starts.get(s.page).copied().unwrap_or(0);
                        let (page, filter) = (s.page, s.plan_filter.clone());
                        let plan_load = s.plan_load.clone();
                        // 重载页面的快照顺带按库真值修正 ready/可勾选（勾选保存失败不能把
                        // 「开始执行」一直禁用，见页面分支的说明）。
                        load_plan_state(
                            &self.out,
                            &plan_load,
                            path,
                            ui.get_directory().to_string(),
                            PlanQuery::page(start, page, filter, false),
                        );
                    } else if s.task.as_ref() == Some(&path)
                        && s.pending_selection == 0
                        && s.plan_recompute_inflight == 0
                        && !ui.get_busy()
                    {
                        // C-01：勾选批次落库后用既有分析资料重算受影响的计划（后台线程，
                        // 不阻塞界面）；重算完成前禁止执行旧计划，失败后要求重新分析。
                        let out = self.out.clone();
                        let recompute_path = path.clone();
                        // 在途勾选期间错过重算结果呈现时，这里补一次计划页重载
                        //（C-01：依赖改变必须显示给用户；cancelled=0 的后续重算
                        // 不会再次触发呈现）。
                        let recompute_dirty = std::mem::take(&mut s.plan_recompute_dirty);
                        if recompute_dirty {
                            let start = s.page_starts.get(s.page).copied().unwrap_or(0);
                            let (page, filter) = (s.page, s.plan_filter.clone());
                            let plan_load = s.plan_load.clone();
                            load_plan_state(
                                &self.out,
                                &plan_load,
                                path.clone(),
                                ui.get_directory().to_string(),
                                PlanQuery::page(start, page, filter, false),
                            );
                        }
                        s.plan_recompute_inflight += 1;
                        ui.set_ready(false);
                        ui.set_plan_editable(false);
                        std::thread::spawn(move || {
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    engine::recompute_plan(&recompute_path)
                                }));
                            let result = match result {
                                Ok(Ok(cancelled)) => Ok(cancelled),
                                Ok(Err(error)) => Err(format!("重算受影响计划失败：{error:#}")),
                                Err(_) => Err("重算受影响计划时后台操作意外退出".into()),
                            };
                            let _ = out.send(Event::PlanRecomputed(recompute_path, result));
                        });
                        // 重算完成后再恢复就绪；提前取得的快照不能放行旧计划。
                    }
                }
                Event::PlanRecomputed(path, result) => {
                    // C-01：依赖变化只在仍显示该任务、勾选批次已落库且尚未进入执行时
                    // 呈现；新勾选在途时记脏标志，待 pending 归零的 SelectionSaved
                    // 补一次重载（否则行状态变化会因 cancelled=0 的后续重算而永久
                    // 不上屏）；已切任务的迟到事件不碰界面。其它工具运行中仍须消费
                    // 对应重算结果，否则在途计数会永久阻止整理计划执行。
                    if self.state.borrow().task.as_ref() == Some(&path) {
                        let busy = ui.get_busy();
                        let mut s = self.state.borrow_mut();
                        s.plan_recompute_inflight = s.plan_recompute_inflight.saturating_sub(1);
                        let cancelled = match result {
                            Ok(cancelled) => cancelled,
                            Err(error) => {
                                s.plan_recompute_failed = true;
                                ui.set_ready(false);
                                if !busy {
                                    ui.set_error_text(error.into());
                                }
                                continue;
                            }
                        };
                        if s.pending_selection != 0 {
                            s.plan_recompute_dirty = true;
                            drop(s);
                        } else if cancelled > 0 {
                            if !busy {
                                ui.set_status(
                                    format!(
                                        "已按取消的勾选重算：{cancelled} 个依赖项失效，转为未勾选"
                                    )
                                    .into(),
                                );
                            }
                            // 重载当前计划页让重算后的行状态可见（与勾选保存失败的
                            // 重载同一模式，页面代际机制会丢弃迟到结果）。
                            let page = (
                                s.page_starts.get(s.page).copied().unwrap_or(0),
                                s.page,
                                s.plan_filter.clone(),
                                s.plan_load.clone(),
                            );
                            drop(s);
                            load_plan_state(
                                &self.out,
                                &page.3,
                                path.clone(),
                                ui.get_directory().to_string(),
                                PlanQuery::page(page.0, page.1, page.2, false),
                            );
                        } else {
                            drop(s);
                        }
                        let s = self.state.borrow();
                        if !busy
                            && s.plan_recompute_inflight == 0
                            && s.pending_selection == 0
                            && !s.plan_recompute_failed
                        {
                            load_plan_state(
                                &self.out,
                                &s.plan_load,
                                path,
                                ui.get_directory().to_string(),
                                PlanQuery::readiness(),
                            );
                        }
                    }
                }
                Event::PlanPage(path, mut actions, page, gen, filter, state_gen, snapshot) => {
                    if self.state.borrow().task.as_ref() != Some(&path) {
                        continue;
                    }
                    let mut s = self.state.borrow_mut();
                    // 过期事件：页面代际须仍是当前请求（只取快照的请求不动页面代际）。
                    let latest = s.plan_load.page.load(Ordering::Acquire);
                    if !plan_page_event_accepted(
                        gen,
                        latest,
                        filter.as_deref(),
                        s.plan_filter.as_deref(),
                    ) {
                        continue;
                    }
                    let more = actions.len() > 100;
                    actions.truncate(100);
                    if more && s.page_starts.len() <= page + 1 {
                        if let Some(last) = actions.last() {
                            s.page_starts.push(last.id);
                        }
                    }
                    s.page = page;
                    // 只有真的还有上一页/下一页时才让按钮可用，避免点了没有任何反应。
                    ui.set_plan_prev_enabled(page > 0);
                    ui.set_plan_next_enabled(more);
                    ui.set_plan_page_label(format!("第 {} 页 · 每页最多 100 条", page + 1).into());
                    let rows = actions
                        .into_iter()
                        .map(|a| PlanRow {
                            id: a.id.to_string().into(),
                            selected: a.selected,
                            kind: match a.kind {
                                ActionKind::Delete => "删除",
                                ActionKind::Move => "移动/重命名",
                                ActionKind::EmptyDirectory => "空目录复查",
                            }
                            .into(),
                            source: a.source.into(),
                            target: a
                                .target
                                .unwrap_or_else(|| {
                                    a.keeper
                                        .as_ref()
                                        .map(|v| format!("保留 {}", v.0))
                                        .unwrap_or_default()
                                })
                                .into(),
                            // H-05/C-07：空目录清理是整理收尾的强制动作，行内明示不可取消，
                            // 与复选框的禁用口径一致（C-11 如实显示）。
                            reason: if matches!(a.kind, ActionKind::EmptyDirectory) {
                                format!("{}（整理完成后强制清理，不可取消）", a.reason)
                            } else {
                                a.reason
                            }
                            .into(),
                            state: plan_row_state(a.selected, a.state.as_str()).into(),
                        })
                        .collect::<Vec<_>>();
                    ui.set_plans(Rc::new(VecModel::from(rows)).into());
                    // 失败收尾的请求顺带取回摘要：与页面同一次开库，界面线程不再自己开库。
                    if let Some(summary) = snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.summary.as_ref())
                    {
                        apply_summary(ui, &mut s, summary);
                    }
                    // 页面重建后同步一次就绪：勾选保存失败触发重载时，不能让「开始执行」
                    // 因为一次瞬时数据库失败而一直禁用。快照按当前配置/目录纯比较判定。
                    if !ui.get_busy()
                        && s.pending_selection == 0
                        && s.plan_recompute_inflight == 0
                        && !s.plan_recompute_failed
                        && s.task.as_ref() == Some(&path)
                        && s.plan_load.state.load(Ordering::Acquire) == state_gen
                    {
                        apply_plan_readiness(ui, &s, snapshot.as_ref());
                    }
                }
                Event::PlanState(path, gen, snapshot) => {
                    let s = self.state.borrow();
                    if s.task.as_ref() != Some(&path) {
                        continue;
                    }
                    // 过期快照：更新的请求在途，低代际结果不得改写当前就绪状态。
                    if s.plan_load.state.load(Ordering::Acquire) != gen {
                        continue;
                    }
                    // 运行中与勾选在途：状态栏文案归任务事件与勾选流程，就绪重算不得改写。
                    if ui.get_busy()
                        || s.pending_selection != 0
                        || s.plan_recompute_inflight != 0
                        || s.plan_recompute_failed
                    {
                        continue;
                    }
                    apply_plan_readiness(ui, &s, snapshot.as_ref());
                }
                Event::Notice(text) => {
                    if let Some(payload) = text.strip_prefix("SETTINGS_LOADED|") {
                        // S1-01：启动线程的设置快照不是任何设置操作的终态——只刷新
                        // 界面字段，不清进行中的保存/初始化标志、不消费 close_after，
                        // 关闭留给真正在跑的任务或初始化自己的终态事件（U-09）。
                        self.apply_settings_snapshot(ui, payload, false);
                        continue;
                    }
                    if let Some(payload) = text.strip_prefix("SETTINGS_READY|") {
                        self.apply_settings_snapshot(ui, payload, true);
                        continue;
                    }

                    if let Some(rest) = text.strip_prefix("CONVERTER_RUNTIME_SAVED|") {
                        let mut fields = rest.splitn(2, '|');
                        let generation = fields.next().and_then(|value| value.parse::<u64>().ok());
                        let path = fields.next().unwrap_or_default();
                        let accepted = generation.is_some_and(|generation| {
                            let s = self.state.borrow();
                            s.convert_readiness_generation == generation
                                && ui.get_convert_runtime_saving()
                        });
                        if accepted {
                            ui.set_convert_runtime_saving(false);
                            ui.set_convert_runtime_confirmed(true);
                            ui.set_convert_runtime_dir(path.into());
                            ui.set_convert_status("正在检查已安装组件…".into());
                            start_markdown_readiness(ui, &self.state, &self.out);
                            start_snap_readiness(ui, &self.state, &self.out);
                            ui.set_notice_text("共享 Xberg 目录已保存，重启后自动恢复".into());
                            if self.close_after_if_idle(ui) {
                                let _ = slint::quit_event_loop();
                            }
                        }
                    } else if text == "CONVERTER_INIT_OK" {
                        self.state.borrow_mut().convert_init_cancel = None;
                        ui.set_convert_initializing(false);
                        ui.set_convert_ready(false);
                        ui.set_convert_status("初始化完成，正在重新检查可选组件…".into());
                        if ui.get_screen() == 5 {
                            ui.set_notice_text(
                                "转 Markdown 组件初始化完成，正在校验运行目录".into(),
                            );
                        }
                        let closing = self.state.borrow().close_after;
                        if self.close_after_if_idle(ui) {
                            let _ = slint::quit_event_loop();
                        } else if !closing {
                            start_markdown_readiness(ui, &self.state, &self.out);
                        }
                    } else if let Some(error) = text.strip_prefix("CONVERTER_INIT_CANCELLED|") {
                        self.state.borrow_mut().convert_init_cancel = None;
                        ui.set_convert_initializing(false);
                        ui.set_convert_ready(false);
                        ui.set_convert_status("初始化已取消，可稍后重试".into());
                        if ui.get_screen() == 5 {
                            ui.set_notice_text(format!("转 Markdown 初始化已取消：{error}").into());
                        }
                        if self.close_after_if_idle(ui) {
                            let _ = slint::quit_event_loop();
                        }
                    } else {
                        ui.set_notice_text(text.into());
                    }
                }
                // Error 更新错误文案。
                // 计划筛选曾乐观置「加载中…」：失败必须恢复分页控件，避免永久中间态。
                Event::Error(text) => {
                    if let Some(error) = text.strip_prefix("SETTINGS_LOAD_ERR|") {
                        // S1-01：启动线程装载设置失败只落到设置页状态行——它不是
                        // 设置操作的终态，不动运行中标志、不消费 close_after。
                        ui.set_settings_status(error.into());
                        continue;
                    }

                    if let Some(error) = text.strip_prefix("SETTINGS_ERROR|") {
                        ui.set_settings_status(error.into());
                        ui.set_convert_initializing(false);
                        ui.set_convert_runtime_saving(false);
                        self.state.borrow_mut().convert_init_cancel = None;
                        // U-09/S1-01：任务运行中不得消费 close_after 提前退出，关闭由
                        // 任务终态统一完成（镜像 on_close_requested 的守卫口径）。
                        if self.close_after_if_idle(ui) {
                            let _ = slint::quit_event_loop();
                        }
                        continue;
                    }

                    if let Some(rest) = text.strip_prefix("CONVERTER_RUNTIME_ERR|") {
                        let mut fields = rest.splitn(3, '|');
                        let generation = fields.next().and_then(|value| value.parse::<u64>().ok());
                        let error = fields.next().unwrap_or_default();
                        let accepted = generation.is_some_and(|generation| {
                            let s = self.state.borrow();
                            s.convert_readiness_generation == generation
                                && ui.get_convert_runtime_saving()
                        });
                        if accepted {
                            ui.set_convert_runtime_saving(false);
                            ui.set_convert_runtime_confirmed(false);
                            ui.set_convert_ready(false);
                            ui.set_convert_detail_text(error.into());
                            ui.set_convert_detail_open(false);
                            ui.set_convert_status("Xberg 运行目录不可用，请修正后重试".into());
                            if matches!(ui.get_screen(), 5 | 6) {
                                let mut s = self.state.borrow_mut();
                                push_event_log(
                                    &mut s.convert_logs,
                                    format!("Xberg 运行目录校验失败：{error}"),
                                );
                                ui.set_convert_log_text(log_panel_text(&s.convert_logs).into());
                                // 直接上屏后文本已是最新，清掉脏标记避免尾部重复重建。
                                self.convert_log_dirty.set(false);
                                let first_item = error.split('；').next().unwrap_or(error);
                                let brief: String = first_item.chars().take(90).collect();
                                let message = if first_item.len() < error.len()
                                    || first_item.chars().count() > 90
                                {
                                    format!("{brief}… 详情见转换日志")
                                } else {
                                    brief
                                };
                                ui.set_error_text(message.into());
                            }
                            if self.close_after_if_idle(ui) {
                                let _ = slint::quit_event_loop();
                            }
                        }
                    } else if let Some(error) = text.strip_prefix("CONVERTER_INIT_ERR|") {
                        self.state.borrow_mut().convert_init_cancel = None;
                        ui.set_convert_initializing(false);
                        ui.set_convert_ready(false);
                        ui.set_convert_status("可选组件未就绪，请重试初始化".into());
                        if ui.get_screen() == 5 {
                            ui.set_convert_detail_text(error.into());
                            ui.set_convert_detail_open(false);
                        }
                        if self.close_after_if_idle(ui) {
                            let _ = slint::quit_event_loop();
                        }
                    } else {
                        ui.set_error_text(text.into());
                    }
                    if ui.get_has_task() && !ui.get_busy() {
                        let s = self.state.borrow();
                        let page = s.page;
                        ui.set_plan_prev_enabled(page > 0);
                        ui.set_plan_next_enabled(page + 1 < s.page_starts.len());
                        ui.set_plan_page_label(
                            format!("第 {} 页 · 每页最多 100 条", page + 1).into(),
                        );
                    }
                }
                Event::PlanLoadFailed(text, gen, filter) => {
                    // 计划加载失败带代际/筛选归属：请求已过期（用户切走筛选/翻页后
                    // 旧请求才失败）时不得把红条误报到当前正确视图上。
                    let accepted = {
                        let s = self.state.borrow();
                        s.plan_load.page.load(Ordering::Acquire) == gen
                            && s.plan_filter.as_deref() == filter.as_deref()
                    };
                    if accepted {
                        ui.set_error_text(text.into());
                    }
                }
                Event::ExtractCount(generation, count) => {
                    let current = self.state.borrow().extract_generation;
                    // 确认框仍开着（kind=1）且事件仍是当前代际才更新文案；用户已确认/返回、
                    // 或已改目录重新发起清点时，过期事件整体丢弃，不打扰运行中状态（X-02）。
                    if ui.get_confirm_kind() == 1 && ui.get_screen() == 2 && generation == current {
                        match count {
                            Ok(0) => {
                                // X-02：发现数为零时提示无可处理包，不启动空解压任务——
                                // 清点门禁保持（确认按钮保持禁用），用户点「返回检查」
                                // 重新选择目录或调整规则。
                                ui.set_confirm_text(
                                    "未发现可处理的压缩包（按当前扫描范围，已排除「解压失败」目录），本次不会解压任何文件；不启动空解压任务。点「返回检查」可重新选择目录或调整规则。"
                                        .into(),
                                );
                            }
                            Ok(n) => {
                                ui.set_confirm_text(
                                    extract_confirm_text(
                                        &format!("清点到 {n} 个压缩包（按当前扫描范围，已排除「解压失败」目录）。"),
                                        ui.get_directory().as_str(),
                                    )
                                    .into(),
                                );
                                // 数量已知：解除清点门禁，勾选后即可确认（X-02）。
                                ui.set_confirm_pending(false);
                            }
                            Err(error) => {
                                // 清点失败：关闭确认框只留红条——占位文案下继续允许确认
                                // 会让用户在数量未知的状态启动任务（X-02）。
                                ui.set_confirm_kind(0);
                                ui.set_confirm_pending(false);
                                ui.set_error_text(error.into());
                            }
                        }
                    }
                }
                Event::ExtractDone(_path, summary) => {
                    pending_status = None;
                    terminal = true;
                    // X-02 一段式收尾：只清运行态与展示摘要；不改 ready/has_task（目录整理两段式专用）。
                    {
                        let mut s = self.state.borrow_mut();
                        s.control = None;
                        s.extracting = false;
                    }
                    ui.set_busy(false);
                    ui.set_paused(false);
                    // 未完全解开的包不一定都成功移入「解压失败」（移动本身可能失败或未执行）：
                    // 计数只报「未完全解开」，隔离数量另报，二者都不当成完成解压（X-06）。
                    ui.set_quarantined_count(
                        i32::try_from(summary.archives_quarantined).unwrap_or(i32::MAX),
                    );
                    ui.set_progress(-1.0);
                    ui.set_progress_note("".into());
                    ui.set_metrics(summary.extract_description().into());
                    // X-05/S-02：完整成功即永久删除原包及分卷，收尾必须如实报出删除与保留口径
                    //（删除失败会让引擎中止整个任务走 Failed，因此这里不可能把失败报成成功）。
                    ui.set_status(
                        if summary.archives_failed == 0 {
                            format!(
                                "解压结束：成功 {} 包；完整成功解出的原压缩包及分卷已永久删除（不经回收站，不可恢复），已有文件与已落盘结果保留；详情见「进度与日志」。",
                                summary.archives_ok
                            )
                        } else {
                            format!(
                                "解压结束：成功 {} 包（原压缩包及分卷已永久删除，不可恢复）；未完全解开 {} 包（其中 {} 个原包或分卷已移入「解压失败」）保留待处理，详情见「进度与日志」。",
                                summary.archives_ok,
                                summary.archives_failed,
                                summary.archives_quarantined
                            )
                        }
                        .into(),
                    );
                    // U-10：任务结束后刷新失败列表当前页——收尾日志已全部落库，
                    // 此刻取数才能看到完整的失败明细（任务结束后仍可查看）。
                    if self.state.borrow().extract_task.is_some() {
                        let page = self.state.borrow().fail_page;
                        request_fail_page(&self.state, page);
                    }
                    if self.close_after_if_idle(ui) {
                        let _ = slint::quit_event_loop();
                    }
                }
                Event::MdNeedsConfirm(text) => {
                    pending_status = None;
                    terminal = true;
                    let close_after = self.state.borrow().close_after;
                    // M-07/M-11：输出/分片冲突转确认框；挂起的操作保存在 state.md_pending，
                    // 确认（kind=4）后重跑，返回检查则由下一次启动自然顶替。
                    {
                        let mut s = self.state.borrow_mut();
                        s.control = None;
                        push_event_log(&mut s.logs, format!("等待确认：{text}"));
                        self.log_dirty.set(true);
                    }
                    ui.set_busy(false);
                    ui.set_paused(false);
                    ui.set_progress(-1.0);
                    ui.set_progress_note("".into());
                    if close_after && self.close_after_if_idle(ui) {
                        // U-09：用户已确认「停止并关闭」。MD 扫描与冲突检查不经过任务
                        // 检查点，操作自然结束走到这里——此时应直接退出应用，不得再弹
                        // 覆盖确认把用户留在界面里，也不得残留 close_after。
                        let _ = slint::quit_event_loop();
                    } else if close_after || ui.get_confirm_kind() == 3 {
                        // U-09：「停止任务并关闭」确认框打开期间任务自然走到冲突点：
                        // busy/control 均已清空，保持关闭确认框不被破坏性覆盖确认顶替，
                        // 用户确认后经 on_confirmed(3) 的无任务分支直接退出；冲突详情
                        // 已写入运行日志（上面的 push_event_log）。
                        ui.set_status(
                            "任务已结束；检测到输出冲突（详情见运行日志），请先完成关闭确认".into(),
                        );
                    } else {
                        ui.set_confirm_text(text.into());
                        ui.set_acknowledge(false);
                        ui.set_confirm_kind(4);
                    }
                }
                Event::MdDone(text) => {
                    if let Some(rest) = text.strip_prefix("CONVERTER_DONE|") {
                        let mut fields = rest.split('|');
                        let success = fields.next().unwrap_or("0");
                        let partial = fields.next().unwrap_or("0");
                        let failed = fields.next().unwrap_or("0");
                        let skipped_existing = fields.next().unwrap_or("0");
                        let skipped_duplicate = fields.next().unwrap_or("0");
                        let stopped = fields.next() == Some("1");
                        let final_status = if stopped && failed != "0" {
                            "已停止，转换失败"
                        } else if stopped {
                            "已停止"
                        } else if partial != "0" || failed != "0" {
                            "转换部分失败"
                        } else {
                            "转换完成"
                        };
                        let total_elapsed = {
                            let mut s = self.state.borrow_mut();
                            // T-22：收尾统计的总耗时从任务起算到本事件到达时刻。
                            let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
                            s.convert_cancel = None;
                            s.runtime = RuntimeMode::Organizer;
                            s.convert_current.clear();
                            push_event_log(&mut s.convert_logs, format!(
                                "转 Markdown {final_status}：成功 {success}，部分提取 {partial}，失败 {failed}，已有结果跳过 {skipped_existing}，重复结果跳过 {skipped_duplicate}"
                            ));
                            self.log_dirty.set(true);
                            elapsed
                        };
                        // S1-05：收尾消费「busy 期间错过的就绪结果」标记。
                        let readiness_missed = {
                            let mut s = self.state.borrow_mut();
                            std::mem::take(&mut s.convert_readiness_missed)
                        };
                        ui.set_busy(false);
                        ui.set_paused(false);
                        ui.set_convert_progress(if stopped { -1.0 } else { 1.0 });
                        ui.set_convert_progress_note(final_status.into());
                        ui.set_convert_metrics(format!("成功 {success} · 部分提取 {partial} · 失败 {failed} · 已有结果跳过 {skipped_existing} · 重复结果跳过 {skipped_duplicate} · 总耗时 {total_elapsed:.1}s").into());
                        ui.set_convert_status(final_status.into());
                        ui.set_convert_log_text(
                            log_panel_text(&self.state.borrow().convert_logs).into(),
                        );
                        // 收尾日志直接上屏（S1-03 的例外路径），并清掉脏标记。
                        self.convert_log_dirty.set(false);
                        if ui.get_screen() == 5 {
                            ui.set_status(
                                if stopped && failed != "0" {
                                    "转 Markdown 已停止，但存在转换失败，详情见进度与日志"
                                } else if stopped {
                                    "转 Markdown 已停止；已完成的输出保留"
                                } else if partial != "0" || failed != "0" {
                                    "转 Markdown 部分失败，详情见进度与日志"
                                } else {
                                    "转 Markdown 已完成，详情见进度与日志"
                                }
                                .into(),
                            );
                        }
                        let closing = self.state.borrow().close_after;
                        if self.close_after_if_idle(ui) {
                            let _ = slint::quit_event_loop();
                        } else if !closing && readiness_missed {
                            // S1-05/T-05/T-06：busy 期间被丢弃的就绪结果在任务收尾
                            // 补查一次（此刻发起侧的忙碌拒绝已解除），页面就绪状态
                            // 回到当前真值，不得长期停留过期就绪状态。
                            start_markdown_readiness(ui, &self.state, &self.out);
                        }
                        continue;
                    }
                    if let Some(error) = text.strip_prefix("CONVERTER_FAIL|") {
                        let cancelled = self
                            .state
                            .borrow()
                            .convert_cancel
                            .as_ref()
                            .is_some_and(|flag| flag.load(Ordering::Acquire));
                        let total_elapsed = {
                            let mut s = self.state.borrow_mut();
                            // T-22：失败收尾的总耗时同样从任务起算到本事件到达时刻
                            //（与 DONE 分支同口径）。
                            let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
                            s.convert_cancel = None;
                            s.runtime = RuntimeMode::Organizer;
                            s.convert_current.clear();
                            push_event_log(
                                &mut s.convert_logs,
                                format!("转 Markdown 失败：{error}"),
                            );
                            self.log_dirty.set(true);
                            elapsed
                        };
                        // S1-05：失败收尾同样消费并补查「busy 期间错过的就绪结果」。
                        let readiness_missed = {
                            let mut s = self.state.borrow_mut();
                            std::mem::take(&mut s.convert_readiness_missed)
                        };
                        ui.set_busy(false);
                        ui.set_paused(false);
                        ui.set_convert_progress(-1.0);
                        ui.set_convert_progress_note("".into());
                        // 失败事件不带逐文件统计（Err 只携带原因），收尾统计行用
                        // 失败/停止口径 + 总耗时覆盖——否则「正在处理 X」「正在扫描…」
                        // 等运行中文案会一直残留到下一次任务（与 DONE 分支不对称）。
                        ui.set_convert_metrics(
                            format!(
                                "{} · 总耗时 {total_elapsed:.1}s",
                                if cancelled {
                                    "已停止"
                                } else {
                                    "转换失败"
                                }
                            )
                            .into(),
                        );
                        ui.set_convert_status(
                            if cancelled {
                                "已停止，可重新开始"
                            } else {
                                "转换失败，请查看日志"
                            }
                            .into(),
                        );
                        if ui.get_screen() == 5 {
                            if cancelled {
                                ui.set_notice_text("转 Markdown 已停止；已完成的输出保留".into());
                                ui.set_status("转 Markdown 已停止".into());
                            } else {
                                ui.set_error_text(error.into());
                                ui.set_status("转 Markdown 失败，详情见进度与日志".into());
                            }
                        }
                        ui.set_convert_log_text(
                            log_panel_text(&self.state.borrow().convert_logs).into(),
                        );
                        // 收尾日志直接上屏（S1-03 的例外路径），并清掉脏标记。
                        self.convert_log_dirty.set(false);
                        let closing = self.state.borrow().close_after;
                        if self.close_after_if_idle(ui) {
                            let _ = slint::quit_event_loop();
                        } else if !closing && readiness_missed {
                            // S1-05/T-05/T-06：与 DONE 分支同口径的补查。
                            start_markdown_readiness(ui, &self.state, &self.out);
                        }
                        continue;
                    }
                    pending_status = None;
                    terminal = true;
                    {
                        let mut s = self.state.borrow_mut();
                        s.control = None;
                        s.runtime = RuntimeMode::Organizer;
                        push_event_log(&mut s.logs, text.clone());
                        self.log_dirty.set(true);
                    }
                    ui.set_busy(false);
                    ui.set_paused(false);
                    ui.set_progress(-1.0);
                    ui.set_progress_note("".into());
                    ui.set_status(text.into());
                    if self.close_after_if_idle(ui) {
                        let _ = slint::quit_event_loop();
                    }
                }
                Event::GitDone(text) => {
                    pending_status = None;
                    terminal = true;
                    {
                        let mut s = self.state.borrow_mut();
                        s.control = None;
                        s.runtime = RuntimeMode::Organizer;
                        push_event_log(&mut s.logs, text.clone());
                        self.log_dirty.set(true);
                        // 收尾时把共享进度的最终状态定格上屏（G-13）。
                        if let Some(shared) = &s.git_shared {
                            if let Ok(state_text) = shared.state.lock() {
                                ui.set_git_state(state_text.as_str().into());
                            }
                            if let Ok(stage) = shared.stage.lock() {
                                ui.set_git_stage(stage.as_str().into());
                            }
                            ui.set_git_done(
                                i32::try_from(shared.done.load(Ordering::Relaxed))
                                    .unwrap_or(i32::MAX),
                            );
                            ui.set_git_total(
                                i32::try_from(shared.total.load(Ordering::Relaxed))
                                    .unwrap_or(i32::MAX),
                            );
                        }
                        s.git_shared = None;
                    }
                    ui.set_busy(false);
                    ui.set_paused(false);
                    ui.set_progress(-1.0);
                    ui.set_progress_note("".into());
                    ui.set_status(text.into());
                    if self.close_after_if_idle(ui) {
                        let _ = slint::quit_event_loop();
                    }
                }
            }
        }
        if let Some(text) = pending_status {
            ui.set_status(text.into());
        }
        terminal
    }
    /// Ready/Done 收尾：清运行态、上屏摘要与收尾文案，再让计划 worker 取回页面与就绪快照。
    /// 就绪与勾选可编辑性等快照落地后按任务库真值给出（界面线程不开库，H-02）；这里先按
    /// 「不可执行」上屏（fail-closed，不凭任务收尾前的陈旧值放行），收尾文案则按事件类型——
    /// 分析结束与整理结束是事件自身的确定性结论，不依赖库读取（C-11 如实展示）。
    fn finish_task(&self, ui: &AppWindow, path: PathBuf, summary: &Summary, analysis: bool) {
        // 新计划一律回到「全部」筛选：沿用上一任务的筛选可能恰好计数为 0，
        // 造成「空列表 + 高亮禁用胶囊」的死角。
        let (filter, organizer_visible) = {
            let mut s = self.state.borrow_mut();
            s.task = Some(path.clone());
            s.page = 0;
            s.page_starts = vec![0];
            s.control = None;
            s.applying = false;
            s.extracting = false;
            s.plan_filter = None;
            apply_summary(ui, &mut s, summary);
            (None, s.tool == Tool::Organizer)
        };
        ui.set_busy(false);
        ui.set_paused(false);
        ui.set_ready(false);
        ui.set_plan_editable(false);
        ui.set_has_task(true);
        if organizer_visible {
            ui.set_panel(1);
        }
        ui.set_plan_filter(0);
        ui.set_progress(-1.0);
        ui.set_progress_note("".into());
        ui.set_plan_prev_enabled(false);
        ui.set_plan_next_enabled(false);
        if organizer_visible {
            ui.set_status(
                if analysis {
                    "分析完成（只读）。请检查计划，然后确认执行整理。".into()
                } else {
                    format!(
                        "整理结束：已永久删除 {} 项（不可恢复）· 错误 {} 项；完整记录见「进度与日志」。",
                        summary.deleted, summary.errors
                    )
                }
                .into(),
            );
        }
        // 新任务加载落地前清空上一任务的旧行：action id 是各任务库各自的 rowid，
        // 旧行在此窗口内仍可交互，会把勾选写进新任务库的同 id 动作。
        ui.set_plans(Rc::new(VecModel::from(Vec::<PlanRow>::new())).into());
        // 页面与就绪快照同一次开库取回：快照落地后按库真值点亮或禁用执行（C-11）。
        load_plan_state(
            &self.out,
            &self.state.borrow().plan_load,
            path,
            ui.get_directory().to_string(),
            PlanQuery::page(0, 0, filter, false),
        );
        if self.close_after_if_idle(ui) {
            let _ = slint::quit_event_loop();
        }
    }
    /// 实时指标与进度：只在真正运行时刷新，空闲时不碰 metrics（否则耗时一直涨）。
    fn refresh_runtime(&self, ui: &AppWindow) {
        if !ui.get_busy() {
            return;
        }
        let s = self.state.borrow();
        let Some(control) = &s.control else {
            // 转 Markdown 任务无共享 Control（独立取消原子量）：共享进度条不得
            // 固定显示「准备中」——如实指向转 Markdown 页的实时进度（U-03/H-02）。
            if s.runtime == RuntimeMode::MarkdownConverter {
                ui.set_progress(-1.0);
                ui.set_progress_note("转换任务进行中，实时进度见转 Markdown 页".into());
                // T-22：运行期间实时显示当前文件与耗时（busy 门禁保证空闲不刷）；
                // 尚未进入单文件处理（扫描/文件间隙）时只报运行时长。
                let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
                ui.set_convert_metrics(
                    if s.convert_current.is_empty() {
                        format!("已运行 {elapsed:.1}s")
                    } else {
                        format!("正在处理 {} · 耗时 {elapsed:.1}s", s.convert_current)
                    }
                    .into(),
                );
            } else {
                ui.set_progress(-1.0);
                ui.set_progress_note("准备中".into());
            }
            return;
        };
        // Git：从共享进度读取全部界面字段（G-13；不写整理流程计数器）。
        if s.runtime == RuntimeMode::Git {
            if let Some(shared) = &s.git_shared {
                if let Ok(branch) = shared.branch.lock() {
                    ui.set_git_branch(branch.as_str().into());
                }
                if let Ok(upstream) = shared.upstream.lock() {
                    ui.set_git_upstream(upstream.as_str().into());
                }
                if let Ok(current) = shared.current.lock() {
                    ui.set_git_current(current.as_str().into());
                }
                if let Ok(stage) = shared.stage.lock() {
                    ui.set_git_stage(stage.as_str().into());
                }
                if let Ok(state_text) = shared.state.lock() {
                    ui.set_git_state(state_text.as_str().into());
                }
                let retry = shared.retry.load(Ordering::Relaxed);
                ui.set_git_retry(i32::try_from(retry).unwrap_or(i32::MAX));
                ui.set_git_retry_wait(
                    i32::try_from(shared.retry_wait.load(Ordering::Relaxed)).unwrap_or(i32::MAX),
                );
                let done = shared.done.load(Ordering::Relaxed);
                let total = shared.total.load(Ordering::Relaxed);
                ui.set_git_done(i32::try_from(done).unwrap_or(i32::MAX));
                ui.set_git_total(i32::try_from(total).unwrap_or(i32::MAX));
                let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
                if total > 0 {
                    // 进度分数为显示用途（Slint progress 即 f32），整数→浮点无受检 API。
                    // [quality-baseline approved 2026-10-03] 同类显示转换，经用户裁定保留
                    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                    let progress = (done as f64 / total as f64).clamp(0.0, 1.0) as f32;
                    ui.set_progress(progress);
                    ui.set_progress_note(format!("{done} / {total}").into());
                } else {
                    ui.set_progress(-1.0);
                    ui.set_progress_note("".into());
                }
                ui.set_metrics(
                    format!("已完成 {done} / {total} · 重试 {retry} 次 · 耗时 {elapsed:.1}s")
                        .into(),
                );
            }
            return;
        }
        // MD：worker 经 control 上报已完成/总数（不写整理的读取量指标）。
        if s.runtime == RuntimeMode::Md {
            let done = control.completed.load(Ordering::Relaxed);
            let planned = control.planned().unwrap_or(0);
            let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
            ui.set_metrics(format!("已处理 {done} / {planned} · 耗时 {elapsed:.1}s").into());
            if planned > 0 {
                // 进度分数为显示用途（Slint progress 即 f32），整数→浮点无受检 API。
                // [quality-baseline approved 2026-10-03] 同类显示转换，经用户裁定保留
                #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                let progress = (done as f64 / planned as f64).clamp(0.0, 1.0) as f32;
                ui.set_progress(progress);
                ui.set_progress_note(format!("{done} / {planned}").into());
            } else {
                ui.set_progress(-1.0);
                ui.set_progress_note("".into());
            }
            return;
        }
        let read = control.read_bytes.load(Ordering::Relaxed);
        let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
        let done = control.completed.load(Ordering::Relaxed);
        let scanned = control.scanned.load(Ordering::Relaxed);
        // 执行分母：worker 数出的「仍勾选且待执行」项数优先（U-03）；尚未数出时沿用摘要里的
        // 全量分母（旧口径是启动前同步数出，这里把这段等待挪到后台，界面先见到反馈）。
        let planned = control.planned().unwrap_or(s.planned);
        // 执行阶段不再扫描，写“扫描 0 个文件”只会让人误以为没扫到东西，这里只报执行进度。
        if s.applying {
            ui.set_metrics(
                format!("执行中：已处理 {done} / {planned} 项 · 耗时 {elapsed:.1}s").into(),
            );
        } else if s.extracting {
            // 解压不写整理流程的计数器（read_bytes/completed 只由哈希与计划执行累加），
            // 沿用整理口径会显示恒为 0 的「读取 / 平均读取 / 计划项」。这里只报解压
            // 自身真实可得的包进度与耗时（U-03：总量未知时不得编造数字）。
            ui.set_metrics(format!("解压中：已处理 {done} 包 · 耗时 {elapsed:.1}s").into());
        } else {
            // 吞吐速率为显示用途，u64→f64 的精度损失无意义。
            // [quality-baseline approved 2026-09-19] 显示用途转换，经用户裁定保留
            #[allow(clippy::cast_precision_loss)]
            let rate_mib_s = read as f64 / elapsed / 1_048_576.0;
            ui.set_metrics(format!("扫描 {} 个文件 · 读取 {} · 已处理 {} 个计划项 · 耗时 {:.1}s · 平均读取 {:.1} MiB/s",scanned,bytes(read),done,elapsed,rate_mib_s).into());
        }
        // 执行阶段按计划项计数；分析阶段总量未知（扫描/哈希/解压包大小不能提前预知）
        if s.applying && planned > 0 {
            // 进度分数为显示用途，整数→浮点的精度损失无意义。
            // [quality-baseline approved 2026-09-19] Slint progress 属性即 f32，整数→浮点无受检 API
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let progress = (done as f64 / planned as f64).clamp(0.0, 1.0) as f32;
            ui.set_progress(progress);
            ui.set_progress_note(format!("{done} / {planned} 项").into());
        } else if s.applying {
            // 执行阶段没有可执行的计划项（全部取消勾选或空计划）时，不要显示“已扫描 0 个文件”
            // 让人误以为没扫到东西。
            ui.set_progress(-1.0);
            ui.set_progress_note("没有待执行的计划项".into());
        } else if s.extracting {
            // 解压期间不显示整理口径的进度说明；已处理包数与耗时由上面的实时指标行给出。
            ui.set_progress(-1.0);
            ui.set_progress_note("".into());
        } else {
            ui.set_progress(-1.0);
            ui.set_progress_note(format!("已扫描 {scanned} 个文件 · 读取 {}", bytes(read)).into());
        }
    }
}

/// 摘要上屏：任务收尾与失败重载共用（失败重载的摘要与计划页取自同一次后台开库）。
fn apply_summary(ui: &AppWindow, state: &mut State, summary: &Summary) {
    ui.set_summary(summary.description().into());
    // 计数为显示用途，超出 i32 的极端值饱和显示即可。
    ui.set_plan_delete_count(i32::try_from(summary.planned_delete).unwrap_or(i32::MAX));
    ui.set_plan_move_count(i32::try_from(summary.planned_move).unwrap_or(i32::MAX));
    ui.set_plan_git_count(i32::try_from(summary.planned_git).unwrap_or(i32::MAX));
    ui.set_plan_empty_count(i32::try_from(summary.planned_empty).unwrap_or(i32::MAX));
    ui.set_metrics(
        format!(
            "扫描 {} 个文件 · 错误 {} 项 · 已永久删除 {} 项",
            summary.scanned, summary.errors, summary.deleted
        )
        .into(),
    );
    state.planned = summary.planned_delete + summary.planned_move + summary.planned_empty;
}

/// 初始界面状态：全部规则规格 + 默认配置（仅会话内存，不落盘）。测试与 run() 共用。
fn initial_state() -> Result<State> {
    let specs: Vec<RuleSpec> = serde_json::from_str(include_str!("../resources/rules.json"))?;
    let (fail_sender, fail_receiver) = mpsc::channel();
    let (snap_sender, snap_receiver) = mpsc::channel();
    let (acp_sender, acp_receiver) = mpsc::channel();
    Ok(State {
        config: Config::default(),
        specs,
        invalid_rule_inputs: HashMap::new(),
        section: "去重".into(),
        task: None,
        control: None,
        logs: VecDeque::new(),
        convert_logs: VecDeque::new(),
        page: 0,
        page_starts: vec![0],
        started: Instant::now(),
        close_after: false,
        pending_selection: 0,
        plan_recompute_inflight: 0,
        plan_recompute_failed: false,
        plan_recompute_dirty: false,
        applying: false,
        extracting: false,
        planned: 0,
        plan_filter: None,
        selection_failed: false,
        show_advanced: false,
        tool: Tool::Organizer,
        runtime: RuntimeMode::Organizer,
        md_pending: None,
        git_shared: None,
        convert_cancel: None,
        convert_current: String::new(),
        convert_init_cancel: None,
        convert_preparing: false,
        convert_pending_options: None,
        convert_readiness_generation: 0,
        convert_readiness_missed: false,
        convert_preflight_generation: 0,
        engine_overrides: None,
        plan_load: Arc::new(PlanLoadSync::default()),
        readiness_status_pending: Cell::new(false),
        extract_task: None,
        fail_page: 0,
        fail_page_starts: vec![0],
        fail_gen: 0,
        fail_sender,
        fail_receiver: RefCell::new(fail_receiver),
        extract_generation: 0,
        snap_init_cancel: None,
        snap_generation: 0,
        snap_commands: None,
        snap_foreground_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        snap_sender,
        snap_receiver: RefCell::new(snap_receiver),
        acp_observer: None,
        acp_sender,
        acp_receiver: RefCell::new(acp_receiver),
        acp_observation_generation: Arc::new(AtomicU64::new(0)),
        acp_request_id: 0,
        acp_pending_request: None,
        acp_service_error: None,
        acp_draft_edited: false,
        acp_explicit_stopped: false,
    })
}

/// 与 `run` 相同，但在事件循环启动前调用 `hook`——自动化测试用它安装驱动定时器，
/// 以真实回调路径驱动确认流，而不必访问任何内部状态。
pub fn run_with_pre_loop_hook(hook: impl FnOnce(&AppWindow) + 'static) -> Result<()> {
    run_with_engine_overrides(hook, None)
}

/// 允许测试注入状态目录；生产 GUI 走 `run()` / `run_with_pre_loop_hook`。
/// MD 整理与 Git 工具的回调装配（M/G 分区；R-01 参数直接在页面提供）。
/// 生产 `run_with_engine_overrides` 与无头测试 `with_gui` 共用：测试直接以真实回调驱动。
fn wire_md_git(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    {
        let weak = ui.as_weak();
        ui.on_md_select_subpage(move |page| {
            if let Some(ui) = weak.upgrade() {
                if page == 0 || page == 1 {
                    ui.set_md_subpage(page);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_md_choose_input(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(
                    &ui,
                    "选择要合并的目录",
                    ui.get_md_input_dir().as_str(),
                    |ui, path| {
                        ui.set_md_input_dir(path.display().to_string().into());
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_md_choose_output(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(
                    &ui,
                    "选择合并输出目录",
                    ui.get_md_output_dir().as_str(),
                    |ui, path| {
                        ui.set_md_output_dir(path.display().to_string().into());
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_md_split_choose_dir(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(
                    &ui,
                    "选择拆分输出目录",
                    ui.get_md_split_dir().as_str(),
                    |ui, path| {
                        ui.set_md_split_dir(path.display().to_string().into());
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_md_split_choose_file(move || {
            if let Some(ui) = weak.upgrade() {
                // U-07：文件选择从最近输入目录开始。
                let mut dialog = rfd::FileDialog::new()
                    .set_title("选择要拆分的 .md 文件")
                    .add_filter("Markdown", &["md"]);
                let entered = PathBuf::from(ui.get_md_split_file().to_string());
                let start = if entered.is_file() {
                    entered.parent().map(Path::to_path_buf)
                } else if entered.is_dir() {
                    Some(entered.clone())
                } else {
                    let fallback = PathBuf::from(ui.get_md_input_dir().to_string());
                    fallback.is_dir().then_some(fallback)
                };
                if let Some(start) = start {
                    dialog = dialog.set_directory(start);
                }
                if let Some(path) = dialog.pick_file() {
                    ui.set_md_split_file(path.display().to_string().into());
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_git_choose_repo(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(
                    &ui,
                    "选择 Git 项目目录",
                    ui.get_git_repo().as_str(),
                    |ui, path| {
                        ui.set_git_repo(path.display().to_string().into());
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_md_merge_start(move || {
            if let Some(ui) = weak.upgrade() {
                start_md_merge(&ui, &state, &out);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_md_split_start(move || {
            if let Some(ui) = weak.upgrade() {
                start_md_split(&ui, &state, &out);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_git_start(move || {
            if let Some(ui) = weak.upgrade() {
                start_git(&ui, &state, &out);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_git_stop(move || {
            // G-15：停止 = 不再处理新文件、不启动下一次 retry；当前 git 命令自然结束。
            // 其他任务运行中跨页点击时同样必须真正取消：control 为 None 时回落
            // 转换/初始化的取消原子量（与 on_confirmed(3)、on_cancel_task 同构，H-02）。
            let control = state.borrow().control.clone();
            let converter_cancel = state.borrow().convert_cancel.clone();
            let converter_init_cancel = state.borrow().convert_init_cancel.clone();
            let stopping_converter = control.is_none();
            let stopping_shared_non_git =
                control.is_some() && state.borrow().runtime != RuntimeMode::Git;
            if let Some(control) = control {
                control.cancel();
            } else if let Some(cancel) = converter_cancel {
                cancel.store(true, Ordering::Release);
            } else if let Some(cancel) = converter_init_cancel {
                cancel.store(true, Ordering::Release);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_status(if stopping_converter {
                    // 回落取消的是转 Markdown 任务：文案如实，不写 Git 专属描述。
                    "正在停止转 Markdown；正在取消当前文件".into()
                } else if stopping_shared_non_git {
                    // 取消的是其他工具的共享任务（整理/解压/MD）：通用取消口径。
                    "正在取消；当前操作完成后停止，不会继续后续操作".into()
                } else {
                    "正在停止：等待当前 git 命令结束后不再继续；已成功推送的文件保持成功".into()
                });
            }
        });
    }
}

/// 转 Markdown 的可选组件初始化与转换回调。组件状态、路径和任务均独立于旧工具。
fn settings_payload() -> Result<String, String> {
    let config = crate::xberg_settings::settings()?;
    Ok(serde_json::json!({
        "custom": config.custom.map(|p| crate::platform::display_path_text(&p.display().to_string())),
        "downloaded": config.downloaded.map(|p| crate::platform::display_path_text(&p.display().to_string())),
        "downloaded_active": config.source == crate::xberg_settings::Source::Downloaded,
        "active": crate::xberg_settings::load()?.map(|p| crate::platform::display_path_text(&p.display().to_string()))
    }).to_string())
}
fn send_settings_result(out: &EventSender, result: Result<String, String>) {
    let event = match result {
        Ok(payload) => Event::Notice(format!("SETTINGS_READY|{payload}")),
        Err(error) => Event::Error(format!("SETTINGS_ERROR|{error}")),
    };
    let _ = out.send(event);
}
fn wire_settings(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    let weak = ui.as_weak();
    let state = state.clone();
    let out = out.clone();
    ui.on_settings_action(move |action| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_busy()
            || ui.get_convert_initializing()
            || ui.get_convert_preparing()
            || ui.get_convert_runtime_saving()
        {
            return;
        }
        if action == "choose" {
            pick_directory(
                &ui,
                "选择 Xberg 运行目录",
                ui.get_settings_custom_dir().as_str(),
                |ui, path| {
                    ui.set_settings_custom_dir(path.display().to_string().into());
                },
            );
            return;
        }
        let action = action.to_string();
        let custom = PathBuf::from(ui.get_settings_custom_dir().as_str());
        let cancel =
            (action == "download").then(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
        ui.set_convert_initializing(action == "download");
        ui.set_convert_runtime_saving(action != "download");
        ui.set_settings_status("正在处理，请稍候…".into());
        state.borrow_mut().convert_init_cancel.clone_from(&cancel);
        let out = out.clone();
        std::thread::spawn(move || {
            let result = (|| {
                match action.as_str() {
                    "custom" => markdown_assets::save_runtime_dir(&custom)?,
                    "downloaded" => {
                        let saved = crate::xberg_settings::settings()?
                            .downloaded
                            .ok_or("尚未下载 Xberg")?;
                        crate::xberg_runtime::validate_assets(&saved, "engine")?;
                        crate::xberg_settings::select(crate::xberg_settings::Source::Downloaded)?;
                        // S3-01：转换页初始化入口移除后，历史「已下载但未写
                        // notice」的用户没有其它修复入口，切换来源时补齐
                        // notice（已存在则幂等），避免 readiness 卡死无解。
                        markdown_assets::ensure_document_notice()?;
                    }
                    "download" => {
                        let cancel = cancel
                            .as_ref()
                            .ok_or_else(|| "下载取消信号不可用".to_string())?;
                        markdown_assets::download_runtime(cancel, |message| {
                            let _ = out.send(Event::Status(format!("SETTINGS_PROGRESS|{message}")));
                        })?;
                    }
                    _ => return Err("未知设置操作".into()),
                }
                settings_payload()
            })();
            send_settings_result(&out, result);
        });
    });
}

fn wire_markdown_converter(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_convert_choose_runtime(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(
                    &ui,
                    "选择 Xberg 运行目录",
                    ui.get_convert_runtime_dir().as_str(),
                    |ui, path| {
                        let mut s = state.borrow_mut();
                        s.convert_readiness_generation =
                            s.convert_readiness_generation.wrapping_add(1);
                        ui.set_convert_runtime_confirmed(false);
                        ui.set_convert_ready(false);
                        ui.set_convert_status("目录已更换，请点击「使用此目录」进行校验".into());
                        ui.set_convert_runtime_dir(path.display().to_string().into());
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_convert_runtime_edited(move || {
            if let Some(ui) = weak.upgrade() {
                let mut s = state.borrow_mut();
                s.convert_readiness_generation = s.convert_readiness_generation.wrapping_add(1);
                ui.set_convert_runtime_confirmed(false);
                ui.set_convert_ready(false);
                ui.set_convert_status("目录已修改，请点击「使用此目录」重新校验".into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_convert_use_runtime(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let path = PathBuf::from(ui.get_convert_runtime_dir().as_str());
            if path.as_os_str().is_empty() {
                ui.set_convert_runtime_confirmed(false);
                ui.set_convert_ready(false);
                ui.set_convert_status("请先选择 Xberg 运行目录".into());
                return;
            }
            let generation = {
                let mut s = state.borrow_mut();
                s.convert_readiness_generation = s.convert_readiness_generation.wrapping_add(1);
                s.convert_readiness_generation
            };
            ui.set_convert_ready(false);
            ui.set_convert_runtime_confirmed(false);
            ui.set_convert_runtime_saving(true);
            ui.set_convert_status("正在校验并保存 Xberg 运行目录…".into());
            let out = out.clone();
            std::thread::spawn(move || {
                let result = markdown_assets::save_runtime_dir(&path);
                let event = match result {
                    Ok(()) => Event::Notice(format!(
                        "CONVERTER_RUNTIME_SAVED|{generation}|{}",
                        path.display()
                    )),
                    Err(error) => {
                        Event::Error(format!("CONVERTER_RUNTIME_ERR|{generation}|{error}"))
                    }
                };
                let _ = out.send(event);
            });
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_convert_choose_input(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(
                    &ui,
                    "选择待转换目录",
                    ui.get_convert_input_dir().as_str(),
                    |ui, path| {
                        ui.set_convert_input_dir(path.display().to_string().into());
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_convert_choose_output(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(
                    &ui,
                    "选择 Markdown 输出目录",
                    ui.get_convert_output_dir().as_str(),
                    |ui, path| {
                        ui.set_convert_output_dir(path.display().to_string().into());
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_convert_start(move || {
            if let Some(ui) = weak.upgrade() {
                start_markdown_conversion(&ui, &state, &out);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_convert_stop(move || {
            // 与 on_confirmed(3)/on_cancel_task/on_git_stop 同构：其他任务运行中
            // 跨页点击时必须真正取消共享任务，control 命中即取消之（H-02）。
            let control = state.borrow().control.clone();
            let converter_cancel = state.borrow().convert_cancel.clone();
            let converter_init_cancel = state.borrow().convert_init_cancel.clone();
            let stopping_shared_task = control.is_some();
            if let Some(control) = control {
                control.cancel();
            } else if let Some(cancel) = converter_cancel {
                cancel.store(true, Ordering::Release);
            } else if let Some(cancel) = converter_init_cancel {
                cancel.store(true, Ordering::Release);
            }
            if let Some(ui) = weak.upgrade() {
                if stopping_shared_task {
                    // 取消的是其他工具的共享任务：文案如实按通用取消口径。
                    ui.set_status("正在取消；当前操作完成后停止，不会继续后续操作".into());
                    return;
                }
                ui.set_convert_status(
                    if ui.get_convert_preparing() {
                        "正在取消启动前检查…"
                    } else if ui.get_convert_initializing() {
                        "正在取消初始化…"
                    } else {
                        "正在停止；正在取消当前文件"
                    }
                    .into(),
                );
                ui.set_status(
                    if ui.get_convert_preparing() {
                        "正在取消转 Markdown 启动前检查"
                    } else if ui.get_convert_initializing() {
                        "正在取消转 Markdown 组件初始化"
                    } else {
                        "正在停止转 Markdown；正在取消当前文件"
                    }
                    .into(),
                );
            }
        });
    }
}

fn selected_conversion_groups(ui: &AppWindow) -> Vec<FormatGroup> {
    let mut groups = Vec::new();
    if ui.get_convert_pdf() {
        groups.push(FormatGroup::Pdf);
    }
    if ui.get_convert_office() {
        groups.push(FormatGroup::Office);
    }
    if ui.get_convert_images() {
        groups.push(FormatGroup::Images);
    }
    if ui.get_convert_media() {
        groups.push(FormatGroup::Media);
    }
    if ui.get_convert_other() {
        groups.push(FormatGroup::Other);
    }
    groups
}

/// 异步检查已安装的可选组件；只读校验可能很慢，绝不阻塞 Slint 事件线程。
fn start_markdown_readiness(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    if ui.get_busy()
        || ui.get_convert_initializing()
        || ui.get_convert_preparing()
        || ui.get_convert_runtime_saving()
    {
        return;
    }
    if ui.get_convert_runtime_dir().trim().is_empty() {
        ui.set_convert_runtime_confirmed(false);
        ui.set_convert_ready(false);
        ui.set_convert_status("请先保存共享 Xberg 运行目录".into());
        return;
    }
    if !ui.get_convert_runtime_confirmed() {
        ui.set_convert_ready(false);
        ui.set_convert_status("请点击「保存目录」保存共享 Xberg 运行目录".into());
        return;
    }
    let groups = selected_conversion_groups(ui);
    if groups.is_empty() {
        ui.set_convert_ready(false);
        ui.set_convert_status("至少选择一种转换类型".into());
        return;
    }
    let generation = {
        let mut s = state.borrow_mut();
        s.convert_readiness_generation = s.convert_readiness_generation.wrapping_add(1);
        s.convert_readiness_generation
    };
    ui.set_convert_ready(false);
    ui.set_convert_status("正在检查已安装组件…".into());
    let out = out.clone();
    std::thread::spawn(move || {
        // T-05/T-06：按当前勾选的场景检查必需组件，缺失时开始按钮保持关闭。
        let result = markdown::readiness_for_groups(&groups);
        let (ok, message) = match result {
            Ok(()) => (true, "已安装组件就绪，可离线使用".to_string()),
            Err(error) => (false, error),
        };
        let _ = out.send(Event::Status(format!(
            "CONVERTER_READINESS|{generation}|{}|{message}",
            i32::from(ok)
        )));
    });
}

// S3-01：转换页的「初始化可选组件」入口已移除（XB-19：初始化集中在设置页，
// markdown_assets::initialize 只按文档成员校验，纯媒体用户点击会误报失败）。
// start_markdown_initialize 及其回调接线随之删除；组件状态统一由按场景的
// start_markdown_readiness 呈现，修复入口经页内「前往设置」进入设置页。

fn start_markdown_conversion(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    if ui.get_busy()
        || ui.get_convert_preparing()
        || ui.get_convert_initializing()
        || ui.get_convert_runtime_saving()
        || !ui.get_convert_ready()
    {
        return;
    }
    let input_dir = PathBuf::from(ui.get_convert_input_dir().as_str());
    let output_dir = PathBuf::from(ui.get_convert_output_dir().as_str());
    let Ok(timeout_secs) = ui.get_convert_timeout_secs().parse::<u64>() else {
        ui.set_error_text("单文件超时必须是正整数秒数".into());
        return;
    };
    if timeout_secs == 0 {
        ui.set_error_text("单文件超时必须大于 0".into());
        return;
    }
    if Instant::now()
        .checked_add(Duration::from_secs(timeout_secs))
        .is_none()
    {
        ui.set_error_text("单文件超时超出系统可表示范围".into());
        return;
    }
    let groups = selected_conversion_groups(ui);
    if groups.is_empty() {
        ui.set_error_text("至少选择一种转换类型".into());
        return;
    }
    let options = markdown::Options {
        input_dir,
        output_dir,
        flat: ui.get_convert_flat(),
        groups,
        timeout_secs,
    };
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let generation = {
        let mut s = state.borrow_mut();
        s.convert_cancel = Some(cancel.clone());
        s.convert_preparing = true;
        s.convert_pending_options = Some(options.clone());
        // S1-05：新任务不继承上一任务错过的就绪补查标记。
        s.convert_readiness_missed = false;
        s.convert_preflight_generation = s.convert_preflight_generation.wrapping_add(1);
        s.convert_preflight_generation
    };
    ui.set_ready(false);
    ui.set_error_text("".into());
    ui.set_notice_text("".into());
    ui.set_convert_preparing(true);
    ui.set_convert_status("正在检查所选组件和输入输出目录…".into());
    let out = out.clone();
    std::thread::spawn(move || {
        let result = (|| {
            if cancel.load(Ordering::Acquire) {
                return Err("用户取消检查".to_string());
            }
            let input = std::fs::canonicalize(&options.input_dir)
                .map_err(|error| format!("无法访问输入目录：{error}"))?;
            if !input.is_dir() {
                return Err("输入路径不是目录".to_string());
            }
            let output = std::fs::canonicalize(&options.output_dir)
                .map_err(|error| format!("无法访问输出目录：{error}"))?;
            if !output.is_dir() {
                return Err("输出路径不是目录".to_string());
            }
            if input == output {
                return Err("输入与输出目录不能相同".to_string());
            }
            markdown::readiness_for_groups(&options.groups)?;
            if cancel.load(Ordering::Acquire) {
                return Err("用户取消检查".to_string());
            }
            Ok(())
        })();
        let ok = result.is_ok();
        let message = result.err().unwrap_or_default();
        let _ = out.send(Event::Status(format!(
            "CONVERTER_PREFLIGHT|{generation}|{}|{message}",
            i32::from(ok)
        )));
    });
}

fn launch_markdown_conversion(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    out: &EventSender,
    options: markdown::Options,
) {
    let cancel = state
        .borrow()
        .convert_cancel
        .clone()
        .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
    {
        let mut s = state.borrow_mut();
        s.convert_preparing = false;
        s.convert_pending_options = None;
        s.runtime = RuntimeMode::MarkdownConverter;
        s.started = Instant::now();
        s.convert_current.clear();
    }
    ui.set_convert_preparing(false);
    ui.set_convert_detail_text("".into());
    ui.set_convert_detail_open(false);
    ui.set_busy(true);
    ui.set_paused(false);
    ui.set_ready(false);
    ui.set_error_text("".into());
    ui.set_notice_text("".into());
    ui.set_convert_metrics("正在扫描…".into());
    ui.set_convert_progress(-1.0);
    ui.set_convert_progress_note("正在扫描".into());
    ui.set_convert_status("正在转换…".into());
    let out = out.clone();
    std::thread::spawn(move || {
        let result = markdown::run(&options, &cancel, |event| {
            let text = match event {
                markdown::Event::Started { total } => format!("CONVERTER_STARTED|{total}"),
                markdown::Event::FileStarted {
                    relative,
                    index,
                    total,
                } => format!(
                    "CONVERTER_FILE_STARTED|{index}|{total}|{}",
                    relative.display()
                ),
                markdown::Event::FileFinished {
                    relative,
                    partial,
                    success,
                    message,
                } => format!(
                    "CONVERTER_FILE_FINISHED|{}|{}|{}|{}",
                    i32::from(partial),
                    i32::from(success),
                    relative.display(),
                    message.replace('|', "／")
                ),
                markdown::Event::Log(text) => {
                    let _ = out.send(Event::Status(format!("CONVERTER_LOG|{text}")));
                    return;
                }
                markdown::Event::FileCancelled { relative } => {
                    format!("CONVERTER_FILE_CANCELLED|{}", relative.display())
                }
            };
            let _ = out.send(Event::Status(text));
        });
        let text = match result {
            Ok(summary) => format!(
                "CONVERTER_DONE|{}|{}|{}|{}|{}|{}",
                summary.success,
                summary.partial,
                summary.failed,
                summary.skipped_existing,
                summary.skipped_duplicate,
                i32::from(summary.stopped)
            ),
            Err(error) => format!("CONVERTER_FAIL|{error}"),
        };
        let _ = out.send(Event::MdDone(text));
    });
}

/// 已安装资产只在工作线程校验；打开工具页不会联网。
fn start_snap_readiness(ui: &AppWindow, state: &Rc<RefCell<State>>, _out: &EventSender) {
    if ui.get_snap_initializing() {
        return;
    }
    let (generation, sender) = {
        let mut state = state.borrow_mut();
        state.snap_generation = state.snap_generation.wrapping_add(1);
        (state.snap_generation, state.snap_sender.clone())
    };
    ui.set_snap_ready(false);
    ui.set_snap_asset_status("正在离线校验已安装组件…".into());
    ui.set_snap_asset_detail("".into());
    ui.set_snap_error("".into());
    std::thread::spawn(move || {
        let _ = sender.send(SnapMessage::Readiness(
            generation,
            snap_ocr_assets::readiness(),
        ));
        wake_event_loop();
    });
}

fn start_snap_initialize(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    if ui.get_snap_ready() || ui.get_snap_initializing() {
        return;
    }
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (generation, sender) = {
        let mut state = state.borrow_mut();
        state.snap_generation = state.snap_generation.wrapping_add(1);
        state.snap_init_cancel = Some(cancel.clone());
        (state.snap_generation, state.snap_sender.clone())
    };
    ui.set_snap_ready(false);
    ui.set_snap_initializing(true);
    ui.set_snap_error("".into());
    ui.set_snap_asset_detail("".into());
    ui.set_snap_progress("正在准备组件…".into());
    ui.set_snap_asset_status("正在初始化可选组件…".into());
    std::thread::spawn(move || {
        let result = snap_ocr_assets::initialize(&cancel, |progress| {
            let _ = sender.send(SnapMessage::Progress(generation, progress));
            wake_event_loop();
        });
        let result = if cancel.load(Ordering::Acquire) {
            Err("用户取消初始化".to_owned())
        } else {
            result
        };
        let _ = sender.send(SnapMessage::Initialized(generation, result));
        wake_event_loop();
    });
}

/// 每条请求打开独立管道连接；服务重启时无需保存失效句柄。
#[cfg(windows)]
fn snap_pipe_request(request: &serde_json::Value) -> Result<serde_json::Value, String> {
    snap_pipe_request_on(
        &snap_ocr_assets::pipe_name(),
        request,
        SNAP_PIPE_READ_TIMEOUT,
    )
}

/// 截图服务读响应的 deadline（C-1）：监督线程是全部管道命令的唯一串行执行者，
/// 服务挂起时一次永久阻塞会让界面按钮永久禁用且无恢复路径（O-11/O-30）。
/// 心跳/控制类命令正常都在毫秒级返回；10 秒已远超正常窗口，仅用于兜底挂起。
#[cfg(windows)]
const SNAP_PIPE_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// 在指定命名管道上执行一次「单连接请求-响应」（C-1：读响应带 deadline，
/// 服务挂起时按超时错误返回而不是永久阻塞）。生产用固定管道名与默认超时；
/// 回归测试经管道名与超时参数注入，用本地服务端模拟挂起。
#[cfg(windows)]
fn snap_pipe_request_on(
    pipe: &str,
    request: &serde_json::Value,
    read_timeout: Duration,
) -> Result<serde_json::Value, String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut stream = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe)
        {
            Ok(stream) => break stream,
            // 服务只有一个管道实例。前一条响应写出后，它还需断开客户端并
            // 重新等待连接；立即发送下一条请求时应等待这个正常交接窗口。
            Err(error) if error.raw_os_error() == Some(231) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(format!("无法连接截图服务：{error}")),
        }
    };
    let mut payload =
        serde_json::to_vec(request).map_err(|error| format!("请求编码失败：{error}"))?;
    payload.push(b'\n');
    stream
        .write_all(&payload)
        .map_err(|error| format!("发送服务请求失败：{error}"))?;
    stream
        .flush()
        .map_err(|error| format!("刷新服务请求失败：{error}"))?;
    // 读响应放入独立线程、主线程限时 join（crate::process::join_with_deadline）：
    // 超时后取消原句柄上的未决读，让读线程以错误返回并退出（防线程泄漏）。
    let mut read_stream = stream
        .try_clone()
        .map_err(|error| format!("复制服务连接失败：{error}"))?;
    let reader = std::thread::Builder::new()
        .name("snap-pipe-read".into())
        .spawn(move || {
            let mut response = String::new();
            BufReader::new(&mut read_stream)
                .read_line(&mut response)
                .map(|_| response)
        })
        .map_err(|error| format!("启动响应读取线程失败：{error}"))?;
    let response = match crate::process::join_with_deadline(reader, read_timeout) {
        Some(Ok(response)) => response,
        Some(Err(error)) => return Err(format!("读取服务响应失败：{error}")),
        None => {
            cancel_pipe_io(&stream);
            return Err(format!(
                "截图服务响应超时（{read_timeout:.1?} 未返回）；请在设置页重试后台连接"
            ));
        }
    };
    if response.is_empty() {
        return Err("截图服务未返回响应".into());
    }
    serde_json::from_str(&response).map_err(|error| format!("截图服务响应格式错误：{error}"))
}

/// 取消句柄上的未决同步 I/O（C-1）：读线程阻塞在复制句柄的 read_line 上，
/// 对原句柄调用 CancelIoEx 即可跨线程取消该未决读（两句柄指向同一管道实例）。
#[cfg(windows)]
fn cancel_pipe_io(stream: &std::fs::File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::CancelIoEx;
    // SAFETY: stream 在调用期间存活且句柄有效；CancelIoEx 只取消该句柄关联管道
    // 实例上的未决 I/O（lpoverlapped 为空表示取消全部），不触碰其他资源；返回值
    // 仅指示是否存在未决 I/O，失败时读线程会随连接关闭自行退出，无需处理。
    unsafe {
        CancelIoEx(stream.as_raw_handle(), std::ptr::null());
    }
}

#[cfg(not(windows))]
fn snap_pipe_request(_request: &serde_json::Value) -> Result<serde_json::Value, String> {
    Err("截图服务仅支持 Windows".into())
}

/// 热键录制时 Shift+数字行/标点在 US 布局下的反向映射表（E-4）：带 Shift 的按键
/// 事件文本是布局映射后的符号（如 Shift+7 → "&"），录制必须还原回基础键。
/// 本表必须与 optional/snap-ocr-worker 侧录制逻辑的同一映射逐对保持一致
/// （US 布局主键区 Shift 符号共 21 对：数字行 10 对 + 标点 11 对）。
const SNAP_SHIFT_SYMBOL_BASE: [(char, char); 21] = [
    ('!', '1'),
    ('@', '2'),
    ('#', '3'),
    ('$', '4'),
    ('%', '5'),
    ('^', '6'),
    ('&', '7'),
    ('*', '8'),
    ('(', '9'),
    (')', '0'),
    ('~', '`'),
    ('_', '-'),
    ('+', '='),
    ('{', '['),
    ('}', ']'),
    ('|', '\\'),
    (':', ';'),
    ('"', '\''),
    ('<', ','),
    ('>', '.'),
    ('?', '/'),
];

/// 录制主键归一化（E-4）：取单字符文本为主键；Shift 按下且主键为布局符号时
/// 反向映射回基础键（"&" → '7'），其余原样返回。多字符文本（输入法组合等）返回 None。
fn normalize_recorded_primary_key(text: &str, shift: bool) -> Option<char> {
    let code = text
        .chars()
        .next()
        .filter(|_| text.chars().nth(1).is_none())?;
    if !shift {
        return Some(code);
    }
    Some(
        SNAP_SHIFT_SYMBOL_BASE
            .iter()
            .find_map(|(symbol, base)| (*symbol == code).then_some(*base))
            .unwrap_or(code),
    )
}

fn snap_named_hotkey(code: char) -> Option<&'static str> {
    use slint::platform::Key;
    [
        (Key::Backspace, "Backspace"),
        (Key::Tab, "Tab"),
        (Key::Return, "Return"),
        (Key::Escape, "Escape"),
        (Key::Space, "Space"),
        (Key::PageUp, "PageUp"),
        (Key::PageDown, "PageDown"),
        (Key::End, "End"),
        (Key::Home, "Home"),
        (Key::LeftArrow, "Left"),
        (Key::UpArrow, "Up"),
        (Key::RightArrow, "Right"),
        (Key::DownArrow, "Down"),
        (Key::Insert, "Insert"),
        (Key::Delete, "Delete"),
    ]
    .into_iter()
    .find_map(|(special, label)| (char::from(special) == code).then_some(label))
}

fn snap_attach_main_exe(ping_response: serde_json::Value) -> Result<serde_json::Value, String> {
    if ping_response["ok"] != true {
        return Err(ping_response["error"]
            .as_str()
            .unwrap_or("截图服务心跳失败")
            .to_owned());
    }
    if ping_response["shared_xberg_protocol"] != 2
        || ping_response["background_service_protocol"] != 1
    {
        return Err(
            "当前截图服务不支持共享 Xberg，请退出旧服务并更新截图组件；未启动第二个引擎".into(),
        );
    }
    let path = std::env::current_exe().map_err(|error| format!("无法定位主程序：{error}"))?;
    let response = snap_pipe_request(&serde_json::json!({
        "command": "attach-main-exe",
        "path": path,
    }))?;
    if response["ok"] != true {
        return Err(response["error"]
            .as_str()
            .unwrap_or("截图服务更新主程序路径失败")
            .to_owned());
    }
    Ok(ping_response)
}

fn snap_supervisor_ensure() -> Result<serde_json::Value, String> {
    let ping = serde_json::json!({"command": "ping"});
    if let Ok(response) = snap_pipe_request(&ping) {
        return snap_attach_main_exe(response);
    }
    crate::xberg_runtime::background_allowed()?;
    crate::xberg_settings::required()?;
    let executable = snap_ocr_assets::worker_install_path()?;
    let mut probe = std::process::Command::new(&executable);
    probe.arg("--capabilities");
    let output = crate::process::run_with_timeout(&mut probe, std::time::Duration::from_secs(10))
        .map_err(|error| format!("截图工作进程能力检查失败：{error}"))?;
    let capabilities: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_default();
    if !output.status.success()
        || output.stdout_truncated
        || capabilities["shared_xberg_protocol"] != 2
        || capabilities["background_service_protocol"] != 1
    {
        return Err(
            "截图工作进程尚未支持共享 Xberg，需更新可选组件发布物；不会启动旧版独占引擎".into(),
        );
    }
    let main_exe = std::env::current_exe().map_err(|error| format!("无法定位主程序：{error}"))?;
    let mut child = std::process::Command::new(&executable)
        .arg("--service")
        .arg("--main-exe")
        .arg(main_exe)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动截图服务：{error}"))?;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(150));
        if let Ok(response) = snap_pipe_request(&ping) {
            return snap_attach_main_exe(response);
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("截图服务状态不可读：{error}"))?
        {
            use std::io::Read;
            let mut message = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut message);
            }
            let detail = message.trim();
            return Err(if detail.is_empty() {
                format!("截图服务启动失败（退出码：{status}）")
            } else {
                format!("截图服务启动失败：{detail}")
            });
        }
    }
    Err("截图服务启动后未响应；请检查托盘及服务状态，再点击重连".into())
}

fn ensure_snap_supervisor(state: &Rc<RefCell<State>>, _out: &EventSender) {
    if state.borrow().snap_commands.is_none() {
        let (sender, receiver) = mpsc::channel();
        let output = state.borrow().snap_sender.clone();
        let foreground_busy = state.borrow().snap_foreground_busy.clone();
        std::thread::spawn(move || {
            let mut connected = false;
            loop {
                let command = match receiver.recv_timeout(Duration::from_millis(500)) {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) if connected => SnapCommand::Request(
                        serde_json::json!({"command": "get-state", "gui_pid":std::process::id(), "gui_busy":foreground_busy.load(Ordering::Acquire)}),
                    ),
                    Err(mpsc::RecvTimeoutError::Timeout)
                        if crate::xberg_runtime::background_allowed().is_ok()
                            && crate::xberg_settings::required().is_ok() =>
                    {
                        // XB-22：临时管道断线后自动重建后台；托盘退出 marker 或未配置
                        // Xberg 时不尝试复活，避免绕过 XB-23 或反复报错。
                        SnapCommand::Ensure
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let (mut result, requested) = match command {
                    SnapCommand::Ensure => (snap_supervisor_ensure(), true),
                    SnapCommand::Request(request) => (
                        snap_pipe_request(&request),
                        request["command"] != "get-state",
                    ),
                };
                if result.is_err() {
                    if let Err(stopped) = crate::xberg_runtime::background_allowed() {
                        result = Err(stopped);
                    }
                }
                connected = result.is_ok();
                if output
                    .send(SnapMessage::Service(result, requested))
                    .is_err()
                {
                    break;
                }
                wake_event_loop();
            }
        });
        state.borrow_mut().snap_commands = Some(sender);
    }
    snap_send_command(state, SnapCommand::Ensure);
}

fn snap_send_command(state: &Rc<RefCell<State>>, command: SnapCommand) {
    if let Some(sender) = &state.borrow().snap_commands {
        let _ = sender.send(command);
    }
}

fn wire_snap_ocr(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    let weak = ui.as_weak();
    let state_init = state.clone();
    ui.on_snap_initialize(move || {
        if let Some(ui) = weak.upgrade() {
            start_snap_initialize(&ui, &state_init);
        }
    });
    let weak = ui.as_weak();
    let state_cancel = state.clone();
    ui.on_snap_cancel_initialize(move || {
        if let Some(cancel) = &state_cancel.borrow().snap_init_cancel {
            cancel.store(true, Ordering::Release);
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_snap_asset_status("正在取消初始化…".into());
        }
    });
    let weak = ui.as_weak();
    let state_connect = state.clone();
    let out_connect = out.clone();
    ui.on_snap_connect(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_snap_request_pending(true);
            ui.set_snap_service_status("正在连接截图服务…".into());
            ensure_snap_supervisor(&state_connect, &out_connect);
        }
    });
    let weak = ui.as_weak();
    ui.on_snap_record_key(move |key, control, alt, shift, meta| {
        let Some(ui) = weak.upgrade() else { return };
        if !(control || alt || shift || meta) {
            return;
        }
        let Some(code) = normalize_recorded_primary_key(&key, shift) else {
            return;
        };
        let mut combination = String::with_capacity(24);
        if control {
            combination.push_str("Ctrl+");
        }
        if alt {
            combination.push_str("Alt+");
        }
        if shift {
            combination.push_str("Shift+");
        }
        if meta {
            combination.push_str("Win+");
        }
        if code.is_ascii_alphanumeric() {
            combination.push(code.to_ascii_uppercase());
        } else if let Some(number) = (code as u32)
            .checked_sub(char::from(slint::platform::Key::F1) as u32)
            .filter(|number| *number < 24)
        {
            // number ∈ 1..=24：直接按十进制拼出 F1–F24。
            let number = number + 1;
            combination.push('F');
            combination.push_str(&number.to_string());
        } else if let Some(name) = snap_named_hotkey(code) {
            combination.push_str(name);
        } else {
            return;
        }
        ui.set_snap_hotkey_draft(combination.into());
        ui.set_snap_recording(false);
        ui.set_snap_error("".into());
    });
    let weak = ui.as_weak();
    let state_save = state.clone();
    ui.on_snap_save_settings(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_snap_request_pending(true);
            snap_send_command(
                &state_save,
                SnapCommand::Request(serde_json::json!({
                    "command": "save-settings",
                    "hotkey": ui.get_snap_hotkey_draft().as_str(),
                    "autostart": ui.get_snap_autostart_draft(),
                })),
            );
        }
    });
    let weak = ui.as_weak();
    ui.on_snap_draft_autostart(move |enabled| {
        if let Some(ui) = weak.upgrade() {
            ui.set_snap_autostart_draft(enabled);
        }
    });
    let weak = ui.as_weak();
    let state_retry = state.clone();
    ui.on_snap_retry_load(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_snap_request_pending(true);
            snap_send_command(
                &state_retry,
                SnapCommand::Request(serde_json::json!({"command": "retry-load"})),
            );
        }
    });
    let weak = ui.as_weak();
    let state_capture = state.clone();
    ui.on_snap_capture(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_snap_request_pending(true);
            snap_send_command(
                &state_capture,
                SnapCommand::Request(serde_json::json!({"command": "capture"})),
            );
        }
    });
}

pub fn run_with_engine_overrides(
    hook: impl FnOnce(&AppWindow) + 'static,
    overrides: Option<EngineTestOverrides>,
) -> Result<()> {
    let ui = AppWindow::new()?;
    ui.set_system_dark(system_dark());
    let state = Rc::new(RefCell::new(initial_state()?));
    state.borrow_mut().engine_overrides = overrides;
    // 性能耗时打点（`perf-tracing` 特性，默认关闭）：日志写在状态目录的独立子目录里，
    // 测试注入状态目录时同样隔离在注入目录内。句柄绑到本函数作用域，退出前刷盘。
    // P-10：诊断日志常开；perf-tracing 启用时性能层由 logging::init 一并合并。
    let _diagnostic_log_guard = {
        let directory = state.borrow().engine_overrides.as_ref().map_or_else(
            || crate::config::state_dir().ok(),
            |o| Some(o.state_dir.clone()),
        );
        directory.as_deref().and_then(crate::logging::init)
    };
    let (channel, receiver) = mpsc::sync_channel::<Event>(256);
    let out = EventSender::new(channel);
    let tools = registry::tools()
        .iter()
        .map(|tool| ToolRow {
            id: tool.id.into(),
            name: tool.name.into(),
            summary: tool.summary.into(),
        })
        .collect::<Vec<_>>();
    ui.set_tool_count(i32::try_from(registry::tools().len()).unwrap_or(i32::MAX));
    ui.set_tools(Rc::new(VecModel::from(tools)).into());
    refresh(&ui, &state.borrow());
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_choose_directory(move || {
            if let Some(ui) = weak.upgrade() {
                // 两个工具页共用此回调：标题按当前工具区分，不得把「整理」文案带到解压页。
                let mut dialog =
                    rfd::FileDialog::new().set_title(if state.borrow().tool == Tool::Extract {
                        "选择需要解压的目录"
                    } else {
                        "选择需要整理的目录"
                    });
                // 已经输入过目录时从这里开始，省掉用户重新导航一遍。
                let entered = PathBuf::from(ui.get_directory().as_str());
                if entered.is_dir() {
                    dialog = dialog.set_directory(&entered);
                }
                if let Some(path) = dialog.pick_folder() {
                    ui.set_directory(path.display().to_string().into());
                    // 立即撤销旧执行权，再由后台快照恢复仍匹配的计划。
                    sync_ready_after_directory(&ui, &state.borrow(), &out);
                }
            }
        });
    }
    wire_sync(&ui, &state, &out);
    wire_md_git(&ui, &state, &out);
    wire_markdown_converter(&ui, &state, &out);
    wire_snap_ocr(&ui, &state, &out);
    wire_settings(&ui, &state, &out);
    wire_acp_http(&ui, &state);
    // 启动落在注册表第一个工具（P-02 顺序：递归解压在前）。必须在 wire_sync 之后调用：
    // 回调接线前的 invoke 是空调用，窗口会停在目录整理页。
    if std::env::args_os().any(|arg| arg == "--snap-ocr-settings") {
        ui.invoke_select_tool("snap-ocr".into());
    } else {
        ui.invoke_select_tool("recursive-extract".into());
    }
    wire_task_lifecycle(&ui, &state, &out);
    // 事件泵：100ms 轮询负责进度/耗时/主题刷新与兜底排空；结果事件由 worker 唤醒后
    // 立即排空（`EventSender` → `invoke_from_event_loop` → 事件循环线程上的排空钩子），
    // 因此结果上屏不受轮询周期限制（H-02）。绘制频率仍由这里的 100ms 上界约束。
    let pump = Rc::new(UiPump::new(receiver, state.clone(), out.clone()));
    let timer = slint::Timer::default();
    {
        let weak = ui.as_weak();
        let pump = pump.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(100),
            move || {
                if let Some(ui) = weak.upgrade() {
                    pump.run(&ui);
                }
            },
        );
    }
    // 唤醒钩子只在事件循环线程登记：worker 侧 post 的空闭包在这里执行，立即排空事件通道。
    // 事件循环退出后必须清掉钩子，否则本次运行的界面状态会被留在线程局部存储里。
    EVENT_LOOP_DRAIN.with(|slot| {
        let weak = ui.as_weak();
        let pump = pump.clone();
        *slot.borrow_mut() = Some(Rc::new(move || {
            if let Some(ui) = weak.upgrade() {
                pump.run(&ui);
            }
        }));
    });
    // First show the window. Rules stay in memory for this session only.
    ui.show()?;
    center_window(ui.window());
    // AH-06：窗口可交互后才投递自动连接；每次 GUI 打开仅投递一次。
    let acp_startup = state.clone();
    let acp_startup_timer = slint::Timer::default();
    acp_startup_timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(1),
        move || {
            start_acp_observer(&acp_startup, true);
        },
    );
    let startup = out.clone();
    std::thread::spawn(move || {
        // S1-01：启动快照用独立前缀，界面侧不把它当设置操作终态（不清标志、
        // 不消费 close_after），避免迟到快照在任务运行中提前退出或抹掉初始化状态。
        let event =
            match crate::xberg_runtime::resume_background().and_then(|()| settings_payload()) {
                Ok(payload) => Event::Notice(format!("SETTINGS_LOADED|{payload}")),
                Err(error) => Event::Error(format!("SETTINGS_LOAD_ERR|{error}")),
            };
        let _ = startup.send(event);
    });
    // 窗口刚映射时系统还会套用默认位置，稍后再居中一次，保证首屏就是居中的
    let centered = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(120), move || {
        if let Some(ui) = centered.upgrade() {
            center_window(ui.window());
        }
    });
    if std::env::args_os().any(|arg| arg == "--settings") {
        ui.invoke_navigation(7);
    }
    hook(&ui);
    let loop_result = slint::run_event_loop();
    // 退出后清掉唤醒钩子：它持有本次运行的 State 与事件通道，留着会跨运行泄漏。
    EVENT_LOOP_DRAIN.with(|slot| {
        let _ = slot.borrow_mut().take();
    });
    // AH-07：关闭 GUI 仅停止自己的只读观察，后台服务与在途操作继续完成。
    state.borrow_mut().acp_observer.take();
    // P-10：界面正常收场留痕（guard 在本函数结尾 drop 时刷盘）。
    tracing::info!(
        reason = loop_result
            .as_ref()
            .err()
            .map(ToString::to_string)
            .unwrap_or_default(),
        "图形界面退出"
    );
    loop_result?;
    Ok(())
}

/// 任务生命周期与窗口控制回调（确认、暂停、取消、勾选、计划翻页/筛选、
/// 打开任务目录、自绘标题栏拖动、关窗确认）：生产装配与无头测试装配共用同一
/// 接线，保证经 invoke 层驱动的用例走与产品完全一致的处理器。
fn wire_task_lifecycle(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    let (selection_writer, selection_queue) = mpsc::channel::<(PathBuf, i64, bool)>();
    let selection_out = out.clone();
    std::thread::spawn(move || {
        while let Ok((task, id, selected)) = selection_queue.recv() {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Database::open_existing(&task).and_then(|db| db.set_selected(id, selected))
            }));
            let result =
                result.unwrap_or_else(|_| Err(anyhow::anyhow!("保存勾选时后台操作意外退出")));
            let saved = result.is_ok().then_some((id, selected));
            let _ = selection_out.send(Event::SelectionSaved(
                task,
                saved,
                result.err().map(|error| format!("{error:#}")),
            ));
        }
    });
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_confirmed(move |kind| {
            if let Some(ui) = weak.upgrade() {
                if kind == 5 {
                    if ui.get_confirm_kind() != 5 || !can_request_acp_stop(&ui, &state.borrow()) {
                        return;
                    }
                    state.borrow_mut().acp_explicit_stopped = true;
                    ui.set_acp_explicit_stopped(true);
                    request_acp_action(
                        &ui,
                        &state,
                        AcpCommand::Stop,
                        "正在退出模型服务：停止接新请求，取消并等待在途任务安全收尾…",
                    );
                    return;
                }
                if kind == 3 {
                    state.borrow_mut().close_after = true;
                    // 各 GUI 操作可并行（例如旧工具任务与字体初始化），关闭必须请求全部可取消阶段停止。
                    let (control, converter_cancel, converter_init_cancel, snap_init_cancel) = {
                        let state = state.borrow();
                        (
                            state.control.clone(),
                            state.convert_cancel.clone(),
                            state.convert_init_cancel.clone(),
                            state.snap_init_cancel.clone(),
                        )
                    };
                    let has_running = control.is_some()
                        || converter_cancel.is_some()
                        || converter_init_cancel.is_some()
                        || snap_init_cancel.is_some()
                        || ui.get_busy()
                        || ui.get_convert_initializing()
                        || ui.get_convert_preparing()
                        || ui.get_convert_runtime_saving()
                        || ui.get_snap_initializing();
                    if let Some(control) = control {
                        control.cancel();
                    }
                    if let Some(cancel) = converter_cancel {
                        cancel.store(true, Ordering::Release);
                    }
                    if let Some(cancel) = converter_init_cancel {
                        cancel.store(true, Ordering::Release);
                    }
                    if let Some(cancel) = snap_init_cancel {
                        cancel.store(true, Ordering::Release);
                    }
                    if !has_running {
                        let _ = slint::quit_event_loop();
                        return;
                    }
                    ui.set_status("取消任务中，完成当前安全操作后关闭".into());
                } else if kind == 4 {
                    // M-07/M-11：覆盖确认后重跑挂起的 MD 操作（写入前重新扫描/规划）。
                    confirm_md_override(&ui, &state, &out);
                } else if kind == 2 {
                    start_task(&ui, &state, &out, true);
                } else if ui.get_screen() == 2 {
                    start_extract(&ui, &state, &out);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_pause_task(move||{if let Some(ui)=weak.upgrade(){
            let s=state.borrow();
            // H-02/G-13：Git 与转 Markdown 任务不接入暂停检查点（Git 逐文件提交、转换
            // 顶层串行），暂停对它们无效；如实提示而不是翻转 paused 冻结状态显示。
            if s.runtime==RuntimeMode::Git||s.runtime==RuntimeMode::MarkdownConverter||s.control.is_none(){
                drop(s);
                ui.set_status("当前工具的任务不支持暂停".into());
                return;
            }
            if let Some(control)=&s.control{let pause=!control.is_paused();control.pause(pause);ui.set_paused(pause);
                ui.set_status(if pause{"已请求暂停；正在运行的压缩包在完成后暂停，Hash 和整理操作在分块/文件边界暂停"}else{"继续处理"}.into());}
        }});
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_cancel_task(move || {
            // 不同 GUI 阶段可并行；独立请求所有取消信号，截图服务本身不受影响。
            let (control, converter_cancel, converter_init_cancel, snap_init_cancel) = {
                let state = state.borrow();
                (
                    state.control.clone(),
                    state.convert_cancel.clone(),
                    state.convert_init_cancel.clone(),
                    state.snap_init_cancel.clone(),
                )
            };
            if let Some(control) = control {
                control.cancel();
            }
            if let Some(cancel) = converter_cancel {
                cancel.store(true, Ordering::Release);
            }
            if let Some(cancel) = converter_init_cancel {
                cancel.store(true, Ordering::Release);
            }
            if let Some(cancel) = snap_init_cancel {
                cancel.store(true, Ordering::Release);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_status("正在取消；当前操作完成后停止，不会继续后续操作".into());
            }
        });
    }
    {
        let state = state.clone();
        let out = out.clone();
        let selection_writer = selection_writer.clone();
        let weak = ui.as_weak();
        ui.on_plan_toggle(move |id, selected| {
            let task = state.borrow().task.clone();
            if let (Some(task), Ok(id)) = (task, id.parse::<i64>()) {
                if let Some(ui) = weak.upgrade() {
                    if ui.get_busy() || state.borrow().plan_recompute_inflight != 0 {
                        return;
                    }
                    // 勾选保存中 ready 暂降，避免在途勾选时误点执行；其他项仍可在 Slint 侧继续编辑。
                    ui.set_ready(false);
                    // C-11：勾选后即时显示对应状态（取消勾选→已取消勾选，重新勾选→待执行），
                    // 不等数据库事件回来；保存失败时由 SelectionSaved/重载恢复真值。
                    patch_plan_row(&ui, id, selected);
                }
                state.borrow_mut().pending_selection += 1;
                if let Err(error) = selection_writer.send((task, id, selected)) {
                    let (task, _, _) = error.0;
                    let _ = out.send(Event::SelectionSaved(
                        task,
                        None,
                        Some("保存勾选时后台操作意外退出".into()),
                    ));
                }
            }
        });
    }
    {
        let state = state.clone();
        let out = out.clone();
        let weak = ui.as_weak();
        ui.on_plan_page(move |direction| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            // 目录原文随请求带走：页面自带的就绪快照同样按当前目录判定（见 classify_plan_snapshot）。
            let directory = ui.get_directory().to_string();
            let state = state.borrow();
            let Some(task) = state.task.clone() else {
                return;
            };
            let next = if direction < 0 {
                state.page.saturating_sub(1)
            } else {
                state.page + 1
            };
            if let Some(start) = state.page_starts.get(next).copied() {
                // 先发起加载，成功事件再提交 page：失败时 page/按钮保持与列表一致，避免前进软锁。
                load_plan_state(
                    &out,
                    &state.plan_load,
                    task,
                    directory,
                    PlanQuery::page(start, next, state.plan_filter.clone(), false),
                );
            }
        });
    }
    {
        // 计划类型筛选：""=全部，否则 snake_case kind
        let state = state.clone();
        let out = out.clone();
        let weak = ui.as_weak();
        ui.on_filter_plan(move |kind| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            // 目录原文随请求带走：页面自带的就绪快照同样按当前目录判定（见 classify_plan_snapshot）。
            let directory = ui.get_directory().to_string();
            let mut s = state.borrow_mut();
            let Some(task) = s.task.clone() else {
                return;
            };
            // Slint 胶囊已先改 plan-filter 显示；这里必须同步 Rust 状态，否则 PlanPage 事件会被拒。
            s.plan_filter = if kind.is_empty() {
                None
            } else {
                Some(kind.to_string())
            };
            s.page = 0;
            s.page_starts = vec![0];
            ui.set_plan_prev_enabled(false);
            ui.set_plan_next_enabled(false);
            ui.set_plan_page_label("加载中…".into());
            load_plan_state(
                &out,
                &s.plan_load,
                task,
                directory,
                PlanQuery::page(0, 0, s.plan_filter.clone(), false),
            );
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.on_open_report(move || {
            if let Some(path) = &state.borrow().task {
                if let Err(error) = open::that(path) {
                    if let Some(ui) = weak.upgrade() {
                        show_error(&ui, error);
                    }
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let drag = Rc::new(RefCell::new(None::<WindowDrag>));
        let anchor = drag.clone();
        ui.on_window_drag_start(move |x, y| {
            if let Some(ui) = weak.upgrade() {
                let window = ui.window();
                // 最大化状态下不在按下时立即还原：双击（无移动）要让 double-clicked 读到
                // 仍最大化，从而切换为还原；真正的拖动由 drag-move 的 restoring 分支还原。
                let restoring = window.is_maximized();
                let position = window.position();
                *anchor.borrow_mut() = Some(WindowDrag {
                    origin: (f64::from(position.x), f64::from(position.y)),
                    press: pointer_position().unwrap_or((f64::from(x), f64::from(y))),
                    restoring,
                });
            }
        });
        let weak = ui.as_weak();
        ui.on_window_drag_move(move |x, y| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut state = drag.borrow_mut();
            let Some(drag) = state.as_mut() else {
                return;
            };
            let pointer = pointer_position().unwrap_or((f64::from(x), f64::from(y)));
            if drag.restoring {
                // 按下时未还原（见 drag-start 注释）：真正的拖动在这里触发还原，
                // 等窗口离开最大化后再重新锚定，避免和系统还原位置互相覆盖。
                if ui.window().is_maximized() {
                    ui.window().set_maximized(false);
                    return;
                }
                let position = ui.window().position();
                drag.origin = (f64::from(position.x), f64::from(position.y));
                drag.press = pointer;
                drag.restoring = false;
                return;
            }
            // 拖动坐标为显示用途：饱和换算，越界值夹到 i32 边界。
            // [quality-baseline approved 2026-09-19] clamp 后截断数学上不可能；std 无 f64→i32 受检转换
            #[allow(clippy::cast_possible_truncation)]
            let drag_x = (drag.origin.0 + pointer.0 - drag.press.0)
                .clamp(f64::from(i32::MIN), f64::from(i32::MAX))
                .round() as i32;
            // [quality-baseline approved 2026-09-19] 同 drag_x：clamp 后截断不可能，无受检转换可用
            #[allow(clippy::cast_possible_truncation)]
            let drag_y = (drag.origin.1 + pointer.1 - drag.press.1)
                .clamp(f64::from(i32::MIN), f64::from(i32::MAX))
                .round() as i32;
            ui.window()
                .set_position(slint::PhysicalPosition::new(drag_x, drag_y));
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.window().on_close_requested(move||{
                if let Some(ui)=weak.upgrade(){if ui.get_busy() || ui.get_convert_initializing() || ui.get_convert_preparing() || ui.get_convert_runtime_saving() || ui.get_snap_initializing(){
                ui.set_confirm_text("任务或可选组件初始化仍在进行。确认后会请求取消，等待当前操作结束，再关闭窗口。已经完成的操作不会自动回滚。已经启动的截图服务不受影响。".into());
                ui.set_confirm_kind(3);ui.set_acknowledge(false);return slint::CloseRequestResponse::KeepWindowShown;
            }}
            if let Some(control)=&state.borrow().control{control.cancel();}
            if let Some(cancel)=&state.borrow().convert_cancel{cancel.store(true, Ordering::Release);}
            if let Some(cancel)=&state.borrow().convert_init_cancel{cancel.store(true, Ordering::Release);}
            // O-06/O-16：取消进行中的截图 OCR 初始化；已启动的服务独立存活，不随窗口关闭停止。
            if let Some(cancel)=&state.borrow().snap_init_cancel{cancel.store(true, Ordering::Release);}
            // Slint 1.17 的 CloseRequestResponse 只有 HideWindow / KeepWindowShown，
            // HideWindow 仅隐藏窗口、事件循环仍在跑；必须显式 quit 才能让进程真正退出。
            let _=slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
        });
    }
}
/// 默认入口：无额外装配钩子。
pub fn run() -> Result<()> {
    run_with_pre_loop_hook(|_| ())
}

#[cfg(test)]
mod gui_tests {
    //! 无头 GUI 测试：Slint 平台绑定初始化线程，因此所有用例经专用工作线程串行执行，
    //! 每个用例开始前重置为初始状态，互不干扰且与显示器/事件循环解耦。
    use super::*;
    use std::sync::{mpsc, Mutex, OnceLock};

    // 覆盖 AH-08/AH-09/AH-12：GUI 消费真实类型快照，不将 saved 冒充 running。
    #[test]
    fn acp_status_preserves_running_config_and_edited_draft() {
        with_gui(|app| {
            let running = ServiceConfig {
                executable: "agent.exe".into(),
                arguments: vec!["含 空格".into()],
                port: 8765,
            };
            let saved = ServiceConfig {
                port: 8766,
                ..running.clone()
            };
            let status = ServiceStatus {
                phase: crate::acp_api::ServicePhase::Ready,
                saved_config: Some(saved),
                running_config: Some(running),
                service_pid: Some(123),
                agent_pid: Some(456),
                executing: 2,
                waiting: 3,
                error: None,
            };
            apply_acp_status(&app.ui, &app.state, &status);
            assert!(app.ui.get_acp_ready());
            assert!(app.ui.get_acp_pending_apply());
            assert_eq!(app.ui.get_acp_port().as_str(), "8766");
            assert!(app.ui.get_acp_saved_config().contains("8766"));
            assert!(app.ui.get_acp_running_config().contains("8765"));
            assert_eq!(app.ui.get_acp_service_pid().as_str(), "123");
            assert_eq!(app.ui.get_acp_agent_pid().as_str(), "456");
            assert_eq!(app.ui.get_acp_executing(), 2);
            assert_eq!(app.ui.get_acp_waiting(), 3);
            app.ui.set_acp_port("invalid draft".into());
            app.ui.invoke_acp_config_edited();
            apply_acp_status(&app.ui, &app.state, &status);
            assert_eq!(app.ui.get_acp_port().as_str(), "invalid draft");
        })
        .unwrap();
    }

    // 覆盖 AH-09：保存完成后的迟到旧快照不得回退已保存值，单字段编辑也不得混入旧配置。
    #[test]
    fn acp_late_snapshot_after_save_keeps_saved_config_and_unedited_fields() {
        with_gui(|app| {
            let old = ServiceConfig {
                executable: "old-agent.exe".into(),
                arguments: vec!["--old".into()],
                port: 8765,
            };
            let saved = ServiceConfig {
                executable: "new-agent.exe".into(),
                arguments: vec!["--new".into(), "value with spaces".into()],
                port: 8766,
            };
            let old_snapshot = ServiceStatus {
                phase: crate::acp_api::ServicePhase::Ready,
                saved_config: Some(old.clone()),
                running_config: Some(old.clone()),
                service_pid: Some(123),
                ..ServiceStatus::default()
            };
            let saved_snapshot = ServiceStatus {
                saved_config: Some(saved.clone()),
                ..old_snapshot.clone()
            };
            let sender = app.state.borrow().acp_sender.clone();
            let generation = app.state.borrow().acp_observation_generation.clone();
            let old_generation = generation.load(Ordering::Acquire);
            sender
                .send(AcpMessage::Snapshot(
                    old_generation,
                    Ok(old_snapshot.clone()),
                ))
                .unwrap();
            apply_acp_messages(&app.ui, &app.state);
            app.ui.set_acp_executable(saved.executable.clone().into());
            app.ui
                .set_acp_arguments(acp_settings::format_arguments(&saved.arguments).into());
            app.ui.set_acp_port(saved.port.to_string().into());
            app.ui.invoke_acp_config_edited();
            app.ui.set_acp_request_pending(true);
            let request = AcpRequest {
                id: 1,
                operation: AcpOperation::Save,
            };
            app.state.borrow_mut().acp_pending_request = Some(request);
            sender
                .send(AcpMessage::Completed(request, Ok(saved_snapshot.clone())))
                .unwrap();
            apply_acp_messages(&app.ui, &app.state);
            assert!(!app.ui.get_acp_request_pending());
            assert_eq!(app.ui.get_acp_executable().as_str(), saved.executable);
            assert_eq!(app.ui.get_acp_port().as_str(), "8766");

            // 旧读取已在保存前取到 A，但直到保存 B 的完成消息落地之后才送达。
            sender
                .send(AcpMessage::Snapshot(old_generation, Ok(old_snapshot)))
                .unwrap();
            apply_acp_messages(&app.ui, &app.state);
            let after_late_snapshot = (
                app.ui.get_acp_executable().to_string(),
                app.ui.get_acp_arguments().to_string(),
                app.ui.get_acp_port().to_string(),
                app.ui.get_acp_saved_config().to_string(),
                app.ui.get_acp_pending_apply(),
            );

            // 用户此后只改端口；正确的 B 快照不得把其余两个输入永久锁在 A。
            app.ui.set_acp_port("8767".into());
            app.ui.invoke_acp_config_edited();
            sender
                .send(AcpMessage::Snapshot(
                    generation.load(Ordering::Acquire),
                    Ok(saved_snapshot),
                ))
                .unwrap();
            apply_acp_messages(&app.ui, &app.state);
            assert_eq!(app.ui.get_acp_port().as_str(), "8767");
            assert_eq!(
                app.ui.get_acp_executable().as_str(),
                saved.executable,
                "AH-09：只编辑端口时启动程序仍应是刚保存的 B"
            );
            assert_eq!(
                app.ui.get_acp_arguments().as_str(),
                acp_settings::format_arguments(&saved.arguments),
                "AH-09：只编辑端口时启动参数仍应是刚保存的 B"
            );
            assert_eq!(after_late_snapshot.0, saved.executable);
            assert_eq!(
                after_late_snapshot.1,
                acp_settings::format_arguments(&saved.arguments)
            );
            assert_eq!(after_late_snapshot.2, "8766");
            assert!(
                after_late_snapshot.3.contains("new-agent.exe")
                    && after_late_snapshot.3.contains("8766"),
                "AH-09：迟到 A 不能将已保存配置展示回退到 A"
            );
            assert!(
                after_late_snapshot.4,
                "AH-09：运行 A、已保存 B 的待应用入口不能被旧快照移除"
            );
            assert!(app.ui.get_acp_running_config().contains(&old.executable));
            assert!(app.ui.get_acp_saved_config().contains(&saved.executable));
            assert!(app.state.borrow().acp_observer.is_none());
        })
        .unwrap();
    }

    // 覆盖 AH-08/AH-09：自动连接错误不能被无错误的 stopped 快照擦除，也不能消费保存门禁。
    #[test]
    fn acp_connection_error_survives_stopped_snapshot_while_save_is_pending() {
        with_gui(|app| {
            let sender = app.state.borrow().acp_sender.clone();
            let generation = app.state.borrow().acp_observation_generation.clone();
            let error = "Agent 启动失败：找不到 configured-agent.exe";
            app.ui.set_acp_request_pending(true);
            app.ui.set_acp_operation("正在校验并保存配置…".into());
            sender
                .send(AcpMessage::Completed(
                    AcpRequest::CONNECT,
                    Err(error.into()),
                ))
                .unwrap();
            apply_acp_messages(&app.ui, &app.state);
            assert!(format!(
                "{}{}",
                app.ui.get_acp_action_error().as_str(),
                app.ui.get_acp_service_error().as_str()
            )
            .contains(error));
            assert!(app.ui.get_acp_request_pending());
            sender
                .send(AcpMessage::Snapshot(
                    generation.load(Ordering::Acquire),
                    Ok(ServiceStatus {
                        phase: crate::acp_api::ServicePhase::Stopped,
                        error: None,
                        ..ServiceStatus::default()
                    }),
                ))
                .unwrap();
            apply_acp_messages(&app.ui, &app.state);
            assert_eq!(app.ui.get_acp_phase().as_str(), "stopped");

            // 走真实回调验证保存门禁，不能因连接完成或状态轮询又派发一次保存。
            assert!(
                app.ui.get_acp_request_pending(),
                "AH-09：连接完成和状态快照不能解除正在保存的门禁"
            );
            app.ui.invoke_acp_save_config();
            assert!(app.ui.get_acp_request_pending());
            assert_eq!(app.ui.get_acp_operation().as_str(), "正在校验并保存配置…");
            assert!(app.state.borrow().acp_observer.is_none());
            assert!(!app.ui.get_acp_ready());
            assert!(
                format!(
                    "{}{}",
                    app.ui.get_acp_action_error().as_str(),
                    app.ui.get_acp_service_error().as_str()
                )
                .contains(error),
                "AH-08：连接失败详情须保留，不能被 stopped/error=None 轮询抹去"
            );
            // 真正就绪后才清空连接错误，观察回包仍不能消费正在保存的门禁。
            sender
                .send(AcpMessage::Snapshot(
                    generation.load(Ordering::Acquire),
                    Ok(ServiceStatus {
                        phase: crate::acp_api::ServicePhase::Ready,
                        service_pid: Some(123),
                        ..ServiceStatus::default()
                    }),
                ))
                .unwrap();
            apply_acp_messages(&app.ui, &app.state);
            assert!(app.ui.get_acp_ready());
            assert!(app.ui.get_acp_service_error().is_empty());
            assert!(app.ui.get_acp_request_pending());
            assert_eq!(app.ui.get_acp_operation().as_str(), "正在校验并保存配置…");
        })
        .unwrap();
    }

    // 覆盖 AH-08/AH-09/AH-10：已保存有效配置的失败后台应提供恢复入口；主动退出保持禁用。
    #[test]
    fn acp_failed_background_without_running_config_offers_apply_but_respects_explicit_stop() {
        with_gui(|app| {
            let sender = app.state.borrow().acp_sender.clone();
            let generation = app.state.borrow().acp_observation_generation.clone();
            let failed = ServiceStatus {
                phase: crate::acp_api::ServicePhase::Error,
                saved_config: Some(ServiceConfig {
                    executable: "configured-agent.exe".into(),
                    arguments: vec!["--mode".into(), "acp".into()],
                    port: 8765,
                }),
                running_config: None,
                service_pid: Some(123),
                error: Some("ACP 初始化失败".into()),
                ..ServiceStatus::default()
            };
            sender
                .send(AcpMessage::Snapshot(
                    generation.load(Ordering::Acquire),
                    Ok(failed.clone()),
                ))
                .unwrap();
            apply_acp_messages(&app.ui, &app.state);
            let recovery_visible = app.ui.get_acp_pending_apply();
            assert!(app.ui.get_acp_status_known());
            assert!(!app.ui.get_acp_request_pending());
            assert!(!app.ui.get_acp_explicit_stopped());
            assert_eq!(app.ui.get_confirm_kind(), 0);
            assert_eq!(app.ui.get_acp_phase().as_str(), "error");
            assert!(!app.ui.get_acp_ready());
            assert_eq!(app.ui.get_acp_running_config().as_str(), "无");
            assert_eq!(app.ui.get_acp_service_error().as_str(), "ACP 初始化失败");
            assert!(app
                .ui
                .get_acp_saved_config()
                .contains("configured-agent.exe"));

            // 隔离地恢复「本 GUI 已主动退出」会话，不触发真实 Stop 或启动后台。
            app.state.borrow_mut().acp_explicit_stopped = true;
            app.ui.set_acp_explicit_stopped(true);
            let request = AcpRequest {
                id: 1,
                operation: AcpOperation::Stop,
            };
            app.state.borrow_mut().acp_pending_request = Some(request);
            sender
                .send(AcpMessage::Completed(
                    request,
                    Ok(ServiceStatus {
                        phase: crate::acp_api::ServicePhase::Stopped,
                        service_pid: None,
                        error: None,
                        ..failed.clone()
                    }),
                ))
                .unwrap();
            apply_acp_messages(&app.ui, &app.state);
            // 即使后来又看到失败后台，快照不得撤销用户退出意图并自动恢复。
            sender
                .send(AcpMessage::Snapshot(
                    generation.load(Ordering::Acquire),
                    Ok(failed),
                ))
                .unwrap();
            apply_acp_messages(&app.ui, &app.state);
            assert!(app.state.borrow().acp_explicit_stopped);
            assert!(app.ui.get_acp_explicit_stopped());
            assert!(!app.ui.get_acp_pending_apply());
            app.ui.invoke_acp_apply_and_restart();
            assert!(app.state.borrow().acp_explicit_stopped);
            assert!(app.ui.get_acp_explicit_stopped());
            assert!(!app.ui.get_acp_request_pending());
            assert_eq!(app.ui.get_acp_operation().as_str(), "退出已完成");
            assert!(app.state.borrow().acp_observer.is_none());
            assert!(
                recovery_visible,
                "AH-09：后台存在但 running_config=None 时，有效已保存配置必须提供应用恢复入口"
            );
        })
        .unwrap();
    }

    // 覆盖 AH-10：退出使用独立确认类型，未确认不能触发 stop 或改变会话标记。
    #[test]
    fn acp_stop_requires_its_own_confirmation_without_overriding_existing_modal() {
        with_gui(|app| {
            app.ui.set_acp_status_known(true);
            app.ui.set_acp_service_pid("123".into());
            app.ui.set_confirm_kind(3);
            app.ui.invoke_acp_request_stop();
            assert_eq!(app.ui.get_confirm_kind(), 3);
            app.ui.set_confirm_kind(0);
            app.ui.invoke_acp_request_stop();
            assert_eq!(app.ui.get_confirm_kind(), 5);
            assert!(app.state.borrow().acp_observer.is_none());
            assert!(!app.state.borrow().acp_explicit_stopped);
            app.ui.set_confirm_kind(0);
            app.ui.invoke_confirmed(5);
            assert!(!app.state.borrow().acp_explicit_stopped);
            assert!(app.state.borrow().acp_observer.is_none());
        })
        .unwrap();
    }

    // AH-09/AH-10：抢占后的 UI 门禁属于具体 Stop 请求，旧操作和重复结果都不得消费。
    #[test]
    fn acp_superseded_completions_preserve_stop_pending_and_result() {
        with_gui(|app| {
            let sender = app.state.borrow().acp_sender.clone();
            let apply_request = AcpRequest {
                id: 1,
                operation: AcpOperation::Apply,
            };
            let stop_request = AcpRequest {
                id: 2,
                operation: AcpOperation::Stop,
            };
            let obsolete_stop = AcpRequest {
                id: 0,
                operation: AcpOperation::Stop,
            };
            let ready = ServiceStatus {
                phase: crate::acp_api::ServicePhase::Ready,
                service_pid: Some(123),
                ..ServiceStatus::default()
            };
            for stop_result in [
                Ok(ServiceStatus {
                    phase: crate::acp_api::ServicePhase::Stopped,
                    ..ServiceStatus::default()
                }),
                Err("退出安全收尾失败".to_string()),
            ] {
                apply_acp_status(&app.ui, &app.state, &ready);
                {
                    let mut state = app.state.borrow_mut();
                    state.acp_explicit_stopped = true;
                    state.acp_pending_request = Some(stop_request);
                }
                app.ui.set_acp_explicit_stopped(true);
                app.ui.set_acp_request_pending(true);
                app.ui.set_acp_apply_inflight(false);
                app.ui.set_acp_operation("正在退出…".into());
                app.ui.set_acp_action_error("".into());
                sender
                    .send(AcpMessage::Completed(apply_request, Ok(ready.clone())))
                    .unwrap();
                sender
                    .send(AcpMessage::Completed(
                        obsolete_stop,
                        Err("旧退出失败".into()),
                    ))
                    .unwrap();
                sender
                    .send(AcpMessage::Completed(
                        AcpRequest::CONNECT,
                        Err("迟到连接失败".into()),
                    ))
                    .unwrap();
                apply_acp_messages(&app.ui, &app.state);
                assert!(app.ui.get_acp_request_pending());
                assert_eq!(app.state.borrow().acp_pending_request, Some(stop_request));
                assert_eq!(app.ui.get_acp_operation().as_str(), "正在退出…");
                assert!(app.ui.get_acp_action_error().is_empty());

                let succeeded = stop_result.is_ok();
                sender
                    .send(AcpMessage::Completed(stop_request, stop_result))
                    .unwrap();
                apply_acp_messages(&app.ui, &app.state);
                assert!(!app.ui.get_acp_request_pending());
                assert!(app.state.borrow().acp_pending_request.is_none());
                assert_eq!(
                    app.ui.get_acp_operation().as_str(),
                    if succeeded {
                        "退出已完成"
                    } else {
                        "退出未成功"
                    },
                );
                if succeeded {
                    assert_eq!(app.ui.get_acp_phase().as_str(), "stopped");
                    assert!(app.ui.get_acp_service_pid().is_empty());
                } else {
                    assert_eq!(app.ui.get_acp_action_error().as_str(), "退出安全收尾失败");
                    assert_eq!(app.ui.get_acp_service_error().as_str(), "退出安全收尾失败");
                    assert!(!app.ui.get_acp_status_known());
                }
                let terminal = (
                    app.ui.get_acp_operation(),
                    app.ui.get_acp_action_error(),
                    app.ui.get_acp_service_error(),
                    app.ui.get_acp_phase(),
                    app.ui.get_acp_status_known(),
                );
                sender
                    .send(AcpMessage::Completed(apply_request, Ok(ready.clone())))
                    .unwrap();
                sender
                    .send(AcpMessage::Completed(stop_request, Ok(ready.clone())))
                    .unwrap();
                apply_acp_messages(&app.ui, &app.state);
                assert_eq!(
                    terminal,
                    (
                        app.ui.get_acp_operation(),
                        app.ui.get_acp_action_error(),
                        app.ui.get_acp_service_error(),
                        app.ui.get_acp_phase(),
                        app.ui.get_acp_status_known(),
                    ),
                );
                app.ui.invoke_acp_request_stop();
                app.ui.invoke_acp_apply_and_restart();
                assert_eq!(app.ui.get_confirm_kind(), 0);
                assert!(app.ui.get_acp_explicit_stopped());
                assert!(!app.ui.get_acp_request_pending());
                assert!(app.state.borrow().acp_observer.is_none());
            }
        })
        .unwrap();
    }

    // AH-09/AH-10：真实 GUI 回调和真实 observer/IPC，控制端保持 Apply 未完成，
    // 只有另一条 Stop 连接才能取消排空。不能用测试 sender 绕过串行控制线程。
    #[cfg(windows)]
    #[test]
    fn acp_gui_confirmed_stop_preempts_pending_apply_over_independent_control_path() {
        use acp_control_test::ControlStub;

        with_gui(|app| {
            let mut control = ControlStub::new(app);
            apply_acp_status(&app.ui, &app.state, &control.status());
            assert!(app.ui.get_acp_pending_apply());
            app.ui.invoke_acp_apply_and_restart();
            assert!(ControlStub::wait_for(app, || control.count("Apply") == 1));
            assert!(ControlStub::wait_for(app, || app.ui.get_acp_phase() == "draining"));
            assert!(app.ui.get_acp_request_pending());
            assert_eq!(control.status().executing, 1);

            // 无关确认不能被退出入口覆盖，也不能借 confirmed(5) 派发 Stop。
            app.ui.set_confirm_kind(3);
            app.ui.invoke_acp_request_stop();
            let existing_modal_preserved = app.ui.get_confirm_kind() == 3;
            app.ui.invoke_confirmed(5);
            app.ui.set_confirm_kind(0);
            app.ui.invoke_confirmed(5);
            let no_unconfirmed_stop =
                control.count("Stop") == 0 && !app.ui.get_acp_explicit_stopped();

            app.ui.invoke_acp_request_stop();
            let first_confirmation_opened = app.ui.get_confirm_kind() == 5;
            let no_stop_before_confirmation = control.count("Stop") == 0;
            // 取消后再确认旧 kind 不得停止；同一窗口应仍能重新请求退出。
            app.ui.set_confirm_kind(0);
            app.ui.invoke_confirmed(5);
            let cancelled_confirmation_respected =
                control.count("Stop") == 0 && !app.ui.get_acp_explicit_stopped();
            app.ui.invoke_acp_request_stop();
            let second_confirmation_opened = app.ui.get_confirm_kind() == 5;
            app.ui.invoke_confirmed(5);
            app.ui.invoke_confirmed(5);

            // Apply 尚未收尾时必须已经被后台消费 Stop，而不是排到 Apply 后面。
            let stop_consumed_before_apply_release =
                ControlStub::wait_for(app, || control.count("Stop") == 1);
            let resources_released_by_stop = control.status().phase
                == crate::acp_api::ServicePhase::Stopped
                && control.status().executing == 0
                && control.status().running_config.is_none();
            let explicit_stop_recorded = app.ui.get_acp_explicit_stopped();
            let completion_delivered = if stop_consumed_before_apply_release {
                ControlStub::wait_for(app, || {
                    control.apply_finished() && !app.ui.get_acp_request_pending()
                })
            } else {
                false
            };
            app.ui.set_confirm_kind(0);
            app.ui.invoke_acp_apply_and_restart();
            let restarted = ControlStub::wait_for(app, || control.count("Apply") != 1);
            let no_restart_after_explicit_stop = !restarted && app.ui.get_acp_explicit_stopped();

            // 先完整走过取消、重新确认和完成回调，再执行能失败的断言。
            // 即使门禁导致 Stop 未发送，也释放 fixture 的 Apply 并有界收回 IPC worker。
            let cleanup_finished = control.finish(app);
            assert!(cleanup_finished, "隔离控制端和在途 Apply 必须有界收尾");
            assert!(existing_modal_preserved);
            assert!(no_unconfirmed_stop && no_stop_before_confirmation);
            assert!(cancelled_confirmation_respected);
            assert!(
                first_confirmation_opened && second_confirmation_opened,
                "AH-10：Apply 在途 Draining 时，真实退出回调必须仍打开独立退出确认"
            );
            assert!(
                stop_consumed_before_apply_release,
                "AH-10：确认退出必须经独立控制连接送达 Stop，不能等待 Apply 自然排空"
            );
            assert!(resources_released_by_stop && completion_delivered);
            assert!(explicit_stop_recorded && no_restart_after_explicit_stop);
        })
        .unwrap();
    }

    // 保存门禁不能被「允许 Stop 抢占 Apply」的修复一并放开。
    #[cfg(windows)]
    #[test]
    fn acp_gui_stop_respects_pending_save_then_callbacks_remain_usable() {
        use acp_control_test::ControlStub;

        with_gui(|app| {
            let mut control = ControlStub::new(app);
            apply_acp_status(&app.ui, &app.state, &control.status());
            control.hold_status_queries();
            app.ui.invoke_acp_save_config();
            assert!(ControlStub::wait_for(app, || {
                control.count("Status") > 0 && acp_settings::load_config().unwrap().is_some()
            }));
            assert!(app.ui.get_acp_request_pending());
            app.ui.invoke_acp_request_stop();
            let no_save_confirmation = app.ui.get_confirm_kind() == 0;
            app.ui.invoke_confirmed(5);
            let save_did_not_stop = control.count("Stop") == 0
                && !app.ui.get_acp_explicit_stopped()
                && app.ui.get_acp_request_pending();

            control.release_status_queries();
            let save_completed = ControlStub::wait_for(app, || !app.ui.get_acp_request_pending());
            app.ui.invoke_acp_request_stop();
            let later_confirmation_opened = app.ui.get_confirm_kind() == 5;
            app.ui.invoke_confirmed(5);
            let later_stop_consumed = ControlStub::wait_for(app, || control.count("Stop") == 1);
            let stop_completed = ControlStub::wait_for(app, || !app.ui.get_acp_request_pending());
            app.ui.set_confirm_kind(0);
            app.ui.invoke_acp_apply_and_restart();
            let restarted = ControlStub::wait_for(app, || control.count("Apply") != 0);
            let stayed_stopped = app.ui.get_acp_explicit_stopped()
                && !restarted
                && control.status().phase == crate::acp_api::ServicePhase::Stopped;
            let cleanup_finished = control.finish(app);
            assert!(cleanup_finished);
            assert!(
                no_save_confirmation && save_did_not_stop,
                "AH-09：保存中不得弹出退出确认或派发 Stop"
            );
            assert!(save_completed && later_confirmation_opened);
            assert!(later_stop_consumed && stop_completed && stayed_stopped);
        })
        .unwrap();
    }

    #[cfg(windows)]
    mod acp_control_test {
        use super::*;
        use sha2::{Digest, Sha256};
        use std::ffi::OsString;
        use std::fmt::Write;
        use std::sync::atomic::AtomicBool;
        use std::sync::MutexGuard;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
        use tokio::sync::Mutex;
        use tokio_util::sync::CancellationToken;

        // 不改变产品控制协议；测试监听同用户/会话、独立 state root 派生的私有管道。
        fn pipe_name() -> String {
            use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
            use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
            use windows_sys::Win32::Security::{
                GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
            };
            use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
            #[link(name = "kernel32")]
            extern "system" {
                fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
            }
            let mut token = std::ptr::null_mut();
            // SAFETY: 当前进程伪句柄无需输入或释放，在当前进程中有效。
            let process = unsafe { GetCurrentProcess() };
            // SAFETY: 当前进程伪句柄有效，token 为可写输出指针。
            let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) };
            assert_ne!(opened, 0);
            let mut size = 0;
            // SAFETY: 零长查询只取得 TOKEN_USER 所需空间。
            unsafe {
                GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &raw mut size);
            }
            let word = std::mem::size_of::<usize>();
            let mut storage = vec![0_usize; usize::try_from(size).unwrap().div_ceil(word)];
            // SAFETY: storage 按 usize 对齐且至少 size 字节，token 为成功打开的句柄。
            let received = unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    storage.as_mut_ptr().cast(),
                    size,
                    &raw mut size,
                )
            };
            // SAFETY: 关闭本函数唯一拥有的真实 token 句柄。
            unsafe { CloseHandle(token) };
            assert_ne!(received, 0);
            // SAFETY: 成功的 TOKEN_USER 查询返回正确对齐、仍存活的完整结构。
            let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
            let mut sid = std::ptr::null_mut();
            // SAFETY: SID 属于仍存活的 storage；转换函数写入其分配的 UTF-16 字符串。
            let converted = unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut sid) };
            assert_ne!(converted, 0);
            let mut length = 0;
            loop {
                // SAFETY: 转换成功后 sid 指向以零结尾的字符串，length 未越过终止符。
                let character = unsafe { sid.add(length) };
                // SAFETY: character 位于尚未释放的转换结果中，包含可读 UTF-16 码元。
                if unsafe { *character } == 0 {
                    break;
                }
                length += 1;
            }
            // SAFETY: 前述扫描确定了有效字符串长度，分配尚未释放。
            let user = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid, length) });
            // SAFETY: 转换函数的输出使用 LocalFree 释放一次。
            unsafe { LocalFree(sid.cast()) };
            let mut session = 0;
            // SAFETY: 当前 PID 有效，session 为可写输出指针。
            let queried = unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) };
            assert_ne!(queried, 0);
            let root = crate::xberg_settings::state_dir().unwrap();
            let digest = Sha256::digest(root.as_os_str().to_string_lossy().as_bytes());
            let mut suffix = String::with_capacity(digest.len() * 2);
            for byte in digest {
                write!(&mut suffix, "{byte:02x}").unwrap();
            }
            format!(r"\\.\pipe\jchtools-acp-http-{user}-{session}-{suffix}")
        }

        struct TestStateRoot {
            previous: Option<OsString>,
            _root: tempfile::TempDir,
            _lock: MutexGuard<'static, ()>,
        }
        impl TestStateRoot {
            fn new() -> Self {
                let lock = crate::asset_util::test_env::env_lock()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let root = tempfile::tempdir().unwrap();
                let previous = std::env::var_os("JCHTOOLS_TEST_STATE_DIR");
                std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.path().join("state"));
                Self {
                    previous,
                    _root: root,
                    _lock: lock,
                }
            }
        }
        impl Drop for TestStateRoot {
            fn drop(&mut self) {
                match self.previous.take() {
                    Some(root) => std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root),
                    None => std::env::remove_var("JCHTOOLS_TEST_STATE_DIR"),
                }
            }
        }

        #[derive(Clone)]
        struct ControlServerState {
            state: Arc<Mutex<ServiceStatus>>,
            operations: Arc<Mutex<Vec<String>>>,
            hold_queries: Arc<AtomicBool>,
            release_queries: CancellationToken,
            stopped: CancellationToken,
            shutdown: CancellationToken,
            apply_finished: Arc<AtomicBool>,
        }

        pub(super) struct ControlStub {
            server: ControlServerState,
            done: mpsc::Receiver<()>,
            worker: Option<std::thread::JoinHandle<()>>,
            ui_state: Rc<RefCell<State>>,
            _environment: TestStateRoot,
        }

        impl ControlStub {
            pub(super) fn new(app: &GuiTestApp) -> Self {
                let environment = TestStateRoot::new();
                let running = ServiceConfig {
                    executable: std::env::current_exe()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    arguments: Vec::new(),
                    port: 8765,
                };
                let state = Arc::new(Mutex::new(ServiceStatus {
                    phase: crate::acp_api::ServicePhase::Ready,
                    saved_config: Some(ServiceConfig {
                        port: 8766,
                        ..running.clone()
                    }),
                    running_config: Some(running),
                    service_pid: Some(std::process::id()),
                    executing: 1,
                    ..ServiceStatus::default()
                }));
                let operations = Arc::new(Mutex::new(Vec::new()));
                let hold_queries = Arc::new(AtomicBool::new(false));
                let release_queries = CancellationToken::new();
                let stopped = CancellationToken::new();
                let shutdown = CancellationToken::new();
                let apply_finished = Arc::new(AtomicBool::new(false));
                let (ready_tx, ready_rx) = mpsc::channel();
                let (done_tx, done) = mpsc::channel();
                let name = pipe_name();
                let server = ControlServerState {
                    state,
                    operations,
                    hold_queries,
                    release_queries,
                    stopped,
                    shutdown,
                    apply_finished,
                };
                let worker = {
                    let server = server.clone();
                    std::thread::spawn(move || {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap();
                        runtime.block_on(async move {
                            let mut listener = ServerOptions::new()
                                .first_pipe_instance(true)
                                .reject_remote_clients(true)
                                .create(&name)
                                .unwrap();
                            ready_tx.send(()).unwrap();
                            let mut workers = tokio::task::JoinSet::new();
                            loop {
                                tokio::select! {
                                    () = server.shutdown.cancelled() => break,
                                    connected = listener.connect() => connected.unwrap(),
                                }
                                let pipe = listener;
                                listener = ServerOptions::new()
                                    .reject_remote_clients(true)
                                    .create(&name)
                                    .unwrap();
                                let server = server.clone();
                                workers.spawn(async move { consume(pipe, &server).await });
                                while workers.try_join_next().is_some() {}
                            }
                            while workers.join_next().await.is_some() {}
                        });
                        let _ = done_tx.send(());
                    })
                };
                let fixture = Self {
                    server,
                    done,
                    worker: Some(worker),
                    ui_state: app.state.clone(),
                    _environment: environment,
                };
                ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                fixture
            }

            pub(super) fn status(&self) -> ServiceStatus {
                self.server.state.blocking_lock().clone()
            }
            pub(super) fn count(&self, operation: &str) -> usize {
                self.server
                    .operations
                    .blocking_lock()
                    .iter()
                    .filter(|seen| *seen == operation)
                    .count()
            }
            pub(super) fn apply_finished(&self) -> bool {
                self.server.apply_finished.load(Ordering::Acquire)
            }
            pub(super) fn hold_status_queries(&self) {
                self.server.hold_queries.store(true, Ordering::Release);
            }
            pub(super) fn release_status_queries(&self) {
                self.server.release_queries.cancel();
            }
            pub(super) fn wait_for(app: &GuiTestApp, done: impl Fn() -> bool) -> bool {
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    apply_acp_messages(&app.ui, &app.state);
                    if done() {
                        return true;
                    }
                    if Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            pub(super) fn finish(&mut self, app: &GuiTestApp) -> bool {
                // 不经产品 Stop 做兜底，否则会掩盖真实 GUI 没有派发 Stop 的回归。
                app.state.borrow_mut().acp_observer.take();
                self.server.stopped.cancel();
                self.server.release_queries.cancel();
                let queued_stop_consumed = self.wait_for_queued_stop();
                let completed = Self::wait_for(app, || !app.ui.get_acp_request_pending());
                self.server.shutdown.cancel();
                let joined = self.join_worker();
                queued_stop_consumed && completed && joined
            }
            fn wait_for_queued_stop(&self) -> bool {
                if !self.ui_state.borrow().acp_explicit_stopped {
                    return true;
                }
                // 门禁修复而控制线程尚未修复时，Stop 可能仍排在 Apply 后。
                // 兜底取消释放 Apply 后，先让这条已有请求进入隔离端点，再恢复环境。
                let deadline = Instant::now() + Duration::from_secs(3);
                while self.count("Stop") == 0 {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                true
            }
            fn join_worker(&mut self) -> bool {
                if self.worker.is_none() {
                    return true;
                }
                if self.done.recv_timeout(Duration::from_secs(3)).is_err()
                    && !self
                        .worker
                        .as_ref()
                        .is_some_and(std::thread::JoinHandle::is_finished)
                {
                    return false;
                }
                self.worker
                    .take()
                    .is_some_and(|worker| worker.join().is_ok())
            }
        }

        impl Drop for ControlStub {
            fn drop(&mut self) {
                self.ui_state.borrow_mut().acp_observer.take();
                self.server.stopped.cancel();
                self.server.release_queries.cancel();
                let _ = self.wait_for_queued_stop();
                self.server.shutdown.cancel();
                let _ = self.join_worker();
            }
        }

        async fn consume(
            mut pipe: NamedPipeServer,
            server: &ControlServerState,
        ) -> std::io::Result<()> {
            let ControlServerState {
                state,
                operations,
                hold_queries,
                release_queries,
                stopped,
                shutdown,
                apply_finished,
            } = server;
            let operation = tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                operation = async {
                    let length = pipe.read_u32_le().await?;
                    if usize::try_from(length).unwrap() > acp_settings::CONTROL_FRAME_LIMIT {
                        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "超长控制帧"));
                    }
                    let mut bytes = vec![0; usize::try_from(length).unwrap()];
                    pipe.read_exact(&mut bytes).await?;
                    serde_json::from_slice::<String>(&bytes)
                        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
                } => operation?,
            };
            let result: std::result::Result<ServiceStatus, crate::acp_api::ServiceError> =
                match operation.as_str() {
                    "Apply" => {
                        state.lock().await.phase = crate::acp_api::ServicePhase::Draining;
                        operations.lock().await.push(operation.clone());
                        // 真实 IPC 回复保持悬挂：串行 observer 无法在这里偷跑下一条 Stop。
                        tokio::select! {
                            () = stopped.cancelled() => {},
                            () = shutdown.cancelled() => {},
                        }
                        apply_finished.store(true, Ordering::Release);
                        Err(crate::acp_api::ServiceError::new(
                            crate::acp_api::ServiceErrorKind::Stopping,
                            "隔离控制端：Stop 已取消在途 Apply",
                        ))
                    }
                    "Stop" => {
                        let status = {
                            let mut status = state.lock().await;
                            status.phase = crate::acp_api::ServicePhase::Stopped;
                            status.running_config = None;
                            status.service_pid = None;
                            status.agent_pid = None;
                            status.executing = 0;
                            status.waiting = 0;
                            status.clone()
                        };
                        stopped.cancel();
                        operations.lock().await.push(operation.clone());
                        Ok(status)
                    }
                    "Status" => {
                        operations.lock().await.push(operation.clone());
                        if hold_queries.load(Ordering::Acquire) {
                            tokio::select! {
                                () = release_queries.cancelled() => {},
                                () = shutdown.cancelled() => {},
                            }
                        }
                        Ok(state.lock().await.clone())
                    }
                    _ => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            operation,
                        ))
                    }
                };
            let response = serde_json::to_vec(&serde_json::json!({
                "protocol": 1,
                "pid": std::process::id(),
                "result": result,
            }))?;
            pipe.write_u32_le(u32::try_from(response.len()).unwrap())
                .await?;
            pipe.write_all(&response).await?;
            pipe.flush().await
        }
    }

    // 覆盖 M-08（小数大小换算后为正整数字节即合法：0.5 MB = 524288 字节；
    // 0 与换算后非整数字节的输入仍必须拒绝）
    #[test]
    fn split_size_accepts_integral_fractions_and_rejects_zero() {
        assert_eq!(
            parse_size_bytes("0.5", 1),
            Ok(512 * 1024),
            "0.5 MB = 524288 字节，合法输入不得拒绝"
        );
        assert_eq!(parse_size_bytes("0.5", 0), Ok(512), "0.5 KB = 512 字节");
        assert_eq!(parse_size_bytes("1.5", 0), Ok(1536), "1.5 KB = 1536 字节");
        assert!(parse_size_bytes("0", 0).is_err(), "0 字节必须拒绝");
        assert!(
            parse_size_bytes("0.1", 0).is_err(),
            "0.1 KB = 102.4 字节换算后非整数，必须拒绝"
        );
        assert!(
            parse_size_bytes("0.0", 1).is_err(),
            "0.0 MB 换算后为 0，必须拒绝"
        );
    }

    // 覆盖 M-11（同名分片确认覆盖后必须实际写出：确认不得再次弹框或空转）
    #[test]
    fn md_split_existing_output_requires_confirmation_then_overwrites() {
        let dir = temp_test_dir("md-split-confirm");
        let docs = dir.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        let content = "0123456789".repeat(150); // 1500 字节 → 1 KB 限制下 2 片
        std::fs::write(docs.join("doc.md"), &content).unwrap();
        let parts = dir.join("parts");
        std::fs::create_dir_all(&parts).unwrap();
        std::fs::write(parts.join("doc_001.md"), "旧内容").unwrap();
        let parts_text = parts.display().to_string();
        with_gui(move |app| {
            app.ui.invoke_select_tool("md-organizer".into());
            app.ui.set_md_subpage(1);
            app.ui
                .set_md_split_file(docs.join("doc.md").display().to_string().into());
            app.ui.set_md_split_size("1".into());
            app.ui.set_md_split_unit(0);
            app.ui.set_md_split_dir(parts_text.clone().into());
            app.ui.invoke_md_split_start();
            assert!(
                pump_until(app, || app.ui.get_confirm_kind() == 4),
                "同名分片已存在必须先弹确认框（M-11）"
            );
            assert_eq!(
                std::fs::read_to_string(parts.join("doc_001.md")).unwrap(),
                "旧内容",
                "确认前不得写入"
            );
            app.ui.set_acknowledge(true);
            app.ui.set_confirm_kind(0);
            confirm_md_override(&app.ui, &app.state, &app.pump.out);
            assert!(
                pump_until(app, || {
                    !app.ui.get_busy() && app.ui.get_status().contains("拆分完成")
                }),
                "确认覆盖后必须完成写出而不是再次弹确认（M-11）：status={}",
                app.ui.get_status()
            );
            assert_eq!(
                std::fs::read_to_string(parts.join("doc_001.md")).unwrap(),
                &content[..1024],
                "确认后旧分片被覆盖为新内容前 1024 字节"
            );
            assert_eq!(
                std::fs::read_to_string(parts.join("doc_002.md")).unwrap(),
                &content[1024..],
                "第二片写出剩余内容"
            );
            let _ = std::fs::remove_dir_all(&dir);
        })
        .unwrap();
    }

    // 覆盖 H-02/U-06（回归：MD/Git/转换任务收尾必须复位 paused——否则残留的
    // paused 会丢弃后续任务的普通状态事件，并把暂停按钮显示成「继续」）
    #[test]
    fn md_and_git_finish_reset_paused() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.set_busy(true);
            ui.set_paused(true);
            app.pump
                .out
                .send(Event::MdDone("合并完成：0 个文件".into()))
                .unwrap();
            app.pump.run(ui);
            assert!(!ui.get_busy(), "收尾清 busy");
            assert!(!ui.get_paused(), "MD 收尾必须复位 paused（修复前残留）");
            ui.set_busy(true);
            ui.set_paused(true);
            app.pump
                .out
                .send(Event::GitDone("Git 任务完成".into()))
                .unwrap();
            app.pump.run(ui);
            assert!(!ui.get_busy());
            assert!(!ui.get_paused(), "Git 收尾同样复位 paused");
        })
        .unwrap();
    }

    // 覆盖 P-02/U-11（新工具导航注册与切工具回默认子页）
    #[test]
    fn md_and_git_tools_navigate_and_reset_subpage() {
        with_gui(|app| {
            app.ui.invoke_select_tool("md-organizer".into());
            assert_eq!(app.ui.get_screen(), 3, "MD 整理应进入独立页面（M-01）");
            assert_eq!(app.ui.get_active_tool_id(), "md-organizer");
            app.ui.set_md_subpage(1);
            app.ui.invoke_select_tool("git-tools".into());
            assert_eq!(app.ui.get_screen(), 4, "Git 工具应进入独立页面（G-01）");
            app.ui.invoke_select_tool("markdown-converter".into());
            assert_eq!(app.ui.get_screen(), 5, "转 Markdown 应进入独立页面（T-01）");
            assert_eq!(app.ui.get_active_tool_id(), "markdown-converter");
            // U-11：切回 MD 整理必须回到默认子页「合并 MD」，不得停留在拆分子页
            app.ui.invoke_select_tool("md-organizer".into());
            assert_eq!(app.ui.get_md_subpage(), 0, "切工具回默认子页（U-11）");
            // 现有工具不受影响（H-03：不删除、不隐藏、不改名）
            app.ui.invoke_select_tool("recursive-extract".into());
            assert_eq!(app.ui.get_screen(), 2);
            app.ui.invoke_select_tool("directory-organizer".into());
            assert_eq!(app.ui.get_screen(), 0);
        })
        .unwrap();
    }

    // 覆盖 T-01/T-05/T-07：公开导航进入转 Markdown 页面时，默认选项完整且未初始化不得启动；
    // 切回旧工具后状态栏不得残留新工具文案。
    #[test]
    fn markdown_converter_navigation_is_isolated_before_initialization() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("markdown-converter".into());
            assert_eq!(ui.get_screen(), 5, "转 Markdown 必须进入独立页面");
            assert_eq!(ui.get_active_tool_id(), "markdown-converter");
            assert!(ui.get_convert_pdf(), "默认应启用 PDF");
            assert!(ui.get_convert_office(), "默认应启用 Office");
            assert!(ui.get_convert_images(), "默认应启用图片");
            assert!(ui.get_convert_media(), "默认应启用 MP4/M4A");
            assert!(ui.get_convert_other(), "默认应启用其他格式");
            assert!(!ui.get_convert_flat(), "默认应保留输入目录层级");
            assert_eq!(ui.get_convert_timeout_secs().as_str(), "21600");
            assert!(!ui.get_convert_ready(), "未初始化时组件不得标记为就绪");
            assert!(!ui.get_busy(), "未初始化时不得存在转换任务");

            // 通过公开回调尝试启动：未就绪必须被 GUI 门禁拦截。
            let before = ui.get_status().to_string();
            ui.invoke_convert_start();
            assert!(!ui.get_busy(), "未初始化时开始转换必须保持禁用");
            assert_eq!(ui.get_status().as_str(), before);

            // T-29：即使测试直接注入 ready，超出系统可表示范围的正整数也必须
            // 明确拒绝且不得进入 busy；不得恢复旧的静默钳制行为。
            let timeout_root = temp_test_dir("convert-navigation-timeout");
            let timeout_input = timeout_root.join("input");
            let timeout_output = timeout_root.join("output");
            std::fs::create_dir_all(&timeout_input).unwrap();
            std::fs::create_dir_all(&timeout_output).unwrap();
            ui.set_convert_ready(true);
            ui.set_convert_input_dir(timeout_input.display().to_string().into());
            ui.set_convert_output_dir(timeout_output.display().to_string().into());
            ui.set_convert_timeout_secs(u64::MAX.to_string().into());
            ui.invoke_convert_start();
            assert!(!ui.get_busy(), "不可表示的超时必须禁止启动转换");
            assert!(ui.get_error_text().contains("超出系统可表示范围"));
            let _ = std::fs::remove_dir_all(timeout_root);

            // 覆盖 T-23/T-24：部分失败和主动停止必须呈现不同的最终状态。
            ui.set_busy(true);
            // 覆盖 T-24：单文件失败的文件名和原因在批次收尾后仍可从 GUI 日志查看。
            app.pump
                .out
                .send(Event::Status(
                    "CONVERTER_FILE_FINISHED|0|0|broken.pdf|解析失败".into(),
                ))
                .unwrap();
            app.pump.run(ui);
            app.pump
                .out
                .send(Event::MdDone("CONVERTER_DONE|0|0|1|0|0|0".into()))
                .unwrap();
            app.pump.run(ui);
            assert_eq!(ui.get_convert_status().as_str(), "转换部分失败");
            assert!(ui.get_convert_log_text().contains("broken.pdf"));
            assert!(ui.get_convert_log_text().contains("解析失败"));
            ui.set_busy(true);
            app.pump
                .out
                .send(Event::MdDone("CONVERTER_DONE|0|1|0|0|0|0".into()))
                .unwrap();
            app.pump.run(ui);
            assert_eq!(ui.get_convert_status().as_str(), "转换部分失败");
            ui.set_busy(true);
            app.pump
                .out
                .send(Event::MdDone("CONVERTER_DONE|0|0|0|0|0|1".into()))
                .unwrap();
            app.pump.run(ui);
            assert_eq!(ui.get_convert_status().as_str(), "已停止");

            // 覆盖 T-05/T-06：无效运行目录的长清单须保留在日志，错误横幅不能撑坏页面。
            let generation = app.state.borrow().convert_readiness_generation;
            ui.set_convert_runtime_saving(true);
            let long_error = format!("xberg.exe 缺失；{}", "models/缺失项；".repeat(80));
            app.pump
                .out
                .send(Event::Error(format!(
                    "CONVERTER_RUNTIME_ERR|{generation}|{long_error}"
                )))
                .unwrap();
            app.pump.run(ui);
            assert!(ui.get_error_text().chars().count() <= 220);
            assert!(ui.get_error_text().contains("xberg.exe"));
            assert!(!ui.get_error_text().contains("models/"));
            assert!(ui.get_convert_log_text().contains(&long_error));

            ui.invoke_select_tool("directory-organizer".into());
            assert_eq!(ui.get_screen(), 0, "切回目录整理必须进入旧页面");
            assert_eq!(ui.get_status().as_str(), "请选择需要整理的目录");
            assert!(
                !ui.get_status().contains("转 Markdown"),
                "旧工具状态不得残留转 Markdown 文案"
            );
        })
        .unwrap();
    }

    // 覆盖 T-23/T-24/XB-08：停止请求不能掩盖取消异常，原请求未结束的诊断应保留。
    #[test]
    fn converter_stop_with_failure_preserves_failed_final_state_and_diagnostic() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("markdown-converter".into());
            ui.set_busy(true);
            let diagnostic = "取消后原转换请求仍未结束，无法安全继续共享会话";
            app.pump
                .out
                .send(Event::Status(format!(
                    "CONVERTER_FILE_FINISHED|0|0|blocked.pdf|{diagnostic}"
                )))
                .unwrap();
            app.pump
                .out
                .send(Event::MdDone("CONVERTER_DONE|0|0|1|0|0|1".into()))
                .unwrap();
            app.pump.run(ui);
            assert_eq!(ui.get_convert_status().as_str(), "已停止，转换失败");
            assert!(ui.get_status().contains("已停止"));
            assert!(ui.get_status().contains("失败"));
            assert!(ui.get_convert_progress_note().contains("失败"));
            assert!(ui.get_convert_metrics().contains("失败 1"));
            assert!(ui.get_convert_log_text().contains("blocked.pdf"));
            assert!(ui.get_convert_log_text().contains(diagnostic));
            assert!(!ui.get_busy());

            ui.set_busy(true);
            app.pump
                .out
                .send(Event::MdDone("CONVERTER_DONE|0|0|0|0|0|1".into()))
                .unwrap();
            app.pump.run(ui);
            assert_eq!(ui.get_convert_status().as_str(), "已停止");
            assert!(!ui.get_status().contains("失败"), "正常取消不报告转换失败");

            ui.set_busy(true);
            app.pump
                .out
                .send(Event::MdDone("CONVERTER_DONE|0|0|1|0|0|0".into()))
                .unwrap();
            app.pump.run(ui);
            assert_eq!(ui.get_convert_status().as_str(), "转换部分失败");
        })
        .unwrap();
    }

    // 覆盖 T-05/T-06/U：组件缺失的完整清单进入独立折叠详情，页面状态与任务日志
    // 保持简短，不把十几项路径直接铺在页头、横幅或执行日志里。
    #[test]
    fn converter_readiness_long_detail_stays_in_log() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("markdown-converter".into());
            let generation = app.state.borrow().convert_readiness_generation;
            let detail = "文档转换组件未就绪：Xberg 运行目录缺失必需文件（13 项）：LICENSE；models/det.onnx；models/rec.onnx";
            app.pump
                .out
                .send(Event::Status(format!(
                    "CONVERTER_READINESS|{generation}|0|{detail}"
                )))
                .unwrap();
            app.pump.run(ui);
            assert_eq!(ui.get_convert_status().as_str(), "可选组件未就绪，请主动初始化");
            assert!(ui.get_notice_text().is_empty());
            assert!(
                ui.get_error_text().is_empty(),
                "组件就绪结果不应产生全局错误：{}",
                ui.get_error_text()
            );
            assert_eq!(ui.get_convert_detail_text().as_str(), detail);
            assert!(!ui.get_convert_detail_open());
            assert!(!ui.get_convert_log_text().contains(detail));
        })
        .unwrap();
    }

    // 覆盖 T-05/T-06/U：上一用例迟到的后台事件不得串入新用例的界面。
    // S1-04（复审修正）：测试 harness 每个用例新建独立窗口与事件通道，receiver
    // 随上一用例的装配一起 drop——这一结构性事实就是隔离本身。原版与第一版
    // 重写都只在断言端「排空后无红字」，而旧 sender 的 send 必然 Err 且被吞，
    // 断言对「共享通道」回归不敏感（恒真）。现直接锁住结构性事实：新用例中
    // 上一用例的 sender 必须 send Err（断连）；若有人把 harness 改成共享通道，
    // 该断言即红。阳性对照证明「错误事件送达本用例通道必然上屏」，保证断言
    // 机制本身有效。
    #[test]
    fn gui_test_cases_do_not_share_late_background_errors() {
        // 阳性对照：错误事件送达「本用例」通道时必然上屏为红字。
        with_gui(|app| {
            app.pump
                .out
                .send(Event::Error("上一用例的迟到错误".into()))
                .unwrap();
            app.pump.run(&app.ui);
            assert_eq!(
                app.ui.get_error_text().as_str(),
                "上一用例的迟到错误",
                "阳性对照失败：送达本用例通道的错误必须上屏"
            );
        })
        .unwrap();
        // 取出上一用例的 sender（经 with_gui 返回值带出），带进下一个用例
        // 验证它已随旧装配断连。
        let stale = with_gui(|app| app.pump.out.clone()).unwrap();
        with_gui(move |app| {
            let outcome = stale.send(Event::Error("上一用例的迟到错误".into()));
            assert!(
                outcome.is_err(),
                "上一用例的 sender 仍然连通：用例间事件通道未隔离，迟到事件可污染新用例"
            );
            // 防御性保持：即便未来出现其它迟到路径，新用例界面也不得有红字。
            let ui = &app.ui;
            let deadline = Instant::now() + Duration::from_millis(200);
            while Instant::now() < deadline {
                app.pump.run(ui);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                ui.get_error_text().is_empty(),
                "旧事件污染新界面：{}",
                ui.get_error_text()
            );
        })
        .unwrap();
    }

    // 覆盖 U-09（S1-01）：任务运行中迟到的设置收尾事件不得消费 close_after 提前
    // 退出——关闭必须留给任务终态（CONVERTER_DONE/FAIL 等）统一完成。
    #[test]
    fn settings_result_while_task_running_keeps_close_after() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.set_busy(true);
            app.state.borrow_mut().close_after = true;
            app.pump
                .out
                .send(Event::Notice(
                    "SETTINGS_READY|{\"custom\":null,\"downloaded\":null,\"downloaded_active\":false,\"active\":\"\"}"
                        .into(),
                ))
                .unwrap();
            app.pump.run(ui);
            assert!(
                app.state.borrow().close_after,
                "任务运行中 SETTINGS_READY 不得消费 close_after（U-09）"
            );
            app.pump
                .out
                .send(Event::Error("SETTINGS_ERROR|校验失败".into()))
                .unwrap();
            app.pump.run(ui);
            assert!(
                app.state.borrow().close_after,
                "任务运行中 SETTINGS_ERROR 不得消费 close_after（U-09）"
            );
        })
        .unwrap();
    }

    // U-09：重叠的 GUI 任务与组件操作都完成前不得消费关闭请求。
    #[test]
    fn close_after_waits_for_concurrent_task_and_converter_initialization() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.set_busy(true);
            ui.set_convert_initializing(true);
            app.state.borrow_mut().close_after = true;

            app.pump
                .out
                .send(Event::MdDone("其他任务完成".into()))
                .unwrap();
            app.pump.run(ui);
            assert!(!ui.get_busy());
            assert!(ui.get_convert_initializing());
            assert!(
                app.state.borrow().close_after,
                "任务收尾不得越过仍在进行的组件初始化关闭窗口（U-09）"
            );

            ui.set_busy(true);
            app.pump
                .out
                .send(Event::Notice("CONVERTER_INIT_OK".into()))
                .unwrap();
            app.pump.run(ui);
            assert!(!ui.get_convert_initializing());
            assert!(
                app.state.borrow().close_after,
                "初始化收尾不得越过仍在运行的任务关闭窗口（U-09）"
            );

            app.pump
                .out
                .send(Event::MdDone("剩余任务完成".into()))
                .unwrap();
            app.pump.run(ui);
            assert!(
                !app.state.borrow().close_after,
                "全部 GUI 管理的操作收尾后应消费关闭请求"
            );
            // 其它无 busy 标志的 GUI 阶段也必须挡住关闭；只有全部空闲时才消费请求。
            app.state.borrow_mut().close_after = true;
            ui.set_convert_preparing(true);
            assert!(!app.pump.close_after_if_idle(ui), "转换预检在途时不得关闭");
            assert!(app.state.borrow().close_after);
            ui.set_convert_preparing(false);

            ui.set_convert_runtime_saving(true);
            assert!(!app.pump.close_after_if_idle(ui), "配置保存在途时不得关闭");
            assert!(app.state.borrow().close_after);
            ui.set_convert_runtime_saving(false);

            ui.set_snap_initializing(true);
            assert!(
                !app.pump.close_after_if_idle(ui),
                "截图字体初始化在途时不得关闭"
            );
            assert!(app.state.borrow().close_after);
            ui.set_snap_initializing(false);
            assert!(
                app.pump.close_after_if_idle(ui),
                "所有操作空闲后应消费关闭请求"
            );
        })
        .unwrap();
    }

    // 转换启动预检与旧工具任务可并行；预检终态即使在 busy 时到达也必须清除
    // pending 状态，不能让转换页面永久停在“正在检查”。
    #[test]
    fn converter_preflight_result_does_not_strand_when_other_task_runs() {
        with_gui(|app| {
            let ui = &app.ui;
            let generation = 7;
            {
                let mut state = app.state.borrow_mut();
                state.convert_preparing = true;
                state.convert_preflight_generation = generation;
                state.convert_pending_options = Some(markdown::Options {
                    input_dir: PathBuf::new(),
                    output_dir: PathBuf::new(),
                    flat: false,
                    groups: Vec::new(),
                    timeout_secs: 1,
                });
                state.convert_cancel = Some(Arc::new(std::sync::atomic::AtomicBool::new(false)));
            }
            ui.set_convert_preparing(true);
            ui.set_busy(true);

            app.pump
                .out
                .send(Event::Status(format!(
                    "CONVERTER_PREFLIGHT|{generation}|1|已通过"
                )))
                .unwrap();
            app.pump.run(ui);

            assert!(!ui.get_convert_preparing());
            assert!(!app.state.borrow().convert_preparing);
            assert!(app.state.borrow().convert_pending_options.is_none());
            assert!(app.state.borrow().convert_cancel.is_none());
            assert!(ui.get_busy(), "预检事件不得结束另一个工具的任务");
            assert!(
                ui.get_convert_status().contains("其他任务"),
                "转换页需明确提示其它任务运行中且可稍后重试"
            );
        })
        .unwrap();
    }

    // 覆盖 U-09/XB-20（S1-01）：启动线程的设置快照（SETTINGS_LOADED）不是设置
    // 操作的终态——到达时不得抹掉进行中的初始化状态，也不得消费 close_after。
    #[test]
    fn startup_settings_snapshot_keeps_running_initialization_state() {
        with_gui(|app| {
            let ui = &app.ui;
            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            ui.set_convert_initializing(true);
            app.state.borrow_mut().convert_init_cancel = Some(cancel);
            app.state.borrow_mut().close_after = true;
            app.pump
                .out
                .send(Event::Notice(
                    "SETTINGS_LOADED|{\"custom\":null,\"downloaded\":null,\"downloaded_active\":false,\"active\":\"\"}"
                        .into(),
                ))
                .unwrap();
            app.pump.run(ui);
            assert!(
                ui.get_convert_initializing(),
                "启动快照不得抹掉进行中的初始化状态"
            );
            assert!(
                app.state.borrow().convert_init_cancel.is_some(),
                "启动快照不得清掉初始化取消句柄"
            );
            assert!(
                app.state.borrow().close_after,
                "启动快照不得消费 close_after（U-09）"
            );
        })
        .unwrap();
    }

    // 覆盖 U-10/S-07（S1-03）：CONVERTER_LOG 只置脏、由轮询尾部按面板可见性统一
    // 重建——页面不在转换页时不得逐条整段重排上屏；回到转换页后的第一次刷新
    // 按环形缓冲补齐最新日志。
    #[test]
    fn converter_log_rebuild_is_throttled_to_visible_page() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("markdown-converter".into());
            app.pump
                .out
                .send(Event::Status("CONVERTER_LOG|screen5-line".into()))
                .unwrap();
            app.pump.run(ui);
            assert!(
                ui.get_convert_log_text().contains("screen5-line"),
                "转换页可见时日志必须在轮询尾部上屏"
            );
            ui.invoke_select_tool("directory-organizer".into());
            assert_eq!(ui.get_screen(), 0);
            app.pump
                .out
                .send(Event::Status("CONVERTER_LOG|away-line".into()))
                .unwrap();
            app.pump.run(ui);
            assert!(
                !ui.get_convert_log_text().contains("away-line"),
                "面板不可见时 CONVERTER_LOG 不得逐条整段重建上屏（S1-03）"
            );
            ui.invoke_select_tool("markdown-converter".into());
            app.pump.run(ui);
            assert!(
                ui.get_convert_log_text().contains("away-line"),
                "回到转换页后的刷新必须按环形缓冲补齐最新日志"
            );
        })
        .unwrap();
    }

    // 覆盖 T-05/T-06（S1-05）：busy 期间到达的当前代就绪结果被丢弃后，任务终态
    // （DONE/FAIL 收尾）必须补查一次就绪，页面不得长期停留在过期的就绪状态。
    #[test]
    fn converter_done_rechecks_readiness_dropped_while_busy() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("markdown-converter".into());
            ui.set_convert_runtime_dir("C:\\xberg".into());
            ui.set_convert_runtime_confirmed(true);
            let generation = app.state.borrow().convert_readiness_generation;
            ui.set_busy(true);
            app.pump
                .out
                .send(Event::Status(format!(
                    "CONVERTER_READINESS|{generation}|0|组件缺失"
                )))
                .unwrap();
            app.pump.run(ui);
            assert!(!ui.get_convert_ready(), "busy 期间就绪结果不得直接上屏");
            app.pump
                .out
                .send(Event::MdDone("CONVERTER_DONE|1|0|0|0|0|0".into()))
                .unwrap();
            app.pump.run(ui);
            assert!(
                app.state.borrow().convert_readiness_generation > generation,
                "任务终态必须对 busy 期间被丢弃的就绪结果发起补查（S1-05/T-05）"
            );
            assert_eq!(
                ui.get_convert_status().as_str(),
                "正在检查已安装组件…",
                "补查发起后状态应进入检查中"
            );
        })
        .unwrap();
    }

    // 覆盖 XB-19/T-05（S3-01）：初始化入口集中在设置页——转换页不得再保留
    // 「初始化可选组件」按钮（该入口只按文档成员校验，纯媒体用户点击会误报
    // 失败），页面呈现就绪状态并提供「前往设置」。
    #[test]
    fn convert_page_has_no_initialize_entry() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/app.slint"),
        )
        .unwrap();
        assert!(
            !source.contains("convert-initialize"),
            "转换页不得保留初始化回调入口（S3-01/XB-19）"
        );
        assert!(
            !source.contains("初始化可选组件"),
            "转换页不得保留「初始化可选组件」按钮文案（S3-01/XB-19）"
        );
        let strip = source
            .split("if root.screen == 5")
            .nth(1)
            .unwrap_or_default()
            .split("if root.screen == 7")
            .next()
            .unwrap_or_default();
        assert!(
            strip.contains("前往设置"),
            "转换页必须保留「前往设置」入口（XB-19）"
        );
    }

    // 覆盖 T-05/T-06/XB-19（S3-01）：入口移除后，纯媒体场景的就绪状态仍按勾选
    // 场景计算——只勾 MP4/M4A 时走正常就绪检查，不要求文档组件初始化。
    #[test]
    fn media_only_selection_computes_readiness_without_page_initialize() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("markdown-converter".into());
            ui.set_convert_runtime_dir("C:\\xberg".into());
            ui.set_convert_runtime_confirmed(true);
            ui.set_convert_pdf(false);
            ui.set_convert_office(false);
            ui.set_convert_images(false);
            ui.set_convert_media(true);
            ui.set_convert_other(false);
            let generation = app.state.borrow().convert_readiness_generation;
            ui.invoke_convert_selection_changed();
            assert!(
                app.state.borrow().convert_readiness_generation > generation,
                "纯媒体勾选必须触发按场景就绪检查（XB-19）"
            );
            assert_eq!(
                ui.get_convert_status().as_str(),
                "正在检查已安装组件…",
                "就绪检查应正常发起，不得要求文档组件初始化"
            );
        })
        .unwrap();
    }

    // 覆盖 U-06（S3-04）：用户主动取消初始化不得显示为红色组件错误——取消只更新
    // 中性状态行，红色错误文本与「组件未就绪」详情都必须保持空。
    #[test]
    fn snap_cancelled_initialize_is_not_shown_as_error() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.set_snap_error("".into());
            ui.set_snap_asset_detail("".into());
            let generation = app.state.borrow().snap_generation;
            app.state
                .borrow()
                .snap_sender
                .send(SnapMessage::Initialized(
                    generation,
                    Err("用户取消初始化".into()),
                ))
                .expect("发送取消消息");
            app.pump.apply_snap_messages(ui);
            assert!(
                ui.get_snap_error().is_empty(),
                "用户取消不得写入红色错误文本（U-06）"
            );
            assert_eq!(
                ui.get_snap_asset_detail().as_str(),
                "",
                "用户取消不得汇入「后台组件未就绪」红色详情（U-06）"
            );
            assert!(
                ui.get_snap_asset_status().contains("取消"),
                "取消应以中性状态行提示：{}",
                ui.get_snap_asset_status()
            );
            assert!(!ui.get_snap_ready());
        })
        .unwrap();
    }

    // 覆盖 M-02/M-04/M-07（MD 合并的真实回调端到端：扫描→合并→完成文案与输出内容）
    #[test]
    fn md_merge_flow_runs_end_to_end_via_real_callbacks() {
        let dir = temp_test_dir("md-merge-flow");
        let docs = dir.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join("a.md"),
            "# 甲

内容A",
        )
        .unwrap();
        std::fs::write(
            docs.join("b.md"),
            "# 乙
内容B
",
        )
        .unwrap();
        let output = docs.join("merged.md");
        let docs_text = docs.display().to_string();
        with_gui(move |app| {
            app.ui.invoke_select_tool("md-organizer".into());
            app.ui.set_md_input_dir(docs_text.clone().into());
            app.ui.set_md_recursive(true);
            app.ui.set_md_output_name("merged.md".into());
            app.ui.set_md_output_dir(docs_text.clone().into());
            app.ui.invoke_md_merge_start();
            assert!(
                pump_until(app, || !app.ui.get_busy()
                    && app.ui.get_status().contains("合并完成")),
                "合并应经真实回调完成：status={}",
                app.ui.get_status()
            );
            let merged = std::fs::read_to_string(&output).unwrap();
            assert!(
                merged.contains(
                    "# a.md

## 甲"
                ),
                "文件名标题+内部标题下移：{merged}"
            );
            assert!(
                merged.contains(
                    "## 乙
内容B"
                ),
                "{merged}"
            );
            assert!(
                merged.contains(
                    "内容A

# b.md"
                ),
                "文件间必须空行分隔：{merged:?}"
            );
            assert!(
                std::fs::read_to_string(docs.join("a.md"))
                    .unwrap()
                    .starts_with("# 甲"),
                "原文件不得被改动（M-02）"
            );
            let _ = std::fs::remove_dir_all(&dir);
        })
        .unwrap();
    }

    // 覆盖 M-07（输出已存在：先确认、不静默覆盖；确认后完成覆盖）
    #[test]
    fn md_merge_existing_output_requires_confirmation() {
        let dir = temp_test_dir("md-merge-confirm");
        let docs = dir.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join("a.md"),
            "# 甲
",
        )
        .unwrap();
        std::fs::write(docs.join("merged.md"), "旧输出").unwrap();
        let docs_text = docs.display().to_string();
        with_gui(move |app| {
            app.ui.invoke_select_tool("md-organizer".into());
            app.ui.set_md_input_dir(docs_text.clone().into());
            app.ui.set_md_output_name("merged.md".into());
            app.ui.set_md_output_dir(docs_text.clone().into());
            app.ui.invoke_md_merge_start();
            // 冲突 → 确认框（kind=4），旧输出未被破坏
            assert!(
                pump_until(app, || app.ui.get_confirm_kind() == 4),
                "输出已存在必须弹确认框（M-07）"
            );
            assert_eq!(
                std::fs::read_to_string(dir.join("docs").join("merged.md")).unwrap(),
                "旧输出",
                "确认前不得破坏已有文件"
            );
            app.ui.set_acknowledge(true);
            app.ui.set_confirm_kind(0);
            confirm_md_override(&app.ui, &app.state, &app.pump.out);
            assert!(
                pump_until(app, || !app.ui.get_busy()
                    && app.ui.get_status().contains("合并完成")),
                "确认覆盖后应完成：status={}",
                app.ui.get_status()
            );
            let merged = std::fs::read_to_string(dir.join("docs").join("merged.md")).unwrap();
            assert!(merged.contains("# a.md"), "确认后输出被覆盖写入：{merged}");
            let _ = std::fs::remove_dir_all(&dir);
        })
        .unwrap();
    }

    // 覆盖 C-11（页面结果只按当前页面代际与视图筛选应用；过期页面不落地）
    #[test]
    fn plan_page_event_rejects_stale_generation_and_filter() {
        // 低代际事件晚到：不得因 gen!=latest 而应用（列表不能被上一筛选/上一页的结果覆盖）。
        assert!(
            !plan_page_event_accepted(5, 6, None, None),
            "低代际事件必须拒绝"
        );
        // 高代际且 filter 与视图一致：可应用（翻页为「先加载、成功再提交」，不校验预置页码）。
        assert!(plan_page_event_accepted(
            6,
            6,
            Some("delete"),
            Some("delete")
        ));
        assert!(plan_page_event_accepted(6, 6, None, None));
        // 同代但用户已切筛选：拒绝。
        assert!(!plan_page_event_accepted(6, 6, None, Some("delete")));
    }
    // 覆盖 S-07, U-10
    #[test]
    fn failed_event_log_respects_300_cap() {
        // 回归（U-10）：Failed 收尾此前直接 push_back 不查上限；一次任务先积累 300 条
        // 日志再收到失败事件时界面日志会到 301 条。失败收尾必须与普通日志同受 300 条约束。
        let mut logs = VecDeque::new();
        for i in 0..300 {
            push_event_log(&mut logs, format!("日志 {i}"));
        }
        push_event_log(&mut logs, "任务失败：测试".into());
        assert_eq!(logs.len(), 300, "界面日志最多保留最近 300 条（U-10）");
        assert_eq!(
            logs.back().map(std::string::String::as_str),
            Some("任务失败：测试"),
            "最新一条在最上"
        );
        assert_eq!(
            logs.front().map(std::string::String::as_str),
            Some("日志 1"),
            "最旧一条被挤出"
        );
    }
    // 覆盖 U-06（失败收尾如实重载摘要，不得崩溃或停留旧数据）
    #[test]
    fn failed_reload_updates_summary_without_reborrow_panic() {
        // 回归：Failed 收尾此前把 task 提取放在 if-let scrutinee 里（edition 2021 下
        // borrow() 临时存活到整个语句结束），块内 borrow_mut 更新 planned 必 BorrowMutError
        // panic——用户在任务执行中点「取消」即崩掉整个应用，且无任何测试覆盖该分支。
        with_gui(|app| {
            let dir = temp_test_dir("failed-reload");
            let db = Database::create(&dir).unwrap();
            for i in 0..3 {
                let action = crate::model::Action {
                    id: 0,
                    kind: crate::model::ActionKind::Delete,
                    source: format!("f{i}.txt"),
                    target: None,
                    reason: "测试".into(),
                    expected: None,
                    keeper: None,
                    hash: None,
                    mode: crate::config::DeleteMode::Permanent,
                    selected: true,
                    state: "pending".into(),
                };
                db.add_action(&action).unwrap();
            }
            let summary = crate::model::Summary {
                scanned: 5,
                scanned_bytes: 0,
                archives_ok: 0,
                archives_failed: 0,
                archives_quarantined: 0,
                extracted: 0,
                planned_delete: 3,
                planned_move: 0,
                planned_git: 0,
                planned_empty: 0,
                candidate_bytes: 0,
                deleted: 0,
                moved: 0,
                skipped: 0,
                errors: 0,
                permanent_bytes: 0,
            };
            db.set("summary", &summary).unwrap();
            drop(db);
            {
                let mut s = app.state.borrow_mut();
                s.task = Some(dir.clone());
            }
            // 失败收尾的摘要与计划页改由既有计划 worker 在同一次开库取回（界面线程不再开库，
            // H-02）：这里用真实通道 + 真实事件泵排空应用，路径与事件循环一致。
            reload_after_failed(&app.ui, &app.state, &app.pump.out);
            assert!(
                pump_until(app, || app.ui.get_plan_delete_count() == 3),
                "Failed 收尾必须从任务库重载摘要计数：summary={}",
                app.ui.get_summary()
            );
            let s = app.state.borrow();
            assert_eq!(s.planned, 3, "Failed 收尾应从任务库重载分母计数");
        })
        .unwrap();
    }
    struct GuiTestApp {
        ui: AppWindow,
        state: Rc<RefCell<State>>,
        pump: UiPump,
    }
    impl GuiTestApp {
        fn reset(&self) {
            // 上一用例可能残留未排空的事件：先丢弃，避免串到本用例的断言里。
            while self.pump.receiver.try_iter().next().is_some() {}
            *self.state.borrow_mut() = initial_state().unwrap();
            self.ui.set_ready(false);
            self.ui.set_error_text("".into());
            self.ui.set_notice_text("".into());
            self.ui.set_theme(0);
            self.ui.set_acp_executable("".into());
            self.ui.set_acp_arguments("".into());
            self.ui.set_acp_port("8765".into());
            self.ui.set_acp_saved_config("正在读取…".into());
            self.ui.set_acp_running_config("正在读取…".into());
            self.ui.set_acp_phase("unconfigured".into());
            self.ui
                .set_acp_status("正在后台读取配置并连接模型服务…".into());
            self.ui.set_acp_status_known(false);
            self.ui.set_acp_ready(false);
            self.ui.set_acp_pending_apply(false);
            self.ui.set_acp_request_pending(false);
            self.ui.set_acp_explicit_stopped(false);
            self.ui.set_acp_service_pid("".into());
            self.ui.set_acp_agent_pid("".into());
            self.ui.set_acp_executing(0);
            self.ui.set_acp_waiting(0);
            self.ui.set_acp_operation("".into());
            self.ui.set_acp_action_error("".into());
            self.ui.set_acp_service_error("".into());
            self.ui.set_confirm_kind(0);
            self.ui.set_confirm_pending(false);
            self.ui.set_section(0);
            self.ui.set_panel(0);
            self.ui.set_show_advanced(false);
            self.ui.set_screen(0);
            self.ui.set_active_tool_id("directory-organizer".into());
            self.ui.set_quarantined_count(0);
            self.ui.set_acknowledge(false);
            self.ui.set_directory("".into());
            self.ui.set_has_task(false);
            self.ui.set_busy(false);
            // 转换页的编辑属性不属于 State；每个 GUI 用例必须显式恢复，
            // 否则一个超大 timeout 输入会污染后续用例。
            self.ui.set_convert_input_dir("".into());
            self.ui.set_convert_output_dir("".into());
            self.ui.set_convert_flat(false);
            self.ui.set_convert_pdf(true);
            self.ui.set_convert_office(true);
            self.ui.set_convert_images(true);
            self.ui.set_convert_media(true);
            self.ui.set_convert_other(true);
            self.ui.set_convert_timeout_secs("21600".into());
            self.ui.set_convert_ready(false);
            self.ui.set_convert_runtime_confirmed(false);
            self.ui.set_convert_initializing(false);
            self.ui.set_convert_preparing(false);
            self.ui.set_convert_runtime_saving(false);
            self.ui.set_convert_runtime_dir("".into());
            self.ui.set_convert_status("尚未检查可选组件".into());
            self.ui.set_convert_detail_text("".into());
            self.ui.set_convert_detail_open(false);
            self.ui.set_convert_metrics("尚未开始".into());
            self.ui.set_convert_progress(-1.0);
            self.ui.set_convert_progress_note("".into());
            self.ui.set_snap_asset_status("尚未检查可选组件".into());
            self.ui.set_snap_asset_detail("".into());
            self.ui.set_snap_detail_open(false);
            self.ui.set_progress(-1.0);
            self.ui.set_progress_note("".into());
            self.ui.set_plan_filter(0);
            self.ui.set_plan_prev_enabled(false);
            self.ui.set_plan_next_enabled(false);
            self.ui.set_summary("".into());
            self.ui.set_metrics("".into());
            self.ui.set_status("".into());
            self.ui.set_confirm_text("".into());
            self.ui
                .set_plans(Rc::new(VecModel::from(Vec::<PlanRow>::new())).into());
            self.ui.set_md_subpage(0);
            self.ui.set_fail_ready(false);
            self.ui
                .set_fail_rows(Rc::new(VecModel::from(Vec::<FailRow>::new())).into());
            self.ui.set_fail_total(0);
            self.ui.set_fail_prev_enabled(false);
            self.ui.set_fail_next_enabled(false);
            self.ui.set_fail_page_label("第 1 页".into());
            self.ui.set_git_branch("".into());
            self.ui.set_git_upstream("".into());
            self.ui.set_git_current("".into());
            self.ui.set_git_stage("".into());
            self.ui.set_git_retry(0);
            self.ui.set_git_retry_wait(0);
            self.ui.set_git_total(0);
            self.ui.set_git_done(0);
            self.ui.set_git_state("".into());
            reset_tool_list(&self.ui);
            refresh(&self.ui, &self.state.borrow());
        }
    }
    type Job = Box<dyn FnOnce(&GuiTestApp) + Send>;
    fn gui() -> &'static Mutex<mpsc::Sender<Job>> {
        static JOBS: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
        JOBS.get_or_init(|| {
            // C'-1：装配先隔离截图服务端点——本装配的 State 常驻进程，监督线程
            // 可能在任意用例中被触发（如切换 snap-ocr 页的真实导航回调），隔离
            // 变量一经设置不恢复。设置在共享进程环境锁内，与 snap_ocr_assets /
            // markdown_assets 改写同一变量的用例互不覆盖；覆盖生效后管道名与
            // 资产根都脱离生产位置，ensure 链路只能对测试专用名失败返回。
            {
                let _lock = crate::asset_util::test_env::env_lock()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                std::env::set_var(
                    "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT",
                    snap_test_isolated_root(),
                );
            }
            let (tx, rx) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("gui-test-worker".into())
                .spawn(move || {
                    i_slint_backend_testing::init_no_event_loop();
                    // 每个用例拥有独立窗口、状态和事件通道。仅排空共享通道无法
                    // 隔离上一用例尚未结束的后台线程，它们可能稍后继续发送事件。
                    while let Ok(job) = rx.recv() {
                        let ui = AppWindow::new().unwrap();
                        let state = Rc::new(RefCell::new(initial_state().unwrap()));
                        ui.set_tool_count(
                            i32::try_from(registry::tools().len()).unwrap_or(i32::MAX),
                        );
                        let tools = registry::tools()
                            .iter()
                            .map(|tool| ToolRow {
                                id: tool.id.into(),
                                name: tool.name.into(),
                                summary: tool.summary.into(),
                            })
                            .collect::<Vec<_>>();
                        ui.set_tools(Rc::new(VecModel::from(tools)).into());
                        // 真实事件泵：后台 worker（计划加载、就绪快照、勾选保存）把结果发到通道，
                        // 用例用 `pump_until` 排空并应用，走的是与事件循环完全相同的代码路径。
                        let (event_tx, event_rx) = mpsc::sync_channel::<Event>(256);
                        let out = EventSender::new(event_tx);
                        wire_sync(&ui, &state, &out);
                        wire_md_git(&ui, &state, &out);
                        // 截图 OCR 回调同样接入无头装配：录制热键等纯界面逻辑可经真实回调断言；
                        // 仅装配不启动任何资产/服务线程（那些由用户主动回调触发）。
                        wire_snap_ocr(&ui, &state, &out);
                        wire_markdown_converter(&ui, &state, &out);
                        wire_settings(&ui, &state, &out);
                        wire_acp_http(&ui, &state);
                        wire_task_lifecycle(&ui, &state, &out);
                        refresh(&ui, &state.borrow());
                        let pump = UiPump::new(event_rx, state.clone(), out);
                        let app = GuiTestApp { ui, state, pump };
                        app.reset();
                        let _ =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&app)));
                    }
                })
                .expect("启动 GUI 测试工作线程");
            Mutex::new(tx)
        })
    }
    /// GUI 测试的截图资产隔离根（C'-1）：仓库 `.tmp/` 下固定目录，进程内首次
    /// 访问时创建，与生产 state dir 完全分离。目录常驻进程不删除，由
    /// `python scripts/make_tmp.py clean` 统一清理（`.tmp/` 约定）。
    fn snap_test_isolated_root() -> PathBuf {
        static ROOT: OnceLock<PathBuf> = OnceLock::new();
        ROOT.get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(".tmp")
                .join("gui-test-snap-assets");
            let _ = std::fs::create_dir_all(&root);
            root
        })
        .clone()
    }

    /// 驱动后台结果落地：真实 worker 线程发送、真实事件泵应用，最多等 30 秒。
    /// Windows 上逐条启动 Git 的开销可能超过 5 秒；不以该偶然耗时替代功能断言。
    /// `done` 为真即返回；超时返回 false，由调用方断言给出可读失败原因。
    fn pump_until(app: &GuiTestApp, done: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            app.pump.run(&app.ui);
            if done() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn with_gui<R: Send + 'static>(
        job: impl FnOnce(&GuiTestApp) -> R + Send + 'static,
    ) -> Result<R, String> {
        let (result_tx, result_rx) = mpsc::sync_channel::<Result<R, String>>(1);
        gui()
            .lock()
            .unwrap()
            .send(Box::new(move |app| {
                // 捕获用例内的 panic 并把消息经结果通道带回：worker 线程必须存活以服务后续用例，
                // 同时失败原因要随测试结果透出，而不是被 RecvError 掩盖。
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(app)));
                match outcome {
                    Ok(value) => {
                        let _ = result_tx.send(Ok(value));
                    }
                    Err(payload) => {
                        let msg = if let Some(m) = payload.downcast_ref::<&'static str>() {
                            (*m).to_string()
                        } else if let Some(m) = payload.downcast_ref::<String>() {
                            m.clone()
                        } else {
                            "未知 panic".to_string()
                        };
                        let _ = result_tx.send(Err(msg));
                    }
                }
            }))
            .unwrap();
        result_rx.recv().unwrap()
    }
    fn rule_value_at(ui: &AppWindow, key: &str) -> Option<String> {
        let rules = ui.get_rules();
        (0..rules.row_count()).find_map(|i| {
            let row = rules.row_data(i).unwrap();
            (row.key.as_str() == key).then(|| row.value.to_string())
        })
    }

    // 覆盖 XB-20/XB-21/XB-24：设置页真实保存回调、持久结果和页面隔离。
    #[test]
    fn shared_settings_rejects_invalid_assets_and_keeps_saved_source() {
        with_gui(|app| {
            let _lock = crate::asset_util::test_env::env_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let root = tempfile::tempdir().unwrap();
            struct Restore(Option<std::ffi::OsString>);
            impl Drop for Restore {
                fn drop(&mut self) {
                    if let Some(value) = &self.0 {
                        std::env::set_var("JCHTOOLS_TEST_STATE_DIR", value);
                    } else {
                        std::env::remove_var("JCHTOOLS_TEST_STATE_DIR");
                    }
                }
            }
            let _restore = Restore(std::env::var_os("JCHTOOLS_TEST_STATE_DIR"));
            std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.path());
            std::fs::write(root.path().join("xberg.exe"), b"settings fixture").unwrap();
            let ui = &app.ui;
            ui.set_convert_runtime_saving(false);
            ui.set_convert_initializing(false);
            ui.invoke_navigation(7);
            assert_eq!(ui.get_screen(), 7);
            assert!(ui.get_active_tool_id().is_empty());
            crate::xberg_settings::save(root.path()).unwrap();
            app.pump
                .out
                .send(Event::Notice(format!(
                    "SETTINGS_READY|{}",
                    settings_payload().unwrap()
                )))
                .unwrap();
            app.pump.run(ui);
            ui.set_settings_custom_dir(root.path().display().to_string().into());
            ui.invoke_settings_action("custom".into());
            assert!(pump_until(app, || !ui.get_convert_runtime_saving()));
            assert!(ui.get_convert_runtime_confirmed());
            // XB-19/XB-26：保存只要求 xberg.exe 在场（含根可执行），场景资产按
            // 各功能就绪检查分别校验；成功必须给出持久保存确认（XB-18）。
            assert!(ui.get_settings_status().contains("配置已持久保存"));
            assert_eq!(
                crate::xberg_settings::required().unwrap(),
                root.path().canonicalize().unwrap()
            );
            let saved = ui.get_convert_runtime_dir();
            ui.set_settings_custom_dir(root.path().join("missing").display().to_string().into());
            ui.invoke_settings_action("custom".into());
            assert!(pump_until(app, || !ui.get_convert_runtime_saving()));
            assert_eq!(ui.get_convert_runtime_dir(), saved);
            // 保存校验不再强制无关场景资产：缺目录由 xberg.exe 在场校验直接拒绝，
            // 状态行必须说明具体拒绝原因（XB-20），已保存来源保持不变。
            assert!(ui.get_settings_status().contains("xberg.exe"));
            ui.invoke_select_tool("snap-ocr".into());
            assert_eq!(ui.get_screen(), 6);
            assert_eq!(ui.get_active_tool_id(), "snap-ocr");
            // XB-23/O-30：主动退出是正常状态，不显示红色连接故障。
            app.state
                .borrow()
                .snap_sender
                .send(SnapMessage::Service(
                    Err("后台已从托盘退出；重新打开 JchTools 后自动启动".into()),
                    false,
                ))
                .unwrap();
            app.pump.apply_snap_messages(ui);
            assert!(!ui.get_snap_connected());
            assert!(
                ui.get_snap_error().is_empty(),
                "主动退出不应显示错误：{}",
                ui.get_snap_error()
            );
            assert!(ui.get_snap_service_status().contains("已从托盘退出"));
        })
        .unwrap();
    }

    // 覆盖 C'-2：识别中点「截图识别」收到 {"ok":false,"error":"正在识别"} 后
    // requested=true 写入红字；识别结束纯轮询（requested=false）收到 ok 且
    // error 为 null 的正常状态时必须清除旧红字——修复前该分支只在「requested
    // 或 error 非空」时写，红字会一直残留到下一次请求。
    #[test]
    fn snap_error_clears_when_polling_reports_recovery() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.set_snap_error("".into());
            app.state
                .borrow()
                .snap_sender
                .send(SnapMessage::Service(
                    Ok(serde_json::json!({"ok": false, "error": "正在识别"})),
                    true,
                ))
                .expect("发送请求失败消息");
            app.pump.run(ui);
            assert_eq!(
                ui.get_snap_error().as_str(),
                "正在识别",
                "请求被服务拒绝时必须显示错误文本"
            );
            app.state
                .borrow()
                .snap_sender
                .send(SnapMessage::Service(
                    Ok(serde_json::json!({"ok": true, "model": "loaded", "task": "idle"})),
                    false,
                ))
                .expect("发送轮询消息");
            app.pump.run(ui);
            assert_eq!(
                ui.get_snap_error().as_str(),
                "",
                "轮询恢复正常（ok 且 error 为 null）后旧红字必须清除（C'-2）"
            );
        })
        .unwrap();
    }

    // 覆盖 C'-1：测试资产根覆盖必须同时隔离截图服务管道名——无头 GUI 装置的
    // 监督线程 ping 测试派生名（不存在的管道）即失败返回，绝不触碰真实用户
    // 会话的截图服务。修复前 pipe_name() 无视覆盖变量返回生产真名 → 本断言红
    // （该真名可被真实服务应答，attach-main-exe 会把测试二进制写入其
    // launcher.json，2026-09-29 本机已实际发生）。
    #[test]
    fn snap_asset_root_override_isolates_pipe_name() {
        let lock = crate::asset_util::test_env::env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT");
        let root = snap_test_isolated_root();
        std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", &root);
        let pipe = crate::snap_ocr_assets::pipe_name();
        let asset_root = crate::snap_ocr_assets::asset_root();
        match previous {
            Some(value) => std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", value),
            None => std::env::remove_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT"),
        }
        drop(lock);
        assert_eq!(asset_root, root, "测试资产根覆盖必须同时接管资产根（C'-1）");
        assert!(
            pipe.starts_with(r"\\.\pipe\jchtools-snap-ocr-test-"),
            "管道名必须派生自隔离资产根的测试专用名，不得返回生产真名（C'-1）：{pipe}"
        );
    }

    // 覆盖 C'-1（端到端）：经真实 on_select_tool → ensure_snap_supervisor 链路
    // 切换 snap-ocr 页，监督线程的 ping 只能命中测试专用管道名（无服务监听）、
    // readiness 只能对 .tmp 隔离根失败，因此不得向真实 state dir 写入任何内容
    // ——以真实 launcher.json 前后字节不变取证（该文件是既往污染的实际受害点）。
    #[test]
    fn gui_test_assembly_isolates_real_snap_service() {
        with_gui(|app| {
            let ui = &app.ui;
            let launcher = crate::config::state_dir()
                .ok()
                .map(|dir| dir.join("snap-ocr").join("launcher.json"));
            let before = launcher
                .as_deref()
                .and_then(|path| std::fs::read(path).ok());
            ui.set_snap_error("".into());
            ui.invoke_select_tool("snap-ocr".into());
            assert_eq!(ui.get_screen(), 6, "隔离不得改变页面导航语义（O-01）");
            // 隔离下 ensure 的确定结局：ping 测试专用名失败 → readiness 对空
            // 隔离根失败 → Service(Err) 经真实事件泵落地为错误文本。
            assert!(
                pump_until(app, || !ui.get_snap_error().is_empty()),
                "隔离下 ensure 必须以服务未连接错误落地（不得悬挂或触达真实服务）"
            );
            let after = launcher
                .as_deref()
                .and_then(|path| std::fs::read(path).ok());
            assert_eq!(
                before, after,
                "切换 snap-ocr 页不得读写真实 launcher.json（C'-1）"
            );
        })
        .unwrap();
    }

    // 覆盖 P-02, P-04, H-03
    #[test]
    fn initial_surface_lists_defaults() {
        with_gui(|app| {
            let ui = &app.ui;
            assert!(!ui.get_ready(), "初始状态不得就绪");
            assert_eq!(ui.get_theme(), 2, "默认深色主题（P-04）");
            // O-01：截图 OCR 与原有工具并列，导航至独立工具页。
            let tools = ui.get_tools();
            let snap_visible = (0..tools.row_count())
                .filter_map(|index| tools.row_data(index))
                .any(|tool| tool.id == "snap-ocr");
            assert!(snap_visible, "侧栏必须提供截图 OCR 入口");
            ui.invoke_select_tool("snap-ocr".into());
            assert_eq!(ui.get_screen(), 6, "截图 OCR 必须进入独立页面");
            assert_eq!(ui.get_tool_search().as_str(), "", "启动不得预置搜索过滤");
        })
        .unwrap();
    }
    // 覆盖 R-01, C-02
    #[test]
    fn section_switch_swaps_rule_rows() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_section(3); // 安全与性能（目录整理：0去重/1归类/2清理/3安全与性能）
            assert_eq!(ui.get_section(), 3);
            assert_eq!(
                ui.get_rules().row_count(),
                2,
                "安全与性能默认只显示 2 条基础规则（全局删除方式 + 递归扫描）"
            );
            assert!(
                rule_value_at(ui, "hash_workers").is_none(),
                "Hash 并行工作线程属于高级层"
            );
            ui.invoke_toggle_advanced(true);
            assert!(ui.get_show_advanced());
            assert_eq!(
                ui.get_rules().row_count(),
                6,
                "打开高级层后安全与性能应显示全部 6 条（隐藏/系统属性/排除 glob/Hash 线程；磁盘预留属于递归解压）"
            );
            assert_eq!(rule_value_at(ui, "hash_workers").as_deref(), Some("6"));
            assert!(
                rule_value_at(ui, "reserve_bytes").is_none(),
                "磁盘预留属于递归解压的工具规则"
            );
            ui.invoke_toggle_advanced(false);
            ui.invoke_select_section(0); // 去重
            assert!(
                rule_value_at(ui, "dedup_same_name").is_some(),
                "去重开关在去重分区"
            );
            ui.invoke_toggle_advanced(true);
            assert!(
                rule_value_at(ui, "same_name_same_size").is_none(),
                "C-02：版本取舍开关不得出现在规则面板"
            );
            assert!(
                rule_value_at(ui, "same_size_keep").is_none(),
                "C-02：版本取舍保留规则不得出现"
            );
            assert!(
                rule_value_at(ui, "theme").is_none(),
                "主题不再作为目录整理的规则行"
            );
            assert!(
                rule_value_at(ui, "max_depth").is_none(),
                "解压规则不得出现在目录整理的面板"
            );
        })
        .unwrap();
    }
    // 覆盖 R-01, X-05, X-08（解压侧只剩安全与性能项：处置已固定，高级层是防护上限）
    #[test]
    fn advanced_rows_hidden_until_toggled() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("recursive-extract".into());
            ui.invoke_select_section(0); // 解压（递归解压：0解压/1安全与性能）
            assert_eq!(
                ui.get_rules().row_count(),
                0,
                "解压分区没有基础规则：原包处置与冲突策略已按 X-05/X-04 固定移除"
            );
            assert!(
                rule_value_at(ui, "max_depth").is_none(),
                "最大嵌套层数属于高级层"
            );
            assert!(
                rule_value_at(ui, "nested_archives").is_none(),
                "R-02：嵌套解压开关不得出现（嵌套解压始终开启，受最大嵌套层数限制）"
            );
            assert!(
                rule_value_at(ui, "dedup_same_name").is_none(),
                "去重规则不得出现在递归解压的面板"
            );
            ui.invoke_toggle_advanced(true);
            assert!(ui.get_show_advanced());
            assert_eq!(
                ui.get_rules().row_count(),
                4,
                "打开高级层后解压分区应显示全部 4 条防护上限（层数/条目/比例/磁盘预留）"
            );
            assert_eq!(rule_value_at(ui, "max_depth").as_deref(), Some("16"));
            assert!(
                rule_value_at(ui, "archive_delete").is_none(),
                "X-05/R-02：原包处置不得再是可配置项"
            );
            ui.invoke_toggle_advanced(false);
            assert_eq!(ui.get_rules().row_count(), 0, "关闭高级层必须恢复基础视图");
        })
        .unwrap();
    }
    // 覆盖 P-02, R-01, X-01（两工具规则分区互不串扰）
    #[test]
    fn tool_routing_swaps_screens_and_sections() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("recursive-extract".into());
            assert_eq!(ui.get_screen(), 2, "递归解压工具页");
            assert_eq!(ui.get_active_tool_id().as_str(), "recursive-extract");
            assert_eq!(app.state.borrow().tool, Tool::Extract);
            assert_eq!(ui.get_section(), 0, "切工具回到第一个分区");
            ui.invoke_toggle_advanced(true);
            assert!(
                rule_value_at(ui, "max_depth").is_some(),
                "解压分区的高级层应显示防护上限"
            );
            assert!(
                rule_value_at(ui, "archive_delete").is_none(),
                "X-05/R-02：原包处置不得再是可配置项"
            );
            // 安全是两工具共享分区：递归解压视角不含 Hash 线程；磁盘预留在解压分区（高级层）
            ui.invoke_select_section(1);
            assert!(
                rule_value_at(ui, "hash_workers").is_none(),
                "Hash 线程属于目录整理"
            );
            assert!(
                rule_value_at(ui, "reserve_bytes").is_none(),
                "磁盘预留属于解压分区，不在安全与性能"
            );
            ui.invoke_select_section(0);
            assert!(
                rule_value_at(ui, "reserve_bytes").is_some(),
                "磁盘预留属于递归解压"
            );
            ui.invoke_toggle_advanced(false);
            ui.invoke_select_tool("directory-organizer".into());
            assert_eq!(ui.get_screen(), 0, "切回目录整理路由不串");
            assert_eq!(ui.get_active_tool_id().as_str(), "directory-organizer");
            assert_eq!(app.state.borrow().tool, Tool::Organizer);
            assert!(
                rule_value_at(ui, "dedup_same_name").is_some(),
                "默认分区是「去重」"
            );
            assert!(
                rule_value_at(ui, "nested_archives").is_none(),
                "解压规则不得出现在目录整理面板"
            );
        })
        .unwrap();
    }
    // 覆盖 C-11, U-11（回归 2026-09-19：运行中切换工具不得谎报「任务已结束」）。
    // 复现：确认执行的同一拍 busy 已同步置位、任务库状态已是 executing，此时切走
    // 再切回工具，on_select_tool → sync_ready_after_directory 的就绪重算会把一切非
    // ready 状态归为「已结束」，状态栏谎报「上次任务已结束」；切到递归解压方向则谎报
    // 「目录已就绪」。运行中文案由任务事件负责，工具切换不得改写。
    #[test]
    fn switching_tools_while_busy_keeps_running_status_text() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"same").unwrap();
        std::fs::write(root.join("b.txt"), b"same").unwrap();
        let state_dir = tmp.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        let prepared =
            engine::prepare_at(&root, Config::default(), Context::default(), &state_dir).unwrap();
        // 任务执行中：apply_with 执行期任务库状态为 executing（非 ready）。
        Database::open_existing(&prepared.directory)
            .unwrap()
            .set("status", &"executing".to_string())
            .unwrap();
        let task = prepared.directory;
        let directory = root.to_string_lossy().to_string();
        with_gui(move |app| {
            let ui = &app.ui;
            app.state.borrow_mut().task = Some(task.clone());
            ui.set_has_task(true);
            ui.set_busy(true);
            ui.set_directory(directory.clone().into());
            ui.set_status("正在执行已确认的整理计划".into());
            ui.invoke_select_tool("recursive-extract".into());
            ui.invoke_select_tool("directory-organizer".into());
            // 排空一段窗口：若工具切换确实发起了就绪重算，其快照会在这段时间落地并改写文案。
            for _ in 0..20 {
                app.pump.run(ui);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(ui.get_busy(), "切换工具不得改变运行状态");
            assert_eq!(
                ui.get_status().as_str(),
                "正在执行已确认的整理计划",
                "运行中切换工具不得把状态文案改写为「已结束/已就绪」（C-11 如实展示）"
            );
        })
        .unwrap();
    }
    // 覆盖 R-01, R-02, C-02
    #[test]
    fn dependent_rows_follow_their_switches() {
        with_gui(|app| {
            let ui = &app.ui;
            // 清理：修正扩展名不再依赖独立的类型检测行（影子键随行联动），位于高级层
            ui.invoke_select_section(2);
            ui.invoke_toggle_advanced(true);
            assert!(
                rule_value_at(ui, "fix_extension").is_some(),
                "修正扩展名在清理分区的高级层可见"
            );
            assert!(
                rule_value_at(ui, "detect_type").is_none(),
                "类型检测不再单列为规则行"
            );
            ui.invoke_rule_bool("fix_extension".into(), true);
            assert!(
                app.state.borrow().config.detect_type,
                "开启修正扩展名必须自动开启类型检测"
            );
            ui.invoke_toggle_advanced(false);
            // 归类：R-02 已删除自定义分类规则与 classify 开关；大文件阈值（字节口径，
            // 高级层）依赖大文件单独归类开启后才显示
            ui.invoke_select_section(1);
            assert!(rule_value_at(ui, "custom_categories").is_none());
            assert!(
                rule_value_at(ui, "classify").is_none(),
                "R-02：归类方式开关已删除（固定「大类」一级结构）"
            );
            assert!(rule_value_at(ui, "large_threshold_bytes").is_none());
            ui.invoke_toggle_advanced(true);
            ui.invoke_rule_bool("large_files".into(), true);
            assert!(rule_value_at(ui, "large_threshold_bytes").is_some());
            ui.invoke_toggle_advanced(false);
            // 去重（R-02）：同名/副本名/不同名三个独立开关 + 重复组保留规则；
            // C-02 禁止版本取舍开关后，去重基础层固定 4 行，不再有随开关出现/收回的行。
            ui.invoke_select_section(0);
            assert!(
                rule_value_at(ui, "dedup_same_name").is_some()
                    && rule_value_at(ui, "dedup_copy_names").is_some()
                    && rule_value_at(ui, "dedup_other_names").is_some(),
                "去重三类开关必须同屏独立出现"
            );
            assert_eq!(
                ui.get_rules().row_count(),
                4,
                "去重基础层：三个去重开关 + 保留规则"
            );
            // R-02：解压侧不再有冲突删除方式行；目录整理任何分区都不得出现解压专用项。
            ui.invoke_select_tool("recursive-extract".into());
            ui.invoke_select_section(0);
            ui.invoke_toggle_advanced(true);
            assert!(
                rule_value_at(ui, "conflict_delete").is_none()
                    && rule_value_at(ui, "extract_conflict").is_none(),
                "X-04/R-02：解压冲突处置已固定，解压面板不得再有冲突策略行"
            );
            ui.invoke_toggle_advanced(false);
            ui.invoke_select_tool("directory-organizer".into());
            for section in 0..4 {
                ui.invoke_select_section(section);
            }
            ui.invoke_toggle_advanced(true);
            assert!(
                rule_value_at(ui, "conflict_delete").is_none(),
                "目录整理面板不含解压覆盖删除方式行"
            );
            ui.invoke_toggle_advanced(false);
        })
        .unwrap();
    }
    // 覆盖 R-02, C-08
    #[test]
    fn merged_rows_write_shadow_fields() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_section(0); // 去重
                                         // R-02：同名/副本名/不同名同内容三类独立启停，互不联动。
            ui.invoke_rule_bool("dedup_same_name".into(), false);
            {
                let cfg = &app.state.borrow().config;
                assert!(!cfg.dedup_same_name, "同名去重开关只写自己");
                assert!(
                    cfg.dedup_copy_names && !cfg.dedup_other_names,
                    "副本名/不同名去重必须保持独立，不得被联动（各自保持默认值：副本名开、不同名关）"
                );
            }
            ui.invoke_rule_bool("dedup_copy_names".into(), false);
            ui.invoke_rule_bool("dedup_other_names".into(), true);
            {
                let cfg = &app.state.borrow().config;
                assert!(
                    !cfg.dedup_copy_names && cfg.dedup_other_names,
                    "两类开关各自独立生效（不同名默认关闭，可手动开启）"
                );
            }
            // C-02 禁止版本淘汰开关后，剩余合并行：修正扩展名一行仍驱动两个细粒度字段。
            ui.invoke_select_section(2);
            ui.invoke_rule_bool("fix_extension".into(), true);
            {
                let cfg = &app.state.borrow().config;
                assert!(
                    cfg.fix_extension && cfg.detect_type,
                    "修正扩展名一行必须同时开启类型检测"
                );
            }
        })
        .unwrap();
    }
    // 覆盖 U-06 / 附录 D：非法输入保留供修正，并阻止以旧配置开始。
    #[test]
    fn invalid_number_input_reports_error_preserves_value_and_blocks_start() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("recursive-extract".into());
            ui.invoke_toggle_advanced(true); // 最大嵌套层数属于高级层，先让它显示出来
            ui.invoke_rule_text("max_depth".into(), "abc".into());
            assert!(
                ui.get_error_text().contains("非负整数"),
                "必须提示非法输入：{}",
                ui.get_error_text()
            );
            assert_eq!(
                rule_value_at(ui, "max_depth").as_deref(),
                Some("abc"),
                "非法输入必须保留，不能替用户恢复旧配置"
            );
            let root = temp_test_dir("invalid-number-start");
            ui.set_directory(root.display().to_string().into());
            ui.invoke_request_extract_start();
            assert_eq!(ui.get_confirm_kind(), 0, "非法数字不得启动解压清点");
            assert!(!ui.get_busy(), "非法数字不得启动解压任务");
            ui.invoke_rule_text("max_depth".into(), "16".into());
            assert!(ui.get_error_text().is_empty(), "修正输入后必须恢复合法状态");
        })
        .unwrap();
    }
    #[test]
    fn invalid_number_draft_survives_navigation_and_empty_and_overflow_inputs() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("recursive-extract".into());
            ui.invoke_toggle_advanced(true);
            for text in ["", "18446744073709551616", "4294967296"] {
                ui.invoke_rule_text("max_depth".into(), text.into());
                ui.invoke_select_section(1);
                ui.invoke_select_section(0);
                assert_eq!(rule_value_at(ui, "max_depth").as_deref(), Some(text));
                ui.invoke_request_extract_start();
                assert_eq!(ui.get_confirm_kind(), 0);
                assert!(!ui.get_error_text().is_empty(), "空值/溢出须显示字段错误");
            }
        })
        .unwrap();
    }

    #[test]
    fn invalid_large_threshold_is_ignored_only_until_rule_reenabled() {
        let root = temp_test_dir("invalid-large-threshold");
        with_gui(move |app| {
            let ui = &app.ui;
            ui.invoke_select_tool("directory-organizer".into());
            ui.invoke_select_section(1);
            ui.invoke_toggle_advanced(true);
            ui.invoke_rule_bool("large_files".into(), true);
            ui.invoke_rule_text("large_threshold_bytes".into(), "abc".into());
            ui.invoke_rule_bool("large_files".into(), false);
            assert!(
                ui.get_error_text().is_empty(),
                "关闭附属规则不应阻止无关处理"
            );
            ui.invoke_select_tool("recursive-extract".into());
            ui.set_directory(root.display().to_string().into());
            ui.invoke_request_extract_start();
            assert_eq!(ui.get_confirm_kind(), 1, "整理草稿不得阻止独立解压工具");
            ui.set_confirm_kind(0);
            ui.invoke_select_tool("directory-organizer".into());
            ui.invoke_select_section(1);
            ui.invoke_rule_bool("large_files".into(), true);
            ui.invoke_request_start();
            assert!(!ui.get_busy(), "重新启用非法附属字段必须禁止分析");
            assert!(!ui.get_error_text().is_empty());
            assert_eq!(
                rule_value_at(ui, "large_threshold_bytes").as_deref(),
                Some("abc")
            );
        })
        .unwrap();
    }
    #[test]
    fn directory_dialog_uses_own_field_start() {
        let root = temp_test_dir("directory-dialog-fields");
        let fields = [
            "shared",
            "md-input",
            "md-output",
            "md-split",
            "git",
            "runtime",
            "custom",
            "convert-input",
            "convert-output",
        ];
        for field in fields {
            std::fs::create_dir_all(root.join(field)).unwrap();
        }
        with_gui(move |app| {
            let ui = &app.ui;
            ui.set_directory(root.join("shared").display().to_string().into());
            ui.set_md_input_dir(root.join("md-input").display().to_string().into());
            ui.set_md_output_dir(root.join("md-output").display().to_string().into());
            ui.set_md_split_dir(root.join("md-split").display().to_string().into());
            ui.set_git_repo(root.join("git").display().to_string().into());
            ui.set_convert_runtime_dir(root.join("runtime").display().to_string().into());
            ui.set_settings_custom_dir(root.join("custom").display().to_string().into());
            ui.set_convert_input_dir(root.join("convert-input").display().to_string().into());
            ui.set_convert_output_dir(root.join("convert-output").display().to_string().into());
            for entered in [
                ui.get_md_input_dir(),
                ui.get_md_output_dir(),
                ui.get_md_split_dir(),
                ui.get_git_repo(),
                ui.get_convert_runtime_dir(),
                ui.get_settings_custom_dir(),
                ui.get_convert_input_dir(),
                ui.get_convert_output_dir(),
            ] {
                assert_eq!(
                    directory_dialog_start(entered.as_str()),
                    Some(PathBuf::from(entered.as_str())),
                    "U-07：每个目录选择须使用当前字段，而不是其他工具的目录"
                );
            }
            assert_eq!(
                directory_dialog_start(""),
                None,
                "空输入不得回退其他工具字段"
            );
        })
        .unwrap();
    }

    // 覆盖 C-10, P-04
    #[test]
    fn theme_choice_keeps_plan_ready_but_rule_change_invalidates() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.set_ready(true);
            ui.invoke_select_theme(2); // 深色（「关于」页的主题选择）
            assert_eq!(ui.get_theme(), 2);
            assert!(ui.get_ready(), "纯外观的主题切换不得使已生成的计划失效");
            ui.invoke_rule_bool("clean_temp".into(), false);
            assert!(!ui.get_ready(), "实际规则变更必须使计划失效并要求重新分析");
        })
        .unwrap();
    }
    // 覆盖 C-10, C-11, R-04（切换工具不是规则或目录改动，就绪计划必须保持可执行）
    #[test]
    fn switching_tools_keeps_ready_plan_executable() {
        with_gui(|app| {
            let ui = &app.ui;
            let dir = temp_test_dir("switch-keeps-ready");
            let root = crate::fsutil::normalize_root(&dir).unwrap();
            {
                let db = Database::create(&dir).unwrap();
                db.set("root", &crate::fsutil::path_string(&root).unwrap())
                    .unwrap();
                db.set("config", &app.state.borrow().config.clone())
                    .unwrap();
                db.set("status", &"ready").unwrap();
            }
            app.state.borrow_mut().task = Some(dir.clone());
            ui.set_directory(dir.display().to_string().into());
            ui.set_has_task(true);
            ui.set_ready(true);
            ui.invoke_select_tool("recursive-extract".into());
            ui.invoke_select_tool("directory-organizer".into());
            // 就绪重算在后台 worker 上完成（开库 + canonicalize 不进界面线程）：等快照落地。
            assert!(
                pump_until(app, || ui.get_ready()),
                "任务、配置与目录都没变，切工具往返后计划必须仍可执行：status={}",
                ui.get_status()
            );
            assert!(
                !ui.get_status().contains("已改变"),
                "不得谎报规则或目录已改变：{}",
                ui.get_status()
            );
            let _ = std::fs::remove_dir_all(&dir);
        })
        .unwrap();
    }
    // 覆盖 U-11/H-02：整理任务后台收尾时，不得把用户当前选中的另一工具面板顶走。
    #[test]
    fn organizer_finish_does_not_replace_another_tool_panel() {
        with_gui(|app| {
            let ui = &app.ui;
            let dir = temp_test_dir("finish-while-other-tool-visible");
            ui.invoke_select_tool("md-organizer".into());
            ui.set_status("MD 工具正在显示".into());
            assert_eq!(ui.get_panel(), 0);
            app.pump
                .finish_task(ui, dir.clone(), &Summary::default(), true);
            assert_eq!(ui.get_active_tool_id().as_str(), "md-organizer");
            assert_eq!(ui.get_panel(), 0, "后台整理收尾不得切换其它工具面板");
            assert_eq!(ui.get_status().as_str(), "MD 工具正在显示");
            let _ = std::fs::remove_dir_all(dir);
        })
        .unwrap();
    }
    // 覆盖 C-01：依赖重算在途时拒绝新的勾选保存，避免两个重算线程并发修改同一任务库。
    #[test]
    fn plan_toggle_is_gated_during_dependency_recompute() {
        with_gui(|app| {
            let dir = temp_test_dir("plan-recompute-toggle-gate");
            app.state.borrow_mut().task = Some(dir.clone());
            app.state.borrow_mut().plan_recompute_inflight = 1;
            app.ui.invoke_plan_toggle("1".into(), false);
            assert_eq!(app.state.borrow().pending_selection, 0);
            assert_eq!(app.state.borrow().plan_recompute_inflight, 1);
            let _ = std::fs::remove_dir_all(dir);
        })
        .unwrap();
    }
    // C-01：快速连续勾选按用户顺序串行保存，数据库与即时 UI 最终都保留最后一次选择。
    #[test]
    fn rapid_plan_toggles_persist_last_intent() {
        with_gui(|app| {
            let task = temp_test_dir("plan-toggle-order");
            let id = {
                let db = Database::create(&task).unwrap();
                db.set("status", &"ready".to_string()).unwrap();
                db.add_action(&crate::model::Action {
                    id: 0,
                    kind: crate::model::ActionKind::Delete,
                    source: "synthetic.txt".into(),
                    target: None,
                    reason: "测试".into(),
                    expected: None,
                    keeper: None,
                    hash: None,
                    mode: crate::config::DeleteMode::Permanent,
                    selected: false,
                    state: "pending".into(),
                })
                .unwrap()
            };
            app.state.borrow_mut().task = Some(task.clone());
            app.ui.set_plans(
                Rc::new(VecModel::from(vec![PlanRow {
                    id: id.to_string().into(),
                    selected: false,
                    kind: "删除".into(),
                    source: "synthetic.txt".into(),
                    target: "".into(),
                    reason: "测试".into(),
                    state: "已取消勾选".into(),
                }]))
                .into(),
            );
            app.ui.invoke_plan_toggle(id.to_string().into(), true);
            app.ui.invoke_plan_toggle(id.to_string().into(), false);
            assert_eq!(app.state.borrow().pending_selection, 2);
            assert!(
                !app.ui.get_plans().row_data(0).unwrap().selected,
                "最后一次取消勾选必须即时生效"
            );
            // 阻止测试计划进入依赖重算；保存路径仍是 GUI 回调与真实任务库。
            app.state.borrow_mut().plan_recompute_inflight = 1;
            assert!(
                pump_until(app, || app.state.borrow().pending_selection == 0),
                "排队的勾选写入应收尾"
            );
            assert!(
                !Database::open_existing(&task)
                    .unwrap()
                    .action(id)
                    .unwrap()
                    .selected,
                "数据库必须保留最后一次明确取消勾选"
            );
            assert!(
                !app.ui.get_plans().row_data(0).unwrap().selected,
                "较早保存完成不得把更新的 UI 勾选改回"
            );
            let _ = std::fs::remove_dir_all(task);
        })
        .unwrap();
    }

    // C-01：即使较早成功事件先到，仍在途的较新意图也不得被回写覆盖。
    #[test]
    fn stale_selection_completion_keeps_latest_ui_choice() {
        with_gui(|app| {
            let task = PathBuf::from("stale-selection-event");
            app.state.borrow_mut().task = Some(task.clone());
            app.state.borrow_mut().pending_selection = 2;
            app.ui.set_busy(true);
            app.ui.set_plans(
                Rc::new(VecModel::from(vec![PlanRow {
                    id: "41".into(),
                    selected: false,
                    kind: "删除".into(),
                    source: "synthetic.txt".into(),
                    target: "".into(),
                    reason: "测试".into(),
                    state: "已取消勾选".into(),
                }]))
                .into(),
            );

            app.pump
                .out
                .send(Event::SelectionSaved(task.clone(), Some((41, true)), None))
                .unwrap();
            app.pump.run(&app.ui);
            assert_eq!(app.state.borrow().pending_selection, 1);
            assert!(
                !app.ui.get_plans().row_data(0).unwrap().selected,
                "较早勾选完成事件不得撤销用户较新的取消勾选"
            );

            app.pump
                .out
                .send(Event::SelectionSaved(task, Some((41, false)), None))
                .unwrap();
            app.pump.run(&app.ui);
            assert!(!app.ui.get_plans().row_data(0).unwrap().selected);
        })
        .unwrap();
    }
    // 覆盖 U-11（切工具回到默认面板；共享面板索引不得停留在另一工具才有的面板）
    #[test]
    fn switching_tools_resets_shared_panel_to_default() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.set_panel(2);
            ui.invoke_select_tool("recursive-extract".into());
            assert_eq!(
                ui.get_panel(),
                0,
                "递归解压没有第三面板，panel=2 会让内容区整块空白"
            );
            ui.set_panel(1);
            ui.invoke_select_tool("directory-organizer".into());
            assert_eq!(
                ui.get_panel(),
                0,
                "切回目录整理必须回到默认面板「处理规则」"
            );
        })
        .unwrap();
    }
    // 覆盖 C-11（已结束任务不得谎报「规则或目录已改变」，状态须如实）
    #[test]
    fn switching_back_to_finished_task_reports_finished_status() {
        with_gui(|app| {
            let ui = &app.ui;
            let dir = temp_test_dir("switch-finished");
            let root = crate::fsutil::normalize_root(&dir).unwrap();
            {
                let db = Database::create(&dir).unwrap();
                db.set("root", &crate::fsutil::path_string(&root).unwrap())
                    .unwrap();
                db.set("config", &app.state.borrow().config.clone())
                    .unwrap();
                db.set("status", &"finished").unwrap();
            }
            app.state.borrow_mut().task = Some(dir.clone());
            ui.set_directory(dir.display().to_string().into());
            ui.set_has_task(true);
            ui.invoke_select_tool("recursive-extract".into());
            ui.invoke_select_tool("directory-organizer".into());
            // 就绪重算在后台 worker 上完成：等快照落地后按库真值断言（C-11 如实展示）。
            assert!(
                pump_until(app, || ui.get_status().contains("已结束")),
                "任务只是已结束，状态必须如实说明而不是谎报改动：{}",
                ui.get_status()
            );
            assert!(!ui.get_ready(), "已结束的任务不得重新点亮执行");
            assert!(
                !ui.get_plan_editable(),
                "已结束的任务不得再允许勾选（勾选必然写不进任务库）"
            );
            let _ = std::fs::remove_dir_all(&dir);
        })
        .unwrap();
    }
    // 覆盖 R-04（目录改动后立即反馈，不得凭陈旧目录继续）
    #[test]
    fn root_edited_reports_missing_directory() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.set_directory("D:/surely-missing-dir-42/data".into());
            ui.invoke_root_edited();
            assert!(
                ui.get_status().contains("目录不存在"),
                "输入不存在的目录必须立即提示：status={} directory={}",
                ui.get_status(),
                ui.get_directory()
            );
        })
        .unwrap();
    }
    // 覆盖 P-02
    #[test]
    fn search_tools_filters_registry() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_search_tools("目录".into());
            assert_eq!(ui.get_tools().row_count(), 1);
            ui.invoke_search_tools("不存在的工具名".into());
            assert_eq!(ui.get_tools().row_count(), 0);
        })
        .unwrap();
    }
    // 覆盖 P-02
    #[test]
    fn navigation_switches_screen() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_navigation(1);
            assert_eq!(ui.get_screen(), 1, "关于页");
            ui.invoke_navigation(0);
            assert_eq!(ui.get_screen(), 0);
        })
        .unwrap();
    }
    // 覆盖 R-04：选择新目录后必须立即撤销旧计划执行权，不能等磁盘读取返回。
    #[test]
    fn directory_recheck_disarms_before_worker_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("old");
        let other = tmp.path().join("new");
        let storage = tmp.path().join("state");
        for path in [&root, &other, &storage] {
            std::fs::create_dir(path).unwrap();
        }
        let prepared =
            engine::prepare_at(&root, Config::default(), Context::default(), &storage).unwrap();
        with_gui(move |app| {
            app.state.borrow_mut().task = Some(prepared.directory);
            app.state.borrow_mut().tool = Tool::Organizer;
            app.ui.set_has_task(true);
            app.ui.set_ready(true);
            app.ui.set_directory(other.display().to_string().into());
            sync_ready_after_directory(&app.ui, &app.state.borrow(), &app.pump.out);
            assert!(!app.ui.get_ready(), "目录已改变时旧计划不得继续执行");
            assert!(pump_until(app, || {
                app.ui.get_status().as_str() == PlanReadyState::Changed.status_text()
            }));
        })
        .unwrap();
    }

    // 覆盖 R-04、C-11：翻页请求顶替目录检查时，仍须解释为何计划已经失效。
    #[test]
    fn directory_recheck_status_survives_superseding_page_request() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("old");
        let other = tmp.path().join("new");
        let storage = tmp.path().join("state");
        for path in [&root, &other, &storage] {
            std::fs::create_dir(path).unwrap();
        }
        let prepared =
            engine::prepare_at(&root, Config::default(), Context::default(), &storage).unwrap();
        with_gui(move |app| {
            app.state.borrow_mut().task = Some(prepared.directory.clone());
            app.state.borrow_mut().tool = Tool::Organizer;
            app.ui.set_has_task(true);
            app.ui.set_ready(true);
            app.ui
                .set_status(PlanReadyState::Ready.status_text().into());
            app.ui.set_directory(other.display().to_string().into());
            sync_ready_after_directory(&app.ui, &app.state.borrow(), &app.pump.out);
            load_plan_state(
                &app.pump.out,
                &app.state.borrow().plan_load,
                prepared.directory,
                app.ui.get_directory().to_string(),
                PlanQuery::page(0, 0, None, false),
            );
            assert!(
                pump_until(app, || {
                    app.ui.get_status().as_str() == PlanReadyState::Changed.status_text()
                }),
                "较新的页面结果不得遗失目录改变后的状态反馈"
            );
            assert!(!app.ui.get_ready());
        })
        .unwrap();
    }

    // ---- 纯函数回归：执行分母计数与就绪判定（不依赖 GUI 工作线程）----

    /// 覆盖 C-11/H-02：就绪只由「后台快照 + 当前界面配置/目录」决定。快照对应的目录或库内
    /// 配置已不是当前值时不得凭陈旧快照点亮执行；任务库读不到时既不可执行也不可勾选。
    #[test]
    fn plan_readiness_fails_closed_on_stale_snapshot() {
        let config = Config::default();
        let directory = "D:/data";
        // worker 侧的库内配置已剔除 theme（见 plan_snapshot），界面侧同样剔除后比较。
        let snapshot = |status: Option<&str>, requested: &str, root_matches: bool, db: &Config| {
            let mut config_json = serde_json::to_value(db).unwrap();
            config_json.as_object_mut().unwrap().remove("theme");
            PlanSnapshot {
                status: status.map(str::to_string),
                config_json: Some(config_json),
                root_matches,
                requested_directory: requested.to_string(),
                summary: None,
            }
        };
        // 目录、库内配置与根目录都一致且任务就绪：可执行。
        assert!(matches!(
            classify_plan_snapshot(
                Some(&snapshot(Some("ready"), directory, true, &config)),
                directory,
                &config
            ),
            PlanReadyState::Ready
        ));
        // 用户已改目录（去抖重算尚未回来）：不得用旧快照点亮执行。
        assert!(matches!(
            classify_plan_snapshot(
                Some(&snapshot(Some("ready"), "D:/other", true, &config)),
                directory,
                &config
            ),
            PlanReadyState::Changed
        ));
        // 规则已改或目录与任务库根目录不一致：按「已改变」处理（可继续勾选，但不能执行）。
        let other = Config {
            clean_temp: true,
            ..Config::default()
        };
        assert!(matches!(
            classify_plan_snapshot(
                Some(&snapshot(Some("ready"), directory, true, &other)),
                directory,
                &config
            ),
            PlanReadyState::Changed
        ));
        assert!(matches!(
            classify_plan_snapshot(
                Some(&snapshot(Some("ready"), directory, false, &config)),
                directory,
                &config
            ),
            PlanReadyState::Changed
        ));
        // 任务已结束：既不可执行也不可勾选（勾选必然写不进任务库）。
        assert!(matches!(
            classify_plan_snapshot(
                Some(&snapshot(Some("finished"), directory, true, &config)),
                directory,
                &config
            ),
            PlanReadyState::Finished
        ));
        // 任务库快照缺少状态元数据时不得误报成已结束。
        assert!(matches!(
            classify_plan_snapshot(
                Some(&snapshot(None, directory, true, &config)),
                directory,
                &config
            ),
            PlanReadyState::Unavailable
        ));
        // 任务库读不到（快照缺失）：fail-closed。
        assert!(matches!(
            classify_plan_snapshot(None, directory, &config),
            PlanReadyState::Unavailable
        ));
    }

    fn temp_test_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("jchtools-gui-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // 覆盖 U-03, C-11（进度分母只含仍勾选且待执行的项）
    #[test]
    fn count_selected_pending_ignores_unselected_and_done() {
        use crate::config::DeleteMode;
        use crate::model::{Action, ActionKind};
        let dir = temp_test_dir("count-selected");
        let db = Database::create(&dir).unwrap();
        // 2 个仍勾选且待执行；1 个取消勾选；1 个已执行
        for (selected, state) in [
            (true, "pending"),
            (true, "pending"),
            (false, "pending"),
            (true, "done"),
        ] {
            let action = Action {
                id: 0,
                kind: ActionKind::Delete,
                source: format!("a-{selected}-{state}"),
                target: None,
                reason: String::new(),
                expected: None,
                keeper: None,
                hash: None,
                mode: DeleteMode::Keep,
                selected,
                state: state.to_string(),
            };
            let id = db.add_action(&action).unwrap();
            // add_action 固定写 pending，显式标记需要非 pending 的状态。
            if state != "pending" {
                db.mark_action(id, state).unwrap();
            }
        }
        drop(db);
        assert_eq!(count_selected_pending(&dir).unwrap(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 覆盖 S-02, R-02, C-08（整理确认逐项如实告知删除后果：按当前开关与删除方式，解压处置不再出现）
    #[test]
    fn organizer_warning_mentions_only_organizer_deletes() {
        let warning = Config::default().destructive_warning();
        assert!(
            warning.contains("重复副本") && warning.contains("永久删除"),
            "整理确认必须告知重复副本的删除方式：{warning}"
        );
        for category in ["系统附属文件", "临时与备份文件", "零字节文件"] {
            assert!(
                warning.contains(category),
                "C-08：整理确认必须逐项说明清理后果（{category}）：{warning}"
            );
        }
        assert!(
            warning.contains("空目录"),
            "H-05/C-07：空目录清理不可关闭，必须如实告知：{warning}"
        );
        assert!(
            warning.contains("不可由本软件恢复"),
            "S-02：必须明确告知永久删除不可恢复：{warning}"
        );
        for forbidden in ["原压缩包", "解压覆盖", "解压冲突"] {
            assert!(
                !warning.contains(forbidden),
                "R-02：解压侧的处置已固定，不得出现在整理确认里（{forbidden}）：{warning}"
            );
        }
        // 关闭的清理项如实显示为「不清理」，而不是宣称会删除。
        let config = Config {
            clean_temp: false,
            ..Config::default()
        };
        assert!(
            config
                .destructive_warning()
                .contains("临时与备份文件：不清理"),
            "关闭的清理类别不得宣称删除：{}",
            config.destructive_warning()
        );
    }

    // 覆盖 X-04, X-05, R-02, R-03, C-08, S-04（规则表与配置只暴露合同允许的开关与默认值）
    #[test]
    fn rules_and_config_expose_only_contracted_switches() {
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../resources/rules.json")).unwrap();
        let keys: Vec<&str> = rows.iter().filter_map(|row| row["key"].as_str()).collect();
        let config = serde_json::to_value(Config::default()).unwrap();
        // X-05/X-04/R-02：原包处置、冲突策略、冲突删除方式与嵌套解压开关不再存在。
        for forbidden in [
            "nested_archives",
            "archive_delete",
            "extract_conflict",
            "conflict_delete",
            "clean_empty_dirs",
        ] {
            assert!(
                !keys.contains(&forbidden),
                "R-02：规则表不得再有 {forbidden} 行（行为已固定）"
            );
            assert!(
                config.get(forbidden).is_none(),
                "R-02：配置不得再有 {forbidden} 字段"
            );
        }
        // C-07/H-05：空目录清理不可关闭，因此不得再有总开关或统一删除方式行。
        assert!(
            !keys.contains(&"cleanup_delete"),
            "C-08：清理删除方式必须按项独立，不得再有一个覆盖全部清理项的行"
        );
        assert!(
            config.get("cleanup_delete").is_none(),
            "C-08：配置不得再有 cleanup_delete 字段"
        );
        // C-08 六类独立开关：规则表保留且默认值逐项与合同一致。
        for (key, expected) in [
            ("clean_junk", true),
            ("clean_temp", false),
            ("clean_zero", false),
            ("clean_copy_name", true),
            ("normalize_names", true),
            ("fix_extension", false),
        ] {
            assert!(keys.contains(&key), "C-08：规则表必须保留独立开关 {key}");
            assert_eq!(
                config[key].as_bool(),
                Some(expected),
                "C-08：{key} 的默认值必须为 {expected}"
            );
        }
        // C-08：涉及删除的清理项各自独立覆盖删除方式，默认跟随全局文件删除方式。
        for key in ["junk_delete", "temp_delete", "zero_delete"] {
            assert!(keys.contains(&key), "C-08：{key} 必须以独立删除方式行暴露");
            assert_eq!(
                config[key].as_str(),
                Some("global"),
                "C-08：{key} 默认必须跟随全局文件删除方式"
            );
        }
        // S-04/R-03：默认覆盖隐藏与系统属性资料，排除规则默认为空（不额外缩小范围）。
        assert_eq!(config["include_hidden"].as_bool(), Some(true));
        assert_eq!(config["include_system"].as_bool(), Some(true));
        assert_eq!(config["exclusions"].as_str(), Some(""));
        assert_eq!(config["recursive"].as_bool(), Some(true));
    }

    // 覆盖 R-03, S-04（主动关闭递归或排除资料时明确提示范围缩小；恢复默认范围后提示消失）
    #[test]
    fn narrowing_scope_shows_notice_and_restoring_clears_it() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("recursive-extract".into());
            assert_eq!(ui.get_notice_text().as_str(), "", "默认范围不提示缩小");
            ui.invoke_rule_bool("recursive".into(), false);
            assert!(
                ui.get_notice_text().contains("已缩小处理范围"),
                "R-03：关闭递归必须明确提示范围缩小：{}",
                ui.get_notice_text()
            );
            assert!(
                ui.get_notice_text().contains("子目录"),
                "提示须说明只处理所选目录的直接子项：{}",
                ui.get_notice_text()
            );
            ui.invoke_rule_bool("include_hidden".into(), false);
            assert!(
                ui.get_notice_text().contains("隐藏"),
                "S-04：不包含隐藏资料同样要提示范围缩小：{}",
                ui.get_notice_text()
            );
            ui.invoke_rule_bool("recursive".into(), true);
            ui.invoke_rule_bool("include_hidden".into(), true);
            assert_eq!(
                ui.get_notice_text().as_str(),
                "",
                "恢复默认范围后不得残留缩小提示"
            );
        })
        .unwrap();
    }

    // 覆盖 X-02/R-02：递归解压入口必须按解压专属规则校验并在错误时不启动。
    #[test]
    fn extract_start_validates_extract_configuration() {
        let root = temp_test_dir("extract-config-validation");
        with_gui(move |app| {
            let ui = &app.ui;
            ui.set_directory(root.display().to_string().into());
            app.state.borrow_mut().config.max_depth = 0;

            start_extract(ui, &app.state, &app.pump.out);

            assert!(
                ui.get_error_text().contains("嵌套层数"),
                "无效解压配置必须显示对应错误：{}",
                ui.get_error_text()
            );
            assert!(!ui.get_busy(), "校验失败不得启动解压任务");
            let _ = std::fs::remove_dir_all(&root);
        })
        .unwrap();
    }

    // 覆盖 X-02, S-05（受保护目录在打开确认框之前就被拒绝，不得进入"清点中"占位态）
    // 平台门禁原因：S-05 的安装目录保护分支依赖 Windows 环境变量与路径语义。
    #[cfg(windows)]
    #[test]
    fn extract_start_rejects_protected_directory_immediately() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("recursive-extract".into());
            let protected = std::env::var("SystemRoot").unwrap();
            ui.set_directory(protected.clone().into());
            ui.invoke_request_extract_start();
            assert_ne!(
                ui.get_confirm_kind(),
                1,
                "受保护目录不得打开解压确认框（清点必然失败）"
            );
            assert!(
                !ui.get_error_text().is_empty(),
                "必须给出明确错误：{}",
                ui.get_error_text()
            );
        })
        .unwrap();
    }

    // 覆盖 C-11, C-01（另一工具失败/取消后切回整理：仍就绪的计划必须保持可勾选）
    #[test]
    fn plan_editable_recovers_when_ready_task_is_shown_again() {
        // 回归：plan-editable 曾只在任务事件里刷新，而 Event::Failed 是两个工具共用的收尾
        // 事件——解压取消会把禁用态带到整理工具，切回后计划可执行却勾不动（C-01 逐项可勾选）。
        with_gui(|app| {
            let task = temp_test_dir("plan-editable-task");
            let root = temp_test_dir("plan-editable-root");
            let canonical = std::fs::canonicalize(&root).unwrap();
            let db = Database::create(&task).unwrap();
            db.set("root", &crate::fsutil::path_string(&canonical).unwrap())
                .unwrap();
            db.set("config", &crate::config::Config::default()).unwrap();
            db.set("status", &"ready".to_string()).unwrap();
            drop(db);
            {
                let mut s = app.state.borrow_mut();
                s.task = Some(task.clone());
            }
            let ui = &app.ui;
            ui.set_has_task(true);
            ui.set_directory(root.to_string_lossy().to_string().into());
            // 模拟解压取消/失败后残留的禁用态（Event::Failed 收尾时置为 false）。
            ui.set_plan_editable(false);
            sync_ready_after_directory(ui, &app.state.borrow(), &app.pump.out);
            // 就绪判定在后台 worker 上（开库 + canonicalize 不进界面线程）：等快照落地。
            assert!(
                pump_until(app, || ui.get_plan_editable() && ui.get_ready()),
                "仍就绪的整理计划在切回后必须保持可勾选（C-01/C-11）：status={}",
                ui.get_status()
            );
            assert!(ui.get_ready(), "同一状态下主按钮应可用");
            // 反向：任务已取消/失败（任务库状态非 ready）后勾选必须禁用，
            // 否则用户点到的是必然被 db::set_selected 拒绝的行（C-11）。
            let db = Database::open_existing(&task).unwrap();
            db.set("status", &"cancelled".to_string()).unwrap();
            drop(db);
            sync_ready_after_directory(ui, &app.state.borrow(), &app.pump.out);
            assert!(
                pump_until(app, || !ui.get_plan_editable()),
                "已取消任务的计划不得再可勾选（C-11）：status={}",
                ui.get_status()
            );
            assert!(!ui.get_ready(), "已取消任务不得再显示为可执行");
        })
        .unwrap();
    }

    // 覆盖 C-11（计划行「已执行/已跳过/执行失败」必须以中文显示，不得透出英文状态值；
    // 任务库存 done/skipped/failed，读取链必须翻译）
    #[test]
    fn plan_row_state_maps_terminal_states_to_chinese() {
        assert_eq!(plan_row_state(true, "done"), "已执行");
        assert_eq!(plan_row_state(true, "skipped"), "已跳过");
        assert_eq!(plan_row_state(true, "failed"), "执行失败");
        // 既有中文旧值别名保持稳定
        assert_eq!(plan_row_state(true, "已执行"), "已执行");
        assert_eq!(plan_row_state(true, "已跳过"), "已跳过");
        assert_eq!(plan_row_state(true, "执行失败"), "执行失败");
        // 未执行语义不回归：勾选即时生效口径保持
        assert_eq!(plan_row_state(true, "pending"), "待执行");
        assert_eq!(plan_row_state(false, "pending"), "已取消勾选");
        assert_eq!(plan_row_state(true, "unselected"), "待执行");
        assert_eq!(plan_row_state(false, "unselected"), "已取消勾选");
        // 勾选门禁联动（ui/app.slint 只认「待执行/已取消勾选」可勾选）：
        // 补映射后已执行/已跳过/执行失败行仍不得落入可勾选状态。
        for terminal in ["done", "skipped", "failed", "已执行", "已跳过", "执行失败"] {
            assert!(
                !matches!(plan_row_state(true, terminal), "待执行" | "已取消勾选"),
                "终态 {terminal} 不得映射为可勾选状态（勾选门禁失效）"
            );
        }
    }

    // 覆盖 U-11, C-11（切到 MD 整理/Git 工具时状态栏必须显示本工具中性文案，
    // 不得出现「分析/整理/规则」等目录整理专属流程词）
    #[test]
    fn switching_to_md_or_git_shows_neutral_status() {
        with_gui(|app| {
            let ui = &app.ui;
            let assert_neutral = |ui: &AppWindow, page: &str| {
                let status = ui.get_status().to_string();
                for forbidden in ["分析", "整理", "规则"] {
                    assert!(
                        !status.contains(forbidden),
                        "{page} 页状态栏不得出现目录整理流程词（{forbidden}）：{status}"
                    );
                }
            };
            ui.invoke_select_tool("md-organizer".into());
            assert_neutral(ui, "MD 整理");
            ui.invoke_select_tool("git-tools".into());
            assert_neutral(ui, "Git 工具");
            // 目录输入有效时是「就绪」类提示，同样不得串用目录整理流程词
            let ready_dir = temp_test_dir("neutral-status-dir");
            ui.set_directory(ready_dir.display().to_string().into());
            ui.invoke_select_tool("md-organizer".into());
            assert_neutral(ui, "MD 整理");
            assert!(
                ui.get_status().contains("就绪"),
                "目录有效时应显示就绪类提示：{}",
                ui.get_status()
            );
            // 目录无效时的反馈同样保持中性
            ui.set_directory("D:/surely-missing-dir-42/data".into());
            ui.invoke_root_edited();
            assert_neutral(ui, "MD 整理");
            assert!(
                ui.get_status().contains("目录不存在"),
                "无效目录必须立即提示：{}",
                ui.get_status()
            );
            let _ = std::fs::remove_dir_all(&ready_dir);
        })
        .unwrap();
    }

    /// 构造 MD 输入/输出目录：`file_count` 个带内容的 .md 文件。
    fn make_md_fixture(tag: &str, file_count: usize) -> (PathBuf, PathBuf, PathBuf) {
        let dir = temp_test_dir(tag);
        let docs = dir.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        let content = "# 标题\n\n正文内容\n".repeat(8192); // 约 120KB/文件
        for i in 0..file_count {
            std::fs::write(docs.join(format!("doc{i:03}.md")), &content).unwrap();
        }
        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        (docs, out_dir, dir)
    }

    // 覆盖 U-03：真实完成事件使用本次任务时钟，合并/拆分都不能继承旧 started。
    #[test]
    fn md_start_resets_started_clock() {
        let (docs, out_dir, dir) = make_md_fixture("md-clock-merge", 1);
        let merged_output = out_dir.join("merged.md");
        let docs_text = docs.display().to_string();
        let out_text = out_dir.display().to_string();
        with_gui(move |app| {
            app.ui.invoke_select_tool("md-organizer".into());
            let stale = Instant::now()
                .checked_sub(Duration::from_secs(600))
                .expect("系统运行时间不足 600 秒，无法构造旧时钟");
            app.state.borrow_mut().started = stale;
            app.ui.set_md_input_dir(docs_text.into());
            app.ui.set_md_output_name("merged.md".into());
            app.ui.set_md_output_dir(out_text.into());
            app.ui.invoke_md_merge_start();
            assert!(
                app.state.borrow().started > stale,
                "合并启动必须同步重置任务时钟"
            );
            assert!(
                pump_until(app, || app.ui.get_status().contains("合并完成")),
                "真实合并完成结果应上屏：{}",
                app.ui.get_status()
            );
        })
        .unwrap();
        assert!(merged_output.is_file(), "合并完成必须生成真实输出");
        let _ = std::fs::remove_dir_all(&dir);

        let (docs, out_dir, dir) = make_md_fixture("md-clock-split", 1);
        let input = docs.join("doc000.md");
        let input_text = input.display().to_string();
        let out_text = out_dir.display().to_string();
        with_gui(move |app| {
            app.ui.invoke_select_tool("md-organizer".into());
            app.ui.set_md_subpage(1);
            let stale = Instant::now()
                .checked_sub(Duration::from_secs(600))
                .expect("系统运行时间不足 600 秒，无法构造旧时钟");
            app.state.borrow_mut().started = stale;
            app.ui.set_md_split_file(input_text.into());
            app.ui.set_md_split_size("1".into());
            app.ui.set_md_split_unit(1);
            app.ui.set_md_split_dir(out_text.into());
            app.ui.invoke_md_split_start();
            assert!(
                app.state.borrow().started > stale,
                "拆分启动必须同步重置任务时钟"
            );
            assert!(
                pump_until(app, || app.ui.get_status().contains("拆分完成")),
                "真实拆分完成结果应上屏：{}",
                app.ui.get_status()
            );
        })
        .unwrap();
        assert!(
            std::fs::read_dir(&out_dir)
                .unwrap()
                .any(|entry| entry.unwrap().path().is_file()),
            "拆分完成必须生成真实分片"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 覆盖 U-09（MD/Git 任务启动时必须重置 close_after：一次「停止并关闭」只作用于
    // 当次任务收尾，不得残留到之后的任务让收尾时意外退出应用）
    #[test]
    fn md_and_git_start_reset_close_after() {
        let (docs, out_dir, dir) = make_md_fixture("md-close-after-merge", 2);
        let docs_text = docs.display().to_string();
        let out_text = out_dir.display().to_string();
        with_gui(move |app| {
            app.ui.invoke_select_tool("md-organizer".into());
            app.state.borrow_mut().close_after = true;
            app.ui.set_md_input_dir(docs_text.into());
            app.ui.set_md_output_name("merged.md".into());
            app.ui.set_md_output_dir(out_text.into());
            app.ui.invoke_md_merge_start();
            assert!(
                !app.state.borrow().close_after,
                "MD 合并启动必须重置 close_after（U-09）：残留会让之后的任务收尾时意外关闭应用"
            );
            assert!(pump_until(app, || !app.ui.get_busy()), "合并应正常收尾");
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        // 拆分
        let (docs, out_dir, dir) = make_md_fixture("md-close-after-split", 2);
        let input_text = docs.join("doc000.md").display().to_string();
        let out_text = out_dir.display().to_string();
        with_gui(move |app| {
            app.ui.invoke_select_tool("md-organizer".into());
            app.ui.set_md_subpage(1);
            app.state.borrow_mut().close_after = true;
            app.ui.set_md_split_file(input_text.into());
            app.ui.set_md_split_size("1".into());
            app.ui.set_md_split_unit(0);
            app.ui.set_md_split_dir(out_text.into());
            app.ui.invoke_md_split_start();
            assert!(
                !app.state.borrow().close_after,
                "MD 拆分启动必须重置 close_after（U-09）"
            );
            assert!(pump_until(app, || !app.ui.get_busy()), "拆分应正常收尾");
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        // Git：仓库无效也会快速收尾，启动路径的重置必须同样生效
        let git_dir = temp_test_dir("git-close-after");
        let git_text = git_dir.display().to_string();
        with_gui(move |app| {
            app.ui.invoke_select_tool("git-tools".into());
            app.state.borrow_mut().close_after = true;
            app.ui.set_git_repo(git_text.into());
            app.ui.set_git_branch("旧分支".into());
            app.ui.set_git_upstream("旧远端".into());
            app.ui.set_git_current("旧文件.md".into());
            app.ui.set_git_stage("push".into());
            app.ui.set_git_retry(3);
            app.ui.set_git_retry_wait(80);
            app.ui.set_git_total(12);
            app.ui.set_git_done(9);
            app.ui.set_git_state("完成".into());
            app.ui.invoke_git_start();
            assert_eq!(app.ui.get_git_branch().as_str(), "", "新任务不得保留旧分支");
            assert_eq!(
                app.ui.get_git_upstream().as_str(),
                "",
                "新任务不得保留旧 upstream"
            );
            assert_eq!(
                app.ui.get_git_current().as_str(),
                "",
                "新任务不得保留旧文件"
            );
            assert_eq!(app.ui.get_git_retry(), 0, "新任务重试次数从零开始");
            assert_eq!(app.ui.get_git_retry_wait(), 0, "新任务不得保留旧退避计时");
            assert_eq!(app.ui.get_git_total(), 0, "新任务待处理总数从零开始");
            assert_eq!(app.ui.get_git_done(), 0, "新任务完成数从零开始");
            assert_eq!(app.ui.get_git_state().as_str(), "检查仓库");
            assert!(
                !app.state.borrow().close_after,
                "Git 启动必须重置 close_after（U-09）"
            );
            assert!(pump_until(app, || !app.ui.get_busy()), "Git 任务应快速收尾");
            assert_eq!(
                app.ui.get_git_state().as_str(),
                "失败",
                "无效仓库收尾不得残留检查仓库状态"
            );
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&git_dir);
    }

    // 覆盖 G-03~G-08、G-13（GUI 入口端到端）：从 git-tools 页面启动任务，经引擎对
    // 真实本地仓库逐文件提交并推送到裸远端，收尾把共享进度定格上屏。引擎行为已由
    // tests/git_tools.rs 覆盖；本用例补 GUI 启动接缝（目录属性读取、事件回传、
    // 完成态与 busy 复位）与真实推送结果的组合验证。
    #[test]
    fn git_start_pushes_real_repository_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let git = crate::git_tools::find_git().expect("PATH 中需有可用 git");
        let base = dir.path();
        let run_git = |cwd: &std::path::Path, args: &[&str]| {
            let ok = std::process::Command::new(&git)
                // 固定夹具默认分支为 master（与 tests/git_tools.rs::git_ok 一致，
                // 只作用于本次调用）：全局 init.defaultBranch 非 master 时，
                // 后续 push master 与裸远端 HEAD 指向都会失败。
                .args(["-c", "init.defaultBranch=master"])
                .args(args)
                .current_dir(cwd)
                .status()
                .is_ok_and(|status| status.success());
            assert!(ok, "夹具 git 调用失败：git {args:?} @ {}", cwd.display());
        };
        // seed（含初始提交）→ 裸远端 → clone 出带 upstream 的工作仓库（与
        // tests/git_tools.rs::fixture 同构）；仓库级提交身份对无全局配置的环境生效。
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        run_git(&seed, &["init", "-q"]);
        // 种子仓库同样需要仓库级提交身份：CI runner 与无全局 user.name/user.email
        // 的环境下首个提交会因缺身份失败（回归：CI run 37135458960）。
        run_git(&seed, &["config", "user.name", "JchTools Test"]);
        run_git(&seed, &["config", "user.email", "test@jchtools.local"]);
        std::fs::write(seed.join("README.md"), "init\n").unwrap();
        run_git(&seed, &["add", "README.md"]);
        run_git(&seed, &["commit", "-q", "-m", "init"]);
        let remote = base.join("remote.git");
        run_git(
            base,
            &["init", "-q", "--bare", &remote.display().to_string()],
        );
        run_git(
            &seed,
            &["push", "-q", &remote.display().to_string(), "master"],
        );
        let repo = base.join("repo");
        run_git(
            base,
            &["clone", "-q", &remote.display().to_string(), "repo"],
        );
        run_git(&repo, &["config", "user.name", "JchTools Test"]);
        run_git(&repo, &["config", "user.email", "test@jchtools.local"]);
        // 未提交的新文件是任务的全部输入。
        std::fs::write(repo.join("feature.md"), "from gui\n").unwrap();

        let repo_text = repo.display().to_string();
        with_gui(move |app| {
            app.ui.invoke_select_tool("git-tools".into());
            app.ui.set_git_repo(repo_text.into());
            app.ui.invoke_git_start();
            assert!(pump_until(app, || !app.ui.get_busy()), "Git 任务应正常收尾");
            assert_eq!(
                app.ui.get_git_state().as_str(),
                "完成",
                "收尾必须把共享状态定格上屏（G-13）：git_state={}",
                app.ui.get_git_state()
            );
            assert_eq!(app.ui.get_git_done(), 1, "单个文件应计入完成数");
            assert!(app.ui.get_git_total() >= 1, "总量应至少包含该文件");
        })
        .unwrap();

        // 推送结果以远端为准（G-08）：裸远端 HEAD 与工作仓库一致，且工作区已干净。
        let head = |cwd: &std::path::Path| {
            let out = std::process::Command::new(&git)
                .args(["rev-parse", "HEAD"])
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(out.status.success(), "rev-parse 失败");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        assert_eq!(
            head(&repo),
            head(&remote),
            "GUI 启动的任务必须把提交推送到远端"
        );
        let status = std::process::Command::new(&git)
            .args(["status", "--porcelain"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            status.status.success() && status.stdout.is_empty(),
            "提交后工作区应干净：{}",
            String::from_utf8_lossy(&status.stdout)
        );
    }

    // 覆盖 X-02（清点为 0 时必须提示无可处理包：不解除清点门禁、确认保持不可用，
    // 不得启动空解压任务）
    #[test]
    fn extract_count_zero_keeps_confirm_gated() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("keep.txt"), b"keep me").unwrap();
        let data_text = data.display().to_string();
        with_gui(move |app| {
            let ui = &app.ui;
            ui.invoke_select_tool("recursive-extract".into());
            ui.set_directory(data_text.into());
            ui.invoke_request_extract_start();
            assert!(
                pump_until(app, || {
                    ui.get_confirm_kind() == 1 && ui.get_confirm_text().contains("未发现")
                }),
                "清点为 0 必须给出「未发现可处理包」的如实提示：{}",
                ui.get_confirm_text()
            );
            assert!(
                ui.get_confirm_pending(),
                "清点为 0 不得解除清点门禁（确认必须保持不可用，X-02 不启动空解压任务）"
            );
            assert!(
                !ui.get_confirm_text().contains("永久删除"),
                "零包提示不得保留破坏性告知（没有任务可启动）：{}",
                ui.get_confirm_text()
            );
            // 返回检查：门禁状态随确认框关闭一并复位，可重新发起
            ui.set_confirm_kind(0);
            ui.set_confirm_pending(false);
        })
        .unwrap();
    }

    // 覆盖 X-02, S-02, X-04, X-05, X-06, H-07（发现数 > 0 的确认文案必须完整披露：
    // 原包及分卷永久删除且不可恢复、失败/取消保留、已有文件保留、冲突只为新文件自动改名、
    // 失败包去向，且不得出现覆盖或冲突策略授权）。该文案由 extract_confirm_text 构造，
    // 端到端零包用例无法覆盖（目录无压缩包时按 X-02 不再进入确认执行），故在此逐条断言。
    #[test]
    fn extract_confirm_text_discloses_deletion_rules() {
        let text = extract_confirm_text(
            "清点到 2 个压缩包（按当前扫描范围，已排除「解压失败」目录）。\n",
            "D:/data",
        );
        assert!(
            text.contains("永久删除"),
            "X-02/S-02：确认文案必须说明完整成功后原包及分卷永久删除：{text}"
        );
        assert!(
            text.contains("不可恢复"),
            "X-02/S-02：确认文案必须说明永久删除不可恢复：{text}"
        );
        assert!(
            text.contains("保留"),
            "X-05：确认文案必须说明失败/部分/取消时保留原包：{text}"
        );
        assert!(
            text.contains("已有文件"),
            "X-05/H-07：确认文案必须说明已有文件保留：{text}"
        );
        assert!(
            text.contains("自动改文件名"),
            "X-04/H-07：确认文案必须说明冲突只为新文件自动改名：{text}"
        );
        assert!(
            text.contains("「解压失败」"),
            "X-06：确认文案必须说明失败包去向：{text}"
        );
        assert!(
            !text.contains("覆盖") && !text.contains("冲突策略"),
            "X-04/H-07：解压确认不得出现覆盖或冲突策略授权：{text}"
        );
    }

    /// 构造带失败事件的解压任务库（U-10）：返回任务库目录。
    /// 事件 id 顺序即写入顺序；与 archive.rs 的真实写入序列一致：
    /// 失败记录在前，其卷集的「移入解压失败」紧跟其后，成功包与任务级记录穿插。
    fn make_extract_task_db(tag: &str) -> PathBuf {
        let dir = temp_test_dir(tag);
        let db = Database::create(&dir).unwrap();
        // 失败包 1：解压失败（引擎报错），两个卷整组隔离
        db.log("解压", "bad.7z", "", "失败", "压缩包已损坏", 0)
            .unwrap();
        db.log(
            "解压",
            "bad.7z",
            "解压失败/bad.7z",
            "移入解压失败",
            "解压失败：压缩包已损坏",
            0,
        )
        .unwrap();
        db.log(
            "解压",
            "bad.7z.002",
            "解压失败/bad.7z.002",
            "移入解压失败",
            "解压失败：压缩包已损坏",
            0,
        )
        .unwrap();
        // 失败包 2：未完全解开（成员被跳过），一个卷已隔离
        db.log(
            "解压",
            "part.zip",
            "",
            "未完全解开",
            "有成员被跳过或未落盘（排除规则）；原包保留",
            0,
        )
        .unwrap();
        db.log(
            "解压",
            "part.zip",
            "解压失败/part.zip",
            "移入解压失败",
            "未能完全解开：有成员被跳过",
            0,
        )
        .unwrap();
        // 成功包与任务级记录：不得出现在失败列表
        db.log(
            "解压",
            "ok.zip",
            "",
            "成功",
            "已完全解开；原包与分卷按 X-05 永久删除",
            0,
        )
        .unwrap();
        // 源包清理失败：X-05 单独阶段，不冒充坏包
        db.log(
            "删除",
            "ok2.zip",
            "",
            "失败",
            "源包清理未完成：删除时出错",
            0,
        )
        .unwrap();
        db.log("任务", "", "", "完成", "解压结束：成功 1 包", 0)
            .unwrap();
        dir
    }

    // 覆盖 U-10, X-06, S-07（本次解压失败项分页列表）：每个失败包/卷集一项，显示原路径、
    // 失败原因与隔离后位置；列表显示失败项总数；成功包与任务级记录不出现；源包清理失败
    // 单独标明阶段；无隔离记录时如实留空，不编造「已隔离」；切换工具不清空该列表。
    #[test]
    fn fail_list_groups_failed_events_per_volume_set() {
        let task = make_extract_task_db("fail-list-basic");
        let task_for_cleanup = task.clone();
        with_gui(move |app| {
            app.state.borrow_mut().extract_task = Some(task.clone());
            app.state.borrow_mut().extract_generation = 5;
            // 模拟 watcher 回传任务目录发现（watcher 自身另有专测）；代际须与当前一致。
            let sender = app.state.borrow().fail_sender.clone();
            sender.send(FailChannelMsg::TaskFound(task, 5)).unwrap();
            assert!(
                pump_until(app, || {
                    app.ui.get_fail_ready() && app.ui.get_fail_total() == 3
                }),
                "失败列表必须显示失败项总数 3：total={} ready={}",
                app.ui.get_fail_total(),
                app.ui.get_fail_ready()
            );
            let rows = app.ui.get_fail_rows();
            assert_eq!(rows.row_count(), 3, "一页应包含全部 3 个失败项");
            let first = rows.row_data(0).unwrap();
            assert_eq!(first.stage.as_str(), "解压失败");
            assert_eq!(first.source.as_str(), "bad.7z", "U-10：必须显示原路径");
            assert_eq!(
                first.reason.as_str(),
                "压缩包已损坏",
                "U-10：必须显示失败原因"
            );
            assert!(
                first.target.as_str().starts_with("解压失败/bad.7z")
                    && first.target.contains("2 个卷"),
                "整组卷集隔离必须显示隔离后位置与卷数：{}",
                first.target
            );
            let second = rows.row_data(1).unwrap();
            assert_eq!(second.stage.as_str(), "未完全解开");
            assert_eq!(second.source.as_str(), "part.zip");
            assert_eq!(second.target.as_str(), "解压失败/part.zip");
            let third = rows.row_data(2).unwrap();
            assert_eq!(
                third.stage.as_str(),
                "源包清理失败（X-05）",
                "源包清理失败必须单独标明阶段，不冒充坏包"
            );
            assert_eq!(third.source.as_str(), "ok2.zip");
            assert_eq!(
                third.target.as_str(),
                "",
                "无隔离记录必须如实留空，不得编造「已隔离」"
            );
            assert!(rows.row_data(0).is_some());
            assert!(
                !app.ui.get_fail_prev_enabled() && !app.ui.get_fail_next_enabled(),
                "只有一页时两个翻页按钮都必须禁用"
            );
            // 切换工具不清空该列表（U-10）
            app.ui.invoke_select_tool("git-tools".into());
            assert_eq!(app.ui.get_fail_total(), 3, "切换工具不得清空失败列表");
            assert_eq!(app.ui.get_fail_rows().row_count(), 3);
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&task_for_cleanup);
    }

    // 覆盖 U-10（附录 E：超过 300 项也不得截断，全部可翻页查询；翻页按钮只在对应页
    // 真实存在时启用；最后一项仍能翻页查到）
    #[test]
    fn fail_list_pages_beyond_single_page_limit() {
        let dir = temp_test_dir("fail-list-paging");
        {
            let db = Database::create(&dir).unwrap();
            for i in 0..305 {
                db.log(
                    "解压",
                    &format!("bad{i}.zip"),
                    "",
                    "失败",
                    &format!("损坏 {i}"),
                    0,
                )
                .unwrap();
            }
        }
        let dir_text = dir.display().to_string();
        with_gui(move |app| {
            app.state.borrow_mut().extract_task = Some(PathBuf::from(&dir_text));
            request_fail_page(&app.state, 0);
            assert!(
                pump_until(app, || {
                    app.ui.get_fail_total() == 305 && app.ui.get_fail_rows().row_count() == 100
                }),
                "首页应取 100 项且总数如实显示 305：total={}",
                app.ui.get_fail_total()
            );
            assert!(app.ui.get_fail_next_enabled(), "305 项必须有下一页");
            assert!(!app.ui.get_fail_prev_enabled(), "首页没有上一页");
            app.ui.invoke_fail_page(1);
            assert!(
                pump_until(app, || {
                    app.ui.get_fail_rows().row_count() == 100
                        && app.ui.get_fail_page_label().contains("第 2 页")
                }),
                "第二页应取 100 项：{}",
                app.ui.get_fail_page_label()
            );
            app.ui.invoke_fail_page(2);
            assert!(
                pump_until(app, || {
                    app.ui.get_fail_rows().row_count() == 100
                        && app.ui.get_fail_page_label().contains("第 3 页")
                }),
                "第三页应取 100 项：{}",
                app.ui.get_fail_page_label()
            );
            assert!(app.ui.get_fail_prev_enabled() && app.ui.get_fail_next_enabled());
            app.ui.invoke_fail_page(3);
            assert!(
                pump_until(app, || {
                    app.ui.get_fail_rows().row_count() == 5
                        && !app.ui.get_fail_next_enabled()
                        && app.ui.get_fail_page_label().contains("第 4 页")
                }),
                "最后一页 5 项且不再有下一页（按钮只在对应页真实存在时启用）：{}",
                app.ui.get_fail_page_label()
            );
            let rows = app.ui.get_fail_rows();
            let last = rows.row_data(4).unwrap();
            assert_eq!(
                last.source.as_str(),
                "bad304.zip",
                "附录 E：最后一项仍能翻页查到，不因 300 条截断"
            );
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 覆盖 U-10（失败列表只保留「本次」任务：启动下一次解压任务前清空上一任务的列表）
    #[test]
    fn reset_fail_list_clears_for_next_task() {
        let task = make_extract_task_db("fail-list-reset");
        let task_for_cleanup = task.clone();
        with_gui(move |app| {
            app.state.borrow_mut().extract_task = Some(task.clone());
            app.state.borrow_mut().extract_generation = 1;
            let sender = app.state.borrow().fail_sender.clone();
            sender.send(FailChannelMsg::TaskFound(task, 1)).unwrap();
            assert!(
                pump_until(app, || app.ui.get_fail_ready()
                    && app.ui.get_fail_total() == 3),
                "前置：失败列表已填充"
            );
            let mut s = app.state.borrow_mut();
            reset_fail_list(&app.ui, &mut s);
            drop(s);
            assert!(
                !app.ui.get_fail_ready() && app.ui.get_fail_total() == 0,
                "新任务开始必须清空上一任务的失败列表"
            );
            assert_eq!(app.ui.get_fail_rows().row_count(), 0);
            assert!(app.state.borrow().extract_task.is_none());
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&task_for_cleanup);
    }

    // 覆盖 U-10（失败列表的数据源发现：状态目录下出现新任务目录时 watcher 回传路径与代际）
    #[test]
    fn extract_task_watcher_discovers_new_task_dir() {
        let state_root = temp_test_dir("watch-extract-task");
        let tasks_root = state_root.join("tasks");
        std::fs::create_dir_all(tasks_root.join("old-task")).unwrap();
        let known = vec![tasks_root.join("old-task")];
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            watch_extract_task(&tasks_root, &known, 7, &sender, Duration::from_secs(10));
        });
        // 模拟引擎在任务开始的最初几步创建任务库目录
        std::thread::sleep(Duration::from_millis(150));
        std::fs::create_dir_all(state_root.join("tasks").join("new-task")).unwrap();
        match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(FailChannelMsg::TaskFound(path, generation)) => {
                assert!(
                    path.ends_with("new-task"),
                    "应发现快照之外的新任务目录：{path:?}"
                );
                assert_eq!(generation, 7, "发现消息必须携带发起时的清点代际");
            }
            other => panic!("watcher 应回传任务目录发现消息：{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&state_root);
    }

    // 覆盖 E-4（热键录制：Shift+数字行/标点在 US 布局上报为符号文本，必须反向映射
    // 回基础键，不得静默丢弃）——通过真实回调端到端断言组合键字符串。
    #[test]
    fn snap_record_key_maps_shift_symbol_to_base_key() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.set_snap_hotkey_draft("".into());
            ui.set_snap_recording(true);
            // Shift+7 经键盘布局映射上报为 "&"：录制结果必须是 Ctrl+Shift+7。
            ui.invoke_snap_record_key("&".into(), true, false, true, false);
            assert_eq!(
                ui.get_snap_hotkey_draft().as_str(),
                "Ctrl+Shift+7",
                "Shift+符号必须反向映射回基础键（E-4），不得静默丢键"
            );
            assert!(!ui.get_snap_recording(), "录制成功后必须退出录制态");
            assert_eq!(ui.get_snap_error().as_str(), "", "成功录制不得报错");
            // 对照：Shift+字母不受映射影响（布局上报已是大写字母）。
            ui.set_snap_recording(true);
            ui.invoke_snap_record_key("A".into(), true, false, true, false);
            assert_eq!(ui.get_snap_hotkey_draft().as_str(), "Ctrl+Shift+A");
            // 对照：不带 Shift 的符号不映射（保持既有行为，不扩大范围）。
            ui.set_snap_hotkey_draft("".into());
            ui.set_snap_recording(true);
            ui.invoke_snap_record_key("&".into(), true, false, false, false);
            assert_eq!(
                ui.get_snap_hotkey_draft().as_str(),
                "",
                "无 Shift 的符号主键仍不构成熟知组合，保持静默拒绝"
            );
            assert!(ui.get_snap_recording(), "未录制成功时保持录制态");
        })
        .unwrap();
    }

    // 覆盖 E-4（映射表逐对审计：US 布局 Shift 符号全部还原回基础键；
    // Shift+字母不映射；无 Shift 的符号不映射——与 worker 侧表必须逐对一致）。
    #[test]
    fn snap_shift_symbol_map_covers_every_pair() {
        let pairs = [
            ('!', '1'),
            ('@', '2'),
            ('#', '3'),
            ('$', '4'),
            ('%', '5'),
            ('^', '6'),
            ('&', '7'),
            ('*', '8'),
            ('(', '9'),
            (')', '0'),
            ('~', '`'),
            ('_', '-'),
            ('+', '='),
            ('{', '['),
            ('}', ']'),
            ('|', '\\'),
            (':', ';'),
            ('"', '\''),
            ('<', ','),
            ('>', '.'),
            ('?', '/'),
        ];
        assert_eq!(
            pairs.len(),
            SNAP_SHIFT_SYMBOL_BASE.len(),
            "审计表与实现表长度一致"
        );
        for (symbol, base) in pairs {
            assert_eq!(
                normalize_recorded_primary_key(&symbol.to_string(), true),
                Some(base),
                "Shift+{symbol} 必须映射回基础键 {base}"
            );
        }
        // Shift+字母：布局上报已是大写字母，不在映射表内，必须原样保留。
        for letter in ['A', 'Z', 'a', 'z', '7'] {
            assert_eq!(
                normalize_recorded_primary_key(&letter.to_string(), true),
                Some(letter),
                "Shift+{letter} 不得被符号映射改写"
            );
        }
        // 无 Shift 的符号不映射（不扩大既有录制范围）。
        for symbol in ['&', '~', '/', '\''] {
            assert_eq!(
                normalize_recorded_primary_key(&symbol.to_string(), false),
                Some(symbol),
                "无 Shift 的 {symbol} 必须原样返回"
            );
        }
        // 多字符文本（输入法组合串）不构成可录制主键。
        assert_eq!(normalize_recorded_primary_key("ab", true), None);
        assert_eq!(normalize_recorded_primary_key("", true), None);
    }

    /// 从指标行的「耗时 N.Ns」/「总耗时 N.Ns」提取耗时秒数（T-22 测试辅助）：
    /// `find("耗时")` 同时命中「总耗时」后半段，取其后首个 f64。
    fn elapsed_in_metrics(metrics: &str) -> Option<f64> {
        let tail = &metrics[metrics.find("耗时")? + "耗时".len()..];
        let end = tail.find('s')?;
        tail[..end].trim().parse().ok()
    }

    // 覆盖 T-22（转 Markdown：任务状态须含耗时——运行期间实时显示，收尾统计含总耗时）。
    // 事件驱动走真实事件泵：FILE_STARTED 后同一次 pump.run 的周期刷新即须带耗时，
    // DONE 收尾统计行须含从任务起算的总耗时。
    #[test]
    fn convert_metrics_show_elapsed_while_running_and_total_on_done() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.set_busy(true);
            app.state.borrow_mut().runtime = RuntimeMode::MarkdownConverter;
            let stale = Instant::now()
                .checked_sub(Duration::from_secs(2))
                .expect("系统运行时间不足 2 秒，无法构造旧时钟");
            app.state.borrow_mut().started = stale;
            app.pump
                .out
                .send(Event::Status("CONVERTER_STARTED|2".into()))
                .unwrap();
            app.pump
                .out
                .send(Event::Status(
                    "CONVERTER_FILE_STARTED|1|2|docs/a.pdf".into(),
                ))
                .unwrap();
            app.pump.run(ui);
            let running = ui.get_convert_metrics().to_string();
            assert!(
                running.contains("正在处理 docs/a.pdf"),
                "运行行必须保留当前文件：{running}"
            );
            assert!(
                running.contains("耗时"),
                "转换运行期间实时指标必须显示耗时（T-22）：{running}"
            );
            assert!(
                elapsed_in_metrics(&running).is_some_and(|secs| secs >= 2.0),
                "耗时必须从任务起算（不得显示 0.0s 新时钟）：{running}"
            );
            app.pump
                .out
                .send(Event::MdDone("CONVERTER_DONE|1|0|0|0|0|0".into()))
                .unwrap();
            app.pump.run(ui);
            let final_metrics = ui.get_convert_metrics().to_string();
            assert!(
                final_metrics.contains("成功 1"),
                "收尾统计保留既有计数：{final_metrics}"
            );
            assert!(
                final_metrics.contains("总耗时"),
                "转换收尾统计必须含总耗时（T-22）：{final_metrics}"
            );
            assert!(
                elapsed_in_metrics(&final_metrics).is_some_and(|secs| secs >= 2.0),
                "总耗时必须从任务起算到收尾：{final_metrics}"
            );
        })
        .unwrap();
    }

    // 覆盖 T-22 对称性：CONVERTER_FAIL 收尾必须清掉运行中文案——修复前 FAIL
    // 分支只重置 progress/note/status 不碰 metrics，失败后界面长期残留
    //「正在处理 X」或「正在扫描…」（与 DONE 分支的收尾统计不对称）。
    #[test]
    fn convert_fail_clears_running_metrics_text() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.set_busy(true);
            app.state.borrow_mut().runtime = RuntimeMode::MarkdownConverter;
            app.pump
                .out
                .send(Event::Status("CONVERTER_STARTED|2".into()))
                .unwrap();
            app.pump
                .out
                .send(Event::Status(
                    "CONVERTER_FILE_STARTED|1|2|docs/a.pdf".into(),
                ))
                .unwrap();
            app.pump.run(ui);
            let running = ui.get_convert_metrics().to_string();
            assert!(
                running.contains("正在处理"),
                "前置：运行中指标应含运行中文案，实测：{running}"
            );
            app.pump
                .out
                .send(Event::MdDone("CONVERTER_FAIL|组件不可用".into()))
                .unwrap();
            app.pump.run(ui);
            let final_metrics = ui.get_convert_metrics().to_string();
            assert!(
                !final_metrics.contains("正在处理") && !final_metrics.contains("正在扫描"),
                "失败收尾后不得残留运行中文案（正在处理/正在扫描），实测：{final_metrics}"
            );
            assert!(
                final_metrics.contains("总耗时"),
                "失败收尾统计应与 DONE 分支同口径含总耗时，实测：{final_metrics}"
            );
            // 扫描阶段失败：metrics 残留「正在扫描…」同样必须被收尾清除。
            ui.set_convert_metrics("正在扫描…".into());
            app.pump
                .out
                .send(Event::MdDone("CONVERTER_FAIL|扫描失败".into()))
                .unwrap();
            app.pump.run(ui);
            let scan_fail = ui.get_convert_metrics().to_string();
            assert!(
                !scan_fail.contains("正在扫描") && !scan_fail.contains("正在处理"),
                "扫描阶段失败的收尾同样不得残留运行中文案，实测：{scan_fail}"
            );
        })
        .unwrap();
    }

    // 覆盖 U-11 例外：转换任务运行中切页再切回，进度条不得被无条件重置为
    // 不确定态——修复前 on_select_tool 无条件 set_convert_progress(-1)，长任务
    // 期间离开再回来进度条到下一个 CONVERTER_FILE_STARTED 前保持不确定。
    #[test]
    fn select_tool_keeps_convert_progress_while_running() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.set_busy(true);
            app.state.borrow_mut().runtime = RuntimeMode::MarkdownConverter;
            app.pump
                .out
                .send(Event::Status(
                    "CONVERTER_FILE_STARTED|2|4|docs/a.pdf".into(),
                ))
                .unwrap();
            app.pump.run(ui);
            let before = ui.get_convert_progress();
            let note_before = ui.get_convert_progress_note().to_string();
            assert!(
                (before - 0.25).abs() < 0.01,
                "前置：注入 2/4 进度应约为 0.25，实测：{before}"
            );
            // 切走再切回：运行态保留实时进度。
            ui.invoke_select_tool("md-organizer".into());
            ui.invoke_select_tool("markdown-converter".into());
            assert!(
                (ui.get_convert_progress() - before).abs() < f32::EPSILON,
                "转换运行中切页再切回必须保留实时进度：{before} → {}",
                ui.get_convert_progress()
            );
            assert_eq!(
                ui.get_convert_progress_note().as_str(),
                note_before,
                "运行中切页不得清空进度注释"
            );
            // 非运行态切页：保持原重置口径（上次任务残留进度不得带到新会话）。
            ui.set_busy(false);
            app.state.borrow_mut().runtime = RuntimeMode::Organizer;
            ui.invoke_select_tool("md-organizer".into());
            ui.invoke_select_tool("markdown-converter".into());
            assert!(
                (ui.get_convert_progress() + 1.0).abs() < f32::EPSILON,
                "非运行态切页仍须重置进度为不确定态，实测：{}",
                ui.get_convert_progress()
            );
        })
        .unwrap();
    }

    // 覆盖 U-01（进度口径）：CONVERTER_FILE_STARTED 的大批次进度必须先做比例
    // 再落属性——修复前分子分母各自 u16 饱和后再相除，>65535 文件时
    // 70000/100000 被显示为 1.0（饱和失真）。
    #[test]
    fn convert_file_started_progress_ratio_survives_large_totals() {
        with_gui(|app| {
            let ui = &app.ui;
            app.pump
                .out
                .send(Event::Status(
                    "CONVERTER_FILE_STARTED|70001|100000|docs/big.pdf".into(),
                ))
                .unwrap();
            app.pump.run(ui);
            let progress = ui.get_convert_progress();
            assert!(
                (progress - 0.7).abs() < 0.01,
                "70000/100000 的进度应约为 0.7，实测：{progress}（u16 饱和会失真为 1.0）"
            );
            // 钳制口径不变：超出上限的比值仍不得超过 1.0。
            app.pump
                .out
                .send(Event::Status(
                    "CONVERTER_FILE_STARTED|200001|100000|docs/big.pdf".into(),
                ))
                .unwrap();
            app.pump.run(ui);
            assert!(
                ui.get_convert_progress() <= 1.0,
                "进度比值必须保持 0.0-1.0 钳制，实测：{}",
                ui.get_convert_progress()
            );
        })
        .unwrap();
    }

    // 覆盖 T-22/U-03（真实回调启动转换必须重置耗时时钟：不得沿用上次任务的旧时钟）。
    // 组件未就绪环境：启动后真实 worker 快速以 CONVERTER_FAIL 收尾，收尾前同步断言时钟。
    #[test]
    fn convert_start_resets_started_clock() {
        let dir = temp_test_dir("convert-clock");
        let input = dir.join("input");
        let output = dir.join("output");
        std::fs::create_dir_all(&input).unwrap();
        std::fs::create_dir_all(&output).unwrap();
        let input_text = input.display().to_string();
        let output_text = output.display().to_string();
        with_gui(move |app| {
            let ui = &app.ui;
            ui.invoke_select_tool("markdown-converter".into());
            ui.set_convert_ready(true);
            ui.set_convert_input_dir(input_text.clone().into());
            ui.set_convert_output_dir(output_text.clone().into());
            let stale = Instant::now()
                .checked_sub(Duration::from_secs(600))
                .expect("系统运行时间不足 600 秒，无法构造旧时钟");
            app.state.borrow_mut().started = stale;
            ui.invoke_convert_start();
            let generation = app.state.borrow().convert_preflight_generation;
            app.pump
                .out
                .send(Event::Status(format!(
                    "CONVERTER_PREFLIGHT|{generation}|1|已通过"
                )))
                .unwrap();
            app.pump.run(ui);
            assert!(ui.get_busy(), "组件就绪 + 目录有效时启动必须真正进入运行态");
            assert!(
                app.state.borrow().started > stale,
                "转换启动必须重置耗时时钟（T-22/U-03），不得沿用 600 秒前旧时钟"
            );
            // 收尾不等待真实组件探测（本机可能装有 Xberg，探测耗时不可控）：
            // 置取消标志让真实 worker 在首个检查点自行退出，同时用合成停止事件
            // 驱动与生产完全相同的 CONVERTER_DONE 收尾路径清 busy。
            if let Some(cancel) = app.state.borrow().convert_cancel.clone() {
                cancel.store(true, Ordering::Release);
            }
            app.pump
                .out
                .send(Event::MdDone("CONVERTER_DONE|0|0|0|0|0|1".into()))
                .unwrap();
            assert!(
                pump_until(app, || !ui.get_busy()),
                "停止事件必须走生产收尾路径清除 busy，测试不得悬挂"
            );
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 覆盖 T-09：输入与输出目录相同必须在进入转换 busy 前拒绝；修复前
    // start_markdown_conversion 先置 busy，再由后台 scan 返回失败。
    #[test]
    fn convert_same_input_output_rejected_before_busy() {
        let dir = temp_test_dir("convert-same-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.display().to_string();
        with_gui(move |app| {
            let ui = &app.ui;
            ui.invoke_select_tool("markdown-converter".into());
            ui.set_convert_ready(true);
            ui.set_convert_input_dir(text.clone().into());
            ui.set_convert_output_dir(text.into());
            ui.invoke_convert_start();
            assert!(pump_until(app, || ui
                .get_error_text()
                .contains("输入与输出目录不能相同")));
            assert!(!ui.get_busy(), "相同输入输出目录不得进入转换 busy 状态");
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
    // T-07/H-02：目录元数据检查必须留在后台预检，网络路径等慢 I/O 不得阻塞 GUI。
    #[test]
    fn convert_directory_validation_runs_in_background_preflight() {
        let root = temp_test_dir("convert-directory-preflight");
        let output = root.join("output");
        std::fs::create_dir_all(&output).unwrap();
        let input = root.join("missing-input");
        let input_text = input.display().to_string();
        let output_text = output.display().to_string();
        with_gui(move |app| {
            let ui = &app.ui;
            ui.invoke_select_tool("markdown-converter".into());
            ui.set_convert_ready(true);
            ui.set_convert_input_dir(input_text.into());
            ui.set_convert_output_dir(output_text.into());

            ui.invoke_convert_start();
            assert!(ui.get_convert_preparing(), "目录检查应先进入后台预检");
            assert!(!ui.get_busy(), "目录预检失败前不应开始转换");
            assert!(pump_until(app, || ui
                .get_error_text()
                .contains("无法访问输入目录")));
            assert!(!ui.get_busy(), "无效输入目录不得启动转换任务");
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    // XB-20：组件初始化或共享目录保存期间，不允许旧的 ready 快照启动转换。
    #[test]
    fn convert_start_is_gated_during_settings_operations() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("markdown-converter".into());
            ui.set_convert_ready(true);
            ui.set_convert_initializing(true);
            ui.invoke_convert_start();
            assert!(!ui.get_convert_preparing());
            assert!(ui.get_error_text().is_empty());

            ui.set_convert_initializing(false);
            ui.set_convert_runtime_saving(true);
            ui.invoke_convert_start();
            assert!(!ui.get_convert_preparing());
            assert!(ui.get_error_text().is_empty());
        })
        .unwrap();
    }

    // 覆盖 T-29：超出 Instant 可表示范围的正整数必须如实报错，不钳制后
    // 进入后台；修复前 u64::MAX 会在 Deadline::new 的 Instant 加法处 panic，
    // 转换线程不回传终态而使界面永久 busy。
    #[test]
    fn convert_timeout_overflow_rejected_before_busy() {
        let dir = temp_test_dir("convert-timeout-overflow");
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.display().to_string();
        with_gui(move |app| {
            let ui = &app.ui;
            ui.invoke_select_tool("markdown-converter".into());
            ui.set_convert_ready(true);
            ui.set_convert_input_dir(text.clone().into());
            ui.set_convert_output_dir(text.into());
            ui.set_convert_timeout_secs(u64::MAX.to_string().into());
            ui.invoke_convert_start();
            assert!(!ui.get_busy(), "不可表示的超时不得进入转换 busy 状态");
            assert!(
                ui.get_error_text().contains("超出系统可表示范围"),
                "应如实报告超时范围错误：{}",
                ui.get_error_text()
            );
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 覆盖 C-1（snap 管道读响应必须有 deadline：服务接受连接后挂起不响应时，
    // 请求必须按超时错误返回，监督线程不得永久阻塞导致页面按钮永久禁用，O-11/O-30）。
    // 用注入的管道名与 300ms 小超时模拟服务挂起；生产默认 10 秒路径同一实现。
    #[cfg(windows)]
    #[test]
    fn snap_pipe_read_times_out_when_service_hangs() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
        use windows_sys::Win32::System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
        };

        // 测试专用管道名（按进程隔离）：不得占用真实服务管道名，避免与用户
        // 正在运行的截图服务互抢实例或向其发送请求。
        let name = format!(r"\\.\pipe\jchtools-gui-test-hang-{}", std::process::id());
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: wide 以 NUL 结尾且调用期间存活；security 属性传空表示默认安全描述符。
        let pipe = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                std::ptr::null(),
            )
        };
        assert!(pipe != INVALID_HANDLE_VALUE, "创建测试管道服务端失败");
        // 服务端线程：完成连接后挂起（永不响应），模拟服务卡死。
        #[derive(Clone, Copy)]
        struct SendPipe(windows_sys::Win32::Foundation::HANDLE);
        // SAFETY: 句柄经包装后仅移入服务端线程独占使用，不再被创建线程触碰；
        // Win32 句柄本身可跨线程使用，Send 仅解除裸指针的编译期限制。
        unsafe impl Send for SendPipe {}
        fn serve_hung_pipe(pipe: SendPipe) {
            let SendPipe(pipe) = pipe;
            // SAFETY: pipe 为本线程独占的有效句柄；同步模式不使用 overlapped，
            // 客户端先连上时返回 FALSE + ERROR_PIPE_CONNECTED(535)，同为已连接。
            unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) };
            // 挂起远超测试墙钟：模拟服务进程活着但永不响应（真实挂起场景）。
            // 线程随测试进程退出终止，用例不 join，避免拖慢测试。
            std::thread::sleep(Duration::from_secs(30));
            // SAFETY: 关闭本线程独占的句柄，无未完成 I/O 需要等待。
            unsafe { CloseHandle(pipe) };
        }
        let pipe = SendPipe(pipe);
        // 函数边界传递整个 SendPipe：闭包按整值捕获（Send），避免精确捕获裸句柄字段。
        // 服务端线程随测试进程退出终止（挂起即测试目的，不 join）。
        std::thread::spawn(move || serve_hung_pipe(pipe));
        let started = Instant::now();
        let result = snap_pipe_request_on(
            &name,
            &serde_json::json!({"command": "ping"}),
            Duration::from_millis(300),
        );
        let elapsed = started.elapsed();
        let message = result.expect_err("服务挂起时读响应必须以错误返回，不得永久阻塞");
        assert!(
            message.contains("超时"),
            "错误必须说明是响应超时（C-1）：{message}"
        );
        assert!(
            elapsed >= Duration::from_millis(250),
            "必须等到 deadline 才返回（提前失败说明走的是其他错误路径）：{elapsed:.1?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "必须在超时 + 余量内返回（C-1 修复前此处永久挂起）：{elapsed:.1?}"
        );
    }

    // 覆盖 H-02 / G-13（暂停按钮：整理/解压任务经真实 Control 暂停与继续；
    // Git、转 Markdown 与无任务时如实提示不支持，不翻转 paused 显示）
    #[test]
    fn pause_button_toggles_organizer_and_reports_unsupported_runtimes() {
        with_gui(|app| {
            let ui = &app.ui;
            // 无任务（control=None）：不得伪装暂停成功
            ui.invoke_pause_task();
            assert!(
                ui.get_status().contains("不支持暂停"),
                "无任务时暂停必须如实提示"
            );
            // 目录整理任务：真实 Control 上暂停/继续切换（H-02 处理状态可见）
            let control = Arc::new(Control::default());
            app.state.borrow_mut().control = Some(control.clone());
            app.state.borrow_mut().runtime = RuntimeMode::Organizer;
            ui.invoke_pause_task();
            assert!(ui.get_paused(), "暂停后界面必须呈现已暂停");
            assert!(control.is_paused(), "暂停必须写入真实控制通道");
            assert!(ui.get_status().contains("已请求暂停"));
            ui.invoke_pause_task();
            assert!(!ui.get_paused(), "再次点击恢复处理");
            assert!(!control.is_paused());
            assert_eq!(ui.get_status().as_str(), "继续处理");
            // Git / 转 Markdown 运行口径不接入暂停检查点（G-13）：如实提示且不动 paused
            for unsupported in [RuntimeMode::Git, RuntimeMode::MarkdownConverter] {
                app.state.borrow_mut().runtime = unsupported;
                ui.invoke_pause_task();
                assert!(ui.get_status().contains("不支持暂停"));
                assert!(!ui.get_paused(), "不支持的工具不得翻转 paused 显示");
            }
        })
        .unwrap();
    }

    // 覆盖 H-02（取消入口可用：control 命中即取消；无共享控制器的转换、组件初始化
    // 与截图初始化各自回落独立取消原子量，不谎报取消而任务继续）
    #[test]
    fn cancel_button_cancels_control_and_falls_back_to_dedicated_atoms() {
        with_gui(|app| {
            let ui = &app.ui;
            let control = Arc::new(Control::default());
            app.state.borrow_mut().control = Some(control.clone());
            ui.invoke_cancel_task();
            assert!(control.is_cancelled(), "取消必须写入真实控制通道");
            assert!(ui.get_status().contains("正在取消"));

            let convert_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            app.state.borrow_mut().control = None;
            app.state.borrow_mut().convert_cancel = Some(convert_cancel.clone());
            ui.invoke_cancel_task();
            assert!(
                convert_cancel.load(Ordering::Acquire),
                "转 Markdown 任务必须经独立原子量取消"
            );

            let init_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            app.state.borrow_mut().convert_cancel = None;
            app.state.borrow_mut().convert_init_cancel = Some(init_cancel.clone());
            ui.invoke_cancel_task();
            assert!(
                init_cancel.load(Ordering::Acquire),
                "转 Markdown 组件初始化必须可取消"
            );

            let snap_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            app.state.borrow_mut().convert_init_cancel = None;
            app.state.borrow_mut().snap_init_cancel = Some(snap_cancel.clone());
            ui.invoke_cancel_task();
            assert!(
                snap_cancel.load(Ordering::Acquire),
                "截图 OCR 初始化取消入口必须可用（O-06）"
            );
            let control = Arc::new(Control::default());
            let convert_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let convert_init_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let snap_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            {
                let mut state = app.state.borrow_mut();
                state.control = Some(control.clone());
                state.convert_cancel = Some(convert_cancel.clone());
                state.convert_init_cancel = Some(convert_init_cancel.clone());
                state.snap_init_cancel = Some(snap_cancel.clone());
            }
            ui.set_busy(true);
            ui.set_convert_preparing(true);
            ui.set_convert_initializing(true);
            ui.set_snap_initializing(true);
            ui.invoke_cancel_task();
            assert!(control.is_cancelled());
            assert!(convert_cancel.load(Ordering::Acquire));
            assert!(convert_init_cancel.load(Ordering::Acquire));
            assert!(snap_cancel.load(Ordering::Acquire));

            let control = Arc::new(Control::default());
            let convert_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let convert_init_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let snap_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            {
                let mut state = app.state.borrow_mut();
                state.control = Some(control.clone());
                state.convert_cancel = Some(convert_cancel.clone());
                state.convert_init_cancel = Some(convert_init_cancel.clone());
                state.snap_init_cancel = Some(snap_cancel.clone());
            }
            ui.invoke_confirmed(3);
            assert!(control.is_cancelled());
            assert!(convert_cancel.load(Ordering::Acquire));
            assert!(convert_init_cancel.load(Ordering::Acquire));
            assert!(snap_cancel.load(Ordering::Acquire));
            ui.set_busy(false);
            ui.set_convert_preparing(false);
            ui.set_convert_initializing(false);
            ui.set_snap_initializing(false);
            let mut state = app.state.borrow_mut();
            state.control = None;
            state.convert_cancel = None;
            state.convert_init_cancel = None;
            state.snap_init_cancel = None;
        })
        .unwrap();
    }

    // 覆盖 G-15（Git 停止：真正取消共享控制器、不再处理新文件；跨工具停止共享
    // 任务与转 Markdown 回落分支的文案如实区分）
    #[test]
    fn git_stop_cancels_control_and_reports_per_runtime() {
        with_gui(|app| {
            let ui = &app.ui;
            let control = Arc::new(Control::default());
            app.state.borrow_mut().control = Some(control.clone());
            app.state.borrow_mut().runtime = RuntimeMode::Git;
            ui.invoke_git_stop();
            assert!(control.is_cancelled(), "停止必须真正取消 Git 任务（G-15）");
            assert!(ui.get_status().contains("已成功推送的文件保持成功"));

            // 其他工具的共享任务（整理/解压/MD）：通用取消口径
            app.state.borrow_mut().runtime = RuntimeMode::Organizer;
            let shared = Arc::new(Control::default());
            app.state.borrow_mut().control = Some(shared.clone());
            ui.invoke_git_stop();
            assert!(shared.is_cancelled());
            assert!(ui.get_status().contains("正在取消；当前操作完成后停止"));

            // control 为 None：回落取消转 Markdown 任务（转换页「停止任务」同源）
            let convert_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            app.state.borrow_mut().control = None;
            app.state.borrow_mut().convert_cancel = Some(convert_cancel.clone());
            ui.invoke_git_stop();
            assert!(convert_cancel.load(Ordering::Acquire));
            assert!(ui.get_status().contains("正在停止转 Markdown"));
        })
        .unwrap();
    }

    // 覆盖 T-23（停止转 Markdown：运行/启动前检查/初始化三种在途阶段文案如实；
    // 跨工具停止共享任务时按通用取消口径，不改写转换页状态行）
    #[test]
    fn convert_stop_reports_each_phase_and_cancels_shared_task() {
        with_gui(|app| {
            let ui = &app.ui;
            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            app.state.borrow_mut().convert_cancel = Some(cancel.clone());
            ui.invoke_convert_stop();
            assert!(cancel.load(Ordering::Acquire), "停止必须写入转换取消原子量");
            assert_eq!(
                ui.get_convert_status().as_str(),
                "正在停止；正在取消当前文件"
            );
            assert!(ui.get_status().contains("正在停止转 Markdown"));

            let prep_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            app.state.borrow_mut().convert_cancel = Some(prep_cancel.clone());
            ui.set_convert_preparing(true);
            ui.invoke_convert_stop();
            assert!(prep_cancel.load(Ordering::Acquire));
            assert!(ui.get_convert_status().contains("正在取消启动前检查"));

            ui.set_convert_preparing(false);
            ui.set_convert_initializing(true);
            let init_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            app.state.borrow_mut().convert_cancel = None;
            app.state.borrow_mut().convert_init_cancel = Some(init_cancel.clone());
            ui.invoke_convert_stop();
            assert!(init_cancel.load(Ordering::Acquire));
            assert!(ui.get_convert_status().contains("正在取消初始化"));

            // 其他工具共享任务运行中：control 命中即取消，文案按通用口径且不改写转换状态行
            ui.set_convert_initializing(false);
            let control = Arc::new(Control::default());
            app.state.borrow_mut().control = Some(control.clone());
            ui.invoke_convert_stop();
            assert!(control.is_cancelled());
            assert_eq!(
                ui.get_status().as_str(),
                "正在取消；当前操作完成后停止，不会继续后续操作"
            );
            assert!(
                ui.get_convert_status().contains("正在取消初始化"),
                "停止共享任务不得改写转换页状态行"
            );
        })
        .unwrap();
    }

    // 「打开任务目录」按钮无任务时的安全无操作：不打开外部程序、不报错
    //（入口只在 has-task 时渲染；无独立合同行，行为边界与 C-11 计划页一致）
    #[test]
    fn open_report_without_task_is_a_safe_noop() {
        with_gui(|app| {
            let before = app.ui.get_status().to_string();
            app.ui.invoke_open_report();
            assert_eq!(app.ui.get_status().as_str(), before);
            assert!(app.ui.get_error_text().is_empty());
        })
        .unwrap();
    }

    // 覆盖 C-11（计划页经真实回调翻页与类型筛选：按钮只在对应页真实存在时可用；
    // 末页再点下一页保持原页；切换类型筛选回到第一页且只显示所选类型）
    #[test]
    fn plan_pagination_and_kind_filter_via_real_callbacks() {
        use crate::config::DeleteMode;
        use crate::model::{Action, ActionKind};
        let dir = temp_test_dir("plan-paging");
        {
            let db = Database::create(&dir).unwrap();
            for i in 0..120 {
                db.add_action(&Action {
                    id: 0,
                    kind: ActionKind::Delete,
                    source: format!("del-{i}.txt"),
                    target: None,
                    reason: String::new(),
                    expected: None,
                    keeper: None,
                    hash: None,
                    mode: DeleteMode::Keep,
                    selected: true,
                    state: "pending".into(),
                })
                .unwrap();
            }
            for i in 0..10 {
                db.add_action(&Action {
                    id: 0,
                    kind: ActionKind::Move,
                    source: format!("mv-{i}.txt"),
                    target: None,
                    reason: String::new(),
                    expected: None,
                    keeper: None,
                    hash: None,
                    mode: DeleteMode::Keep,
                    selected: true,
                    state: "pending".into(),
                })
                .unwrap();
            }
        }
        let cleanup = dir.clone();
        with_gui(move |app| {
            let ui = &app.ui;
            app.state.borrow_mut().task = Some(dir.clone());
            app.state.borrow_mut().page = 0;
            app.state.borrow_mut().page_starts = vec![0];
            ui.invoke_filter_plan("".into());
            assert!(
                pump_until(app, || ui.get_plans().row_count() == 100),
                "全部筛选的第一页应显示 100 条：{}",
                ui.get_plans().row_count()
            );
            assert!(!ui.get_plan_prev_enabled() && ui.get_plan_next_enabled());
            assert!(ui.get_plan_page_label().contains("第 1 页"));

            ui.invoke_plan_page(1);
            assert!(
                pump_until(app, || ui.get_plan_page_label().contains("第 2 页")),
                "下一页应提交第二页：{}",
                ui.get_plan_page_label()
            );
            assert_eq!(ui.get_plans().row_count(), 30, "第二页应只剩 30 条");
            assert!(ui.get_plan_prev_enabled() && !ui.get_plan_next_enabled());

            // 末页再点下一页：没有可发起的请求，页面保持第二页
            ui.invoke_plan_page(1);
            app.pump.run(ui);
            assert!(ui.get_plan_page_label().contains("第 2 页"));
            assert_eq!(app.state.borrow().page, 1);

            ui.invoke_plan_page(-1);
            assert!(
                pump_until(app, || ui.get_plan_page_label().contains("第 1 页")),
                "上一页应回到第一页"
            );
            assert_eq!(ui.get_plans().row_count(), 100);

            // 类型筛选：回到第一页且只显示移动项（C-11）
            ui.invoke_filter_plan("move".into());
            assert!(
                pump_until(app, || ui.get_plans().row_count() == 10),
                "筛选 move 后应只显示 10 条：{}",
                ui.get_plans().row_count()
            );
            assert!(ui.get_plan_page_label().contains("第 1 页"));
            assert!(!ui.get_plan_prev_enabled() && !ui.get_plan_next_enabled());
            let rows = ui.get_plans();
            for i in 0..rows.row_count() {
                assert_eq!(
                    rows.row_data(i).unwrap().kind.as_str(),
                    "移动/重命名",
                    "筛选后不得混入其他类型"
                );
            }
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&cleanup);
    }

    // 覆盖 R-01（规则面板 choice 行经真实回调取值：索引映射 choices 值并同步
    // 行内下拉；越界索引安全忽略，不改动当前值）
    #[test]
    fn rule_choice_updates_value_and_ignores_out_of_range_index() {
        use crate::config::DeleteMode;
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_section(3); // 安全与性能：全局删除方式在此分区
                                         // choice 行的取值以配置与行内下拉索引为口径（row.value 不随选择就地改写）
            let choice_index = |ui: &AppWindow| -> i32 {
                let rules = ui.get_rules();
                (0..rules.row_count())
                    .find_map(|i| {
                        let row = rules.row_data(i).unwrap();
                        (row.key.as_str() == "global_delete").then_some(row.index)
                    })
                    .unwrap_or(-1)
            };
            assert_eq!(
                app.state.borrow().config.global_delete,
                DeleteMode::Permanent,
                "默认值应为永久删除"
            );
            assert_eq!(choice_index(ui), 1, "行内下拉应停在默认选项");
            ui.invoke_rule_choice("global_delete".into(), 0);
            assert_eq!(
                app.state.borrow().config.global_delete,
                DeleteMode::Keep,
                "选择索引 0 应切换为保留"
            );
            assert_eq!(choice_index(ui), 0, "行内下拉应同步到所选选项");
            ui.invoke_rule_choice("global_delete".into(), 7);
            assert_eq!(
                app.state.borrow().config.global_delete,
                DeleteMode::Keep,
                "越界索引必须安全忽略"
            );
            assert_eq!(choice_index(ui), 0, "越界索引不得改动行内下拉");
            ui.invoke_rule_choice("global_delete".into(), 1);
            assert_eq!(
                app.state.borrow().config.global_delete,
                DeleteMode::Permanent
            );
            assert_eq!(choice_index(ui), 1);
        })
        .unwrap();
    }

    // 覆盖 O-06（初始化不重入、取消状态如实提示）与管道按钮的即时反馈：
    // 命令按钮点击即置「请求在途」，服务未连接时命令侧安全无操作（挂起兜底另有专测）
    #[test]
    fn snap_buttons_immediate_feedback_and_initialize_reentry_gating() {
        with_gui(|app| {
            let ui = &app.ui;
            // 已就绪：初始化按钮不得重新进入初始化
            ui.set_snap_ready(true);
            ui.invoke_snap_initialize();
            assert!(!ui.get_snap_initializing());
            assert_eq!(
                app.state.borrow().snap_generation,
                0,
                "已就绪时不得发起新初始化"
            );
            // 初始化在途：同样不重入
            ui.set_snap_ready(false);
            ui.set_snap_initializing(true);
            ui.invoke_snap_initialize();
            assert_eq!(
                app.state.borrow().snap_generation,
                0,
                "初始化在途时不得重入"
            );
            // 取消按钮：状态行如实提示（O-06 取消语义）
            ui.invoke_snap_cancel_initialize();
            assert!(ui.get_snap_asset_status().contains("正在取消初始化"));
            // 管道按钮：点击即置请求在途标志并给出状态反馈
            ui.invoke_snap_connect();
            assert!(ui.get_snap_request_pending());
            assert!(ui.get_snap_service_status().contains("正在连接"));
            ui.set_snap_request_pending(false);
            ui.invoke_snap_capture();
            assert!(ui.get_snap_request_pending());
            ui.set_snap_request_pending(false);
            ui.invoke_snap_retry_load();
            assert!(ui.get_snap_request_pending());
            ui.set_snap_request_pending(false);
            ui.invoke_snap_save_settings();
            assert!(ui.get_snap_request_pending());
            // 自启动草稿开关：真实回调写草稿值（生效另经 save-settings 命令）
            ui.invoke_snap_draft_autostart(true);
            assert!(ui.get_snap_autostart_draft());
            ui.invoke_snap_draft_autostart(false);
            assert!(!ui.get_snap_autostart_draft());
        })
        .unwrap();
    }

    // 覆盖 T-06 / XB-19（共享 Xberg 目录：编辑即失效就绪与已确认；空目录直接拒绝；
    // 无效目录后台校验失败回传后不得标记已确认）
    #[test]
    fn convert_runtime_edit_and_use_directory_revalidation_flow() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.set_convert_ready(true);
            ui.set_convert_runtime_confirmed(true);
            ui.invoke_convert_runtime_edited();
            assert!(!ui.get_convert_ready(), "目录编辑后必须失效就绪");
            assert!(!ui.get_convert_runtime_confirmed());
            assert!(ui.get_convert_status().contains("重新校验"));

            // 空目录：使用前校验直接拒绝并给出指引
            ui.set_convert_runtime_dir("".into());
            ui.invoke_convert_use_runtime();
            assert!(!ui.get_convert_runtime_saving());
            assert!(ui.get_convert_status().contains("请先选择 Xberg 运行目录"));

            // 无效目录：隔离设置状态目录后走真实保存线程，失败事件回传不标记已确认
            let _lock = crate::asset_util::test_env::env_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let root = tempfile::tempdir().unwrap();
            struct Restore(Option<std::ffi::OsString>);
            impl Drop for Restore {
                fn drop(&mut self) {
                    match &self.0 {
                        Some(value) => std::env::set_var("JCHTOOLS_TEST_STATE_DIR", value.clone()),
                        None => std::env::remove_var("JCHTOOLS_TEST_STATE_DIR"),
                    }
                }
            }
            let _restore = Restore(std::env::var_os("JCHTOOLS_TEST_STATE_DIR"));
            std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.path());
            ui.set_convert_runtime_dir(root.path().display().to_string().into());
            ui.invoke_convert_use_runtime();
            assert!(ui.get_convert_runtime_saving(), "点击使用后进入保存校验中");
            assert!(
                pump_until(app, || !ui.get_convert_runtime_saving()),
                "无效目录校验必须以失败收尾"
            );
            assert!(
                !ui.get_convert_runtime_confirmed(),
                "校验失败不得标记已确认"
            );
            assert!(!ui.get_convert_ready());
            assert!(ui.get_convert_status().contains("Xberg 运行目录不可用"));
        })
        .unwrap();
    }

    // 覆盖 XB-20（设置页操作门禁：任务、启动前检查等在途时不接受新操作；
    // 未知操作以设置页状态行报错，不静默）
    #[test]
    fn settings_actions_are_gated_while_busy_and_reject_unknown_action() {
        with_gui(|app| {
            let ui = &app.ui;
            assert_eq!(ui.get_settings_status().as_str(), "正在读取配置…");
            ui.set_busy(true);
            ui.invoke_settings_action("custom".into());
            assert!(
                !ui.get_convert_runtime_saving() && !ui.get_convert_initializing(),
                "任务运行中设置操作必须被门禁"
            );
            assert_eq!(ui.get_settings_status().as_str(), "正在读取配置…");
            ui.set_busy(false);
            ui.set_convert_preparing(true);
            ui.invoke_settings_action("custom".into());
            assert!(!ui.get_convert_runtime_saving(), "预检在途同样门禁");
            assert_eq!(ui.get_settings_status().as_str(), "正在读取配置…");
            ui.set_convert_preparing(false);
            ui.invoke_settings_action("bogus".into());
            assert!(
                pump_until(app, || ui.get_settings_status().contains("未知设置操作")),
                "未知操作必须报错：{}",
                ui.get_settings_status()
            );
            assert!(
                !ui.get_convert_runtime_saving(),
                "失败收尾必须复位保存中标志"
            );
        })
        .unwrap();
    }

    // 覆盖 U-11 / M-01（MD 子页页签经真实回调切换；无效页号安全忽略）
    #[test]
    fn md_subpage_tabs_switch_via_real_callback() {
        with_gui(|app| {
            app.ui.invoke_md_select_subpage(1);
            assert_eq!(app.ui.get_md_subpage(), 1, "页签应切到拆分子页");
            app.ui.invoke_md_select_subpage(0);
            assert_eq!(app.ui.get_md_subpage(), 0, "页签应切回合并子页");
            app.ui.invoke_md_select_subpage(2);
            assert_eq!(app.ui.get_md_subpage(), 0, "无效页号必须安全忽略");
        })
        .unwrap();
    }
}
