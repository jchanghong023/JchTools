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
                    ("md-split", 3, 0, 1, false, false, false, false, &["开始拆分"][..]),
                    ("md-merge", 3, 0, 0, false, false, false, false, &["开始合并"][..]),
                    ("md-merge-running", 3, 0, 0, true, false, false, false, &["取消任务"][..]),
                    ("md-split-running", 3, 0, 1, true, false, false, false, &["取消任务"][..]),
                    ("organizer-start", 0, 2, 0, false, false, false, false, &["开始分析"][..]),
                    ("organizer-ready", 0, 2, 0, false, true, false, false, &["确认并执行整理"][..]),
                    ("organizer-running", 0, 2, 0, true, false, false, false, &["暂停", "取消任务"][..]),
                    ("organizer-paused", 0, 2, 0, true, false, true, false, &["继续", "取消任务"][..]),
                    ("extract-start", 2, 1, 0, false, false, false, false, &["开始解压"][..]),
                    ("extract-running", 2, 1, 0, true, false, false, false, &["暂停", "取消任务"][..]),
                    ("extract-paused", 2, 1, 0, true, false, true, false, &["继续", "取消任务"][..]),
                    ("git-running", 4, 0, 0, true, false, false, false, &["停止任务"][..]),
                    ("convert-start", 5, 0, 0, false, false, false, false, &["开始转换"][..]),
                    ("convert-running", 5, 0, 0, true, false, false, false, &["停止任务"][..]),
                    ("convert-preparing", 5, 0, 0, false, false, false, true, &["取消检查", "开始转换"][..]),
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

struct DiagnosticConsumer {
    screen: i32,
    name: &'static str,
    page_id: &'static str,
    detail_id: &'static str,
    text_id: &'static str,
    diagnostic_property: &'static str,
    detail_open_property: Option<&'static str>,
    anchor_label: &'static str,
    anchor_type: &'static str,
}

const DIAGNOSTIC_CONSUMERS: [DiagnosticConsumer; 5] = [
    DiagnosticConsumer {
        screen: 7,
        name: "settings",
        page_id: "settings-scroll",
        detail_id: "snap-settings-detail",
        text_id: "snap-settings-detail-text",
        diagnostic_property: "snap-error",
        detail_open_property: None,
        anchor_label: "检查 / 修复组件",
        anchor_type: "AppButton",
    },
    DiagnosticConsumer {
        screen: 6,
        name: "snapshot",
        page_id: "snap-page-scroll",
        detail_id: "snap-page-detail",
        text_id: "snap-page-detail-text",
        diagnostic_property: "snap-error",
        detail_open_property: Some("snap-detail-open"),
        anchor_label: "前往设置",
        anchor_type: "AppButton",
    },
    DiagnosticConsumer {
        screen: 5,
        name: "convert",
        page_id: "converter-scroll",
        detail_id: "convert-detail-scroll",
        text_id: "convert-detail-text",
        diagnostic_property: "convert-detail-text",
        detail_open_property: Some("convert-detail-open"),
        anchor_label: "截图与文档组件随包提供；音频资产在设置中按需下载，故障原因见详情。",
        anchor_type: "Muted",
    },
    DiagnosticConsumer {
        screen: 8,
        name: "acp-service",
        page_id: "acp-scroll",
        detail_id: "acp-error-scroll",
        text_id: "acp-error-text",
        diagnostic_property: "acp-service-error",
        detail_open_property: None,
        anchor_label: "退出模型服务",
        anchor_type: "AppButton",
    },
    DiagnosticConsumer {
        screen: 8,
        name: "acp-action",
        page_id: "acp-scroll",
        detail_id: "acp-error-scroll",
        text_id: "acp-error-text",
        diagnostic_property: "acp-action-error",
        detail_open_property: None,
        anchor_label: "退出模型服务",
        anchor_type: "AppButton",
    },
];

impl DiagnosticConsumer {
    fn show(&self, view: &mut View, diagnostic: &str) -> String {
        // 同页的两类 ACP 错误也从全新阅读位置开始；仅注入界面状态，不调用服务。
        view.set("screen", 0);
        view.render();
        view.set("acp-service-error", Value::String("".into()));
        view.set("acp-action-error", Value::String("".into()));
        view.set(self.diagnostic_property, Value::String(diagnostic.into()));
        if let Some(property) = self.detail_open_property {
            view.set(property, true);
        }
        view.set("screen", self.screen);
        view.render();
        if self.diagnostic_property == "acp-action-error" {
            format!("操作错误：{diagnostic}\n")
        } else {
            diagnostic.to_owned()
        }
    }
}

/// U-05：短诊断与后续恢复操作之间不应预留一个多行滚动框的空白。
#[test]
fn diagnostic_details_short_messages_do_not_reserve_empty_multiline_space() {
    with_review_remediation_view(|view| {
        let diagnostic = "诊断样本：组件资产暂不可用。";
        // 各消费者使用同一真实阅读区与操作锚点检查。
        for (width, height) in [(960, 620), (1120, 720)] {
            view.window.set_size(PhysicalSize::new(width, height));
            view.pixels = SharedPixelBuffer::new(width, height);
            for (theme, theme_name) in [(1, "light"), (2, "dark")] {
                view.set("theme", theme);
                for consumer in &DIAGNOSTIC_CONSUMERS {
                    let expected = consumer.show(view, diagnostic);
                    let (text, recovery) =
                        reveal_diagnostic_recovery(view, consumer);
                    let context = format!(
                        "diagnostic-short-{}-{width}x{height}-{theme_name}",
                        consumer.name
                    );
                    view.screenshot(&context);
                    let viewport = diagnostic_area(view, consumer.detail_id);
                    let viewport_position = viewport.absolute_position();
                    let viewport_size = viewport.size();
                    assert!(
                        viewport_size.height >= text.size().height - 0.5
                            && viewport_size.height <= text.size().height * 2.0 + 20.0
                            && (text.absolute_position().y - viewport_position.y).abs() <= 0.5,
                        "{context}：短诊断的实际阅读区应随正文收缩，不得保留长诊断的固定阅读高度；正文 {:?}，阅读区 {viewport_size:?}",
                        text.size()
                    );
                    let gap = recovery.absolute_position().y
                        - (text.absolute_position().y + text.size().height);
                    assert!(
                        gap >= -0.5 && gap <= text.size().height * 2.0 + 20.0,
                        "{context}：单行诊断之后应紧接恢复操作，不得保留多行空白；正文高度 {}，间距 {gap}",
                        text.size().height
                    );
                    assert_eq!(text.accessible_label().as_deref(), Some(expected.as_str()));
                }
            }
        }
    });
}

/// U-05：长诊断保留完整内容，限制阅读区高度，通过真实滚轮读到末尾。
#[test]
fn diagnostic_details_long_messages_are_bounded_and_scroll_to_the_end() {
    with_review_remediation_view(|view| {
        let diagnostic = (0..48)
            .map(|line| {
                format!("诊断记录 {line:02}：D:/测试资料/组件/模型.bin；保留定位上下文。")
            })
            .collect::<Vec<_>>()
            .join("\n");
        // 服务错误与操作错误分别保留原文，包括操作错误前缀和末尾换行。
        for (width, height) in [(960, 620), (1120, 720)] {
            view.window.set_size(PhysicalSize::new(width, height));
            view.pixels = SharedPixelBuffer::new(width, height);
            for (theme, theme_name) in [(1, "light"), (2, "dark")] {
                view.set("theme", theme);
                for consumer in &DIAGNOSTIC_CONSUMERS {
                    let expected = consumer.show(view, &diagnostic);
                    let (text, recovery) =
                        reveal_diagnostic_recovery(view, consumer);
                    let context = format!(
                        "diagnostic-long-{}-{width}x{height}-{theme_name}",
                        consumer.name
                    );
                    view.screenshot(&format!("{context}-top"));
                    let text_size = text.size();
                    let recovery_position = recovery.absolute_position();
                    let viewport = diagnostic_area(view, consumer.detail_id);
                    let viewport_position = viewport.absolute_position();
                    let viewport_size = viewport.size();
                    let viewport_bottom = viewport_position.y + viewport_size.height;
                    let reading_height = viewport_size.height;
                    assert!(
                        text_size.height > f32::from(u16::try_from(height).unwrap())
                            && reading_height >= 2.0 * 13.0
                            && reading_height
                                <= f32::from(u16::try_from(height).unwrap()) / 3.0,
                        "{context}：长诊断应保留完整正文且阅读区封顶，不能将恢复操作推出窗口；正文 {text_size:?}，阅读区 {reading_height}"
                    );
                    let pointer = LogicalPosition::new(
                        viewport_position.x + viewport_size.width / 2.0,
                        viewport_position.y + viewport_size.height / 2.0,
                    );
                    for _ in 0..32 {
                        let remaining =
                            text.absolute_position().y + text.size().height - viewport_bottom;
                        if remaining <= 0.5 {
                            break;
                        }
                        let before = text.absolute_position().y;
                        view.ui.window().dispatch_event(WindowEvent::PointerScrolled {
                            position: pointer,
                            delta_x: 0.0,
                            // 最后一步只请求尚未进入视口的距离，避免越过正文末尾后
                            // 把剩余滚轮量正常冒泡给外层页面。
                            delta_y: -remaining.min(viewport_size.height / 2.0),
                        });
                        view.render();
                        assert!(
                            (recovery.absolute_position().y - recovery_position.y).abs() <= 0.5,
                            "{context}：尚有未读正文时只能滚动内部阅读区，恢复操作应保持原位；原位置 {recovery_position:?}，实际位置 {:?}，剩余 {remaining}，视口 {viewport_position:?}/{viewport_size:?}",
                            recovery.absolute_position()
                        );
                        assert!(
                            text.absolute_position().y < before - 0.5,
                            "{context}：未读到末尾前诊断正文必须响应滚轮"
                        );
                    }
                    let bottom = text.absolute_position().y + text.size().height;
                    assert!(
                        bottom <= viewport_bottom + 0.5
                            && bottom >= viewport_position.y + 13.0,
                        "{context}：完整诊断末尾必须能滚入阅读区，实际底部 {bottom}"
                    );
                    assert_eq!(
                        text.accessible_label().as_deref(),
                        Some(expected.as_str()),
                        "{context}：滚动不能截断、替换或丢失诊断内容"
                    );
                    assert_eq!(text.size(), text_size);
                    view.screenshot(&format!("{context}-end"));
                }
            }
        }
    });
}

#[track_caller]
fn reveal_diagnostic_recovery(
    view: &mut View,
    consumer: &DiagnosticConsumer,
) -> (ElementHandle, ElementHandle) {
    let page = diagnostic_area(view, consumer.page_id);
    let page_position = page.absolute_position();
    let page_size = page.size();
    for _ in 0..24 {
        let text = diagnostic_element(view, consumer.text_id);
        let viewport = diagnostic_element(view, consumer.detail_id);
        let recovery = page
            .query_descendants()
            .match_type_name(consumer.anchor_type)
            .match_predicate({
                let label = consumer.anchor_label;
                move |element| element.accessible_label().as_deref() == Some(label)
            })
            .find_first();
        if let (Some(text), Some(viewport), Some(recovery)) = (text, viewport, recovery) {
            let viewport_position = viewport.absolute_position();
            let viewport_size = viewport.size();
            let recovery_position = recovery.absolute_position();
            let recovery_size = recovery.size();
            if viewport_position.y >= page_position.y
                && text.absolute_position().y >= viewport_position.y - 0.5
                && text.absolute_position().y < recovery_position.y
                && viewport_position.y + viewport_size.height <= recovery_position.y + 0.5
                && viewport_position.y + viewport_size.height
                    <= page_position.y + page_size.height + 0.5
                && viewport_position.x >= page_position.x
                && viewport_position.x + viewport_size.width
                    <= page_position.x + page_size.width + 0.5
                && recovery_position.y + recovery_size.height
                    <= page_position.y + page_size.height + 0.5
                && recovery_position.x >= page_position.x
                && recovery_position.x + recovery_size.width
                    <= page_position.x + page_size.width + 0.5
            {
                return (text, recovery);
            }
        }
        // 卡片右侧内边距属于外层页面，不把“寻找诊断”误变为滚动诊断正文。
        view.ui.window().dispatch_event(WindowEvent::PointerScrolled {
            position: LogicalPosition::new(
                page_position.x + page_size.width - 12.0,
                page_position.y + page_size.height / 2.0,
            ),
            delta_x: 0.0,
            delta_y: -60.0,
        });
        view.render();
    }
    panic!(
        "滚动整页后诊断阅读区和后续操作必须同时位于窗口内：screen={:?}，窗口 {}x{}",
        view.ui.get_property("screen"),
        view.pixels.width(),
        view.pixels.height()
    );
}

#[track_caller]
fn diagnostic_area(view: &View, id: &str) -> ElementHandle {
    diagnostic_element(view, id).expect("诊断阅读区应具有可查询的真实布局")
}

fn diagnostic_element(view: &View, id: &str) -> Option<ElementHandle> {
    // find_by_element_id/label 内部先 find_all，连 .next() 也遍历全部内置节点。
    // 精确限定 AppWindow ID 并 find_first，命中后即停止，避免无关节点的调试元数据警告。
    ElementQuery::from_root(&view.ui)
        .match_id(format!("AppWindow::{id}"))
        .find_first()
}

/// U-05/U-10：空失败列表的介绍贴近卡片顶部，空余空间留在介绍与分页之间。
#[test]
fn extract_empty_failure_list_keeps_intro_adjacent_and_pagination_visible() {
    with_review_remediation_view(|view| {
        view.set("screen", 2);
        view.set("panel", 2);
        view.set("busy", false);
        view.set("fail-total", 0);
        view.set("fail-prev-enabled", false);
        view.set("fail-next-enabled", false);
        for (width, height) in [(960, 620), (1120, 720)] {
            view.window.set_size(PhysicalSize::new(width, height));
            view.pixels = SharedPixelBuffer::new(width, height);
            for (theme, theme_name) in [(1, "light"), (2, "dark")] {
                view.set("theme", theme);
                for (ready, state) in [(false, "unassociated"), (true, "empty")] {
                    view.set("fail-ready", ready);
                    view.render();
                    let context =
                        format!("extract-failures-{state}-{width}x{height}-{theme_name}");
                    view.screenshot(&context);
                    // 文本只用于定位现有标题，不将提示措辞变成产品要求。
                    let title_label = "本次解压失败项（共 0 项）";
                    let title = ElementHandle::find_by_accessible_label(&view.ui, title_label)
                        .find(|element| element.size().width > 0.0 && element.size().height > 0.0)
                        .expect("空失败列表应保留真实标题");
                    let card = ElementHandle::find_by_element_type_name(&view.ui, "Surface")
                        .find(|element| {
                            element
                                .query_descendants()
                                .match_predicate(move |child| {
                                    child.accessible_label().as_deref() == Some(title_label)
                                })
                                .find_first()
                                .is_some()
                        })
                        .expect("失败列表标题应位于真实卡片中");
                    let previous = view.button("上一页");
                    let next = view.button("下一页");
                    let pagination_top = previous
                        .absolute_position()
                        .y
                        .min(next.absolute_position().y);
                    let title_position = title.absolute_position();
                    let title_size = title.size();
                    let hint = card
                        .query_descendants()
                        .match_inherits("Text")
                        .find_all()
                        .into_iter()
                        .filter(|element| {
                            let position = element.absolute_position();
                            (position.x - title_position.x).abs() <= 1.0
                                && position.y > title_position.y + 0.5
                                && position.y < pagination_top
                                && element.size().width > 0.0
                                && element.size().height > 0.0
                        })
                        .min_by(|first, second| {
                            first
                                .absolute_position()
                                .y
                                .total_cmp(&second.absolute_position().y)
                        })
                        .expect("空失败列表应在标题下保留真实状态提示");
                    let card_position = card.absolute_position();
                    let card_size = card.size();
                    let hint_position = hint.absolute_position();
                    let hint_size = hint.size();
                    let title_top_gap = title_position.y - card_position.y;
                    let hint_gap = hint_position.y - (title_position.y + title_size.height);
                    assert!(
                        title_top_gap >= 0.0
                            && title_top_gap <= 24.0
                            && title_size.height <= 30.0,
                        "{context}：标题应贴近卡片顶部且不得拉伸成空白块；卡片 {card_position:?}/{card_size:?}，标题 {title_position:?}/{title_size:?}"
                    );
                    assert!(
                        hint_gap >= -0.5 && hint_gap <= 16.0 && hint_size.height <= 40.0,
                        "{context}：空态提示应紧接标题，不得被卡片剩余空间垂直拉散；标题 {title_position:?}/{title_size:?}，提示 {hint_position:?}/{hint_size:?}，间距 {hint_gap}"
                    );
                    let window_width = f32::from(u16::try_from(width).unwrap());
                    let window_height = f32::from(u16::try_from(height).unwrap());
                    for button in [previous, next] {
                        let position = button.absolute_position();
                        let size = button.size();
                        let bottom = position.y + size.height;
                        let card_bottom_gap = card_position.y + card_size.height - bottom;
                        assert!(
                            position.x >= card_position.x
                                && position.x + size.width
                                    <= card_position.x + card_size.width + 0.5
                                && position.x + size.width <= window_width + 0.5
                                && position.y >= window_height / 2.0
                                && position.y >= hint_position.y + hint_size.height
                                && bottom <= window_height + 0.5
                                && card_bottom_gap >= -0.5
                                && card_bottom_gap <= 24.0,
                            "{context}：分页按钮应完整留在窗口下部、卡片底部，不能跟随介绍上浮或被裁掉；按钮 {position:?}/{size:?}，卡片底部间距 {card_bottom_gap}"
                        );
                    }
                }
            }
        }
    });
}
