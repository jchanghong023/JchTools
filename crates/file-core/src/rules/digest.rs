use crate::fsutil;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// C-19：命名摘要与 C-20：长度受限候选
// ---------------------------------------------------------------------------

/// C-19 命名摘要输入：分析开始时该项相对于所选根的原始完整路径（`/` 分隔，无前后
/// 斜杠，保留原始大小写与 Unicode 序列，不做 NFC 或副本后缀清理），无 BOM UTF-8
/// 编码计算 SHA-256，取十六进制大写前 `hex_units` 位（8 起，每次 +2，最多 64）。
pub fn path_digest(rel: &str, hex_units: usize) -> String {
    let digest = Sha256::digest(rel.as_bytes());
    let hex = hex::encode_upper(digest);
    hex[..hex_units.min(64)].to_string()
}
/// C-20 阈值常量：40 个 UTF-16 单元是冲突候选的退让阈值；255 是单段名称硬上限。
pub const CONFLICT_YIELD_UNITS: usize = 40;
pub const SEGMENT_LIMIT_UNITS: usize = 255;
/// C-19/C-20 摘要候选：优先「最近一级来源_主体_摘要.扩展名」；该形式（或 C-18 逐层
/// 候选）超过 40 单元时，按 C-20 去掉来源段，改用「主体前段(≤25，继续截短至放得下)
/// _摘要.扩展名」。扩展名与摘要位数永不截断；仍放不下时允许超过 40 但不得超过 255，
/// 连一个合法主体字符都容不下时返回 None（该项安全失败）。
/// force_truncate 仅表示已进入 C-20 强制降级；普通无来源项仍先尝试完整主体。
pub fn digest_candidate(
    nearest_level: Option<&str>,
    stem: &str,
    digest: &str,
    extension: &str,
    index: Option<u32>,
    force_truncate: bool,
) -> Option<String> {
    let separator = 1usize;
    let digest_units = digest.encode_utf16().count();
    let ext_units = extension.encode_utf16().count();
    let level_units = nearest_level.map_or(0, |level| level.encode_utf16().count() + separator);
    // C-20：稳定序号同样计入长度预算，先截短主体腾位，不截摘要/扩展名。
    let index_units = index.map_or(0, |n| n.to_string().encode_utf16().count() + separator);
    let fixed_no_source = separator + digest_units + ext_units + index_units;
    if fixed_no_source + 1 > SEGMENT_LIMIT_UNITS {
        // 分隔符之外连一个主体字符都放不下。
        return None;
    }
    let build = |stem_part: &str, with_level: bool, limit: usize| -> Option<String> {
        if stem_part.is_empty() {
            return None;
        }
        let total = if with_level {
            level_units
                + stem_part.encode_utf16().count()
                + separator
                + digest_units
                + ext_units
                + index_units
        } else {
            stem_part.encode_utf16().count() + fixed_no_source
        };
        if total > limit {
            return None;
        }
        let index_text = index.map_or(String::new(), |n| format!("_{n}"));
        Some(if with_level {
            let level = nearest_level.unwrap_or("");
            format!("{level}_{stem_part}_{digest}{index_text}{extension}")
        } else {
            format!("{stem_part}_{digest}{index_text}{extension}")
        })
    };
    // C-19：先比较完整候选与 40 单元阈值，不提前截短；超限候选不分配输出字符串。
    if !force_truncate {
        if let Some(candidate) = build(stem, nearest_level.is_some(), CONFLICT_YIELD_UNITS) {
            return Some(candidate);
        }
    }
    // 超过 40：去掉来源段，主体截短到放得下（不拆代理对）；预算连第一个字符的
    // UTF-16 单元都容不下时仍保留第一个完整字符——结果允许超过 40 但不超过 255，
    // 与此前「逐字符剥离到剩一个字符为止」的循环终态逐一相同（含首字符为代理对、
    // 占 2 单元的情形）。
    let capped = truncate_utf16_units(stem, 25);
    let budget = CONFLICT_YIELD_UNITS.saturating_sub(fixed_no_source);
    let floor = capped.chars().next().map_or(0, char::len_utf16);
    let stem_part = truncate_utf16_units(&capped, budget.max(floor));
    build(&stem_part, false, SEGMENT_LIMIT_UNITS)
}
/// 按 UTF-16 单元上限截断字符串：逐字符累计 len_utf16，不切开代理对。
pub fn truncate_utf16_units(text: &str, max_units: usize) -> String {
    let mut out = String::new();
    let mut units = 0usize;
    for ch in text.chars() {
        let need = ch.len_utf16();
        if units + need > max_units {
            break;
        }
        units += need;
        out.push(ch);
    }
    out
}
/// 生成 `stem (N)ext」形式的候选名（供目录整理之外的兜底路径使用）；截断口径与
/// fsutil::suffixed_candidate 一致。
pub fn suffixed_candidate(stem: &str, ext: &str, index: u64) -> String {
    fsutil::suffixed_candidate(stem, ext, index)
}
