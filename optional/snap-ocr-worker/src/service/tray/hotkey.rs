//! 托盘热键键域解析：原 TextSnap 修饰键 + 主键的固定映射，不调用 Win32 API。

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, VK_BACK, VK_DELETE, VK_DOWN, VK_END,
    VK_ESCAPE, VK_F1, VK_HOME, VK_INSERT, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT,
    VK_SPACE, VK_TAB, VK_UP,
};

/// 原 TextSnap 热键键域：修饰键 + 字母/数字/F1～F24/固定命名键；
/// 解析只做固定映射，注册是否可用由 Win32 RegisterHotKey 决定。
pub(crate) fn parse_hotkey(raw: &str) -> Result<(u32, u32), String> {
    let mut flags = MOD_NOREPEAT;
    let mut key = None;
    for part in raw.split('+') {
        let part = part.trim().to_ascii_uppercase();
        let modifier = match part.as_str() {
            "CTRL" | "CONTROL" => MOD_CONTROL,
            "ALT" => MOD_ALT,
            "SHIFT" => MOD_SHIFT,
            "WIN" | "META" => MOD_WIN,
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
            (1..=24).contains(&n).then_some(u32::from(VK_F1) - 1 + n)
        } else {
            match part.as_str() {
                "BACKSPACE" => Some(u32::from(VK_BACK)),
                "TAB" => Some(u32::from(VK_TAB)),
                "RETURN" => Some(u32::from(VK_RETURN)),
                "ESCAPE" => Some(u32::from(VK_ESCAPE)),
                "SPACE" => Some(u32::from(VK_SPACE)),
                "PAGEUP" => Some(u32::from(VK_PRIOR)),
                "PAGEDOWN" => Some(u32::from(VK_NEXT)),
                "END" => Some(u32::from(VK_END)),
                "HOME" => Some(u32::from(VK_HOME)),
                "LEFT" => Some(u32::from(VK_LEFT)),
                "UP" => Some(u32::from(VK_UP)),
                "RIGHT" => Some(u32::from(VK_RIGHT)),
                "DOWN" => Some(u32::from(VK_DOWN)),
                "INSERT" => Some(u32::from(VK_INSERT)),
                "DELETE" => Some(u32::from(VK_DELETE)),
                _ => None,
            }
        };
        if key.is_none() {
            return Err("快捷键主键不支持".into());
        }
    }
    if flags == MOD_NOREPEAT || key.is_none() {
        return Err("快捷键需包含修饰键与有效主键".into());
    }
    Ok((flags, key.unwrap_or_default()))
}
