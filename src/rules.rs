use crate::{
    config::{Config, DeleteChoice, KeepPolicy},
    fsutil,
    model::FileRecord,
};
use anyhow::Result;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};
use std::{cmp::Ordering, path::Path, sync::OnceLock};
use unicode_normalization::UnicodeNormalization;

pub fn build_exclusions(text: &str) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for part in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        builder.add(
            GlobBuilder::new(part)
                .case_insensitive(true)
                .literal_separator(false)
                .build()?,
        );
    }
    Ok(builder.build()?)
}
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
    // `- Copy`：ASCII 连字符 + 可选空格 + Copy（忽略大小写）。
    let lower = stem.to_lowercase();
    if let Some(pos) = lower.rfind("copy") {
        if lower[pos + 4..].is_empty() {
            let before = &stem[..pos];
            let trimmed = before.trim_end_matches(' ');
            if let Some(hyphen) = trimmed.strip_suffix('-') {
                let base = hyphen.trim_end_matches(' ').to_string();
                return Some((base, CopyMarker::Label));
            }
        }
    }
    None
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
    format!("{out}{extension}")
}
/// C-02 副本名键：剥除全部末尾标记的主体 + 未改变的扩展名（附录 B：键只按
/// 副本标记剥除后连同扩展名做忽略大小写比较；小写折叠由 files.name 列的存储口径完成）。
pub fn copy_key(name: &str) -> String {
    let (stem, extension) = fsutil::split_compound_name(name);
    let (base, _) = split_copy_markers(stem);
    format!("{base}{extension}")
}
/// C-02 名称关系判定的 `normal` 列取值：副本名键的小写折叠。
pub fn normal_key(name: &str) -> String {
    copy_key(name).to_lowercase()
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
/// C-16 名称规范化流水线（顺序按附录 B：先 NFC/空白，再副本后缀清理），
/// 返回主体与扩展名（扩展名含前导点，复合扩展名整体保留）。
pub fn derive_stem_ext(name: &str, cfg: &Config) -> (String, String) {
    let (stem, extension) = fsutil::split_compound_name(name);
    let mut stem = if cfg.normalize_names {
        normalize_stem(stem)
    } else {
        stem.to_string()
    };
    if cfg.clean_copy_name {
        stem = {
            let converted = clean_copy_output(&format!("{stem}{extension}"));
            let (new_stem, _) = fsutil::split_compound_name(&converted);
            new_stem.to_string()
        };
    }
    (stem, extension.to_string())
}
/// 附录 B 合法性处理：只作用于实际派生的新名称——控制字符与 Windows 禁用字符替换为
/// `_`，末尾 ASCII 点/空格去掉，空主体 / `.` / `..` 用 `_`；设备保留名前加 `_`。
pub fn legalize_derived(stem: &str) -> String {
    let mut out: String = stem
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
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
    if ["CON", "PRN", "AUX", "NUL"].contains(&head.as_str())
        || ((head.starts_with("COM") || head.starts_with("LPT"))
            && head.len() == 4
            && head.ends_with(|c: char| ('1'..='9').contains(&c)))
    {
        out = format!("_{out}");
    }
    out
}
/// 纯参考实现：实际去重匹配在 planner::deduplicate 的 SQL 中。仅供测试对照。
#[cfg(test)]
pub fn duplicate_allowed(a: &FileRecord, b: &FileRecord, cfg: &Config) -> bool {
    if a.name == b.name {
        cfg.dedup_same_name
    } else if a.normalized == b.normalized {
        cfg.dedup_copy_names
    } else {
        cfg.dedup_other_names
    }
}
/// Ordering::Less means a is the preferred keeper. Ties never rely on traversal order.
/// C-03：最短名称按完整文件名的 UTF-16 单元数升序；平局按相对路径长度（UTF-16 单元）
/// 升序、再按路径稳定升序决胜（附录 B）。
pub fn compare(a: &FileRecord, b: &FileRecord, policy: KeepPolicy) -> Ordering {
    let primary = match policy {
        KeepPolicy::Newest => b.snapshot.modified_ns.cmp(&a.snapshot.modified_ns),
        KeepPolicy::Oldest => a.snapshot.modified_ns.cmp(&b.snapshot.modified_ns),
        KeepPolicy::ShortestName => a
            .name
            .encode_utf16()
            .count()
            .cmp(&b.name.encode_utf16().count()),
    };
    primary
        .then_with(|| {
            a.rel
                .encode_utf16()
                .count()
                .cmp(&b.rel.encode_utf16().count())
        })
        .then_with(|| a.rel.cmp(&b.rel))
}
/// SQL 排序片段（供 planner 拼接进 ORDER BY）。name16/rel16 是扫描期预计算的
/// UTF-16 单元数列；决胜列 rel 为 SQLite BINARY 文本序（与附录 B 的「UTF-16 单元逐
/// 单元升序」仅在星形字符与 U+E000..U+FFFF 的相对顺序上有差异，且只在忽略大小写
/// 比较仍相同的路径之间才用到，保持确定性即可）。
pub fn ordering_sql(policy: KeepPolicy) -> &'static str {
    match policy {
        KeepPolicy::Newest => "mtime DESC,rel16,rel",
        KeepPolicy::Oldest => "mtime ASC,rel16,rel",
        KeepPolicy::ShortestName => "name16,rel16,rel",
    }
}
/// C-04：可靠文件标识是否证明两个目录项指向同一物理文件。
/// 条件：标识相同、两侧链接数 >= 2（两个目录项必然抬高链接数）、且标识未退化。
/// Windows 的「卷:索引高:索引低」在部分文件系统上索引恒为 0，无法区分不同文件
/// （与 C-13 的 hash_cache 同口径）；退化时不得据此跳过重复处理。
/// Unix 的「设备:inode」两段结构本身就唯一标识文件，不套用该三段口径。
pub fn identity_proves_same_file(a: &FileRecord, b: &FileRecord) -> bool {
    a.snapshot.identity == b.snapshot.identity
        && a.snapshot.links >= 2
        && b.snapshot.links >= 2
        && !(cfg!(windows) && crate::hash_cache::identity_is_degenerate(&a.snapshot.identity))
}
/// C-08：按内容签名修正错误扩展名的保守判定。true = 旧扩展名确实错误、可以改名。
/// 签名不足以确定实际类型（只识别到外层容器/存储）、旧扩展名属于同一容器家族的更具体
/// 格式、或旧扩展名本就是同义写法时一律不改名；合法的同义扩展名不强制统一。
pub fn extension_needs_fix(old: &str, detected: &str) -> bool {
    !equivalent_extension(old, detected)
        && !specialized_extension(old)
        && !outer_container_extension(detected)
}
/// 同义扩展名（同一格式的常见写法），不算错误扩展名。
fn equivalent_extension(old: &str, detected: &str) -> bool {
    old == detected
        || (matches!(old, "jpeg" | "jpe" | "jfif") && detected == "jpg")
        || (old == "tiff" && detected == "tif")
        || (old == "htm" && detected == "html")
        || (old == "mid" && detected == "midi")
}
/// 识别结果只说明外层容器/存储，不含更具体的格式信息：压缩包与 OLE 存储
/// （doc/xls/ppt/msi/vsd 等都只会被识别成同一个 OLE 存储类型）都属此类。
/// 通用压缩后缀与 `archive_name` 保持同口径，另加 OLE 存储（infer 报 msi）。
fn outer_container_extension(ext: &str) -> bool {
    matches!(
        ext,
        "zip"
            | "gz"
            | "bz2"
            | "xz"
            | "zst"
            | "tar"
            | "7z"
            | "rar"
            | "cab"
            | "iso"
            | "wim"
            | "lz"
            | "lzma"
            | "cpio"
            | "lzh"
            | "msi"
    )
}
/// 旧扩展名是该容器家族里的更具体格式，识别结果粒度更粗（infer 对 OOXML 变体
/// 只报 docx/xlsx/pptx 基础类型）：按更粗的结果改名会把正确的专用扩展名改错。
fn specialized_extension(ext: &str) -> bool {
    matches!(
        ext,
        "docm"
            | "dotx"
            | "dotm"
            | "xlsm"
            | "xltx"
            | "xltm"
            | "xlsb"
            | "potx"
            | "potm"
            | "ppsx"
            | "ppsm"
            | "epub"
            | "odt"
            | "ods"
            | "odp"
            | "jar"
            | "apk"
    )
}
// ---------------------------------------------------------------------------
// 附录 A：普通文件大类映射（C-05 / C-06）
// ---------------------------------------------------------------------------

/// 附录 A 唯一归类映射：按最终完整文件名（小写）匹配，先匹配最长复合后缀，再匹配
/// 最后一段扩展名；X-10 命名族分卷（`.7z.001`、`.partN.rar`、`.rNN`、`.zNN`）按
/// 附录 A 说明归「压缩包」；未匹配与无扩展名归「其他」。
// 输入已预先小写，ends_with 的字面量比较即为忽略大小写口径。
#[allow(clippy::case_sensitive_file_extension_comparisons)]
pub fn category_for(name_lower: &str) -> &'static str {
    const COMPOUND: [&str; 6] = [
        ".tar.gz",
        ".tar.bz2",
        ".tar.xz",
        ".tar.zst",
        ".tar.lzma",
        ".tar.z",
    ];
    if COMPOUND.iter().any(|suffix| name_lower.ends_with(suffix)) {
        return "压缩包";
    }
    // X-10 数字尾卷：白名单扩展名 + 恰好三位 .NNN。
    if let Some(dot) = name_lower.rfind('.') {
        if let Some(prev) = name_lower[..dot].rfind('.') {
            let tail = &name_lower[dot + 1..];
            let inner = &name_lower[prev + 1..dot];
            let numbered = tail.len() == 3 && tail.bytes().all(|b| b.is_ascii_digit());
            if numbered
                && [
                    "7z", "zip", "rar", "tar", "gz", "bz2", "xz", "zst", "lzma", "z", "tgz",
                    "tbz2", "txz", "tzst",
                ]
                .contains(&inner)
            {
                return "压缩包";
            }
        }
    }
    if name_lower.ends_with(".rar") {
        return "压缩包";
    }
    // part rar 分卷：主干.partN.rar。
    if let Some(pos) = name_lower.rfind(".part") {
        let after = &name_lower[pos + 5..];
        if let Some(rar) = after.rfind(".rar") {
            if after[..rar].bytes().all(|b| b.is_ascii_digit()) && !after[..rar].is_empty() {
                return "压缩包";
            }
        }
    }
    // 老式尾卷族：.r00～.r99 / .z01～.z99（附录 A：整理中的 X-10 命名族分卷归「压缩包」；
    // zip 族起始编号是 01，z00 不属命名族；data.001 这类无格式孤立编号仍归「其他」）。
    if let Some((_, tail)) = name_lower.rsplit_once('.') {
        let digits = tail.as_bytes();
        if tail.len() == 3
            && (tail.starts_with('r') || tail.starts_with('z'))
            && digits[1..].iter().all(u8::is_ascii_digit)
            && (tail.starts_with('r') || &tail[1..] != "00")
        {
            return "压缩包";
        }
    }
    let extension = name_lower.rsplit_once('.').map_or("", |(_, tail)| tail);
    match extension {
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg" | "ts"
        | "mts" | "m2ts" | "3gp" | "vob" | "rm" | "rmvb" => "视频",
        "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "opus" | "wma" | "aiff" | "aif"
        | "ape" | "mid" | "midi" => "音频",
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "svg" | "ico"
        | "heic" | "heif" | "avif" | "psd" | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" => {
            "图片"
        }
        "pdf" | "txt" | "md" | "rtf" | "doc" | "docx" | "docm" | "xls" | "xlsx" | "xlsm"
        | "csv" | "ppt" | "pptx" | "pptm" | "odt" | "ods" | "odp" | "epub" | "mobi" | "azw"
        | "azw3" | "chm" | "html" | "htm" => "文档",
        "zip" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "lzma" | "z" | "tgz" | "tbz2"
        | "txz" | "tzst" | "lz4" | "br" | "lz" => "压缩包",
        "exe" | "com" | "msi" | "msix" | "appx" | "msixbundle" | "appxbundle" | "bat" | "cmd"
        | "ps1" | "apk" => "程序",
        _ => "其他",
    }
}
/// 兼容旧调用点（测试）：按单个扩展名（不带点、小写）取大类。
pub fn category(extension: &str) -> &'static str {
    category_for(&format!(".{extension}"))
}
// ---------------------------------------------------------------------------
// 清理项（C-08）
// ---------------------------------------------------------------------------

/// 清理项类别（C-08）：三类清理各自独立启停，并可独立覆盖删除方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupKind {
    Junk,
    Temp,
    Zero,
}
/// C-08：清理命中判定与类别。返回 None 表示不清理。
/// planner 按类别解析删除方式（[`cleanup_delete`]），不得比较原因文案。
pub fn cleanup_reason(rel: &str, size: u64, cfg: &Config) -> Option<(CleanupKind, &'static str)> {
    let file = Path::new(rel).file_name()?.to_str()?.to_lowercase();
    if cfg.clean_junk
        && (["thumbs.db", ".ds_store", "desktop.ini"].contains(&file.as_str())
            || file.starts_with("._")
            || rel.split('/').any(|s| s.to_lowercase() == "__macosx"))
    {
        return Some((CleanupKind::Junk, "用户开启的垃圾文件规则"));
    }
    if cfg.clean_temp
        && (has_ext(&file, "tmp")
            || has_ext(&file, "temp")
            || has_ext(&file, "bak")
            || file.starts_with("~$"))
    {
        return Some((CleanupKind::Temp, "用户开启的临时/备份文件规则"));
    }
    if cfg.clean_zero && size == 0 {
        return Some((CleanupKind::Zero, "用户开启的零字节文件规则"));
    }
    None
}
/// C-08：清理项的删除方式覆盖（默认跟随全局文件删除方式）。
pub fn cleanup_delete(cfg: &Config, kind: CleanupKind) -> DeleteChoice {
    match kind {
        CleanupKind::Junk => cfg.junk_delete,
        CleanupKind::Temp => cfg.temp_delete,
        CleanupKind::Zero => cfg.zero_delete,
    }
}
/// 扩展名等值判断：按最后一个点切分比较尾段，等价于 `ends_with(".ext")`
/// （输入已预先 lowercase，无大小写歧义）。rsplit_once 按字符边界切分，不会 panic。
fn has_ext(name: &str, ext: &str) -> bool {
    matches!(name.rsplit_once('.'), Some((_, tail)) if tail == ext)
}
// ---------------------------------------------------------------------------
// X-01 / X-10：解压白名单与分卷
// ---------------------------------------------------------------------------

/// X-01：自动解压白名单。判断口径不是「这个文件是不是压缩格式」，而是「它是不是
/// 用户意义上的归档文件」——`.cab` 是安装介质的一部分（Office/驱动/更新包），
/// `.iso`/`.wim`/`.esd` 是系统镜像，`.msi`/`.msix`/`.appx` 是安装包，`.jar`/`.apk`/
/// `.whl`/`.nupkg` 是程序包，`.docx`/`.xlsx`/`.epub`/`.odt` 是文档容器，`.exe`/`.com`
/// 可能是自解压包：它们即使能被 7-Zip 打开，也一律不自动解压（只看扩展名，不猜内容）。
/// 分卷只认可独立解开的第一卷：`.7z.001` / `.zip.001` 认 001、`.partN.rar` 认 part1；
/// 其余卷（`.z01`/`.r00`/`.002`）由删除与隔离的卷集合逻辑成组处理，不单独入队。
pub fn archive_name(name: &str) -> bool {
    let name = name.to_lowercase();
    if has_ext(&name, "rar") {
        // 只有 `.partN.rar` 的 part1 算分卷主体；`report.partial.rar` 这类普通包不受影响。
        static PART: OnceLock<regex::Regex> = OnceLock::new();
        let re = PART.get_or_init(|| match regex::Regex::new(r"\.part(\d+)\.rar$") {
            Ok(re) => re,
            // 常量正则语法错误只可能是开发期笔误，按不可达处理
            Err(_) => unreachable!("constant regex"),
        });
        if let Some(caps) = re.captures(&name) {
            return caps[1].parse::<u64>().ok() == Some(1);
        }
        return true;
    }
    // 白名单之外的一切格式（含引擎能打开的 cab/iso/wim/lzh/cpio 等）都不自动解压：
    // 维护「禁止列表」必然漏掉新出现的容器格式，白名单是唯一可靠的边界。
    // 压缩流（.gz/.bz2/.xz/.zst/.lzma/.z）允许前面再带一层 `.tar`，整体作为一个包
    // （`.tar.gz` 等由流后缀本身覆盖，`.tgz`/`.tbz2`/`.txz`/`.tzst` 是别名）。
    // `.lz4` / `.br` / `.lz`（真 lzip）不在白名单内：捆绑引擎没有对应解码器，
    // 放进来只会把这类文件一律推进「解压失败」，不如完全不碰（X-09）。
    // 卷（`.002`、`.part2.rar`、`.r00`、`.z01`）不是独立解压对象，不在这里放行；
    // 白名单外格式的卷（`.iso.001`）与配不上主包的孤立编号文件（`data.001`）同样不匹配。
    [
        ".zip", ".7z", ".tar", ".gz", ".bz2", ".xz", ".zst", ".lzma", ".z", ".tgz", ".tbz2",
        ".txz", ".tzst", ".7z.001", ".zip.001",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
}
pub fn multipart_name(name: &str) -> bool {
    let n = name.to_lowercase();
    // 与 archive_name 对齐：只有 .partN.rar 才算 RAR 分卷；contains(".part") 会把
    // report.partial.rar 这类普通包误判为分卷。
    static PART: OnceLock<regex::Regex> = OnceLock::new();
    let re = PART.get_or_init(|| match regex::Regex::new(r"\.part(\d+)\.rar$") {
        Ok(re) => re,
        // 常量正则语法错误只可能是开发期笔误，按不可达处理
        Err(_) => unreachable!("constant regex"),
    });
    n.ends_with(".001") || re.is_match(&n)
}
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
pub fn digest_candidate(
    nearest_level: Option<&str>,
    stem: &str,
    digest: &str,
    extension: &str,
    index: Option<u32>,
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
    let build = |stem_part: &str, with_level: bool| -> Option<String> {
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
        if total > SEGMENT_LIMIT_UNITS {
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
    // 先试带来源的 C-19 形式（主体截到 25）；放得下且 ≤40 即返回。
    if nearest_level.is_some() {
        let capped = truncate_utf16_units(stem, 25);
        if let Some(candidate) = build(&capped, true) {
            if candidate.encode_utf16().count() <= CONFLICT_YIELD_UNITS {
                return Some(candidate);
            }
        }
    }
    // 超过 40：去掉来源段，主体从 25 起逐字符截短到 ≤40（不拆代理对）。
    let mut stem_part = truncate_utf16_units(stem, 25);
    loop {
        let total = stem_part.encode_utf16().count() + fixed_no_source;
        if total <= CONFLICT_YIELD_UNITS || stem_part.is_empty() {
            break;
        }
        let next = truncate_utf16_units(&stem_part, stem_part.encode_utf16().count() - 1);
        if next.is_empty() {
            break;
        }
        stem_part = next;
    }
    build(&stem_part, false)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Snapshot;

    fn record(id: i64, name: &str, normalized: &str) -> FileRecord {
        FileRecord {
            id,
            rel: format!("dir/{name}"),
            name: name.into(),
            normalized: normalized.into(),
            snapshot: Snapshot {
                size: 1,
                modified_ns: 10,
                created_ns: None,
                identity: id.to_string(),
                links: 1,
            },
            hash: Some("h".into()),
            cleanable: false,
        }
    }
    /// 复刻 planner::deduplicate keepers 查询中的 SQL 匹配条件（与 rust 参考实现逐分支对照）：
    /// `(name=?2 AND ?4) OR (name<>?2 AND normal=?3 AND ?5) OR (name<>?2 AND normal<>?3 AND ?6)`
    fn sql_match(a: &FileRecord, b: &FileRecord, cfg: &Config) -> bool {
        match (a.name == b.name, a.normalized == b.normalized) {
            (true, _) => cfg.dedup_same_name,
            (false, true) => cfg.dedup_copy_names,
            (false, false) => cfg.dedup_other_names,
        }
    }

    // 覆盖 C-04（同一物理文件判定：标识必须可靠，且链接数证明存在第二个目录项）
    #[test]
    fn identity_proves_same_file_requires_reliable_identity() {
        let pair = |identity: &str, links: u64| {
            let mut a = record(1, "a.txt", "a.txt");
            a.snapshot.identity = identity.into();
            a.snapshot.links = links;
            let mut b = record(2, "b.txt", "b.txt");
            b.snapshot.identity = identity.into();
            b.snapshot.links = links;
            (a, b)
        };
        // 真实硬链接：两侧标识相同且编号可靠、链接数 >= 2 → 证明同一物理文件。
        let reliable = if cfg!(windows) { "12:34:56" } else { "2049:99" };
        let (a, b) = pair(reliable, 2);
        assert!(
            identity_proves_same_file(&a, &b),
            "可靠标识 + 两个目录项必须判定为同一物理文件"
        );
        let (a, b) = pair(reliable, 3);
        assert!(identity_proves_same_file(&a, &b));
        // 标识相同但链接数只有 1：两个目录项不可能指向同一物理文件，不得据此跳过去重。
        let (a, b) = pair(reliable, 1);
        assert!(
            !identity_proves_same_file(&a, &b),
            "链接数不足以证明同一物理文件"
        );
        // 标识不同：不是同一物理文件。
        let (mut a, b) = pair(reliable, 2);
        a.snapshot.identity = "99:88:77".into();
        assert!(!identity_proves_same_file(&a, &b));
        // Windows 上部分文件系统索引恒为 0（hash_cache::identity_is_degenerate 同口径）：
        // 该标识无法区分不同文件，退化时不得作为跳过依据（C-04）。
        #[cfg(windows)]
        {
            let (a, b) = pair("12:0:0", 2);
            assert!(
                !identity_proves_same_file(&a, &b),
                "退化标识不得证明同一物理文件"
            );
        }
    }

    // 覆盖 C-08（三类清理的识别与类别归属：planner 按类别解析删除方式，不比较中文原因串）
    #[test]
    fn cleanup_reason_reports_category() {
        let cfg = Config {
            clean_temp: true,
            clean_zero: true,
            ..Config::default()
        };
        assert_eq!(
            cleanup_reason("a/Thumbs.db", 1, &cfg).map(|(kind, _)| kind),
            Some(CleanupKind::Junk)
        );
        assert_eq!(
            cleanup_reason("a/__MACOSX/x", 1, &cfg).map(|(kind, _)| kind),
            Some(CleanupKind::Junk)
        );
        assert_eq!(
            cleanup_reason("a/x.tmp", 1, &cfg).map(|(kind, _)| kind),
            Some(CleanupKind::Temp)
        );
        assert_eq!(
            cleanup_reason("a/~$x.docx", 1, &cfg).map(|(kind, _)| kind),
            Some(CleanupKind::Temp)
        );
        assert_eq!(
            cleanup_reason("a/zero.dat", 0, &cfg).map(|(kind, _)| kind),
            Some(CleanupKind::Zero)
        );
        assert_eq!(
            cleanup_reason("a/keep.txt", 5, &cfg),
            None,
            "普通文件不清理"
        );
    }

    // 覆盖 C-08（扩展名修正的保守口径：别名与容器识别都不得触发改名）
    #[test]
    fn extension_fix_is_conservative() {
        for (old, detected) in [
            ("jpg", "jpg"),
            ("jpeg", "jpg"),
            ("jpe", "jpg"),
            ("jfif", "jpg"),
            ("tif", "tif"),
            ("tiff", "tif"),
            ("htm", "html"),
            ("html", "html"),
            ("mid", "midi"),
            ("midi", "midi"),
        ] {
            assert!(
                !extension_needs_fix(old, detected),
                "{old} -> {detected} 是同义扩展名，不得强制统一"
            );
        }
        // 复合/包格式：更粗的识别结果不得把专用扩展名改粗（C-08 例：Office ZIP 容器）。
        for old in [
            "docm", "dotx", "dotm", "xlsm", "xltm", "xlsb", "potx", "ppsx", "ppsm", "epub", "odt",
            "ods", "odp", "jar", "apk",
        ] {
            for detected in ["zip", "docx", "xlsx", "pptx"] {
                assert!(
                    !extension_needs_fix(old, detected),
                    "{old} 的相对具体扩展名不得按 {detected} 改粗"
                );
            }
        }
        // 包装格式（压缩流内层未知，如 svgz/tgz）识别到容器后缀时同样不是错误扩展名。
        for (old, detected) in [("svgz", "gz"), ("tgz", "gz"), ("tbz2", "bz2")] {
            assert!(
                !extension_needs_fix(old, detected),
                "{old} 不得按外层容器 {detected} 改名"
            );
        }
        // 容器识别只说明外层容器（OLE 存储同样只会被识别成 msi）：不得据此改成压缩包/安装包后缀。
        for detected in [
            "zip", "gz", "bz2", "xz", "zst", "tar", "7z", "rar", "cab", "iso", "wim", "lz", "lzma",
            "cpio", "lzh", "msi",
        ] {
            assert!(
                !extension_needs_fix("bin", detected),
                "容器识别 {detected} 不得直接作为改名依据"
            );
        }
        // 明确的错误扩展名仍必须修正（默认关闭，开启后列入计划）。
        for (old, detected) in [
            ("txt", "png"),
            ("bin", "pdf"),
            ("jpg", "png"),
            ("mp4", "mp3"),
        ] {
            assert!(
                extension_needs_fix(old, detected),
                "{old} -> {detected} 是错误扩展名，应修正"
            );
        }
    }

    // 覆盖 C-08 / 附录 B（副本标记输出转换：`(N)` 转 `_N`，其余标记移除；复合扩展名与
    // 编号分卷整体保留；`_1` 不是标记，转换幂等）
    #[test]
    fn copy_marker_conversion_matches_appendix_b() {
        for (input, expected) in [
            ("报告 (1) (02).pdf", "报告_1_2.pdf"),
            ("report - Copy (1).tar.gz", "report_1.tar.gz"),
            ("报告副本.pdf", "报告.pdf"),
            ("报告副本说明.pdf", "报告副本说明.pdf"),
            ("(1).pdf", "__1.pdf"),
            ("report(1).pdf", "report_1.pdf"),
            ("report (01).pdf", "report_1.pdf"),
            ("report (0).pdf", "report_0.pdf"),
            ("report - copy.pdf", "report.pdf"),
            ("report  副本.pdf", "report.pdf"),
            ("资料 (1).tar.gz", "资料_1.tar.gz"),
            ("包 (1).7z.001", "包_1.7z.001"),
            ("报告_1.pdf", "报告_1.pdf"),
            ("report.final (1).txt", "report.final_1.txt"),
        ] {
            assert_eq!(clean_copy_output(input), expected, "{input}");
        }
    }

    // 覆盖 C-02（副本名键：剥除全部标记的主体 + 原扩展名）
    #[test]
    fn copy_key_strips_all_markers() {
        for (input, expected) in [
            ("报告 (1) (02).pdf", "报告.pdf"),
            ("report - Copy (1).tar.gz", "report.tar.gz"),
            ("报告副本.pdf", "报告.pdf"),
            ("资料.tar.gz", "资料.tar.gz"),
            ("包.7z.001", "包.7z.001"),
        ] {
            assert_eq!(copy_key(input), expected, "{input}");
        }
    }

    // 覆盖 C-08（NFC 与连续空白规范化：单一开关、不改扩展名、压成 ASCII 空格）
    #[test]
    fn normalize_stem_collapses_whitespace_and_nfc() {
        let normalized = normalize_stem("  a　b  c　");
        assert_eq!(normalized, "a b c");
        // NFD 组合字符归并为 NFC 预组合形式。
        let nfd = "e\u{301}";
        assert_eq!(normalize_stem(nfd), "e\u{301}".nfc().to_string());
    }

    // 覆盖 C-02, R-02（三类名称关系独立启停，SQL 与参考实现一致）
    #[test]
    fn duplicate_allowed_matches_planner_sql() {
        // 覆盖三种名称关系（同名 / 副本名 / 不同名）与三种开关组合。
        let pairs = [
            (record(1, "a.txt", "a.txt"), record(2, "a.txt", "a.txt")), // 同名
            (record(1, "a.txt", "a.txt"), record(2, "a (1).txt", "a.txt")), // 副本名
            (record(1, "a.txt", "a.txt"), record(2, "b.txt", "b.txt")), // 不同名
        ];
        for dedup_same_name in [false, true] {
            for dedup_copy_names in [false, true] {
                for dedup_other_names in [false, true] {
                    let cfg = Config {
                        dedup_same_name,
                        dedup_copy_names,
                        dedup_other_names,
                        ..Config::default()
                    };
                    for (a, b) in &pairs {
                        assert_eq!(
                            duplicate_allowed(a, b, &cfg), sql_match(a, b, &cfg),
                            "a={:?} b={:?} flags=({dedup_same_name},{dedup_copy_names},{dedup_other_names})",
                            a.name, b.name
                        );
                    }
                }
            }
        }
    }

    // 覆盖 C-03, S-08（SQL 排序与 Rust 参考实现一致：预览与计划的保留者永不分裂）
    #[test]
    fn ordering_sql_agrees_with_rust_compare() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let seed = [
            ("a/x", "x", 5i64, 10u64),
            ("b/yy", "yy", 5, 20),
            ("c", "zzzz", 9, 7),
            ("d/long/name", "n", 5, 20),
            ("e", "mm", 9, 7),
        ];
        // 纯字面量子查询：不建表、不写 DML（static_check 会按任务库 schema 逐条
        // prepare 源码中的 SQL，测试内的建表/插入语句会与 schema 校验冲突）。
        use std::fmt::Write as _;
        let units = |text: &str| text.encode_utf16().count();
        let mut rows =
            format!(
            "SELECT '{}' AS rel, '{}' AS name, {} AS mtime, {} AS size, {} AS name16, {} AS rel16",
            seed[0].0, seed[0].1, seed[0].2, seed[0].3, units(seed[0].1), units(seed[0].0)
        );
        for (rel, name, mtime, size) in &seed[1..] {
            let _ = write!(
                rows,
                " UNION ALL SELECT '{rel}', '{name}', {mtime}, {size}, {}, {}",
                units(name),
                units(rel)
            );
        }
        let records: Vec<FileRecord> = seed
            .iter()
            .map(|(rel, name, mtime, size)| FileRecord {
                id: 0,
                rel: rel.to_string(),
                name: name.to_string(),
                normalized: name.to_string(),
                snapshot: Snapshot {
                    size: *size,
                    modified_ns: *mtime,
                    created_ns: None,
                    identity: rel.to_string(),
                    links: 1,
                },
                hash: None,
                cleanable: false,
            })
            .collect();
        for policy in [
            KeepPolicy::Newest,
            KeepPolicy::Oldest,
            KeepPolicy::ShortestName,
        ] {
            let sql = format!("SELECT rel FROM ({rows}) ORDER BY {}", ordering_sql(policy));
            let sql_order: Vec<String> = conn
                .prepare(&sql)
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            let by_rel = |rel: &str| records.iter().find(|r| r.rel == rel).unwrap();
            let mut rust_order: Vec<&str> = records.iter().map(|r| r.rel.as_str()).collect();
            rust_order.sort_by(|a, b| compare(by_rel(a), by_rel(b), policy));
            assert_eq!(
                sql_order, rust_order,
                "{policy:?} 的 SQL 与 Rust 排序必须一致"
            );
        }
    }

    // 覆盖附录 A（大类映射：复合后缀、分卷族、.ts 归视频、未匹配归其他）
    #[test]
    fn category_matches_appendix_a() {
        for (name, expected) in [
            ("a.mp4", "视频"),
            ("a.TS", "视频"),
            ("a.m2ts", "视频"),
            ("a.mp3", "音频"),
            ("a.opus", "音频"),
            ("a.jpg", "图片"),
            ("a.HEIC", "图片"),
            ("a.raw", "图片"),
            ("a.pdf", "文档"),
            ("a.azw3", "文档"),
            ("a.chm", "文档"),
            ("a.zip", "压缩包"),
            ("a.tar.gz", "压缩包"),
            ("a.TAR.ZST", "压缩包"),
            ("a.lz4", "压缩包"),
            ("a.br", "压缩包"),
            ("a.7z.001", "压缩包"),
            ("a.part01.rar", "压缩包"),
            ("b.r00", "压缩包"),
            ("b.z02", "压缩包"),
            ("b.z00", "其他"),
            ("a.exe", "程序"),
            ("a.ps1", "程序"),
            ("a.msixbundle", "程序"),
            ("a.unknownext", "其他"),
            ("noext", "其他"),
            ("data.001", "其他"),
        ] {
            assert_eq!(category_for(&name.to_lowercase()), expected, "{name}");
        }
    }

    // 覆盖 C-19（命名摘要：SHA-256 大写十六进制、前 8 位起、可扩展）
    #[test]
    fn path_digest_is_uppercase_sha256_prefix() {
        let d8 = path_digest("a/b/合同.pdf", 8);
        assert_eq!(d8.len(), 8);
        assert!(d8
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
        let d10 = path_digest("a/b/合同.pdf", 10);
        assert_eq!(&d10[..8], &d8);
        // 相同输入相同摘要；不同输入（通常）不同。
        assert_eq!(path_digest("a/b/合同.pdf", 8), d8);
    }

    // 覆盖 C-20（长度受限摘要候选：≤40 退让、扩展名不截断、超限返回 None）
    #[test]
    fn digest_candidate_respects_unit_budget() {
        let long_stem = "二".repeat(60);
        // C-20：带来源形式超 40 时去掉来源段，只保留主体前段+摘要+扩展名。
        let candidate =
            digest_candidate(Some("年报"), &long_stem, "A83F21C7", ".pdf", None).unwrap();
        assert!(candidate.encode_utf16().count() <= CONFLICT_YIELD_UNITS);
        assert!(candidate.starts_with("二"));
        assert!(candidate.ends_with("_A83F21C7.pdf"));
        assert!(!candidate.contains('年'));
        // 短主体带来源：保留 C-19 完整形式。
        let with_source =
            digest_candidate(Some("年报"), "产品说明_1", "A13F72C4", ".pdf", None).unwrap();
        assert_eq!(with_source, "年报_产品说明_1_A13F72C4.pdf");
        // 无来源段时省略来源。
        let none_source = digest_candidate(None, "资料", "A83F21C7", ".pdf", None).unwrap();
        assert_eq!(none_source, "资料_A83F21C7.pdf");
        // 扩展名与摘要自身挤爆预算：允许超过 40 但不得超过 255；再放不下则 None。
        let huge_ext = format!(".{}", "e".repeat(300));
        assert!(digest_candidate(None, "x", "A83F21C7", &huge_ext, None).is_none());
    }
}
