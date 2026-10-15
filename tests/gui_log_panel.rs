#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    future_incompatible,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]

//! 日志展开的窗口内交互边界：真实 Slint 控件、指针、键盘和软件渲染，绝不创建 OS 窗口。
//! 覆盖 U-10/U-09/U-12：阅读面积、滚动复制、只读、安全确认优先及任务控制隔离。
//! P3-24/P3-25：静态 Git 计数无鼠标/Tab 焦点伪交互，截图快捷键保存不受另一页自启草稿影响。
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
use slint_interpreter::{Compiler, ComponentDefinition, ComponentInstance, Value};
use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
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
        // 首帧与可视变化由 Slint 标脏；非可视事件不强制重绘，保留 ReusedBuffer 的真实像素。
        let stride = self.pixels.width() as usize;
        let pixels = &mut self.pixels;
        self.window.draw_if_needed(|renderer| {
            renderer.render(pixels.make_mut_slice(), stride);
        });
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
            Value::Number(5.0) => 165.0,
            _ => 65.0,
        };
        for _ in 0..8 {
            let button = ElementHandle::find_by_accessible_label(&self.ui, "放大").next();
            if button.is_some_and(|button| {
                let position = button.absolute_position();
                position.y >= 46.0
                    && position.y + button.size().height
                        < f32::from(u16::try_from(self.pixels.height()).unwrap()) - reading_margin
            }) {
                return;
            }
            self.ui
                .window()
                .dispatch_event(WindowEvent::PointerScrolled {
                    position: LogicalPosition::new(
                        f32::from(u16::try_from(self.pixels.width()).unwrap()) - 150.0,
                        f32::from(u16::try_from(self.pixels.height()).unwrap()) - 150.0,
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
        assert!(
            center.x > 0.0 && center.x < f32::from(u16::try_from(self.pixels.width()).unwrap())
        );
        assert!(
            center.y > 46.0 && center.y < f32::from(u16::try_from(self.pixels.height()).unwrap()),
            "按钮应在实际窗口内：{label}"
        );
        self.click_at(center);
    }

    fn dispatch_key_event(&self, event: WindowEvent) {
        self.ui.window().dispatch_event(event);
    }

    fn key(&mut self, key: impl Into<SharedString>) {
        let text = key.into();
        self.dispatch_key_event(WindowEvent::KeyPressed { text: text.clone() });
        self.dispatch_key_event(WindowEvent::KeyReleased { text });
        self.render();
    }

    fn shortcut(&mut self, modifier: Key, key: impl Into<SharedString>) {
        self.dispatch_key_event(WindowEvent::KeyPressed {
            text: modifier.into(),
        });
        self.key(key);
        self.dispatch_key_event(WindowEvent::KeyReleased {
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
        // 标签/正文元数据仅用于定位；用普通鼠标交互聚焦，不要求专门的焦点遍历或自动焦点。
        let editor = self.editor(expected);
        let position = editor.absolute_position();
        let size = editor.size();
        self.click_at(LogicalPosition::new(
            position.x + size.width / 2.0,
            position.y + size.height / 2.0,
        ));
        *self.state.clipboard.borrow_mut() = None;
        self.shortcut(Key::Control, "a");
        self.shortcut(Key::Control, "c");
        assert_eq!(
            self.state.clipboard.borrow().as_deref(),
            Some(expected),
            "全选复制必须取得真实日志，而不是旧快照或其他工具日志"
        );
    }

    fn pixels_in(&self, element: &ElementHandle) -> Vec<u8> {
        // MinimalSoftwareWindow 的默认缩放为 1；仅比较控件真实占据的渲染区域。
        let position = element.absolute_position();
        let size = element.size();
        let left = position.x.floor().max(0.0) as u32;
        let top = position.y.floor().max(0.0) as u32;
        let right = ((position.x + size.width).ceil() as u32).min(self.pixels.width());
        let bottom = ((position.y + size.height).ceil() as u32).min(self.pixels.height());
        assert!(left < right && top < bottom, "控件必须在实际渲染区域内");
        let stride = self.pixels.width() as usize * 3;
        let mut region = Vec::new();
        for row in top..bottom {
            let start = row as usize * stride + left as usize * 3;
            let end = row as usize * stride + right as usize * 3;
            region.extend_from_slice(&self.pixels.as_bytes()[start..end]);
        }
        region
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

fn headless_definition() -> (Rc<HeadlessState>, ComponentDefinition) {
    let state = Rc::new(HeadlessState::default());
    slint::platform::set_platform(Box::new(HeadlessPlatform(state.clone()))).unwrap();
    // 编译器读取调试开关时串行访问环境；各窗口/平台仍只属于各自测试线程。
    let setup = jchtools_foundation::test_support::env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous_debug = std::env::var_os("SLINT_EMIT_DEBUG_INFO");
    std::env::set_var("SLINT_EMIT_DEBUG_INFO", "1");
    let mut compiler = Compiler::default();
    if let Some(previous_debug) = previous_debug {
        std::env::set_var("SLINT_EMIT_DEBUG_INFO", previous_debug);
    } else {
        std::env::remove_var("SLINT_EMIT_DEBUG_INFO");
    }
    drop(setup);
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
    (state, definition)
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

fn with_review_remediation_view(exercise: impl FnOnce(&mut View) + Send + 'static) {
    // 每个用例使用独立线程，避免重复设置 Slint 的线程局部平台。
    std::thread::spawn(move || {
        let (state, definition) = headless_definition();
        let ui = definition.create().unwrap();
        ui.show().unwrap();
        let window = state.window.borrow().as_ref().unwrap().clone();
        window.set_size(PhysicalSize::new(1120, 720));
        let mut view = View {
            ui,
            state,
            window,
            pixels: SharedPixelBuffer::new(1120, 720),
            task_actions: Rc::new(Cell::new(0)),
            confirmations: Rc::new(Cell::new(0)),
        };
        exercise(&mut view);
        view.ui.hide().unwrap();
    })
    .join()
    .unwrap();
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
        view.ui.window().dispatch_event(WindowEvent::PointerPressed {
            position: center,
            button: PointerEventButton::Left,
        });
        view.render();
        view.render();
        let pressed = view.pixels_in(&count);
        view.screenshot("review-remediation-git-count-pressed");
        assert!(idle == pressed, "P3-24：静态 Git 计数不能呈现按压/焦点反馈");
        view.ui.window().dispatch_event(WindowEvent::PointerReleased {
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
        assert_eq!(view.ui.get_property("plan-filter").unwrap(), Value::Number(0.0));
        view.button("Git 项目 1");
        view.button("移动 2");
        assert_eq!(filter_requests.get(), 0, "静态计数不能请求任何计划筛选");

        // 共享 Pill 的正常可点行为不变；从移动项 Tab 离开也不能让静态计数获得焦点。
        view.click("移动 2");
        assert_eq!(view.ui.get_property("plan-filter").unwrap(), Value::Number(2.0));
        assert_eq!(filter_requests.get(), 1);
        view.key(Key::Tab);
        view.render();
        assert!(
            idle == view.pixels_in(&count),
            "P3-24：Tab 导航必须跳过禁用的静态 Git 计数"
        );
        view.screenshot("review-remediation-git-count-tab-skipped");
        view.click("全部");
        assert_eq!(view.ui.get_property("plan-filter").unwrap(), Value::Number(0.0));
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
        assert!(save_requested.replace(false), "真实点击必须产生快捷键保存请求");

        // 服务确认热键生效；不把未保存的另一页草稿冒充已保存值。
        view.set("snap-hotkey", Value::String("Ctrl+Alt+P".into()));
        view.render();
        view.screenshot("review-remediation-hotkey-saved");
        view.click("保存快捷键");
        assert!(!save_requested.get(), "热键生效后不能重复提交无变化的设置");
        assert_eq!(view.button("保存快捷键").accessible_enabled(), Some(false));
        assert_eq!(view.ui.get_property("snap-autostart").unwrap(), Value::Bool(false));
        assert_eq!(view.ui.get_property("snap-autostart-draft").unwrap(), Value::Bool(true));

        // 返回设置页，保留的自启草稿必须仍以勾选状态呈现给用户。
        view.set("screen", 7);
        view.render();
        for _ in 0..8 {
            if let Some(checkbox) =
                ElementHandle::find_by_accessible_label(&view.ui, "登录时自动启动后台服务")
                    .find(|element| {
                        let position = element.absolute_position();
                        position.y > 46.0 && position.y + element.size().height < 720.0
                    })
            {
                assert_eq!(checkbox.accessible_checked(), Some(true));
                view.screenshot("review-remediation-hotkey-autostart-draft-preserved");
                return;
            }
            view.ui.window().dispatch_event(WindowEvent::PointerScrolled {
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
                let (field, icon) = fields.iter().find_map(|field| {
                    if (field.size().height - field_height).abs() > 0.1 {
                        return None;
                    }
                    field.query_descendants().match_type_name("Ico").find_first()
                        .map(|icon| (field, icon))
                }).expect("搜索框与目录输入框都应显示前置图标");
                let center = field.absolute_position().y + field.size().height / 2.0;
                let icon_center = icon.absolute_position().y + icon.size().height / 2.0;
                assert!(
                    (icon_center - center).abs() <= 0.5,
                    "{width}×{height}，{field_height}px 输入框：图标中心 {icon_center}，输入框中心 {center}"
                );
                let input = field.query_descendants()
                    .match_type_name("TextInput").find_first().unwrap();
                assert!(
                    (input.size().height - field.size().height).abs() <= 1.0,
                    "图标居中不得压缩文字输入区域的高度"
                );
            }
        }
    });
}
