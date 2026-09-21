//! 跨运行哈希缓存（合同 C-13）：状态目录级的共享 SQLite 库，按
//! （文件标识、大小、修改时间）记录已算出的整文件哈希，供后续运行的候选
//! 文件直接复用、不再重读内容。每次分析都会新建任务库，所以缓存独立于
//! 单个任务库、放在状态目录根，跨任务存活。
//!
//! 缓存只是分析加速：任何打开/读写故障都必须由调用方降级为「无缓存、
//! 全部重算」，`MUST NOT` 演变成任务失败；文件标识退化（无法区分不同
//! 文件）时禁用复用。判定依据仍是 C-02 的整文件哈希一致。
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct HashCache {
    conn: Connection,
}

/// 文件标识形如「卷序列号:索引高:索引低」（fsutil::snapshot_with）。
/// 部分文件系统不提供索引号（恒为 0）：此时不同文件共用同一键，复用会
/// 误判重复，`MUST NOT` 入缓存（C-13）；非三段结构一律视为退化。
pub fn identity_is_degenerate(identity: &str) -> bool {
    let parts: Vec<&str> = identity.split(':').collect();
    match parts.as_slice() {
        [_, high, low] => high == &"0" && low == &"0",
        _ => true,
    }
}

impl HashCache {
    /// 打开（必要时创建）状态目录下的 hash-cache.sqlite3。失败由调用方降级。
    pub fn open(state_dir: &Path) -> Result<Self> {
        let conn =
            Connection::open(state_dir.join("hash-cache.sqlite3")).context("打开哈希缓存库失败")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS hash_cache(
                 identity TEXT NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
                 hash TEXT NOT NULL, updated INTEGER NOT NULL,
                 PRIMARY KEY(identity,size,mtime))",
        )?;
        Ok(Self { conn })
    }

    /// 查缓存哈希；键与 C-13 一致（文件标识、大小、修改时间三者必须全等）。
    pub fn lookup(&self, identity: &str, size: i64, mtime: i64) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT hash FROM hash_cache WHERE identity=?1 AND size=?2 AND mtime=?3",
        )?;
        Ok(stmt
            .query_row(params![identity, size, mtime], |row| row.get(0))
            .optional()?)
    }

    /// 批量写入本次新算出的哈希；单事务保证短锁，updated 供后续清理策略使用。
    pub fn store(&self, entries: &[(String, i64, i64, String)]) -> Result<()> {
        let updated = chrono::Utc::now().timestamp();
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let mut stmt = self.conn.prepare_cached(
                "INSERT INTO hash_cache(identity,size,mtime,hash,updated) VALUES(?1,?2,?3,?4,?5) \
                 ON CONFLICT(identity,size,mtime) DO UPDATE SET hash=excluded.hash,updated=excluded.updated",
            )?;
            for (identity, size, mtime, hash) in entries {
                stmt.execute(params![identity, size, mtime, hash, updated])?;
            }
            Ok::<_, anyhow::Error>(())
        })();
        match result {
            Ok(()) => self.conn.execute_batch("COMMIT")?,
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                return Err(error);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::identity_is_degenerate;

    // 覆盖 C-13
    #[test]
    fn degenerate_identities_are_never_cacheable() {
        // 索引号恒为 0 的卷无法区分不同文件：复用会误判重复（C-13 禁用）。
        assert!(identity_is_degenerate("123:0:0"));
        // 非三段结构（残缺/多余段）一律视为退化，宁可重算。
        assert!(identity_is_degenerate("abc"));
        assert!(identity_is_degenerate(""));
        assert!(identity_is_degenerate("1:2:3:4"));
        // 正常的「卷:索引高:索引低」可入缓存。
        assert!(!identity_is_degenerate("123:4:567"));
    }
}
