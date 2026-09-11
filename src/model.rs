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
