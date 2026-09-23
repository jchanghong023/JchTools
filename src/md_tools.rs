//! MD 整理核心逻辑（合同 M 分区）：「合并 MD」与「拆分 MD」。
//! 独立于 GUI：合并按 M-03 排序、M-04/M-05 标题下移、M-06 代码块保护，逐文件逐行流式处理；
//! 拆分按 M-09/M-10 在 UTF-8 安全边界分片，分片按编号顺序二进制拼接可无损还原原文件。
//! 所有原始文件只读；输出一律写入用户指定的新文件（M-02/M-08/M-11）。

use anyhow::{bail, Context as _, Result};
use std::{
    fs::File,
    io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
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

/// Windows 口径的同一文件判定：大小写与分隔符（`/` 与 `\`）不敏感的路径文本比较。
/// 仅用于输出文件排除这一用途；排序仍用原始路径，不受本归一化影响。
fn same_file_path(a: &Path, b: &Path) -> bool {
    fn normalized(path: &Path) -> String {
        path.to_string_lossy().to_lowercase().replace('\\', "/")
    }
    normalized(a) == normalized(b)
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
    let mut rows: Vec<(SystemTime, MergeEntry)> = Vec::new();
    let max_depth = if recursive { usize::MAX } else { 1 };
    for item in WalkDir::new(root)
        .follow_links(false)
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
    rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| natural_cmp(&a.1.rel, &b.1.rel)));
    Ok(rows.into_iter().map(|(_, entry)| entry).collect())
}

/// 相对路径自然排序（M-03 第二排序键）：数字段按数值比较（前导零不参与），
/// 非数字段按字节逐字符比较（UTF-8 字节序与 Unicode 码点序一致）。
/// 不同路径互不相等，因此排序结果确定、稳定、可重复。
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
        b'-' | b'*' | b'+' | b'>' => true,
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

/// 按 M-04~M-07 把 `entries` 流式合并写入 `output`。
/// `overwrite=false` 且输出已存在时报错（调用方必须先完成覆盖确认）；
/// 每个文件处理前回调 `on_file(当前序号, 总数)`，并在文件边界响应取消与进度。
pub fn merge_markdown(
    entries: &[MergeEntry],
    output: &Path,
    overwrite: bool,
    control: &Control,
    on_file: &dyn Fn(usize, usize) -> Result<()>,
) -> Result<MergeStats> {
    if output.exists() && !overwrite {
        bail!(
            "输出文件已存在：{}（未确认覆盖，拒绝写入）",
            output.display()
        );
    }
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建输出目录失败：{}", parent.display()))?;
        }
    }
    let target =
        File::create(output).with_context(|| format!("创建输出文件失败：{}", output.display()))?;
    let mut out = BufWriter::with_capacity(512 * 1024, target);
    let mut prev_empty = true;
    for (index, entry) in entries.iter().enumerate() {
        control.checkpoint()?;
        on_file(index + 1, entries.len())?;
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
        loop {
            let mut line: Vec<u8> = Vec::with_capacity(1024);
            let read = reader
                .read_until(b'\n', &mut line)
                .with_context(|| format!("读取输入文件失败：{}", entry.path.display()))?;
            if read == 0 {
                break;
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
    }
    out.flush()
        .with_context(|| format!("写输出文件失败（磁盘可能已满）：{}", output.display()))?;
    Ok(MergeStats {
        files: entries.len(),
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

/// 按计划写出全部分片，返回写入的总字节数。
/// `overwrite=false` 且任一目标已存在时报错（调用方必须先完成冲突确认，M-11）。
/// 逐片分块复制，不在内存中持有整个文件（M-11 大文件实现）。
pub fn run_split(
    input: &Path,
    plan: &SplitPlan,
    out_dir: &Path,
    overwrite: bool,
    control: &Control,
    on_part: &dyn Fn(usize, usize) -> Result<()>,
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
        on_part(index + 1, plan.bounds.len())?;
        let path = out_dir.join(&names[index]);
        let target =
            File::create(&path).with_context(|| format!("创建分片失败：{}", path.display()))?;
        let mut out = BufWriter::with_capacity(256 * 1024, target);
        reader
            .seek(SeekFrom::Start(start))
            .with_context(|| format!("定位输入失败：{}", input.display()))?;
        let mut remain = end.saturating_sub(start);
        while remain > 0 {
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
    }
    Ok(written)
}
