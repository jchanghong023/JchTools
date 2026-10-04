//! Slint OCR 结果窗：物理工作区定位、布局保真文字和复制交互。
//! 设置窗与结果窗由同一份 Slint 文件编译，供独立后台服务使用。

use std::path::Path;
use std::sync::{Arc, OnceLock};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;

// Slint 1.17.1 生成代码在内部嵌入桩发射 todo!，并在生成的属性升级路径使用
// unwrap、生成大量对外不可达的 pub 项；与主窗口相同，仅隔离生成模块，
// 不放宽本文件的业务实现。
#[allow(
    clippy::unwrap_used,
    clippy::todo,
    unreachable_pub,
    single_use_lifetimes
)]
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

/// 进程级字体注册状态（O-13）：模型常驻于服务生命周期，字体文件在服务生命
/// 周期内不变，注册一次即可。「取消 → 后台重载」路径每次 ModelLoaded(Ok) 都
/// 会走到这里——若每次都重新读取 16MB 字体并注册新 blob，fontique 是否去重
/// 并无保证，存在无界的重复注册内存增长风险；第二次及以后调用直接跳过读取
/// 与注册。
static FONT_REGISTERED: OnceLock<()> = OnceLock::new();

/// 向 Slint 字体库注册 OCR 资产中的等宽字体；缺失或损坏均显式报错。进程级
/// 只注册一次（见 `FONT_REGISTERED`），重复调用是幂等的。
///
/// # Errors
/// 字体文件缺失、损坏或 Slint 后端初始化失败。
pub fn register_fonts(font_dir: &Path) -> Result<(), ResultWindowError> {
    let font = font_dir.join("NotoSansMonoCJKsc-Regular.otf");
    ensure_font_registered(&FONT_REGISTERED, &font, read_font_file, register_font_blob)
}

/// 读取字体文件（独立步骤；幂等测试注入计数读取器验证只读一次）。
fn read_font_file(font: &Path) -> Result<Vec<u8>, ResultWindowError> {
    std::fs::read(font)
        .map_err(|_| ResultWindowError::FontUnavailable("字体资产缺失或无法读取".into()))
}

/// 注册字体 blob（共享字体库依赖已创建的平台上下文，spawn_local 初始化
/// Slint 后端）。
fn register_font_blob(bytes: Vec<u8>) -> Result<(), ResultWindowError> {
    slint::spawn_local(std::future::ready(()))
        .map_err(|e| ResultWindowError::FontUnavailable(format!("窗口后端不可用：{e}")))?;
    let blob = slint::fontique_010::fontique::Blob::new(Arc::new(bytes));
    let fonts = slint::fontique_010::shared_collection().register_fonts(blob, None);
    if fonts.is_empty() {
        return Err(ResultWindowError::FontUnavailable("字体文件损坏".into()));
    }
    Ok(())
}

/// 幂等注册核心：`registered` 已置位时跳过读取与注册；仅成功注册后才置位，
/// 失败不置位，下次调用（如用户重试加载模型）仍会重新读取并注册。
fn ensure_font_registered(
    registered: &OnceLock<()>,
    font: &Path,
    load: impl FnOnce(&Path) -> Result<Vec<u8>, ResultWindowError>,
    register: impl FnOnce(Vec<u8>) -> Result<(), ResultWindowError>,
) -> Result<(), ResultWindowError> {
    if registered.get().is_some() {
        return Ok(());
    }
    let bytes = load(font)?;
    let outcome = register(bytes);
    if outcome.is_ok() {
        let _ = registered.set(());
    }
    outcome
}

/// US 键盘布局 Shift+符号的反向映射表（O-14 热键录制）：Slint 录制到的
/// `event.text` 是布局映射后的符号（如 Shift+7 → "&"），而热键域（worker 的
/// `tray::parse_hotkey` 与主程序 gui.rs 同口径）只认未修饰的原键码，录制时必须
/// 把符号还原成原键，否则 Ctrl+Shift+7 之类组合会报「快捷键主键不支持」。
/// 左列为 Shift 修饰后的符号，右列为原键；共 21 对（数字行 10 + 其余 11）。
/// 主程序 gui.rs 侧维护同一张表，两边必须逐对一致。
const SHIFT_SYMBOL_BASE_KEYS: [(&str, &str); 21] = [
    ("!", "1"),
    ("@", "2"),
    ("#", "3"),
    ("$", "4"),
    ("%", "5"),
    ("^", "6"),
    ("&", "7"),
    ("*", "8"),
    ("(", "9"),
    (")", "0"),
    ("~", "`"),
    ("_", "-"),
    ("+", "="),
    ("{", "["),
    ("}", "]"),
    ("|", "\\"),
    (":", ";"),
    ("\"", "'"),
    ("<", ","),
    (">", "."),
    ("?", "/"),
];

/// 录制热键的主键归一化（O-14）：Shift 按下且字符命中上表时还原为原键；
/// Shift+字母（录制到的是大写字母）、无 Shift 的按键与命名键原样返回。
#[must_use]
pub fn normalize_shift_key(shift: bool, key: &str) -> String {
    if shift {
        if let Some((_, base)) = SHIFT_SYMBOL_BASE_KEYS
            .iter()
            .find(|(symbol, _)| *symbol == key)
        {
            return (*base).to_owned();
        }
    }
    key.to_owned()
}

/// 结果/进度/设置窗共用的主题颜色值。窗口自身以 Slint `Palette` 为默认来源，
/// 此公开辅助 API 供服务接线在需要显式同步主题时使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeColors {
    pub background: (u8, u8, u8),
    pub surface: (u8, u8, u8),
    pub text: (u8, u8, u8),
    pub secondary_text: (u8, u8, u8),
    pub accent: (u8, u8, u8),
    pub border: (u8, u8, u8),
}

#[must_use]
pub fn theme_colors(dark: bool) -> ThemeColors {
    if dark {
        ThemeColors {
            background: (21, 23, 27),
            surface: (30, 33, 39),
            text: (238, 241, 248),
            secondary_text: (167, 174, 187),
            accent: (106, 162, 255),
            border: (56, 61, 70),
        }
    } else {
        ThemeColors {
            background: (251, 251, 253),
            surface: (255, 255, 255),
            text: (28, 29, 34),
            secondary_text: (91, 95, 107),
            accent: (37, 99, 207),
            border: (217, 219, 227),
        }
    }
}

macro_rules! apply_theme_tokens {
    ($window:expr, $dark:expr) => {{
        let colors = theme_colors($dark);
        let theme = SnapTheme::get($window);
        theme.set_dark($dark);
        theme.set_background(slint::Color::from_rgb_u8(
            colors.background.0,
            colors.background.1,
            colors.background.2,
        ));
        theme.set_surface(slint::Color::from_rgb_u8(
            colors.surface.0,
            colors.surface.1,
            colors.surface.2,
        ));
        theme.set_text(slint::Color::from_rgb_u8(
            colors.text.0,
            colors.text.1,
            colors.text.2,
        ));
        theme.set_text_secondary(slint::Color::from_rgb_u8(
            colors.secondary_text.0,
            colors.secondary_text.1,
            colors.secondary_text.2,
        ));
        theme.set_accent(slint::Color::from_rgb_u8(
            colors.accent.0,
            colors.accent.1,
            colors.accent.2,
        ));
        theme.set_border(slint::Color::from_rgb_u8(
            colors.border.0,
            colors.border.1,
            colors.border.2,
        ));
    }};
}

/// 读取 Windows 当前用户的应用主题；非 Windows 后端保持浅色默认。
#[cfg(windows)]
fn system_theme_is_dark() -> bool {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let name: Vec<u16> = "AppsUseLightTheme"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut value = 1u32;
    let mut size = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(u32::MAX);
    // SAFETY: path/name 均为 NUL 结尾的 UTF-16；value/size 是配套 DWORD 输出缓冲区。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut value).cast(),
            &raw mut size,
        )
    };
    status == 0 && value == 0
}

#[cfg(not(windows))]
fn system_theme_is_dark() -> bool {
    false
}

/// 将 worker 窗口主题同步到当前用户系统主题，供服务接线使用。
pub fn apply_system_theme(window: &ResultWindow) {
    apply_theme_tokens!(window, system_theme_is_dark());
}

/// 将 worker 进度窗主题同步到当前用户系统主题，供服务接线使用。
pub fn apply_progress_theme(window: &ProgressWindow) {
    apply_theme_tokens!(window, system_theme_is_dark());
}

/// 将 worker 设置窗主题同步到当前用户系统主题，供服务接线使用。
pub fn apply_settings_theme(window: &SettingsWindow) {
    apply_theme_tokens!(window, system_theme_is_dark());
}

fn activate_result_window(
    window: &ResultWindow,
    position: slint::PhysicalPosition,
    size: slint::PhysicalSize,
) -> bool {
    if !window.window().is_visible() {
        return false;
    }
    // 窗口首次显示后才能得到目标显示器的 DPI；此时重新指定物理尺寸，
    // 避免首次建窗的逻辑尺寸按 150% 缩放成超出工作区的大窗口。
    window.window().set_size(size);
    window.window().set_position(position);
    let handle = window.window().window_handle();
    if let Ok(native) = handle.window_handle() {
        if let RawWindowHandle::Win32(win) = native.as_raw() {
            let hwnd = win.hwnd.get() as windows_sys::Win32::Foundation::HWND;
            // SAFETY: HWND 属于当前存活的 Slint 窗口；显示不改变置顶等持久标志。
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
                    hwnd,
                    windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOW,
                );
            }
            // SAFETY: 同一存活窗口句柄；前台切换失败仅表现为窗口不被前置，无副作用。
            let foreground =
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd) }
                    != 0;
            return foreground;
        }
    }
    false
}

fn schedule_activation(
    window: slint::Weak<ResultWindow>,
    position: slint::PhysicalPosition,
    size: slint::PhysicalSize,
    attempt: u8,
) {
    let delay = if attempt == 0 { 1 } else { 16 };
    slint::Timer::single_shot(std::time::Duration::from_millis(delay), move || {
        if let Some(window) = window.upgrade() {
            if activate_result_window(&window, position, size) {
                return;
            }
            if attempt < 2 {
                schedule_activation(window.as_weak(), position, size, attempt + 1);
            } else {
                window.set_hint("结果窗已显示，请从任务栏切换到截图 OCR 结果".into());
            }
        }
    });
}

// Preserve TextSnap's f32 rounding and truncation (including large work areas).
// [quality-baseline approved 2026-10-03] 冻结行为换算豁免，经用户裁定保留
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
        apply_system_theme(&window);
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
        schedule_activation(window.as_weak(), position, size, 0);
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

    /// 刷新当前结果窗的系统主题；服务定时调用以覆盖运行期间的主题切换。
    pub fn refresh_theme(&self) {
        apply_system_theme(&self.inner);
    }

    /// 主动关闭并清空（新截图前隐藏旧结果，O-20）。
    pub fn hide_and_clear(&self) {
        self.inner.set_ocr_text("".into());
        let _ = self.inner.hide();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_font_registered, normalize_shift_key, theme_colors, ResultWindow, ResultWindowError,
        ResultWindowHandle, SettingsWindow,
    };
    use slint::ComponentHandle;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, OnceLock};

    // 覆盖 O-13（修复3回归）：字体注册进程级只执行一次——「取消 → 后台重载」
    // 路径每次 ModelLoaded(Ok) 都会调 register_fonts，若每次都重新读取 16MB
    // 字体并注册新 blob，存在无界重复注册的内存增长风险。注入计数读取器
    // 断言连续调用两次只读一次文件；修复前（无幂等层）读取器被调用两次。
    #[test]
    fn font_registration_reads_file_once_across_reloads() {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let registers = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let registered = OnceLock::new();
        let font = std::path::Path::new("fake-font.otf");

        let make_load = || {
            let reads = Arc::clone(&reads);
            move |_font: &std::path::Path| {
                reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(vec![0u8; 4])
            }
        };
        let make_register = || {
            let registers = Arc::clone(&registers);
            move |_bytes: Vec<u8>| {
                registers.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        };

        ensure_font_registered(&registered, font, make_load(), make_register())
            .expect("首次注册应成功");
        ensure_font_registered(&registered, font, make_load(), make_register())
            .expect("第二次调用（模拟重载路径）应幂等成功");
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "字体文件只允许读取一次（O-13：字体与模型同生命周期，进程级注册一次）"
        );
        assert_eq!(
            registers.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "字体 blob 只允许注册一次"
        );
    }

    // 覆盖修复3的失败语义：注册失败不置位，下次调用仍重新读取并注册
    // （用户重试加载模型时字体有第二次机会，失败不被缓存）。
    #[test]
    fn failed_font_registration_is_not_cached() {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let make_load = || {
            let reads = Arc::clone(&reads);
            move |_font: &std::path::Path| {
                reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(vec![0u8; 4])
            }
        };
        let failing = move |_bytes: Vec<u8>| -> Result<(), ResultWindowError> {
            Err(ResultWindowError::FontUnavailable("字体文件损坏".into()))
        };
        let registered = OnceLock::new();
        let font = std::path::Path::new("fake-font.otf");
        let outcome = ensure_font_registered(&registered, font, make_load(), failing);
        assert!(outcome.is_err(), "首次注册失败应报错");
        assert!(
            registered.get().is_none(),
            "失败不得置位幂等标记（否则用户重试永远拿不到字体）"
        );
        let succeeded = ensure_font_registered(&registered, font, make_load(), |_| Ok(()));
        assert!(succeeded.is_ok(), "重试注册应成功");
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "失败未缓存：重试应重新读取字体文件"
        );
    }

    // 覆盖 O-14（E-4 回归）：US 布局 Shift+符号反向映射表全量核对——
    // 上下两行各 10/11 对，Shift 按下时符号必须还原为未修饰的原键，
    // 否则录制 Ctrl+Shift+7 之类组合会被 parse_hotkey 判为「主键不支持」。
    #[test]
    fn shift_symbols_map_back_to_base_keys() {
        let shifted = [
            ("!", "1"),
            ("@", "2"),
            ("#", "3"),
            ("$", "4"),
            ("%", "5"),
            ("^", "6"),
            ("&", "7"),
            ("*", "8"),
            ("(", "9"),
            (")", "0"),
            ("~", "`"),
            ("_", "-"),
            ("+", "="),
            ("{", "["),
            ("}", "]"),
            ("|", "\\"),
            (":", ";"),
            ("\"", "'"),
            ("<", ","),
            (">", "."),
            ("?", "/"),
        ];
        assert_eq!(shifted.len(), 21, "映射表应为 21 对（数字行 10 + 其余 11）");
        for (symbol, base) in shifted {
            assert_eq!(
                normalize_shift_key(true, symbol),
                base,
                "Shift+{symbol} 应还原为 {base}"
            );
        }
    }

    // 覆盖 O-14（E-4 回归）：映射只在「Shift 按下且命中符号表」时生效——
    // Shift+字母是已大写字母不映射；无 Shift 的符号、数字与命名键原样返回。
    #[test]
    fn non_shifted_or_letter_keys_pass_through() {
        assert_eq!(normalize_shift_key(true, "A"), "A", "Shift+字母不映射");
        assert_eq!(normalize_shift_key(true, "N"), "N");
        assert_eq!(normalize_shift_key(true, "F5"), "F5", "功能键不映射");
        assert_eq!(
            normalize_shift_key(true, "Backspace"),
            "Backspace",
            "命名键不映射"
        );
        assert_eq!(
            normalize_shift_key(false, "&"),
            "&",
            "无 Shift 的符号不映射"
        );
        assert_eq!(normalize_shift_key(false, "7"), "7");
        assert_eq!(normalize_shift_key(false, "Backspace"), "Backspace");
        assert_eq!(normalize_shift_key(true, ""), "", "空串原样返回");
    }

    // 覆盖 P-04/O-21 回归：worker 窗口必须提供成对的浅色/深色 token，
    // 不能把深色系统下的结果窗固定成白底黑字。
    #[test]
    fn theme_tokens_have_distinct_light_and_dark_palettes() {
        assert_ne!(theme_colors(false), theme_colors(true));
        assert_eq!(theme_colors(true).background, (21, 23, 27));
        assert_eq!(theme_colors(false).background, (251, 251, 253));
    }

    // 覆盖 O-14（E-4 集成）：设置窗的录制归一化回调接到 Rust 映射函数后，
    // 经 slint 回调入口输入 Shift+"&"（US 布局的 Shift+7）应得到 "7"，
    // 组装出的 Ctrl+Shift+7 落在 parse_hotkey 的键域内。
    #[test]
    fn settings_window_hotkey_callback_restores_shifted_digit() {
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
        let window =
            SettingsWindow::new().unwrap_or_else(|error| panic!("设置窗创建失败：{error}"));
        // 与 service.rs open_settings 的生产接线一致。
        window.on_normalize_shift_key(|shift, key| normalize_shift_key(shift, &key).into());
        assert_eq!(
            window.invoke_normalize_shift_key(true, "&".into()).as_str(),
            "7"
        );
        assert_eq!(
            window
                .invoke_normalize_shift_key(false, "&".into())
                .as_str(),
            "&"
        );
        assert_eq!(
            window.invoke_normalize_shift_key(true, "A".into()).as_str(),
            "A"
        );
        // O-30/UX：长诊断默认折叠，主动展开后仍可收起，不挤出底部操作。
        assert!(!window.get_status_detail_open());
        window.set_status_message("组件未初始化；请前往设置页检查 Xberg、字体和许可文件".into());
        window.set_status_detail_open(true);
        assert!(window.get_status_detail_open());
        window.set_status_detail_open(false);
        assert!(!window.get_status_detail_open());
        let draft = format!(
            "Ctrl+Shift+{}",
            window.invoke_normalize_shift_key(true, "&".into())
        );
        assert_eq!(draft, "Ctrl+Shift+7");
    }

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
