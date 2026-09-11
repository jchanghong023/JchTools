use crate::{config::Config, model::{Action, FileRecord, Snapshot, Summary}};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Params, Row};
use serde::{de::DeserializeOwned, Serialize};
use std::path::{Path, PathBuf};

pub const SCHEMA: &str = include_str!("schema.sql");
pub struct Database { pub conn: Connection, pub directory: PathBuf }
impl Database {
    pub fn create(directory: &Path) -> Result<Self> {
        std::fs::create_dir_all(directory)?;
        let value = Self::open(directory)?;
        value.conn.execute_batch(SCHEMA)?;
        Ok(value)
    }
    pub fn open(directory: &Path) -> Result<Self> {
        let conn = Connection::open(directory.join("task.sqlite3"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=FILE; PRAGMA cache_size=-65536; PRAGMA foreign_keys=ON;")?;
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
    pub fn config(&self) -> Result<Config> { self.get("config") }
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
        let mut statement = self.conn.prepare("SELECT id,body,selected,state FROM actions WHERE id>?1 ORDER BY id LIMIT ?2")?;
        let rows = statement.query_map(params![after, limit.min(1000) as i64], action_row)?;
        let result = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(result)
    }
    pub fn set_selected(&self, id: i64, selected: bool) -> Result<()> {
        let status: String = self.get("status")?;
        anyhow::ensure!(status == "ready", "只能修改待确认的整理计划");
        let changed = self.conn.execute("UPDATE actions SET selected=?1 WHERE id=?2 AND state='pending' AND EXISTS(SELECT 1 FROM metadata WHERE key='status' AND value=?3)", params![selected, id, serde_json::to_string("ready")?])?;
        anyhow::ensure!(changed==1, "计划已开始执行或该项不存在，选择未改变");
        Ok(())
    }
    pub fn mark_action(&self, id: i64, state: &str) -> Result<()> {
        self.conn.execute("UPDATE actions SET state=?1 WHERE id=?2", params![state,id])?;
        Ok(())
    }
    pub fn reserve_target(&self, target: &str, file_id: i64) -> Result<bool> {
        Ok(self.conn.execute("INSERT OR IGNORE INTO targets(path,file_id) VALUES(?1,?2)", params![target.to_lowercase(),file_id])? == 1)
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
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)
            .context("导出文件已存在或不可写，请换一个名称")?;
        let mut writer = csv::Writer::from_writer(file);
        writer.write_record(["time", "phase", "result", "source", "target", "reason", "logical_bytes"])?;
        let mut statement = self.conn.prepare("SELECT time,phase,result,source,target,reason,size FROM events ORDER BY id")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let mut values = (0..6).map(|i| row.get::<_,String>(i)).collect::<rusqlite::Result<Vec<_>>>()?;
            // Prevent spreadsheet formula injection by untrusted filenames.
            for value in &mut values {
                if value.chars().next().is_some_and(|c| "=+-@\t\r".contains(c)) { value.insert(0, '\''); }
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
