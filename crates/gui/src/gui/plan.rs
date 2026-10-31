//! 计划页与就绪快照：后台取数请求、快照分类与计划行显示辅助（自 gui.rs 拆分）。
use super::{
    invalidate, patch_rule_row, AppWindow, Arc, Config, Database, Event, EventSender, Ordering,
    Path, PathBuf, PlanLoadSync, PlanRow, PlanSnapshot, Rc, RefCell, Result, State, Tool, VecModel,
};
use slint::Model;

/// 计划页请求参数：起始 action id、页码与筛选（`None`=全部）。
pub(super) struct PlanPageRequest {
    pub(super) start: i64,
    pub(super) page: usize,
    pub(super) filter: Option<String>,
}

/// 计划 worker 的取数口径：页面、就绪快照与摘要都取自同一次开库（界面线程不开库，H-02）。
pub(super) struct PlanQuery {
    /// `None`=只取就绪快照，不重载计划页（切工具、目录编辑、勾选保存后重算就绪）
    pub(super) page: Option<PlanPageRequest>,
    /// 连摘要一起取：失败收尾复用同一次开库，界面线程不再自己开库读摘要
    pub(super) summary: bool,
}

impl PlanQuery {
    /// 只取就绪快照，不重载计划页。
    pub(super) fn readiness() -> Self {
        Self {
            page: None,
            summary: false,
        }
    }
    /// 取指定页 + 就绪快照；`summary` 为真时同一次开库带回摘要（失败收尾用）。
    pub(super) fn page(start: i64, page: usize, filter: Option<String>, summary: bool) -> Self {
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
    Page(PlanSnapshot, Vec<jchtools_file_core::model::Action>),
    State(PlanSnapshot),
}

/// 发起计划页/就绪快照加载：worker 线程开库取数，界面线程只等结果。
/// 目录原文在界面线程读一次随请求带走：快照落地时界面据此判断是否仍对应当前输入，
/// 因此界面线程既不打开任务库，也不做 canonicalize（H-02：网络盘与大目录不卡界面）。
pub(super) fn load_plan_state(
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
            // 页面与就绪判定必须读取同一只读事务中的数据库选择。
            db.conn.execute_batch("BEGIN DEFERRED")?;
            let snapshot = plan_snapshot(&db, &directory, summary);
            let loaded = match page_start {
                Some(start) => PlanLoaded::Page(
                    snapshot,
                    db.actions_page_filtered(start, 101, page_filter.as_deref())?,
                ),
                None => PlanLoaded::State(snapshot),
            };
            db.conn.execute_batch("COMMIT")?;
            Ok::<_, anyhow::Error>(loaded)
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
pub(super) fn plan_snapshot(db: &Database, directory: &str, with_summary: bool) -> PlanSnapshot {
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
pub(super) enum PlanReadyState {
    Ready,
    Changed,
    Finished,
    Unavailable,
}

impl PlanReadyState {
    /// 状态栏文案：不谎报改动，也不把读不到库说成任务已结束。
    pub(super) fn status_text(self) -> &'static str {
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
pub(super) fn classify_plan_snapshot(
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
pub(super) fn apply_plan_readiness(ui: &AppWindow, state: &State, snapshot: Option<&PlanSnapshot>) {
    let outcome = classify_plan_snapshot(snapshot, ui.get_directory().as_str(), &state.config);
    ui.set_plan_editable(outcome.editable());
    ui.set_ready(
        matches!(outcome, PlanReadyState::Ready)
            && invalid_number_error(state, Tool::Organizer).is_none()
            && !state.plan_page_pending
            && state.pending_selection == 0
            && state.plan_recompute_inflight == 0
            && !state.plan_recompute_failed,
    );
    if state.tool == Tool::Organizer && state.readiness_status_pending.replace(false) {
        ui.set_status(outcome.status_text().into());
    }
}

/// 附录 D：仅当前工具实际启用的数字字段参与门禁。
pub(super) fn invalid_number_error(state: &State, tool: Tool) -> Option<String> {
    let owner = match tool {
        Tool::Extract => "extract",
        Tool::Organizer => "organizer",
        Tool::Md | Tool::Git | Tool::MarkdownConverter | Tool::SnapOcr | Tool::CodeAgent => {
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

pub(super) fn retain_invalid_number(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    key: &str,
    value: &str,
) {
    let error = {
        let mut s = state.borrow_mut();
        s.invalid_rule_inputs
            .insert(key.to_string(), value.to_string());
        invalid_number_error(&s, s.tool)
    };
    patch_rule_row(ui, key, |row| row.value = value.into());
    invalidate(ui, &state.borrow());
    ui.set_error_text(error.unwrap_or_default().into());
}

/// 计划页事件是否可应用：代际须仍是 latest，且 filter 与当前视图一致。
/// page 不再要求与 UI 预置值一致：翻页采用「先加载、成功再提交」，加载期间 state.page 仍是旧页。
pub(super) fn plan_page_event_accepted(
    event_gen: u64,
    latest: u64,
    event_filter: Option<&str>,
    ui_filter: Option<&str>,
) -> bool {
    event_gen == latest && event_filter == ui_filter
}

/// 执行阶段的进度分母：只统计仍勾选且待执行（selected=1 且 state='pending'）的计划项，
/// 用户取消勾选的项不计入。计数失败时返回 Err，调用方保留全量分母继续执行。
pub(super) fn count_selected_pending(task: &Path) -> Result<u64> {
    let db = Database::open_existing(task)?;
    let n: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM actions WHERE selected=1 AND state='pending'",
        [],
        |r| r.get(0),
    )?;
    // COUNT(*) 恒非负，max(0) 仅防御损坏库；转换在 64 位平台无损。
    Ok(u64::try_from(n.max(0)).unwrap_or(0))
}

/// C-11：计划行状态的中文显示。未勾选且尚未执行的计划行按勾选实际状态显示
/// 「已取消勾选」，重新勾选恢复「待执行」（勾选即时生效，不必等执行阶段）。
/// 任务库存英文状态（done/skipped/failed，engine 执行后回写），读取链在此翻译成中文；
/// 旧任务库可能已存中文，别名一并保留。终态行不会落入「待执行/已取消勾选」，
/// 与 ui/app.slint 的勾选门禁（只认这两种状态可勾选）保持一致。
pub(super) fn plan_row_state(selected: bool, stored: &str) -> &str {
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
pub(super) fn patch_plan_row(ui: &AppWindow, id: i64, selected: bool) {
    mutate_plan_row(ui, id, |row| {
        row.selected = selected;
        row.state = plan_row_state(selected, row.state.as_str()).into();
    });
}
