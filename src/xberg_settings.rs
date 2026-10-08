//! XB-18：独立于任务库的应用设置。主程序和截图服务编译同一份实现。
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

pub fn state_dir() -> Result<PathBuf, String> {
    if let Some(path) = test_directory_override(cfg!(any(test, feature = "test-hooks")), |key| {
        std::env::var_os(key)
    })? {
        return Ok(path);
    }
    directories_next::ProjectDirs::from("", "", "JchTools")
        .map(|dirs| dirs.data_local_dir().to_path_buf())
        .ok_or_else(|| "无法定位 JchTools 用户配置目录".into())
}

/// 显式测试策略只负责隔离根；生产策略不查询这些环境变量。
fn test_directory_override(
    enabled: bool,
    read: impl Fn(&str) -> Option<OsString>,
) -> Result<Option<PathBuf>, String> {
    if enabled {
        if let Some(path) = read("JCHTOOLS_TEST_STATE_DIR") {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("测试状态目录必须为绝对路径".into());
            }
            return Ok(Some(path));
        }
        // 现有隔离测试不应读写真实用户配置。
        for key in [
            "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT",
            "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT",
        ] {
            if let Some(path) = read(key).map(PathBuf::from).filter(|p| p.is_absolute()) {
                return Ok(Some(path.join("app-settings")));
            }
        }
    }
    Ok(None)
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

fn read_legacy(root: &Path) -> Result<Option<String>, String> {
    let legacy_root = if cfg!(any(test, feature = "test-hooks")) {
        std::env::var_os("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT").map(PathBuf::from)
    } else {
        None
    }
    .unwrap_or_else(|| root.join("markdown-assets"));
    match std::fs::read_to_string(legacy_root.join("xberg-runtime-path.txt")) {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("读取旧 Xberg 配置失败：{e}")),
    }
}

fn legacy_directory_value(value: &str) -> Result<&str, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("旧 Xberg 运行目录配置为空".into());
    }
    if !Path::new(value).is_absolute() {
        return Err("旧 Xberg 运行目录不是绝对路径，未迁移或覆盖配置".into());
    }
    Ok(value)
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
    let Some(value) = read_legacy(&root)? else {
        return Ok(None);
    };
    let value = legacy_directory_value(&value)?;
    db.execute(
        "INSERT OR IGNORE INTO app_settings(key,value) VALUES('xberg_directory',?1)",
        [value],
    )
    .map_err(|e| format!("迁移 Xberg 配置到 SQLite 失败：{e}"))?;
    read()
        .map(|value| value.map(PathBuf::from))
        .map_err(|e| format!("读取迁移后的配置失败：{e}"))
}

/// XB-20/XB-21：两种来源各自保存，所有功能只读取一个当前来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Custom,
    Downloaded,
}
impl Source {
    fn key(self) -> &'static str {
        match self {
            Self::Custom => "xberg_custom_directory",
            Self::Downloaded => "xberg_downloaded_directory",
        }
    }
    fn value(self) -> &'static str {
        match self {
            Self::Custom => "custom",
            Self::Downloaded => "downloaded",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub source: Source,
    pub custom: Option<PathBuf>,
    pub downloaded: Option<PathBuf>,
}

/// 兼容旧 SQLite 与文本配置；失效路径仍保留供用户修复。
pub fn settings() -> Result<Settings, String> {
    let legacy = load()?;
    let db = open(&state_dir()?)?;
    let read = |key: &str| -> Result<Option<String>, String> {
        db.query_row(
            "SELECT value FROM app_settings WHERE key=?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| format!("读取 Xberg 配置失败：{e}"))
    };
    let source = match read("xberg_source")?.as_deref() {
        None | Some("custom") => Source::Custom,
        Some("downloaded") => Source::Downloaded,
        Some(_) => return Err("保存的 Xberg 来源无效，请重新选择".into()),
    };
    Ok(Settings {
        source,
        custom: read(Source::Custom.key())?
            .map(PathBuf::from)
            .or_else(|| (source == Source::Custom).then_some(legacy).flatten()),
        downloaded: read(Source::Downloaded.key())?.map(PathBuf::from),
    })
}

pub fn select(source: Source) -> Result<(), String> {
    let config = settings()?;
    let path = match source {
        Source::Custom => config.custom,
        Source::Downloaded => config.downloaded,
    }
    .ok_or("该来源尚无已保存的目录")?;
    save_source(source, &path)
}

/// 目录与来源在同一 SQLite 事务内落盘，失败不改写有效配置。
pub fn save_source(source: Source, path: &Path) -> Result<(), String> {
    if !path.is_absolute() || !path.is_dir() || !path.join("xberg.exe").is_file() {
        return Err("请选择包含 xberg.exe 的有效绝对目录".into());
    }
    let path = std::fs::canonicalize(path).map_err(|e| format!("解析 Xberg 目录失败：{e}"))?;
    let text = path.to_str().ok_or("Xberg 路径无法编码为 Unicode")?;
    let root = state_dir()?;
    let mut db = open(&root)?;
    let tx = db.transaction().map_err(|e| e.to_string())?;
    // 切到下载来源前，把旧版唯一目录保留为用户目录。
    tx.execute("INSERT OR IGNORE INTO app_settings(key,value) SELECT 'xberg_custom_directory',value FROM app_settings WHERE key='xberg_directory' AND NOT EXISTS(SELECT 1 FROM app_settings WHERE key='xberg_source')", [])
        .map_err(|e| e.to_string())?;
    if source == Source::Downloaded {
        let migrate_text: bool = tx
            .query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM app_settings WHERE key IN ('xberg_source','xberg_custom_directory'))",
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("读取旧 Xberg 来源配置失败：{e}"))?;
        if migrate_text {
            if let Some(value) = read_legacy(&root)? {
                let value = legacy_directory_value(&value)?;
                tx.execute(
                    "INSERT INTO app_settings(key,value) VALUES('xberg_custom_directory',?1)",
                    [value],
                )
                .map_err(|e| format!("迁移旧 Xberg 用户目录失败：{e}"))?;
            }
        }
    }
    for (key, value) in [
        (source.key(), text),
        ("xberg_source", source.value()),
        ("xberg_directory", text),
    ] {
        tx.execute("INSERT INTO app_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key, value])
            .map_err(|e| format!("保存 Xberg 配置失败：{e}"))?;
    }
    tx.commit().map_err(|e| format!("提交 Xberg 配置失败：{e}"))
}

pub fn save(path: &Path) -> Result<(), String> {
    save_source(Source::Custom, path)
}

pub fn required() -> Result<PathBuf, String> {
    load()?.ok_or_else(|| "尚未配置 Xberg：请在设置页下载 Xberg 或保存已有目录".into())
}

#[cfg(test)]
mod tests {
    use super::test_directory_override;
    use std::ffi::OsString;
    use std::path::PathBuf;

    // 覆盖 XB-18/XB-21：生产策略不读取任何测试环境变量，不能重定向配置。
    #[test]
    fn production_directory_policy_ignores_test_environment() {
        let override_root =
            test_directory_override(false, |_| panic!("生产策略不能访问测试环境变量"));
        assert_eq!(override_root, Ok(None));
    }

    // 覆盖 XB-18：隔离测试可显式选择根目录，且不触碰真实用户配置。
    #[test]
    fn explicit_test_directory_policy_preserves_isolation() {
        let root = PathBuf::from("C:/synthetic-jchtools-state");
        assert_eq!(
            test_directory_override(true, |key| {
                (key == "JCHTOOLS_TEST_STATE_DIR").then(|| root.clone().into_os_string())
            }),
            Ok(Some(root))
        );
        assert!(test_directory_override(true, |key| {
            (key == "JCHTOOLS_TEST_STATE_DIR").then(|| OsString::from("relative-state"))
        })
        .is_err());
    }
}
