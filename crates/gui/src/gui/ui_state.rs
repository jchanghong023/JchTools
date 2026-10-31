//! 界面状态同步：目录 probe、invalidate、主题、范围提示与日志面板（自 gui.rs 拆分）。
#[cfg(test)]
use super::gui_tests;
use super::{
    load_plan_state, registry, wake_event_loop, AppWindow, Arc, Config, EventSender,
    IdleDirectoryMessage, IdleDirectoryRequest, Path, PlanQuery, Rc, RefCell, State, Tool, ToolRow,
    VecDeque, VecModel,
};

fn idle_directory_is_dir(path: &Path) -> bool {
    #[cfg(test)]
    {
        let probe = gui_tests::review_remediation_directory_probes()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .cloned();
        if let Some(probe) = probe {
            return probe();
        }
    }
    path.is_dir()
}

pub(super) fn invalidate_idle_directory(state: &State) -> u64 {
    let generation = state.idle_directory_generation.get().wrapping_add(1);
    state.idle_directory_generation.set(generation);
    state
        .idle_directory_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pending = None;
    generation
}

fn start_idle_directory_probe(ui: &AppWindow, state: &State, generation: u64) {
    ui.set_status("正在检查目录…".into());
    let queue = Arc::clone(&state.idle_directory_queue);
    {
        let mut queued = queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queued.pending = Some(IdleDirectoryRequest {
            generation,
            directory: ui.get_directory().to_string(),
            tool: state.tool,
        });
        if queued.running {
            return;
        }
        queued.running = true;
    }
    let sender = state.idle_directory_sender.clone();
    std::thread::spawn(move || loop {
        let request = {
            let mut queued = queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(request) = queued.pending.take() {
                request
            } else {
                queued.running = false;
                return;
            }
        };
        let exists = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            idle_directory_is_dir(Path::new(&request.directory))
        }))
        .unwrap_or(false);
        if sender
            .send(IdleDirectoryMessage { request, exists })
            .is_err()
        {
            return;
        }
        wake_event_loop();
    });
}

pub(super) fn apply_idle_directory_messages(ui: &AppWindow, state: &State) {
    for message in state.idle_directory_receiver.borrow_mut().try_iter() {
        let request = message.request;
        if ui.get_busy()
            || ui.get_status() != "正在检查目录…"
            || request.generation != state.idle_directory_generation.get()
            || request.directory != ui.get_directory().as_str()
            || request.tool != state.tool
            || (state.tool == Tool::Organizer && ui.get_has_task())
        {
            continue;
        }
        ui.set_status(
            if message.exists {
                match request.tool {
                    Tool::Extract => "目录已就绪；点「开始解压」后会先弹一次确认",
                    Tool::Organizer => "目录已就绪；「分析」只读不改文件，随时可以开始",
                    Tool::Md | Tool::Git => "目录已就绪",
                    _ => continue,
                }
            } else {
                "目录不存在或无法访问，请检查路径"
            }
            .into(),
        );
    }
}

pub(super) fn invalidate(ui: &AppWindow, state: &State) {
    let generation = invalidate_idle_directory(state);
    let tool = state.tool;
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
        start_idle_directory_probe(ui, state, generation);
        return;
    }
    // U-11/C-11：MD 整理与 Git 工具没有「分析→计划→执行」流程，状态栏必须用本工具
    // 中性文案，不得串用目录整理的流程词（分析/整理/规则）误导用户。
    if matches!(tool, Tool::Md | Tool::Git) {
        if ui.get_directory().is_empty() {
            ui.set_status("请选择需要处理的目录".into());
            return;
        }
        start_idle_directory_probe(ui, state, generation);
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
    if tool == Tool::CodeAgent {
        ui.set_status("CodeAgent 在独立后台维护本地 Token，关闭窗口不停止检查".into());
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
    start_idle_directory_probe(ui, state, generation);
}

/// 目录输入变化与切换工具共用的就绪同步：已有任务时按「任务 + 配置 + 目录」在后台重算
/// （开任务库与 canonicalize 都在 worker 线程，见 `load_plan_state`），而不是一律 invalidate
/// ——用户可能只是重新输入了同一路径或切去看了一眼另一个工具，不应清掉仍可执行的计划。
/// 结果落地时按请求代际与「当前目录/配置」筛选，并给真实状态文案（C-11 如实展示）。
pub(super) fn sync_ready_after_directory(ui: &AppWindow, state: &State, out: &EventSender) {
    // 同 invalidate 的 busy 门禁：运行中的状态栏归任务事件管，目录输入与工具切换
    // 不得触发就绪重算把执行中任务改写为「已结束/可执行」。
    if ui.get_busy() {
        invalidate_idle_directory(state);
        return;
    }
    if ui.get_has_task() && state.tool == Tool::Organizer {
        if let Some(task) = state.task.clone() {
            ui.set_ready(false);
            invalidate_idle_directory(state);
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
    invalidate(ui, state);
}

pub(super) fn apply_theme(ui: &AppWindow, state: &State) {
    ui.set_theme(match state.config.theme.as_str() {
        "light" => 1,
        "dark" => 2,
        _ => 0,
    });
}

/// 「GUI 有活动」的统一判定：任务运行、转 Markdown 初始化/预检/保存收尾或
/// 截图 OCR 初始化任一进行中即为有活动（五个界面 getter 的合取）。新增运行
/// 阶段标志时只需在此扩展一处，关窗守卫与各空闲判定自动同步，不会漏掉阶段。
pub(super) fn gui_activity_in_flight(ui: &AppWindow) -> bool {
    ui.get_busy()
        || ui.get_convert_initializing()
        || ui.get_convert_preparing()
        || ui.get_convert_runtime_saving()
        || ui.get_snap_initializing()
}

/// Event::Failed 收尾：任务库仍在时重载摘要与计划页，避免界面停留在失败前的旧数据。
/// 摘要与计划页取自同一次后台开库（`PlanQuery::page(..., summary=true)`），界面线程不开库
/// （H-02：失败收尾也要立刻画出状态，不能被磁盘/网络盘卡住）。
/// 注意 task 必须先用普通 let 从 state 提取：edition 2021 下 if-let scrutinee 的
/// borrow() Ref 临时存活到整个语句结束，块内 borrow_mut 会 BorrowMutError panic
/// ——任务取消/失败且有任务时 100% 触发（回归见 gui_tests）。
pub(super) fn reload_after_failed(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    // task 先用普通 let 提取后再 if-let：scrutinee 里的 borrow() Ref 不会存活到块内，
    // 块内的 borrow_mut 与共享借用才安全（缺陷模式见回归测试 failed_reload_*）。
    let task = state.borrow().task.clone();
    if let Some(task) = task {
        let (start, page, filter, plan_load) = {
            let mut s = state.borrow_mut();
            s.plan_page_pending = true;
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
pub(super) fn refresh_scope_notice(ui: &AppWindow, config: &Config) {
    match scope_notice(config) {
        Some(text) => ui.set_notice_text(text.into()),
        None => {
            if ui.get_notice_text().starts_with(SCOPE_NOTICE_PREFIX) {
                ui.set_notice_text("".into());
            }
        }
    }
}

/// 事件循环把日志写进界面环形缓冲的统一入口（U-10/S-07：界面仅保留最近 300 条）。
/// Failed 收尾与普通日志同走此路径，避免失败消息绕过条数上限。
pub(super) fn push_event_log(logs: &mut VecDeque<String>, text: String) {
    if logs.len() >= 300 {
        logs.pop_front();
    }
    logs.push_back(text);
}

/// 日志面板当前是否可见：「进度与日志」在目录整理页是 panel 2、在递归解压页是 panel 1
/// （ui/app.slint 两处 `if root.panel == …` 的条件元素）。不可见时条件子树根本没实例化，
/// 重建 300 行文本纯属浪费——事件循环据此只在可见时上屏（见轮询尾部的 log_dirty）。
pub(super) fn log_panel_visible(ui: &AppWindow) -> bool {
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
pub(super) fn log_panel_text(logs: &VecDeque<String>) -> String {
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
pub(super) fn reset_tool_list(ui: &AppWindow) {
    let tools = full_tool_rows();
    ui.set_tools(Rc::new(VecModel::from(tools)).into());
    ui.set_tool_search("".into());
}

/// 注册表全量工具行：run_gui 初始装配、无头测试装配与 reset_tool_list 同源复用，
/// registry::ToolDescriptor 或 ToolRow 增删字段只需改下方 From 实现一处。
pub(super) fn full_tool_rows() -> Vec<ToolRow> {
    registry::tools().iter().map(ToolRow::from).collect()
}

// ToolRow 来自 jchtools_ui（外部类型），&registry::ToolDescriptor 是本地类型的
// fundamental 引用包裹，孤儿规则允许本 crate 为其实现 From。
impl From<&registry::ToolDescriptor> for ToolRow {
    fn from(tool: &registry::ToolDescriptor) -> Self {
        ToolRow {
            id: tool.id.into(),
            name: tool.name.into(),
            summary: tool.summary.into(),
        }
    }
}
