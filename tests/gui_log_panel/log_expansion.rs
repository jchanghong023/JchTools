use super::*;
use std::fmt::Write as _;

fn log_fixture(tool: &str) -> String {
    let mut text = String::new();
    // U-10：注入窗口的日志也使用最新在上顺序；本控件用例不冒充日志收集/300 条淘汰验证。
    for line in (0..180).rev() {
        writeln!(
            text,
            "{tool} 日志 {line:03}：路径 D:/测试目录/文件-{line:03}.md · 保留完整错误与处理结果"
        )
        .unwrap();
    }
    text
}

fn exercise(view: &mut View, screen: i32, panel: i32, name: &str, busy: bool) {
    view.set("screen", screen);
    view.set("panel", panel);
    view.set("busy", busy);
    let log = log_fixture(name);
    let other_log = log_fixture("另一工具，不应混入");
    let source = if screen == 5 {
        "convert-log-text"
    } else {
        "log-text"
    };
    view.set(
        "log-text",
        Value::String(
            if screen == 5 {
                other_log.clone()
            } else {
                log.clone()
            }
            .into(),
        ),
    );
    view.set(
        "convert-log-text",
        Value::String(if screen == 5 { log.clone() } else { other_log }.into()),
    );
    view.render();

    view.reveal_expand_button();
    view.screenshot(&format!(
        "{name}-{}x{}-normal-{busy}",
        view.pixels.width(),
        view.pixels.height()
    ));
    view.click("放大");
    assert!(view.has_button("还原"), "点击标题栏应显示展开卡片");
    let editor = view.editor(&log);
    let size = editor.size();
    let position = editor.absolute_position();
    assert!(
        position.x < 64.0 && position.y > 46.0,
        "展开应覆盖侧栏下方主内容，而不是另开窗口"
    );
    assert!(size.width > f32::from(u16::try_from(view.pixels.width()).unwrap()) * 0.85);
    assert!(
        size.height > f32::from(u16::try_from(view.pixels.height()).unwrap()) * 0.65,
        "正文必须真实获得可读高度"
    );
    view.screenshot(&format!(
        "{name}-{}x{}-expanded-{busy}",
        view.pixels.width(),
        view.pixels.height()
    ));

    // 用真实鼠标选择正文、键盘全选复制；直接尝试输入、删除、剪切验证日志不能被编辑。
    view.copy_all(&log);
    view.key("禁止写入日志");
    view.editor(&log);
    view.key(Key::Delete);
    view.editor(&log);
    view.shortcut(Key::Control, "x");
    view.editor(&log);
    view.copy_all(&log);

    // 滚轮改变真实文本项的窗口坐标，证明滚动的是正文，而不是遮罩后原页面。
    view.shortcut(Key::Control, Key::Home);
    let input = editor
        .query_descendants()
        .match_type_name("TextInput")
        .find_first()
        .unwrap();
    let before = input.absolute_position().y;
    view.ui
        .window()
        .dispatch_event(WindowEvent::PointerScrolled {
            position: LogicalPosition::new(
                position.x + size.width / 2.0,
                position.y + size.height / 2.0,
            ),
            delta_x: 0.0,
            delta_y: -240.0,
        });
    view.render();
    assert!(
        input.absolute_position().y < before - 20.0,
        "长日志正文应响应滚轮"
    );
    view.copy_all(&log);

    // 输入是生产 UiPump 的更新边界；消费者断言来自窗口正文及键盘复制，而非 expanded-log-text。
    let updated = format!("{name} 实时新增：处理仍在继续\n{log}");
    view.set(source, Value::String(updated.clone().into()));
    view.render();
    view.editor(&updated);
    view.copy_all(&updated);
    view.click("还原");
    assert!(!view.has_button("还原"));
    view.editor(&updated);
    view.click("放大");
    view.copy_all(&updated);
    view.key(Key::Escape);
    assert!(!view.has_button("还原"), "正文获得焦点时 Esc 应还原");

    // 普通鼠标还原与正文 Esc 均须保留更新后的内容及任务状态，不要求 Shift+Tab 遍历焦点。
    view.click("放大");
    view.copy_all(&updated);
    view.click("还原");
    assert!(!view.has_button("还原"), "鼠标还原必须返回原布局");
    view.editor(&updated);
    view.click("放大");
    view.copy_all(&updated);
    view.key(Key::Escape);
    assert!(!view.has_button("还原"), "正文操作后 Esc 必须还原");
    view.editor(&updated);
    assert!(
        matches!(view.ui.get_property("busy").unwrap(), Value::Bool(value) if value == busy),
        "日志还原不得改变任务运行状态"
    );

    view.click("放大");
    view.set(
        "confirm-text",
        Value::String("后台任务仍在处理文件。是否停止任务并关闭？".into()),
    );
    view.set("confirm-kind", 3);
    view.render();
    view.screenshot(&format!(
        "{name}-{}x{}-close-confirm-{busy}",
        view.pixels.width(),
        view.pixels.height()
    ));
    view.click("还原");
    assert!(
        view.has_button("停止并关闭"),
        "背景还原点击不能关闭安全确认"
    );
    assert!(view.has_button("还原"), "背景还原点击不能穿透安全确认");
    view.editor(&updated);
    view.key(Key::Escape);
    assert!(view.has_button("停止并关闭"), "Esc 不能关闭安全确认");
    assert!(view.has_button("还原"), "Esc 不能穿透安全确认还原底层");
    assert_eq!(view.confirmations.get(), 0, "不能误确认停止任务");
    view.click("返回检查");
    assert!(
        !view.has_button("停止并关闭"),
        "最上层确认按钮必须接收真实点击"
    );
    assert!(view.has_button("还原"));
    view.key(Key::Escape);
    assert!(!view.has_button("还原"));

    // 页面/panel 改变是已有 Rust 控制器边界；退回原页也不能携带旧展开层。
    view.click("放大");
    view.set("screen", 1);
    view.render();
    assert!(!view.has_button("还原"));
    view.set("screen", screen);
    view.render();
    assert!(!view.has_button("还原"));
    view.click("放大");
    view.copy_all(&updated);
    view.set("panel", i32::from(panel == 0));
    view.render();
    assert!(!view.has_button("还原"));
    view.set("panel", panel);
    view.render();
    assert!(!view.has_button("还原"));
    assert_eq!(
        view.task_actions.get(),
        0,
        "阅读日志不得误暂停/取消/停止后台任务"
    );
}

#[test]
fn all_tool_logs_support_headless_expansion_and_modal_boundaries() {
    // 单测试串行矩阵：Slint 平台仅绑定此测试线程，既不依赖 winit 也不更改全局鼠标/焦点。
    let (state, definition) = headless_definition();
    for (width, height) in [(960, 620), (1120, 720)] {
        for (screen, panel, name) in [
            (0, 2, "organizer"),
            (2, 1, "extract"),
            (3, 0, "md"),
            (4, 0, "git"),
            (5, 0, "convert"),
        ] {
            for busy in [false, true] {
                let ui = definition.create().unwrap();
                ui.show().unwrap();
                let window = state.window.borrow().as_ref().unwrap().clone();
                window.set_size(PhysicalSize::new(width, height));
                let task_actions = Rc::new(Cell::new(0));
                let confirmations = Rc::new(Cell::new(0));
                for name in ["pause-task", "cancel-task", "git-stop", "convert-stop"] {
                    let actions = task_actions.clone();
                    ui.set_callback(name, move |_| {
                        actions.set(actions.get() + 1);
                        Value::Void
                    })
                    .unwrap();
                }
                let confirmed = confirmations.clone();
                ui.set_callback("confirmed", move |_| {
                    confirmed.set(confirmed.get() + 1);
                    Value::Void
                })
                .unwrap();
                let mut view = View {
                    ui,
                    state: state.clone(),
                    window,
                    pixels: SharedPixelBuffer::new(width, height),
                    task_actions,
                    confirmations,
                };
                exercise(&mut view, screen, panel, name, busy);
                view.ui.hide().unwrap();
            }
        }
    }
}
