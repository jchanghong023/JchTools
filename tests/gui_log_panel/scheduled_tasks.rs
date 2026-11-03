use super::*;

fn element(view: &View, id: &str) -> Option<ElementHandle> {
    ElementQuery::from_root(&view.ui).match_id(id).find_first()
}

fn page(view: &View) -> ElementHandle {
    let page = ElementQuery::from_root(&view.ui)
        .match_type_name("ScheduledTasksPage")
        .find_first()
        .expect("定时任务页面应有真实滚动视口");
    let position = page.absolute_position();
    let size = page.size();
    assert!(
        position.x >= 0.0
            && position.y >= 46.0
            && position.x + size.width <= view.pixels.width() as f32 + 0.5
            && position.y + size.height <= view.pixels.height() as f32 + 0.5,
        "页面视口不能超出窗口：{position:?}/{size:?}"
    );
    page
}

fn fully_inside(child: &ElementHandle, viewport: &ElementHandle) -> bool {
    let p = child.absolute_position();
    let s = child.size();
    let v = viewport.absolute_position();
    let vs = viewport.size();
    s.width > 0.0
        && s.height > 0.0
        && p.x >= v.x - 0.5
        && p.y >= v.y - 0.5
        && p.x + s.width <= v.x + vs.width + 0.5
        && p.y + s.height <= v.y + vs.height + 0.5
}

#[track_caller]
fn visible(view: &View, id: &str) -> ElementHandle {
    let child = element(view, id).unwrap_or_else(|| panic!("缺少可见控件 {id}"));
    let viewport = page(view);
    assert!(
        fully_inside(&child, &viewport),
        "控件 {id} 被视口裁切：控件 {:?}/{:?}，视口 {:?}/{:?}",
        child.absolute_position(),
        child.size(),
        viewport.absolute_position(),
        viewport.size()
    );
    child
}

fn reveal(view: &mut View, ids: &[&str]) {
    for _ in 0..40 {
        let viewport = page(view);
        if ids
            .iter()
            .all(|id| element(view, id).is_some_and(|child| fully_inside(&child, &viewport)))
        {
            return;
        }
        let p = viewport.absolute_position();
        let s = viewport.size();
        // 使用页面右侧边距，避免滚动指针落在多行编辑器内部。
        view.ui
            .window()
            .dispatch_event(WindowEvent::PointerScrolled {
                position: LogicalPosition::new(p.x + s.width - 12.0, p.y + s.height / 2.0),
                delta_x: 0.0,
                delta_y: -60.0,
            });
        view.render();
    }
    panic!("滚动页面后控件仍未完整进入视口：{ids:?}");
}

fn assert_row(view: &View, ids: &[&str]) {
    let controls: Vec<_> = ids.iter().map(|id| visible(view, id)).collect();
    for pair in controls.windows(2) {
        let left = pair[0].absolute_position();
        let right = pair[1].absolute_position();
        assert!(
            left.x + pair[0].size().width <= right.x + 0.5,
            "同排主要操作不能重叠：{:?}/{:?}，{:?}/{:?}",
            left,
            pair[0].size(),
            right,
            pair[1].size()
        );
    }
}

fn click_element(view: &mut View, id: &str) {
    let button = visible(view, id);
    let p = button.absolute_position();
    let s = button.size();
    view.click_at(LogicalPosition::new(
        p.x + s.width / 2.0,
        p.y + s.height / 2.0,
    ));
}

fn model(rows: Vec<Value>) -> Value {
    Value::Model(slint::ModelRc::new(slint::VecModel::from(rows)))
}

fn row(fields: &[(&str, Value)]) -> Value {
    Value::Struct(
        fields
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect(),
    )
}

/// ST-02/ST-03/ST-05、U-05：真实滚动视口中列表、完整高级配置和日志操作可达。
#[test]
fn scheduled_tasks_layout_at_minimum_and_standard_sizes() {
    with_review_remediation_view(|view| {
        view.set(
            "scheduled-rows",
            model(vec![row(&[
                ("id", Value::String("1".into())),
                ("name", Value::String("定期备份".into())),
                ("period", Value::String("每 1 小时".into())),
                ("enabled", Value::Bool(true)),
                ("running", Value::Bool(false)),
                ("state", Value::String("等待中".into())),
                ("result", Value::String("成功".into())),
                ("next-run", Value::String("2026-11-03 10:00".into())),
            ])]),
        );
        view.set(
            "scheduled-runs",
            model(vec![row(&[
                ("id", Value::String("10".into())),
                (
                    "summary",
                    Value::String("2026-11-03 09:00 · 手动 · 成功".into()),
                ),
            ])]),
        );
        view.set("scheduled-selected-run", Value::String("10".into()));
        view.set(
            "scheduled-log-text",
            Value::String("stdout: 完成\nstderr: 无".into()),
        );
        let saves = Rc::new(Cell::new(0));
        let exports = Rc::new(Cell::new(0));
        for (name, count) in [
            ("scheduled-save", saves.clone()),
            ("scheduled-export-log", exports.clone()),
        ] {
            view.ui
                .set_callback(name, move |_| {
                    count.set(count.get() + 1);
                    Value::Void
                })
                .unwrap();
        }

        for (width, height) in [(960, 620), (1120, 720)] {
            view.window.set_size(PhysicalSize::new(width, height));
            view.pixels = SharedPixelBuffer::new(width, height);
            // 重建页面以将各阶段的滚动位置归零，不模拟任何后台执行。
            view.set("screen", 0);
            view.set("scheduled-editor-open", false);
            view.set("scheduled-advanced", false);
            view.set("scheduled-log-open", false);
            view.render();
            view.set("screen", 9);
            view.render();
            visible(view, "ScheduledTasksPage::scheduled-new-button");
            visible(view, "ScheduledTasksPage::scheduled-task-name");
            assert_row(
                view,
                &[
                    "ScheduledTasksPage::scheduled-edit-button",
                    "ScheduledTasksPage::scheduled-delete-button",
                    "ScheduledTasksPage::scheduled-toggle-button",
                    "ScheduledTasksPage::scheduled-run-button",
                    "ScheduledTasksPage::scheduled-stop-button",
                    "ScheduledTasksPage::scheduled-logs-button",
                ],
            );
            view.screenshot(&format!("scheduled-list-{width}x{height}"));

            view.set("scheduled-editor-open", true);
            view.render();
            reveal(
                view,
                &[
                    "ScheduledTasksPage::scheduled-name-field",
                    "ScheduledTasksPage::scheduled-command-field",
                ],
            );
            reveal(
                view,
                &[
                    "ScheduledTasksPage::scheduled-directory-field",
                    "ScheduledTasksPage::scheduled-browse-button",
                ],
            );
            assert_row(
                view,
                &[
                    "ScheduledTasksPage::scheduled-directory-field",
                    "ScheduledTasksPage::scheduled-browse-button",
                ],
            );
            reveal(
                view,
                &[
                    "ScheduledTasksPage::scheduled-interval-field",
                    "ScheduledTasksPage::scheduled-unit-combo",
                ],
            );
            reveal(view, &["ScheduledTasksPage::scheduled-advanced-button"]);
            click_element(view, "ScheduledTasksPage::scheduled-advanced-button");
            reveal(view, &["ScheduledTasksPage::scheduled-shell-combo"]);
            visible(view, "ScheduledTasksPage::scheduled-shell-combo");
            reveal(
                view,
                &[
                    "ScheduledTasksPage::scheduled-timeout-field",
                    "ScheduledTasksPage::scheduled-save-button",
                    "ScheduledTasksPage::scheduled-cancel-button",
                ],
            );
            assert_row(
                view,
                &[
                    "ScheduledTasksPage::scheduled-save-button",
                    "ScheduledTasksPage::scheduled-cancel-button",
                ],
            );
            view.screenshot(&format!("scheduled-editor-advanced-{width}x{height}"));
            let before = saves.get();
            click_element(view, "ScheduledTasksPage::scheduled-save-button");
            assert_eq!(saves.get(), before + 1, "保存按钮应在滚动后可点击");

            view.set("screen", 0);
            view.set("scheduled-editor-open", false);
            view.set("scheduled-log-open", true);
            view.render();
            view.set("screen", 9);
            view.render();
            reveal(
                view,
                &[
                    "ScheduledTasksPage::scheduled-run-summary",
                    "ScheduledTasksPage::scheduled-select-run-button",
                ],
            );
            visible(view, "ScheduledTasksPage::scheduled-run-summary");
            reveal(
                view,
                &[
                    "ScheduledTasksPage::scheduled-copy-button",
                    "ScheduledTasksPage::scheduled-export-button",
                ],
            );
            assert_row(
                view,
                &[
                    "ScheduledTasksPage::scheduled-copy-button",
                    "ScheduledTasksPage::scheduled-export-button",
                ],
            );
            view.screenshot(&format!("scheduled-logs-{width}x{height}"));
            *view.state.clipboard.borrow_mut() = None;
            click_element(view, "ScheduledTasksPage::scheduled-copy-button");
            assert_eq!(
                view.state.clipboard.borrow().as_deref(),
                Some("stdout: 完成\nstderr: 无"),
                "复制按钮应全选真实日志并写入剪贴板"
            );
            let before = exports.get();
            click_element(view, "ScheduledTasksPage::scheduled-export-button");
            assert_eq!(exports.get(), before + 1, "导出按钮应在滚动后可点击");
        }
    });
}

/// ST-03/U-09：确认层打开前已聚焦的任务草稿不能接收穿透键盘事件。
#[test]
fn scheduled_tasks_confirmation_blocks_keyboard_input() {
    with_review_remediation_view(|view| {
        let actions = Rc::new(Cell::new(0));
        for name in ["scheduled-new", "scheduled-save"] {
            let actions = actions.clone();
            view.ui
                .set_callback(name, move |_| {
                    actions.set(actions.get() + 1);
                    Value::Void
                })
                .unwrap();
        }
        view.set("screen", 9);
        view.set("scheduled-editor-open", true);
        view.set("scheduled-name", Value::String("draft".into()));
        view.render();

        click_element(view, "ScheduledTasksPage::scheduled-name-field");
        view.key("a");
        let Value::String(draft) = view.ui.get_property("scheduled-name").unwrap() else {
            panic!("任务名称应为字符串");
        };
        assert_ne!(draft.to_string(), "draft", "输入前必须真实聚焦任务草稿");

        view.set("confirm-kind", 3);
        view.render();
        assert!(view.has_button("停止并关闭"), "安全确认应处于打开状态");
        for key in [" ", "\n", "x"] {
            view.key(key);
            let Value::String(current) = view.ui.get_property("scheduled-name").unwrap() else {
                panic!("任务名称应为字符串");
            };
            assert_eq!(current, draft, "确认层打开时键盘不能修改底层任务草稿");
            assert_eq!(actions.get(), 0, "确认层打开时键盘不能触发任务操作");
            assert!(view.has_button("停止并关闭"), "键盘不能意外关闭确认层");
        }

        view.click("返回检查");
        assert!(!view.has_button("停止并关闭"));
        click_element(view, "ScheduledTasksPage::scheduled-name-field");
        view.key("b");
        let Value::String(after) = view.ui.get_property("scheduled-name").unwrap() else {
            panic!("任务名称应为字符串");
        };
        assert_ne!(after, draft, "关闭确认后任务草稿应重新接受输入");
        click_element(view, "ScheduledTasksPage::scheduled-new-button");
        assert_eq!(actions.get(), 1, "关闭确认后任务操作应恢复");
    });
}
