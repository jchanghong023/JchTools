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

/// O-05/O-21：字体编入服务程序，新安装和登录自启均不依赖字体缓存。
pub(crate) const EMBEDDED_FONT: &[u8] =
    include_bytes!("../../../resources/fonts/NotoSansMonoCJKsc-Regular.otf");

/// 向 Slint 字体库注册 OCR 资产中的等宽字体；缺失或损坏均显式报错。进程级
/// 只注册一次（见 `FONT_REGISTERED`），重复调用是幂等的。
///
/// # Errors
/// 字体文件缺失、损坏或 Slint 后端初始化失败。
pub fn register_fonts(font_dir: &Path) -> Result<(), ResultWindowError> {
    let font = font_dir.join("NotoSansMonoCJKsc-Regular.otf");
    ensure_font_registered(
        &FONT_REGISTERED,
        &font,
        |_| Ok(EMBEDDED_FONT.to_vec()),
        register_font_blob,
    )
}

/// 注册字体 blob（共享字体库依赖已创建的平台上下文，spawn_local 初始化
/// Slint 后端）。
fn register_font_blob(bytes: Vec<u8>) -> Result<(), ResultWindowError> {
    slint::spawn_local(std::future::ready(()))
        .map_err(|e| ResultWindowError::FontUnavailable(format!("窗口后端不可用：{e}")))?;
    let blob = slint::fontique_011::fontique::Blob::new(Arc::new(bytes));
    let fonts = slint::fontique_011::shared_collection().register_fonts(blob, None);
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
    pub danger: (u8, u8, u8),
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
            // S9-02（U-06）：错误提示的红色令牌，浅/深两套都提供。
            danger: (229, 72, 77),
            border: (56, 61, 70),
        }
    } else {
        ThemeColors {
            background: (251, 251, 253),
            surface: (255, 255, 255),
            text: (28, 29, 34),
            secondary_text: (91, 95, 107),
            accent: (37, 99, 207),
            danger: (220, 38, 38),
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
        theme.set_danger(slint::Color::from_rgb_u8(
            colors.danger.0,
            colors.danger.1,
            colors.danger.2,
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
                tracing::debug!(
                    component = "ocr_result_window",
                    event = "ocr_result_activation_completed",
                    attempt = u32::from(attempt) + 1,
                    "截图结果窗已切换到前台"
                );
                return;
            }
            if attempt < 2 {
                tracing::warn!(
                    component = "ocr_result_window",
                    event = "ocr_result_activation_retried",
                    stage = "foreground",
                    attempt = u32::from(attempt) + 1,
                    "截图结果窗尚未切换到前台，继续既有重试"
                );
                schedule_activation(window.as_weak(), position, size, attempt + 1);
            } else {
                tracing::warn!(
                    component = "ocr_result_window",
                    event = "ocr_result_activation_fallback",
                    stage = "foreground",
                    attempt = u32::from(attempt) + 1,
                    fallback = "taskbar",
                    "截图结果窗前台切换重试结束，提示从任务栏切换"
                );
                // 信息性提示保持 accent 蓝色（U-06：信息蓝、错误红）。
                window.set_hint_error(false);
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
        // S9-12：非正工作区（宽或高 ≤ 0）显式拒绝，不得经 scaled_extent 钳成
        // 1×1 继续建窗（截图侧 capture() 已同步校验，此处为结果窗侧防线）。
        if ww <= 0 || wh <= 0 {
            return Err(ResultWindowError::CreateFailed(
                "显示器工作区尺寸无效".into(),
            ));
        }
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
                    // S9-02（U-06：错误用红色）：复制失败是错误提示，置错误态
                    // 走 danger 令牌，不再用 accent 蓝色。
                    Err(reason) => {
                        w.set_hint_error(true);
                        w.set_hint(format!("复制失败：{reason}").into());
                    }
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

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
mod tests;
