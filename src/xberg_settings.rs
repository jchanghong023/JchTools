//! XB-18：独立于任务库的应用设置。主程序和截图服务编译同一份实现。
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

pub fn state_dir() -> Result<PathBuf, String> {
    if cfg!(debug_assertions) {
        if let Some(path) = std::env::var_os("JCHTOOLS_TEST_STATE_DIR") {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("测试状态目录必须为绝对路径".into());
            }
            return Ok(path);
        }
        // 现有隔离测试不应读写真实用户配置。
        for key in [
            "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT",
            "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT",
        ] {
            if let Some(path) = std::env::var_os(key)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
            {
                return Ok(path.join("app-settings"));
            }
        }
    }
    directories_next::ProjectDirs::from("", "", "JchTools")
        .map(|dirs| dirs.data_local_dir().to_path_buf())
        .ok_or_else(|| "无法定位 JchTools 用户配置目录".into())
}

fn open(root: &Path) -> Result<Connection, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("创建应用配置目录失败：{e}"))?;
    let db = Connection::open(root.join("config.sqlite3"))
        .map_err(|e| format!("打开应用配置 SQLite 失败：{e}"))?;
    db.busy_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    db.execute_batch(include_str!("app_settings.sql"))
        .map_err(|e| format!("初始化应用配置 SQLite 失败：{e}"))?;
    Ok(db)
}

/// 仅首次迁移旧文本。SQLite 有值时不读取旧文本，迁移失败不删除原件。
pub fn load() -> Result<Option<PathBuf>, String> {
    let root = state_dir()?;
    let db = open(&root)?;
    let read = || {
        db.query_row(
            "SELECT value FROM app_settings WHERE key='xberg_directory'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
    };
    if let Some(value) = read().map_err(|e| format!("读取 Xberg 配置失败：{e}"))? {
        if value.trim().is_empty() {
            return Err("已保存的 Xberg 配置为空".into());
        }
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err("已保存的 Xberg 目录不是绝对路径，请重新保存".into());
        }
        return Ok(Some(path));
    }
    let legacy_root = if cfg!(debug_assertions) {
        std::env::var_os("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT").map(PathBuf::from)
    } else {
        None
    }
    .unwrap_or_else(|| root.join("markdown-assets"));
    let value = match std::fs::read_to_string(legacy_root.join("xberg-runtime-path.txt")) {
        Ok(value) => value,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("读取旧 Xberg 配置失败：{e}")),
    };
    let value = value.trim();
    if value.is_empty() {
        return Err("旧 Xberg 运行目录配置为空".into());
    }
    if !Path::new(value).is_absolute() {
        return Err("旧 Xberg 运行目录不是绝对路径，未迁移或覆盖配置".into());
    }
    db.execute(
        "INSERT OR IGNORE INTO app_settings(key,value) VALUES('xberg_directory',?1)",
        [value],
    )
    .map_err(|e| format!("迁移 Xberg 配置到 SQLite 失败：{e}"))?;
    read()
        .map(|value| value.map(PathBuf::from))
        .map_err(|e| format!("读取迁移后的配置失败：{e}"))
}

/// 先校验再提交；错误不会覆盖旧值。场景资产校验由使用方分别完成。
pub fn save(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || !path.is_dir() || !path.join("xberg.exe").is_file() {
        return Err("请选择包含 xberg.exe 的有效绝对目录".into());
    }
    let path = std::fs::canonicalize(path).map_err(|e| format!("解析 Xberg 目录失败：{e}"))?;
    let text = path.to_str().ok_or("Xberg 路径无法编码为 Unicode")?;
    open(&state_dir()?)?.execute(
        "INSERT INTO app_settings(key,value) VALUES('xberg_directory',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [text])
        .map_err(|e| format!("保存 Xberg 配置到 SQLite 失败：{e}"))?;
    Ok(())
}

pub fn required() -> Result<PathBuf, String> {
    load()?.ok_or_else(|| "尚未配置 Xberg：请在转 Markdown 或截图 OCR 页保存共享运行目录".into())
}
