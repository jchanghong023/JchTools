/// 沿既有围栏/行内代码/链接语义扫描真实图片目标；只记录区间，不重写正文。
/// alt text、普通链接的目标/title 及已带媒体目录的目标仍按原解析规则处理。
pub(super) fn scan_image_links(text: &str, visit: &mut impl FnMut(std::ops::Range<usize>)) {
    let mut fence: Option<(char, usize)> = None;
    let mut containers = Vec::new();
    let mut inline_code: Option<usize> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let remaining = &text[offset..];
        let line_start = offset;
        offset += line.len();
        let body = line.strip_suffix('\n').unwrap_or(line);
        let body = body.strip_suffix('\r').unwrap_or(body);
        // 只剥离当前容器实际占用的前缀；围栏的三空格限制仍相对于容器内容。
        // 空白列表行保留容器，退出列表/引用则同时结束其中未显式关闭的围栏。
        let (mut content_start, matched) = continue_containers(body, &containers);
        if matched != containers.len() {
            containers.truncate(matched);
            fence = None;
            inline_code = None;
        }
        if let Some((open_char, open_len)) = fence {
            if closes_fence(&body[content_start..], open_char, open_len) {
                fence = None;
            }
            continue;
        }
        while let Some((container, consumed)) = opening_container(&body[content_start..]) {
            containers.push(container);
            content_start += consumed;
        }
        if let Some((ch, run, _indent)) = opening_fence(&body[content_start..]) {
            inline_code = None;
            fence = Some((ch, run));
            continue;
        }
        scan_image_links_in_line(body, remaining, line_start, &mut inline_code, visit);
    }
}

/// 容器只参与围栏定位，不参与正文重建；列表宽度相对于上一层容器。
#[derive(Clone, Copy)]
enum MarkdownContainer {
    List(usize),
    Quote,
}

fn continue_containers(line: &str, containers: &[MarkdownContainer]) -> (usize, usize) {
    let mut offset = 0;
    for (matched, container) in containers.iter().enumerate() {
        let remaining = &line[offset..];
        let consumed = match container {
            MarkdownContainer::List(width) => {
                let spaces = leading_spaces(remaining);
                if spaces >= *width {
                    *width
                } else if remaining.trim().is_empty() {
                    // 列表中的空行不要求补齐内容缩进，也不会关闭代码围栏。
                    remaining.len()
                } else {
                    return (offset, matched);
                }
            }
            MarkdownContainer::Quote => {
                let Some(consumed) = quote_prefix(remaining) else {
                    return (offset, matched);
                };
                consumed
            }
        };
        offset += consumed;
    }
    (offset, containers.len())
}

fn quote_prefix(line: &str) -> Option<usize> {
    let indent = leading_spaces(line);
    if indent > 3 || line.as_bytes().get(indent) != Some(&b'>') {
        return None;
    }
    Some(indent + 1 + usize::from(line.as_bytes().get(indent + 1) == Some(&b' ')))
}

fn opening_container(line: &str) -> Option<(MarkdownContainer, usize)> {
    if let Some(consumed) = quote_prefix(line) {
        return Some((MarkdownContainer::Quote, consumed));
    }
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let bytes = line.as_bytes();
    // 分隔线优先于列表，不能从 `- - -` / `***` 虚构后续围栏的容器。
    if let Some(marker @ (b'-' | b'*')) = bytes.get(indent) {
        let mut count = 0;
        let thematic = bytes[indent..].iter().all(|byte| {
            if byte == marker {
                count += 1;
                true
            } else {
                *byte == b' ' || *byte == b'\t'
            }
        });
        if thematic && count >= 3 {
            return None;
        }
    }
    let marker_end = match bytes.get(indent)? {
        b'-' | b'+' | b'*' => indent + 1,
        b'0'..=b'9' => {
            let digits = bytes[indent..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digits > 9 || !matches!(bytes.get(indent + digits), Some(b'.' | b')')) {
                return None;
            }
            indent + digits + 1
        }
        _ => return None,
    };
    let rest = &line[marker_end..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let padding = if rest.trim().is_empty() {
        1
    } else {
        leading_spaces(rest)
    };
    // CommonMark：标记后 1–4 空格才是内容前缀；更多空格只消费一个，
    // 避免把真正的四空格缩进代码误认成新围栏。空列表项仍保留下一行内容宽度。
    let width = marker_end
        + if (1..=4).contains(&padding) {
            padding
        } else {
            1
        };
    Some((MarkdownContainer::List(width), width.min(line.len())))
}

fn scan_image_links_in_line(
    line: &str,
    remaining: &str,
    offset: usize,
    inline_code: &mut Option<usize>,
    visit: &mut impl FnMut(std::ops::Range<usize>),
) {
    let mut i = 0;
    let bytes = line.as_bytes();
    while i < bytes.len() {
        // CommonMark 正文中的反斜杠转义只作用于 ASCII 标点；代码跨度内
        // 则是普通字符。成对消费也保留 \\ 后的真实图片/反引号语义。
        if inline_code.is_none()
            && bytes[i] == b'\\'
            && bytes.get(i + 1).is_some_and(u8::is_ascii_punctuation)
        {
            i += 2;
            continue;
        }
        if bytes[i] == b'`' {
            let run = bytes[i..].iter().take_while(|&&byte| byte == b'`').count();
            if let Some(open_len) = *inline_code {
                if run == open_len {
                    *inline_code = None;
                }
            } else {
                // 没有同长度关闭串时，反引号只是正文；不能吞掉真实图片。
                if closing_backticks(&remaining[i + run..], run).is_some() {
                    *inline_code = Some(run);
                }
            }
            i += run;
            continue;
        }
        if inline_code.is_none() && line[i..].starts_with("![") {
            let Some(close_alt) = image_label_end(line, i + 2) else {
                i += 1;
                continue;
            };
            if bytes.get(close_alt + 1) != Some(&b'(') {
                i += 1;
                continue;
            }
            let destination_start = close_alt + 2;
            let Some(close_link) = image_link_end(line, destination_start) else {
                i += 1;
                continue;
            };
            let destination = line[destination_start..close_link].trim_start();
            let leading = line[destination_start..close_link].len() - destination.len();
            let token_end = destination
                .char_indices()
                .find(|(_, character)| character.is_whitespace())
                .map_or(destination.len(), |(index, _)| index);
            let token = &destination[..token_end];
            let (token, bracketed) = match token.strip_prefix('<').and_then(|s| s.strip_suffix('>'))
            {
                Some(token) => (token, true),
                None => (token, false),
            };
            let token_start = offset + destination_start + leading + usize::from(bracketed);
            visit(token_start..token_start + token.len());
            // alt/title 的字面文本不参与引用扫描或行内代码状态。
            i = close_link + 1;
            continue;
        }
        if inline_code.is_none() && bytes[i] == b'[' {
            if let Some(close_label) = image_label_end(line, i + 1) {
                if bytes.get(close_label + 1) == Some(&b'(') {
                    if let Some(close_link) = ordinary_link_end(line, close_label + 2) {
                        // label 使用正文共享的代码状态；跨行代码优先于目标/title。
                        let label = &line[i + 1..close_label];
                        if label.contains("![") || label.contains('`') {
                            scan_image_links_in_line(
                                label,
                                &remaining[i + 1..],
                                offset + i + 1,
                                inline_code,
                                visit,
                            );
                        }
                        if inline_code.is_some() {
                            i = close_label;
                            continue;
                        }
                        // 只有合法普通链接的目标及 title 才不属于正文。
                        i = close_link + 1;
                        continue;
                    }
                }
            }
        }
        let Some(ch) = line[i..].chars().next() else {
            break;
        };
        i += ch.len_utf8();
    }
}

/// 平衡括号只确定候选边界；普通链接还须包含合法目标和可选 title，
/// 否则其中的真实图片仍是正文。括号 title 沿用图片解析的嵌套规则。
fn ordinary_link_end(line: &str, start: usize) -> Option<usize> {
    let end = image_link_end(line, start)?;
    let content = line[start..end].trim();
    let bytes = content.as_bytes();
    // 目标可为空；开头空白后的 title 仍须按字面量保留。
    let title_only = line.as_bytes()[start].is_ascii_whitespace()
        && bytes
            .first()
            .is_some_and(|&byte| matches!(byte, b'"' | b'\'' | b'('));
    let mut i = 0;
    let mut depth = 0usize;
    let angle = bytes.first() == Some(&b'<');
    if angle {
        i += 1;
    }
    while !title_only && i < bytes.len() {
        let byte = bytes[i];
        if byte == b'\\' && bytes.get(i + 1).is_some_and(u8::is_ascii_punctuation) {
            i += 2;
            continue;
        }
        if angle {
            if byte == b'>' {
                i += 1;
                break;
            }
            if byte == b'<' || byte == b'\r' || byte == b'\n' {
                return None;
            }
        } else {
            match byte {
                b'(' => depth += 1,
                b')' => depth = depth.checked_sub(1)?,
                byte if byte.is_ascii_whitespace() => break,
                byte if byte.is_ascii_control() || byte == b'<' || byte == b'>' => {
                    return None;
                }
                _ => {}
            }
        }
        i += 1;
    }
    if depth != 0 || (angle && bytes.get(i - 1) != Some(&b'>')) {
        return None;
    }
    if i == bytes.len() {
        return Some(end);
    }
    if !title_only && !bytes[i].is_ascii_whitespace() {
        return None;
    }
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    let Some(&delimiter) = bytes.get(i) else {
        return Some(end);
    };
    let title_end = match delimiter {
        b'(' => image_link_end(content, i + 1)?,
        b'"' | b'\'' => {
            i += 1;
            loop {
                let &byte = bytes.get(i)?;
                if byte == b'\\' && bytes.get(i + 1).is_some_and(u8::is_ascii_punctuation) {
                    i += 2;
                } else if byte == delimiter {
                    break i;
                } else {
                    i += 1;
                }
            }
        }
        _ => return None,
    };
    content[title_end + 1..].trim().is_empty().then_some(end)
}

/// 图片目标可含平衡括号，title 可含字面括号；不能在 title 内提前结束链接，
/// 否则其余 title 文本会被当作正文图片改写。反斜杠转义与尖括号目标保持原样。
fn image_link_end(line: &str, start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut depth = 1usize;
    let mut quote = None;
    let mut angle = false;
    let mut i = start;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'\\' && bytes.get(i + 1).is_some_and(u8::is_ascii_punctuation) {
            i += 2;
            continue;
        }
        if let Some(delimiter) = quote {
            if byte == delimiter {
                quote = None;
            }
        } else if angle {
            if byte == b'>' {
                angle = false;
            }
        } else {
            match byte {
                b'<' if line[start..i].trim().is_empty() => angle = true,
                b'"' | b'\'' if depth == 1 && i > start && bytes[i - 1].is_ascii_whitespace() => {
                    quote = Some(byte);
                }
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// 查找同长度的关闭反引号串；空段落或新围栏是代码跨度的块边界。
fn closing_backticks(text: &str, run: usize) -> Option<usize> {
    let mut offset = 0;
    for (line_index, line) in text.split_inclusive('\n').enumerate() {
        if line_index != 0 && (line.trim().is_empty() || opening_fence(line).is_some()) {
            return None;
        }
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'`' {
                i += 1;
                continue;
            }
            let length = bytes[i..].iter().take_while(|&&byte| byte == b'`').count();
            if length == run {
                return Some(offset + i);
            }
            i += length;
        }
        offset += line.len();
    }
    None
}

/// 图片 label 可含嵌套方括号、转义及代码，不能把 alt 内的 `](` 当作目标开头。
fn image_label_end(line: &str, start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut depth = 1usize;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if bytes.get(i + 1).is_some_and(u8::is_ascii_punctuation) => i += 2,
            b'`' => {
                let run = bytes[i..].iter().take_while(|&&byte| byte == b'`').count();
                i += run;
                if let Some(close) = closing_backticks(&line[i..], run) {
                    i += close + run;
                }
            }
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// 识别围栏开启行：缩进 ≤3 个空格、行首连续 ≥3 个反引号或波浪线。
/// 开启行之后的其余内容按语言标签处理；反引号围栏的标签不能含反引号。
fn opening_fence(line: &str) -> Option<(char, usize, usize)> {
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let body = &line[indent..];
    let ch = body.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let run = body
        .chars()
        .take_while(|&candidate| candidate == ch)
        .count();
    (run >= 3 && (ch != '`' || !body[run..].contains('`'))).then_some((ch, run, indent))
}

/// 判定关闭行：同字符、连续长度不小于开启行、其后只剩空白（语言标签不算关闭）。
fn closes_fence(line: &str, open_char: char, open_len: usize) -> bool {
    let indent = leading_spaces(line);
    if indent > 3 {
        return false;
    }
    let body = &line[indent..];
    let run = body
        .chars()
        .take_while(|&candidate| candidate == open_char)
        .count();
    // 围栏字符是单字节 ASCII，run 个字符对应 run 个字节的边界。
    run >= open_len && body[run..].trim().is_empty()
}

fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}
