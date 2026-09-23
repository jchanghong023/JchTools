use crate::{
    config::Config,
    convert,
    model::{Action, FileRecord, Snapshot, Summary},
};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Params, Row};
use serde::{de::DeserializeOwned, Serialize};
use std::path::{Path, PathBuf};

pub const SCHEMA: &str = include_str!("schema.sql");
/// 当前代码已知的任务库 schema 版本；库版本高于此值时 fail-fast，避免用旧逻辑读新库；
/// 低于此值时同样拒绝——旧库的计划基于已废止的归类规则（3 起：归类依据由创建时间改为
/// 创建时间与修改时间中最早者，结构未变、版本号标记归类口径代次），按 R-04 必须重新
/// 分析，不做数据迁移。
pub const SCHEMA_VERSION: i64 = 3;
pub struct Database {
    pub conn: Connection,
    pub directory: PathBuf,
}
impl Database {
    pub fn create(directory: &Path) -> Result<Self> {
        std::fs::create_dir_all(directory)?;
        let mut value = Self::open(directory)?;
        // 建表与索引只提交一次，避免新任务为每条 DDL 单独提交。
        let transaction = value.conn.transaction()?;
        transaction.execute_batch(SCHEMA)?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
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
        // 版本检查只对“打开既有库”生效：新建库 user_version 恒为 0，建库事务随后写入
        // 当前版本。库版本与当前已知版本不一致则拒绝（fail-fast）：高版本结构未知，
        // 低版本库基于已废止的归类规则，按 R-04 必须重新分析。
        if existing_only {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            anyhow::ensure!(
                version == SCHEMA_VERSION,
                "任务库版本不兼容（{version} ≠ {SCHEMA_VERSION}）：旧任务由历史版本生成，请重新分析后再执行"
            );
        }
        Ok(Self {
            conn,
            directory: directory.to_path_buf(),
        })
    }
    pub fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.conn.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, serde_json::to_string(value)?])?;
        Ok(())
    }
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Result<T> {
        let value: String =
            self.conn
                .query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
                    r.get(0)
                })?;
        Ok(serde_json::from_str(&value)?)
    }
    // 配置可能来自旧版本任务库，需剥除已删除的设置键后再反序列化；
    // metadata 里存的是原始 JSON 文本，不能经 get<T> 先反序列化成字符串。
    pub fn config(&self) -> Result<Config> {
        let raw: String =
            self.conn
                .query_row("SELECT value FROM metadata WHERE key=?1", ["config"], |r| {
                    r.get(0)
                })?;
        Config::from_json_text(&raw)
    }
    pub fn summary(&self) -> Result<Summary> {
        self.get("summary")
    }
    pub fn log(
        &self,
        phase: &str,
        source: &str,
        target: &str,
        result: &str,
        reason: &str,
        size: u64,
    ) -> Result<()> {
        // 执行阶段每动作两条日志（数万动作），必须命中语句缓存：conn.execute 每次重新
        // 解析 SQL，批量语句缓存在大任务上是秒级差异。
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO events(time,phase,source,target,result,reason,size) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        )?;
        stmt.execute(params![
            chrono::Utc::now().to_rfc3339(),
            phase,
            source,
            target,
            result,
            reason,
            i64::try_from(size)?
        ])?;
        Ok(())
    }
    /// `name` 传分析时的原文件名；`name` 列的小写折叠与 name16/rel16（C-03 平局规则
    /// 用的 UTF-16 单元数）在此统一计算，调用方不再各自预折叠。
    pub fn insert_file(
        &self,
        rel: &str,
        name: &str,
        normal: &str,
        snapshot: &Snapshot,
    ) -> Result<()> {
        let folded = name.to_lowercase();
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO files(rel,name,normal,size,mtime,created,name16,rel16,identity,links) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        )?;
        stmt.execute(params![
            rel,
            folded,
            normal,
            i64::try_from(snapshot.size)?,
            snapshot.modified_ns,
            snapshot.created_ns,
            i64::try_from(name.encode_utf16().count())?,
            i64::try_from(rel.encode_utf16().count())?,
            snapshot.identity,
            i64::try_from(snapshot.links)?
        ])?;
        Ok(())
    }
    /// 扫描登记目录行。name 由调用方小写后传入（与 files.name 同口径）。
    pub fn insert_dir(&self, rel: &str, name: &str, depth: i64) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("INSERT INTO directories(rel,name,depth) VALUES(?1,?2,?3)")?;
        stmt.execute(params![rel, name, depth])?;
        Ok(())
    }
    /// 哈希回写（候选分页循环逐条到达，走语句缓存）。
    pub fn set_file_hash(&self, id: i64, hash: &str) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("UPDATE files SET hash=?1 WHERE id=?2")?;
        stmt.execute(params![hash, id])?;
        Ok(())
    }
    pub fn deactivate_file_id(&self, id: i64) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("UPDATE files SET active=0 WHERE id=?1")?;
        stmt.execute(params![id])?;
        Ok(())
    }
    pub fn deactivate_file_rel(&self, rel: &str) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("UPDATE files SET active=0 WHERE rel=?1")?;
        stmt.execute(params![rel])?;
        Ok(())
    }
    pub fn mark_cleanable(&self, id: i64) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("UPDATE files SET cleanable=1 WHERE id=?1")?;
        stmt.execute(params![id])?;
        Ok(())
    }
    pub fn insert_keeper(&self, file_id: i64, hash: &str, name: &str, normal: &str) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("INSERT INTO keepers(file_id,hash,name,normal) VALUES(?1,?2,?3,?4)")?;
        stmt.execute(params![file_id, hash, name, normal])?;
        Ok(())
    }
    /// 记录扫描污点：该目录里存在盘上可见但未入盘点的内容（被过滤/读取失败），
    /// 空目录规划据此（并向上传播）拒绝把它当作空目录。
    pub fn remember_taint(&self, rel: &str) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("INSERT OR IGNORE INTO scan_taint(rel) VALUES(?1)")?;
        stmt.execute(params![rel])?;
        Ok(())
    }
    pub fn files<P: Params>(&self, sql: &str, args: P) -> Result<Vec<FileRecord>> {
        // prepare_cached：分页循环里同一 SQL 反复到达，避免每页重新解析。
        // 传入的 SQL 文本必须跨调用逐字稳定，否则缓存永远不命中（只是退回逐次解析）。
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(args, file_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    /// 与 FILE_COLUMNS 同序的按 id 取行 SQL（写死列清单换静态常量，避免每次调用 format!）。
    const FILE_BY_ID_SQL: &str =
        "SELECT id,rel,name,normal,size,mtime,created,name16,rel16,identity,links,hash,cleanable FROM files WHERE id=?1";
    pub fn file(&self, id: i64) -> Result<FileRecord> {
        let mut stmt = self.conn.prepare_cached(Self::FILE_BY_ID_SQL)?;
        stmt.query_row([id], file_row).map_err(Into::into)
    }
    /// 按计划顺序取一页重复候选（duplicate_order JOIN files，一条语句取整批，
    /// 替代逐候选 file(id) 的主键单行查询）。seq 是 duplicate_order 的游标列。
    pub fn duplicate_page(&self, after: i64, limit: i64) -> Result<Vec<(i64, FileRecord)>> {
        let sql = format!(
            "SELECT o.seq,{} FROM duplicate_order AS o CROSS JOIN files AS f ON f.id=o.id \
             WHERE o.seq>?1 ORDER BY o.seq LIMIT ?2",
            file_columns_qualified("f")
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![after, limit], |row| {
            Ok((row.get::<_, i64>(0)?, file_row_offset(row, 1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn add_action(&self, action: &Action) -> Result<i64> {
        let mut stmt = self
            .conn
            .prepare_cached("INSERT INTO actions(kind,source,target,body,selected,state) VALUES(?1,?2,?3,?4,?5,'pending')")?;
        stmt.execute(params![
            serde_json::to_string(&action.kind)?,
            action.source,
            action.target,
            serde_json::to_string(action)?,
            action.selected
        ])?;
        Ok(self.conn.last_insert_rowid())
    }
    pub fn action(&self, id: i64) -> Result<Action> {
        self.conn
            .query_row(
                "SELECT id,body,selected,state FROM actions WHERE id=?1",
                [id],
                action_row,
            )
            .map_err(Into::into)
    }
    pub fn actions_page(&self, after: i64, limit: usize) -> Result<Vec<Action>> {
        self.actions_page_filtered(after, limit, None)
    }
    /// kind_filter: None=全部；Some("delete"/"move"/"empty_directory")（serde snake_case）。
    /// kind 白名单外的取值视为调用错误，直接报错，避免拼接出意外 SQL 语义。
    pub fn actions_page_filtered(
        &self,
        after: i64,
        limit: usize,
        kind_filter: Option<&str>,
    ) -> Result<Vec<Action>> {
        let limit = convert::usize_as_i64(limit.min(1000));
        let rows = if let Some(kind) = kind_filter {
            anyhow::ensure!(
                matches!(kind, "delete" | "move" | "empty_directory"),
                "未知的行动类型筛选：{kind}"
            );
            let like = format!("\"{kind}\"");
            let mut statement = self.conn.prepare_cached("SELECT id,body,selected,state FROM actions WHERE id>?1 AND kind=?2 ORDER BY id LIMIT ?3")?;
            let rows = statement
                .query_map(params![after, like, limit], action_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        } else {
            let mut statement = self.conn.prepare_cached(
                "SELECT id,body,selected,state FROM actions WHERE id>?1 ORDER BY id LIMIT ?2",
            )?;
            let rows = statement
                .query_map(params![after, limit], action_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        Ok(rows)
    }
    pub fn set_selected(&self, id: i64, selected: bool) -> Result<()> {
        let status: String = self.get("status")?;
        anyhow::ensure!(status == "ready", "只能修改待确认的整理计划");
        let changed = self.conn.execute("UPDATE actions SET selected=?1 WHERE id=?2 AND state='pending' AND EXISTS(SELECT 1 FROM metadata WHERE key='status' AND value=?3)", params![selected, id, serde_json::to_string("ready")?])?;
        anyhow::ensure!(changed == 1, "计划已开始执行或该项不存在，选择未改变");
        Ok(())
    }
    pub fn mark_action(&self, id: i64, state: &str) -> Result<()> {
        // 状态白名单：与 engine.rs 实际使用的状态集合保持一致。
        anyhow::ensure!(
            matches!(
                state,
                "pending" | "done" | "skipped" | "failed" | "unselected"
            ),
            "非法的行动状态：{state}"
        );
        let mut stmt = self
            .conn
            .prepare_cached("UPDATE actions SET state=?1 WHERE id=?2")?;
        let changed = stmt.execute(params![state, id])?;
        anyhow::ensure!(changed == 1, "行动不存在或状态未改变");
        Ok(())
    }
    pub fn reserve_target(&self, target: &str, file_id: i64) -> Result<bool> {
        // 仅 Windows 大小写不敏感文件系统上折叠大小写；Linux 等平台 Report.txt 与 report.txt 是不同目标。
        let key = if cfg!(windows) {
            target.to_lowercase()
        } else {
            target.to_string()
        };
        let mut stmt = self
            .conn
            .prepare_cached("INSERT OR IGNORE INTO targets(path,file_id) VALUES(?1,?2)")?;
        Ok(stmt.execute(params![key, file_id])? == 1)
    }
    pub fn event_page(&self, before: i64, limit: usize) -> Result<Vec<String>> {
        let before = if before <= 0 { i64::MAX } else { before };
        let mut statement = self.conn.prepare("SELECT time,phase,result,source,target,reason FROM events WHERE id<?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = statement.query_map(
            params![before, convert::usize_as_i64(limit.min(1000))],
            |row| {
                let values = (0..6)
                    .map(|i| row.get::<_, String>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(values.join(" | "))
            },
        )?;
        let result = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(result)
    }
    pub fn file_by_path(&self, rel: &str) -> Result<Option<FileRecord>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {FILE_COLUMNS} FROM files WHERE rel=?1"),
                [rel],
                file_row,
            )
            .optional()?)
    }
}
pub const FILE_COLUMNS: &str =
    "id,rel,name,normal,size,mtime,created,name16,rel16,identity,links,hash,cleanable";
/// 同一列清单的别名限定形式，供与其他表 JOIN 的查询使用（未限定的 `id` 会歧义）。
/// 由 FILE_COLUMNS 派生，避免两份清单各自漂移。
pub fn file_columns_qualified(alias: &str) -> String {
    FILE_COLUMNS
        .split(',')
        .map(|column| format!("{alias}.{column}"))
        .collect::<Vec<_>>()
        .join(",")
}
/// SQLite 以有符号 i64 存 size/links；写入方恒非负，读回负值即库损坏，按错误上报。
fn nonneg_u64(row: &Row<'_>, idx: usize) -> rusqlite::Result<u64> {
    u64::try_from(row.get::<_, i64>(idx)?).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Integer,
            "库中存在负的文件大小/链接数".into(),
        )
    })
}
fn file_row(row: &Row<'_>) -> rusqlite::Result<FileRecord> {
    file_row_offset(row, 0)
}
/// 与 FILE_COLUMNS 同序的行解析，从第 `offset` 列起读（供前面带有附加列的 JOIN 使用）。
fn file_row_offset(row: &Row<'_>, offset: usize) -> rusqlite::Result<FileRecord> {
    let column = |i: usize| i + offset;
    Ok(FileRecord {
        id: row.get(column(0))?,
        rel: row.get(column(1))?,
        name: row.get(column(2))?,
        normalized: row.get(column(3))?,
        snapshot: Snapshot {
            size: nonneg_u64(row, column(4))?,
            modified_ns: row.get(column(5))?,
            created_ns: row.get(column(6))?,
            identity: row.get(column(9))?,
            links: nonneg_u64(row, column(10))?,
        },
        hash: row.get(column(11))?,
        cleanable: row.get(column(12))?,
    })
}
fn action_row(row: &Row<'_>) -> rusqlite::Result<Action> {
    let json: String = row.get(1)?;
    let mut action: Action = serde_json::from_str(&json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    action.id = row.get(0)?;
    action.selected = row.get(2)?;
    action.state = row.get(3)?;
    Ok(action)
}
