//! GUI 组装层：Slint 界面的状态、回调整体在此实现；`main.rs` 只是薄壳入口。
//! 同步回调集中在 `wire_sync`，便于无头测试装配后直接断言界面状态。
/// Slint 生成代码（target/**/out/app.rs）不做 unwrap/可达性审查：机器生成、
/// 修复无意义且计数随 UI 改版大幅波动；业务代码保持 unwrap/expect 全面禁止。
#[allow(clippy::unwrap_used)]
#[allow(unreachable_pub)]
mod generated_ui {
    slint::include_modules!();
}
pub use generated_ui::*;

use crate::{
    config::{ClassifyMode, Config, ConflictPolicy, DEFAULT_CUSTOM_CATEGORIES},
    control::{ConflictAnswer, Context, Control, Event},
    db::Database,
    engine,
    model::{bytes, ActionKind},
    platform, registry,
};
use anyhow::{Context as _, Result};
use serde::Deserialize;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::{
    cell::RefCell,
    collections::VecDeque,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// 当前工具（P-02 两个注册工具）：决定规则分区集合、流程与状态文案。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Extract,
    Organizer,
}
impl Tool {
    fn sections(self) -> &'static [&'static str] {
        match self {
            Tool::Extract => &["解压", "安全与性能"],
            Tool::Organizer => &["去重", "归类", "清理", "安全与性能"],
        }
    }
    fn from_id(id: &str) -> Option<Self> {
        match id {
            "recursive-extract" => Some(Tool::Extract),
            "directory-organizer" => Some(Tool::Organizer),
            _ => None,
        }
    }
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
}
struct State {
    config: Config,
    specs: Vec<RuleSpec>,
    section: String,
    task: Option<PathBuf>,
    control: Option<Arc<Control>>,
    conflict: Option<mpsc::SyncSender<ConflictAnswer>>,
    logs: VecDeque<String>,
    page: usize,
    page_starts: Vec<i64>,
    started: Instant,
    close_after: bool,
    pending_selection: usize,
    applying: bool,
    planned: u64,
    plan_filter: Option<String>,
    /// 本轮勾选保存中出现过失败：pending 归零时用于决定是否重载计划页
    selection_failed: bool,
    /// 「显示高级选项」开关：只影响显示，不落盘、不改变任何默认值
    show_advanced: bool,
    /// 当前工具：切工具时同步重置规则分区（P-02 两工具各自只显示相关分区，R-01）。
    tool: Tool,
    /// 测试注入：覆盖任务状态目录与回收站实现；生产路径为 None，仍走 engine::prepare/apply。
    engine_overrides: Option<EngineTestOverrides>,
    /// 计划页加载代际（跨线程）：丢弃晚到的旧 filter/page 结果。
    plan_load: Arc<PlanLoadSync>,
}
/// 计划页加载代际同步：worker 完成后写入 `completed` 的 gen（单调不降），
/// 事件循环只应用「事件 gen 仍是 latest 且 filter/page 与当前视图一致」的结果。
/// completed 仅记录最新完成代际，供 worker 避免低代际覆盖；UI 应用走事件自带 actions。
#[derive(Default)]
struct PlanLoadSync {
    /// 最新一次请求的代际（每次 load_plan_filtered 递增）
    latest: AtomicU64,
    /// 最近完成的代际；低代际完成不得覆盖高代际
    completed: Mutex<Option<PlanLoadDone>>,
}
struct PlanLoadDone {
    gen: u64,
}
/// 无头 GUI 测试用的引擎注入：隔离任务库到 tempfile，并避免污染真实回收站。
pub struct EngineTestOverrides {
    pub state_dir: PathBuf,
    pub recycler: Arc<dyn platform::Recycler>,
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
#[allow(non_snake_case)]
mod win32_monitor {
    use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
    #[repr(C)]
    pub(super) struct MonitorInfo {
        pub cbSize: u32,
        pub rcMonitor: RECT,
        pub rcWork: RECT,
        pub dwFlags: u32,
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
    let foreground = unsafe { GetForegroundWindow() };
    let monitor = if foreground.is_null() {
        let mut point = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&raw mut point) } == 0 {
            return;
        }
        unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) }
    } else {
        unsafe { MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) }
    };
    if monitor.is_null() {
        return;
    }
    // Win32 ABI 要求的 cbSize：结构体仅数十字节，截断不可能发生。
    #[allow(clippy::cast_possible_truncation)]
    let cb_size = std::mem::size_of::<MonitorInfo>() as u32;
    let mut info = MonitorInfo {
        cbSize: cb_size,
        rcMonitor: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        rcWork: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        dwFlags: 0,
    };
    // 兼容部分声明布局：cbSize 必须正确
    if unsafe { GetMonitorInfoW(monitor, &raw mut info) } == 0 {
        return;
    }
    let work = info.rcWork;
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
    // DWORD 缓冲区大小恒为 4，截断不可能发生。
    #[allow(clippy::cast_possible_truncation)]
    let mut size = std::mem::size_of::<u32>() as u32;
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
/// 规则行是否显示：高级层默认隐藏；联动项在依赖未开启且自身仍是默认值时不显示。
/// 已经改过值的行必须保留可见，否则依赖关掉后用户既看不到该行、也无法把它改回去。
fn rule_visible(spec: &RuleSpec, config: &Config, show_advanced: bool) -> bool {
    if !show_advanced && spec.tier == Tier::Advanced {
        return false;
    }
    match spec.key.as_str() {
        "custom_categories" => {
            config.classify == ClassifyMode::Custom
                || config.custom_categories != DEFAULT_CUSTOM_CATEGORIES
        }
        "large_threshold_gib" => config.large_files || config.large_threshold_gib != 1,
        _ => true,
    }
}
fn visible_rows(state: &State) -> Result<Vec<RuleRow>> {
    let data = serde_json::to_value(&state.config)?;
    let tool = match state.tool {
        Tool::Extract => "extract",
        Tool::Organizer => "organizer",
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
/// 目录输入变化后的就绪同步：已有任务时用 recompute_ready 按
/// 「任务 + 配置 + 目录」重算，而不是一律 invalidate——
/// 用户可能只是重新输入了同一路径，不应清掉仍可执行的计划。
fn sync_ready_after_directory(ui: &AppWindow, state: &State) {
    if ui.get_has_task() && state.tool == Tool::Organizer {
        if let Some(task) = state.task.clone() {
            let ready = recompute_ready(ui, state, &task);
            ui.set_ready(ready);
            ui.set_status(if ready {
                SharedString::from("目录与当前计划一致，可以确认执行")
            } else {
                SharedString::from("规则或目录已改变，请重新分析后再执行")
            });
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
/// 注意 task 必须先用普通 let 从 state 提取：edition 2021 下 if-let scrutinee 的
/// borrow() Ref 临时存活到整个语句结束，块内 borrow_mut 会 BorrowMutError panic
/// ——任务取消/失败且有任务时 100% 触发（回归见 gui_tests）。
fn reload_after_failed(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    sender: &mpsc::SyncSender<Event>,
) {
    // task 先用普通 let 提取后再 if-let：scrutinee 里的 borrow() Ref 不会存活到块内，
    // 块内的 borrow_mut 与共享借用才安全（缺陷模式见回归测试 failed_reload_*）。
    let task = state.borrow().task.clone();
    if let Some(task) = task {
        if let Ok(db) = Database::open_existing(&task) {
            if let Ok(summary) = db.summary() {
                ui.set_summary(summary.description().into());
                // 计数为显示用途，超出 i32 的极端值饱和显示即可。
                ui.set_plan_delete_count(i32::try_from(summary.planned_delete).unwrap_or(i32::MAX));
                ui.set_plan_move_count(i32::try_from(summary.planned_move).unwrap_or(i32::MAX));
                ui.set_plan_link_count(i32::try_from(summary.planned_link).unwrap_or(i32::MAX));
                ui.set_plan_empty_count(i32::try_from(summary.planned_empty).unwrap_or(i32::MAX));
                {
                    let mut s = state.borrow_mut();
                    s.planned = summary.planned_delete
                        + summary.planned_move
                        + summary.planned_link
                        + summary.planned_empty;
                }
                ui.set_metrics(
                    format!(
                        "扫描 {} 个文件 · 错误 {} 项 · 已回收 {} 项",
                        summary.scanned, summary.errors, summary.recycled
                    )
                    .into(),
                );
            }
        }
        let (start, page, filter) = {
            let s = state.borrow();
            (
                s.page_starts.get(s.page).copied().unwrap_or(0),
                s.page,
                s.plan_filter.clone(),
            )
        };
        let plan_load = state.borrow().plan_load.clone();
        load_plan_filtered(sender, &plan_load, task, start, page, filter);
    }
}
fn load_plan_filtered(
    sender: &mpsc::SyncSender<Event>,
    plan_load: &Arc<PlanLoadSync>,
    path: PathBuf,
    start: i64,
    page: usize,
    kind: Option<String>,
) {
    // 递增代际：同一会话里筛选/翻页会并发发起多次加载，晚到的低代际结果不得覆盖当前视图。
    let gen = plan_load.latest.fetch_add(1, Ordering::AcqRel) + 1;
    let load = Arc::clone(plan_load);
    // 线程需要所有权：从引用克隆出独立句柄（成功路径用 sender，失败路径用 err_sender）。
    let sender = sender.clone();
    let err_sender = sender.clone();
    // 失败路径要区分「回空页」与「只报错」两种事件（见下），不走 async_work 的单事件映射。
    // 打开的是引擎已生成的任务库：缺失时报错，不静默新建空库。
    std::thread::spawn(move || {
        // 与 async_work 一致地拦截 panic：否则失败事件缺失会让 UI 停在「加载中…」。
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Database::open_existing(&path)
                .and_then(|db| db.actions_page_filtered(start, 101, kind.as_deref()))
        }));
        let load_result = match result {
            Ok(inner) => inner,
            Err(_) => Err(anyhow::anyhow!("计划加载时后台操作意外退出；请重试")),
        };
        let event = match load_result {
            Ok(actions) => {
                record_plan_load_done(&load, gen);
                Event::PlanPage(path, actions, page, gen, kind)
            }
            Err(error) => {
                if page == 0 {
                    // 筛选切换/任务加载（都从第 0 页开始）失败必须回空页：否则列表残留
                    // 上一筛选甚至上一任务的行，与已切换的胶囊不一致。
                    record_plan_load_done(&load, gen);
                    let _ = sender.send(Event::PlanPage(path, Vec::new(), page, gen, kind.clone()));
                }
                // 失败提示带代际/筛选归属：过期请求（用户已切走筛选/翻页）的失败
                // 不得把红条误报到当前正确视图上，UI 侧按 gen/filter 决定是否上屏。
                let _ = err_sender.send(Event::PlanLoadFailed(format!("{error:#}"), gen, kind));
                return;
            }
        };
        let _ = sender.send(event);
    });
}
/// 只允许写入不低代际的结果：先完成的高代际不被后完成的低代际覆盖。
fn record_plan_load_done(load: &PlanLoadSync, gen: u64) {
    let mut guard = load
        .completed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.as_ref().is_none_or(|done| done.gen <= gen) {
        *guard = Some(PlanLoadDone { gen });
    }
}
/// 按「任务状态 + 当前配置 + 目录一致性」重新计算整理计划的就绪状态。
/// theme 是纯外观设置，不影响计划内容，比较时剔除；
/// SelectionSaved 与 PlanPage 两处共用，避免一处重算一处漏算。
fn recompute_ready(ui: &AppWindow, state: &State, path: &Path) -> bool {
    let strip_theme = |value: &mut serde_json::Value| {
        if let Some(object) = value.as_object_mut() {
            object.remove("theme");
        }
    };
    let Ok(db) = Database::open_existing(path) else {
        return false;
    };
    // 任务状态是第一道闸：已执行/已取消/失败的计划页重新加载时不能把 ready 重新点亮。
    if !db
        .get::<String>("status")
        .is_ok_and(|status| status == "ready")
    {
        return false;
    }
    let Some(mut db_config) = db
        .config()
        .ok()
        .and_then(|config| serde_json::to_value(&config).ok())
    else {
        return false;
    };
    let Ok(mut current) = serde_json::to_value(&state.config) else {
        return false;
    };
    strip_theme(&mut db_config);
    strip_theme(&mut current);
    db_config == current
        && db.get::<String>("root").is_ok_and(|root| {
            std::fs::canonicalize(Path::new(ui.get_directory().as_str()))
                .is_ok_and(|selected| selected == Path::new(&root))
        })
}
/// 计划页事件是否可应用：代际须仍是 latest，且 filter 与当前视图一致。
/// page 不再要求与 UI 预置值一致：翻页采用「先加载、成功再提交」，加载期间 state.page 仍是旧页。
/// 过期事件直接拒绝，不触碰 completed 缓存——否则低代际事件会 take 走高代际结果并一并丢弃。
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
/// 用户取消勾选的项不计入。计数失败时返回 Err，调用方可保留旧 planned。
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
fn start_task(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    sender: &mpsc::SyncSender<Event>,
    apply: bool,
) {
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
        s.logs.clear();
        s.page = 0;
        s.page_starts = vec![0];
        s.conflict = None;
        s.applying = apply;
        // 执行分母按将实际执行的勾选数修正：规划期 planned 是全量，用户可能已取消部分勾选。
        // 计数失败时保留旧 planned，不阻断执行（进度分母可能偏大，但仍可收敛）。
        if apply {
            if let Some(task) = s.task.clone() {
                if let Ok(n) = count_selected_pending(&task) {
                    s.planned = n;
                }
            }
        }
    }
    ui.set_busy(true);
    ui.set_ready(false);
    ui.set_paused(false);
    ui.set_error_text("".into());
    ui.set_notice_text("".into());
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
    let ask_sender = sender.clone();
    let ask_control = control.clone();
    let context = Context {
        control: control.clone(),
        events: Some(sender.clone()),
        decisions: Arc::new(move |info| {
            let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
            ask_sender
                .send(Event::Conflict(info, reply_sender))
                .context("界面已经关闭")?;
            loop {
                ask_control.check_cancelled()?;
                match reply_receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(reply) => return Ok(reply),
                    Err(mpsc::RecvTimeoutError::Timeout) => (),
                    Err(_) => anyhow::bail!("冲突选择窗口已关闭"),
                }
            }
        }),
    };
    let sender = sender.clone();
    // 同一次 GUI 会话里可能先分析再执行：覆盖对象必须可重复使用，不能 take。
    let overrides = state
        .borrow()
        .engine_overrides
        .as_ref()
        .map(|o| EngineTestOverrides {
            state_dir: o.state_dir.clone(),
            recycler: o.recycler.clone(),
        });
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if apply {
                // apply 分支在函数入口已校验任务库存在；工作线程内不使用 unwrap，
                // 若状态被并发改动则按错误返回，交给事件循环统一呈现。
                let Some(task_path) = task.as_deref() else {
                    anyhow::bail!("内部错误：执行阶段任务库缺失");
                };
                match &overrides {
                    Some(o) => engine::apply_with(task_path, context, o.recycler.clone()),
                    None => engine::apply(task_path, context),
                }
            } else {
                match &overrides {
                    // engine 已提供 prepare_with：测试注入的 recycler 必须传入，与 apply_with 对称。
                    Some(o) => engine::prepare_with(
                        &directory,
                        configuration,
                        context,
                        &o.state_dir,
                        o.recycler.clone(),
                    ),
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
        let _ = sender.send(event);
    });
}
/// 「递归解压」一段式启动（X-02）：一段确认后连续执行到结束；无计划审核环节，
/// 结束事件 ExtractDone 只收尾摘要，不改 ready/has_task（那是目录整理两段式的状态）。
fn start_extract(ui: &AppWindow, state: &Rc<RefCell<State>>, sender: &mpsc::SyncSender<Event>) {
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
        s.conflict = None;
        s.applying = false;
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
    ui.set_status("正在解压；成功原包按规则处置（默认回收站），失败原包移入「解压失败」".into());
    let ask_sender = sender.clone();
    let ask_control = control.clone();
    let context = Context {
        control: control.clone(),
        events: Some(sender.clone()),
        decisions: Arc::new(move |info| {
            let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
            ask_sender
                .send(Event::Conflict(info, reply_sender))
                .context("界面已经关闭")?;
            loop {
                ask_control.check_cancelled()?;
                match reply_receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(reply) => return Ok(reply),
                    Err(mpsc::RecvTimeoutError::Timeout) => (),
                    Err(_) => anyhow::bail!("冲突选择窗口已关闭"),
                }
            }
        }),
    };
    let sender = sender.clone();
    let overrides = state
        .borrow()
        .engine_overrides
        .as_ref()
        .map(|o| EngineTestOverrides {
            state_dir: o.state_dir.clone(),
            recycler: o.recycler.clone(),
        });
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match overrides {
            Some(overrides) => engine::extract_run_at(
                &directory,
                configuration,
                context,
                &overrides.state_dir,
                None,
                overrides.recycler,
            ),
            None => engine::extract_run(&directory, configuration, context),
        }));
        let event = match result {
            Ok(Ok(result)) => Event::ExtractDone(result.directory, result.summary),
            Ok(Err(error)) => Event::Failed(format!("{error:#}")),
            Err(_) => Event::Failed("解压线程意外退出；未执行的步骤不会继续".into()),
        };
        let _ = sender.send(event);
    });
}
/// 「开始解压」确认框文案（X-02）：数量由后台清点（大目录不阻塞界面），
/// 清点完成前先给占位文案，事件到达后原位更新。
fn extract_confirm_text(count_text: &str, directory: &str, config: &Config) -> String {
    let dispose = match config.archive_delete.resolve() {
        crate::config::DeleteMode::Keep => "保留不删",
        crate::config::DeleteMode::Recycle => "移入回收站",
        crate::config::DeleteMode::Permanent => "永久删除",
    };
    format!(
        "目标目录：{directory}

将递归解压压缩包（含嵌套包），就地解到各包所在位置。
{count_text}
成功完全解开的原包：{dispose}；未完全解开的原包：移入「解压失败」子目录等待人工处理。

{}",
        config.destructive_warning()
    )
}
fn show_error(ui: &AppWindow, error: impl std::fmt::Display) {
    ui.set_error_text(error.to_string().into());
}
/// 事件循环把日志写进界面环形缓冲的统一入口（U-10/S-07：界面仅保留最近 300 条）。
/// Failed 收尾与普通日志同走此路径，避免失败消息绕过条数上限。
fn push_event_log(logs: &mut VecDeque<String>, text: String) {
    if logs.len() >= 300 {
        logs.pop_front();
    }
    logs.push_back(text);
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
fn wire_sync(ui: &AppWindow, state: &Rc<RefCell<State>>, sender: &mpsc::SyncSender<Event>) {
    {
        let weak = ui.as_weak();
        let state = state.clone();
        // 目录每敲一键都同步重算就绪（打开任务 SQLite + 双侧序列化 + canonicalize），
        // 在网络盘/UNC 上会逐键阻塞 UI：去抖 300ms，停顿后才真正重算。
        let debounce = std::rc::Rc::new(slint::Timer::default());
        ui.on_root_edited({
            let weak = weak.clone();
            let state = state.clone();
            let debounce = debounce.clone();
            move || {
                if let Some(ui) = weak.upgrade() {
                    // 编辑目录立即失效执行按钮（旧同步行为的安全属性）：去抖窗口内不得凭
                    // 陈旧 ready 打开确认框；随后去抖重算，目录与计划仍一致时重新点亮。
                    ui.set_ready(false);
                    // 无任务时重算很轻（仅本地存在性检查），保持同步反馈；
                    // 有任务时的重算要开任务库+双侧序列化，网络盘上逐键卡顿，去抖 300ms。
                    if !ui.get_has_task() {
                        sync_ready_after_directory(&ui, &state.borrow());
                        return;
                    }
                    let weak = weak.clone();
                    let state = state.clone();
                    debounce.start(
                        slint::TimerMode::SingleShot,
                        Duration::from_millis(300),
                        move || {
                            if let Some(ui) = weak.upgrade() {
                                sync_ready_after_directory(&ui, &state.borrow());
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
        ui.on_select_tool(move |id| {
            if let Some(ui) = weak.upgrade() {
                // 点回工具时恢复全量列表：搜索过滤不得在切页后残留。
                reset_tool_list(&ui);
                if let Some(tool) = Tool::from_id(id.as_str()) {
                    let screen = match tool {
                        Tool::Extract => 2,
                        Tool::Organizer => 0,
                    };
                    ui.set_screen(screen);
                    ui.set_active_tool_id(id.clone());
                    {
                        let mut s = state.borrow_mut();
                        s.tool = tool;
                        // 切工具回到该工具的第一个规则分区；分区集合随工具变化（R-01）。
                        s.section = tool.sections()[0].to_string();
                    }
                    ui.set_section(0);
                    refresh(&ui, &state.borrow());
                    invalidate(&ui, tool);
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
                let numeric = state
                    .borrow()
                    .specs
                    .iter()
                    .find(|s| s.key == key.as_str())
                    .is_some_and(|s| s.kind == "number");
                let parsed = if numeric {
                    if let Ok(v) = value.parse::<u64>() {
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
                        show_error(&ui, "该设置需要输入非负整数");
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
                    // 联动行的可见性也取决于自身的值（如 large_threshold_gib 改回 1、
                    // custom_categories 改回默认）：与 on_rule_choice 同步可见性，增量插删不重建整表。
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
        let sender = sender.clone();
        ui.on_request_start(move || {
            if let Some(ui) = weak.upgrade() {
                // C-01：分析只读不改文件，无需破坏性确认；只有「执行计划」（kind=2）需要确认。
                start_task(&ui, &state, &sender, false);
            }
        });
    }
    {
        // X-02：解压是一段确认。先弹确认框（数量先占位），后台清点压缩包后原位更新文案。
        let weak = ui.as_weak();
        let state = state.clone();
        let sender = sender.clone();
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
                let count_text = state.borrow().config.destructive_warning();
                ui.set_confirm_text(
                    extract_confirm_text(
                        "正在清点压缩包…",
                        ui.get_directory().as_str(),
                        &state.borrow().config,
                    )
                    .into(),
                );
                let _ = count_text;
                ui.set_acknowledge(false);
                ui.set_confirm_kind(1);
                let sender = sender.clone();
                let config = {
                    let s = state.borrow();
                    s.config.clone()
                };
                std::thread::spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        engine::count_archives(&directory, &config)
                    }));
                    let event = match result {
                        Ok(Ok(count)) => Event::ExtractCount(Ok(count)),
                        Ok(Err(error)) => Event::ExtractCount(Err(format!("{error:#}"))),
                        Err(_) => Event::ExtractCount(Err("清点压缩包时后台操作意外退出".into())),
                    };
                    let _ = sender.send(event);
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
}

/// 初始界面状态：全部规则规格 + 默认配置（仅会话内存，不落盘）。测试与 run() 共用。
fn initial_state() -> Result<State> {
    let specs: Vec<RuleSpec> = serde_json::from_str(include_str!("../resources/rules.json"))?;
    Ok(State {
        config: Config::default(),
        specs,
        section: "去重".into(),
        task: None,
        control: None,
        conflict: None,
        logs: VecDeque::new(),
        page: 0,
        page_starts: vec![0],
        started: Instant::now(),
        close_after: false,
        pending_selection: 0,
        applying: false,
        planned: 0,
        plan_filter: None,
        selection_failed: false,
        show_advanced: false,
        tool: Tool::Organizer,
        engine_overrides: None,
        plan_load: Arc::new(PlanLoadSync::default()),
    })
}

/// 与 `run` 相同，但在事件循环启动前调用 `hook`——自动化测试用它安装驱动定时器，
/// 以真实回调路径驱动确认流，而不必访问任何内部状态。
pub fn run_with_pre_loop_hook(hook: impl FnOnce(&AppWindow) + 'static) -> Result<()> {
    run_with_engine_overrides(hook, None)
}

/// 允许测试注入状态目录与回收站实现；生产 GUI 走 `run()` / `run_with_pre_loop_hook`。
pub fn run_with_engine_overrides(
    hook: impl FnOnce(&AppWindow) + 'static,
    overrides: Option<EngineTestOverrides>,
) -> Result<()> {
    let ui = AppWindow::new()?;
    ui.set_system_dark(system_dark());
    let state = Rc::new(RefCell::new(initial_state()?));
    state.borrow_mut().engine_overrides = overrides;
    let (sender, receiver) = mpsc::sync_channel::<Event>(256);
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
        ui.on_choose_directory(move || {
            if let Some(ui) = weak.upgrade() {
                let mut dialog = rfd::FileDialog::new().set_title("选择需要整理的目录");
                // 已经输入过目录时从这里开始，省掉用户重新导航一遍。
                let entered = PathBuf::from(ui.get_directory().as_str());
                if entered.is_dir() {
                    dialog = dialog.set_directory(&entered);
                }
                if let Some(path) = dialog.pick_folder() {
                    ui.set_directory(path.display().to_string().into());
                    // 已有任务时按 recompute_ready 重算，而不是无条件失效（同一路径不应清掉计划）。
                    sync_ready_after_directory(&ui, &state.borrow());
                }
            }
        });
    }
    wire_sync(&ui, &state, &sender);
    // 启动落在注册表第一个工具（P-02 顺序：递归解压在前）。必须在 wire_sync 之后调用：
    // 回调接线前的 invoke 是空调用，窗口会停在目录整理页。
    ui.invoke_select_tool("recursive-extract".into());
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let sender = sender.clone();
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
                    ui.set_conflict_visible(false);
                } else if kind == 2 {
                    start_task(&ui, &state, &sender, true);
                } else if ui.get_screen() == 2 {
                    start_extract(&ui, &state, &sender);
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
                ui.set_status("正在取消；不会继续后续删除和移动".into());
                ui.set_conflict_visible(false);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_answer_conflict(move |index, all| {
            let policy = match index {
                0 => ConflictPolicy::Overwrite,
                1 => ConflictPolicy::Skip,
                2 => ConflictPolicy::Newest,
                3 => ConflictPolicy::Largest,
                _ => ConflictPolicy::KeepBoth,
            };
            if let Some(reply) = state.borrow_mut().conflict.take() {
                let _ = reply.send(ConflictAnswer {
                    policy,
                    apply_all: all,
                });
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_conflict_visible(false);
            }
        });
    }
    {
        let state = state.clone();
        let sender = sender.clone();
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
                }
                state.borrow_mut().pending_selection += 1;
                let sender = sender.clone();
                std::thread::spawn(move || {
                    // 与 async_work 一致地拦截 panic：否则 pending_selection 永远减不到 0，
                    // 会静默阻断后续的开始执行与历史载入。
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        Database::open_existing(&task).and_then(|db| db.set_selected(id, selected))
                    }));
                    let result = result
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("保存勾选时后台操作意外退出")));
                    let saved = result.is_ok().then_some((id, selected));
                    let _ = sender.send(Event::SelectionSaved(
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
        let sender = sender.clone();
        ui.on_plan_page(move |direction| {
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
                load_plan_filtered(
                    &sender,
                    &state.plan_load,
                    task,
                    start,
                    next,
                    state.plan_filter.clone(),
                );
            }
        });
    }
    {
        // 计划类型筛选：""=全部，否则 snake_case kind
        let state = state.clone();
        let sender = sender.clone();
        let weak = ui.as_weak();
        ui.on_filter_plan(move |kind| {
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
            if let Some(ui) = weak.upgrade() {
                ui.set_plan_prev_enabled(false);
                ui.set_plan_next_enabled(false);
                ui.set_plan_page_label("加载中…".into());
            }
            load_plan_filtered(&sender, &s.plan_load, task, 0, 0, s.plan_filter.clone());
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
            #[allow(clippy::cast_possible_truncation)]
            let drag_x = (drag.origin.0 + pointer.0 - drag.press.0)
                .clamp(f64::from(i32::MIN), f64::from(i32::MAX))
                .round() as i32;
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
    let timer = slint::Timer::default();
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let sender = sender.clone();
        // “跟随系统”主题需要感知运行期的系统深浅色切换；轮询注册表成本极低，每 2 秒一次。
        // 开机不足 10 秒时减法会下溢 panic，必须用 checked_sub 兜底。
        let last_theme_poll = Rc::new(std::cell::Cell::new(
            Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(Instant::now),
        ));
        let theme_poll = last_theme_poll.clone();
        timer.start(slint::TimerMode::Repeated,Duration::from_millis(100),move||{
            let Some(ui)=weak.upgrade()else{return;};
            if theme_poll.get().elapsed()>=Duration::from_secs(2){
                theme_poll.set(Instant::now());
                ui.set_system_dark(system_dark());
            }
            let mut log_changed=false;
            for event in receiver.try_iter().take(256){
                match event{
                    Event::Status(text)=>{if !ui.get_paused(){ui.set_status(text.into());}},
                    Event::Log(text)=>{let mut s=state.borrow_mut();push_event_log(&mut s.logs,text);log_changed=true;},
                    Event::Conflict(info,reply)=>{
                        // 冲突事件可能在用户已请求取消后才被本定时器处理：已取消的任务不再弹
                        // 冲突框，直接丢弃事件；reply 发送端随事件一起释放，worker 侧会在
                        // check_cancelled 或通道断开时自行退出，不会悬挂。
                        let cancelled={let s=state.borrow();s.control.as_ref().is_some_and(|control|control.is_cancelled())};
                        if cancelled{continue;}
                        state.borrow_mut().conflict=Some(reply);
                        let show=platform::display_path_text;
                        ui.set_conflict_text(format!("目标：{}\n\n现有文件：{} · 修改时间 {}\n新解压文件：{} · 修改时间 {}\n\n覆盖旧文件仍遵守已配置的回收站 / 永久删除策略；选择前不会继续后续解压。",
                            show(&info.existing),bytes(info.existing_size),platform::display_time_text(info.existing_time),
                            bytes(info.incoming_size),platform::display_time_text(info.incoming_time)).into());
                        ui.set_conflict_choice(4);ui.set_conflict_all(false);ui.set_conflict_visible(true);
                        ui.set_status("正在等待你选择解压冲突策略；选择前不会继续后续解压".into());
                    }
                    Event::Ready(path,summary)|Event::Done(path,summary)=>{
                        let ready=Database::open_existing(&path).and_then(|db|db.get::<String>("status")).is_ok_and(|v|v=="ready");
                        // 新计划一律回到「全部」筛选：沿用上一任务的筛选可能恰好计数为 0，
                        // 造成「空列表 + 高亮禁用胶囊」的死角。
                        let filter={let mut s=state.borrow_mut();s.task=Some(path.clone());s.page=0;s.page_starts=vec![0];s.conflict=None;s.control=None;
                         s.applying=false;s.planned=summary.planned_delete+summary.planned_move+summary.planned_link+summary.planned_empty;
                         s.plan_filter=None;None};
                        ui.set_busy(false);ui.set_paused(false);ui.set_conflict_visible(false);ui.set_ready(ready);ui.set_has_task(true);
                        ui.set_summary(summary.description().into());ui.set_panel(1);
                        ui.set_plan_delete_count(i32::try_from(summary.planned_delete).unwrap_or(i32::MAX));
                        ui.set_plan_move_count(i32::try_from(summary.planned_move).unwrap_or(i32::MAX));
                        ui.set_plan_link_count(i32::try_from(summary.planned_link).unwrap_or(i32::MAX));
                        ui.set_plan_empty_count(i32::try_from(summary.planned_empty).unwrap_or(i32::MAX));
                        ui.set_plan_filter(0);
                        // 任务结束后停止实时计时，改写最终统计，避免「耗时」空闲继续增长
                        ui.set_metrics(format!("扫描 {} 个文件 · 错误 {} 项 · 已回收 {} 项",
                            summary.scanned,summary.errors,summary.recycled).into());
                        ui.set_progress(-1.0);ui.set_progress_note("".into());
                        ui.set_plan_prev_enabled(false);ui.set_plan_next_enabled(false);
                        ui.set_status(if ready{
                            "分析完成（只读）。请检查计划，然后确认执行整理。".into()
                        }else{
                            format!("整理结束：已回收 {} 项 · 永久删除 {} 项 · 错误 {} 项；完整记录见「进度与日志」。",
                                summary.recycled,summary.deleted,summary.errors)
                        }.into());
                        // 新任务加载落地前清空上一任务的旧行：action id 是各任务库各自的
                        // rowid，旧行在此窗口内仍可交互，会把勾选写进新任务库的同 id 动作。
                        ui.set_plans(Rc::new(VecModel::from(Vec::<PlanRow>::new())).into());
                        load_plan_filtered(&sender,&state.borrow().plan_load,path,0,0,filter);
                        if state.borrow().close_after{let _=slint::quit_event_loop();}
                    }
                    Event::Failed(error)=>{
                        // 用户点了“取消任务”时不要用红色错误条报同一个消息：取消是预期操作，不是故障。
                        let cancelled={let s=state.borrow();s.control.as_ref().is_some_and(|control|control.is_cancelled())};
                        {let mut s=state.borrow_mut();s.control=None;s.conflict=None;push_event_log(&mut s.logs,error.clone());log_changed=true;}
                        ui.set_busy(false);ui.set_ready(false);ui.set_paused(false);ui.set_conflict_visible(false);
                        if cancelled{
                            ui.set_notice_text(error.into());
                            ui.set_status("任务已取消；已完成的操作不会自动回滚，详情见进度与日志".into());
                        }else{
                            ui.set_error_text(error.into());
                            ui.set_status("任务已停止；已完成的操作不会自动回滚，详情见进度与日志".into());
                        }
                        ui.set_progress(-1.0);ui.set_progress_note("".into());
                        // 失败/取消后计划与摘要可能已部分变化：从任务库重载摘要与当前计划页，
                        // 避免界面停留在失败前的旧数据（task 提取的借用安全见 reload_after_failed 注释）。
                        reload_after_failed(&ui,&state,&sender);
                        if state.borrow().close_after{let _=slint::quit_event_loop();}
                    }
                    Event::SelectionSaved(path,saved,error)=>{
                        let mut s=state.borrow_mut();s.pending_selection=s.pending_selection.saturating_sub(1);
                        if let Some(error)=error {ui.set_error_text(error.into());s.selection_failed=true;}
                        // 就地把该行勾选值改成数据库里的真实值：用户编辑会让 CheckBox 脱离
                        // `checked: item.selected` 绑定，只有这里回写模型才能保证界面与数据一致
                        //（无障碍/自动化切换时 Slint 不一定立即重绘，更需要这一步）。
                        // 只在当前展示的仍是该任务时回写：action id 是各任务库各自的 rowid，
                        // 载入别的任务后可能恰好出现相同 id，不能按 id 跨任务匹配。
                        let mut reload_page=false;let mut batch_failed=false;
                        if s.task.as_ref()==Some(&path){
                            if let Some((id,selected))=saved{
                                let plans=ui.get_plans();
                                if let Some(model)=plans.as_any().downcast_ref::<VecModel<PlanRow>>(){
                                    for i in 0..model.row_count(){
                                        if let Some(mut row)=model.row_data(i){
                                            if row.id.as_str()==id.to_string(){row.selected=selected;model.set_row_data(i,row);break;}
                                        }
                                    }
                                }
                            }
                            if s.pending_selection==0 && !ui.get_busy(){
                                // 失败可能发生在本轮任何一次勾选（不一定最后一个事件），只要
                                // pending 归零且出现过失败，就重载当前页让界面回到数据库真实状态。
                                batch_failed=std::mem::take(&mut s.selection_failed);
                                reload_page=batch_failed;
                            }
                        }
                        if s.task.as_ref()==Some(&path) && s.pending_selection==0 && !ui.get_busy(){
                            ui.set_ready(!batch_failed && recompute_ready(&ui,&s,&path));
                        }
                        if reload_page{
                            let start=s.page_starts.get(s.page).copied().unwrap_or(0);
                            let (page,filter)=(s.page,s.plan_filter.clone());
                            let plan_load=s.plan_load.clone();
                            load_plan_filtered(&sender,&plan_load,path,start,page,filter);
                        }
                    }
                    Event::PlanPage(path,mut actions,page,gen,filter)=>{
                        if state.borrow().task.as_ref()!=Some(&path){continue;}
                        let mut s=state.borrow_mut();
                        // 过期事件：不碰 completed，避免低代际事件取走/清掉更高代际结果后双方都被丢弃。
                        let latest=s.plan_load.latest.load(Ordering::Acquire);
                        if !plan_page_event_accepted(gen,latest,page,filter.as_deref(),s.page,s.plan_filter.as_deref()){continue;}
                        // 代际仍是最新：应用事件自带 actions；同代 completed 只做清理，不再 take 后丢弃。
                        {
                            let mut guard=s.plan_load.completed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            if guard.as_ref().is_some_and(|done|done.gen==gen){*guard=None;}
                        }
                        let more=actions.len()>100;actions.truncate(100);
                        if more&&s.page_starts.len()<=page+1{
                            if let Some(last)=actions.last(){s.page_starts.push(last.id);}
                        }
                        s.page=page;
                        // 只有真的还有上一页/下一页时才让按钮可用，避免点了没有任何反应。
                        ui.set_plan_prev_enabled(page>0);
                        ui.set_plan_next_enabled(more);
                        ui.set_plan_page_label(format!("第 {} 页 · 每页最多 100 条",page+1).into());
                        let rows=actions.into_iter().map(|a|PlanRow{id:a.id.to_string().into(),selected:a.selected,
                            kind:match a.kind{ActionKind::Delete=>"删除",ActionKind::Move=>"移动/重命名",ActionKind::Hardlink=>"硬链接",ActionKind::EmptyDirectory=>"空目录复查"}.into(),
                            source:a.source.into(),target:a.target.unwrap_or_else(||a.keeper.as_ref().map(|v|format!("保留 {}",v.0)).unwrap_or_default()).into(),reason:a.reason.into(),
                            state:match a.state.as_str(){"pending"=>"待执行","done"=>"已执行","skipped"=>"已跳过","unselected"=>"已取消勾选","failed"=>"执行失败",other=>other}.into()}).collect::<Vec<_>>();
                        ui.set_plans(Rc::new(VecModel::from(rows)).into());
                        // 页面重建后同步一次 ready：勾选保存失败触发重载时，不能让「开始执行」
                        // 因为一次瞬时数据库失败而一直禁用。
                        if !ui.get_busy() && s.pending_selection==0 && s.task.as_ref()==Some(&path){
                            ui.set_ready(recompute_ready(&ui,&s,&path));
                        }
                    }
                    Event::Notice(text)=>ui.set_notice_text(text.into()),
                    // Error 更新错误文案。
                    // 计划筛选曾乐观置「加载中…」：失败必须恢复分页控件，避免永久中间态。
                    Event::Error(text)=>{
                        ui.set_error_text(text.into());
                        if ui.get_has_task() && !ui.get_busy(){
                            let s=state.borrow();
                            let page=s.page;
                            ui.set_plan_prev_enabled(page>0);
                            ui.set_plan_next_enabled(page+1 < s.page_starts.len());
                            ui.set_plan_page_label(format!("第 {} 页 · 每页最多 100 条",page+1).into());
                        }
                    },
                    Event::PlanLoadFailed(text,gen,filter)=>{
                        // 计划加载失败带代际/筛选归属：请求已过期（用户切走筛选/翻页后
                        // 旧请求才失败）时不得把红条误报到当前正确视图上。
                        let accepted={let s=state.borrow();
                            s.plan_load.latest.load(Ordering::Acquire)==gen
                            && s.plan_filter.as_deref()==filter.as_deref()};
                        if accepted{ui.set_error_text(text.into());}
                    },
                    Event::ExtractCount(count)=>{
                        // 确认框仍开着（kind=1）才更新文案；用户已确认/返回则丢弃，不打扰运行中状态。
                        if ui.get_confirm_kind()==1&&ui.get_screen()==2{
                            match count{
                                Ok(n)=>ui.set_confirm_text(extract_confirm_text(
                                    &format!("清点到 {n} 个压缩包（按当前扫描范围，已排除「解压失败」目录）。"),
                                    ui.get_directory().as_str(),&state.borrow().config).into()),
                                Err(error)=>{
                                    // 清点失败：关闭确认框只留红条——占位文案下继续允许确认
                                    // 会让用户在数量未知的状态启动任务（X-02）。
                                    ui.set_confirm_kind(0);
                                    ui.set_error_text(error.into());
                                },
                            }
                        }
                    },
                    Event::ExtractDone(_path,summary)=>{
                        // X-02 一段式收尾：只清运行态与展示摘要；不改 ready/has_task（目录整理两段式专用）。
                        {let mut s=state.borrow_mut();s.control=None;s.conflict=None;}
                        ui.set_busy(false);ui.set_paused(false);ui.set_conflict_visible(false);
                        ui.set_quarantined_count(
                            i32::try_from(summary.archives_failed).unwrap_or(i32::MAX),
                        );
                        ui.set_progress(-1.0);ui.set_progress_note("".into());
                        ui.set_metrics(summary.extract_description().into());
                        ui.set_status(format!("解压结束：成功 {} 包；{} 包移入「解压失败」，详情见「进度与日志」。",
                            summary.archives_ok,summary.archives_failed).into());
                        if state.borrow().close_after{let _=slint::quit_event_loop();}
                    },
                }
            }
            if log_changed{ui.set_log_text(state.borrow().logs.iter().rev().cloned().collect::<Vec<_>>().join("\n").into());}
            let busy=ui.get_busy();
            let s=state.borrow();
            // 只在真正运行时刷新实时指标；空闲时不碰 metrics，避免耗时一直涨
            if busy {
                if let Some(control)=&s.control{
                    let read=control.read_bytes.load(Ordering::Relaxed);let elapsed=s.started.elapsed().as_secs_f64().max(0.001);
                    let done=control.completed.load(Ordering::Relaxed);let scanned=control.scanned.load(Ordering::Relaxed);
                    // 执行阶段不再扫描，写“扫描 0 个文件”只会让人误以为没扫到东西，这里只报执行进度。
                    if s.applying{
                        ui.set_metrics(format!("执行中：已处理 {} / {} 项 · 耗时 {:.1}s",done,s.planned,elapsed).into());
                    }else{
                        // 吞吐速率为显示用途，u64→f64 的精度损失无意义。
                        #[allow(clippy::cast_precision_loss)]
                        let rate_mib_s = read as f64 / elapsed / 1048576.0;
                        ui.set_metrics(format!("扫描 {} 个文件 · 读取 {} · 已处理 {} 个计划项 · 耗时 {:.1}s · 平均读取 {:.1} MiB/s",scanned,bytes(read),done,elapsed,rate_mib_s).into());
                    }
                    // 执行阶段按计划项计数；分析阶段总量未知（扫描/哈希/解压包大小不能提前预知）
                    if s.applying&&s.planned>0{
                        // 进度分数为显示用途，整数→浮点的精度损失无意义。
                        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                        let progress = (done as f64 / s.planned as f64).clamp(0.0, 1.0) as f32;
                        ui.set_progress(progress);
                        ui.set_progress_note(format!("{done} / {} 项",s.planned).into());
                    }else if s.applying{
                        // 执行阶段没有可执行的计划项（全部取消勾选或空计划）时，不要显示“已扫描 0 个文件”
                        // 让人误以为没扫到东西。
                        ui.set_progress(-1.0);ui.set_progress_note("没有待执行的计划项".into());
                    }else{
                        ui.set_progress(-1.0);
                        ui.set_progress_note(format!("已扫描 {scanned} 个文件 · 读取 {}",bytes(read)).into());
                    }
                }else{
                    ui.set_progress(-1.0);ui.set_progress_note("准备中".into());
                }
            }
        });
    }
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
    slint::run_event_loop()?;
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

    // 覆盖 C-11
    #[test]
    fn plan_page_event_rejects_stale_gen_without_touching_completed() {
        // 低代际事件晚到：不得因 gen!=latest 而应用；completed 仍留给高代际事件。
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
                planned_link: 0,
                planned_empty: 0,
                candidate_bytes: 0,
                deleted: 0,
                recycled: 0,
                moved: 0,
                linked: 0,
                skipped: 0,
                errors: 0,
                permanent_bytes: 0,
                recycled_bytes: 0,
            };
            db.set("summary", &summary).unwrap();
            drop(db);
            {
                let mut s = app.state.borrow_mut();
                s.task = Some(dir.clone());
            }
            let (tx, _rx) = mpsc::sync_channel::<crate::control::Event>(8);
            reload_after_failed(&app.ui, &app.state, &tx);
            let s = app.state.borrow();
            assert_eq!(s.planned, 3, "Failed 收尾应从任务库重载分母计数");
            assert!(app.ui.get_plan_delete_count() == 3, "摘要计数应刷新");
        })
        .unwrap();
    }
    struct GuiTestApp {
        ui: AppWindow,
        state: Rc<RefCell<State>>,
    }
    impl GuiTestApp {
        fn reset(&self) {
            *self.state.borrow_mut() = initial_state().unwrap();
            self.ui.set_ready(false);
            self.ui.set_error_text("".into());
            self.ui.set_notice_text("".into());
            self.ui.set_theme(0);
            self.ui.set_confirm_kind(0);
            self.ui.set_section(0);
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
                    let (dummy_tx, _) = mpsc::sync_channel::<Event>(1);
                    wire_sync(&ui, &state, &dummy_tx);
                    refresh(&ui, &state.borrow());
                    let app = GuiTestApp { ui, state };
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

    // 覆盖 P-02, P-04
    #[test]
    fn initial_surface_lists_defaults() {
        with_gui(|app| {
            let ui = &app.ui;
            assert!(!ui.get_ready(), "初始状态不得就绪");
            assert_eq!(ui.get_theme(), 0, "默认跟随系统主题");
            assert_eq!(ui.get_tool_count(), 2, "当前注册的工具数量");
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
                3,
                "安全与性能默认只显示 3 条基础规则"
            );
            assert!(
                rule_value_at(ui, "hash_workers").is_none(),
                "Hash 并行工作线程属于高级层"
            );
            ui.invoke_toggle_advanced(true);
            assert!(ui.get_show_advanced());
            assert_eq!(
                ui.get_rules().row_count(),
                7,
                "打开高级层后安全与性能应显示全部 7 条（reserve 属于递归解压）"
            );
            assert_eq!(rule_value_at(ui, "hash_workers").as_deref(), Some("2"));
            assert!(
                rule_value_at(ui, "reserve_gib").is_none(),
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
    // 覆盖 R-01
    #[test]
    fn advanced_rows_hidden_until_toggled() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("recursive-extract".into());
            ui.invoke_select_section(0); // 解压（递归解压：0解压/1安全与性能）
            assert_eq!(
                ui.get_rules().row_count(),
                2,
                "解压分区默认只显示 2 条基础规则（原包处置与冲突策略）"
            );
            assert!(
                rule_value_at(ui, "max_depth").is_none(),
                "最大嵌套层数属于高级层"
            );
            assert!(
                rule_value_at(ui, "nested_archives").is_none(),
                "嵌套解压属于高级层"
            );
            assert!(
                rule_value_at(ui, "dedup_same_name").is_none(),
                "去重规则不得出现在递归解压的面板"
            );
            ui.invoke_toggle_advanced(true);
            assert!(ui.get_show_advanced());
            assert_eq!(
                ui.get_rules().row_count(),
                9,
                "打开高级层后解压分区应显示全部 9 条"
            );
            assert_eq!(rule_value_at(ui, "max_depth").as_deref(), Some("16"));
            assert_eq!(
                rule_value_at(ui, "archive_delete").as_deref(),
                Some("recycle"),
                "X-05：成功原包默认移入回收站"
            );
            ui.invoke_toggle_advanced(false);
            assert_eq!(ui.get_rules().row_count(), 2, "关闭高级层必须恢复基础视图");
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
            assert!(
                rule_value_at(ui, "archive_delete").is_some(),
                "默认分区是「解压」"
            );
            // 安全是两工具共享分区：递归解压视角不含 Hash 线程（reserve 属于高级层，先展开）
            ui.invoke_select_section(1);
            ui.invoke_toggle_advanced(true);
            assert!(
                rule_value_at(ui, "hash_workers").is_none(),
                "Hash 线程属于目录整理"
            );
            assert!(
                rule_value_at(ui, "reserve_gib").is_some(),
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
                rule_value_at(ui, "archive_delete").is_none(),
                "解压规则不得出现在目录整理面板"
            );
        })
        .unwrap();
    }
    // 覆盖 R-01, R-02, C-02
    #[test]
    fn dependent_rows_follow_their_switches() {
        with_gui(|app| {
            let ui = &app.ui;
            // 清理：修正扩展名不再依赖独立的类型检测行，直接可见并自动联动开启
            ui.invoke_select_section(2);
            assert!(
                rule_value_at(ui, "fix_extension").is_some(),
                "修正扩展名始终可见"
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
            // 归类：自定义分类规则依赖归类方式；大文件阈值依赖大文件单独归类（已移入高级层）
            ui.invoke_select_section(1);
            assert!(rule_value_at(ui, "custom_categories").is_none());
            ui.invoke_rule_choice("classify".into(), 4); // 按自定义扩展名规则
            assert!(rule_value_at(ui, "custom_categories").is_some());
            assert!(rule_value_at(ui, "large_threshold_gib").is_none());
            ui.invoke_toggle_advanced(true);
            ui.invoke_rule_bool("large_files".into(), true);
            assert!(rule_value_at(ui, "large_threshold_gib").is_some());
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
            // conflict_delete 属于递归解压的「解压」分区高级层；目录整理任何分区都不出现。
            ui.invoke_select_tool("recursive-extract".into());
            ui.invoke_select_section(0);
            ui.invoke_toggle_advanced(true);
            assert!(
                rule_value_at(ui, "conflict_delete").is_some(),
                "解压覆盖旧文件的删除方式在解压高级层"
            );
            ui.invoke_toggle_advanced(false);
            ui.invoke_select_section(1);
            ui.invoke_toggle_advanced(true);
            assert!(
                rule_value_at(ui, "conflict_delete").is_none(),
                "安全分区不含解压覆盖删除方式行"
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
                    cfg.dedup_copy_names && cfg.dedup_other_names,
                    "副本名/不同名去重必须保持独立，不得被联动"
                );
            }
            ui.invoke_rule_bool("dedup_copy_names".into(), false);
            ui.invoke_rule_bool("dedup_other_names".into(), false);
            {
                let cfg = &app.state.borrow().config;
                assert!(
                    !cfg.dedup_copy_names && !cfg.dedup_other_names,
                    "两类开关各自独立生效"
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
    // ---- 纯函数回归：执行分母计数（不依赖 GUI 工作线程）----

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

    // 覆盖 X-02（解压一段确认：确认框必须就地解压去向、成功原包处置、失败去向与降级告知）
    #[test]
    fn extract_start_shows_single_confirmation_with_dispose_info() {
        with_gui(|app| {
            let ui = &app.ui;
            ui.invoke_select_tool("recursive-extract".into());
            let dir = temp_test_dir("extract-confirm");
            ui.set_directory(dir.display().to_string().into());
            ui.invoke_request_extract_start();
            assert_eq!(ui.get_confirm_kind(), 1, "解压是一段确认（kind=1）");
            let text = ui.get_confirm_text().to_string();
            assert!(
                text.contains("正在清点压缩包"),
                "数量先占位、后台清点后原位更新：{text}"
            );
            assert!(
                text.contains("就地解到各包所在位置"),
                "就地解压去向（X-03）：{text}"
            );
            assert!(
                text.contains("移入回收站"),
                "成功原包默认处置（X-05）：{text}"
            );
            assert!(
                text.contains("「解压失败」"),
                "失败原包去向（X-06）：{text}"
            );
            assert!(
                text.contains("回收失败后永久删除：已开启"),
                "S-02 降级默认开启，确认时必须明确告知：{text}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        })
        .unwrap();
    }

    // 覆盖 U-03（总量未知不得编造百分比：百分比文本与不定光带的声明锁定）
    #[test]
    fn progress_semantics_declared_in_ui() {
        let slint = include_str!("../ui/app.slint");
        assert!(
            slint.contains("if root.busy && root.progress >= 0: Text {"),
            "U-03：百分比文本只应在总量已知（progress>=0）时显示"
        );
        assert!(
            slint.contains("running: root.progress < 0;"),
            "U-03：总量未知时使用不定光带"
        );
        assert!(
            slint.contains(": \"正在处理，总量未知\""),
            "U-03：无障碍标签如实声明总量未知"
        );
    }

    // 覆盖 U-06（用户取消 MUST NOT 报成错误：取消走蓝条通知的声明锁定）
    #[test]
    fn cancel_notice_not_error_declared_in_ui() {
        let source = include_str!("gui.rs");
        assert!(
            source.contains("ui.set_notice_text(error.into());"),
            "U-06：取消必须走蓝条通知而不是红条错误"
        );
        assert!(
            source.contains("任务已取消；已完成的操作不会自动回滚"),
            "U-06：取消后的状态文案不得是错误口径"
        );
    }

    // 覆盖 U-09（运行中关窗必须弹「停止任务并关闭」确认并拦截关闭）
    #[test]
    fn busy_close_confirmation_declared_in_ui() {
        let source = include_str!("gui.rs");
        let slint = include_str!("../ui/app.slint");
        assert!(
            source.contains("if ui.get_busy(){"),
            "U-09：运行中关窗必须先进确认分支"
        );
        assert!(
            source.contains("CloseRequestResponse::KeepWindowShown"),
            "U-09：未确认前窗口不得关闭"
        );
        assert!(slint.contains("停止任务并关闭"), "U-09：确认框标题");
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

    // 覆盖 C-11（任务结束/取消后计划复选框不得再可点：只有"待执行"行可勾选）
    #[test]
    fn plan_checkbox_gated_by_row_state_declared_in_ui() {
        let slint = include_str!("../ui/app.slint");
        assert!(
            slint.contains(
                "enabled: !root.busy && root.confirm-kind == 0 && item.state == \"待执行\";"
            ),
            "C-11：计划复选框必须同时按行状态（待执行）门禁，避免已结束任务的行仍可点击必报错"
        );
    }
}
