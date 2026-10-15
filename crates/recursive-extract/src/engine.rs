use crate::archive::{self, SevenZip};
use anyhow::{bail, Result};
use jchtools_file_core::{
    config::{self, Config},
    control::Context as TaskContext,
    fsutil, rules,
    scan::{ensure_git_boundary_checked, scan, special_prefixes, ScopeFilter},
    task::{
        create_task_db, observe, preflight_root, record_task_failure, resolve_protection,
        TaskResult, QUARANTINE_DIR_NAME,
    },
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

#[cfg(test)]
use jchtools_file_core::{db::Database, model::Summary, task::Job};

/// 「递归解压」工具的一段式执行（X-02）：确认后连续运行到结束——扫描登记压缩包、
/// 就地解压（X-03）、成功原包按处置策略处理（X-05 默认永久删除）、失败原包移入
/// 「解压失败」子目录（X-06）。没有计划审核环节，也不生成整理计划。
pub fn extract_run(root: &Path, config: Config, context: TaskContext) -> Result<TaskResult> {
    extract_run_at(root, config, context, &config::state_dir()?, None)
}
/// 测试注入变体：独立状态目录 + 显式引擎路径（与 prepare_at 同口径，E-02 不向用户提供）。
pub fn extract_run_at(
    root: &Path,
    config: Config,
    context: TaskContext,
    state: &Path,
    engine_path: Option<&Path>,
) -> Result<TaskResult> {
    let control = context.control.clone();
    observe("extract_preflight", Some(&control), |stage| {
        *stage = "protection";
        let protected = resolve_protection()?;
        extract_run_with(
            root,
            config,
            context,
            state,
            engine_path,
            protected.as_deref(),
        )
    })
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "archive_extract", skip_all)
)]
fn extract_run_with(
    root: &Path,
    config: Config,
    context: TaskContext,
    state: &Path,
    engine_path: Option<&Path>,
    protected: Option<&Path>,
) -> Result<TaskResult> {
    let control = context.control.clone();
    observe("archive_extract", Some(&control), |stage| {
        let (root, protected) = preflight_root(root, &config, protected, true)?;
        *stage = "task_database";
        let (_guard, mut job) = create_task_db(state, &root, config, context, "executing")?;
        let directory = job.db.directory.clone();
        let task_span = tracing::info_span!("file_task", task_id = %directory.file_name().unwrap_or_default().to_string_lossy());
        let _task_entered = task_span.enter();
        tracing::info!(
            event = "task_created",
            state = "executing",
            "解压任务已建立"
        );
        let result = (|| {
            *stage = "scan";
            scan(&mut job, true, state, protected.as_deref())?;
            let count: i64 = job.db.conn.query_row(
                "SELECT COUNT(*) FROM archives WHERE state='pending'",
                [],
                |r| r.get(0),
            )?;
            if count > 0 {
                *stage = "extract_queue";
                archive::extract_queued(&mut job, || match engine_path {
                    Some(path) => SevenZip::with_executable(path),
                    None => SevenZip::from_bundle(),
                })?;
            } else {
                job.log(
                    "解压",
                    "",
                    "",
                    "提示",
                    "所选目录（按当前扫描范围）没有发现压缩包；未做任何改动",
                    0,
                )?;
            }
            *stage = "finalize_database";
            job.db.set("summary", &job.summary)?;
            job.log(
                "任务",
                "",
                "",
                "完成",
                &format!(
                    "解压结束：成功 {} 包（成功原包及分卷已永久删除）；未完全解开 {} 包；已移入「{}」 {} 个原包或分卷文件；未隔离或部分隔离的源项原地保留，详情见失败列表与日志",
                    job.summary.archives_ok,
                    job.summary.archives_failed,
                    QUARANTINE_DIR_NAME,
                    job.summary.archives_quarantined
            ),
                0,
            )?;
            #[cfg(feature = "perf-tracing")]
            jchtools_foundation::perf::extract_done(
                job.summary.scanned,
                job.summary.archives_ok,
                job.summary.archives_failed,
            );
            tracing::info!(
                event = "archive_extract_summary",
                scanned = job.summary.scanned,
                ok = job.summary.archives_ok,
                failed = job.summary.archives_failed,
                "递归解压结束"
            );
            Ok(TaskResult {
                directory: directory.clone(),
                summary: job.summary.clone(),
            })
        })();
        if let Err(error) = &result {
            record_task_failure(&mut job, error);
        } else {
            job.db.set("status", &"finished")?;
        }
        result
    })
}
/// 确认框用的压缩包计数（X-02）：只读快速清点，与正式扫描同一套过滤口径
/// （递归/隐藏/系统/排除规则/跳过「解压失败」/Git 整树排除/状态与程序目录剪枝，X-07）。
/// 失败即报错，不回退猜测值。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "count_archives", skip_all)
)]
pub fn count_archives(root: &Path, config: &Config) -> Result<u64> {
    observe("count_archives", None, |stage| {
        *stage = "protection";
        let protected = resolve_protection()?;
        *stage = "scan";
        let result = count_archives_with(root, config, protected.as_deref());
        if let Ok(count) = &result {
            tracing::debug!(event = "archive_count_result", count, "压缩包清点完成");
        }
        result
    })
}
/// S-05 受保护目录参数化变体（测试注入合成受保护目录），其余行为与 [`count_archives`] 一致。
fn count_archives_with(root: &Path, config: &Config, protected: Option<&Path>) -> Result<u64> {
    let (root, protected_prune) = preflight_root(root, config, protected, true)?;
    let excluded = rules::build_exclusions(&config.exclusions)?;
    // 与 scan 同源的特殊目录剪枝口径。状态目录此时可能尚不存在（首次运行先弹
    // 确认再建目录）：canonicalize 失败视为无重叠，不阻止清点。
    let state = fs::canonicalize(jchtools_file_core::config::state_dir()?).ok();
    let executable_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().and_then(|d| fs::canonicalize(d).ok()));
    let (state_prefix, exe_prefix, root_under_special) =
        special_prefixes(&root, state.as_deref(), executable_dir.as_deref());
    if root_under_special {
        // 选定根位于状态目录/程序目录内：扫描将整树剪枝，清点必须同为 0。
        return Ok(0);
    }
    // S-05：根包含 Windows 系统目录时该子树整树剪枝，清点与扫描同一口径。
    let protected_prefix = protected_prune
        .as_ref()
        .and_then(|dir| fsutil::relative_string(&root, dir).ok());
    // 剪枝口径与扫描共用同一个实现（避免两处各自漂移成不同范围）。
    let scope_filter = ScopeFilter {
        excluded: &excluded,
        include_hidden: config.include_hidden,
        include_system: config.include_system,
        quarantine: Some(QUARANTINE_DIR_NAME),
        state_prefix,
        exe_prefix,
        protected_prefix,
        root_under_special,
    };
    // X-10：缺主包的老式族尾卷组按「残缺但可归组的卷集计一包」参与清点。
    // 归组依赖同目录兄弟关系，先按目录收集文件名再统一计数，与扫描的兄弟判定同源。
    let mut names_by_dir: HashMap<PathBuf, (Vec<String>, HashSet<String>)> = HashMap::new();
    let mut boundary_error = None;
    let mut filter_error = None;
    for entry in walkdir::WalkDir::new(&root)
        .follow_links(false)
        .min_depth(1)
        .max_depth(if config.recursive { usize::MAX } else { 1 })
        .into_iter()
        .filter_entry(|entry| {
            let meta = match fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta,
                Err(error) => {
                    filter_error.get_or_insert_with(|| {
                        format!("无法读取目录项元数据 {}：{error}", entry.path().display())
                    });
                    return false;
                }
            };
            let rel = match fsutil::relative_string(&root, entry.path()) {
                Ok(rel) => rel,
                Err(error) => {
                    filter_error.get_or_insert_with(|| {
                        format!(
                            "无法确定目录项相对路径 {}：{error:#}",
                            entry.path().display()
                        )
                    });
                    return false;
                }
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // 选定根目录自身不参与隐藏/系统/名称等判定——扫描同口径：筛选只作用于
            // 根目录的子项，隐藏的选定根目录不会把整棵树剪掉（否则确认框报 0，
            // 正式扫描却能找到包）。
            // 范围剪枝先于 Git 边界判定（与扫描同口径：walk_dir 只下钻未剪枝目录），
            // 被范围规则剪掉的目录不做边界判定，用户排除的不可读目录不得让清点失败。
            let in_scope = rel.is_empty() || !scope_filter.prunes(&rel, &name, &meta);
            if !in_scope {
                return false;
            }
            // H-06：目录直接含 .git（目录或文件）时整树排除，识别后不遍历内部。
            // 边界判定失败时必须中止清点，不可将未知目录当作已排除。
            if entry.file_type().is_dir() {
                match fsutil::is_git_root(entry.path()) {
                    Ok(true) => return false,
                    Ok(false) => {}
                    Err(error) => {
                        boundary_error.get_or_insert_with(|| {
                            format!("{}：{error:#}", entry.path().display())
                        });
                        return false;
                    }
                }
                // X-08：只跳过带有效所有权标记的实际工作区，普通同名用户目录仍处理。
                match fsutil::is_extraction_workspace(entry.path()) {
                    Ok(true) => return false,
                    Ok(false) => {}
                    Err(error) => {
                        filter_error.get_or_insert_with(|| {
                            format!("无法检查解压工作区 {}：{error:#}", entry.path().display())
                        });
                        return false;
                    }
                }
            }
            true
        })
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str() else {
            bail!(
                "无法完成确认框清点：文件名不能无损表示为 UTF-8：{}",
                entry.path().display()
            );
        };
        let parent = entry
            .path()
            .parent()
            .map_or_else(|| root.clone(), Path::to_path_buf);
        let (names, entry_groups) = names_by_dir.entry(parent).or_default();
        // 入口键必须使用原名：Unicode 小写转换会破坏 Windows ordinal 主干比较。
        if let Some(key) = rules::archive_entry_key(name) {
            entry_groups.insert(key);
        }
        names.push(name.to_ascii_lowercase());
    }
    ensure_git_boundary_checked(boundary_error.as_deref())?;
    if let Some(error) = filter_error {
        bail!("无法完成确认框清点，目录项检查失败：{error}");
    }
    let mut count = 0u64;
    for (names, entry_groups) in names_by_dir.values() {
        count += entry_groups.len() as u64;
        count += rules::count_tail_only_old_style_groups(names);
        // X-10 缺入口的残缺组（part rar 缺首卷 / 数字尾卷缺 .001）同样按
        // 「残缺但可归组的卷集计一包」参与清点，与扫描入队同口径。
        count += rules::part_rar_missing_first_groups(names).len() as u64;
        count += rules::numbered_volume_missing_entry_groups(names).len() as u64;
    }
    Ok(count)
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
mod lock_tests {
    use super::*;

    fn ambiguous_volume_entries_fixture() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        for name in [
            "data.zip.000",
            "data.zip.001",
            "data.zip",
            "project.part1.rar",
            "project.part01.rar",
            "project.part001.rar",
            "a/data.zip.001",
            "b/data.zip.001",
            "unsupported.iso.000",
            "unsupported.iso.001",
        ] {
            fs::write(root.join(name), name.as_bytes()).unwrap();
        }
        (temp, root)
    }

    // 覆盖 X-02/X-10：同目录歧义入口同族一包，不合并不同目录或独立单包。
    #[test]
    fn ambiguous_volume_entries_count_once() {
        let (_temp, root) = ambiguous_volume_entries_fixture();
        assert_eq!(
            count_archives_with(&root, &Config::default(), None).unwrap(),
            5,
            "数字起始歧义一组、part 首卷宽度歧义一组、独立 ZIP 与两目录各一包"
        );
    }

    #[test]
    fn ambiguous_volume_entries_enqueue_once() {
        let (temp, root) = ambiguous_volume_entries_fixture();
        let state = temp.path().join("state");
        let db = Database::create(&state).unwrap();
        let mut job = Job {
            root: fs::canonicalize(root).unwrap(),
            config: Config::default(),
            context: TaskContext::default(),
            db,
            summary: Summary::default(),
        };
        scan(&mut job, true, &state, None).unwrap();
        let queued: i64 = job
            .db
            .conn
            .query_row("SELECT count(*) FROM archives", [], |row| row.get(0))
            .unwrap();
        assert_eq!(queued, 5, "扫描入队必须与确认框同族一包口径一致");
        assert_eq!(job.summary.errors, 0);
        assert_eq!(job.summary.scanned, 10, "归组不遗漏普通文件扫描记录");
    }

    fn ordinal_volume_tail_fixture() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        for name in [
            "İ.zip.002",
            "i\u{0307}.zip.003",
            "İ.part02.rar",
            "i\u{0307}.part03.rar",
        ] {
            fs::write(root.join(name), name.as_bytes()).unwrap();
        }
        (temp, root)
    }

    // 覆盖 X-10：Unicode lowercase 的展开结果不能合并 Windows ordinal 不同主干。
    #[test]
    fn ordinal_volume_tail_count() {
        let (_temp, root) = ordinal_volume_tail_fixture();
        assert_eq!(
            count_archives_with(&root, &Config::default(), None).unwrap(),
            4,
            "İ 与 i 加组合点不是相同主干，各自数字族与 part 族共四个残缺组"
        );
    }

    #[test]
    fn ordinal_volume_tail_enqueue() {
        let (temp, root) = ordinal_volume_tail_fixture();
        let state = temp.path().join("state");
        let db = Database::create(&state).unwrap();
        let mut job = Job {
            root: fs::canonicalize(root).unwrap(),
            config: Config::default(),
            context: TaskContext::default(),
            db,
            summary: Summary::default(),
        };
        scan(&mut job, true, &state, None).unwrap();
        let queued: i64 = job
            .db
            .conn
            .query_row("SELECT count(*) FROM archives", [], |row| row.get(0))
            .unwrap();
        assert_eq!(queued, 4, "不同 Unicode 主干的四个残缺卷族都必须单独入队");
        assert_eq!(job.summary.errors, 0);
        assert_eq!(job.summary.scanned, 4);
    }
    fn r01_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let state = temp.path().join("state");
        (temp, root, state)
    }
    // 覆盖 X-06/U-10：缺首卷预检不启动引擎；容器占用时摘要不虚报隔离。
    #[test]
    fn r01_extraction_summary_reports_failed_isolation() {
        let (_temp, root, state) = r01_fixture();
        fs::write(root.join("bad.part02.rar"), b"missing-first-volume").unwrap();
        fs::write(root.join("解压失败"), b"container occupied").unwrap();
        let task = extract_run_at(&root, Config::default(), TaskContext::default(), &state, None).unwrap();
        assert_eq!(task.summary.archives_failed, 1);
        assert_eq!(task.summary.archives_quarantined, 0);
        let db = Database::open_existing(&task.directory).unwrap();
        let completion: String = db.conn.query_row(
            "SELECT reason FROM events WHERE phase='任务' AND result='完成' ORDER BY id DESC LIMIT 1",
            [], |row| row.get(0),
        ).unwrap();
        assert!(!completion.contains("未完全解开并移入"), "{completion}");
        assert!(completion.contains("已移入") && completion.contains("0 个"), "{completion}");
        assert!(completion.contains("未隔离") && completion.contains("日志"), "{completion}");
        assert_eq!(fs::read(root.join("bad.part02.rar")).unwrap(), b"missing-first-volume");
        assert_eq!(fs::read(root.join("解压失败")).unwrap(), b"container occupied");
    }
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
mod s05_scope_tests {
    use super::*;
    /// S-05 合成受保护子树夹具：root/WinFake 内含「系统样」文件，root 内含普通文件。
    /// 不用真实系统目录做破坏性实验；返回（临时目录、根、受保护目录的规范化路径）。
    fn synthetic_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir_all(root.join("WinFake/System32")).unwrap();
        fs::write(root.join("WinFake/System32/a.dll"), b"sys").unwrap();
        fs::write(root.join("normal.txt"), b"n").unwrap();
        let protected = fs::canonicalize(root.join("WinFake")).unwrap();
        (temp, root, protected)
    }
    // 覆盖 S-05（确认框清点与扫描同口径：受保护子树内的压缩包不计入）
    #[test]
    fn count_archives_excludes_synthetic_protected_subtree() {
        let (_temp, root, protected) = synthetic_fixture();
        fs::write(root.join("WinFake/inner.zip"), b"zip").unwrap();
        fs::write(root.join("y.zip"), b"zip").unwrap();
        let count = count_archives_with(&root, &Config::default(), Some(&protected)).unwrap();
        assert_eq!(count, 1, "受保护子树内的压缩包不计入清点");
        assert_eq!(
            count_archives_with(&root, &Config::default(), None).unwrap(),
            2,
            "对照组：未注入受保护目录时两包都计入"
        );
    }

    /// 用 icacls 拒绝 Everyone 的列目录权构造 read_dir 被拒目录：deny ACE 优先于
    /// allow，普通权限即可复现；结束后移除该 ACE 以便临时目录清理。
    #[cfg(windows)] // 平台门禁原因：icacls 与 deny ACE 仅 Windows 存在
    fn set_dir_list_denied(path: &Path, denied: bool) {
        let mut command = std::process::Command::new("icacls");
        command.arg(path);
        if denied {
            command.args(["/deny", "*S-1-1-0:(RD)"]);
        } else {
            command.args(["/remove:d", "*S-1-1-0"]);
        }
        let output = command.output().expect("icacls 应为 Windows 内置命令");
        assert!(
            output.status.success(),
            "icacls 执行失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // 覆盖 X-03/H-06（清点与扫描同口径：被范围规则剪枝的目录不做 Git 边界判定，
    // 用户排除的不可读目录不得让清点失败；范围内不可读目录仍按 H-06 中止）
    #[cfg(windows)] // 平台门禁原因：icacls 与 deny ACE 仅 Windows 存在
    #[test]
    fn count_skips_boundary_check_for_scope_pruned_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir_all(root.join("denied")).unwrap();
        fs::write(root.join("a.zip"), b"zip").unwrap();
        let root = fs::canonicalize(root).unwrap();
        set_dir_list_denied(&root.join("denied"), true);

        // 范围内的不可读目录：边界无法判定必须中止清点（有意 fail-closed，与扫描一致）。
        let error = count_archives_with(&root, &Config::default(), None).unwrap_err();
        assert!(error.to_string().contains("Git 边界判定"), "{error:#}");

        // 同一目录被用户排除规则剪枝后：正式扫描只对未剪枝目录做边界判定
        // （walk_dir 仅下钻未剪枝目录），清点必须同口径成功而不是拒绝启动。
        let config = Config {
            exclusions: "denied".into(),
            ..Config::default()
        };
        assert_eq!(
            count_archives_with(&root, &config, None).unwrap(),
            1,
            "被范围规则剪枝的不可读目录不得让清点失败"
        );

        set_dir_list_denied(&root.join("denied"), false);
    }
}
