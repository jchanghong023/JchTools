use super::*;

// 覆盖 M-08：非法上限时实际主按钮须禁用。
#[test]
fn gui_findings_md_zero_limit_disables_start_button() {
    with_review_remediation_view(|view| {
        // 解释器窗口只装配显示层；精确换算另由 GUI 的真实回调测试覆盖。
        view.ui
            .set_callback("md-split-size-error", |args| match &args[0] {
                Value::String(value) if value.as_str() == "0" => {
                    Value::String("拆分大小必须大于 0".into())
                }
                _ => Value::String("".into()),
            })
            .unwrap();
        view.set("screen", 3);
        view.set("md-subpage", 1);
        view.set("md-split-file", Value::String("input.md".into()));
        view.set("md-split-dir", Value::String("output".into()));
        view.set("md-split-size", Value::String("0".into()));
        view.render();
        assert_eq!(view.button("开始拆分").accessible_enabled(), Some(false));
        assert!(
            ElementHandle::find_by_accessible_label(&view.ui, "拆分大小必须大于 0")
                .next()
                .is_some()
        );
        view.set("md-split-size", Value::String("1".into()));
        view.render();
        assert_eq!(view.button("开始拆分").accessible_enabled(), Some(true));
    });
}

// 覆盖 T-24 / U-06：失败状态必须由实际渲染颜色区分，不能仍呈绿色成功点。
#[test]
fn gui_findings_conversion_failure_indicator_is_not_green() {
    with_review_remediation_view(|view| {
        view.set("screen", 5);
        view.set("convert-ready", true);
        view.set("convert-status", Value::String("转换部分失败".into()));
        view.render();
        let dots: Vec<_> = ElementQuery::from_root(&view.ui)
            .match_descendants()
            .match_type_name("Dot")
            .find_all();
        let dot = dots
            .iter()
            .find(|item| item.type_name().as_deref() == Some("Dot"))
            .unwrap_or(&dots[0]);
        let pixels = view.pixels_in(dot);
        assert!(
            !pixels
                .chunks_exact(3)
                .any(|p| i16::from(p[1]) > i16::from(p[0]) + 40
                    && i16::from(p[1]) > i16::from(p[2]) + 40),
            "转换部分失败仍渲染绿色状态点"
        );
    });
}

#[test]
fn review_remediation_git_count_is_visible_without_filter_or_pointer_feedback() {
    with_review_remediation_view(|view| {
        // P3-24：Git 是移动计划的静态附加计数，不新增 kind=3 筛选。
        // 实际 Move 查询/整树移动复用既有 GUI 回调及 contract_engine 回归。
        let filter_requests = Rc::new(Cell::new(0));
        let requests = filter_requests.clone();
        view.ui
            .set_callback("filter-plan", move |_| {
                requests.set(requests.get() + 1);
                Value::Void
            })
            .unwrap();
        view.set("screen", 0);
        view.set("panel", 1);
        view.set("has-task", true);
        view.set("plan-move-count", 2);
        view.set("plan-git-count", 1);
        view.render();
        let count = view.button("Git 项目 1");
        let position = count.absolute_position();
        let size = count.size();
        let center = LogicalPosition::new(
            position.x + size.width / 2.0,
            position.y + size.height / 2.0,
        );
        assert!(center.x > 0.0 && center.x < 1120.0);
        assert!(center.y > 46.0 && center.y < 720.0);
        view.ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(1100.0, 700.0),
        });
        view.render();
        view.render();
        let idle = view.pixels_in(&count);
        view.screenshot("review-remediation-git-count-idle");

        view.ui
            .window()
            .dispatch_event(WindowEvent::PointerMoved { position: center });
        view.render();
        view.render();
        let hovered = view.pixels_in(&count);
        view.screenshot("review-remediation-git-count-hovered");
        assert!(idle == hovered, "P3-24：静态 Git 计数不能呈现悬停反馈");
        view.ui
            .window()
            .dispatch_event(WindowEvent::PointerPressed {
                position: center,
                button: PointerEventButton::Left,
            });
        view.render();
        view.render();
        let pressed = view.pixels_in(&count);
        view.screenshot("review-remediation-git-count-pressed");
        assert!(idle == pressed, "P3-24：静态 Git 计数不能呈现按压/焦点反馈");
        view.ui
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position: center,
                button: PointerEventButton::Left,
            });
        view.render();
        view.render();

        view.screenshot("review-remediation-git-count-released");
        assert!(
            idle == view.pixels_in(&count),
            "P3-24：静态 Git 计数释放后不能残留焦点反馈"
        );
        count.invoke_accessible_default_action();
        view.render();
        assert_eq!(count.accessible_enabled(), Some(false));
        assert_eq!(
            view.ui.get_property("plan-filter").unwrap(),
            Value::Number(0.0)
        );
        view.button("Git 项目 1");
        view.button("移动 2");
        assert_eq!(filter_requests.get(), 0, "静态计数不能请求任何计划筛选");

        // 共享 Pill 的正常可点行为不变；从移动项 Tab 离开也不能让静态计数获得焦点。
        view.click("移动 2");
        assert_eq!(
            view.ui.get_property("plan-filter").unwrap(),
            Value::Number(2.0)
        );
        assert_eq!(filter_requests.get(), 1);
        view.key(Key::Tab);
        view.render();
        assert!(
            idle == view.pixels_in(&count),
            "P3-24：Tab 导航必须跳过禁用的静态 Git 计数"
        );
        view.screenshot("review-remediation-git-count-tab-skipped");
        view.click("全部");
        assert_eq!(
            view.ui.get_property("plan-filter").unwrap(),
            Value::Number(0.0)
        );
        assert_eq!(filter_requests.get(), 2);
    });
}

#[test]
fn review_remediation_hotkey_save_ignores_autostart_draft_and_preserves_it() {
    with_review_remediation_view(|view| {
        // P3-25/O-15/XB-24：属性注入是服务/UiPump 边界，启用与点击由真实控件消费。
        // 只观察是否产生保存请求；不复制 Rust 控制器的保存或跨页草稿隔离实现。
        let save_requested = Rc::new(Cell::new(false));
        let requested = save_requested.clone();
        view.ui
            .set_callback("snap-save-settings", move |_| {
                requested.set(true);
                Value::Void
            })
            .unwrap();
        view.set("screen", 6);
        view.set("snap-connected", true);
        view.set("snap-hotkey", Value::String("Ctrl+Alt+O".into()));
        view.set("snap-hotkey-draft", Value::String("Ctrl+Alt+O".into()));
        view.set("snap-autostart", false);
        view.set("snap-autostart-draft", false);
        view.render();
        assert_eq!(view.button("保存快捷键").accessible_enabled(), Some(false));
        view.click("保存快捷键");
        assert!(!save_requested.get(), "没有快捷键改动时不得请求保存");

        // 设置页留下未保存自启草稿，再进入截图页；生效热键没有改变。
        view.set("screen", 7);
        view.set("snap-autostart-draft", true);
        view.render();
        view.set("screen", 6);
        view.render();
        view.screenshot("review-remediation-hotkey-autostart-only");
        view.click("保存快捷键");
        assert!(
            !save_requested.get(),
            "P3-25：仅有另一页自启草稿时，截图页真实点击不得产生保存请求"
        );
        assert_eq!(
            view.button("保存快捷键").accessible_enabled(),
            Some(false),
            "仅自启草稿改变不能启用保存快捷键"
        );

        view.set("snap-hotkey-draft", Value::String("Ctrl+Alt+P".into()));
        view.render();
        view.screenshot("review-remediation-hotkey-edited");
        assert_eq!(view.button("保存快捷键").accessible_enabled(), Some(true));
        view.click("保存快捷键");
        assert!(
            save_requested.replace(false),
            "真实点击必须产生快捷键保存请求"
        );

        // 服务确认热键生效；不把未保存的另一页草稿冒充已保存值。
        view.set("snap-hotkey", Value::String("Ctrl+Alt+P".into()));
        view.render();
        view.screenshot("review-remediation-hotkey-saved");
        view.click("保存快捷键");
        assert!(!save_requested.get(), "热键生效后不能重复提交无变化的设置");
        assert_eq!(view.button("保存快捷键").accessible_enabled(), Some(false));
        assert_eq!(
            view.ui.get_property("snap-autostart").unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            view.ui.get_property("snap-autostart-draft").unwrap(),
            Value::Bool(true)
        );

        // 返回设置页，保留的自启草稿必须仍以勾选状态呈现给用户。
        view.set("screen", 7);
        view.render();
        for _ in 0..8 {
            if let Some(checkbox) =
                ElementHandle::find_by_accessible_label(&view.ui, "登录时自动启动后台服务").find(
                    |element| {
                        let position = element.absolute_position();
                        position.y > 46.0 && position.y + element.size().height < 720.0
                    },
                )
            {
                assert_eq!(checkbox.accessible_checked(), Some(true));
                view.screenshot("review-remediation-hotkey-autostart-draft-preserved");
                return;
            }
            view.ui
                .window()
                .dispatch_event(WindowEvent::PointerScrolled {
                    position: LogicalPosition::new(1000.0, 550.0),
                    delta_x: 0.0,
                    delta_y: -250.0,
                });
            view.render();
        }
        panic!("返回设置页后必须能看到仍然勾选的自启草稿");
    });
}

/// 覆盖 U-02/U-05：搜索框与目录输入框共用 Field，图标在实际渲染布局中上下居中。
#[test]
fn input_field_icons_are_vertically_centered() {
    with_review_remediation_view(|view| {
        view.set("screen", 0);
        view.set("tool-count", 8);
        for (width, height) in [(960, 620), (1120, 720)] {
            view.window.set_size(PhysicalSize::new(width, height));
            view.pixels = SharedPixelBuffer::new(width, height);
            view.render();
            view.screenshot(&format!("input-icons-{width}x{height}"));
            let fields = ElementQuery::from_root(&view.ui)
                .match_type_name("Field")
                .find_all();
            for field_height in [32.0, 36.0] {
                let (field, icon) = fields
                    .iter()
                    .find_map(|field| {
                        if (field.size().height - field_height).abs() > 0.1 {
                            return None;
                        }
                        field
                            .query_descendants()
                            .match_type_name("Ico")
                            .find_first()
                            .map(|icon| (field, icon))
                    })
                    .expect("搜索框与目录输入框都应显示前置图标");
                let center = field.absolute_position().y + field.size().height / 2.0;
                let icon_center = icon.absolute_position().y + icon.size().height / 2.0;
                assert!(
                    (icon_center - center).abs() <= 0.5,
                    "{width}×{height}，{field_height}px 输入框：图标中心 {icon_center}，输入框中心 {center}"
                );
                let input = field
                    .query_descendants()
                    .match_type_name("TextInput")
                    .find_first()
                    .unwrap();
                assert!(
                    (input.size().height - field.size().height).abs() <= 1.0,
                    "图标居中不得压缩文字输入区域的高度"
                );
            }
        }
    });
}

/// U-05/U-12/M-01：模拟控制器发布的窗口状态，只验证真实布局与渲染，
/// 不启动任务服务，也不将此用例冒充后台响应性或文件处理正确性验证。
#[test]
fn footer_actions_remain_readable_with_long_paths() {
    with_review_remediation_view(|view| {
        let path = format!(
            "D:/中文资料与任务输出/{}最终报告与附件📁/处理结果.md",
            "需要保留上下文的很长目录名称/".repeat(10)
        );
        let status = format!("处理完成，结果已保存到：{path}");
        let metrics = format!("已处理 128 / 256 项；当前文件及输出目录：{path}");
        let progress_note = format!("正在处理第 128 项并写入输出目录：{path}");
        for property in ["status", "convert-status"] {
            view.set(property, Value::String(status.clone().into()));
        }
        for property in ["metrics", "convert-metrics"] {
            view.set(property, Value::String(metrics.clone().into()));
        }
        for property in ["progress-note", "convert-progress-note"] {
            view.set(property, Value::String(progress_note.clone().into()));
        }
        // 仅通过公开输入使正常主操作可用；长消息由控制器属性边界注入。
        for (property, value) in [
            ("directory", "D:/测试资料"),
            ("md-input-dir", "D:/测试资料"),
            ("md-output-dir", "D:/测试输出"),
            ("md-split-file", "D:/测试资料/报告.md"),
            ("md-split-dir", "D:/测试输出"),
            ("md-split-size", "1"),
            ("convert-input-dir", "D:/测试资料"),
            ("convert-output-dir", "D:/测试输出"),
            ("git-repo", "D:/测试仓库"),
        ] {
            view.set(property, Value::String(value.into()));
        }
        view.set("convert-ready", true);
        view.set("progress", 0.5);
        view.set("convert-progress", 0.5);

        for (width, height) in [(960, 620), (1120, 720)] {
            view.window.set_size(PhysicalSize::new(width, height));
            view.pixels = SharedPixelBuffer::new(width, height);
            for (theme, theme_name) in [(1, "light"), (2, "dark")] {
                view.set("theme", theme);
                // name, screen, panel, MD 子页, busy, ready, paused, preparing, footer 按钮。
                // 拆分完成后的长路径是已观察到的回归，优先保留其失败截图。
                for (name, screen, panel, md_subpage, busy, ready, paused, preparing, labels) in [
                    (
                        "md-split",
                        3,
                        0,
                        1,
                        false,
                        false,
                        false,
                        false,
                        &["开始拆分"][..],
                    ),
                    (
                        "md-merge",
                        3,
                        0,
                        0,
                        false,
                        false,
                        false,
                        false,
                        &["开始合并"][..],
                    ),
                    (
                        "md-merge-running",
                        3,
                        0,
                        0,
                        true,
                        false,
                        false,
                        false,
                        &["取消任务"][..],
                    ),
                    (
                        "md-split-running",
                        3,
                        0,
                        1,
                        true,
                        false,
                        false,
                        false,
                        &["取消任务"][..],
                    ),
                    (
                        "organizer-start",
                        0,
                        2,
                        0,
                        false,
                        false,
                        false,
                        false,
                        &["开始分析"][..],
                    ),
                    (
                        "organizer-ready",
                        0,
                        2,
                        0,
                        false,
                        true,
                        false,
                        false,
                        &["确认并执行整理"][..],
                    ),
                    (
                        "organizer-running",
                        0,
                        2,
                        0,
                        true,
                        false,
                        false,
                        false,
                        &["暂停", "取消任务"][..],
                    ),
                    (
                        "organizer-paused",
                        0,
                        2,
                        0,
                        true,
                        false,
                        true,
                        false,
                        &["继续", "取消任务"][..],
                    ),
                    (
                        "extract-start",
                        2,
                        1,
                        0,
                        false,
                        false,
                        false,
                        false,
                        &["开始解压"][..],
                    ),
                    (
                        "extract-running",
                        2,
                        1,
                        0,
                        true,
                        false,
                        false,
                        false,
                        &["暂停", "取消任务"][..],
                    ),
                    (
                        "extract-paused",
                        2,
                        1,
                        0,
                        true,
                        false,
                        true,
                        false,
                        &["继续", "取消任务"][..],
                    ),
                    (
                        "git-running",
                        4,
                        0,
                        0,
                        true,
                        false,
                        false,
                        false,
                        &["停止任务"][..],
                    ),
                    (
                        "convert-start",
                        5,
                        0,
                        0,
                        false,
                        false,
                        false,
                        false,
                        &["开始转换"][..],
                    ),
                    (
                        "convert-running",
                        5,
                        0,
                        0,
                        true,
                        false,
                        false,
                        false,
                        &["停止任务"][..],
                    ),
                    (
                        "convert-preparing",
                        5,
                        0,
                        0,
                        false,
                        false,
                        false,
                        true,
                        &["取消检查", "开始转换"][..],
                    ),
                ] {
                    view.set("screen", screen);
                    view.set("panel", panel);
                    view.set("md-subpage", md_subpage);
                    view.set("busy", busy);
                    view.set("ready", ready);
                    view.set("paused", paused);
                    view.set("convert-preparing", preparing);
                    view.render();
                    let context = format!("footer-{name}-{width}x{height}-{theme_name}");
                    view.screenshot(&context);
                    assert_footer_action_geometry(view, labels, &context);
                    if preparing {
                        assert_eq!(view.button("取消检查").accessible_enabled(), Some(true));
                        assert_eq!(view.button("开始转换").accessible_enabled(), Some(false));
                    } else {
                        for label in labels {
                            assert_eq!(
                                view.button(label).accessible_enabled(),
                                Some(true),
                                "{context}：操作应保持可用：{label}"
                            );
                        }
                    }
                }
            }
        }
    });
}

#[track_caller]
fn assert_footer_action_geometry(view: &View, labels: &[&str], context: &str) {
    let width = f32::from(u16::try_from(view.pixels.width()).unwrap());
    let height = f32::from(u16::try_from(view.pixels.height()).unwrap());
    let buttons: Vec<_> = labels.iter().map(|label| view.button(label)).collect();
    for (label, button) in labels.iter().zip(&buttons) {
        let position = button.absolute_position();
        let size = button.size();
        assert!(
            position.x >= -0.5
                && position.y >= 46.0
                && position.x + size.width <= width + 0.5
                && position.y + size.height <= height + 0.5,
            "{context}：按钮必须完整位于窗口内容区：{label}，位置 {position:?}，尺寸 {size:?}"
        );
        let text = button
            .query_descendants()
            .match_type_name("Text")
            .find_first()
            .expect("任务按钮应包含真实文字项");
        let text_position = text.absolute_position();
        let text_size = text.size();
        // 这些标签均为全角中文；现有正常字号为 13px，每字需要一个 em。
        // 约束实际 Text 区域，而非锁死按钮总宽度或保留无必要的拉伸空白：
        // 带图标的 60px 按钮虽然存在且可点击，四字标签仍会只剩省略号。
        let readable_width = label.chars().count() as f32 * 13.0;
        assert!(
            text_size.width + 0.5 >= readable_width,
            "{context}：{label} 的完整文字至少需要 {readable_width}px，实际文字区域 {text_size:?}，按钮 {size:?}"
        );
        assert!(
            text_size.height >= 13.0
                && text_position.x >= position.x - 0.5
                && text_position.y >= position.y - 0.5
                && text_position.x + text_size.width <= position.x + size.width + 0.5
                && text_position.y + text_size.height <= position.y + size.height + 0.5,
            "{context}：文字必须完整位于自身按钮内：{label}"
        );
    }
    for (index, first) in buttons.iter().enumerate() {
        let first_position = first.absolute_position();
        let first_size = first.size();
        for (other_index, second) in buttons.iter().enumerate().skip(index + 1) {
            let second_position = second.absolute_position();
            let second_size = second.size();
            assert!(
                first_position.x + first_size.width <= second_position.x + 0.5
                    || second_position.x + second_size.width <= first_position.x + 0.5
                    || first_position.y + first_size.height <= second_position.y + 0.5
                    || second_position.y + second_size.height <= first_position.y + 0.5,
                "{context}：操作按钮不得重叠：{} / {}",
                labels[index],
                labels[other_index]
            );
        }
    }
}
