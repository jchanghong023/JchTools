//! Slint OCR 结果窗：物理工作区定位、布局保真文字和复制交互。
//! 设置窗与结果窗由同一份 Slint 文件编译，供独立后台服务使用。

use std::path::Path;
use std::sync::Arc;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;

// Slint 1.17.1 生成代码在内部嵌入桩发射 todo!，并在生成的属性升级路径使用
// unwrap；与主窗口相同，仅隔离生成模块，不放宽本文件的业务实现。
#[allow(clippy::unwrap_used, clippy::todo)]
mod generated_ui {
    slint::include_modules!();
}
pub use generated_ui::*;

/// 结果窗句柄：持有 Slint 组件，Drop 时自动释放（清文本即清引用）。
pub struct ResultWindowHandle {
    inner: ResultWindow,
}

/// 展示结果窗的错误（去敏：不含截图内容或用户路径，O-30）。
#[derive(Debug)]
pub enum ResultWindowError {
    /// 等宽字体资产缺失或无法注册（O-21：不得静默降级）。
    FontUnavailable(String),
    /// 窗口创建失败。
    CreateFailed(String),
}

impl std::fmt::Display for ResultWindowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResultWindowError::FontUnavailable(m) => {
                write!(f, "结果窗等宽字体不可用：{m}")
            }
            ResultWindowError::CreateFailed(m) => write!(f, "结果窗创建失败：{m}"),
        }
    }
}

impl std::error::Error for ResultWindowError {}

/// 向 Slint 字体库注册 OCR 资产中的等宽字体；缺失或损坏均显式报错。
///
/// # Errors
/// 字体文件缺失、损坏或 Slint 后端初始化失败。
pub fn register_fonts(font_dir: &Path) -> Result<(), ResultWindowError> {
    let font = font_dir.join("NotoSansMonoCJKsc-Regular.otf");
    let bytes = std::fs::read(font)
        .map_err(|_| ResultWindowError::FontUnavailable("字体资产缺失或无法读取".into()))?;
    // 共享字体库依赖已创建的平台上下文，spawn_local 初始化 Slint 后端。
    slint::spawn_local(std::future::ready(()))
        .map_err(|e| ResultWindowError::FontUnavailable(format!("窗口后端不可用：{e}")))?;
    let blob = slint::fontique_010::fontique::Blob::new(Arc::new(bytes));
    let fonts = slint::fontique_010::shared_collection().register_fonts(blob, None);
    if fonts.is_empty() {
        return Err(ResultWindowError::FontUnavailable("字体文件损坏".into()));
    }
    Ok(())
}

fn activate_result_window(
    window: &ResultWindow,
    position: slint::PhysicalPosition,
    size: slint::PhysicalSize,
) {
    if !window.window().is_visible() {
        return;
    }
    // 窗口首次显示后才能得到目标显示器的 DPI；此时重新指定物理尺寸，
    // 避免首次建窗的逻辑尺寸按 150% 缩放成超出工作区的大窗口。
    window.window().set_size(size);
    window.window().set_position(position);
    let handle = window.window().window_handle();
    if let Ok(native) = handle.window_handle() {
        if let RawWindowHandle::Win32(win) = native.as_raw() {
            let hwnd = win.hwnd.get() as windows_sys::Win32::Foundation::HWND;
            // SAFETY: HWND belongs to this live Slint window; no permanent topmost flag is set.
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
                    hwnd,
                    windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOW,
                );
                windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
            }
        }
    }
}

// Preserve TextSnap's f32 rounding and truncation (including large work areas).
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn scaled_extent(extent: i32) -> i32 {
    ((extent as f32 * 0.8) as i32).clamp(1, extent.max(1))
}

impl ResultWindowHandle {
    /// 创建并显示结果窗。
    ///
    /// `monitor_work_area` 为截图显示器的物理像素工作区
    /// (x, y, width, height)；初始尺寸取其 80%（O-21）。
    ///
    /// `on_copy_all` 在用户点击「复制全部」时收到全文：由调用方写剪贴板，
    /// 返回 Ok(()) 后窗口关闭（O-22：复制全部后关窗）。
    ///
    /// # Errors
    /// 窗口创建失败。
    pub fn show_for_monitor<F>(
        text: &str,
        monitor_work_area: (i32, i32, i32, i32),
        on_copy_all: F,
    ) -> Result<Self, ResultWindowError>
    where
        F: Fn(&str) -> Result<(), String> + 'static,
    {
        let window =
            ResultWindow::new().map_err(|e| ResultWindowError::CreateFailed(e.to_string()))?;
        window.set_ocr_text(text.into());

        // Slint 的窗口 API 使用物理像素；尺寸与位置取截图屏幕自己的工作区。
        let (wx, wy, ww, wh) = monitor_work_area;
        let width = scaled_extent(ww);
        let height = scaled_extent(wh);
        let size = slint::PhysicalSize::new(
            u32::try_from(width)
                .map_err(|_| ResultWindowError::CreateFailed("窗口宽度无效".into()))?,
            u32::try_from(height)
                .map_err(|_| ResultWindowError::CreateFailed("窗口高度无效".into()))?,
        );
        window.window().set_size(size);
        window.window().set_position(slint::PhysicalPosition::new(
            wx + (ww - width) / 2,
            wy + (wh - height) / 2,
        ));

        let weak = window.as_weak();
        window.on_copy_all(move || {
            if let Some(w) = weak.upgrade() {
                let full = w.get_ocr_text().to_string();
                match on_copy_all(&full) {
                    Ok(()) => {
                        w.set_ocr_text("".into());
                        let _ = w.hide();
                    }
                    Err(reason) => w.set_hint(format!("复制失败：{reason}").into()),
                }
            }
        });
        let close = window.as_weak();
        window.on_close_requested(move || {
            if let Some(w) = close.upgrade() {
                w.set_ocr_text("".into());
                let _ = w.hide();
            }
        });
        let native_close = window.as_weak();
        window.window().on_close_requested(move || {
            if let Some(w) = native_close.upgrade() {
                w.set_ocr_text("".into());
                let _ = w.hide();
            }
            slint::CloseRequestResponse::KeepWindowShown
        });

        window
            .show()
            .map_err(|e| ResultWindowError::CreateFailed(e.to_string()))?;
        let position = slint::PhysicalPosition::new(wx + (ww - width) / 2, wy + (wh - height) / 2);
        // winit 首次事件循环迭代后才保证 HWND 可用。
        let activate = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(1), move || {
            if let Some(w) = activate.upgrade() {
                activate_result_window(&w, position, size);
            }
        });
        Ok(Self { inner: window })
    }

    /// Slint 事件循环的单次驱动入口由服务消息循环集成（服务侧轮询），
    /// 此处仅暴露组件弱引用供事件循环注册。
    #[must_use]
    pub fn as_weak(&self) -> slint::Weak<ResultWindow> {
        self.inner.as_weak()
    }

    /// 窗口是否仍可见。
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.inner.window().is_visible()
    }

    /// 主动关闭并清空（新截图前隐藏旧结果，O-20）。
    pub fn hide_and_clear(&self) {
        self.inner.set_ocr_text("".into());
        let _ = self.inner.hide();
    }
}

#[cfg(test)]
mod tests {
    use super::{ResultWindow, ResultWindowHandle};
    use slint::ComponentHandle;
    use std::cell::RefCell;
    use std::rc::Rc;

    // 覆盖 O-21/O-22：正文必须实际渲染，复制须保留原文。
    #[test]
    fn ocr_text_changes_visible_body_pixels() {
        assert!(
            slint::platform::set_platform(Box::new(i_slint_backend_testing::TestingBackend::new(
                i_slint_backend_testing::TestingBackendOptions {
                    renderer_name: Some("software".into()),
                    ..Default::default()
                },
            )))
            .is_ok(),
            "测试后端应只初始化一次"
        );

        let window = ResultWindow::new().unwrap_or_else(|error| panic!("结果窗创建失败：{error}"));
        window.window().set_size(slint::PhysicalSize::new(640, 400));
        window
            .show()
            .unwrap_or_else(|error| panic!("结果窗显示失败：{error}"));
        let blank = window
            .window()
            .take_snapshot()
            .unwrap_or_else(|error| panic!("空白窗口渲染失败：{error}"));
        window.set_ocr_text("OCR 可见正文 ABC 123".into());
        let filled = window
            .window()
            .take_snapshot()
            .unwrap_or_else(|error| panic!("文字窗口渲染失败：{error}"));

        assert_eq!((blank.width(), blank.height()), (640, 400));
        assert_eq!((filled.width(), filled.height()), (640, 400));
        let row_bytes = 640 * 4;
        let changed = blank
            .as_bytes()
            .chunks_exact(row_bytes)
            .zip(filled.as_bytes().chunks_exact(row_bytes))
            .enumerate()
            .filter(|(y, _)| (50..350).contains(y))
            .map(|(_, (before, after))| {
                before
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(after.as_chunks::<4>().0.iter())
                    .filter(|(a, b)| a != b)
                    .count()
            })
            .sum::<usize>();
        assert!(
            changed > 100,
            "正文区域未渲染 OCR 文字：仅 {changed} 个像素变化"
        );

        let copied = Rc::new(RefCell::new(String::new()));
        let copied_for_callback = Rc::clone(&copied);
        let sized = ResultWindowHandle::show_for_monitor(
            "OCR 中文  ABC\n  缩进",
            (10, 20, 1920, 1080),
            move |text| {
                *copied_for_callback.borrow_mut() = text.to_owned();
                Ok(())
            },
        )
        .unwrap_or_else(|error| panic!("工作区结果窗创建失败：{error}"));
        let sized_window = sized
            .as_weak()
            .upgrade()
            .unwrap_or_else(|| panic!("窗口句柄已失效"));
        assert_eq!(
            sized_window.window().size(),
            slint::PhysicalSize::new(1536, 864)
        );
        sized_window.invoke_copy_all();
        assert_eq!(*copied.borrow(), "OCR 中文  ABC\n  缩进");
        assert!(!sized.is_visible());
    }
}
