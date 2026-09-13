use crate::config::DeleteMode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub size: u64,
    pub modified_ns: i64,
    pub identity: String,
    pub links: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: i64,
    pub rel: String,
    pub name: String,
    pub normalized: String,
    pub snapshot: Snapshot,
    pub hash: Option<String>,
    pub cleanable: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind { Delete, Move, Hardlink, EmptyDirectory }
/// 动作状态。持久化与界面流转仍以 snake_case 文本（"pending"/"done"/"failed"/"skipped"/"unselected"）
/// 存于 actions.state 列；本枚举提供类型安全的取值/解析，避免各处手写魔法字符串。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionState { Pending, Done, Failed, Skipped, Unselected }
impl ActionState {
    /// 与数据库 actions.state 列、引擎 mark_action 调用一致的 snake_case 文本。
    pub fn as_str(self) -> &'static str {
        match self {
            ActionState::Pending => "pending",
            ActionState::Done => "done",
            ActionState::Failed => "failed",
            ActionState::Skipped => "skipped",
            ActionState::Unselected => "unselected",
        }
    }
}
impl std::str::FromStr for ActionState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(ActionState::Pending),
            "done" => Ok(ActionState::Done),
            "failed" => Ok(ActionState::Failed),
            "skipped" => Ok(ActionState::Skipped),
            "unselected" => Ok(ActionState::Unselected),
            other => Err(format!("未知动作状态：{other}")),
        }
    }
}
impl From<ActionState> for String {
    fn from(state: ActionState) -> Self { state.as_str().into() }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub id: i64,
    pub kind: ActionKind,
    pub source: String,
    pub target: Option<String>,
    pub reason: String,
    pub expected: Option<Snapshot>,
    pub keeper: Option<(String, Snapshot)>,
    pub hash: Option<String>,
    pub mode: DeleteMode,
    pub selected: bool,
    /// snake_case 文本，合法取值见 [`ActionState`]；暂保留 String 以免牵动 db/gui/planner 的读写路径。
    pub state: String,
}
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub scanned: u64,
    pub scanned_bytes: u64,
    pub archives_ok: u64,
    pub archives_failed: u64,
    pub extracted: u64,
    pub planned_delete: u64,
    pub planned_move: u64,
    pub planned_link: u64,
    pub planned_empty: u64,
    pub candidate_bytes: u64,
    pub deleted: u64,
    pub recycled: u64,
    pub moved: u64,
    pub linked: u64,
    pub skipped: u64,
    pub errors: u64,
    pub permanent_bytes: u64,
    pub recycled_bytes: u64,
}
impl Summary {
    pub fn description(&self) -> String {
        format!("扫描 {} 个文件 / {}\n解压成功 {} 包；失败 {} 包；产生 {} 个文件\n待删除 {} 项 · 待移动 {} 项 · 待硬链接 {} 项 · 空目录复查 {} 项\n候选逻辑大小 {}（移入回收站不会立即释放磁盘空间）\n已永久删除 {} 项 / {}；已回收 {} 项 / {}\n已移动 {} 项；已硬链接 {} 项；跳过 {} 项；错误 {} 项",
            self.scanned, bytes(self.scanned_bytes), self.archives_ok, self.archives_failed,
            self.extracted, self.planned_delete, self.planned_move, self.planned_link,
            self.planned_empty, bytes(self.candidate_bytes), self.deleted,
            bytes(self.permanent_bytes), self.recycled, bytes(self.recycled_bytes),
            self.moved, self.linked, self.skipped, self.errors)
    }
}
pub fn bytes(value: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut size = value as f64;
    let mut i = 0;
    while size >= 1024.0 && i + 1 < UNITS.len() { size /= 1024.0; i += 1; }
    if i == 0 { format!("{value} B") } else { format!("{size:.2} {}", UNITS[i]) }
}

#[cfg(test)]
mod tests {
    use super::ActionState;
    use std::str::FromStr;

    #[test]
    fn action_state_text_roundtrip_matches_db_values() {
        for state in [ActionState::Pending, ActionState::Done, ActionState::Failed,
                      ActionState::Skipped, ActionState::Unselected] {
            let text = state.as_str();
            assert_eq!(ActionState::from_str(text).unwrap(), state);
            // 持久化文本必须与 db.rs mark_action 校验的 snake_case 字面量一致。
            assert_eq!(serde_json::to_string(&state).unwrap(), format!("\"{text}\""));
            let as_string: String = state.into();
            assert_eq!(as_string, text);
        }
        assert!(ActionState::from_str("unknown").is_err());
    }
}
