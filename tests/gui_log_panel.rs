//! 日志展开的窗口内交互边界：真实 Slint 控件、指针、键盘和软件渲染，绝不创建 OS 窗口。
//! 覆盖 U-10/U-09/U-12：阅读面积、滚动复制、只读、安全确认优先及任务控制隔离。
// 仅 GUI + test-hooks 装配使用此集成测试；无 GUI 构建不包含被测窗口。
//! 可选截图：JCH_LOG_PANEL_SCREENSHOTS=.tmp/log-panel；默认不写文件。
#![cfg(all(feature = "gui", feature = "test-hooks"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use i_slint_backend_testing::{ElementHandle, ElementQuery};
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{
    Clipboard, Key, Platform, PlatformError, PointerEventButton, WindowAdapter, WindowEvent,
};
use slint::{
    ComponentHandle, LogicalPosition, PhysicalSize, Rgb8Pixel, SharedPixelBuffer, SharedString,
};
use slint_interpreter::{Compiler, ComponentInstance, Value};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

#[derive(Default)]
struct HeadlessState {
    window: RefCell<Option<Rc<MinimalSoftwareWindow>>>,
    clipboard: RefCell<Option<String>>,
    time_ms: Cell<u64>,
}

struct HeadlessPlatform(Rc<HeadlessState>);

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        *self.0.window.borrow_mut() = Some(window.clone());
        Ok(window)
    }

    fn duration_since_start(&self) -> Duration {
        Duration::from_millis(self.0.time_ms.get())
    }

    fn set_clipboard_text(&self, text: &str, clipboard: Clipboard) {
        if clipboard == Clipboard::DefaultClipboard {
            *self.0.clipboard.borrow_mut() = Some(text.to_owned());
        }
    }

    fn clipboard_text(&self, clipboard: Clipboard) -> Option<String> {
        if clipboard == Clipboard::DefaultClipboard {
            self.0.clipboard.borrow().clone()
        } else {
            None
        }
    }
}

struct View {
    ui: ComponentInstance,
    state: Rc<HeadlessState>,
    window: Rc<MinimalSoftwareWindow>,
    pixels: SharedPixelBuffer<Rgb8Pixel>,
    task_actions: Rc<Cell<usize>>,
    confirmations: Rc<Cell<usize>>,
}

impl View {
    fn render(&mut self) {
        // 推进真实动画/changed 回调，但不依赖 wall-clock 或任何 OS 事件循环。
        self.state.time_ms.set(self.state.time_ms.get() + 150);
        slint::platform::update_timers_and_animations();
        self.ui.window().request_redraw();
        let stride = self.pixels.width() as usize;
        let pixels = &mut self.pixels;
        assert!(self.window.draw_if_needed(|renderer| {
            renderer.render(pixels.make_mut_slice(), stride);
        }));
    }

    fn set(&self, name: &str, value: impl Into<Value>) {
        self.ui.set_property(name, value.into()).unwrap();
    }

    fn button(&self, label: &str) -> ElementHandle {
        let buttons: Vec<_> = ElementHandle::find_by_accessible_label(&self.ui, label)
            .filter(|element| {
                element.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Button)
            })
            .filter(|element| element.size().width > 0.0 && element.size().height > 0.0)
            .collect();
        assert_eq!(
            buttons.len(),
            1,
            "应有一个可见按钮：{label}，匹配项：{:?}",
            buttons
                .iter()
                .map(|element| (
                    element.type_name(),
                    element.id(),
                    element.absolute_position(),
                    element.size()
                ))
                .collect::<Vec<_>>()
        );
        buttons.into_iter().next().unwrap()
    }

    fn has_button(&self, label: &str) -> bool {
        ElementHandle::find_by_accessible_label(&self.ui, label)
            .next()
            .is_some()
    }

    fn click_at(&mut self, position: LogicalPosition) {
        for event in [
            WindowEvent::PointerMoved { position },
            WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            },
            WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            },
        ] {
            self.ui.window().dispatch_event(event);
        }
        self.render();
    }

    fn reveal_expand_button(&mut self) {
        // 查询 API 会跳过被 ScrollView 完全裁掉的元素，不能先 unwrap 再滚动。
        // 转换整页可滚动，要同时露出正文；其他工具的固定日志区只需标题栏可点击。
        let reading_margin = match self.ui.get_property("screen").unwrap() {
            Value::Number(screen) if screen == 5.0 => 165.0,
            _ => 65.0,
        };
        for _ in 0..8 {
            let button = ElementHandle::find_by_accessible_label(&self.ui, "放大").next();
            if button.is_some_and(|button| {
                let position = button.absolute_position();
                position.y >= 46.0
                    && position.y + button.size().height
                        < self.pixels.height() as f32 - reading_margin
            }) {
                return;
            }
            self.ui
                .window()
                .dispatch_event(WindowEvent::PointerScrolled {
                    position: LogicalPosition::new(
                        self.pixels.width() as f32 - 150.0,
                        self.pixels.height() as f32 - 150.0,
                    ),
                    delta_x: 0.0,
                    delta_y: -250.0,
                });
            self.render();
        }
        panic!(
            "滚动整页后仍无法找到日志放大按钮：screen={:?}, size={}x{}",
            self.ui.get_property("screen"),
            self.pixels.width(),
            self.pixels.height()
        );
    }

    fn click(&mut self, label: &str) {
        if label == "放大" {
            self.reveal_expand_button();
        }
        let button = self.button(label);
        let position = button.absolute_position();
        let size = button.size();
        let center = LogicalPosition::new(
            position.x + size.width / 2.0,
            position.y + size.height / 2.0,
        );
        assert!(center.x > 0.0 && center.x < self.pixels.width() as f32);
        assert!(
            center.y > 46.0 && center.y < self.pixels.height() as f32,
            "按钮应在实际窗口内：{label}"
        );
        self.click_at(center);
    }

    fn key(&mut self, key: impl Into<SharedString>) {
        let text = key.into();
        self.ui
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        self.ui
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text });
        self.render();
    }

    fn shortcut(&mut self, modifier: Key, key: impl Into<SharedString>) {
        self.ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: modifier.into(),
        });
        self.key(key);
        self.ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: modifier.into(),
        });
        self.render();
    }

    #[track_caller]
    fn editor(&self, text: &str) -> ElementHandle {
        // 原卡片还在遮罩后面；按真实可访问正文和实际几何选最大编辑器，而非读取根属性。
        let expected = text.to_owned();
        ElementQuery::from_root(&self.ui)
            .match_type_name("TextEdit")
            .match_predicate(move |element| {
                element
                    .accessible_value()
                    .is_some_and(|value| value == expected)
            })
            .find_all()
            .into_iter()
            .max_by(|left, right| {
                let left = left.size();
                let right = right.size();
                (left.width * left.height).total_cmp(&(right.width * right.height))
            })
            .expect("真实 TextEdit 必须显示指定日志正文")
    }

    fn copy_all(&mut self, expected: &str) {
        *self.state.clipboard.borrow_mut() = None;
        self.shortcut(Key::Control, "a");
        self.shortcut(Key::Control, "c");
        assert_eq!(
            self.state.clipboard.borrow().as_deref(),
            Some(expected),
            "全选复制必须取得真实日志，而不是旧快照或其他工具日志"
        );
    }

    fn screenshot(&self, name: &str) {
        let Some(directory) = std::env::var_os("JCH_LOG_PANEL_SCREENSHOTS") else {
            return;
        };
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        image::save_buffer_with_format(
            directory.join(format!("{name}.png")),
            self.pixels.as_bytes(),
            self.pixels.width(),
            self.pixels.height(),
            image::ColorType::Rgb8,
            image::ImageFormat::Png,
        )
        .unwrap();
    }
}

fn log_fixture(tool: &str) -> String {
    (0..180).map(|line| format!("{tool} 日志 {line:03}：路径 D:/测试目录/文件-{line:03}.md · 保留完整错误与处理结果\n")).collect()
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
    assert!(size.width > view.pixels.width() as f32 * 0.85);
    assert!(
        size.height > view.pixels.height() as f32 * 0.65,
        "正文必须真实获得可读高度"
    );
    assert_eq!(editor.accessible_read_only(), Some(true));
    view.screenshot(&format!(
        "{name}-{}x{}-expanded-{busy}",
        view.pixels.width(),
        view.pixels.height()
    ));

    // 不点正文先复制：展开时实际焦点必须已经交给正文。
    view.copy_all(&log);
    view.key("禁止写入日志");
    view.shortcut(Key::Control, "x");
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

    // Shift+Tab 从正文回到标题栏按钮；先用 Space 证明按钮焦点，再验证同一位置 Esc 冒泡。
    view.click("放大");
    view.shortcut(Key::Shift, Key::Tab);
    view.key(" ");
    assert!(!view.has_button("还原"), "标题栏还原按钮应可用键盘访问");
    view.click("放大");
    view.shortcut(Key::Shift, Key::Tab);
    view.key(Key::Escape);
    assert!(!view.has_button("还原"), "还原按钮焦点下 Esc 应冒泡");

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
    assert_eq!(view.button("还原").accessible_enabled(), Some(false));
    view.click("还原");
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
    view.set("panel", if panel == 0 { 1 } else { 0 });
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
    let state = Rc::new(HeadlessState::default());
    slint::platform::set_platform(Box::new(HeadlessPlatform(state.clone()))).unwrap();
    let previous_debug = std::env::var_os("SLINT_EMIT_DEBUG_INFO");
    std::env::set_var("SLINT_EMIT_DEBUG_INFO", "1");
    let mut compiler = Compiler::default();
    if let Some(previous_debug) = previous_debug {
        std::env::set_var("SLINT_EMIT_DEBUG_INFO", previous_debug);
    } else {
        std::env::remove_var("SLINT_EMIT_DEBUG_INFO");
    }
    compiler.set_style("fluent".into());
    let result = spin_on::spin_on(
        compiler.build_from_path(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui/app.slint")),
    );
    let definition = result.component("AppWindow").unwrap_or_else(|| {
        panic!(
            "AppWindow 编译失败：{:?}",
            result.diagnostics().collect::<Vec<_>>()
        )
    });
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
