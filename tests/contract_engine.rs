#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    future_incompatible,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]

//! engine.rs 合同回归：Git 整树排除（H-06/S-04）、整理收尾实空清理（H-05/C-07/C-09）、
//! 确认框清点与扫描同口径（X-02）、缓存降级（C-13）与状态口径（C-10/C-11）。
// 测试代码允许 unwrap/expect（与 tests/core.rs 的集成测试惯例一致）：断言失败即测试失败。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]
use jchtools_directory_organizer::engine;
use jchtools_file_core::config::{Config, DeleteMode};
use jchtools_file_core::control::Context;
use jchtools_file_core::db::Database;

use std::{
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

struct Fixture {
    /// 只用于持有临时目录生命周期（测试经 root/state 访问内容）。
    _temp: TempDir,
    root: PathBuf,
    state: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let state = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        Self {
            _temp: temp,
            root,
            state,
        }
    }
    fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }
    fn dir(&self, rel: &str) -> PathBuf {
        let path = self.root.join(rel);
        fs::create_dir_all(&path).unwrap();
        path
    }
    /// 目录形式（普通 clone）的 Git 标记。
    fn git_marker_dir(&self, rel: &str) -> PathBuf {
        let proj = self.dir(rel);
        fs::create_dir_all(proj.join(".git")).unwrap();
        fs::write(proj.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        proj
    }
    /// 文件形式（worktree / submodule）的 Git 标记。
    fn git_marker_file(&self, rel: &str) -> PathBuf {
        let proj = self.dir(rel);
        fs::write(proj.join(".git"), b"gitdir: ../.git/modules/x\n").unwrap();
        proj
    }
    fn plan(&self, config: Config) -> jchtools_file_core::task::TaskResult {
        engine::prepare_at(&self.root, config, Context::default(), &self.state).unwrap()
    }
}
fn base() -> Config {
    Config {
        global_delete: DeleteMode::Permanent,
        // 归类按 C-05 恒开启（大类一级），不再有 classify 开关。
        dedup_other_names: true,
        ..Config::default()
    }
}
fn status(directory: &Path) -> String {
    Database::open_existing(directory)
        .unwrap()
        .get("status")
        .unwrap()
}
fn events(directory: &Path) -> String {
    let db = Database::open_existing(directory).unwrap();
    let mut statement = db
        .conn
        .prepare("SELECT phase,source,target,result,reason FROM events")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok(format!(
                "[{}] {} | {} → {} | {}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(4)?
            ))
        })
        .unwrap();
    let values: Result<Vec<String>, _> = rows.collect();
    values.unwrap().join("\n")
}
#[cfg(windows)]
fn set_hidden(path: &Path, hidden: bool) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, INVALID_FILE_ATTRIBUTES,
    };
    let wide = |p: &Path| {
        let mut value: Vec<u16> = p.as_os_str().encode_wide().collect();
        value.push(0);
        value
    };
    let raw = wide(path);
    // SAFETY: raw 是调用期间有效且以 NUL 结尾的 UTF-16 路径，API 不保留指针。
    let attrs = unsafe { GetFileAttributesW(raw.as_ptr()) };
    assert_ne!(attrs, INVALID_FILE_ATTRIBUTES, "测试前置：读取属性失败");
    let next = if hidden {
        attrs | FILE_ATTRIBUTE_HIDDEN
    } else {
        attrs & !FILE_ATTRIBUTE_HIDDEN
    };
    assert_ne!(
        // SAFETY: raw 生命周期覆盖调用，next 由已读取属性仅切换隐藏位得到。
        unsafe { SetFileAttributesW(raw.as_ptr(), next) },
        0,
        "测试前置：写入隐藏属性失败"
    );
}

// 测试按主题拆分为子模块；本文件保留共享夹具、辅助函数与模块声明。
#[path = "contract_engine/cleanup_and_scan.rs"]
mod cleanup_and_scan;
#[path = "contract_engine/extraction_workspaces.rs"]
mod extraction_workspaces;
#[path = "contract_engine/git_boundaries.rs"]
mod git_boundaries;
