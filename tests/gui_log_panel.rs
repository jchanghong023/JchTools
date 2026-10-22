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

// 测试按主题拆分为子模块；本文件保留共享夹具、辅助函数与模块声明。
#[path = "gui_log_panel/diagnostic_details.rs"]
mod diagnostic_details;
#[path = "gui_log_panel/layout_regressions.rs"]
mod layout_regressions;
#[path = "gui_log_panel/log_expansion.rs"]
mod log_expansion;
#[path = "gui_log_panel/ui_findings.rs"]
mod ui_findings;
