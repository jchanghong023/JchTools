//! GUI 组装层：Slint 界面的状态、回调整体在此实现；`main.rs` 只是薄壳入口。
//! 同步回调集中在 `wire_sync`，便于无头测试装配后直接断言界面状态。
use crate::registry;
use anyhow::Result;
use jchtools_acp_api::acp_api::{
    runtime as acp_runtime, settings as acp_settings, ServiceConfig, ServiceStatus,
};
use jchtools_assets::snap_ocr_assets;
use jchtools_directory_organizer::engine as organizer_engine;
use jchtools_file_core::{
    config::{Config, DeleteChoice},
    control::{self, Context, Control, Event, PlanSnapshot},
    db::Database,
    model::{bytes, ActionKind, Summary},
};
use jchtools_foundation::xberg_settings;
use jchtools_git_tools::git_tools::{self, GitShared, BACKOFF_UNIT};
use jchtools_markdown::{
    markdown::{self, FormatGroup},
    markdown_assets,
};
use jchtools_md_tools::md_tools;
use jchtools_recursive_extract::engine as extract_engine;
use jchtools_ui::{AppWindow, FailRow, PlanRow, RuleRow, ToolRow};
use serde::Deserialize;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
#[cfg(windows)]
use std::io::BufReader;
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

#[cfg(all(test, windows))]
#[path = "../../../tests/fixtures/unresolvable_output.rs"]
mod review_unresolvable_output;

// 拆分自本文件的子模块：装配层按内聚边界分置，彼此经 `use super::*` 互访。
mod acp;
mod center_win;
mod codeagent;
mod convert;
mod fail_list;
mod md_git;
mod plan;
mod rules;
mod run;
mod settings;
mod snap_ocr;
mod task_flow;
mod ui_pump;
mod ui_pump_events;
mod ui_state;
mod wiring;
use acp::{
    apply_acp_messages, can_request_acp_stop, request_acp_action, start_acp_observer,
    wire_acp_http, AcpCommand, AcpMessage, AcpObserver, AcpRequest,
};
use center_win::{center_window, pointer_position, system_dark};
use codeagent::{
    apply_codeagent_messages, request_codeagent, wire_codeagent, CodeAgentCommand,
    CodeAgentObserver,
};
use convert::{launch_markdown_conversion, start_markdown_readiness, wire_markdown_converter};
use fail_list::{
    request_fail_page, reset_fail_list, watch_extract_task, FailChannelMsg,
    EXTRACT_TASK_WATCH_TIMEOUT, FAIL_REFRESH_MIN,
};
use md_git::{confirm_md_override, wire_md_git};
use plan::{
    apply_plan_readiness, count_selected_pending, invalid_number_error, load_plan_state,
    patch_plan_row, plan_page_event_accepted, plan_row_state, retain_invalid_number, PlanQuery,
};
use rules::{changed, hint_for, parse_capacity, patch_rule_row, refresh, sync_rules, RuleSpec};
use run::run_gui;
use settings::{settings_payload, wire_settings};
use snap_ocr::{snap_pipe_request, start_snap_readiness, wire_snap_ocr};
use task_flow::{
    extract_confirm_text, normalize_output_name, parse_size_bytes, pick_directory, show_error,
    start_extract, start_task,
};
use ui_pump::{apply_summary, UiPump};
use ui_state::{
    apply_idle_directory_messages, apply_theme, invalidate, invalidate_idle_directory,
    log_panel_text, log_panel_visible, push_event_log, refresh_scope_notice, reload_after_failed,
    reset_tool_list, sync_ready_after_directory,
};
use wiring::{wire_sync, wire_task_lifecycle};

#[cfg(test)]
use self::{
    acp::*, codeagent::*, fail_list::*, md_git::*, plan::*, run::*, settings::*, snap_ocr::*,
    task_flow::*,
};

// 无头 GUI 测试：共享装配在 gui_tests，主题用例分置（各文件内保留原 lint 降级口径）。
#[cfg(test)]
mod gui_tests;
#[cfg(test)]
mod tests_acp;
#[cfg(test)]
mod tests_acp_control;
#[cfg(test)]
mod tests_converter;
#[cfg(test)]
mod tests_extract_fail;
#[cfg(test)]
mod tests_md_git;
#[cfg(test)]
mod tests_misc;
#[cfg(test)]
mod tests_plan_pages;
#[cfg(test)]
mod tests_review_findings;
#[cfg(test)]
mod tests_review_remediation;
#[cfg(test)]
mod tests_settings;
#[cfg(test)]
mod tests_snap;
#[cfg(test)]
mod tests_tools_navigation;

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
    CodeAgent,
}

impl Tool {
    fn sections(self) -> &'static [&'static str] {
        match self {
            Tool::Extract => &["解压", "安全与性能"],
            Tool::Organizer => &["去重", "归类", "清理", "安全与性能"],
            // R-01：MD 整理与 Git 工具不设规则面板，分区集合为空。
            Tool::Md
            | Tool::Git
            | Tool::MarkdownConverter
            | Tool::SnapOcr
            | Tool::AcpHttp
            | Tool::CodeAgent => &[],
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
            "codeagent" => Some(Tool::CodeAgent),
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

#[derive(Default)]
struct ConvertCounts {
    total: Option<usize>,
    completed: usize,
    success: usize,
    partial: usize,
    failed: usize,
    skipped: usize,
}

impl ConvertCounts {
    fn progress(&self) -> f32 {
        let Some(total) = self.total else {
            return -1.0;
        };
        if total == 0 {
            return 1.0;
        }
        // 显示用途按完整计数求比例，避免大批次经窄整数饱和后失真。
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let progress = (self.completed as f64 / total as f64).clamp(0.0, 1.0) as f32;
        progress
    }

    fn note(&self) -> String {
        self.total.map_or_else(
            || "正在扫描".to_string(),
            |total| format!("{} / {total}", self.completed),
        )
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
struct ReviewRemediationHooks {
    git: Option<Arc<dyn Fn() + Send + Sync>>,
    readiness: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
    preflight: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
    conversion: Option<Arc<dyn Fn() + Send + Sync>>,
    // 时序观察只阻塞真实边界，不替换文件系统或 ACP runtime 返回值。
    path_preflight: Option<Arc<dyn Fn() + Send + Sync>>,
    acp_before_ensure: Option<Arc<dyn Fn() + Send + Sync>>,
    acp_completed: Option<Arc<dyn Fn(AcpOperation) + Send + Sync>>,
}

struct IdleDirectoryRequest {
    generation: u64,
    directory: String,
    tool: Tool,
}

struct IdleDirectoryMessage {
    request: IdleDirectoryRequest,
    exists: bool,
}

/// 每个窗口最多一个目录 probe worker；编辑只覆盖尚未开始的最新请求。
#[derive(Default)]
struct IdleDirectoryQueue {
    pending: Option<IdleDirectoryRequest>,
    running: bool,
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
    /// 当前选择批次重算后的页面尚未成功呈现；独立快照不能越过此门禁。
    plan_page_pending: bool,
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
    /// 由转换事件推进的实际完成计数，不从显示文案反向解析。
    convert_counts: ConvertCounts,
    /// 转 Markdown 组件检查代际；旧检查结果不得覆盖新初始化/检查状态。
    convert_readiness_generation: u64,
    /// 当前代就绪结果在忙碌时被丢弃；任何工具或设置操作收尾后均须补查真值。
    convert_readiness_missed: bool,
    /// 设置操作代际：启动时的旧配置读取不得覆盖之后保存的配置或错误。
    settings_generation: u64,
    #[cfg(test)]
    settings_download: Option<Arc<SettingsDownload>>,
    #[cfg(test)]
    review_remediation: ReviewRemediationHooks,
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
    codeagent_observer: Option<CodeAgentObserver>,
    /// 测试注入：覆盖任务状态目录；生产路径为 None，仍走 organizer_engine::prepare/apply。
    engine_overrides: Option<EngineTestOverrides>,
    /// 解压确认清点的请求代际：迟到的低代际清点事件不得刷新文案或解除门禁（X-02）。
    extract_generation: u64,
    /// 计划页加载代际（跨线程）：丢弃晚到的旧 filter/page 结果。
    plan_load: Arc<PlanLoadSync>,
    /// 目录/工具变化的反馈意图保留到最新有效快照落地，不随被顶替的请求丢失。
    readiness_status_pending: Cell<bool>,
    idle_directory_generation: Cell<u64>,
    idle_directory_queue: Arc<std::sync::Mutex<IdleDirectoryQueue>>,
    idle_directory_sender: mpsc::Sender<IdleDirectoryMessage>,
    idle_directory_receiver: RefCell<mpsc::Receiver<IdleDirectoryMessage>>,
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
    channel: control::EventSender,
    ui_thread: std::thread::ThreadId,
}

impl EventSender {
    fn new(channel: control::EventSender) -> Self {
        Self {
            channel,
            ui_thread: std::thread::current().id(),
        }
    }
    /// 通道句柄：交给引擎的 `Context.events`（界面不接管引擎事件的唤醒节奏）。
    fn channel(&self) -> control::EventSender {
        self.channel.clone()
    }
    /// 回传结果事件并立即唤醒事件循环；接收端已退出时返回 Err（调用方按既有口径忽略）。
    /// 仅错误负载装箱：事件本身最大 300+ 字节，裸 `SendError<Event>` 会让每个 `Result` 都变胖。
    fn send(&self, event: Event) -> Result<(), mpsc::SendError<Box<Event>>> {
        match self.channel.try_send(event) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Disconnected(event)) => return Err(mpsc::SendError(event)),
            Err(mpsc::TrySendError::Full(event)) => {
                wake_event_loop();
                if std::thread::current().id() == self.ui_thread {
                    // 只有界面回调转后台可靠投递；worker 自身背压，避免事件洪峰扩张线程。
                    let channel = self.channel.clone();
                    std::thread::spawn(move || {
                        let _ = channel.send(*event);
                        wake_event_loop();
                    });
                } else {
                    self.channel.send(*event)?;
                }
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

/// 与 `run` 相同，但在事件循环启动前调用 `hook`——自动化测试用它安装驱动定时器，
/// 以真实回调路径驱动确认流，而不必访问任何内部状态。
pub fn run_with_pre_loop_hook(hook: impl FnOnce(&AppWindow) + 'static) -> Result<()> {
    run_with_engine_overrides(hook, None)
}

pub fn run_with_engine_overrides(
    hook: impl FnOnce(&AppWindow) + 'static,
    overrides: Option<EngineTestOverrides>,
) -> Result<()> {
    // P-10：窗口创建、设置加载失败也必须留在诊断日志中。
    let directory = overrides.as_ref().map_or_else(
        || jchtools_file_core::config::state_dir().ok(),
        |overrides| Some(overrides.state_dir.clone()),
    );
    let _diagnostic_log_guard = directory
        .as_deref()
        .and_then(jchtools_foundation::logging::init);
    let span = jchtools_foundation::logging::operation_span("gui", "run");
    let _entered = span.enter();
    let started = Instant::now();
    tracing::info!(event = "gui_started", "主界面启动");
    let result = run_gui(hook, overrides);
    if result.is_ok() {
        tracing::info!(
            event = "gui_completed",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "主界面已关闭"
        );
    } else {
        tracing::error!(
            event = "gui_failed",
            stage = "startup_or_event_loop",
            error_type = "gui_runtime",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "主界面启动或事件循环失败"
        );
    }
    result
}

/// 默认入口：无额外装配钩子。
pub fn run() -> Result<()> {
    run_with_pre_loop_hook(|_| ())
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
    jchtools_xberg::xberg_runtime::background_allowed()?;
    jchtools_foundation::xberg_settings::required()?;
    let executable = snap_ocr_assets::worker_install_path()?;
    let mut probe = std::process::Command::new(&executable);
    probe.arg("--capabilities");
    let output = jchtools_file_core::process::run_with_timeout(
        &mut probe,
        std::time::Duration::from_secs(10),
    )
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
        .map_err(|error| {
            tracing::error!(
                component = "snap_supervisor",
                event = "snap_service_spawn_failed",
                stage = "spawn",
                error_type = ?error.kind(),
                error_code = ?error.raw_os_error(),
                "截图服务进程启动失败"
            );
            format!("无法启动截图服务：{error}")
        })?;
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
                        if jchtools_xberg::xberg_runtime::background_allowed().is_ok()
                            && jchtools_foundation::xberg_settings::required().is_ok() =>
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
                    if let Err(stopped) = jchtools_xberg::xberg_runtime::background_allowed() {
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
