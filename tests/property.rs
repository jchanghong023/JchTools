//! 属性测试：安全边界不靠枚举用例，而靠随机输入上的不变式。
//! 不变式描述的是需求——不得逃逸根目录、不得产生 Windows 非法名——与具体实现无关。
//! 实现调整导致不变式失败时，说明实现破坏了需求，而不是测试太严。
use jchtools::fsutil;
use proptest::{collection, prelude::*};
use std::path::Component;

/// 关闭失败持久化：默认会在源码旁写 proptest-regressions 文件，违反仓库的 .tmp/ 规则；
/// 失败用例由 panic 消息里的 minimal failing input 直接给出，无需落盘重放。
fn config() -> ProptestConfig {
    let mut config = ProptestConfig::with_cases(256);
    config.failure_persistence = None;
    config
}

/// 任意 Unicode 字符串（可能含控制字符、Windows 非法字符、保留名片段、超长文本）。
fn any_string() -> impl Strategy<Value = String> {
    collection::vec(any::<char>(), 0..24).prop_map(|chars| chars.into_iter().collect())
}
/// 组成路径形状的输入：若干任意段，用随机正/反斜杠连接。
fn any_path_like() -> impl Strategy<Value = String> {
    (collection::vec(any_string(), 1..5), collection::vec(any::<bool>(), 5))
        .prop_map(|(parts, backslash)| {
            let mut raw = String::new();
            for (index, part) in parts.iter().enumerate() {
                if index > 0 { raw.push(if backslash[index - 1] {'\\'} else {'/'}); }
                raw.push_str(part);
            }
            raw
        })
}

proptest! {
    #![proptest_config(config())]
    /// 通过校验的相对路径绝不逃逸根目录：相对、无 `..`/`.`/盘符/根组件，
    /// 且每个输出段自身仍能通过组件校验（safe_relative 应逐段校验过）。
    // 覆盖 S-05
    #[test]
    fn safe_relative_output_never_escapes_root(raw in any_path_like()) {
        if let Ok(path) = fsutil::safe_relative(&raw) {
            prop_assert!(!path.is_absolute(), "输出必须是相对路径：{:?} -> {:?}", raw, path);
            prop_assert!(!path.as_os_str().is_empty(), "输出不得为空：{:?}", raw);
            for component in path.components() {
                prop_assert!(matches!(component, Component::Normal(_)),
                    "不得出现 .. / . / 盘符 / 根组件：{:?} -> {:?}", raw, path);
                let text = component.as_os_str().to_string_lossy();
                prop_assert!(fsutil::validate_component(&text).is_ok(),
                    "输出段 {:?} 未被组件校验拒绝（源输入 {:?}）", text, raw);
            }
        }
    }

    /// 绝对路径（任一斜杠开头）与显式 `..` 段必须被拒绝，无论正反斜杠。
    // 覆盖 S-05
    #[test]
    fn safe_relative_rejects_absolute_and_parent_segments(segments in collection::vec(any_string(), 0..4), backslash in any::<bool>()) {
        let separator = if backslash {'\\'} else {'/'};
        let body = segments.join(&separator.to_string());
        let absolute = format!("{separator}{body}");
        prop_assert!(fsutil::safe_relative(&absolute).is_err(), "绝对路径必须拒绝：{:?}", absolute);
        let dotted = format!("a{separator}..{separator}b");
        prop_assert!(fsutil::safe_relative(&dotted).is_err(), ".. 段必须拒绝：{:?}", dotted);
    }

    /// 通过校验的组件必然满足 Windows 兼容不变式（拒绝方向由后续定向属性覆盖）。
    // 覆盖 S-05
    #[test]
    fn validate_component_ok_implies_windows_safe(s in any_string()) {
        if fsutil::validate_component(&s).is_ok() {
            prop_assert!(!s.is_empty() && s != "." && s != "..", "{:?}", s);
            let last = s.chars().last().unwrap();
            prop_assert!(last != '.' && !last.is_whitespace(), "尾随点/空白必须已被拒绝：{:?}", s);
            prop_assert!(!s.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)), "{:?}", s);
            let stem = s.split('.').next().unwrap().to_uppercase();
            prop_assert!(!["CON", "PRN", "AUX", "NUL", "CLOCK$"].contains(&stem.as_str()), "{:?}", s);
            let com_lpt = stem.starts_with("COM") || stem.starts_with("LPT");
            prop_assert!(!com_lpt || stem.chars().count() != 4
                || !stem.chars().last().is_some_and(|c| "123456789¹²³".contains(c)), "{:?}", s);
            prop_assert!(s.encode_utf16().count() <= 255, "{:?}", s);
        }
    }

    /// 尾随点 / 各类 Unicode 空白、Windows 非法字符、控制字符注入后必须被拒绝。
    // 覆盖 S-05
    #[test]
    fn validate_component_rejects_trailing_and_injected_chars(
        s in any_string(),
        trailer in proptest::sample::select(vec!['.', ' ', '\t', '\u{a0}', '\u{3000}']),
        bad in proptest::sample::select(vec!['<', '>', ':', '"', '/', '\\', '|', '?', '*', '\u{0}', '\u{7}']),
        position in any::<usize>(),
    ) {
        // 注意：prop_assert! 会把断言表达式拼进格式串，表达式内的字符串字面量
        // 不能使用 {var} 隐式捕获，必须用位置参数。
        prop_assert!(fsutil::validate_component(&format!("{}{}", s, trailer)).is_err(), "尾随 {:?}", trailer);
        let mut injected = s.clone();
        // String::insert 需要字节边界；position 是字符序号，先换算成字节偏移。
        let byte_index = s.char_indices().nth(position % (s.chars().count() + 1)).map_or(s.len(), |(b, _)| b);
        injected.insert(byte_index, bad);
        prop_assert!(fsutil::validate_component(&injected).is_err(), "注入 {:?}", bad);
    }

    /// 官方保留设备名的拒绝边界：COM0/LPT0 不是保留名；COM1-9 / LPT1-9（含上标 ¹²³ 变体）是。
    // 覆盖 S-05
    #[test]
    fn reserved_device_name_boundary(digit in 0u8..10, base_is_com in any::<bool>()) {
        let name = format!("{}{digit}", if base_is_com {"com"} else {"lpt"});
        let expect_err = digit >= 1;
        prop_assert_eq!(fsutil::validate_component(&name).is_err(), expect_err, "{:?}", name);
        let with_ext = format!("{}.txt", name);
        prop_assert_eq!(fsutil::validate_component(&with_ext).is_err(), expect_err, "{}", with_ext);
        for superscript in ["¹", "²", "³"] {
            let com = format!("com{}", superscript);
            let lpt = format!("lpt{}", superscript);
            prop_assert!(fsutil::validate_component(&com).is_err(), "{}", com);
            prop_assert!(fsutil::validate_component(&lpt).is_err(), "{}", lpt);
        }
    }
}
