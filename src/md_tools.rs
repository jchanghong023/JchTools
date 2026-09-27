//! MD 整理核心逻辑（合同 M 分区）：「合并 MD」与「拆分 MD」。
//! 独立于 GUI：合并按 M-03 排序、M-04/M-05 标题下移、M-06 代码块保护，逐文件逐行流式处理；
//! 拆分按 M-09/M-10 在 UTF-8 安全边界分片，分片按编号顺序二进制拼接可无损还原原文件。
//! 所有原始文件只读；输出一律写入用户指定的新文件（M-02/M-08/M-11）。
//! 扫描不越过链接边界（M-02，与 S-04 同口径）；合并输出先写任务独有的临时文件、
//! 全部成功后改名落盘（M-07：失败或取消不留半成品输出）。

use anyhow::{bail, Context as _, Result};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};
use walkdir::WalkDir;

use crate::control::Control;

/// 合并扫描出的一个输入文件（M-02/M-03）。
#[derive(Debug, Clone)]
pub struct MergeEntry {
    /// 文件路径（以所选根为前缀的绝对路径）
    pub path: PathBuf,
    /// 相对所选根的路径，分隔符统一 `/`（M-03 排序第二键）
    pub rel: String,
    /// 文件名本身（含 `.md` 后缀，M-04 标题文本）
    pub file_name: String,
}

/// Windows 口径的同一文件判定：大小写与分隔符（`/` 与 `\`）不敏感、`..`/`.` 词法消解、
/// 相对路径补当前目录后的路径文本比较。仅用于输出文件排除这一用途；排序仍用原始路径，
/// 不受本归一化影响（M-07：`sub\..\merged.md` 与 `merged.md` 指向同一文件，必须同口径排除）。
fn same_file_path(a: &Path, b: &Path) -> bool {
    fn normalized_absolute_text(path: &Path) -> String {
        use std::path::Component;
        let base = if path.is_absolute() {
            PathBuf::new()
        } else {
            // 相对拼写（如界面手动输入）按当前目录补全后再比较；取不到当前目录时
            // 退化为原样消解，仅影响文本比较，不影响磁盘访问。
            std::env::current_dir().unwrap_or_default()
        };
        let joined = base.join(path);
        let mut prefix = String::new();
        let mut parts: Vec<String> = Vec::new();
        for component in joined.components() {
            match component {
                Component::Prefix(item) => {
                    prefix = item.as_os_str().to_string_lossy().to_lowercase();
                }
                Component::RootDir | Component::CurDir => {}
                Component::ParentDir => {
                    let _ = parts.pop();
                }
                Component::Normal(item) => parts.push(item.to_string_lossy().to_lowercase()),
            }
        }
        if prefix.is_empty() {
            format!("/{}", parts.join("/"))
        } else {
            format!("{prefix}/{}", parts.join("/"))
        }
    }
    normalized_absolute_text(a) == normalized_absolute_text(b)
}

/// M-02/S-04 同口径的入口检查：所选根本身与其路径上的每一级祖先都不得是符号链接 /
/// junction / 其他 reparse point。目录整理（S-04）在「所选根或其访问路径经过链接边界」
/// 时拒绝开始；MD 合并的扫描必须同口径，否则会把链接目标树当作输入读取。
fn ensure_root_is_real_directory(root: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in root.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(meta) => {
                if crate::fsutil::is_link(&meta) {
                    bail!(
                        "所选目录或其上级经过符号链接 / junction（{}）：与目录整理同口径，\
                         MD 整理不读取链接目标，请直接选择真实目录",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                bail!("输入目录不存在或无法访问：{}", root.display());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("无法检查目录边界：{}", current.display()));
            }
        }
    }
    Ok(())
}

/// M-07/M-08 数据安全：输出文件已存在且与任一输入是同一实体（硬链接、`..` 别名、
/// 大小写/分隔符变体等文本比较漏掉的同文件）时，写入会截断另一名字下的原始输入——
/// 必须在进入覆盖确认与任何写入之前拒绝整个任务，要求更换输出名或输出目录。
/// 输出不存在或无法取文件标识（如目标被目录占用）时无从比较，按常规流程继续。
fn reject_output_aliasing_input(output: &Path, entries: &[MergeEntry]) -> Result<()> {
    let Ok(output_snapshot) = crate::fsutil::snapshot(output) else {
        return Ok(());
    };
    for entry in entries {
        let Ok(snapshot) = crate::fsutil::snapshot(&entry.path) else {
            continue; // 单个输入取不到标识不阻断：合并打开该文件时自会给出明确错误
        };
        if snapshot.identity == output_snapshot.identity {
            bail!(
                "输出文件与输入是同一个文件（{} ↔ {}，硬链接或同一实体的不同写法）：\
                 合并会破坏原始输入；请更换输出文件名或输出目录",
                output.display(),
                entry.path.display()
            );
        }
    }
    Ok(())
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// 创建时间不可得时的兜底时间（M-03：不可得按修改时间替代，再不可得取零点）。
fn effective_created(meta: &std::fs::Metadata) -> SystemTime {
    meta.created()
        .or_else(|_| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// 扫描参与合并的 Markdown 文件并按 M-03 排序：创建时间早→晚；
/// 创建时间完全相同时按相对路径自然排序（数字段按数值比较，`2.md` 在 `10.md` 之前）。
/// `exclude` 是本次输出文件路径：位于被扫描目录中时不得作为输入参与合并（M-07）。
/// 扫描不跟随符号链接 / junction 等链接边界（M-02）。
pub fn scan_markdown(
    root: &Path,
    recursive: bool,
    exclude: Option<&Path>,
) -> Result<Vec<MergeEntry>> {
    // M-02/S-04：根自身与全部祖先都不经过链接边界，否则拒绝整个任务
    ensure_root_is_real_directory(root)?;
    let mut rows: Vec<(SystemTime, MergeEntry)> = Vec::new();
    let max_depth = if recursive { usize::MAX } else { 1 };
    for item in WalkDir::new(root)
        .follow_links(false)
        // walkdir 2.5 起根链接默认被跟随（与 follow_links 无关），必须显式关闭（M-02）
        .follow_root_links(false)
        .min_depth(1)
        .max_depth(max_depth)
    {
        let entry = item.with_context(|| format!("扫描目录失败：{}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !is_markdown(path) {
            continue;
        }
        // Windows 路径不区分大小写、两种分隔符等价：输入框与输出框对同一目录的
        // 大小写拼写不同时，按字节比较会漏排除，把本次输出误当输入合并（M-07，
        // 回归见 tests/md_tools.rs merge_excludes_output_regardless_of_path_casing）。
        if exclude.is_some_and(|out| same_file_path(path, out)) {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let meta = std::fs::metadata(path)
            .with_context(|| format!("读取文件属性失败：{}", path.display()))?;
        rows.push((
            effective_created(&meta),
            MergeEntry {
                path: path.to_path_buf(),
                rel,
                file_name,
            },
        ));
    }
    sort_rows(&mut rows);
    let entries: Vec<MergeEntry> = rows.into_iter().map(|(_, entry)| entry).collect();
    // M-07：输出与输入是同一实体（硬链接/别名写法）时在扫描期即拒绝，
    // 界面不得进入覆盖确认——确认后写入会截断另一名字下的原始输入。
    if let Some(exclude) = exclude {
        reject_output_aliasing_input(exclude, &entries)?;
    }
    Ok(entries)
}

/// M-03 排序落地：创建时间早→晚，同一创建时间按相对路径自然排序决胜；
/// 自然排序对数值相等但写法不同的路径（`1.md` / `01.md`）返回 Equal，
/// 追加「归一化原始路径文本（小写、分隔符统一 `/`）字节序」决胜，保证任意
/// 枚举初始顺序都得到同一最终顺序（严格全序、可重复）。
/// 单独成函数供回归测试以受控初始顺序直接驱动（目录枚举顺序不可控，无法检验全序性）。
fn sort_rows(rows: &mut [(SystemTime, MergeEntry)]) {
    rows.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| rel_total_cmp(&a.1.rel, &b.1.rel))
    });
}

/// M-03 第二排序键的全序比较：自然排序优先，耗尽仍相等时用归一化文本决胜
///（归一规则与 `same_file_path` 的文本部分一致：小写、`\` 归一为 `/`；
/// 相对路径不含 `..` 组件，无需绝对化）。
fn rel_total_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn normalize_rel_text(text: &str) -> String {
        text.to_lowercase().replace('\\', "/")
    }
    natural_cmp(a, b).then_with(|| normalize_rel_text(a).cmp(&normalize_rel_text(b)))
}

/// 相对路径自然排序（M-03 第二排序键）：数字段按数值比较（前导零不参与），
/// 非数字段按字节逐字符比较（UTF-8 字节序与 Unicode 码点序一致）。
/// 数值相等但写法不同的路径（`1.md` / `01.md`）在本层返回 Equal；
/// 严格全序由 `rel_total_cmp` 追加归一化文本决胜保证，排序入口一律走后者。
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ai = a.as_bytes();
    let mut bi = b.as_bytes();
    loop {
        match (ai.first(), bi.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(&x), Some(&y)) => {
                if x.is_ascii_digit() && y.is_ascii_digit() {
                    let (an, arest) = split_digits(ai);
                    let (bn, brest) = split_digits(bi);
                    let at = strip_leading_zeros(an);
                    let bt = strip_leading_zeros(bn);
                    match at.len().cmp(&bt.len()) {
                        Ordering::Equal => match at.cmp(bt) {
                            Ordering::Equal => {
                                ai = arest;
                                bi = brest;
                            }
                            other => return other,
                        },
                        other => return other,
                    }
                } else if x.is_ascii_digit() {
                    // 数字段排在非数字段之前（按首字节序，数字字符都小于大多数字母，
                    // 与字节序保持同一口径即可，此处仅保证全序确定性）。
                    return x.cmp(&y).then(Ordering::Less);
                } else if y.is_ascii_digit() {
                    return x.cmp(&y).then(Ordering::Greater);
                } else {
                    match x.cmp(&y) {
                        Ordering::Equal => {
                            ai = &ai[1..];
                            bi = &bi[1..];
                        }
                        other => return other,
                    }
                }
            }
        }
    }
}

fn split_digits(bytes: &[u8]) -> (&[u8], &[u8]) {
    let end = bytes
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(bytes.len());
    bytes.split_at(end)
}

fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|b| *b != b'0').unwrap_or(bytes.len());
    &bytes[start..]
}

/// 合并统计（M-07）。
#[derive(Debug)]
pub struct MergeStats {
    /// 参与合并的文件数
    pub files: usize,
}

/// 标题下移状态机（M-05/M-06）：逐行改写，fenced code block 内的行原样输出。
#[derive(Default)]
struct HeadingShift {
    /// 打开的 fenced code block：（fence 字符，开始长度）
    fence: Option<(u8, usize)>,
    /// 暂缓输出的普通文本行（可能是 Setext 标题文本）：（行体， 行尾字节）
    pending: Option<(Vec<u8>, Vec<u8>)>,
    /// 已读入的输入字节数（空文件判定）
    consumed: usize,
    /// 是否写出过任何字节
    wrote_any: bool,
    /// 已写内容是否以换行结束
    ended_newline: bool,
}

/// 把 `raw`（含行尾）拆成（行体， 行尾）；行尾为 `\n`、`\r\n` 或空（文件末行无换行）。
fn split_ending(raw: &[u8]) -> (&[u8], &[u8]) {
    if raw.last() == Some(&b'\n') {
        let mut end = 1;
        if raw.len() >= 2 && raw[raw.len() - 2] == b'\r' {
            end = 2;
        }
        (&raw[..raw.len() - end], &raw[raw.len() - end..])
    } else {
        (raw, &[])
    }
}

fn leading_spaces(body: &[u8]) -> usize {
    body.iter().take_while(|&&b| b == b' ').count()
}

/// ATX 标题（M-05）：缩进至多三格、1~6 个 `#` 且后接空格/制表符/行尾。
/// 返回（缩进长度， `#` 数量， `#` 之后的内容）。
fn parse_atx(body: &[u8]) -> Option<(usize, usize, &[u8])> {
    let indent = leading_spaces(body);
    if indent > 3 {
        return None;
    }
    let rest = &body[indent..];
    let hashes = rest.iter().take_while(|&&b| b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let after = &rest[hashes..];
    if !after.is_empty() && after[0] != b' ' && after[0] != b'\t' {
        return None;
    }
    Some((indent, hashes, after))
}

/// fenced code block 开始（M-06）：缩进至多三格、``` 或 `~~~` 至少三个。
fn parse_fence(body: &[u8]) -> Option<(u8, usize)> {
    let indent = leading_spaces(body);
    if indent > 3 {
        return None;
    }
    let rest = &body[indent..];
    let &first = rest.first()?;
    if first != b'`' && first != b'~' {
        return None;
    }
    let count = rest.iter().take_while(|&&b| b == first).count();
    (count >= 3).then_some((first, count))
}

/// 当前行是否为闭合 fence：与开始 fence 同字符、长度不小于开始长度、之后仅空格。
fn is_fence_close(body: &[u8], fence: (u8, usize)) -> bool {
    let indent = leading_spaces(body);
    if indent > 3 {
        return false;
    }
    let rest = &body[indent..];
    let trimmed = trim_trailing_spaces(rest);
    trimmed.first() == Some(&fence.0)
        && trimmed.iter().all(|&b| b == fence.0)
        && trimmed.len() >= fence.1
}

fn trim_trailing_spaces(mut bytes: &[u8]) -> &[u8] {
    while bytes.last() == Some(&b' ') {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

/// Setext 下划线（M-05）：缩进至多三格、整行全为 `=`（一级）或 `-`（二级）。
/// 只有紧跟普通文本行（pending 存在）时才构成标题，由调用方保证。
fn setext_level(body: &[u8]) -> Option<u8> {
    let indent = leading_spaces(body);
    if indent > 3 {
        return None;
    }
    let trimmed = trim_trailing_spaces(&body[indent..]);
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.iter().all(|&b| b == b'=') {
        Some(1)
    } else if trimmed.iter().all(|&b| b == b'-') {
        Some(2)
    } else {
        None
    }
}

/// 是否为空行。
fn is_blank(body: &[u8]) -> bool {
    body.iter().all(u8::is_ascii_whitespace)
}

/// 是否为其他块结构起始行（列表、引用等）：这些行不能作为 Setext 标题文本，
/// 其后的下划线行按原样内容（thematic break 等）输出。
fn is_block_start(body: &[u8]) -> bool {
    if leading_spaces(body) >= 4 {
        return true; // 缩进代码块
    }
    let rest = trim_trailing_spaces(body);
    let Some(&first) = rest.first() else {
        return true; // 空行
    };
    match first {
        // 无序列表标记只有在后接空格/制表符或行尾时才是真列表语法（CommonMark）；
        // `*important*`、`+name`、`-text` 这类强调或普通文本不是块起始，必须保留
        // Setext 资格（M-05：下划线行紧跟非空文本行即构成标题），否则其后的
        // `===` / `---` 下划线不再转换。
        b'-' | b'*' | b'+' => match rest.get(1) {
            None => true, // 孤立标记（行尾）按列表项处理
            Some(&second) => second == b' ' || second == b'\t',
        },
        b'>' => true, // 引用块：`>` 后不需要空格
        b'0'..=b'9' => rest
            .iter()
            .skip(1)
            .find(|b| !b.is_ascii_digit())
            .is_some_and(|b| *b == b'.' || *b == b')'),
        _ => false,
    }
}

impl HeadingShift {
    fn feed(&mut self, raw: &[u8], out: &mut impl Write) -> Result<()> {
        self.consumed += raw.len();
        let (body, ending) = split_ending(raw);
        if let Some(fence) = self.fence {
            out.write_all(raw)
                .with_context(|| "写合并输出失败（代码块行）".to_string())?;
            self.wrote_any = true;
            self.ended_newline = !ending.is_empty();
            if is_fence_close(body, fence) {
                self.fence = None;
            }
            return Ok(());
        }
        // 暂缓文本行的处置：下一行是 Setext 下划线则合并为标题，否则原样补写（M-05）
        if let Some((text, pending_ending)) = self.pending.take() {
            if let Some(level) = setext_level(body) {
                self.write_setext(out, &text, level, ending)?;
                return Ok(()); // 下划线行并入标题，不再单独输出
            }
            out.write_all(&text)
                .and_then(|()| out.write_all(&pending_ending))
                .with_context(|| "写合并输出失败（文本行）".to_string())?;
            self.wrote_any = true;
            self.ended_newline = !pending_ending.is_empty();
        }
        if let Some(fence) = parse_fence(body) {
            out.write_all(raw)
                .with_context(|| "写合并输出失败（fence 行）".to_string())?;
            self.wrote_any = true;
            self.ended_newline = !ending.is_empty();
            self.fence = Some(fence);
            return Ok(());
        }
        if let Some((indent, hashes, rest)) = parse_atx(body) {
            // M-05：下移一级、六级封顶（不产生七级）
            let shifted = hashes.saturating_add(1).min(6);
            out.write_all(&body[..indent])
                .and_then(|()| {
                    for _ in 0..shifted {
                        out.write_all(b"#")?;
                    }
                    Ok(())
                })
                .and_then(|()| out.write_all(rest))
                .with_context(|| "写合并输出失败（标题行）".to_string())?;
            if ending.is_empty() {
                out.write_all(b"\n")?;
            } else {
                out.write_all(ending)?;
            }
            self.wrote_any = true;
            self.ended_newline = true;
            return Ok(());
        }
        if is_blank(body) || is_block_start(body) {
            out.write_all(raw)
                .with_context(|| "写合并输出失败（普通行）".to_string())?;
            self.wrote_any = true;
            self.ended_newline = !ending.is_empty();
            return Ok(());
        }
        // 普通文本行暂缓一行输出，等待 Setext 判定
        self.pending = Some((body.to_vec(), ending.to_vec()));
        Ok(())
    }

    /// Setext 标题转换（M-05）：`=` 下划线 → `##`，`-` 下划线 → `###`。
    fn write_setext(
        &mut self,
        out: &mut impl Write,
        text: &[u8],
        level: u8,
        ending: &[u8],
    ) -> Result<()> {
        let indent = leading_spaces(text);
        let hashes: &[u8] = if level == 1 { b"##" } else { b"###" };
        let content = &text[indent..];
        out.write_all(&text[..indent])
            .and_then(|()| out.write_all(hashes))
            .and_then(|()| {
                if !content.is_empty() {
                    out.write_all(b" ")?;
                    out.write_all(content)?;
                }
                Ok(())
            })
            .with_context(|| "写合并输出失败（Setext 转换）".to_string())?;
        if ending.is_empty() {
            out.write_all(b"\n")?;
        } else {
            out.write_all(ending)?;
        }
        self.ended_newline = true;
        self.wrote_any = true;
        Ok(())
    }

    /// 输入结束时补写暂缓文本行。
    fn finish(&mut self, out: &mut impl Write) -> Result<()> {
        if let Some((text, ending)) = self.pending.take() {
            out.write_all(&text)
                .and_then(|()| out.write_all(&ending))
                .with_context(|| "写合并输出失败（收尾行）".to_string())?;
            self.wrote_any = true;
            self.ended_newline = !ending.is_empty();
        }
        Ok(())
    }
}

/// MD 任务的单项进度事件（U-03/U-12）：`FileStarted` 在开始处理某项之前发出，
/// `FileCompleted` 在该项全部写完之后发出；界面的完成计数只应由 `FileCompleted` 驱动，
/// 否则单文件任务开局就会显示 1/1（100%）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdProgress {
    /// 第 `index` 项（1 起）开始处理；`total` 为本次任务的总项数（合并=文件数，拆分=分片数）
    FileStarted(usize, usize),
    /// 第 `index` 项已全部写完
    FileCompleted(usize, usize),
}

/// 合并的行循环每处理多少行响应一次取消/暂停（U-12：文件内也要可停，不能只在文件边界）。
const MERGE_CHECKPOINT_LINES: usize = 512;

/// 在输出目录内创建本次任务独有的临时文件（F05/M-07：先写临时文件再改名落盘，
/// 失败或取消时不留半成品输出）。名称带进程号、纳秒时戳与递增序号且不以 `.md` 结尾，
/// 不会被下一次扫描当作输入；`create_new` 保证绝不覆盖任何已有文件。
fn create_merge_temp_output(dir: &Path) -> Result<(PathBuf, File)> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let mut attempt = 0u32;
    loop {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |delta| delta.as_nanos());
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!(
            ".jchtools-md-out-{}-{nanos}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists && attempt < 8 => {
                attempt += 1;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("创建输出临时文件失败：{}", candidate.display()));
            }
        }
    }
}

/// 按 M-04~M-07 把 `entries` 流式合并写入 `output`（带 Started/Completed 事件的入口）。
/// `overwrite=false` 且输出已存在时报错（调用方必须先完成覆盖确认）。
/// 写入先落在本任务独有的临时文件上，全部成功后改名到目标（M-07：失败/取消不留
/// 半成品输出；输出与任一输入是同一实体时在任何写入前拒绝，要求更换输出名）。
pub fn merge_markdown_with_events(
    entries: &[MergeEntry],
    output: &Path,
    overwrite: bool,
    control: &Control,
    on_event: &dyn Fn(MdProgress) -> Result<()>,
) -> Result<MergeStats> {
    // 同实体拒绝先于覆盖确认：硬链接/别名输出无论如何确认都不允许写
    reject_output_aliasing_input(output, entries)?;
    if output.exists() && !overwrite {
        bail!(
            "输出文件已存在：{}（未确认覆盖，拒绝写入）",
            output.display()
        );
    }
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("创建输出目录失败：{}", parent.display()))?;
    let (temp_path, target) = create_merge_temp_output(parent)?;
    match write_merge_entries(entries, target, control, on_event) {
        Ok(()) => match std::fs::rename(&temp_path, output) {
            Ok(()) => Ok(MergeStats {
                files: entries.len(),
            }),
            Err(error) => {
                let _ = std::fs::remove_file(&temp_path);
                Err(error).with_context(|| {
                    format!(
                        "输出文件落盘失败（{} → {}）：目标可能被占用或权限不足，已清理临时文件",
                        temp_path.display(),
                        output.display()
                    )
                })
            }
        },
        Err(error) => {
            // 取消/失败：删除本次任务拥有的临时文件，不触碰任何已有输出
            let _ = std::fs::remove_file(&temp_path);
            Err(error)
        }
    }
}

/// 逐文件逐行把 `entries` 写入已打开的 `target`（M-04~M-06 转换规则与文件边界逻辑）。
fn write_merge_entries(
    entries: &[MergeEntry],
    target: File,
    control: &Control,
    on_event: &dyn Fn(MdProgress) -> Result<()>,
) -> Result<()> {
    let mut out = BufWriter::with_capacity(512 * 1024, target);
    let mut prev_empty = true;
    for (index, entry) in entries.iter().enumerate() {
        control.checkpoint()?;
        on_event(MdProgress::FileStarted(index + 1, entries.len()))?;
        // M-04：文件之间保证合理空行，防止上一文件最后一行与下一文件标题粘连
        if index > 0 && !prev_empty {
            out.write_all(b"\n")
                .with_context(|| "写合并输出失败（文件分隔）".to_string())?;
        }
        let header = format!("# {}\n\n", entry.file_name);
        out.write_all(header.as_bytes())
            .with_context(|| "写合并输出失败（文件标题）".to_string())?;
        let source = File::open(&entry.path)
            .with_context(|| format!("打开输入文件失败：{}", entry.path.display()))?;
        let mut reader = BufReader::with_capacity(256 * 1024, source);
        let mut shifter = HeadingShift::default();
        let mut first_line = true;
        let mut lines_since_checkpoint = 0usize;
        loop {
            let mut line: Vec<u8> = Vec::with_capacity(1024);
            let read = reader
                .read_until(b'\n', &mut line)
                .with_context(|| format!("读取输入文件失败：{}", entry.path.display()))?;
            if read == 0 {
                break;
            }
            // U-12：文件内按行粒度响应取消/暂停，超大单文件也能及时停下
            lines_since_checkpoint += 1;
            if lines_since_checkpoint >= MERGE_CHECKPOINT_LINES {
                control.checkpoint()?;
                lines_since_checkpoint = 0;
            }
            if first_line {
                first_line = false;
                // 某些编辑器（Windows 旧版记事本、PowerShell 重定向等）会写出带 UTF-8 BOM 的
                // 文件；BOM 字节会让首行不再被识别为标题，破坏 M-04/M-05 的下移（CommonMark
                // 口径：文档开头的 BOM 被忽略）。只在每个文件开头剥除恰好一次，文件中间出现
                // 的相同字节是普通文本、原样保留；拆分路径（M-09/M-10）是字节级无损操作，不剥。
                const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
                if line.starts_with(&UTF8_BOM) {
                    line.drain(..UTF8_BOM.len());
                    if line.is_empty() {
                        continue; // 整个文件只有一个 BOM：按空文件处理
                    }
                }
            }
            shifter.feed(&line, &mut out)?;
        }
        shifter.finish(&mut out)?;
        // 内容不以换行结束时补一个，保证下一文件标题不粘连（M-04）
        if shifter.wrote_any && !shifter.ended_newline {
            out.write_all(b"\n")
                .with_context(|| "写合并输出失败（末行换行）".to_string())?;
        }
        prev_empty = shifter.consumed == 0;
        on_event(MdProgress::FileCompleted(index + 1, entries.len()))?;
    }
    out.flush().context("写输出文件失败（磁盘可能已满）")?;
    Ok(())
}

/// 按 M-04~M-07 把 `entries` 流式合并写入 `output`（兼容旧入口）。
/// `overwrite=false` 且输出已存在时报错（调用方必须先完成覆盖确认）；
/// 每个文件处理前回调 `on_file(当前序号, 总数)`，并在文件边界响应取消与进度。
pub fn merge_markdown(
    entries: &[MergeEntry],
    output: &Path,
    overwrite: bool,
    control: &Control,
    on_file: &dyn Fn(usize, usize) -> Result<()>,
) -> Result<MergeStats> {
    merge_markdown_with_events(entries, output, overwrite, control, &|event| match event {
        MdProgress::FileStarted(index, total) => on_file(index, total),
        MdProgress::FileCompleted(..) => Ok(()),
    })
}

/// 拆分计划（M-09/M-10）：`bounds[i]` 是第 i 片的结束字节偏移（严格递增）。
#[derive(Debug)]
pub struct SplitPlan {
    pub bounds: Vec<u64>,
    /// 每片的最大字节数（用户设置的硬限制）
    pub limit: u64,
}

/// 规划分片边界：每片 ≤ limit 且不切断多字节字符（M-09/M-10）。
/// 限制小到无法同时满足两个要求时报错（此时不产生任何输出，由两遍法保证）。
pub fn plan_splits(input: &Path, limit: u64) -> Result<SplitPlan> {
    let len = std::fs::metadata(input)
        .with_context(|| format!("读取输入文件属性失败：{}", input.display()))?
        .len();
    if limit == 0 {
        bail!("拆分大小必须大于 0 字节");
    }
    if len == 0 {
        return Ok(SplitPlan {
            bounds: Vec::new(),
            limit,
        });
    }
    let mut file =
        File::open(input).with_context(|| format!("打开输入文件失败：{}", input.display()))?;
    let mut bounds = Vec::new();
    let mut start = 0u64;
    while start < len {
        let target = start.saturating_add(limit).min(len);
        if target == len {
            bounds.push(len);
            break;
        }
        let cut = prev_boundary(&mut file, target, start)
            .with_context(|| format!("规划拆分边界失败：{}", input.display()))?;
        if cut <= start {
            bail!(
                "拆分限制（{limit} 字节）过小：无法在不切断多字节字符的前提下分片（M-10）；请增大限制"
            );
        }
        bounds.push(cut);
        start = cut;
    }
    Ok(SplitPlan { bounds, limit })
}

/// 取 ≤target 的最大 UTF-8 安全边界：位置 p 是边界 ⟺ 该处字节不是 UTF-8 连续字节
/// （`10xxxxxx`）。合法 UTF-8 的连续字节串至多 3 个，窗口取 8 字节足够；
/// 极端非法序列全部为连续字节时回退 start+1（字节内容仍不丢失，无损性不受影响）。
fn prev_boundary(file: &mut File, target: u64, start: u64) -> Result<u64> {
    // 窗口须包含 start 自身：start 处的字符若跨过 target（限制小于该字符的字节数），
    // 从 target 回溯遇到的第一个非连续字节正是 start，据此判定「限制过小」而非兜底切分。
    let lo = target.saturating_sub(8).max(start);
    let count = usize::try_from(target - lo + 1).unwrap_or(9).min(9);
    let mut buf = [0u8; 9];
    file.seek(SeekFrom::Start(lo))
        .with_context(|| "定位拆分边界失败".to_string())?;
    file.read_exact(&mut buf[..count])
        .with_context(|| "读取拆分边界字节失败".to_string())?;
    for offset in (0..count).rev() {
        if buf[offset] & 0xC0 != 0x80 {
            let position = lo + u64::try_from(offset).unwrap_or(0);
            if position > start {
                return Ok(position);
            }
            // 唯一的非连续字节就在 start：start 处的多字节字符长于剩余限制，
            // 任何 ≤target 的切分都会切断它——按限制过小报错（M-10），绝不兜底硬切。
            bail!("限制小于单个多字节字符的字节数，无法在字符边界分片");
        }
    }
    // 窗口内全部是连续字节：只可能出现在非法 UTF-8 序列（合法序列至多 3 个连续字节）；
    // 非法序列无字符语义，按单字节推进保证任务可完成，字节内容仍不丢失（无损性不受影响）。
    Ok(start + 1)
}

/// 拆分输出文件名（M-11）：`document.md` → `document_001.md`、`document_002.md`…
/// 至少三位编号；分片总数超过 999 时统一扩大编号宽度（按总数确定），
/// 保证文件名排序与编号数值顺序一致、互不重名。
pub fn split_names(file_name: &str, count: usize) -> Vec<String> {
    let stem = file_name.rsplit_once('.').map_or(file_name, |(s, _)| s);
    let width = count.to_string().len().max(3);
    (1..=count)
        .map(|i| format!("{stem}_{i:0width$}.md"))
        .collect()
}

/// 目标目录中已存在的同名分片（M-11：写入前检测，不静默覆盖）。
pub fn conflicting_outputs(out_dir: &Path, names: &[String]) -> Vec<PathBuf> {
    names
        .iter()
        .map(|name| out_dir.join(name))
        .filter(|path| path.exists())
        .collect()
}

/// 按计划写出全部分片，返回写入的总字节数（带 Started/Completed 事件的入口）。
/// `overwrite=false` 且任一目标已存在时报错（调用方必须先完成冲突确认，M-11）。
/// 逐片分块复制，不在内存中持有整个文件（M-11 大文件实现）。
pub fn run_split_with_events(
    input: &Path,
    plan: &SplitPlan,
    out_dir: &Path,
    overwrite: bool,
    control: &Control,
    on_event: &dyn Fn(MdProgress) -> Result<()>,
) -> Result<u64> {
    let file_name = input
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let names = split_names(&file_name, plan.bounds.len());
    if !overwrite {
        let hits = conflicting_outputs(out_dir, &names);
        if !hits.is_empty() {
            bail!(
                "目标目录已存在同名分片（{} 个，如 {}）；未确认覆盖，拒绝写入",
                hits.len(),
                hits.first()
                    .map_or(String::new(), |p| p.display().to_string())
            );
        }
    }
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("创建输出目录失败：{}", out_dir.display()))?;
    let mut reader =
        File::open(input).with_context(|| format!("打开输入文件失败：{}", input.display()))?;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut written = 0u64;
    let mut start = 0u64;
    for (index, end) in plan.bounds.iter().enumerate() {
        control.checkpoint()?;
        on_event(MdProgress::FileStarted(index + 1, plan.bounds.len()))?;
        let path = out_dir.join(&names[index]);
        let target =
            File::create(&path).with_context(|| format!("创建分片失败：{}", path.display()))?;
        let mut out = BufWriter::with_capacity(256 * 1024, target);
        reader
            .seek(SeekFrom::Start(start))
            .with_context(|| format!("定位输入失败：{}", input.display()))?;
        let mut remain = end.saturating_sub(start);
        while remain > 0 {
            // U-12：分片复制循环内逐块响应取消/暂停，超大单片也能及时停下
            control.checkpoint()?;
            let want = usize::try_from(remain)
                .unwrap_or(buffer.len())
                .min(buffer.len());
            let got = reader
                .read(&mut buffer[..want])
                .with_context(|| format!("读取输入文件失败：{}", input.display()))?;
            if got == 0 {
                bail!("读取输入意外结束：{}", input.display());
            }
            out.write_all(&buffer[..got])
                .with_context(|| format!("写分片失败：{}", path.display()))?;
            written += u64::try_from(got).unwrap_or(0);
            remain -= u64::try_from(got).unwrap_or(0);
        }
        out.flush()
            .with_context(|| format!("写分片失败（磁盘可能已满）：{}", path.display()))?;
        start = *end;
        on_event(MdProgress::FileCompleted(index + 1, plan.bounds.len()))?;
    }
    Ok(written)
}

/// 按计划写出全部分片，返回写入的总字节数（兼容旧入口）。
/// `overwrite=false` 且任一目标已存在时报错（调用方必须先完成冲突确认，M-11）。
/// 每片处理前回调 `on_part(当前序号, 总数)`，并在分片边界响应取消与进度。
pub fn run_split(
    input: &Path,
    plan: &SplitPlan,
    out_dir: &Path,
    overwrite: bool,
    control: &Control,
    on_part: &dyn Fn(usize, usize) -> Result<()>,
) -> Result<u64> {
    run_split_with_events(
        input,
        plan,
        out_dir,
        overwrite,
        control,
        &|event| match event {
            MdProgress::FileStarted(index, total) => on_part(index, total),
            MdProgress::FileCompleted(..) => Ok(()),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{mpsc, Arc, Mutex},
        time::Duration,
    };

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    /// 单文件合并的便捷入口：写入一个输入文件后按主流程（扫描排除输出 → 事件入口合并）执行。
    fn merge_single(root: &Path, file: &str, content: &str) -> String {
        write(&root.join(file), content);
        let output = root.join("merged.md");
        let entries = scan_markdown(root, true, Some(&output)).unwrap();
        merge_markdown_with_events(&entries, &output, false, &Control::default(), &|_| Ok(()))
            .unwrap();
        fs::read_to_string(&output).unwrap()
    }

    /// 用 `cmd /c mklink /J` 创建目录 junction（P-07：Windows 唯一支持平台）。
    fn make_junction(link: &Path, target: &Path) {
        let output = std::process::Command::new("cmd")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()
            .expect("启动 cmd 失败");
        assert!(
            output.status.success(),
            "mklink /J 失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // 覆盖 M-02/S-04（根是 junction：拒绝整个任务，不读取目标树；普通目录正常扫描）
    #[test]
    fn scan_rejects_junction_root_and_accepts_real_directory() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("a.md"), "# 甲").unwrap();
        let link = dir.path().join("link");
        make_junction(&link, &real);
        let error = scan_markdown(&link, true, None).expect_err("根是 junction 时必须拒绝任务");
        let text = format!("{error:#}");
        assert!(
            text.contains("链接") || text.contains("junction"),
            "错误信息须说明链接边界被拒绝：{text}"
        );
        // 正控：真实目录照常扫描（M-02 不因链接防护误伤普通目录）
        let entries = scan_markdown(&real, true, None).unwrap();
        assert_eq!(entries.len(), 1, "普通目录必须正常扫描");
    }

    // 覆盖 M-02/S-04（祖先 junction：所选根的访问路径经过链接边界时同样拒绝）
    #[test]
    fn scan_rejects_junction_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir_all(real.join("sub")).unwrap();
        fs::write(real.join("sub").join("a.md"), "# 甲").unwrap();
        let link = dir.path().join("link");
        make_junction(&link, &real);
        let error = scan_markdown(&link.join("sub"), true, None)
            .expect_err("访问路径经过 junction 时必须拒绝任务");
        let text = format!("{error:#}");
        assert!(
            text.contains("链接") || text.contains("junction"),
            "错误信息须说明链接边界被拒绝：{text}"
        );
    }

    // 覆盖 M-07（输出路径用「..」拼写时仍必须与普通拼写同口径排除本次输出）
    #[test]
    fn scan_excludes_output_written_with_dotdot_alias() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("a.md"), "# 甲");
        write(&root.join("merged.md"), "旧输出");
        let output = root.join("sub").join("..").join("merged.md");
        let entries = scan_markdown(root, true, Some(&output)).unwrap();
        let rels: Vec<&str> = entries.iter().map(|entry| entry.rel.as_str()).collect();
        assert_eq!(
            rels,
            vec!["a.md"],
            "「..」拼写的输出路径必须被排除（M-07）：{rels:?}"
        );
    }

    // 覆盖 M-07（输出路径用 `/` 分隔符拼写时仍必须被排除）
    #[test]
    fn scan_excludes_output_written_with_forward_slashes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("a.md"), "# 甲");
        write(&root.join("merged.md"), "旧输出");
        let output = PathBuf::from(format!("{}/merged.md", root.display()));
        let entries = scan_markdown(root, true, Some(&output)).unwrap();
        let rels: Vec<&str> = entries.iter().map(|entry| entry.rel.as_str()).collect();
        assert_eq!(
            rels,
            vec!["a.md"],
            "分隔符变体拼写的输出必须被排除：{rels:?}"
        );
    }

    // 覆盖 M-07（输出与输入是同一实体的硬链接：扫描期拒绝任务，绝不进入覆盖确认，
    // 更不得在确认覆盖后经输出名截断另一名字下的原始输入）
    #[test]
    fn merge_rejects_output_hardlinked_to_input() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let original = b"# \xe5\x8e\x9f\xe5\xa7\x8b\xe5\x86\x85\xe5\xae\xb9\n"; // "# 原始内容\n"
        fs::write(root.join("a.md"), original).unwrap();
        let output = root.join("out.md");
        fs::hard_link(root.join("a.md"), &output).unwrap();
        let error = scan_markdown(root, true, Some(&output))
            .expect_err("输出与输入是同一实体的硬链接：必须拒绝整个任务");
        assert!(
            format!("{error:#}").contains("同一个文件"),
            "错误信息须说明输出与输入是同一个文件：{error:#}"
        );
        assert_eq!(
            fs::read(root.join("a.md")).unwrap(),
            original,
            "原始输入字节必须原封不动"
        );
    }

    // 覆盖 M-07（正常覆盖既有输出：确认后经临时文件原子替换，不留临时文件，输入不动）
    #[test]
    fn merge_atomic_overwrite_existing_output_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let a_before = b"# \xe7\x94\xb2\n".to_vec(); // "# 甲\n"
        let b_before = b"# \xe4\xb9\x99\n".to_vec(); // "# 乙\n"
        fs::write(root.join("a.md"), &a_before).unwrap();
        fs::write(root.join("b.md"), &b_before).unwrap();
        let output = root.join("merged.md");
        fs::write(&output, "旧输出").unwrap();
        let entries = scan_markdown(root, true, Some(&output)).unwrap();
        assert_eq!(entries.len(), 2);
        merge_markdown_with_events(&entries, &output, true, &Control::default(), &|_| Ok(()))
            .unwrap();
        let text = fs::read_to_string(&output).unwrap();
        assert!(text.contains("# a.md") && text.contains("# b.md"), "{text}");
        assert!(
            !text.contains("旧输出"),
            "确认覆盖后旧内容必须被替换：{text}"
        );
        assert_eq!(
            fs::read(root.join("a.md")).unwrap(),
            a_before,
            "输入不得被改动"
        );
        assert_eq!(
            fs::read(root.join("b.md")).unwrap(),
            b_before,
            "输入不得被改动"
        );
        for entry in fs::read_dir(root).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                !name.starts_with(".jchtools-md-out-"),
                "任务结束后不得残留临时文件：{name}"
            );
        }
    }

    // 覆盖 M-07（输出目标被目录占用：必须明确报错，不得留下临时文件或破坏该目录）
    #[test]
    fn merge_rename_failure_reports_and_cleans_temp() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("a.md"), "# 甲");
        let blocker = root.join("blocker.md");
        fs::create_dir(&blocker).unwrap();
        let entries = scan_markdown(root, true, Some(&blocker)).unwrap();
        assert_eq!(entries.len(), 1);
        let error =
            merge_markdown_with_events(&entries, &blocker, true, &Control::default(), &|_| Ok(()))
                .expect_err("输出目标被目录占用时必须报错");
        assert!(
            format!("{error:#}").contains("失败"),
            "错误信息须明确失败上下文：{error:#}"
        );
        assert!(blocker.is_dir(), "被占用的目录不得被破坏");
        for entry in fs::read_dir(root).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                !name.starts_with(".jchtools-md-out-"),
                "失败收尾不得残留临时文件：{name}"
            );
        }
    }

    fn rows_for(rels: &[&str]) -> Vec<(SystemTime, MergeEntry)> {
        rels.iter()
            .map(|rel| {
                (
                    SystemTime::UNIX_EPOCH,
                    MergeEntry {
                        path: PathBuf::from(format!("/{rel}")),
                        rel: (*rel).to_string(),
                        file_name: rel.rsplit('/').next().unwrap().to_string(),
                    },
                )
            })
            .collect()
    }

    fn rels_of(rows: &[(SystemTime, MergeEntry)]) -> Vec<String> {
        rows.iter().map(|(_, entry)| entry.rel.clone()).collect()
    }

    // 覆盖 M-03（自然排序对数值相等但写法不同的路径不构成全序：追加归一化文本决胜后，
    // 任意初始枚举顺序必须得到同一最终顺序）
    #[test]
    fn sort_is_total_order_across_permutations() {
        // 1.md / 01.md / 001.md 数值段剥零比较全部相等——旧实现直接返回 Equal，
        // 顺序退化为枚举序。全部 6 种初始排列排序后必须得到同一结果。
        let permutations: Vec<Vec<&str>> = vec![
            vec!["1.md", "01.md", "001.md"],
            vec!["1.md", "001.md", "01.md"],
            vec!["01.md", "1.md", "001.md"],
            vec!["01.md", "001.md", "1.md"],
            vec!["001.md", "1.md", "01.md"],
            vec!["001.md", "01.md", "1.md"],
        ];
        let mut orders: Vec<Vec<String>> = Vec::new();
        for permutation in &permutations {
            let mut rows = rows_for(permutation);
            sort_rows(&mut rows);
            orders.push(rels_of(&rows));
        }
        for order in &orders[1..] {
            assert_eq!(
                order, &orders[0],
                "不同初始顺序必须得到同一最终顺序（M-03）"
            );
        }
        assert_eq!(
            orders[0],
            vec![
                "001.md".to_string(),
                "01.md".to_string(),
                "1.md".to_string()
            ],
            "决胜顺序必须确定（归一化文本字节序）"
        );
        // 混入数值不同的路径：数值比较仍优先于文本决胜
        let mut rows = rows_for(&["10.md", "9.md", "010.md", "009.md"]);
        sort_rows(&mut rows);
        assert_eq!(
            rels_of(&rows),
            vec![
                "009.md".to_string(),
                "9.md".to_string(),
                "010.md".to_string(),
                "10.md".to_string(),
            ],
            "数值序优先：9 < 10；同数值按归一化文本决胜：009 < 010、009 < 9"
        );
    }

    // 覆盖 M-03（同一创建时间的 1.md/01.md/001.md：端到端扫描顺序确定）
    #[test]
    fn scan_orders_equal_creation_times_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in ["1.md", "01.md", "001.md"] {
            write(&root.join(name), "x");
            crate::fsutil::set_created_time(
                &root.join(name),
                SystemTime::UNIX_EPOCH + Duration::from_secs(100),
            )
            .unwrap();
        }
        let output = root.join("merged.md");
        let entries = scan_markdown(root, true, Some(&output)).unwrap();
        assert_eq!(
            rels_of(
                &entries
                    .into_iter()
                    .map(|entry| (SystemTime::UNIX_EPOCH, entry))
                    .collect::<Vec<_>>()
            ),
            vec![
                "001.md".to_string(),
                "01.md".to_string(),
                "1.md".to_string()
            ],
            "同创建时间必须按确定的第二排序键排出全序（M-03）"
        );
    }

    // 覆盖 M-05（Setext 判定要求下划线行紧跟「非空文本行」：*important* 是强调文本
    // 不是列表（标记后无空格），其后的 === 必须转换为 ATX 二级标题）
    #[test]
    fn merge_converts_setext_after_emphasis_like_text() {
        let dir = tempfile::tempdir().unwrap();
        let text = merge_single(dir.path(), "emph.md", "*important*\n===\n");
        assert_eq!(
            text, "# emph.md\n\n## *important*\n",
            "强调文本行是普通文本，=== 下划线必须照常转换：{text:?}"
        );
    }

    // 覆盖 M-05（+name / -text 同为非列表文本：其后的 Setext 下划线必须转换）
    #[test]
    fn merge_converts_setext_after_plus_and_dash_text() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("plus.md"), "+name\n===\n");
        write(&root.join("dash.md"), "-text\n---\n");
        let output = root.join("merged.md");
        let entries = scan_markdown(root, true, Some(&output)).unwrap();
        merge_markdown_with_events(&entries, &output, false, &Control::default(), &|_| Ok(()))
            .unwrap();
        let text = fs::read_to_string(&output).unwrap();
        assert!(
            text.contains("## +name"),
            "+name 不是列表（标记后无空格），=== 必须转换：{text:?}"
        );
        assert!(
            text.contains("### -text"),
            "-text 不是列表（标记后无空格），--- 必须转换：{text:?}"
        );
    }

    // 覆盖 M-05/M-06（真列表与围栏内部不因新判定被误转换）
    #[test]
    fn merge_keeps_real_list_and_fence_interior_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("list.md"), "- 真列表项\n\n---\n");
        let text_list = {
            let output = root.join("merged.md");
            let entries = scan_markdown(root, true, Some(&output)).unwrap();
            merge_markdown_with_events(&entries, &output, false, &Control::default(), &|_| Ok(()))
                .unwrap();
            fs::read_to_string(&output).unwrap()
        };
        assert!(
            text_list.contains("- 真列表项") && text_list.contains("\n---\n"),
            "真列表与 thematic break 保持原样：{text_list:?}"
        );
        assert!(
            !text_list.contains("### "),
            "真列表后的 --- 不是 Setext：{text_list:?}"
        );

        let dir2 = tempfile::tempdir().unwrap();
        let text_fence = merge_single(dir2.path(), "fence.md", "```\n*important*\n===\n```\n");
        assert_eq!(
            text_fence, "# fence.md\n\n```\n*important*\n===\n```\n",
            "围栏代码块内部的相同文本必须原样（M-06）：{text_fence:?}"
        );
    }

    // 覆盖 U-03/U-12（单文件任务：FileStarted 发出时完成计数不得把当前文件算进去，
    // 事件序列必须是 Started → Completed）
    #[test]
    fn merge_events_started_before_completed_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("a.md"), "# 甲\n");
        let output = root.join("merged.md");
        let entries = scan_markdown(root, true, Some(&output)).unwrap();
        assert_eq!(entries.len(), 1);
        let completed = std::cell::Cell::new(0usize);
        let seen = std::cell::RefCell::new(Vec::new());
        merge_markdown_with_events(&entries, &output, false, &Control::default(), &|event| {
            match event {
                MdProgress::FileStarted(index, total) => {
                    assert_eq!((index, total), (1, 1));
                    assert_eq!(
                        completed.get(),
                        index - 1,
                        "FileStarted 发出时完成计数不得已包含当前文件（单文件开局不得显示 1/1）"
                    );
                }
                MdProgress::FileCompleted(index, _total) => {
                    completed.set(index);
                }
            }
            seen.borrow_mut().push(event);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            seen.into_inner(),
            vec![
                MdProgress::FileStarted(1, 1),
                MdProgress::FileCompleted(1, 1)
            ],
            "事件序列必须先 Started 后 Completed"
        );
        assert_eq!(completed.into_inner(), 1, "任务完成后完成计数必须达到总数");
    }

    // 覆盖 U-12（合并：第 1 个文件完成后取消，第 2 个文件不得开始，输出不落盘）
    #[test]
    fn merge_stop_between_files_never_starts_next() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("a.md"), "# 甲\n");
        write(&root.join("b.md"), "# 乙\n");
        let output = root.join("merged.md");
        let entries = scan_markdown(root, true, Some(&output)).unwrap();
        assert_eq!(entries.len(), 2);
        let control = Control::default();
        let seen = std::cell::RefCell::new(Vec::new());
        let error = merge_markdown_with_events(&entries, &output, false, &control, &|event| {
            seen.borrow_mut().push(event);
            if event == MdProgress::FileCompleted(1, 2) {
                control.cancel();
            }
            Ok(())
        })
        .expect_err("完成后取消必须中止任务");
        assert!(
            format!("{error:#}").contains("取消"),
            "取消必须以明确错误收尾：{error:#}"
        );
        assert!(
            !seen.borrow().contains(&MdProgress::FileStarted(2, 2)),
            "取消后不得开始下一个输入：{:?}",
            seen.borrow()
        );
        assert!(!output.exists(), "中止的合并不得留下半成品输出");
    }

    // 覆盖 U-12（合并的文件内循环必须有停止检查点：暂停 rendezvous 证明任务停在
    // 大文件中途；取消后及时以错误收尾、不开始下一输入、不留临时文件）
    #[test]
    fn merge_stop_is_timely_inside_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // 40MB / 两千万行：行级处理足够慢，无文件内检查点时整个文件的合并远超等待窗口
        let mut big = String::with_capacity(40 * 1024 * 1024 + 16);
        while big.len() < 40 * 1024 * 1024 {
            big.push_str("x\n");
        }
        fs::write(root.join("big.md"), big).unwrap();
        write(&root.join("small.md"), "# 小\n");
        let output = root.join("merged.md");
        let entries = scan_markdown(root, true, Some(&output)).unwrap();
        assert_eq!(entries.len(), 2);
        let control = Arc::new(Control::default());
        let events: Arc<Mutex<Vec<MdProgress>>> = Arc::new(Mutex::new(Vec::new()));
        let worker_control = Arc::clone(&control);
        let worker_events = Arc::clone(&events);
        let (result_tx, result_rx) = mpsc::channel();
        let worker_output = output.clone();
        std::thread::spawn(move || {
            let result = merge_markdown_with_events(
                &entries,
                &worker_output,
                true,
                &worker_control,
                &|event| {
                    worker_events.lock().unwrap().push(event);
                    if event == MdProgress::FileStarted(1, 2) {
                        // 合流点：文件内检查点应让任务立刻停在大文件开头附近
                        worker_control.pause(true);
                    }
                    Ok(())
                },
            );
            let _ = result_tx.send(result.map(|_| ()).map_err(|error| format!("{error:#}")));
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let started = events
                .lock()
                .unwrap()
                .contains(&MdProgress::FileStarted(1, 2));
            if started {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "等待 FileStarted(1,2) 超时"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // 等待窗口内任务必须停在文件内（未完成第 1 个文件）
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !events
                .lock()
                .unwrap()
                .contains(&MdProgress::FileCompleted(1, 2)),
            "暂停期间大文件不得被处理完成（文件内检查点缺失）"
        );
        control.cancel();
        let result = result_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("取消后任务必须及时收尾");
        let error = result.expect_err("取消中的合并必须失败收尾");
        assert!(error.contains("取消"), "取消必须以明确错误收尾：{error}");
        assert!(
            !events
                .lock()
                .unwrap()
                .contains(&MdProgress::FileStarted(2, 2)),
            "取消后不得开始下一个输入"
        );
        assert!(!output.exists(), "中止的合并不得留下半成品输出");
        for entry in fs::read_dir(root).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                !name.starts_with(".jchtools-md-out-"),
                "中止收尾不得残留临时文件：{name}"
            );
        }
    }

    // 覆盖 U-03/U-12（拆分的事件序列：Started → Completed 成对出现，完成计数由 Completed 驱动）
    #[test]
    fn split_events_started_before_completed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("doc.md"), "0123456789");
        let plan = plan_splits(&root.join("doc.md"), 5).unwrap();
        assert_eq!(plan.bounds.len(), 2);
        let out_dir = root.join("parts");
        let completed = std::cell::Cell::new(0usize);
        let seen = std::cell::RefCell::new(Vec::new());
        run_split_with_events(
            &root.join("doc.md"),
            &plan,
            &out_dir,
            false,
            &Control::default(),
            &|event| {
                match event {
                    MdProgress::FileStarted(index, total) => {
                        assert_eq!(total, 2);
                        assert_eq!(
                            index,
                            completed.get() + 1,
                            "FileStarted 发出时完成计数不得已包含当前分片"
                        );
                    }
                    MdProgress::FileCompleted(index, _total) => {
                        completed.set(index);
                    }
                }
                seen.borrow_mut().push(event);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            seen.into_inner(),
            vec![
                MdProgress::FileStarted(1, 2),
                MdProgress::FileCompleted(1, 2),
                MdProgress::FileStarted(2, 2),
                MdProgress::FileCompleted(2, 2),
            ],
            "每个分片必须先 Started 后 Completed"
        );
        assert_eq!(completed.into_inner(), 2);
    }

    // 覆盖 U-12（拆分：第 1 片完成后取消，第 2 片不得开始）
    #[test]
    fn split_stop_between_parts_never_starts_next() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("doc.md"), "0123456789");
        let plan = plan_splits(&root.join("doc.md"), 5).unwrap();
        assert_eq!(plan.bounds.len(), 2);
        let out_dir = root.join("parts");
        let control = Control::default();
        let seen = std::cell::RefCell::new(Vec::new());
        let error = run_split_with_events(
            &root.join("doc.md"),
            &plan,
            &out_dir,
            false,
            &control,
            &|event| {
                seen.borrow_mut().push(event);
                if event == MdProgress::FileCompleted(1, 2) {
                    control.cancel();
                }
                Ok(())
            },
        )
        .expect_err("完成后取消必须中止任务");
        assert!(
            format!("{error:#}").contains("取消"),
            "取消必须以明确错误收尾：{error:#}"
        );
        assert!(
            !seen.borrow().contains(&MdProgress::FileStarted(2, 2)),
            "取消后不得开始下一分片：{:?}",
            seen.borrow()
        );
        assert!(
            !out_dir.join("doc_002.md").exists(),
            "取消后不得写出后续分片"
        );
    }

    // 覆盖 U-12（拆分的分片复制循环必须有停止检查点：暂停 rendezvous 证明任务停在
    // 复制中途；取消后及时以错误收尾）
    #[test]
    fn split_stop_is_timely_inside_part_copy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let big = vec![b'a'; 40 * 1024 * 1024];
        fs::write(root.join("big.md"), &big).unwrap();
        let plan = plan_splits(&root.join("big.md"), 64 * 1024 * 1024).unwrap();
        assert_eq!(plan.bounds.len(), 1, "40MB 在 64MB 限制下应为单片");
        let out_dir = root.join("parts");
        let control = Arc::new(Control::default());
        let events: Arc<Mutex<Vec<MdProgress>>> = Arc::new(Mutex::new(Vec::new()));
        let worker_control = Arc::clone(&control);
        let worker_events = Arc::clone(&events);
        let (result_tx, result_rx) = mpsc::channel();
        let input = root.join("big.md");
        std::thread::spawn(move || {
            let result =
                run_split_with_events(&input, &plan, &out_dir, false, &worker_control, &|event| {
                    worker_events.lock().unwrap().push(event);
                    if event == MdProgress::FileStarted(1, 1) {
                        // 合流点：分片复制循环内的检查点应让任务立刻停在开头
                        worker_control.pause(true);
                    }
                    Ok(())
                });
            let _ = result_tx.send(result.map(|_| ()).map_err(|error| format!("{error:#}")));
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let started = events
                .lock()
                .unwrap()
                .contains(&MdProgress::FileStarted(1, 1));
            if started {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "等待 FileStarted(1,1) 超时"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !events
                .lock()
                .unwrap()
                .contains(&MdProgress::FileCompleted(1, 1)),
            "暂停期间单片复制不得完成（分片复制循环内检查点缺失）"
        );
        control.cancel();
        let result = result_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("取消后任务必须及时收尾");
        let error = result.expect_err("取消中的拆分必须失败收尾");
        assert!(error.contains("取消"), "取消必须以明确错误收尾：{error}");
    }
}
