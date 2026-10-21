//! 标题下移状态机（M-05/M-06）与逐行字节级判定辅助。
use anyhow::{Context as _, Result};
use std::io::Write;

/// 标题下移状态机（M-05/M-06）：逐行改写，fenced code block 内的行原样输出。
#[derive(Default)]
pub(super) struct HeadingShift {
    /// 打开的 fenced code block：（fence 字符，开始长度）
    fence: Option<(u8, usize)>,
    /// 暂缓输出的普通文本行（可能是 Setext 标题文本）：（行体， 行尾字节）
    pending: Option<(Vec<u8>, Vec<u8>)>,
    /// 已读入的输入字节数（空文件判定）
    pub(super) consumed: usize,
    /// 是否写出过任何字节
    pub(super) wrote_any: bool,
    /// 已写内容是否以换行结束
    pub(super) ended_newline: bool,
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

/// 当前行是否为闭合 fence：与开始 fence 同字符、长度不小于开始长度、之后仅空格或制表符。
fn is_fence_close(body: &[u8], fence: (u8, usize)) -> bool {
    let indent = leading_spaces(body);
    if indent > 3 {
        return false;
    }
    let rest = &body[indent..];
    let end = rest
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t'))
        .map_or(0, |index| index + 1);
    let trimmed = &rest[..end];
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
    let indent = leading_spaces(body);
    if indent >= 4 {
        return true; // 缩进代码块
    }
    let rest = trim_trailing_spaces(&body[indent..]);
    let Some(&first) = rest.first() else {
        return true; // 空行
    };
    // M-04/M-05：独立 thematic break 不是文本行，不能暂存后被下一行 === / --- 改成标题。
    if matches!(first, b'-' | b'*' | b'_') {
        let mut markers = 0;
        let mut only_markers = true;
        for &byte in rest {
            if byte == first {
                markers += 1;
            } else if byte != b' ' && byte != b'\t' {
                only_markers = false;
                break;
            }
        }
        if only_markers && markers >= 3 {
            return true;
        }
    }
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
        b'0'..=b'9' => {
            let Some(marker_end) = rest.iter().position(|b| !b.is_ascii_digit()) else {
                return false;
            };
            marker_end <= 9
                && rest
                    .get(marker_end)
                    .is_some_and(|b| *b == b'.' || *b == b')')
                && rest.get(marker_end + 1).is_none_or(u8::is_ascii_whitespace)
        }
        _ => false,
    }
}

impl HeadingShift {
    pub(super) fn feed(&mut self, raw: &[u8], out: &mut impl Write) -> Result<()> {
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
    pub(super) fn finish(&mut self, out: &mut impl Write) -> Result<()> {
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
