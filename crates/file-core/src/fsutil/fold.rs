use std::fs;

/// Windows 序数忽略大小写比较键，保留 UTF-16 单元顺序；非 Windows 平台原样返回。
/// 相对路径比较前一律先经本函数折叠，「比较前先折叠」这一不变量只有这一处实现。
pub fn fold_rel(name: &str) -> String {
    #[cfg(windows)]
    {
        let mut key = String::new();
        for unit in name.encode_utf16().map(ordinal_fold_unit) {
            // 字符串键需保持 UTF-16 单元的顺序，包括代理项；将代理项映射到其间隔
            // 区域，再把更大的 BMP 单元整体平移，使 String 排序等价于单元序数排序。
            let code_point = u32::from(unit);
            let code_point = if code_point >= 0xD800 {
                code_point + 0x800
            } else {
                code_point
            };
            if let Some(character) = char::from_u32(code_point) {
                key.push(character);
            }
        }
        key
    }
    #[cfg(not(windows))]
    {
        name.to_string()
    }
}

#[cfg(windows)]
fn ordinal_fold_unit(unit: u16) -> u16 {
    if unit < 0x80 {
        return if (u16::from(b'a')..=u16::from(b'z')).contains(&unit) {
            unit - 32
        } else {
            unit
        };
    }
    if (0xD800..=0xDFFF).contains(&unit) {
        return unit;
    }
    // CompareStringOrdinal 使用逐 UTF-16 单元的大写映射；这些希腊字母的简单
    // 单元映射与 Rust 全大写结果不同，按操作系统的序数比较行为折叠。
    match unit {
        0x1F80..=0x1F87 | 0x1F90..=0x1F97 | 0x1FA0..=0x1FA7 => return unit + 0x08,
        0x1FB3 => return 0x1FBC,
        0x1FC3 => return 0x1FCC,
        0x1FF3 => return 0x1FFC,
        _ => {}
    }
    let Some(mut uppercase) = char::from_u32(u32::from(unit)).map(char::to_uppercase) else {
        return unit;
    };
    let Some(mapped) = uppercase.next() else {
        return unit;
    };
    if uppercase.next().is_some() || mapped.len_utf16() != 1 {
        return unit;
    }
    u16::try_from(u32::from(mapped)).unwrap_or(unit)
}

pub fn is_link(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}
