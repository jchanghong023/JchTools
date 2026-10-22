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

//! These tests mutate only tempfile fixtures. 删除一律为永久删除（S-02），不再有回收站注入点。
// 测试代码允许 unwrap/expect 与短名（f/p/q 夹具惯例）：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]
use jchtools_directory_organizer::engine;
use jchtools_file_core::config::*;
use jchtools_file_core::control::{Context, Control};
use jchtools_file_core::db::Database;
use jchtools_file_core::fsutil;
use jchtools_file_core::hashing;
use jchtools_file_core::model::{ActionKind, FileRecord, Snapshot};
use jchtools_file_core::platform::{self, DeleteResult};
use jchtools_file_core::rules;
use rusqlite::params;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    root: PathBuf,
    state: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let state = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        Self { temp, root, state }
    }
    fn write(&self, name: &str, bytes: &[u8], time: i64) -> PathBuf {
        let p = self.root.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        filetime::set_file_mtime(&p, filetime::FileTime::from_unix_time(time, 0)).unwrap();
        p
    }
    fn plan(&self, cfg: Config) -> jchtools_file_core::task::TaskResult {
        engine::prepare_at(&self.root, cfg, Context::default(), &self.state).unwrap()
    }
    fn apply(task: &jchtools_file_core::task::TaskResult) -> jchtools_file_core::task::TaskResult {
        engine::apply(&task.directory, Context::default()).unwrap()
    }
}

fn base() -> Config {
    Config {
        global_delete: DeleteMode::Permanent,
        // 这些用例考的是记账 / 归类 / 排序 / 幂等等其它行为，需要「内容相同的不同名文件」
        // 也进入去重，故显式开启第三类；C-02 的默认值（不同名关闭）由
        // `different_names_not_deduped_by_default` 单独锚定。
        // 归类按 C-05 恒开启（大类一级），不再有 classify 开关。
        dedup_other_names: true,
        clean_copy_name: false,
        ..Config::default()
    }
}
/// 在根下按文件名递归查找（C-05 归类恒移动文件，断言“内容仍在”需按名找）。
fn exists_somewhere(root: &Path, name: &str) -> bool {
    fn walk(dir: &Path, name: &str) -> bool {
        let entries = fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("无法检查夹具目录 {}：{error}", dir.display()));
        for entry in entries {
            let entry = entry.expect("无法读取夹具目录项");
            let path = entry.path();
            let file_type = entry.file_type().expect("无法检查夹具目录项类型");
            if file_type.is_dir() {
                if walk(&path, name) {
                    return true;
                }
            } else if file_type.is_file() && entry.file_name().to_string_lossy() == name {
                return true;
            }
        }
        false
    }
    walk(root, name)
}

// 测试按主题拆分为子模块；本文件保留共享夹具、辅助函数与模块声明。
#[path = "core/apply_safety.rs"]
mod apply_safety;
#[path = "core/cleanup_rules.rs"]
mod cleanup_rules;
#[path = "core/config_contract.rs"]
mod config_contract;
#[path = "core/config_paths.rs"]
mod config_paths;
#[path = "core/dedup_plan.rs"]
mod dedup_plan;
#[path = "core/git_tree.rs"]
mod git_tree;
#[path = "core/lock_cancel.rs"]
mod lock_cancel;
#[path = "core/plan_actions.rs"]
mod plan_actions;
#[path = "core/review_regressions.rs"]
mod review_regressions;
#[path = "core/scan_classify.rs"]
mod scan_classify;
#[path = "core/signature_fix.rs"]
mod signature_fix;
