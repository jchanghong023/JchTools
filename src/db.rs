use crate::{config::Config, model::{Action, FileRecord, Snapshot, Summary}};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Params, Row};
use serde::{de::DeserializeOwned, Serialize};
use std::path::{Path, PathBuf};

pub const SCHEMA: &str = include_str!("schema.sql");
/// 当前代码已知的任务库 schema 版本；库版本高于此值时 fail-fast，避免用旧逻辑读新库。
pub const SCHEMA_VERSION: i64 = 1;
pub struct Database { pub conn: Connection, pub directory: PathBuf }
impl Database {
    pub fn create(directory: &Path) -> Result<Self> {
        std::fs::create_dir_all(directory)?;
        let value = Self::open(directory)?;
        value.conn.execute_batch(SCHEMA)?;
        value.conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(value)
    }
    pub fn open(directory: &Path) -> Result<Self> {
        Self::open_impl(directory, false)
    }
    /// 只读/打开已有任务库的场景使用：库文件不存在时报错，不静默创建空库。
    pub fn open_existing(directory: &Path) -> Result<Self> {
        Self::open_impl(directory, true)
    }
    fn open_impl(directory: &Path, existing_only: bool) -> Result<Self> {
        let path = directory.join("task.sqlite3");
        if existing_only {
            anyhow::ensure!(path.is_file(), "任务库文件不存在：{}", path.display());
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=FILE; PRAGMA cache_size=-65536; PRAGMA foreign_keys=ON;")?;
        // 版本检查：库版本高于当前已知版本则拒绝打开（fail-fast），避免静默读写不兼容结构。
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        anyhow::ensure!(version <= SCHEMA_VERSION,
            "任务库版本过高（{version} > {SCHEMA_VERSION}），请升级 JchTools 后再使用该任务库");
        Ok(Self { conn, directory: directory.to_path_buf() })
    }
    pub fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.conn.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, serde_json::to_string(value)?])?;
        Ok(())
    }
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Result<T> {
        let value: String = self.conn.query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| r.get(0))?;
        Ok(serde_json::from_str(&value)?)
    }
    // 配置可能来自旧版本任务库，需剥除已删除的设置键后再反序列化；
    // metadata 里存的是原始 JSON 文本，不能经 get<T> 先反序列化成字符串。
    pub fn config(&self) -> Result<Config> {
        let raw: String = self.conn.query_row("SELECT value FROM metadata WHERE key=?1", ["config"], |r| r.get(0))?;
        Config::from_json_text(&raw)
    }
    pub fn summary(&self) -> Result<Summary> { self.get("summary") }
    pub fn log(&self, phase: &str, source: &str, target: &str, result: &str, reason: &str, size: u64) -> Result<()> {
        self.conn.execute("INSERT INTO events(time,phase,source,target,result,reason,size) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![chrono::Utc::now().to_rfc3339(), phase, source, target, result, reason, i64::try_from(size)?])?;
        Ok(())
    }
    pub fn insert_file(&self, rel: &str, name: &str, normal: &str, snapshot: &Snapshot) -> Result<()> {
        self.conn.execute("INSERT INTO files(rel,name,normal,size,mtime,identity,links) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![rel, name, normal, i64::try_from(snapshot.size)?, snapshot.modified_ns, snapshot.identity, i64::try_from(snapshot.links)?])?;
        Ok(())
    }
    pub fn files<P: Params>(&self, sql: &str, args: P) -> Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(args, file_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn file(&self, id: i64) -> Result<FileRecord> {
        self.conn.query_row(&format!("SELECT {FILE_COLUMNS} FROM files WHERE id=?1"), [id], file_row).map_err(Into::into)
    }
    pub fn add_action(&self, action: &Action) -> Result<i64> {
        self.conn.execute("INSERT INTO actions(kind,source,target,body,selected,state) VALUES(?1,?2,?3,?4,?5,'pending')",
            params![serde_json::to_string(&action.kind)?, action.source, action.target,
                serde_json::to_string(action)?, action.selected])?;
        Ok(self.conn.last_insert_rowid())
    }
    pub fn action(&self, id: i64) -> Result<Action> {
        self.conn.query_row("SELECT id,body,selected,state FROM actions WHERE id=?1", [id], action_row).map_err(Into::into)
    }
    pub fn actions_page(&self, after: i64, limit: usize) -> Result<Vec<Action>> {
        self.actions_page_filtered(after, limit, None)
    }
    /// kind_filter: None=全部；Some("delete"/"move"/"hardlink"/"empty_directory")（serde snake_case）。
    /// kind 白名单外的取值视为调用错误，直接报错，避免拼接出意外 SQL 语义。
    pub fn actions_page_filtered(&self, after: i64, limit: usize, kind_filter: Option<&str>) -> Result<Vec<Action>> {
        let limit = limit.min(1000) as i64;
        let rows = if let Some(kind) = kind_filter {
            anyhow::ensure!(matches!(kind, "delete" | "move" | "hardlink" | "empty_directory"),
                "未知的行动类型筛选：{kind}");
            let like = format!("\"{kind}\"");
            let mut statement = self.conn.prepare("SELECT id,body,selected,state FROM actions WHERE id>?1 AND kind=?2 ORDER BY id LIMIT ?3")?;
            let rows = statement.query_map(params![after, like, limit], action_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        } else {
            let mut statement = self.conn.prepare("SELECT id,body,selected,state FROM actions WHERE id>?1 ORDER BY id LIMIT ?2")?;
            let rows = statement.query_map(params![after, limit], action_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        Ok(rows)
    }
    pub fn set_selected(&self, id: i64, selected: bool) -> Result<()> {
        let status: String = self.get("status")?;
        anyhow::ensure!(status == "ready", "只能修改待确认的整理计划");
        let changed = self.conn.execute("UPDATE actions SET selected=?1 WHERE id=?2 AND state='pending' AND EXISTS(SELECT 1 FROM metadata WHERE key='status' AND value=?3)", params![selected, id, serde_json::to_string("ready")?])?;
        anyhow::ensure!(changed==1, "计划已开始执行或该项不存在，选择未改变");
        Ok(())
    }
    pub fn mark_action(&self, id: i64, state: &str) -> Result<()> {
        // 状态白名单：与 engine.rs 实际使用的状态集合保持一致。
        anyhow::ensure!(matches!(state, "pending" | "done" | "skipped" | "failed" | "unselected"),
            "非法的行动状态：{state}");
        let changed = self.conn.execute("UPDATE actions SET state=?1 WHERE id=?2", params![state, id])?;
        anyhow::ensure!(changed == 1, "行动不存在或状态未改变");
        Ok(())
    }
    pub fn reserve_target(&self, target: &str, file_id: i64) -> Result<bool> {
        // 仅 Windows 大小写不敏感文件系统上折叠大小写；Linux 等平台 Report.txt 与 report.txt 是不同目标。
        let key = if cfg!(windows) { target.to_lowercase() } else { target.to_string() };
        Ok(self.conn.execute("INSERT OR IGNORE INTO targets(path,file_id) VALUES(?1,?2)", params![key,file_id])? == 1)
    }
    pub fn event_page(&self, before: i64, limit: usize) -> Result<Vec<String>> {
        let before = if before <= 0 { i64::MAX } else { before };
        let mut statement = self.conn.prepare("SELECT time,phase,result,source,target,reason FROM events WHERE id<?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = statement.query_map(params![before,limit.min(1000) as i64], |row| {
            let values = (0..6).map(|i| row.get::<_,String>(i)).collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(values.join(" | "))
        })?;
        let result = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(result)
    }
    pub fn export_csv(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.is_dir() {
                anyhow::bail!("导出目录不存在：{}；请先创建父目录或换一个路径", parent.display());
            }
        }
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)
            .context("导出文件已存在或不可写，请换一个名称")?;
        let mut writer = csv::Writer::from_writer(file);
        writer.write_record(["time", "phase", "result", "source", "target", "reason", "logical_bytes"])?;
        let mut statement = self.conn.prepare("SELECT time,phase,result,source,target,reason,size FROM events ORDER BY id")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let mut values = (0..6).map(|i| row.get::<_,String>(i)).collect::<rusqlite::Result<Vec<_>>>()?;
            // 防表格公式注入：前导空白后再跟危险字符也要加引号前缀。
            // 危险字符判定前先剥 U+FEFF / U+200B / Cf（格式字符）前缀，
            // 防止不可见字符把 =+ 推到 trim_start 之后逃逸转义。
            for value in &mut values {
                let stripped = value.trim_start_matches(is_invisible_or_format);
                let dangerous = stripped.trim_start().chars().next().is_some_and(|c| "=+-@\t\r".contains(c));
                if dangerous { value.insert(0, '\''); }
            }
            values.push(row.get::<_,i64>(6)?.to_string());
            writer.write_record(values)?;
        }
        writer.flush()?;
        Ok(())
    }
    pub fn file_by_path(&self, rel: &str) -> Result<Option<FileRecord>> {
        Ok(self.conn.query_row(&format!("SELECT {FILE_COLUMNS} FROM files WHERE rel=?1"), [rel], file_row).optional()?)
    }
}
/// 危险字符判定前需剥除的前缀字符：U+FEFF（BOM）、U+200B（零宽空格），
/// 以及 Unicode Cf（格式字符）常见区间（方向控制、词连接、标记语言控制等）。
fn is_invisible_or_format(c: char) -> bool {
    if c == '\u{FEFF}' || c == '\u{200B}' { return true; }
    matches!(c as u32,
        0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x180E |
        0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 |
        0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB |
        0x110BD | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}
pub const FILE_COLUMNS: &str = "id,rel,name,normal,size,mtime,identity,links,hash,cleanable";
fn file_row(row: &Row<'_>) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord { id: row.get(0)?, rel: row.get(1)?, name: row.get(2)?, normalized: row.get(3)?,
        snapshot: Snapshot { size: row.get::<_,i64>(4)? as u64, modified_ns: row.get(5)?,
            identity: row.get(6)?, links: row.get::<_,i64>(7)? as u64 }, hash: row.get(8)?, cleanable: row.get(9)? })
}
fn action_row(row: &Row<'_>) -> rusqlite::Result<Action> {
    let json: String = row.get(1)?;
    let mut action: Action = serde_json::from_str(&json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e)))?;
    action.id = row.get(0)?; action.selected = row.get(2)?; action.state = row.get(3)?;
    Ok(action)
}
