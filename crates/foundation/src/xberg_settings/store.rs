// SQLite 连接与旧文本配置迁移：打开、busy 超时与 legacy 读写。
use super::{sqlite_failure, APP_SETTINGS_SCHEMA};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 打开共享应用配置数据库，使用唯一 schema 与原同步和等待策略。
pub fn open(root: &Path) -> Result<Connection, String> {
    open_with_timeout(root, Duration::from_secs(5))
}

/// 打开同一应用配置数据库，仅由调用方选择 SQLite 争用等待时长。
/// `Duration::ZERO` 用于不能因设置存储争用而停顿的后台维护循环（CA-05）。
pub fn open_with_timeout(root: &Path, timeout: Duration) -> Result<Connection, String> {
    std::fs::create_dir_all(root).map_err(|e| {
        tracing::error!(event = "settings_storage_failed", stage = "create_directory", error_type = ?e.kind(), error_code = ?e.raw_os_error(), "创建配置目录失败");
        format!("创建应用配置目录失败：{e}")
    })?;
    let db = Connection::open(root.join("config.sqlite3")).map_err(|e| {
        tracing::error!(event = "settings_storage_failed", stage = "sqlite_open", error_type = ?e.sqlite_error_code(), "打开配置数据库失败");
        format!("打开应用配置 SQLite 失败：{e}")
    })?;
    db.busy_timeout(timeout).map_err(|e| {
        sqlite_failure("busy_timeout", &e);
        e.to_string()
    })?;
    db.execute_batch(APP_SETTINGS_SCHEMA).map_err(|e| {
        sqlite_failure("initialize_schema", &e);
        format!("初始化应用配置 SQLite 失败：{e}")
    })?;
    Ok(db)
}

pub(super) fn read_legacy(root: &Path) -> Result<Option<String>, String> {
    let legacy_root = if cfg!(any(test, feature = "test-hooks")) {
        std::env::var_os("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT").map(PathBuf::from)
    } else {
        None
    }
    .unwrap_or_else(|| root.join("markdown-assets"));
    match std::fs::read_to_string(legacy_root.join("xberg-runtime-path.txt")) {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            tracing::error!(event = "settings_storage_failed", stage = "read_legacy", error_type = ?e.kind(), error_code = ?e.raw_os_error(), "读取旧配置失败");
            Err(format!("读取旧 Xberg 配置失败：{e}"))
        }
    }
}

pub(super) fn legacy_directory_value(value: &str) -> Result<&str, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("旧 Xberg 运行目录配置为空".into());
    }
    if !Path::new(value).is_absolute() {
        return Err("旧 Xberg 运行目录不是绝对路径，未迁移或覆盖配置".into());
    }
    Ok(value)
}

/// 旧文本已读取时，迁入失败只影响持久化，不丢失原目录能力。
pub(super) fn write_legacy_directory(db: &Connection, value: &str) -> bool {
    match db.execute(
        "INSERT OR IGNORE INTO app_settings(key,value) VALUES('xberg_directory',?1)",
        [value],
    ) {
        Ok(written) => {
            if written > 0 {
                tracing::info!(
                    component = "xberg_settings",
                    event = "settings_legacy_migration_written",
                    "旧版共享运行目录迁移写入完成"
                );
            }
            true
        }
        Err(error) => {
            tracing::warn!(
                component = "xberg_settings",
                event = "settings_legacy_migration_unsaved",
                stage = "migrate_directory",
                error_type = ?error.sqlite_error_code(),
                error_code = error.sqlite_error().map(|code| code.extended_code),
                "旧配置迁入未保存，继续使用原目录"
            );
            false
        }
    }
}
