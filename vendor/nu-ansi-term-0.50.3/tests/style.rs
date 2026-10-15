use nu_ansi_term::Style;

#[test]
fn manual_instance_style() {
    let s = Style { ..Style::default() };
    assert_eq!(Style::default(), s);

    let s = Style {
        is_underline: false,
        ..Style::default()
    };
    assert_eq!(Style::default(), s);

    let s = Style {
        foreground: None,
        background: None,
        is_bold: false,
        is_dimmed: false,
        is_italic: false,
        is_underline: false,
        is_blink: false,
        is_reverse: false,
        is_hidden: false,
        is_strikethrough: false,
        prefix_with_reset: false,
    };
    assert_eq!(Style::default(), s);
}

#[test]
fn combined_strings_preserve_explicit_reset_and_complete_style() {
    use nu_ansi_term::{AnsiStrings, Color};

    for first in [
        Style::default(),
        Color::Red.normal(),
        Color::Red.reset_before_style(),
    ] {
        let next = Color::Red.reset_before_style();
        let strings = [first.paint("one"), next.paint("two")];
        assert_eq!(
            AnsiStrings(&strings).to_string(),
            format!("{}one\x1b[0m\x1b[31mtwo\x1b[0m", first.prefix())
        );
    }
}

// 字节输出只在 std 特性下提供，与字符串输出共用样式差分路径。
#[cfg(feature = "std")]
#[test]
fn combined_byte_strings_preserve_explicit_reset() {
    use nu_ansi_term::{AnsiByteStrings, Color};

    let strings = [
        Color::Red.paint(&b"one"[..]),
        Color::Red.reset_before_style().paint(&b"two"[..]),
    ];
    let mut output = Vec::new();
    AnsiByteStrings(&strings).write_to(&mut output).unwrap();
    assert_eq!(output, b"\x1b[31mone\x1b[0m\x1b[31mtwo\x1b[0m");
}
