//! 托盘热键键域解析：原 TextSnap 修饰键 + 主键的固定映射（纯逻辑，无 Win32 依赖）。

/// 原 TextSnap 热键键域：修饰键 + 字母/数字/F1～F24/固定命名键；
/// 解析只做固定映射，注册是否可用由 Win32 RegisterHotKey 决定。
pub(crate) fn parse_hotkey(raw: &str) -> Result<(u32, u32), String> {
    let mut flags = 0x4000;
    let mut key = None;
    for part in raw.split('+') {
        let part = part.trim().to_ascii_uppercase();
        let modifier = match part.as_str() {
            "CTRL" | "CONTROL" => 2,
            "ALT" => 1,
            "SHIFT" => 4,
            "WIN" | "META" => 8,
            _ => 0,
        };
        if modifier != 0 {
            if flags & modifier != 0 {
                return Err("快捷键包含重复修饰键".into());
            }
            flags |= modifier;
            continue;
        }
        if key.is_some() {
            return Err("快捷键不能包含多个主键".into());
        }
        let bytes = part.as_bytes();
        key = if bytes.len() == 1 && (bytes[0].is_ascii_uppercase() || bytes[0].is_ascii_digit()) {
            Some(u32::from(bytes[0]))
        } else if let Some(n) = part.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
            (1..=24).contains(&n).then_some(0x6f + n)
        } else {
            match part.as_str() {
                "BACKSPACE" => Some(0x08),
                "TAB" => Some(0x09),
                "RETURN" => Some(0x0d),
                "ESCAPE" => Some(0x1b),
                "SPACE" => Some(0x20),
                "PAGEUP" => Some(0x21),
                "PAGEDOWN" => Some(0x22),
                "END" => Some(0x23),
                "HOME" => Some(0x24),
                "LEFT" => Some(0x25),
                "UP" => Some(0x26),
                "RIGHT" => Some(0x27),
                "DOWN" => Some(0x28),
                "INSERT" => Some(0x2d),
                "DELETE" => Some(0x2e),
                _ => None,
            }
        };
        if key.is_none() {
            return Err("快捷键主键不支持".into());
        }
    }
    if flags == 0x4000 || key.is_none() {
        return Err("快捷键需包含修饰键与有效主键".into());
    }
    Ok((flags, key.unwrap_or_default()))
}
