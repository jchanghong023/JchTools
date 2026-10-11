//! AH-08/AH-09：应用 SQLite 配置与 Windows 参数边界。
use super::{ServiceConfig, ServiceError, ServiceErrorKind};
use rusqlite::{Connection, OptionalExtension};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

pub(crate) const CONTROL_FRAME_LIMIT: usize = 1024 * 1024;
// 状态同时包含 saved/running 配置，还需为控制响应封装及错误信息保留余量。
const CONFIG_JSON_LIMIT: usize = CONTROL_FRAME_LIMIT / 4;

struct ConfigSizeBudget {
    remaining: usize,
}

impl Write for ConfigSizeBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(std::io::Error::other(format!(
                "配置：JSON 编码大小不能超过 {CONFIG_JSON_LIMIT} 字节"
            )));
        }
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> ServiceError {
    ServiceError::new(ServiceErrorKind::InvalidConfig, message)
}
fn io(message: impl Into<String>) -> ServiceError {
    ServiceError::new(ServiceErrorKind::Io, message)
}
fn open() -> Result<Connection, ServiceError> {
    let root = crate::xberg_settings::state_dir().map_err(io)?;
    std::fs::create_dir_all(&root).map_err(|e| io(format!("创建应用配置目录失败：{e}")))?;
    let db = Connection::open(root.join("config.sqlite3"))
        .map_err(|e| io(format!("打开应用配置 SQLite 失败：{e}")))?;
    db.busy_timeout(Duration::from_secs(5))
        .map_err(|e| io(e.to_string()))?;
    db.execute_batch(include_str!("../app_settings.sql"))
        .map_err(|e| io(format!("初始化应用配置 SQLite 失败：{e}")))?;
    Ok(db)
}
pub fn load_config() -> Result<Option<ServiceConfig>, ServiceError> {
    crate::acp_api::diagnostics::call("acp_settings", "load_config", || {
        let value: Option<String> = open()?
            .query_row(
                "SELECT value FROM app_settings WHERE key='acp_http_config'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| io(format!("读取模型服务配置失败：{e}")))?;
        value
            .map(|text| {
                let config: ServiceConfig = serde_json::from_str(&text)
                    .map_err(|e| invalid(format!("已保存的模型服务配置损坏：{e}")))?;
                validate_config(&config)?;
                Ok(config)
            })
            .transpose()
    })
}
pub fn save_config(config: &ServiceConfig) -> Result<(), ServiceError> {
    crate::acp_api::diagnostics::call("acp_settings", "save_config", || {
        validate_config(config)?;
        let value = serde_json::to_string(config).map_err(|e| invalid(e.to_string()))?;
        let mut db = open()?;
        let tx = db
            .transaction()
            .map_err(|e| io(format!("开始配置事务失败：{e}")))?;
        tx.execute("INSERT INTO app_settings(key,value) VALUES('acp_http_config',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [value])
    .map_err(|e| io(format!("保存模型服务配置失败：{e}")))?;
        tx.commit()
            .map_err(|e| io(format!("提交模型服务配置失败：{e}")))
    })
}
pub fn workspace_dir() -> Result<PathBuf, ServiceError> {
    crate::acp_api::diagnostics::call("acp_settings", "workspace_dir", || {
        let path = crate::xberg_settings::state_dir()
            .map_err(io)?
            .join("acp-workspace");
        std::fs::create_dir_all(&path).map_err(|e| io(format!("创建 ACP 工作目录失败：{e}")))?;
        let canonical =
            std::fs::canonicalize(path).map_err(|e| io(format!("解析 ACP 工作目录失败：{e}")))?;
        #[cfg(windows)]
        {
            use std::path::{Component, Prefix};
            // 原生 Agent 将 cwd 用于会话目录名；Rust 的 namespace 前缀不应跨协议泄漏。
            // 仅转换有等价普通 Windows 表示的盘符/UNC 路径，其他设备路径保持原样。
            let replacement = match canonical.components().next() {
                Some(Component::Prefix(prefix)) => match prefix.kind() {
                    Prefix::VerbatimDisk(_) => Some((4, "")),
                    Prefix::VerbatimUNC(_, _) => Some((8, r"\\")),
                    _ => None,
                },
                _ => None,
            };
            if let Some((end, prefix)) = replacement {
                return match canonical.into_os_string().into_string() {
                    Ok(mut text) => {
                        text.replace_range(..end, prefix);
                        Ok(text.into())
                    }
                    Err(path) => Ok(path.into()),
                };
            }
        }
        Ok(canonical)
    })
}
pub fn parse_port(text: &str) -> Result<u16, ServiceError> {
    crate::acp_api::diagnostics::call("acp_settings", "parse_port", || {
        let text = text.trim();
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid("端口：请输入 1～65535 的整数"));
        }
        text.parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| invalid("端口：请输入 1～65535 的整数"))
    })
}
pub fn validate_config(config: &ServiceConfig) -> Result<(), ServiceError> {
    crate::acp_api::diagnostics::call("acp_settings", "validate_config", || {
        if config.executable.trim().is_empty() {
            return Err(invalid("启动程序：请选择或输入 ACP Agent 程序"));
        }
        if config.executable.contains(['\0', '\r', '\n', '"']) {
            return Err(invalid("启动程序：请单独输入程序路径，不包含引号或换行"));
        }
        if config.arguments.iter().any(|arg| arg.contains('\0')) {
            return Err(invalid("参数：不能包含空字符"));
        }
        if config.port == 0 {
            return Err(invalid("端口：请输入 1～65535 的整数"));
        }
        // 计算实际 JSON 编码字节，包含控制字符/引号转义及 UTF-8，不分配编码缓冲区。
        serde_json::to_writer(
            ConfigSizeBudget {
                remaining: CONFIG_JSON_LIMIT,
            },
            config,
        )
        .map_err(|e| invalid(e.to_string()))
    })
}
/// Windows CRT 引号规则：反斜杠仅在双引号之前具有转义含义。
pub fn parse_arguments(text: &str) -> Result<Vec<String>, ServiceError> {
    crate::acp_api::diagnostics::call("acp_settings", "parse_arguments", || {
        if text.contains('\0') {
            return Err(invalid("参数：不能包含空字符"));
        }
        let mut chars = text.chars().peekable();
        let mut result = Vec::new();
        while chars.peek().is_some() {
            while chars
                .peek()
                .is_some_and(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
            {
                chars.next();
            }
            if chars.peek().is_none() {
                break;
            }
            let mut quoted = false;
            let mut argument = String::new();
            while let Some(&ch) = chars.peek() {
                if !quoted && matches!(ch, ' ' | '\t' | '\r' | '\n') {
                    break;
                }
                let mut slashes = 0;
                while chars.peek() == Some(&'\\') {
                    chars.next();
                    slashes += 1;
                }
                if chars.peek() == Some(&'"') {
                    argument.extend(std::iter::repeat_n('\\', slashes / 2));
                    chars.next();
                    if slashes % 2 != 0 {
                        argument.push('"');
                    } else if quoted && chars.peek() == Some(&'"') {
                        chars.next();
                        argument.push('"');
                    } else {
                        quoted = !quoted;
                    }
                } else {
                    argument.extend(std::iter::repeat_n('\\', slashes));
                    if !quoted
                        && chars
                            .peek()
                            .is_some_and(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
                    {
                        break;
                    }
                    if let Some(ch) = chars.next() {
                        argument.push(ch);
                    }
                }
            }
            if quoted {
                return Err(invalid("参数：双引号未闭合"));
            }
            result.push(argument);
        }
        Ok(result)
    })
}
pub fn format_arguments(arguments: &[String]) -> String {
    arguments
        .iter()
        .map(|argument| {
            if !argument.is_empty() && !argument.contains([' ', '\t', '\r', '\n', '"']) {
                return argument.clone();
            }
            let mut output = String::from("\"");
            let mut slashes = 0;
            for ch in argument.chars() {
                if ch == '\\' {
                    slashes += 1;
                    continue;
                }
                output.extend(std::iter::repeat_n(
                    '\\',
                    if ch == '"' { slashes * 2 + 1 } else { slashes },
                ));
                slashes = 0;
                output.push(ch);
            }
            output.extend(std::iter::repeat_n('\\', slashes * 2));
            output.push('"');
            output
        })
        .collect::<Vec<_>>()
        .join(" ")
}
