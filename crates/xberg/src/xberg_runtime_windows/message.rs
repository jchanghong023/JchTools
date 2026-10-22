use super::MAX_MESSAGE_BYTES;
use serde_json::Value;
use std::io::{BufRead, Write};

const MAX_RESPONSE_ID_BYTES: usize = 4096;

pub(super) enum BoundedLine {
    Complete(Vec<u8>),
    Oversized { id: Option<String>, bytes: u64 },
}

#[derive(Clone, Copy)]
enum CapturedString {
    Key,
    IdValue,
}

/// 在排空超限行时只保留顶层 `id`，不依赖字段顺序，也不把嵌套对象中的
/// 同名字段误认为请求 ID。字符串按 JSON 转义规则扫描，正文永不进入日志。
// 各布尔状态对应 JSON 层级、转义、键和值的独立词法条件。
#[allow(clippy::struct_excessive_bools)]
pub(super) struct ResponseIdScanner {
    depth: usize,
    in_string: bool,
    escaped: bool,
    expecting_key: bool,
    key_complete: bool,
    expecting_value: bool,
    pending_id: bool,
    capture: Option<(CapturedString, Vec<u8>, bool)>,
    pub(super) id: Option<String>,
}

impl ResponseIdScanner {
    pub(super) fn new() -> Self {
        Self {
            depth: 0,
            in_string: false,
            escaped: false,
            expecting_key: false,
            key_complete: false,
            expecting_value: false,
            pending_id: false,
            capture: None,
            id: None,
        }
    }

    pub(super) fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.feed_byte(byte);
        }
    }

    fn feed_byte(&mut self, byte: u8) {
        if self.in_string {
            if self.escaped {
                self.push_captured(byte);
                self.escaped = false;
                return;
            }
            if byte == b'\\' {
                self.push_captured(byte);
                self.escaped = true;
                return;
            }
            if byte == b'"' {
                self.push_captured(byte);
                self.finish_string();
                self.in_string = false;
                return;
            }
            self.push_captured(byte);
            return;
        }

        match byte {
            b'{' => {
                if self.depth == 1 && self.expecting_value {
                    self.expecting_value = false;
                    self.pending_id = false;
                }
                self.depth = self.depth.saturating_add(1);
                if self.depth == 1 {
                    self.expecting_key = true;
                    self.key_complete = false;
                    self.expecting_value = false;
                    self.pending_id = false;
                }
            }
            b'}' => {
                self.depth = self.depth.saturating_sub(1);
                if self.depth == 0 {
                    self.expecting_key = false;
                    self.expecting_value = false;
                    self.pending_id = false;
                }
            }
            b'[' => {
                if self.depth == 1 && self.expecting_value {
                    self.expecting_value = false;
                    self.pending_id = false;
                }
                self.depth = self.depth.saturating_add(1);
            }
            b']' => self.depth = self.depth.saturating_sub(1),
            b',' if self.depth == 1 => {
                self.expecting_key = true;
                self.key_complete = false;
                self.expecting_value = false;
                self.pending_id = false;
            }
            b':' if self.depth == 1 && self.key_complete => {
                self.expecting_key = false;
                self.expecting_value = true;
            }
            b'"' if self.depth == 1 && self.expecting_key => {
                self.in_string = true;
                self.escaped = false;
                self.capture = Some((CapturedString::Key, vec![b'"'], false));
            }
            b'"' if self.depth == 1 && self.expecting_value && self.pending_id => {
                self.in_string = true;
                self.escaped = false;
                self.capture = Some((CapturedString::IdValue, vec![b'"'], false));
            }
            b'"' => {
                // 非 ID 字符串也必须按字符串跳过，正文中的括号和逗号
                // 不能改变 JSON 层级或被当作顶层字段。
                self.in_string = true;
                self.escaped = false;
                self.capture = None;
                if self.depth == 1 && self.expecting_value {
                    self.expecting_value = false;
                    self.pending_id = false;
                }
            }
            byte if self.depth == 1 && self.expecting_value && !byte.is_ascii_whitespace() => {
                self.expecting_value = false;
                self.pending_id = false;
            }
            _ => {}
        }
    }

    fn push_captured(&mut self, byte: u8) {
        if let Some((_, bytes, overflowed)) = self.capture.as_mut() {
            if bytes.len() < MAX_RESPONSE_ID_BYTES {
                bytes.push(byte);
            } else {
                *overflowed = true;
            }
        }
    }

    fn finish_string(&mut self) {
        let Some((kind, bytes, overflowed)) = self.capture.take() else {
            return;
        };
        if overflowed {
            if matches!(kind, CapturedString::Key) {
                self.key_complete = true;
                self.pending_id = false;
            }
            return;
        }
        match kind {
            CapturedString::Key => {
                self.key_complete = true;
                self.pending_id =
                    serde_json::from_slice::<String>(&bytes).is_ok_and(|key| key == "id");
            }
            CapturedString::IdValue => {
                self.id = serde_json::from_slice(&bytes).ok();
                self.expecting_value = false;
            }
        }
    }
}

/// 逐块读取一行；超过上限后继续排空该行，但不再保留正文，避免一条异常
/// 响应把代理一次性推到无界内存。只有上限以内的行才交给 serde_json 解码。
pub(super) fn read_bounded_line(
    reader: &mut impl BufRead,
    limit: u64,
) -> Result<BoundedLine, String> {
    let mut data = Vec::new();
    let mut id_scanner = ResponseIdScanner::new();
    let mut total = 0u64;
    let mut oversized = false;
    loop {
        let chunk = reader
            .fill_buf()
            .map_err(|e| format!("共享 Xberg 读取失败：{e}"))?;
        if chunk.is_empty() {
            return Err("共享 Xberg 响应中断或缺少换行".into());
        }
        let consumed = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(chunk.len(), |index| index + 1);
        let part = &chunk[..consumed];
        total = total.saturating_add(part.len() as u64);
        id_scanner.feed(part);
        if !oversized {
            if total <= limit {
                data.extend_from_slice(part);
            } else {
                oversized = true;
                data.clear();
                data.shrink_to_fit();
            }
        }
        let ended = part.last() == Some(&b'\n');
        reader.consume(consumed);
        if ended {
            return if oversized {
                Ok(BoundedLine::Oversized {
                    id: id_scanner.id,
                    bytes: total,
                })
            } else {
                Ok(BoundedLine::Complete(data))
            };
        }
    }
}

pub(super) fn read_json(reader: &mut impl BufRead) -> Result<Value, String> {
    let line = read_bounded_line(reader, MAX_MESSAGE_BYTES).inspect_err(|_| {
        tracing::warn!(
            event = "xberg_message_read_failed",
            stage = "read",
            "共享管道消息读取失败"
        );
    })?;
    match line {
        BoundedLine::Complete(bytes) => serde_json::from_slice(&bytes).map_err(|_| {
            tracing::warn!(
                event = "xberg_message_decode_failed",
                stage = "decode",
                bytes = bytes.len(),
                "共享管道消息不是有效 JSON"
            );
            "共享 Xberg 返回了无效 JSON".into()
        }),
        BoundedLine::Oversized { bytes, .. } => {
            tracing::warn!(
                event = "xberg_message_read_failed",
                stage = "message_limit",
                bytes,
                "共享管道消息超过上限"
            );
            Err(format!("共享 Xberg 响应超过消息上限（{bytes} 字节）"))
        }
    }
}

pub(super) enum EngineResponse {
    Json(Value),
    Oversized { id: Option<String>, bytes: u64 },
}

pub(super) fn read_engine_response(reader: &mut impl BufRead) -> Result<EngineResponse, String> {
    match read_bounded_line(reader, MAX_MESSAGE_BYTES)? {
        BoundedLine::Complete(bytes) => serde_json::from_slice(&bytes)
            .map(EngineResponse::Json)
            .map_err(|_| "共享 Xberg 返回了无效 JSON".into()),
        BoundedLine::Oversized { id, bytes } => Ok(EngineResponse::Oversized { id, bytes }),
    }
}
pub(super) fn write_json(writer: &mut impl Write, value: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| {
        tracing::warn!(
            event = "xberg_message_write_failed",
            stage = "encode",
            "共享管道消息编码失败"
        );
        e.to_string()
    })?;
    if bytes.len() as u64 > MAX_MESSAGE_BYTES {
        tracing::warn!(
            event = "xberg_message_write_failed",
            stage = "message_limit",
            bytes = bytes.len(),
            "共享管道待发送消息超过上限"
        );
        return Err("Xberg 请求超过消息上限".into());
    }
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .and_then(|()| writer.flush())
        .map_err(|e| {
            tracing::warn!(
                event = "xberg_message_write_failed",
                stage = "write_or_flush",
                error_code = e.raw_os_error(),
                "共享管道消息写入或刷新失败"
            );
            format!("共享 Xberg 写入失败：{e}")
        })
}
