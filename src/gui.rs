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
    config::{Config, DeleteChoice},
    control::{Context, Control, Event, PlanSnapshot},
    db::Database,
    engine,
    git_tools::{self, GitShared, BACKOFF_UNIT},
    md_tools,
    model::{bytes, ActionKind, Summary},
    registry,
};
use anyhow::Result;
use serde::Deserialize;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

/// 当前工具（P-02 四个注册工具）：决定规则分区集合、流程与状态文案。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Extract,
    Organizer,
    Md,
    Git,
}
impl Tool {
    fn sections(self) -> &'static [&'static str] {
        match self {
            Tool::Extract => &["解压", "安全与性能"],
            Tool::Organizer => &["去重", "归类", "清理", "安全与性能"],
            // R-01：MD 整理与 Git 工具不设规则面板，分区集合为空。
            Tool::Md | Tool::Git => &[],
        }
    }
    fn from_id(id: &str) -> Option<Self> {
        match id {
            "recursive-extract" => Some(Tool::Extract),
            "directory-organizer" => Some(Tool::Organizer),
            "md-organizer" => Some(Tool::Md),
            "git-tools" => Some(Tool::Git),
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
    section: String,
    task: Option<PathBuf>,
    control: Option<Arc<Control>>,
    logs: VecDeque<String>,
    page: usize,
    page_starts: Vec<i64>,
    started: Instant,
    close_after: bool,
    pending_selection: usize,
    applying: bool,
    /// 递归解压运行中：解压不写整理流程的 read_bytes/completed 计数器，实时指标与
    /// 进度说明必须走单独口径，否则解压期间会显示恒为 0 的整理指标（U-03）。
    extracting: bool,
    planned: u64,
    plan_filter: Option<String>,
    /// 本轮勾选保存中出现过失败：pending 归零时用于决定是否重载计划页
    selection_failed: bool,
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
/// 否则按非法输入处理（报错并回退，不自动截断）。全程整数运算，不做浮点换算。
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
        Tool::Md | Tool::Git => "",
    };
    Ok(state
        .specs
        .iter()
        .filter(|s| {
            s.section == state.section
                && s.tools.iter().any(|t| t == tool)
                && rule_visible(s, &state.config, state.show_advanced)
        })
        .map(|spec| rule_row(spec, &data))
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
            match state.config.validate() {
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
    // 任务状态是第一道闸：已执行/已取消/失败的计划页重新加载时不能把 ready 重新点亮。
    if snapshot
        .status
        .as_deref()
        .is_none_or(|status| status != "ready")
    {
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
    ui.set_ready(matches!(outcome, PlanReadyState::Ready));
    if state.tool == Tool::Organizer && state.readiness_status_pending.replace(false) {
        ui.set_status(outcome.status_text().into());
    }
}
/// 计划页事件是否可应用：代际须仍是 latest，且 filter 与当前视图一致。
/// page 不再要求与 UI 预置值一致：翻页采用「先加载、成功再提交」，加载期间 state.page 仍是旧页。
fn plan_page_event_accepted(
    event_gen: u64,
    latest: u64,
    _event_page: usize,
    event_filter: Option<&str>,
    _ui_page: usize,
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
fn start_task(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender, apply: bool) {
    if ui.get_busy() {
        return;
    }
    if state.borrow().pending_selection != 0 {
        ui.set_error_text("计划勾选仍在保存中，请稍后再试".into());
        return;
    }
    let (configuration, task) = {
        let s = state.borrow();
        (s.config.clone(), s.task.clone())
    };
    if let Err(error) = configuration.validate() {
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
    let control = Arc::new(Control::default());
    {
        let mut s = state.borrow_mut();
        s.control = Some(control.clone());
        s.started = Instant::now();
        s.close_after = false;
        s.selection_failed = false;
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
    let out = out.clone();
    // 同一次 GUI 会话里可能先分析再执行：覆盖对象必须可重复使用，不能 take。
    let overrides = state
        .borrow()
        .engine_overrides
        .as_ref()
        .map(|o| EngineTestOverrides {
            state_dir: o.state_dir.clone(),
        });
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
            } else {
                match &overrides {
                    // 测试注入只需隔离任务库状态目录（回收站注入随 S-02 一并移除）。
                    Some(o) => engine::prepare_at(&directory, configuration, context, &o.state_dir),
                    None => engine::prepare(&directory, configuration, context),
                }
            }
        }));
        let event = match result {
            Ok(Ok(result)) => {
                if apply {
                    Event::Done(result.directory, result.summary)
                } else {
                    Event::Ready(result.directory, result.summary)
                }
            }
            Ok(Err(error)) => Event::Failed(format!("{error:#}")),
            Err(_) => Event::Failed("整理线程意外退出；未执行的步骤不会继续".into()),
        };
        let _ = out.send(event);
    });
}
/// 「递归解压」一段式启动（X-02）：一段确认后连续执行到结束；无计划审核环节，
/// 结束事件 ExtractDone 只收尾摘要，不改 ready/has_task（那是目录整理两段式的状态）。
fn start_extract(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    if ui.get_busy() {
        return;
    }
    let configuration = {
        let s = state.borrow();
        s.config.clone()
    };
    if let Err(error) = configuration.validate() {
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
    let out = out.clone();
    let overrides = state
        .borrow()
        .engine_overrides
        .as_ref()
        .map(|o| EngineTestOverrides {
            state_dir: o.state_dir.clone(),
        });
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match overrides {
            Some(overrides) => engine::extract_run_at(
                &directory,
                configuration,
                context,
                &overrides.state_dir,
                None,
            ),
            None => engine::extract_run(&directory, configuration, context),
        }));
        let event = match result {
            Ok(Ok(result)) => Event::ExtractDone(result.directory, result.summary),
            Ok(Err(error)) => Event::Failed(format!("{error:#}")),
            Err(_) => Event::Failed("解压线程意外退出；未执行的步骤不会继续".into()),
        };
        let _ = out.send(event);
    });
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
/// 通用目录选择（U-07）：标题按用途传入；从 `directory` 输入框的已有值起步，
/// 选择后经 `apply` 写回对应输入框（MD/Git 各自的目录输入互不影响）。
fn pick_directory(ui: &AppWindow, title: &str, apply: impl FnOnce(&AppWindow, PathBuf)) {
    let mut dialog = rfd::FileDialog::new().set_title(title);
    let entered = PathBuf::from(ui.get_directory().to_string());
    if entered.is_dir() {
        dialog = dialog.set_directory(entered);
    } else {
        // 主输入为空或无效时，退到 MD 输入目录起步（U-07 不无故回到其他位置）。
        let md_input = PathBuf::from(ui.get_md_input_dir().to_string());
        if md_input.is_dir() {
            dialog = dialog.set_directory(md_input);
        }
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
    let progress_out = out.clone();
    let progress_control = Arc::clone(control);
    let stats = md_tools::merge_markdown(
        &entries,
        output,
        overwrite,
        control,
        &|index: usize, total: usize| {
            progress_control.set_planned(u64::try_from(total).unwrap_or(0));
            progress_control
                .completed
                .store(u64::try_from(index).unwrap_or(0), Ordering::Relaxed);
            let _ = progress_out.send(Event::Status(format!("合并中：{index} / {total} 个文件")));
            Ok(())
        },
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
    let progress_out = out.clone();
    let progress_control = Arc::clone(control);
    let written = md_tools::run_split(
        input,
        &plan,
        out_dir,
        overwrite,
        control,
        &|index: usize, total: usize| {
            progress_control.set_planned(u64::try_from(total).unwrap_or(0));
            progress_control
                .completed
                .store(u64::try_from(index).unwrap_or(0), Ordering::Relaxed);
            let _ = progress_out.send(Event::Status(format!("拆分中：{index} / {total} 片")));
            Ok(())
        },
    )
    .map_err(|error| format!("{error:#}"))?;
    let _ = written;
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
    ui.set_error_text("".into());
    ui.set_notice_text("".into());
    ui.set_progress(-1.0);
    ui.set_progress_note("".into());
    ui.set_log_text("".into());
    ui.set_git_state("检查仓库".into());
    ui.set_status("正在验证仓库（分支、upstream 与仓库状态）…".into());
    let worker_out = out.clone();
    std::thread::spawn(move || {
        let git = match git_tools::find_git() {
            Ok(git) => git,
            Err(error) => {
                let _ = worker_out.send(Event::Failed(format!("{error:#}")));
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

/// 勾选切换时就地更新该行：勾选值与状态显示同时改，避免「取消勾选后仍显示待执行」
/// 到数据库事件回来前的不一致；保存失败时由 SelectionSaved/重载恢复数据库真值。
fn patch_plan_row(ui: &AppWindow, id: i64, selected: bool) {
    let plans = ui.get_plans();
    let Some(model) = plans.as_any().downcast_ref::<VecModel<PlanRow>>() else {
        return;
    };
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            if row.id.as_str().parse::<i64>() == Ok(id) {
                row.selected = selected;
                row.state = plan_row_state(selected, row.state.as_str()).into();
                model.set_row_data(i, row);
                break;
            }
        }
    }
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
        3 | 4 => true,
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
                    };
                    ui.set_screen(screen);
                    ui.set_active_tool_id(id.clone());
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
                // 「关于」不是工具：清空 active-tool-id，侧栏不高亮任何工具。
                // 返回工具页统一走 select_tool（工具 NavItem 的点击回调），此处不再恢复列表：
                // 该回调在生产中只被「关于」NavItem 以 1 调用，其余分支属不可达路径。
                if screen == 1 {
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
                let selected = state
                    .borrow()
                    .specs
                    .iter()
                    .find(|s| s.key == key.as_str())
                    .and_then(|s| s.choices.get(usize::try_from(index.max(0)).unwrap_or(0)))
                    .map(|c| c[0].clone());
                if let Some(value) = selected {
                    let hint = state
                        .borrow()
                        .specs
                        .iter()
                        .find(|s| s.key == key.as_str())
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
                let capacity = spec.as_ref().is_some_and(|s| s.unit.as_deref() == Some("bytes"));
                let parsed = if numeric {
                    if let Ok(v) = parse_capacity(value.as_str(), capacity) {
                        serde_json::Value::from(v)
                    } else {
                        // 清空/非法：不写配置，就地把该行显示改回配置真值。
                        // 禁止整表 refresh：会销毁正在编辑的 LineEdit 并丢焦点。
                        let current = {
                            let cfg =
                                serde_json::to_value(&state.borrow().config).unwrap_or_default();
                            cfg.get(key.as_str()).cloned().unwrap_or_default()
                        };
                        let restore = current.as_u64().map(|v| v.to_string()).unwrap_or_default();
                        if value.is_empty() {
                            let key = (*key).to_string();
                            patch_rule_row(&ui, key.as_str(), |row| {
                                row.value = restore.clone().into();
                            });
                            return;
                        }
                        show_error(
                            &ui,
                            if capacity {
                                "该设置需要非负数字，可选单位 B/KiB/MiB/GiB/TiB（换算后须为整数字节）"
                            } else {
                                "该设置需要输入非负整数"
                            },
                        );
                        let key = (*key).to_string();
                        patch_rule_row(&ui, key.as_str(), |row| {
                            row.value = restore.clone().into();
                        });
                        return;
                    }
                } else {
                    serde_json::Value::from(value.to_string())
                };
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
                    // 数值超出字段范围（如超过 u32 上限）等写入失败：与非法文本同口径处理，
                    // 中文提示并回退行显示，避免输入框与配置真值不一致直到整表重建。
                    let restore = {
                        let cfg = serde_json::to_value(&state.borrow().config).unwrap_or_default();
                        cfg.get(key.as_str()).cloned().unwrap_or_default()
                    };
                    let restore = restore.as_u64().map(|v| v.to_string()).unwrap_or_default();
                    patch_rule_row(&ui, key.as_str(), |row| row.value = restore.clone().into());
                    show_error(&ui, "该设置超出允许的范围");
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
                if let Err(error) = state.borrow().config.validate() {
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
            fail_refreshed: Cell::new(
                Instant::now()
                    .checked_sub(FAIL_REFRESH_MIN)
                    .unwrap_or_else(Instant::now),
            ),
        }
    }
    /// 一次刷新：主题 → 排空事件 → 日志上屏 → 失败列表 → 实时指标。
    fn run(&self, ui: &AppWindow) {
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
        self.apply_fail_messages(ui);
        self.refresh_fail_list(ui);
        self.refresh_runtime(ui);
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
                    if !ui.get_paused() {
                        pending_status = Some(text);
                    }
                }
                Event::Log(text) => {
                    let mut s = self.state.borrow_mut();
                    push_event_log(&mut s.logs, text);
                    self.log_dirty.set(true);
                }
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
                    if self.state.borrow().close_after {
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
                    // 就地把该行勾选值改成数据库里的真实值：用户编辑会让 CheckBox 脱离
                    // `checked: item.selected` 绑定，只有这里回写模型才能保证界面与数据一致
                    //（无障碍/自动化切换时 Slint 不一定立即重绘，更需要这一步）。
                    // 只在当前展示的仍是该任务时回写：action id 是各任务库各自的 rowid，
                    // 载入别的任务后可能恰好出现相同 id，不能按 id 跨任务匹配。
                    let mut batch_failed = false;
                    if s.task.as_ref() == Some(&path) {
                        if let Some((id, selected)) = saved {
                            let plans = ui.get_plans();
                            if let Some(model) = plans.as_any().downcast_ref::<VecModel<PlanRow>>()
                            {
                                for i in 0..model.row_count() {
                                    if let Some(mut row) = model.row_data(i) {
                                        // 与 patch_plan_row 同口径比较：按数值比 id，避免每行
                                        // 都拼一次 id.to_string()（保存一行的分配次数与行数同阶）。
                                        if row.id.as_str().parse::<i64>() == Ok(id) {
                                            row.selected = selected;
                                            model.set_row_data(i, row);
                                            break;
                                        }
                                    }
                                }
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
                        && !ui.get_busy()
                    {
                        // 勾选保存成功后重算就绪：由既有计划 worker 取回快照（不开库），
                        // 且不改写状态栏文案（收尾文案归任务事件）。
                        load_plan_state(
                            &self.out,
                            &s.plan_load,
                            path,
                            ui.get_directory().to_string(),
                            PlanQuery::readiness(),
                        );
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
                        page,
                        filter.as_deref(),
                        s.page,
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
                    if ui.get_busy() || s.pending_selection != 0 {
                        continue;
                    }
                    apply_plan_readiness(ui, &s, snapshot.as_ref());
                }
                Event::Notice(text) => ui.set_notice_text(text.into()),
                // Error 更新错误文案。
                // 计划筛选曾乐观置「加载中…」：失败必须恢复分页控件，避免永久中间态。
                Event::Error(text) => {
                    ui.set_error_text(text.into());
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
                    if self.state.borrow().close_after {
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
                    ui.set_progress(-1.0);
                    ui.set_progress_note("".into());
                    if close_after {
                        // U-09：用户已确认「停止并关闭」。MD 扫描与冲突检查不经过任务
                        // 检查点，操作自然结束走到这里——此时应直接退出应用，不得再弹
                        // 覆盖确认把用户留在界面里，也不得残留 close_after。
                        let _ = slint::quit_event_loop();
                    } else {
                        ui.set_confirm_text(text.into());
                        ui.set_acknowledge(false);
                        ui.set_confirm_kind(4);
                    }
                }
                Event::MdDone(text) => {
                    pending_status = None;
                    terminal = true;
                    let close_after = {
                        let mut s = self.state.borrow_mut();
                        s.control = None;
                        s.runtime = RuntimeMode::Organizer;
                        push_event_log(&mut s.logs, text.clone());
                        self.log_dirty.set(true);
                        // U-09：close_after 在此消费；MD 扫描与合并不经过任务检查点，
                        // 「停止并关闭」确认后任务会自然结束——结束时必须关闭窗口，
                        // 而不是把窗口留在运行完成状态、把请求残留到之后的任务。
                        std::mem::take(&mut s.close_after)
                    };
                    ui.set_busy(false);
                    ui.set_progress(-1.0);
                    ui.set_progress_note("".into());
                    ui.set_status(text.into());
                    if close_after {
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
                    ui.set_progress(-1.0);
                    ui.set_progress_note("".into());
                    ui.set_status(text.into());
                    if self.state.borrow().close_after {
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
        let filter = {
            let mut s = self.state.borrow_mut();
            s.task = Some(path.clone());
            s.page = 0;
            s.page_starts = vec![0];
            s.control = None;
            s.applying = false;
            s.extracting = false;
            s.plan_filter = None;
            apply_summary(ui, &mut s, summary);
            None
        };
        ui.set_busy(false);
        ui.set_paused(false);
        ui.set_ready(false);
        ui.set_plan_editable(false);
        ui.set_has_task(true);
        ui.set_panel(1);
        ui.set_plan_filter(0);
        ui.set_progress(-1.0);
        ui.set_progress_note("".into());
        ui.set_plan_prev_enabled(false);
        ui.set_plan_next_enabled(false);
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
        if self.state.borrow().close_after {
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
            ui.set_progress(-1.0);
            ui.set_progress_note("准备中".into());
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
    Ok(State {
        config: Config::default(),
        specs,
        section: "去重".into(),
        task: None,
        control: None,
        logs: VecDeque::new(),
        page: 0,
        page_starts: vec![0],
        started: Instant::now(),
        close_after: false,
        pending_selection: 0,
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
                pick_directory(&ui, "选择要合并的目录", |ui, path| {
                    ui.set_md_input_dir(path.display().to_string().into());
                });
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_md_choose_output(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(&ui, "选择合并输出目录", |ui, path| {
                    ui.set_md_output_dir(path.display().to_string().into());
                });
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_md_split_choose_dir(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(&ui, "选择拆分输出目录", |ui, path| {
                    ui.set_md_split_dir(path.display().to_string().into());
                });
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
                pick_directory(&ui, "选择 Git 项目目录", |ui, path| {
                    ui.set_git_repo(path.display().to_string().into());
                });
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
            if let Some(control) = &state.borrow().control {
                control.cancel();
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_status(
                    "正在停止：等待当前 git 命令结束后不再继续；已成功推送的文件保持成功".into(),
                );
            }
        });
    }
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
    #[cfg(feature = "perf-tracing")]
    let _perf_guard = {
        let directory = state.borrow().engine_overrides.as_ref().map_or_else(
            || crate::config::state_dir().ok(),
            |o| Some(o.state_dir.clone()),
        );
        directory.as_deref().and_then(crate::perf::init)
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
    // 启动落在注册表第一个工具（P-02 顺序：递归解压在前）。必须在 wire_sync 之后调用：
    // 回调接线前的 invoke 是空调用，窗口会停在目录整理页。
    ui.invoke_select_tool("recursive-extract".into());
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_confirmed(move |kind| {
            if let Some(ui) = weak.upgrade() {
                if kind == 3 {
                    state.borrow_mut().close_after = true;
                    // 任务可能已在确认框打开期间结束：此时没有可取消的对象，直接退出窗口，
                    // 否则状态停在“取消任务中”且 close_after 残留，会让之后的任务收尾时意外关闭应用。
                    let control = state.borrow().control.clone();
                    if let Some(control) = control {
                        control.cancel();
                    } else {
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
            if let Some(control)=&state.borrow().control{let pause=!control.is_paused();control.pause(pause);ui.set_paused(pause);
                ui.set_status(if pause{"已请求暂停；正在运行的压缩包在完成后暂停，Hash 和整理操作在分块/文件边界暂停"}else{"继续处理"}.into());}
        }});
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_cancel_task(move || {
            if let Some(control) = &state.borrow().control {
                control.cancel();
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_status("正在取消；当前操作完成后停止，不会继续后续操作".into());
            }
        });
    }
    {
        let state = state.clone();
        let out = out.clone();
        let weak = ui.as_weak();
        ui.on_plan_toggle(move |id, selected| {
            let task = state.borrow().task.clone();
            if let (Some(task), Ok(id)) = (task, id.parse::<i64>()) {
                if let Some(ui) = weak.upgrade() {
                    if ui.get_busy() {
                        return;
                    }
                    // 勾选保存中 ready 暂降，避免在途勾选时误点执行；其他项仍可在 Slint 侧继续编辑。
                    ui.set_ready(false);
                    // C-11：勾选后即时显示对应状态（取消勾选→已取消勾选，重新勾选→待执行），
                    // 不等数据库事件回来；保存失败时由 SelectionSaved/重载恢复真值。
                    patch_plan_row(&ui, id, selected);
                }
                state.borrow_mut().pending_selection += 1;
                let out = out.clone();
                std::thread::spawn(move || {
                    // 与 async_work 一致地拦截 panic：否则 pending_selection 永远减不到 0，
                    // 会静默阻断后续的开始执行与历史载入。
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        Database::open_existing(&task).and_then(|db| db.set_selected(id, selected))
                    }));
                    let result = result
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("保存勾选时后台操作意外退出")));
                    let saved = result.is_ok().then_some((id, selected));
                    let _ = out.send(Event::SelectionSaved(
                        task,
                        saved,
                        result.err().map(|e| format!("{e:#}")),
                    ));
                });
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
            if let Some(ui)=weak.upgrade(){if ui.get_busy(){
                ui.set_confirm_text("任务仍在处理文件。确认后会请求取消，等待正在进行的操作结束，再关闭窗口。已经完成的操作不会自动回滚。".into());
                ui.set_confirm_kind(3);ui.set_acknowledge(false);return slint::CloseRequestResponse::KeepWindowShown;
            }}
            if let Some(control)=&state.borrow().control{control.cancel();}
            // Slint 1.17 的 CloseRequestResponse 只有 HideWindow / KeepWindowShown，
            // HideWindow 仅隐藏窗口、事件循环仍在跑；必须显式 quit 才能让进程真正退出。
            let _=slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
        });
    }
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
    // 窗口刚映射时系统还会套用默认位置，稍后再居中一次，保证首屏就是居中的
    let centered = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(120), move || {
        if let Some(ui) = centered.upgrade() {
            center_window(ui.window());
        }
    });
    hook(&ui);
    let loop_result = slint::run_event_loop();
    // 退出后清掉唤醒钩子：它持有本次运行的 State 与事件通道，留着会跨运行泄漏。
    EVENT_LOOP_DRAIN.with(|slot| {
        let _ = slot.borrow_mut().take();
    });
    loop_result?;
    Ok(())
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
        let output_text = output.display().to_string();
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
            let _ = output_text;
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
            !plan_page_event_accepted(5, 6, 0, None, 0, None),
            "低代际事件必须拒绝"
        );
        // 高代际且 filter 与视图一致：可应用（翻页为「先加载、成功再提交」，不校验预置页码）。
        assert!(plan_page_event_accepted(
            6,
            6,
            0,
            Some("delete"),
            0,
            Some("delete")
        ));
        assert!(plan_page_event_accepted(6, 6, 1, None, 0, None));
        // 同代但用户已切筛选：拒绝。
        assert!(!plan_page_event_accepted(6, 6, 0, None, 0, Some("delete")));
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
            let (tx, rx) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("gui-test-worker".into())
                .spawn(move || {
                    i_slint_backend_testing::init_no_event_loop();
                    let ui = AppWindow::new().unwrap();
                    let state = Rc::new(RefCell::new(initial_state().unwrap()));
                    ui.set_tool_count(i32::try_from(registry::tools().len()).unwrap_or(i32::MAX));
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
                    refresh(&ui, &state.borrow());
                    let pump = UiPump::new(event_rx, state.clone(), out);
                    let app = GuiTestApp { ui, state, pump };
                    while let Ok(job) = rx.recv() {
                        app.reset();
                        let _ =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&app)));
                    }
                })
                .expect("启动 GUI 测试工作线程");
            Mutex::new(tx)
        })
    }
    /// 驱动后台结果落地：真实 worker 线程发送、真实事件泵应用，最多等 5 秒。
    /// `done` 为真即返回；超时返回 false，由调用方断言给出可读失败原因。
    fn pump_until(app: &GuiTestApp, done: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
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

    // 覆盖 P-02, P-04, H-03
    #[test]
    fn initial_surface_lists_defaults() {
        with_gui(|app| {
            let ui = &app.ui;
            assert!(!ui.get_ready(), "初始状态不得就绪");
            assert_eq!(ui.get_theme(), 0, "默认跟随系统主题");
            assert_eq!(
                ui.get_tool_count(),
                4,
                "当前注册的工具数量（P-02：四个工具）"
            );
            // H-03：四个独立工具入口都正常可见（侧栏遍历注册表，不做隐藏、折叠或降级）。
            let tools = ui.get_tools();
            assert_eq!(tools.row_count(), 4, "侧栏必须同时列出全部工具");
            let ids: Vec<String> = (0..tools.row_count())
                .filter_map(|i| tools.row_data(i))
                .map(|tool| tool.id.to_string())
                .collect();
            assert!(
                ids.iter().any(|id| id == "recursive-extract")
                    && ids.iter().any(|id| id == "directory-organizer")
                    && ids.iter().any(|id| id == "md-organizer")
                    && ids.iter().any(|id| id == "git-tools"),
                "四个工具入口必须按注册表 id 出现在侧栏：{ids:?}"
            );
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
    // 覆盖 U-06（非法输入红条提示并回退显示）
    #[test]
    fn invalid_number_input_reports_error_and_reverts_value() {
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
                Some("16"),
                "非法输入必须回退为配置真值"
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

    /// 构造 MD 合并输入/输出目录：`file_count` 个带内容的 .md 文件（让合并耗时可观测）。
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

    /// 从实时指标文本解析「耗时 N s」的秒数（MD/Git 指标口径）。
    fn elapsed_in_metrics(metrics: &str) -> Option<f64> {
        let index = metrics.find("耗时 ")?;
        let rest = metrics[index + "耗时 ".len()..].trim_end_matches('s');
        rest.parse().ok()
    }

    // 覆盖 U-03（MD 合并/拆分启动时耗时必须从本次任务起算，不得沿用上次任务的时钟）
    #[test]
    fn md_start_resets_started_clock() {
        // 合并：上次任务遗留 10 分钟前的旧时钟，启动后实时耗时必须回到本次任务口径。
        let (docs, out_dir, dir) = make_md_fixture("md-clock-merge", 20);
        let docs_text = docs.display().to_string();
        let out_text = out_dir.display().to_string();
        with_gui(move |app| {
            app.ui.invoke_select_tool("md-organizer".into());
            let stale = Instant::now()
                .checked_sub(Duration::from_secs(600))
                .expect("系统运行时间不足 600 秒，无法构造旧时钟");
            app.state.borrow_mut().started = stale;
            app.ui.set_md_input_dir(docs_text.clone().into());
            app.ui.set_md_output_name("merged.md".into());
            app.ui.set_md_output_dir(out_text.into());
            app.ui.invoke_md_merge_start();
            assert!(
                pump_until(app, || {
                    app.ui.get_busy()
                        && elapsed_in_metrics(app.ui.get_metrics().as_str())
                            .is_some_and(|secs| secs < 120.0)
                }),
                "MD 合并启动后耗时必须从本次任务起算（不得显示 600s+ 旧时钟）：metrics={}",
                app.ui.get_metrics()
            );
            assert!(pump_until(app, || !app.ui.get_busy()), "合并应正常收尾");
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        // 拆分：同一口径
        let (docs, out_dir, dir) = make_md_fixture("md-clock-split", 8);
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
            app.ui.set_md_split_unit(1); // MB：单文件上限 1MB，多片写出保证耗时可观测
            app.ui.set_md_split_dir(out_text.into());
            app.ui.invoke_md_split_start();
            assert!(
                pump_until(app, || {
                    app.ui.get_busy()
                        && elapsed_in_metrics(app.ui.get_metrics().as_str())
                            .is_some_and(|secs| secs < 120.0)
                }),
                "MD 拆分启动后耗时必须从本次任务起算：metrics={}",
                app.ui.get_metrics()
            );
            assert!(pump_until(app, || !app.ui.get_busy()), "拆分应正常收尾");
        })
        .unwrap();
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
            app.ui.invoke_git_start();
            assert!(
                !app.state.borrow().close_after,
                "Git 启动必须重置 close_after（U-09）"
            );
            assert!(pump_until(app, || !app.ui.get_busy()), "Git 任务应快速收尾");
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&git_dir);
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
}
