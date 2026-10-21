use crate::config::Config;
use crate::fsutil;
use unicode_normalization::UnicodeNormalization;

// ---------------------------------------------------------------------------
// 附录 B：副本标记与名称规范化（C-02 / C-08 / C-16）
// ---------------------------------------------------------------------------

/// 一段末尾副本标记：`(N)`（ASCII 圆括号 + 一至多个十进制数字）或
/// `- Copy`（忽略大小写，连字符与 Copy 间允许 ASCII 空格）/ 中文 `副本`。
/// 标记前允许零个或多个 ASCII 空格；主体中间的同形字串不算标记。
#[derive(Debug, Clone, PartialEq, Eq)]
enum CopyMarker {
    Numbered(Vec<u8>),
    Label,
}
/// 从主体末尾自右向左收集整段标记；返回剥除后的基础主体与按原顺序（从左往右）的标记。
/// 每段标记在剥除当下即被记录，不重复计数。
fn split_copy_markers(stem: &str) -> (String, Vec<CopyMarker>) {
    let mut rest = stem.to_string();
    let mut markers = Vec::new();
    while let Some((next, marker)) = strip_one_marker(&rest) {
        rest = next;
        // 收集顺序为从右往左；最后统一反转回原顺序。
        markers.push(marker);
    }
    markers.reverse();
    (rest, markers)
}
/// 剥除主体末尾的一段标记（连同其前导 ASCII 空格），同时返回该段标记本身。
fn strip_one_marker(stem: &str) -> Option<(String, CopyMarker)> {
    // (N)：以 ')' 结尾，回找 '('，括号内全部为 ASCII 十进制数字且非空。
    if stem.ends_with(')') {
        if let Some(open) = stem.rfind('(') {
            let digits = &stem[open + 1..stem.len() - 1];
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                let base = stem[..open].trim_end_matches(' ').to_string();
                return Some((base, CopyMarker::Numbered(digits.bytes().collect())));
            }
        }
    }
    // 中文「副本」。
    if let Some(base) = stem.strip_suffix("副本") {
        return Some((base.trim_end_matches(' ').to_string(), CopyMarker::Label));
    }
    // `- Copy`：ASCII 连字符 + 可选空格 + Copy（忽略大小写）。比较只对 ASCII 字母
    // 有意义；`to_lowercase` 会改变部分字符的字节长度（如 U+0130 `İ`），小写串的
    // 字节偏移不能回切原串——改为对尾部片段做 ASCII 忽略大小写比对。
    if let Some(before) = ascii_case_insensitive_suffix(stem, "copy") {
        let trimmed = before.trim_end_matches(' ');
        if let Some(hyphen) = trimmed.strip_suffix('-') {
            let base = hyphen.trim_end_matches(' ').to_string();
            return Some((base, CopyMarker::Label));
        }
    }
    None
}
/// 检查 `text` 是否以 `ascii_suffix` 结尾（仅 ASCII 字母忽略大小写，其余字节精确
/// 相等），是则返回剥除该后缀后的前缀切片。后缀全为 ASCII 时切点必在字符边界。
fn ascii_case_insensitive_suffix<'a>(text: &'a str, ascii_suffix: &str) -> Option<&'a str> {
    let suffix = ascii_suffix.as_bytes();
    let split = text.len().checked_sub(suffix.len())?;
    if text.as_bytes()[split..]
        .iter()
        .zip(suffix)
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
    {
        Some(&text[..split])
    } else {
        None
    }
}
/// 十进制数字串转输出序号：去前导零、全零保留 `0`（附录 B）。
fn marker_digits_to_text(digits: &[u8]) -> String {
    let text = String::from_utf8_lossy(digits);
    let trimmed = text.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".into()
    } else {
        trimmed.to_string()
    }
}
/// C-08 副本后缀清理的输出名转换：每个 `(N)` 转为 `_N`，`- Copy` / `副本` 移除；
/// 基础主体为空时用 `_`。幂等：`_N` 不是标记，转换后的名称再次整理不再变化。
/// 扩展名（含复合扩展名与编号分卷后缀）整体保留，标记只作用于主体。
pub fn clean_copy_output(name: &str) -> String {
    let (stem, extension) = fsutil::split_compound_name(name);
    format!("{}{extension}", clean_copy_stem(stem))
}

fn clean_copy_stem(stem: &str) -> String {
    let (base, markers) = split_copy_markers(stem);
    let mut out = if base.is_empty() && !markers.is_empty() {
        "_".to_string()
    } else {
        base
    };
    for marker in markers {
        if let CopyMarker::Numbered(digits) = marker {
            out.push('_');
            out.push_str(&marker_digits_to_text(&digits));
        }
    }
    out
}
/// C-02 副本名键：剥除全部末尾标记的主体 + 未改变的扩展名（附录 B：键只按
/// 副本标记剥除后连同扩展名做序数忽略大小写比较，折叠由 normal_key 完成）。
pub fn copy_key(name: &str) -> String {
    let (stem, extension) = fsutil::split_compound_name(name);
    let (base, _) = split_copy_markers(stem);
    format!("{base}{extension}")
}
/// C-02 名称关系判定的 `normal` 列取值：副本名的 Windows 序数比较键。
pub fn normal_key(name: &str) -> String {
    fsutil::fold_rel(&copy_key(name))
}
/// C-08 NFC 与连续空白规范化（单一开关）：对主体做 NFC，把 Unicode White_Space
/// 属性字符的连续串压成一个 ASCII 空格，去主体首尾空白；不改扩展名。
pub fn normalize_stem(stem: &str) -> String {
    let nfc: String = stem.nfc().collect();
    let mut out = String::with_capacity(nfc.len());
    let mut in_space = false;
    for ch in nfc.chars() {
        if ch.is_whitespace() {
            in_space = true;
        } else {
            // 首部空白直接丢弃（out 仍空时不补空格），尾部空白由循环自然截断。
            if in_space {
                if !out.is_empty() {
                    out.push(' ');
                }
                in_space = false;
            }
            out.push(ch);
        }
    }
    out
}
/// 对已拆分的主体应用开启的规则，不重新解释主体中新暴露的点。
fn normalize_output_stem(stem: &str, cfg: &Config) -> String {
    let stem = if cfg.normalize_names {
        normalize_stem(stem)
    } else {
        stem.to_string()
    };
    if cfg.clean_copy_name {
        clean_copy_stem(&stem)
    } else {
        stem
    }
}

/// 实际派生主体的输出规则与合法性处理共同收敛；扩展名由调用者原样保留。
pub fn stabilize_derived_stem(stem: &str, cfg: &Config) -> String {
    let mut current = stem.to_string();
    loop {
        let next = legalize_derived(&normalize_output_stem(&current, cfg));
        if next == current {
            return next;
        }
        current = next;
    }
}

/// C-16 名称规范化流水线，保留首次拆分的主体/扩展名边界。
/// 没有实际规则变化的合法完整名保持原样；合法化暴露的开启规则仍须一次完成。
pub fn derive_stem_ext(name: &str, cfg: &Config) -> (String, String) {
    let (original, extension) = fsutil::split_compound_name(name);
    let normalized = normalize_output_stem(original, cfg);
    let exposes_rule = (cfg.normalize_names || cfg.clean_copy_name) && {
        let legalized = legalize_derived(&normalized);
        normalize_output_stem(&legalized, cfg) != legalized
    };
    let stem =
        if normalized != original || exposes_rule || fsutil::validate_component(name).is_err() {
            stabilize_derived_stem(&normalized, cfg)
        } else {
            normalized
        };
    (stem, extension.to_string())
}
/// 附录 B 合法性处理：只作用于实际派生的新名称——控制字符与 Windows 禁用字符替换为
/// `_`，末尾 ASCII 点/空格去掉，空主体 / `.` / `..` 用 `_`；设备保留名前加 `_`。
pub fn legalize_derived(stem: &str) -> String {
    let mut out: String = stem
        .chars()
        .map(|c| {
            if c <= '\u{1f}' || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    if out.is_empty() || out == "." || out == ".." {
        out = "_".into();
    }
    let head = out.split('.').next().unwrap_or("").to_uppercase();
    if ["CON", "PRN", "AUX", "NUL", "CLOCK$"].contains(&head.as_str())
        || ((head.starts_with("COM") || head.starts_with("LPT"))
            && head.chars().count() == 4
            && head
                .chars()
                .last()
                .is_some_and(|c| "123456789¹²³".contains(c)))
    {
        out = format!("_{out}");
    }
    out
}
