//! `result_window` 内联测试模块（字体幂等注册、热键归一化、主题令牌与结果窗渲染回归）；自 result_window.rs 搬移并顶格化。

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
    // S9-02：danger 令牌浅/深两套都提供且互不相同（错误红色，U-06）。
    assert_eq!(theme_colors(true).danger, (229, 72, 77));
    assert_eq!(theme_colors(false).danger, (220, 38, 38));
    assert_ne!(theme_colors(true).danger, theme_colors(true).accent);
    assert_ne!(
        theme_colors(false).danger,
        theme_colors(false).accent,
        "错误红不得与信息蓝相同（U-06）"
    );
}

// 覆盖 S9-02（U-06：错误用红色）：复制失败必须置 hint 错误态（danger
// 令牌），信息性提示保持非错误态（accent）。
#[test]
fn copy_failure_hint_uses_error_state() {
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
    let handle =
        ResultWindowHandle::show_for_monitor("OCR 文本", (10, 20, 1920, 1080), |_| Ok(()))
            .unwrap_or_else(|error| panic!("结果窗创建失败：{error}"));
    let window = handle
        .as_weak()
        .upgrade()
        .unwrap_or_else(|| panic!("窗口句柄已失效"));
    assert!(!window.get_hint_error(), "初始提示不是错误态");
    drop(window);
    let failing = ResultWindowHandle::show_for_monitor("OCR 文本", (10, 20, 1920, 1080), |_| {
        Err("剪贴板被其他程序占用".to_string())
    })
    .unwrap_or_else(|error| panic!("结果窗创建失败：{error}"));
    let failure_window = failing
        .as_weak()
        .upgrade()
        .unwrap_or_else(|| panic!("窗口句柄已失效"));
    failure_window.invoke_copy_all();
    assert!(
        failure_window.get_hint().contains("复制失败"),
        "复制失败须给出失败提示"
    );
    assert!(
        failure_window.get_hint_error(),
        "复制失败必须置错误态走 danger 红色（U-06/S9-02）"
    );
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
    let window = SettingsWindow::new().unwrap_or_else(|error| panic!("设置窗创建失败：{error}"));
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

// 覆盖 S9-12：非正工作区尺寸（宽或高 ≤ 0）必须在 show_for_monitor 入口
// 显式拒绝（CreateFailed），不得经 scaled_extent 钳成 1×1 继续建窗。
#[test]
fn non_positive_work_area_rejects_result_window() {
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
    for area in [(10, 20, 0, 0), (10, 20, 1920, -5), (10, 20, -1920, 1080)] {
        let outcome = ResultWindowHandle::show_for_monitor("文本", area, |_| Ok(()));
        assert!(
            matches!(outcome, Err(ResultWindowError::CreateFailed(_))),
            "非正工作区 {area:?} 必须拒绝建窗，不得钳成 1×1"
        );
    }
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
