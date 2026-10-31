use super::*;
use std::fmt::{self, Write as _};

/// 仅保留 URL 的 scheme/host/path，删除 userinfo、query、fragment。
/// 无法识别的 URL 不原样返回；这不是任意外部正文的安全摘要器。
pub fn safe_url(value: &str) -> String {
    let Some((scheme, rest)) = value.split_once("://") else {
        return "[invalid_url]".into();
    };
    if !["http", "https", "socks", "socks5", "ftp", "ssh"]
        .iter()
        .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
        || value.chars().any(char::is_control)
    {
        return "[invalid_url]".into();
    }
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let clean = &rest[..end];
    let authority_end = clean.find('/').unwrap_or(clean.len());
    let authority = &clean[..authority_end];
    let host = authority.rsplit('@').next().unwrap_or_default();
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return "[invalid_url]".into();
    }
    let mut output = BoundedText {
        value: String::new(),
        truncated: false,
    };
    let _ = write!(output, "{scheme}://{host}{}", &clean[authority_end..]);
    if output.truncated {
        output.value.push_str("[truncated]");
    }
    bounded(&output.value)
}

/// 去敏已知凭据/URL并有界；模型、OCR错误正文不得交给本函数冒称安全。
pub fn safe_error(value: &str) -> String {
    // Never cut an unparsed URL before its @/query delimiter: a long userinfo
    // could otherwise be mistaken for a hostname and expose its prefix.
    if value.len() > TEXT_LIMIT {
        return "[truncated]".into();
    }
    let mut result = String::with_capacity(value.len());
    let mut at = 0;
    while at < value.len() {
        let remaining = &value[at..];
        if let Some(scheme_end) = remaining.find("://").filter(|end| *end <= 8) {
            let scheme = &remaining[..scheme_end];
            if scheme
                .chars()
                .all(|character| character.is_ascii_alphabetic())
            {
                // comma/apostrophe/parenthesis are valid userinfo characters.
                // Parse the whole authority before stripping credentials.
                let end = remaining
                    .find(char::is_whitespace)
                    .unwrap_or(remaining.len());
                result.push_str(&safe_url(&remaining[..end]));
                at += end;
                continue;
            }
        }
        let word_end = remaining
            .find(|character: char| {
                !character.is_ascii_alphanumeric() && character != '_' && character != '-'
            })
            .unwrap_or(remaining.len());
        if word_end > 0 {
            let word = &remaining[..word_end];
            // Debug 字符串会将 JSON 键引号写成反斜杠转义；同样识别其赋值分隔符。
            let tail = remaining[word_end..].trim_start_matches([' ', '\t', '"', '\'', '\\']);
            if (sensitive_name(word)
                && (tail.starts_with(['=', ':'])
                    || ["bearer", "authorization", "cookie", "set-cookie"]
                        .iter()
                        .any(|name| word.eq_ignore_ascii_case(name))))
                || ["sk-", "ghp_", "github_pat_"].iter().any(|prefix| {
                    word.get(..prefix.len())
                        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
                })
            {
                // 保守丢弃剩余诊断文本，避免引号、空白、多值 Cookie 绕过去敏。
                result.push_str("[redacted]");
                break;
            }
            result.push_str(word);
            at += word_end;
        } else {
            let Some(character) = remaining.chars().next() else {
                break;
            };
            result.push(character);
            at += character.len_utf8();
        }
    }
    // helper 本身也不返回物理控制字符；格式器随后转义反斜杠/引号。
    let mut escaped = String::with_capacity(result.len());
    for character in result.chars() {
        if character.is_control() {
            let _ = write!(escaped, "\\u{{{:x}}}", u32::from(character));
        } else {
            escaped.push(character);
        }
    }
    bounded(&escaped)
}

pub(super) fn sensitive_name(name: &str) -> bool {
    [
        "password",
        "passwd",
        "pwd",
        "authorization",
        "bearer",
        "token",
        "access_token",
        "refresh_token",
        "api_key",
        "apikey",
        "api-key",
        "secret",
        "client_secret",
        "private_key",
        "cookie",
        "set_cookie",
        "set-cookie",
        "session",
        "session_token",
        "credential",
        "credentials",
    ]
    .iter()
    .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
}

fn bounded(value: &str) -> String {
    if value.len() <= TEXT_LIMIT {
        return value.to_owned();
    }
    let mut end = TEXT_LIMIT - "[truncated]".len();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}[truncated]", &value[..end])
}

// Display/Debug may write arbitrary chunks; collect only a bounded prefix, without
// first allocating the complete rendered value. Numeric fields use Writer directly.
struct BoundedText {
    value: String,
    truncated: bool,
}

impl fmt::Write for BoundedText {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let available = TEXT_LIMIT.saturating_sub(self.value.len());
        let mut end = value.len().min(available);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.value.push_str(&value[..end]);
        self.truncated |= end < value.len();
        if self.truncated {
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

pub(super) fn render(value: &dyn fmt::Debug) -> String {
    let mut output = BoundedText {
        value: String::new(),
        truncated: false,
    };
    let _ = write!(output, "{value:?}");
    if output.truncated {
        return "[truncated]".into();
    }
    output.value
}
