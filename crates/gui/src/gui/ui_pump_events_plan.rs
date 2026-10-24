//! 界面事件泵的计划页事件处理器：勾选保存回执、依赖重算、计划分页与就绪快照的代际门禁及上屏（自 ui_pump_events.rs 拆分）。
use super::{
    apply_plan_readiness, apply_summary, load_plan_state, organizer_engine,
    plan_page_event_accepted, plan_row_state, ActionKind, AppWindow, Event, Ordering, Path,
    PathBuf, PlanQuery, PlanRow, PlanSnapshot, Rc, UiPump, VecModel,
};
use jchtools_file_core::model::Action;

impl UiPump {
    /// Event::SelectionSaved 的处理：只接受当前任务的选择保存回执；本批次全部落库后
    /// 按数据库实际提交的选择发起依赖重算（C-01）。
    pub(super) fn handle_selection_saved(
        &self,
        ui: &AppWindow,
        path: PathBuf,
        error: Option<String>,
    ) {
        let mut s = self.state.borrow_mut();
        if s.task.as_ref() != Some(&path) {
            return;
        }
        s.pending_selection = s.pending_selection.saturating_sub(1);
        if let Some(error) = error {
            ui.set_error_text(error.into());
        }
        if s.pending_selection == 0 && s.plan_recompute_inflight == 0 {
            // 部分保存失败也针对数据库实际提交的选择重算依赖。
            s.plan_page_pending = true;
            s.plan_load.page.fetch_add(1, Ordering::AcqRel);
            s.plan_load.state.fetch_add(1, Ordering::AcqRel);
            s.plan_recompute_inflight = 1;
            ui.set_ready(false);
            ui.set_plan_editable(false);
            let out = self.out.clone();
            std::thread::spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    organizer_engine::recompute_plan(&path)
                }));
                let result = match result {
                    Ok(Ok(cancelled)) => Ok(cancelled),
                    Ok(Err(error)) => Err(format!("重算受影响计划失败：{error:#}")),
                    Err(_) => Err("重算受影响计划时后台操作意外退出".into()),
                };
                let _ = out.send(Event::PlanRecomputed(path, result));
            });
        }
    }

    /// Event::PlanRecomputed 的处理：重算在途计数回退；失败须重新分析（不得以旧计划
    /// 继续执行），成功后在其余批次同样落定后按 DB 真值重载计划页。
    pub(super) fn handle_plan_recomputed(
        &self,
        ui: &AppWindow,
        path: PathBuf,
        result: Result<usize, String>,
    ) {
        let mut s = self.state.borrow_mut();
        if s.task.as_ref() != Some(&path) {
            return;
        }
        s.plan_recompute_inflight = s.plan_recompute_inflight.saturating_sub(1);
        s.plan_page_pending = true;
        let cancelled = match result {
            Ok(cancelled) => cancelled,
            Err(error) => {
                s.plan_recompute_failed = true;
                ui.set_ready(false);
                ui.set_error_text(error.into());
                return;
            }
        };
        if s.pending_selection != 0 || s.plan_recompute_inflight != 0 {
            return;
        }
        if cancelled > 0 && !ui.get_busy() {
            ui.set_status(
                format!("已按取消的勾选重算：{cancelled} 个依赖项失效，转为未勾选").into(),
            );
        }
        // cancelled=0 也重载：连续勾选及失败行均以 DB 真值呈现。
        load_plan_state(
            &self.out,
            &s.plan_load,
            path,
            ui.get_directory().to_string(),
            PlanQuery::page(
                s.page_starts.get(s.page).copied().unwrap_or(0),
                s.page,
                s.plan_filter.clone(),
                false,
            ),
        );
    }

    /// Event::PlanPage 的处理：页面代际/筛选门禁、分页按钮与行呈现、快照摘要与就绪
    /// 同步；独立快照先于新页到达时按就绪代际补查当前目录。
    // PlanPage 载荷与 Event::PlanPage 变体字段一一对应，按协议字段传参而非另建打包结构。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn handle_plan_page(
        &self,
        ui: &AppWindow,
        path: PathBuf,
        mut actions: Vec<Action>,
        page: usize,
        gen: u64,
        filter: Option<&str>,
        state_gen: u64,
        snapshot: Option<&PlanSnapshot>,
    ) {
        if self.state.borrow().task.as_ref() != Some(&path) {
            return;
        }
        let mut s = self.state.borrow_mut();
        if s.pending_selection != 0 || s.plan_recompute_inflight != 0 {
            return;
        }
        // 过期事件：页面代际须仍是当前请求（只取快照的请求不动页面代际）。
        let latest = s.plan_load.page.load(Ordering::Acquire);
        if !plan_page_event_accepted(gen, latest, filter, s.plan_filter.as_deref()) {
            return;
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
        s.plan_page_pending = snapshot.is_none();
        // 失败收尾的请求顺带取回摘要：与页面同一次开库，界面线程不再自己开库。
        if let Some(summary) = snapshot.and_then(|snapshot| snapshot.summary.as_ref()) {
            apply_summary(ui, &mut s, summary);
        }
        // 页面重建后同步一次就绪：勾选保存失败触发重载时，不能让「开始执行」
        // 因为一次瞬时数据库失败而一直禁用。快照按当前配置/目录纯比较判定。
        if !ui.get_busy()
            && s.pending_selection == 0
            && s.plan_recompute_inflight == 0
            && !s.plan_recompute_failed
            && s.task.as_ref() == Some(&path)
        {
            apply_plan_readiness(ui, &s, snapshot);
        }
        // 独立快照若先于新页到达，会被呈现门禁拒绝；显示后补查当前目录。
        if !ui.get_busy() && s.plan_load.state.load(Ordering::Acquire) != state_gen {
            load_plan_state(
                &self.out,
                &s.plan_load,
                path,
                ui.get_directory().to_string(),
                PlanQuery::readiness(),
            );
        }
    }

    /// Event::PlanState 的处理：就绪快照按请求代际与运行/勾选在途门禁决定是否上屏。
    pub(super) fn handle_plan_state(
        &self,
        ui: &AppWindow,
        path: &Path,
        gen: u64,
        snapshot: Option<&PlanSnapshot>,
    ) {
        let s = self.state.borrow();
        if s.task.as_deref() != Some(path) {
            return;
        }
        // 过期快照：更新的请求在途，低代际结果不得改写当前就绪状态。
        if s.plan_load.state.load(Ordering::Acquire) != gen {
            return;
        }
        // 运行中与勾选在途：状态栏文案归任务事件与勾选流程，就绪重算不得改写。
        if ui.get_busy()
            || s.pending_selection != 0
            || s.plan_recompute_inflight != 0
            || s.plan_recompute_failed
            || s.plan_page_pending
        {
            return;
        }
        apply_plan_readiness(ui, &s, snapshot);
    }

    /// Event::PlanLoadFailed 的处理：过期请求的失败不上屏。
    pub(super) fn handle_plan_load_failed(
        &self,
        ui: &AppWindow,
        text: String,
        gen: u64,
        filter: Option<&str>,
    ) {
        // 计划加载失败带代际/筛选归属：请求已过期（用户切走筛选/翻页后
        // 旧请求才失败）时不得把红条误报到当前正确视图上。
        let accepted = {
            let s = self.state.borrow();
            s.plan_load.page.load(Ordering::Acquire) == gen && s.plan_filter.as_deref() == filter
        };
        if accepted {
            self.state.borrow_mut().plan_page_pending = true;
            ui.set_ready(false);
            ui.set_plan_editable(false);
            ui.set_error_text(text.into());
        }
    }
}
