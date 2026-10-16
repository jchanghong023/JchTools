#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! P3-02 / R-03 / S-01：合法深链的真实扫描不能因入库回放递归耗尽调用线程栈。
//! 枚举仍使用产品自己的 IO 池；小栈仅施加到隔离子进程的 scan 调用线程。
use jchtools_file_core::{
    config::Config,
    control::Context,
    scan::scan,
    task::{create_task_db, Job},
};
use std::{
    fs,
    io::{self, Cursor},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::Ordering,
    time::Duration,
};

const CHILD_ROOT: &str = "JCHTOOLS_REVIEW_SCAN_DEPTH_CHILD_ROOT";
const DEPTH: usize = 1024;
const SMALL_STACK: usize = 256 * 1024;
const REPLAY_PROBE: &str = "review-scan-replay-entry";

fn empty_zip() -> Vec<u8> {
    zip::ZipWriter::new(Cursor::new(Vec::new()))
        .finish()
        .unwrap()
        .into_inner()
}

// 不让 tempfile 的递归清理接触深链；即使建夹具中途 panic 或子进程栈溢出，
// 父进程也只按自己成功创建的深度逐层删除。状态库位于深链之外。
struct DeepFixture {
    directory: Option<tempfile::TempDir>,
    root: PathBuf,
    deepest: PathBuf,
    created_depth: usize,
}

impl DeepFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("data");
        fs::create_dir(&root).unwrap();
        // 扩展路径前缀使这条合法深链不受传统 MAX_PATH 限制。
        let root = fs::canonicalize(root).unwrap();
        let mut fixture = Self {
            directory: Some(directory),
            deepest: root.clone(),
            root,
            created_depth: 0,
        };
        fs::write(fixture.root.join("top.bin"), b"top").unwrap();
        for _ in 0..DEPTH {
            let next = fixture.deepest.join("d");
            fs::create_dir(&next).unwrap();
            fixture.deepest = next;
            fixture.created_depth += 1;
        }
        fs::write(fixture.deepest.join("deep.bin"), b"deep").unwrap();
        fs::write(fixture.deepest.join("deep.zip"), empty_zip()).unwrap();
        fixture
    }

    fn directory(&self) -> &Path {
        self.directory.as_ref().unwrap().path()
    }

    fn cleanup(&mut self) -> io::Result<()> {
        for path in [
            self.root.join("top.bin"),
            self.deepest.join("deep.bin"),
            self.deepest.join("deep.zip"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        while self.created_depth > 0 {
            fs::remove_dir(&self.deepest)?;
            self.deepest.pop();
            self.created_depth -= 1;
        }
        match fs::remove_dir(&self.root) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for DeepFixture {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            // 清理失败也不能回退到深树递归删除，把栈溢出带回整个测试进程。
            if let Some(directory) = self.directory.take() {
                let retained = directory.keep();
                eprintln!("深链逐层清理失败，保留 {}：{error}", retained.display());
            }
        }
    }
}

fn assert_sources(root: &Path) {
    let mut deepest = root.to_path_buf();
    for _ in 0..DEPTH {
        deepest.push("d");
    }
    assert_eq!(fs::read(root.join("top.bin")).unwrap(), b"top");
    assert_eq!(fs::read(deepest.join("deep.bin")).unwrap(), b"deep");
    assert_eq!(fs::read(deepest.join("deep.zip")).unwrap(), empty_zip());
}

fn scan_on_stack(
    mut job: Job,
    state: PathBuf,
    enqueue: bool,
    stack: usize,
) -> (Job, anyhow::Result<()>) {
    std::thread::Builder::new()
        .name(
            if stack == SMALL_STACK {
                "review-deep-scan"
            } else {
                "review-scan-baseline"
            }
            .into(),
        )
        .stack_size(stack)
        .spawn(move || {
            // 只把真实 scan 放在受限栈；建库、SQL 断言与夹具清理不在此线程执行。
            let result = scan(&mut job, enqueue, &state, None);
            (job, result)
        })
        .unwrap()
        .join()
        .unwrap()
}

fn assert_inventory(job: &Job, enqueue: bool) {
    let archive_size = u64::try_from(empty_zip().len()).unwrap();
    assert_eq!(job.summary.scanned, 3);
    assert_eq!(job.summary.scanned_bytes, 3 + 4 + archive_size);
    assert_eq!(job.summary.errors, 0);
    assert_eq!(job.context.control.scanned.load(Ordering::Relaxed), 3);
    let directories: Vec<(String, i64)> = job
        .db
        .conn
        .prepare("SELECT rel,depth FROM directories ORDER BY depth,rel")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        directories.len(),
        DEPTH,
        "必须登记每层目录，不能以截断遍历规避深栈"
    );
    let mut parent = String::new();
    for (index, (rel, depth)) in directories.iter().enumerate() {
        if !parent.is_empty() {
            parent.push('/');
        }
        parent.push('d');
        assert_eq!(rel, &parent);
        assert_eq!(*depth, i64::try_from(index + 1).unwrap());
    }
    let files: Vec<(String, i64)> = job
        .db
        .conn
        .prepare("SELECT rel,size FROM files ORDER BY rel")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut expected = vec![
        ("top.bin".to_string(), 3),
        (format!("{parent}/deep.bin"), 4),
        (
            format!("{parent}/deep.zip"),
            i64::try_from(archive_size).unwrap(),
        ),
    ];
    expected.sort();
    assert_eq!(files, expected, "最深文件必须以真实相对路径和大小入库");
    // archives 的真实列为 rel/depth/state；depth 是压缩包嵌套层级而非目录深度。
    let queued: Vec<(String, i64, String)> = job
        .db
        .conn
        .prepare("SELECT rel,depth,state FROM archives ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    if enqueue {
        assert_eq!(
            queued,
            vec![(format!("{parent}/deep.zip"), 0, "pending".into())]
        );
    } else {
        assert!(queued.is_empty(), "只读盘点不得把压缩包加入解压队列");
    }
    assert_sources(&job.root);
}

fn child_scan(directory: &Path, enqueue: bool) {
    // 仅隔离子进程禁用崩溃对话框，避免旧实现栈溢出时阻塞无人值守回归。
    // 保留该子进程原有错误模式，不改变父测试进程或用户桌面设置。
    #[link(name = "kernel32")]
    extern "system" {
        fn GetErrorMode() -> u32;
        fn SetErrorMode(mode: u32) -> u32;
    }
    const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
    // SAFETY: 两个 Win32 API 均无指针参数，只读取/更新当前隔离子进程的错误模式。
    unsafe {
        SetErrorMode(GetErrorMode() | SEM_NOGPFAULTERRORBOX);
    }
    let root = fs::canonicalize(directory.join("data")).unwrap();
    // 完整大栈阳性对照只在测试子进程存在，先排除夹具、schema、路径和队列断言错误。
    let baseline_state = directory.join("baseline-state");
    let (baseline_guard, baseline) = create_task_db(
        &baseline_state,
        &root,
        Config::default(),
        Context::default(),
        "scanning",
    )
    .unwrap();
    let (baseline, result) = scan_on_stack(baseline, baseline_state, enqueue, 16 * 1024 * 1024);
    result.unwrap();
    assert_inventory(&baseline, enqueue);
    drop(baseline);
    drop(baseline_guard);
    fs::write(directory.join("baseline-passed"), b"complete").unwrap();

    let state = directory.join("small-stack-state");
    let (_guard, job) = create_task_db(
        &state,
        &root,
        Config::default(),
        Context::default(),
        "scanning",
    )
    .unwrap();
    // 用现有 SQLite 能力在首次目录入库处停止一次真实 scan：相同小栈、相同 IO 池
    // 已枚举到最深文件并进入回放，但尚未递归。此测试局部触发器不增加生产接缝。
    job.db
        .conn
        .execute_batch(
            "CREATE TEMP TRIGGER review_replay_probe BEFORE INSERT ON main.directories
         BEGIN SELECT RAISE(ABORT, 'review-scan-replay-entry'); END;",
        )
        .unwrap();
    let (job, probe) = scan_on_stack(job, state.clone(), enqueue, SMALL_STACK);
    let error = probe.expect_err("回放入口探针必须触发，不能静默跳过整棵深树");
    assert!(format!("{error:#}").contains(REPLAY_PROBE), "{error:#}");
    assert_eq!(
        job.context.control.scanned.load(Ordering::Relaxed),
        3,
        "入口探针之前必须已完成最深文件的枚举和快照"
    );
    assert_eq!(job.summary.errors, 0);
    job.db
        .conn
        .execute_batch("DROP TRIGGER review_replay_probe;")
        .unwrap();
    fs::write(directory.join("enumeration-passed"), b"complete").unwrap();

    let (job, result) = scan_on_stack(job, state, enqueue, SMALL_STACK);
    result.unwrap();
    assert_inventory(&job, enqueue);
    fs::write(directory.join("passed"), b"complete").unwrap();
}

fn deep_scan(test: &str, enqueue: bool) {
    if let Some(directory) = std::env::var_os(CHILD_ROOT) {
        child_scan(&PathBuf::from(directory), enqueue);
        return;
    }
    let mut fixture = DeepFixture::new();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture"])
        .env(CHILD_ROOT, fixture.directory());
    // 保留 Result 到清理之后；超时、启动失败和子进程异常均不会绕过逐层清理。
    let output =
        jchtools_file_core::process::run_with_timeout(&mut command, Duration::from_secs(120));
    assert_sources(&fixture.root);
    let baseline = fs::read(fixture.directory().join("baseline-passed"));
    let enumeration = fs::read(fixture.directory().join("enumeration-passed"));
    let passed = fs::read(fixture.directory().join("passed"));
    fixture.cleanup().unwrap();
    let output = output.unwrap();
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        baseline.unwrap_or_default(),
        b"complete",
        "完整阳性对照未通过，不能归因于小栈回放：{diagnostics}"
    );
    assert_eq!(
        enumeration.unwrap_or_default(),
        b"complete",
        "相同小栈的枚举/回放入口探针未通过，不能归因于递归回放：{diagnostics}"
    );
    assert!(
        output.status.success(),
        "完整基线和小栈枚举入口均通过，但真实小栈 scan 未完成；状态 {:?}：{diagnostics}",
        output.status
    );
    assert_eq!(passed.unwrap(), b"complete");
}

#[test]
fn review_remediation_scan_deep_chain_inventory() {
    deep_scan("review_remediation_scan_deep_chain_inventory", false);
}

#[test]
fn review_remediation_scan_deep_chain_archive_queue() {
    deep_scan("review_remediation_scan_deep_chain_archive_queue", true);
}
