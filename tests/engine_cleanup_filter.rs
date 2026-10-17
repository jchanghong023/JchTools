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
#![allow(clippy::unwrap_used)]
//! S5-01 回归：引擎会话清场只结束本仓测试派生的 xberg.exe（可执行路径在
//! 仓库 `.tmp\` 或系统临时目录的 tempfile 目录内），不按映像名全杀——后者会
//! 误杀用户自行运行的 Xberg（与 XB-23「不操作用户自行启动的其他 Xberg」
//! 同口径；修复前 tests/common 的清场执行 `taskkill /IM xberg.exe /F`）。

// 本二进制只消费 session_lock / cleanup_stray_engines：共享支持模块的其余
// 工具在别的测试二进制中使用，此处允许整体引入而不逐项消警。
#[allow(dead_code)]
mod common;

/// 过滤判定单测：给定候选可执行路径，断言测试派生路径被选中、用户路径被排除
/// （清场脚本与该判定共用同一前缀集合，见 tests/common/mod.rs）。
#[test]
fn engine_cleanup_filter_selects_only_test_owned_paths() {
    let repo = env!("CARGO_MANIFEST_DIR");
    // 仓库 .tmp\ 下（大小写与正斜杠变体）命中。
    assert!(common::is_test_owned_engine_path(&format!(
        r"{repo}\.tmp\mock-engines\123-xberg.exe"
    )));
    let upper_repo = format!("{}\\.TMP\\workspace\\xberg.exe", repo.to_uppercase());
    assert!(common::is_test_owned_engine_path(&upper_repo));
    assert!(common::is_test_owned_engine_path(&format!(
        "{repo}/.tmp/gui-smoke/engine/xberg.exe"
    )));
    // %TEMP%\.tmp*（tempfile::tempdir 目录）命中。
    let tempfile_dir = std::env::temp_dir().join(".tmpAbCdEf").join("xberg.exe");
    assert!(common::is_test_owned_engine_path(
        &tempfile_dir.to_string_lossy()
    ));
    // 用户路径：真实引擎测试目录、任意安装目录、仓库内 .tmp 相邻目录
    // （无分隔符，不得误命中）、空串。
    assert!(!common::is_test_owned_engine_path(
        "C:\\Users\\jiang\\Documents\\xberg-test\\xberg-cli-x86_64-pc-windows-msvc\\xberg.exe"
    ));
    assert!(!common::is_test_owned_engine_path(
        "C:\\Program Files\\Xberg\\xberg.exe"
    ));
    assert!(!common::is_test_owned_engine_path(&format!(
        "{repo}\\.tmp-evil\\xberg.exe"
    )));
    assert!(!common::is_test_owned_engine_path(""));
}

#[derive(Debug, Clone, Copy)]
enum WorkerMode {
    Service,
    Broker,
}

impl WorkerMode {
    fn label(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Broker => "broker",
        }
    }

    fn argument(self) -> &'static str {
        match self {
            Self::Service => "--service",
            Self::Broker => "--xberg-broker",
        }
    }
}

// 目录必须由本次夹具独占：PID 命名会复用失败运行留下的 starts.txt，
// 既可能误判新进程已启动，也可能递归清掉不属于本次运行的旧目录。
fn fixture_directory(test_owned: bool, label: &str) -> tempfile::TempDir {
    let parent = if test_owned {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".tmp")
    } else {
        std::env::temp_dir()
    };
    std::fs::create_dir_all(&parent).unwrap();
    tempfile::Builder::new()
        .prefix(&format!("jchtools-s501-{label}-"))
        .tempdir_in(parent)
        .unwrap()
}

// Child 本身的 Drop 不结束进程。守卫在断言、启动等待或清场发生 panic 时
// 也先结束并回收本次子进程，再由后创建前销毁顺序释放其临时目录。
struct TestChild(std::process::Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_fixture_start(child: &mut TestChild, directory: &std::path::Path) {
    let marker = directory.join("starts.txt");
    let expected_pid = child.0.id().to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let started = match std::fs::read_to_string(&marker) {
            Ok(pids) => pids.lines().any(|pid| pid == expected_pid),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => panic!("读取模拟进程启动标记失败：{error}"),
        };
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "模拟进程必须在清场前保持运行"
        );
        if started {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "模拟进程未写入本次 PID 启动标记"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn assert_worker_cleanup(mode: WorkerMode, test_owned: bool, expected_alive: bool) {
    let _session = common::session_lock();
    let owner = if test_owned { "test" } else { "user" };
    let dir = fixture_directory(test_owned, &format!("{owner}-{}", mode.label()));
    let executable = dir.path().join("snap-ocr-worker.exe");
    common::mock_engine_copy("tests/fixtures/shared_xberg.rs", &executable);
    let mut child = TestChild(
        std::process::Command::new(&executable)
            .arg(mode.argument())
            .current_dir(dir.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_fixture_start(&mut child, dir.path());

    common::cleanup_stray_engines();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let alive = loop {
        match child.0.try_wait().unwrap() {
            Some(_) => break false,
            None if expected_alive => break true,
            None if std::time::Instant::now() >= deadline => break true,
            None => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    };
    assert_eq!(
        alive, expected_alive,
        "清场对 {owner} {mode:?} worker 的存活结果不符合所有权边界"
    );
}

#[test]
fn cleanup_keeps_user_owned_service_alive() {
    assert_worker_cleanup(WorkerMode::Service, false, true);
}

#[test]
fn cleanup_stops_test_owned_service() {
    assert_worker_cleanup(WorkerMode::Service, true, false);
}

#[test]
fn cleanup_keeps_user_owned_broker_worker_alive() {
    assert_worker_cleanup(WorkerMode::Broker, false, true);
}

#[test]
fn cleanup_stops_test_owned_broker_worker() {
    assert_worker_cleanup(WorkerMode::Broker, true, false);
}

/// 用户路径的 xberg.exe（临时目录中、但不在测试派生前缀内）必须在清场后存活。
#[test]
fn cleanup_keeps_user_owned_xberg_image_alive() {
    // 会话锁：与其他引擎测试二进制互斥，避免清场与真实引擎测试相互干扰。
    let _session = common::session_lock();
    // 诱饵目录：系统临时目录下、但不在 `.tmp*` 前缀内 → 按用户自有路径对待。
    let decoy_dir = fixture_directory(false, "decoy");
    let decoy_exe = decoy_dir.path().join("xberg.exe");
    common::mock_engine_copy("tests/fixtures/shared_xberg.rs", &decoy_exe);
    let mut child = TestChild(
        std::process::Command::new(&decoy_exe)
            .current_dir(decoy_dir.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_fixture_start(&mut child, decoy_dir.path());
    // 清场（锁内调用，与各引擎测试同口径）。
    common::cleanup_stray_engines();
    let alive = child.0.try_wait().unwrap().is_none();
    assert!(alive, "清场不得按映像名误杀用户路径的 xberg.exe（S5-01）");
}

/// 反向守护：测试派生路径（`%TEMP%\.tmp*`，tempfile::tempdir 前缀）的 xberg.exe
/// 仍是清场目标——收窄清理范围不得让孤儿引擎漏杀（那是本清场存在的目的）。
#[test]
fn cleanup_still_kills_test_owned_xberg_image() {
    let _session = common::session_lock();
    let owned_dir = tempfile::Builder::new()
        .prefix(".tmpjchs501-owned-")
        .tempdir()
        .unwrap();
    let owned_exe = owned_dir.path().join("xberg.exe");
    common::mock_engine_copy("tests/fixtures/shared_xberg.rs", &owned_exe);
    let mut child = TestChild(
        std::process::Command::new(&owned_exe)
            .current_dir(owned_dir.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_fixture_start(&mut child, owned_dir.path());
    common::cleanup_stray_engines();
    // Stop-Process 发起后允许短暂的终止传播：有界等待进程退出。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let exited = loop {
        if child.0.try_wait().unwrap().is_some() {
            break true;
        }
        if std::time::Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    assert!(
        exited,
        "测试派生路径的 xberg.exe 仍必须被清场结束（S5-01 反向）"
    );
}

// Integration-test dependencies are compiled without cfg(test): this exercises
// engine_bundle's real persistence branch, unlike its crate-local unit tests.
#[cfg(feature = "test-hooks")]
mod bounded_engine_warning_regression {
    use super::TestChild;
    use std::{
        fs,
        path::Path,
        process::{Command, ExitStatus, Stdio},
        time::{Duration, Instant},
    };

    const CHILD_ROOT: &str = "JCHTOOLS_REVIEW_F54_CHILD_ROOT";
    const COMPILER_ROOT: &str = "JCHTOOLS_REVIEW_F54_COMPILER_ROOT";
    const TEST_NAME: &str =
        "bounded_engine_warning_regression::rejected_engine_uses_only_bounded_diagnostics";

    // Match common::ensure_child_reaper's Job limits, but own only this run's
    // compiler helper and its descendants, never the whole test executable.
    #[cfg(windows)]
    struct CompilerJob(windows_sys::Win32::Foundation::HANDLE);

    #[cfg(windows)]
    impl CompilerJob {
        fn new() -> Self {
            use windows_sys::Win32::System::JobObjects::{
                CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            // SAFETY: no name or security attributes; this run owns the handle.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            assert!(!handle.is_null(), "create F54 compiler Job: {}", std::io::Error::last_os_error());
            let job = Self(handle);
            // SAFETY: the Win32 POD structure accepts zero initialization.
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: job and limits remain valid for the synchronous call.
            let configured = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    u32::try_from(std::mem::size_of_val(&limits)).unwrap(),
                )
            };
            assert_ne!(configured, 0, "configure F54 compiler Job: {}", std::io::Error::last_os_error());
            job
        }

        fn assign(&self, child: &std::process::Child) {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
            // SAFETY: both handles are live; the helper cannot spawn rustc until
            // the parent releases its stdin barrier after this assignment.
            let assigned = unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle()) };
            assert_ne!(assigned, 0, "assign F54 compiler Job: {}", std::io::Error::last_os_error());
        }
    }

    #[cfg(windows)]
    impl Drop for CompilerJob {
        fn drop(&mut self) {
            use windows_sys::Win32::{
                Foundation::CloseHandle,
                System::JobObjects::{
                    JobObjectBasicAccountingInformation, QueryInformationJobObject,
                    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
                },
            };
            // Closing a Job alone initiates asynchronous termination. Keep its
            // handle until every compiler/linker descendant has actually exited,
            // so the enclosing TempDir cannot be removed while they still run.
            // SAFETY: this guard exclusively owns a live Job handle.
            if unsafe { TerminateJobObject(self.0, 1) } == 0 {
                eprintln!("terminate F54 compiler Job: {}", std::io::Error::last_os_error());
                std::process::abort();
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                // SAFETY: zero is valid for the output-only Win32 POD structure.
                let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
                    unsafe { std::mem::zeroed() };
                // SAFETY: live Job and correctly sized writable output buffer.
                let queried = unsafe {
                    QueryInformationJobObject(
                        self.0,
                        JobObjectBasicAccountingInformation,
                        (&raw mut accounting).cast(),
                        u32::try_from(std::mem::size_of_val(&accounting)).unwrap(),
                        std::ptr::null_mut(),
                    )
                };
                if queried == 0 || Instant::now() >= deadline {
                    // Fail closed, including during unwind: do not drop the
                    // directory if owned-tree teardown could not be established.
                    eprintln!("F54 compiler Job teardown failed: {}", std::io::Error::last_os_error());
                    std::process::abort();
                }
                if accounting.ActiveProcesses == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            // SAFETY: no members remain, and only this guard owns the handle.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn wait_for_exit(child: &mut TestChild) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "F54 probe child timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn check_real_resolution(root: &Path) {
        let executable = std::env::current_exe().unwrap();
        assert_eq!(
            executable.parent().unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap(),
            "probe must run from its exclusive copy, never the Cargo output"
        );
        let state = root.join("state");
        assert_eq!(
            std::env::var_os("JCHTOOLS_TEST_STATE_DIR").unwrap(),
            state.as_os_str()
        );
        assert_eq!(
            jchtools_file_core::config::state_dir().unwrap(),
            state,
            "test-hooks must isolate the actual dependency before resolution"
        );
        let candidate = root.join("resources/7zip").join(if cfg!(windows) {
            "7z.exe"
        } else {
            "7zz"
        });
        let original = fs::read(&candidate).unwrap();
        assert!(!original.is_empty());
        assert!(!root.join("starts.txt").exists());
        let guard = jchtools_foundation::logging::init(&state)
            .expect("real bounded diagnostic subscriber must initialize");
        let outcome = jchtools_file_core::engine_bundle::resolve_executable();
        if let Ok(resolved) = &outcome {
            assert_ne!(resolved, &candidate, "invalid candidate must not be selected");
            assert!(
                resolved.starts_with(&state),
                "any embedded fallback must stay in this probe's state root"
            );
        }
        assert_eq!(
            fs::read(&candidate).unwrap(),
            original,
            "rejected candidate must not be overwritten or deleted"
        );
        // The existing fixture writes starts.txt before processing any arguments.
        // It is a runnable but untrusted engine, not an unexecutable byte stub.
        assert!(
            !root.join("starts.txt").exists()
                && !candidate.parent().unwrap().join("starts.txt").exists(),
            "candidate validation must never execute the rejected engine"
        );
        drop(guard);
        let mut diagnostics = String::new();
        for entry in fs::read_dir(state.join("logs")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|extension| extension == "log") {
                diagnostics.push_str(&fs::read_to_string(path).unwrap());
            }
        }
        assert!(
            diagnostics.contains("event=engine_candidate_rejected"),
            "real rejection must reach the controlled diagnostic logs"
        );
        let unbounded = state.join("engine-warnings.log");
        println!(
            "root_warning_file={} bytes={}",
            unbounded.exists(),
            fs::metadata(&unbounded).map_or(0, |metadata| metadata.len())
        );
        assert!(
            !unbounded.exists(),
            "P-10: rejected engines must not create an unbounded shared state-root log"
        );
    }

    #[test]
    fn rejected_engine_uses_only_bounded_diagnostics() {
        if let Some(root) = std::env::var_os(COMPILER_ROOT) {
            use std::io::Read as _;
            // The parent assigns this helper to its exclusive Job before
            // releasing the barrier. rustc and linker then inherit membership
            // from their first instruction, without a spawn/assign race.
            let mut release = [0];
            std::io::stdin().read_exact(&mut release).unwrap();
            assert_eq!(release, [b'C'], "F54 compiler barrier was not released");
            let root = Path::new(&root);
            let status = Command::new("rustc")
                .args(["--edition=2021", "-C", "target-feature=+crt-static"])
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shared_xberg.rs"))
                .arg("-o")
                .arg(root.join("resources/7zip").join(if cfg!(windows) {
                    "7z.exe"
                } else {
                    "7zz"
                }))
                .current_dir(root)
                .stdin(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "execution-sentinel fixture failed");
            return;
        }
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            check_real_resolution(Path::new(&root));
            return;
        }
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace = repository.join(".tmp/review-repairs/CoreRedProbe");
        assert!(Command::new("python")
            .arg(repository.join("scripts/make_tmp.py"))
            .args(["workspace", "--destination"])
            .arg(&workspace)
            .current_dir(repository)
            .status()
            .unwrap()
            .success());
        // TempDir owns only this run. Child guards are created afterwards, so
        // panic/timeout always kills and reaps them before directory removal.
        let run = tempfile::Builder::new()
            .prefix("f54-")
            .tempdir_in(workspace)
            .unwrap();
        let resources = run.path().join("resources/7zip");
        fs::create_dir_all(&resources).unwrap();
        let executable = run.path().join(if cfg!(windows) {
            "engine-warning-probe.exe"
        } else {
            "engine-warning-probe"
        });
        fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        // Reuse the existing std-only execution-sentinel fixture. No separate
        // Cargo project, dependency compilation, or target directory is needed.
        // The Job guard precedes the child guard, so unwind first reaps the
        // helper, then kills/waits its whole compiler tree, then removes run.
        #[cfg(windows)]
        let compiler_job = CompilerJob::new();
        let mut compiler = TestChild(
            Command::new(&executable)
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env(COMPILER_ROOT, run.path())
                .current_dir(run.path())
                .stdin(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        #[cfg(windows)]
        compiler_job.assign(&compiler.0);
        {
            use std::io::Write as _;
            compiler.0.stdin.take().unwrap().write_all(b"C").unwrap();
        }
        let compiler_status = wait_for_exit(&mut compiler);
        drop(compiler);
        #[cfg(windows)]
        drop(compiler_job);
        assert!(compiler_status.success(), "execution-sentinel fixture failed");
        let output_path = run.path().join("child-output.log");
        let output = fs::File::create(&output_path).unwrap();
        let error_output = output.try_clone().unwrap();
        let mut child = TestChild(
            Command::new(&executable)
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env(CHILD_ROOT, run.path())
                .env("JCHTOOLS_TEST_STATE_DIR", run.path().join("state"))
                .env("JCH_LOG_LEVEL", "info")
                .current_dir(run.path())
                .stdin(Stdio::null())
                .stdout(output)
                .stderr(error_output)
                .spawn()
                .unwrap(),
        );
        let status = wait_for_exit(&mut child);
        drop(child);
        let transcript = fs::read_to_string(output_path).unwrap();
        // Explicitly remove the copied executable and all state even when the
        // child's real behavior assertion failed; only then propagate failure.
        run.close().expect("exclusive F54 probe directory must be removed");
        assert!(status.success(), "F54 dependency probe failed ({status}):\n{transcript}");
    }
}
