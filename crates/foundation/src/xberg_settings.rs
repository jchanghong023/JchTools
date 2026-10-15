//! XB-18：独立于任务库的应用设置。主程序和截图服务使用同一基础库。
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

/// 应用配置的唯一 SQLite schema；ACP 与共享运行目录使用相同资源。
pub const APP_SETTINGS_SCHEMA: &str = include_str!("app_settings.sql");

/// P-10：配置操作的开始和终态在默认 INFO 下可见，不记录数据库值或用户配置正文。
fn observe<T>(
    operation: &'static str,
    body: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let span = crate::logging::operation_span("xberg_settings", operation);
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::info!(event = "settings_operation_started", "配置操作开始");
    let result = body();
    let elapsed_ms = crate::logging::elapsed_ms(started);
    if result.is_ok() {
        tracing::info!(
            event = "settings_operation_completed",
            elapsed_ms,
            "配置操作完成"
        );
    } else {
        tracing::error!(
            event = "settings_operation_failed",
            stage = operation,
            error_type = "configuration",
            elapsed_ms,
            "配置操作失败"
        );
    }
    result
}

fn sqlite_failure(stage: &'static str, error: &rusqlite::Error) {
    tracing::error!(event = "settings_storage_failed", stage,
        error_type = ?error.sqlite_error_code(),
        error_code = error.sqlite_error().map(|code| code.extended_code),
        "配置数据库调用失败");
}

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
            if let Some(path) = read(key).map(PathBuf::from) {
                if !path.is_absolute() {
                    return Err("测试资产目录必须为绝对路径".into());
                }
                return Ok(Some(path.join("app-settings")));
            }
        }
    }
    Ok(None)
}

#[derive(Debug, PartialEq, Eq)]
enum BundleOverride {
    Unchanged,
    Isolated(Option<PathBuf>),
}

/// 测试隔离同时覆盖随包目录和注册表；生产策略不读取测试环境变量。
fn test_bundle_override(
    enabled: bool,
    read: impl Fn(&str) -> Option<OsString>,
) -> Result<BundleOverride, String> {
    if !enabled {
        return Ok(BundleOverride::Unchanged);
    }
    if let Some(path) = read("JCHTOOLS_TEST_BUNDLED_XBERG_DIR") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("测试随包 Xberg 目录必须为绝对路径".into());
        }
        return Ok(BundleOverride::Isolated(Some(path)));
    }
    if test_directory_override(true, read)?.is_some() {
        return Ok(BundleOverride::Isolated(None));
    }
    Ok(BundleOverride::Unchanged)
}

fn valid_runtime_directory(path: &Path) -> Result<bool, String> {
    if !path.is_absolute() {
        return Ok(false);
    }
    let exists = |member: &Path, directory: bool| -> Result<bool, String> {
        match std::fs::metadata(member) {
            Ok(metadata) => Ok(if directory {
                metadata.is_dir()
            } else {
                metadata.is_file()
            }),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(format!("检查 Xberg 运行目录失败：{error}")),
        }
    };
    Ok(exists(path, true)? && exists(&path.join("xberg.exe"), false)?)
}

fn discover_bundle(
    executable_directory: &Path,
    installed_directory: impl FnOnce() -> Result<Option<PathBuf>, String>,
) -> Result<Option<PathBuf>, String> {
    let sibling = executable_directory.join("xberg");
    if valid_runtime_directory(&sibling)? {
        return Ok(Some(sibling));
    }
    if let Some(installed) = installed_directory()? {
        let bundled = installed.join("xberg");
        if valid_runtime_directory(&bundled)? {
            return Ok(Some(bundled));
        }
    }
    Ok(None)
}

#[cfg(windows)]
fn installed_directory() -> Result<Option<PathBuf>, String> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
    };
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY,
    };
    let key: Vec<u16> = "Software\\JchTools".encode_utf16().chain(Some(0)).collect();
    let value: Vec<u16> = "InstallDir".encode_utf16().chain(Some(0)).collect();
    let flags = RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY;
    let mut bytes = 0;
    // SAFETY: 键名和值名均以 NUL 结尾，第一次调用只读取所需缓冲区长度。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut bytes,
        )
    };
    if matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        return Err(format!(
            "读取 JchTools 安装目录注册表失败：系统错误 {status}"
        ));
    }
    if bytes == 0 || bytes % 2 != 0 {
        return Err("JchTools 安装目录注册表字符串无效".into());
    }
    let mut buffer = vec![0_u16; bytes as usize / 2];
    // SAFETY: 缓冲区按注册表返回的字节数分配，API 不会写出该容量。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            flags,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &raw mut bytes,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "读取 JchTools 安装目录注册表失败：系统错误 {status}"
        ));
    }
    buffer.truncate(bytes as usize / 2);
    while buffer.last() == Some(&0) {
        buffer.pop();
    }
    if buffer.is_empty() || buffer.contains(&0) {
        return Err("JchTools 安装目录注册表字符串无效".into());
    }
    let path = PathBuf::from(OsString::from_wide(&buffer));
    if !path.is_absolute() {
        return Err("JchTools 安装目录注册表不是绝对路径".into());
    }
    Ok(Some(path))
}

#[cfg(not(windows))]
fn installed_directory() -> Result<Option<PathBuf>, String> {
    Ok(None)
}

/// 优先发现便携/安装目录内的运行时；WindowsApps CLI 再读取安装器登记的目录。
pub fn bundled_directory() -> Result<Option<PathBuf>, String> {
    if let BundleOverride::Isolated(isolated) =
        test_bundle_override(cfg!(any(test, feature = "test-hooks")), |key| {
            std::env::var_os(key)
        })?
    {
        return match isolated {
            Some(path) if valid_runtime_directory(&path)? => Ok(Some(path)),
            _ => Ok(None),
        };
    }
    let executable =
        std::env::current_exe().map_err(|error| format!("定位当前程序失败：{error}"))?;
    let parent = executable.parent().ok_or("无法定位当前程序所在目录")?;
    discover_bundle(parent, installed_directory)
}

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
        Err(e) => {
            tracing::error!(event = "settings_storage_failed", stage = "read_legacy", error_type = ?e.kind(), error_code = ?e.raw_os_error(), "读取旧配置失败");
            Err(format!("读取旧 Xberg 配置失败：{e}"))
        }
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
    observe("load", || {
        let result = load_inner();
        if let Ok(value) = &result {
            tracing::debug!(
                event = "settings_loaded",
                configured = value.is_some(),
                "共享运行目录配置已读取"
            );
        }
        result
    })
}

fn load_inner() -> Result<Option<PathBuf>, String> {
    let root = state_dir()?;
    let db = open(&root)?;
    load_from_db(&root, &db)
}

fn load_from_db(root: &Path, db: &Connection) -> Result<Option<PathBuf>, String> {
    let read = || {
        db.query_row(
            "SELECT value FROM app_settings WHERE key='xberg_directory'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
    };
    if let Some(value) = read().map_err(|e| {
        sqlite_failure("read_directory", &e);
        format!("读取 Xberg 配置失败：{e}")
    })? {
        if value.trim().is_empty() {
            return Err("已保存的 Xberg 配置为空".into());
        }
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err("已保存的 Xberg 目录不是绝对路径，请重新保存".into());
        }
        return Ok(Some(path));
    }
    let Some(value) = read_legacy(root)? else {
        return Ok(None);
    };
    let value = legacy_directory_value(&value)?;
    db.execute(
        "INSERT OR IGNORE INTO app_settings(key,value) VALUES('xberg_directory',?1)",
        [value],
    )
    .map_err(|e| {
        sqlite_failure("migrate_directory", &e);
        format!("迁移 Xberg 配置到 SQLite 失败：{e}")
    })?;
    tracing::info!(
        component = "xberg_settings",
        event = "settings_legacy_migration_written",
        "旧版共享运行目录迁移写入完成"
    );
    read().map(|value| value.map(PathBuf::from)).map_err(|e| {
        sqlite_failure("read_migrated_directory", &e);
        format!("读取迁移后的配置失败：{e}")
    })
}

/// 旧来源记录分别保留，全部功能只读取一个当前来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Custom,
    Downloaded,
    Bundled,
}
impl Source {
    fn key(self) -> &'static str {
        match self {
            Self::Custom => "xberg_custom_directory",
            Self::Downloaded => "xberg_downloaded_directory",
            Self::Bundled => "xberg_bundled_directory",
        }
    }
    fn value(self) -> &'static str {
        match self {
            Self::Custom => "custom",
            Self::Downloaded => "downloaded",
            Self::Bundled => "bundled",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub source: Source,
    pub custom: Option<PathBuf>,
    pub downloaded: Option<PathBuf>,
    pub bundled: Option<PathBuf>,
}

impl Settings {
    pub fn directory(&self) -> Option<&Path> {
        match self.source {
            Source::Custom => self.custom.as_deref(),
            Source::Downloaded => self.downloaded.as_deref(),
            Source::Bundled => self.bundled.as_deref(),
        }
    }
}

/// 兼容旧 SQLite 与文本配置；失效路径仍保留供用户修复。
pub fn settings() -> Result<Settings, String> {
    observe("read_sources", || {
        let result = settings_inner();
        if let Ok(value) = &result {
            tracing::debug!(
                event = "settings_sources_loaded",
                source = value.source.value(),
                custom_present = value.custom.is_some(),
                downloaded_present = value.downloaded.is_some(),
                bundled_present = value.bundled.is_some(),
                "共享运行目录来源已读取"
            );
        }
        result
    })
}

fn settings_inner() -> Result<Settings, String> {
    let root = state_dir()?;
    let db = open(&root)?;
    settings_from_db(&root, &db)
}

fn migrate_legacy_for_settings(root: &Path, db: &Connection) -> Result<Option<PathBuf>, String> {
    let configured: Option<String> = db
        .query_row(
            "SELECT key FROM app_settings WHERE key IN ('xberg_directory','xberg_source') LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("读取旧 Xberg 配置状态失败：{error}"))?;
    if configured.is_some() {
        return Ok(None);
    }
    let Some(value) = read_legacy(root)? else {
        return Ok(None);
    };
    let value = value.trim();
    if !value.is_empty() && Path::new(value).is_absolute() {
        db.execute(
            "INSERT OR IGNORE INTO app_settings(key,value) VALUES('xberg_directory',?1)",
            [value],
        )
        .map_err(|error| format!("迁移旧 Xberg 配置到 SQLite 失败：{error}"))?;
    }
    Ok(Some(PathBuf::from(value)))
}

fn settings_from_db(root: &Path, db: &Connection) -> Result<Settings, String> {
    // 旧文本迁移在读事务外完成；无效路径仍保留，不阻止随包目录发现。
    let legacy = migrate_legacy_for_settings(root, db)?;
    // 来源和所有路径必须来自同一提交，不能混入另一进程刚保存的字段。
    let tx = db.unchecked_transaction().map_err(|e| {
        sqlite_failure("begin_read_sources", &e);
        format!("开始读取 Xberg 来源配置失败：{e}")
    })?;
    let read = |key: &str| -> Result<Option<String>, String> {
        tx.query_row(
            "SELECT value FROM app_settings WHERE key=?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| {
            sqlite_failure("read_sources", &e);
            format!("读取 Xberg 配置失败：{e}")
        })
    };
    let source = match read("xberg_source")?.as_deref() {
        None | Some("custom") => Source::Custom,
        Some("downloaded") => Source::Downloaded,
        Some("bundled") => Source::Bundled,
        Some(_) => return Err("保存的 Xberg 来源无效，请重新选择".into()),
    };
    let legacy_directory = read("xberg_directory")?.map(PathBuf::from).or(legacy);
    let saved = Settings {
        source,
        custom: read(Source::Custom.key())?.map(PathBuf::from).or_else(|| {
            (source == Source::Custom)
                .then_some(legacy_directory)
                .flatten()
        }),
        downloaded: read(Source::Downloaded.key())?.map(PathBuf::from),
        bundled: read(Source::Bundled.key())?.map(PathBuf::from),
    };
    tx.commit().map_err(|e| {
        sqlite_failure("finish_read_sources", &e);
        format!("完成读取 Xberg 来源配置失败：{e}")
    })?;
    Ok(saved)
}

pub fn select(source: Source) -> Result<(), String> {
    let config = settings()?;
    let path = match source {
        Source::Custom => config.custom,
        Source::Downloaded => config.downloaded,
        Source::Bundled => config.bundled,
    }
    .ok_or("该来源尚无已保存的目录")?;
    save_source(source, &path)
}

/// 目录与来源在同一 SQLite 事务内落盘，失败不改写有效配置。
pub fn save_source(source: Source, path: &Path) -> Result<(), String> {
    observe("save_source", || {
        let result = save_source_inner(source, path);
        if result.is_ok() {
            tracing::info!(
                event = "settings_source_saved",
                source = source.value(),
                "共享运行目录来源已保存"
            );
        }
        result
    })
}

fn active_directory_in_transaction(db: &Connection) -> Result<Option<PathBuf>, String> {
    let read = |key: &str| -> Result<Option<String>, String> {
        db.query_row(
            "SELECT value FROM app_settings WHERE key=?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("读取 Xberg 当前来源失败：{error}"))
    };
    let source = match read("xberg_source")?.as_deref() {
        None | Some("custom") => Source::Custom,
        Some("downloaded") => Source::Downloaded,
        Some("bundled") => Source::Bundled,
        Some(_) => return Err("保存的 Xberg 来源无效，未覆盖原配置".into()),
    };
    let value = read(source.key())?;
    let value = if value.is_none() && source == Source::Custom {
        read("xberg_directory")?
    } else {
        value
    };
    Ok(value.map(PathBuf::from))
}

fn save_source_inner(source: Source, path: &Path) -> Result<(), String> {
    save_directory(source, path, false).map(|_| ())
}

fn save_directory(source: Source, path: &Path, preserve_valid: bool) -> Result<PathBuf, String> {
    tracing::debug!(
        event = "settings_save_stage",
        stage = "validate_directory",
        "校验保存的目录"
    );
    if !path.is_absolute() || !path.is_dir() || !path.join("xberg.exe").is_file() {
        return Err("请选择包含 xberg.exe 的有效绝对目录".into());
    }
    let path = std::fs::canonicalize(path).map_err(|e| {
        tracing::error!(event = "settings_storage_failed", stage = "canonicalize_directory", error_type = ?e.kind(), error_code = ?e.raw_os_error(), "解析共享运行目录失败");
        format!("解析 Xberg 目录失败：{e}")
    })?;
    let text = path.to_str().ok_or("Xberg 路径无法编码为 Unicode")?;
    let root = state_dir()?;
    let mut db = open(&root)?;
    let tx = db.transaction().map_err(|e| {
        tracing::error!(event = "settings_storage_failed", stage = "begin_transaction", error_type = ?e.sqlite_error_code(), "开始配置保存事务失败");
        e.to_string()
    })?;
    let current = active_directory_in_transaction(&tx)?;
    if preserve_valid {
        if let Some(current) = current {
            if valid_runtime_directory(&current)? {
                return Ok(current);
            }
        }
    }
    // 切换来源前保留旧版唯一目录；迁移失败时整个事务回滚。
    tx.execute("INSERT OR IGNORE INTO app_settings(key,value) SELECT 'xberg_custom_directory',value FROM app_settings WHERE key='xberg_directory' AND (NOT EXISTS(SELECT 1 FROM app_settings WHERE key='xberg_source') OR EXISTS(SELECT 1 FROM app_settings WHERE key='xberg_source' AND value='custom'))", [])
        .map_err(|e| { sqlite_failure("migrate_legacy_source", &e); e.to_string() })?;
    if source != Source::Custom {
        let migrate_text: bool = tx
            .query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM app_settings WHERE key IN ('xberg_source','xberg_custom_directory'))",
                [],
                |row| row.get(0),
            )
            .map_err(|e| { sqlite_failure("read_legacy_source", &e); format!("读取旧 Xberg 来源配置失败：{e}") })?;
        if migrate_text {
            if let Some(value) = read_legacy(&root)? {
                let value = value.trim();
                tx.execute(
                    "INSERT INTO app_settings(key,value) VALUES('xberg_custom_directory',?1)",
                    [value],
                )
                .map_err(|e| {
                    sqlite_failure("migrate_custom_directory", &e);
                    format!("迁移旧 Xberg 用户目录失败：{e}")
                })?;
            }
        }
    }
    for (key, value) in [
        (source.key(), text),
        ("xberg_source", source.value()),
        ("xberg_directory", text),
    ] {
        tx.execute("INSERT INTO app_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key, value])
            .map_err(|e| {
                tracing::error!(event = "settings_storage_failed", stage = "write_settings", error_type = ?e.sqlite_error_code(), "写入配置失败");
                format!("保存 Xberg 配置失败：{e}")
            })?;
    }
    tx.commit().map_err(|e| {
        tracing::error!(event = "settings_storage_failed", stage = "commit_transaction", error_type = ?e.sqlite_error_code(), "提交配置事务失败");
        format!("提交 Xberg 配置失败：{e}")
    })?;
    Ok(path)
}

pub fn save(path: &Path) -> Result<(), String> {
    save_source(Source::Custom, path)
}

/// 有效旧来源优先；仅在其失效或不存在时建立随包默认配置。
pub fn effective_directory() -> Result<Option<PathBuf>, String> {
    let config = settings()?;
    if let Some(path) = config.directory() {
        if valid_runtime_directory(path)? {
            return Ok(Some(path.to_path_buf()));
        }
    }
    let Some(bundled) = bundled_directory()? else {
        return Ok(None);
    };
    observe("save_source", || {
        save_directory(Source::Bundled, &bundled, true)
    })
    .map(Some)
}

/// 在代理确认安全切换后保存随包来源；旧用户/下载来源不被删除。
pub fn save_bundled_directory(path: &Path) -> Result<(), String> {
    save_source(Source::Bundled, path)
}

pub fn required() -> Result<PathBuf, String> {
    effective_directory()?.ok_or_else(|| {
        "未找到有效的 Xberg 运行目录：请在设置页检查状态，并从安装包修复随包资产".into()
    })
}

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
mod tests {
    use super::test_directory_override;
    use std::ffi::OsString;
    use std::path::PathBuf;

    // 覆盖 CA-05：后台可选择不等待争用，旧调用方仍保留五秒等待策略。
    #[test]
    fn caller_selected_busy_timeout_preserves_default_and_allows_fail_fast() {
        let root = tempfile::tempdir().unwrap();
        let default = super::open(root.path()).unwrap();
        let timeout: u64 = default
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 5000);
        let immediate = super::open_with_timeout(root.path(), std::time::Duration::ZERO).unwrap();
        let timeout: u64 = immediate
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 0);
        drop(immediate);
        default.execute_batch("BEGIN EXCLUSIVE").unwrap();
        assert!(super::open_with_timeout(root.path(), std::time::Duration::ZERO).is_err());
        default.execute_batch("ROLLBACK").unwrap();
        assert!(super::open_with_timeout(root.path(), std::time::Duration::ZERO).is_ok());
    }

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

    #[test]
    fn bundled_discovery_policy_isolates_real_installations() {
        assert_eq!(
            super::test_bundle_override(false, |_| panic!("生产策略不得读取测试环境变量")),
            Ok(super::BundleOverride::Unchanged)
        );
        for key in [
            "JCHTOOLS_TEST_STATE_DIR",
            "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT",
            "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT",
        ] {
            assert_eq!(
                super::test_bundle_override(true, |name| {
                    (name == key).then(|| OsString::from("C:/isolated-state"))
                }),
                Ok(super::BundleOverride::Isolated(None)),
                "隔离状态必须禁用真实程序目录和安装注册表"
            );
        }
        let bundled = PathBuf::from("C:/isolated-bundle");
        assert_eq!(
            super::test_bundle_override(true, |key| {
                (key == "JCHTOOLS_TEST_BUNDLED_XBERG_DIR")
                    .then(|| bundled.clone().into_os_string())
            }),
            Ok(super::BundleOverride::Isolated(Some(bundled)))
        );
        assert!(super::test_bundle_override(true, |key| {
            (key == "JCHTOOLS_TEST_BUNDLED_XBERG_DIR")
                .then(|| OsString::from("relative-bundle"))
        }).is_err());
    }

    #[test]
    fn sibling_bundle_precedes_installed_registry_directory() {
        let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().parent().unwrap();
        let workspace = repository.join(".tmp/foundation-bundle-discovery");
        assert!(std::process::Command::new("python")
            .arg(repository.join("scripts/make_tmp.py"))
            .args(["workspace", "--destination"])
            .arg(&workspace)
            .current_dir(repository)
            .status().unwrap().success());
        let root = tempfile::tempdir_in(workspace).unwrap();
        let portable = root.path().join("portable");
        let installed = root.path().join("installed");
        for directory in [&portable, &installed] {
            std::fs::create_dir_all(directory.join("xberg")).unwrap();
            std::fs::write(directory.join("xberg/xberg.exe"), b"discovery fixture").unwrap();
        }
        assert_eq!(
            super::discover_bundle(&portable, || panic!("有效的同目录随包版本不得读取注册表")).unwrap(),
            Some(portable.join("xberg"))
        );
        std::fs::remove_file(portable.join("xberg/xberg.exe")).unwrap();
        assert_eq!(
            super::discover_bundle(&portable, || Ok(Some(installed.clone()))).unwrap(),
            Some(installed.join("xberg"))
        );
        assert!(super::discover_bundle(&portable, || Err("注册表读取失败".into())).is_err());
        assert!(super::discover_bundle(&portable, || Ok(None)).unwrap().is_none());
    }

    // 覆盖 XB-18/XB-21：另一进程在来源与路径读取之间提交时，只能返回同一提交快照。
    #[test]
    fn source_paths_are_read_from_one_committed_snapshot() {
        use rusqlite::{ffi, Connection};
        use std::os::raw::{c_char, c_int, c_void};

        struct Switch<'a> {
            writer: &'a Connection,
            selects: usize,
            error: Option<rusqlite::Error>,
        }
        unsafe extern "C" fn switch_before_custom_read(
            context: *mut c_void,
            action: c_int,
            _: *const c_char,
            _: *const c_char,
            _: *const c_char,
            _: *const c_char,
        ) -> c_int {
            // 安全：回调仅在本线程 prepare 内同步调用，context 存活至显式注销。
            let switch = unsafe { &mut *context.cast::<Switch<'_>>() };
            if action == ffi::SQLITE_SELECT {
                switch.selects += 1;
                if switch.selects == 3 {
                    switch.error = switch
                        .writer
                        .execute_batch(
                            "BEGIN IMMEDIATE;
                             UPDATE app_settings SET value='downloaded' WHERE key='xberg_source';
                             UPDATE app_settings SET value='C:/new-downloaded'
                                 WHERE key IN ('xberg_directory','xberg_downloaded_directory');
                             COMMIT;",
                        )
                        .err();
                    if switch.error.is_some() {
                        return ffi::SQLITE_DENY;
                    }
                }
            }
            ffi::SQLITE_OK
        }

        let root = tempfile::tempdir().unwrap();
        let reader = super::open(root.path()).unwrap();
        reader
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 INSERT INTO app_settings(key,value) VALUES
                     ('xberg_source','custom'),
                     ('xberg_directory','C:/old-custom'),
                     ('xberg_custom_directory','C:/old-custom'),
                     ('xberg_downloaded_directory','C:/old-downloaded');",
            )
            .unwrap();
        let writer = super::open(root.path()).unwrap();
        let mut switch = Switch {
            writer: &writer,
            selects: 0,
            error: None,
        };
        // 使用原生 authorizer 仅控制提交点，不额外开启生产 rusqlite 特性。
        let installed = unsafe {
            ffi::sqlite3_set_authorizer(
                reader.handle(),
                Some(switch_before_custom_read),
                (&mut switch as *mut Switch<'_>).cast(),
            )
        };
        assert_eq!(installed, ffi::SQLITE_OK);
        let saved = super::settings_from_db(root.path(), &reader);
        let removed = unsafe {
            ffi::sqlite3_set_authorizer(reader.handle(), None, std::ptr::null_mut())
        };
        assert_eq!(removed, ffi::SQLITE_OK);
        assert!(switch.selects >= 3, "必须经过指定的跨提交读取边界");
        assert!(switch.error.is_none(), "第二连接必须成功提交来源切换");
        let saved = saved.unwrap();
        assert_eq!(saved.source, super::Source::Custom);
        assert_eq!(saved.custom, Some(PathBuf::from("C:/old-custom")));
        assert_eq!(
            saved.downloaded,
            Some(PathBuf::from("C:/old-downloaded")),
            "不能混入来源切换后才提交的下载路径"
        );
        let latest = super::settings_from_db(root.path(), &reader).unwrap();
        assert_eq!(latest.source, super::Source::Downloaded);
        assert_eq!(
            latest.downloaded,
            Some(PathBuf::from("C:/new-downloaded"))
        );
    }
}
