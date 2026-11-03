//! ST-01～ST-05：真实独立后台、Windows 进程树及持久化；不启动桌面。
#![cfg(windows)] // P-07：产品仅支持 Windows，进程树验收依赖 Windows Shell 和进程句柄。
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试夹具出错即应失败。

use base64::Engine as _;
use jchtools_scheduled_tasks::{self as scheduled, IntervalUnit, RunRecord, Task};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE,
};

const SCENARIO: &str = "JCHTOOLS_SCHEDULED_SCENARIO";
const HOST: &str = "JCHTOOLS_SCHEDULED_HOST";
const WAIT: Duration = Duration::from_secs(25);

fn wait_for<T>(description: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "等待超时：{description}");
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn root() -> PathBuf {
    PathBuf::from(std::env::var_os("JCHTOOLS_TEST_STATE_DIR").unwrap())
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

fn task(command: String, directory: &Path) -> Task {
    Task {
        id: 0,
        name: "隔离任务".into(),
        command,
        working_directory: directory.to_string_lossy().into_owned(),
        interval: 1,
        unit: IntervalUnit::Hours,
        enabled: false,
        shell: "powershell.exe".into(),
        environment: BTreeMap::new(),
        timeout_seconds: None,
    }
}

fn started() -> u32 {
    let snapshot =
        scheduled::ensure_started_for_test(PathBuf::from(env!("CARGO_BIN_EXE_JchTools")))
            .expect("启动隔离定时后台");
    assert!(snapshot.running);
    snapshot.pid.expect("后台 PID")
}

fn status(id: i64) -> scheduled::TaskStatus {
    scheduled::list()
        .unwrap()
        .tasks
        .into_iter()
        .find(|item| item.task.id == id)
        .unwrap()
}

fn records(id: i64) -> Vec<RunRecord> {
    scheduled::history(id).unwrap()
}

fn running_record(id: i64) -> RunRecord {
    wait_for("运行中的记录", || {
        records(id)
            .into_iter()
            .find(|record| record.status == "运行中")
    })
}

fn finished(id: i64, run_id: i64) -> RunRecord {
    wait_for("执行结束", || {
        records(id)
            .into_iter()
            .find(|record| record.id == run_id && record.status != "运行中")
    })
}

fn wait_file(path: &Path) {
    wait_for("隔离文件出现", || path.exists().then_some(()));
}

struct ServiceGuard;
impl Drop for ServiceGuard {
    fn drop(&mut self) {
        let _ = scheduled::stop_service();
    }
}

// 持有句柄而非反复按 PID 打开，避免进程退出后的 PID 重用造成误判。
struct Process(HANDLE);
impl Process {
    fn open(pid: u32) -> Self {
        // SAFETY: 仅打开本场景报告的进程，权限用于查询或异常退出测试。
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        assert!(!handle.is_null(), "无法打开隔离进程 {pid}");
        Self(handle)
    }
    fn alive(&self) -> bool {
        let mut exit_code = 0;
        // SAFETY: 句柄在 Drop 前有效，退出码指向有效内存。
        assert_ne!(unsafe { GetExitCodeProcess(self.0, &raw mut exit_code) }, 0);
        exit_code == 259
    }
    fn kill(&self) {
        // SAFETY: 句柄仅指向本场景启动的后台。
        assert_ne!(unsafe { TerminateProcess(self.0, 1) }, 0);
        wait_for("进程退出", || (!self.alive()).then_some(()));
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        // 断言失败时也只清理本场景明确持有句柄的进程，清理过程不再次 panic。
        let mut exit_code = 0;
        // SAFETY: 句柄在 Drop 前有效，退出码指向有效内存。
        let queried = unsafe { GetExitCodeProcess(self.0, &raw mut exit_code) };
        if queried != 0 && exit_code == 259 {
            // SAFETY: 该句柄仅属于本场景启动的后台或其后代。
            let _ = unsafe { TerminateProcess(self.0, 1) };
        }
        // SAFETY: 句柄由 OpenProcess 创建且只关闭一次。
        unsafe { CloseHandle(self.0) };
    }
}

fn subprocess(scenario: &str) {
    let tmp = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("scheduled-")
        .tempdir_in(tmp)
        .unwrap();
    let state = directory.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let registry = format!(
        "Software\\JchTools\\Tests\\Scheduled\\{}",
        uuid::Uuid::new_v4()
    );
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "scenario_entry", "--nocapture"])
        .env(SCENARIO, scenario)
        .env_remove(HOST)
        .env_remove("ST_VALUE")
        .env("JCHTOOLS_TEST_STATE_DIR", &state)
        .env(
            "JCHTOOLS_TEST_CODEAGENT_CREDENTIALS",
            directory.path().join("credentials.json"),
        )
        .env("JCHTOOLS_TEST_CODEAGENT_REGISTRY", registry)
        .env(
            "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT",
            directory.path().join("markdown-assets"),
        )
        .env(
            "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT",
            directory.path().join("screenshot-assets"),
        )
        .env("JCHTOOLS_TEST_SCHEDULED_MINUTE_MS", "1200")
        .output()
        .unwrap();
    if !output.status.success() {
        let kept = directory.keep();
        panic!(
            "场景 {scenario} 失败，现场 {}\nstdout:\n{}\nstderr:\n{}",
            kept.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn persistence_and_snapshots() {
    subprocess("persistence");
}
#[test]
fn periodic_and_busy() {
    subprocess("periodic");
}
#[test]
fn stop_timeout_and_interruption() {
    subprocess("processes");
}
#[test]
fn logs_and_retention() {
    subprocess("logs");
}
#[test]
fn shell_commands_and_encoding() {
    subprocess("shells");
}

#[test]
fn spawning_host() {
    if std::env::var_os(HOST).is_some() {
        let pid = started();
        assert_ne!(pid, 0);
        std::fs::write(root().join("host-pid"), pid.to_string()).unwrap();
    }
}

#[test]
fn scenario_entry() {
    let Ok(scenario) = std::env::var(SCENARIO) else {
        return;
    };
    let _service = ServiceGuard;
    match scenario.as_str() {
        "persistence" => persistence(),
        "periodic" => periodic(),
        "processes" => processes(),
        "logs" => logs(),
        "shells" => shells(),
        _ => panic!("未知隔离场景"),
    }
}

// ST-01 / ST-02 / ST-04 / ST-05：短命宿主退出后后台仍执行；编辑不改运行快照。
fn persistence() {
    let root = root();
    let first = root.join("first");
    let second = root.join("second");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    let release = root.join("release");
    let marker = root.join("detached-result");
    let mut item = task(format!("[Console]::Out.WriteLine($env:ST_VALUE); while (!(Test-Path -LiteralPath {})) {{ Start-Sleep -Milliseconds 50 }}; [IO.File]::WriteAllText({}, (Get-Location).Path)", quoted(&release), quoted(&marker)), &first);
    item.environment
        .insert("ST_VALUE".into(), "original-environment".into());
    item.timeout_seconds = Some(20);
    let id = scheduled::save(&item).unwrap();
    let host = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "spawning_host", "--nocapture"])
        .env(HOST, "1")
        .status()
        .unwrap();
    assert!(host.success());
    let pid = started();
    assert_eq!(
        Some(pid),
        scheduled::ensure_started_for_test(PathBuf::from(env!("CARGO_BIN_EXE_JchTools")))
            .unwrap()
            .pid
    );
    assert_eq!(
        pid,
        std::fs::read_to_string(root.join("host-pid"))
            .unwrap()
            .parse::<u32>()
            .unwrap()
    );
    scheduled::run_now(id).unwrap();
    let running = running_record(id);
    let old_command = item.command.clone();
    item.id = id;
    item.command = "[Console]::Out.WriteLine('edited-command')".into();
    item.enabled = true;
    item.working_directory = second.to_string_lossy().into_owned();
    scheduled::save(&item).unwrap();
    std::fs::write(&release, "release").unwrap();
    let completed = finished(id, running.id);
    assert_eq!(completed.status, "成功");
    assert_eq!(completed.command, old_command);
    assert_eq!(completed.working_directory, first.to_string_lossy());
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        first.to_string_lossy()
    );
    assert!(scheduled::read_log(running.id)
        .unwrap()
        .stdout
        .contains("original-environment"));
    assert!(
        std::env::var_os("ST_VALUE").is_none(),
        "任务变量不得写入宿主环境"
    );
    scheduled::run_now(id).unwrap();
    let edited = wait_for("编辑后的执行", || {
        records(id)
            .into_iter()
            .find(|r| r.id != running.id && r.status != "运行中")
    });
    assert_eq!(edited.status, "成功");
    assert_eq!(edited.command, item.command);
    assert_eq!(edited.working_directory, item.working_directory);
    assert!(scheduled::read_log(edited.id)
        .unwrap()
        .stdout
        .contains("edited-command"));
    scheduled::stop_service().unwrap();
    started();
    let restored = status(id);
    assert_eq!(restored.task.command, item.command);
    assert_eq!(restored.task.working_directory, item.working_directory);
    assert_eq!(
        restored
            .task
            .environment
            .get("ST_VALUE")
            .map(String::as_str),
        Some("original-environment")
    );
    assert_eq!(restored.task.timeout_seconds, Some(20));
    assert!(restored.task.enabled);
    assert_eq!(records(id).len(), 2);
}

// ST-03 / ST-04：固定计划、修改周期、忙时跳过、禁用及禁止并发/删除。
fn periodic() {
    let root = root();
    started();
    let release = root.join("periodic-release");
    let mut item = task(
        format!(
            "while (!(Test-Path -LiteralPath {})) {{ Start-Sleep -Milliseconds 50 }}",
            quoted(&release)
        ),
        &root,
    );
    item.unit = IntervalUnit::Minutes;
    item.interval = 2;
    let id = scheduled::save(&item).unwrap();
    scheduled::set_enabled(id, true).unwrap();
    let first_due = status(id).next_run.expect("启用后排定首次执行");
    std::thread::sleep(Duration::from_millis(300));
    assert!(records(id).is_empty(), "首次执行必须等待一个周期");
    scheduled::run_now(id).unwrap();
    let manual = running_record(id);
    assert_eq!(manual.trigger, "手动");
    assert_eq!(status(id).next_run, Some(first_due), "手动执行不能重置计划");
    item.id = id;
    item.enabled = true;
    item.interval = 1;
    scheduled::save(&item).unwrap();
    assert_eq!(
        status(id).next_run,
        Some(first_due),
        "编辑周期保留已排定时间"
    );
    assert!(scheduled::run_now(id).is_err(), "运行中不允许重复手动执行");
    assert!(scheduled::delete(id).is_err(), "运行中不允许删除");
    let next = wait_for("忙时周期推进", || {
        let now_ms = || {
            i64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap()
        };
        let before = now_ms();
        let next = status(id).next_run?;
        let after = now_ms();
        if before < first_due || next <= after {
            return None;
        }
        let slot = (before - first_due) / 1200;
        // 新旧周期在奇数窗口的下一计划点可能重合，错过区分窗口时等待下一个。
        if slot % 2 != 0 {
            return None;
        }
        if (after - first_due) / 1200 != slot {
            return None;
        }
        Some((next, first_due + (slot + 1) * 1200))
    });
    assert_eq!(next.0, next.1, "编辑后的周期必须从原计划点按新周期推进");
    assert_eq!(records(id).len(), 1, "忙时跳过而不创建记录");
    scheduled::set_enabled(id, false).unwrap();
    let disabled = status(id);
    assert!(
        !disabled.task.enabled && disabled.running,
        "禁用不能结束本次执行"
    );
    std::fs::write(release, "release").unwrap();
    assert_eq!(finished(id, manual.id).status, "成功");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(records(id).len(), 1, "跳过的周期不得补跑");
    scheduled::delete(id).unwrap();

    // 空闲周期执行按计划点推进，而非按刚结束的手动执行重置。
    let mut cycle = task("Start-Sleep -Milliseconds 500".into(), &root);
    cycle.unit = IntervalUnit::Minutes;
    cycle.enabled = true;
    let cycle_id = scheduled::save(&cycle).unwrap();
    let due = status(cycle_id).next_run.unwrap();
    scheduled::run_now(cycle_id).unwrap();
    let first = running_record(cycle_id);
    finished(cycle_id, first.id);
    let after_manual = status(cycle_id).next_run.unwrap();
    assert!(
        after_manual >= due && (after_manual - due) % 1200 == 0,
        "手动执行不能重置计划相位"
    );
    let periodic = wait_for("周期执行", || {
        records(cycle_id).into_iter().find(|r| r.trigger == "周期")
    });
    let next = wait_for("下个固定计划点", || {
        status(cycle_id).next_run.filter(|next| *next > due)
    });
    assert_eq!((next - due) % 1200, 0, "周期执行必须保持原计划相位");
    assert_eq!(finished(cycle_id, periodic.id).status, "成功");
    scheduled::set_enabled(cycle_id, false).unwrap();

    let mut offline = task("[Console]::Out.Write('periodic')".into(), &root);
    offline.unit = IntervalUnit::Minutes;
    offline.interval = 2;
    offline.enabled = true;
    let offline_id = scheduled::save(&offline).unwrap();
    scheduled::stop_service().unwrap();
    std::thread::sleep(Duration::from_millis(5000));
    started();
    assert!(
        records(offline_id).is_empty(),
        "停机期间漏跑不能在恢复时补跑"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(records(offline_id).is_empty(), "恢复后不能立即重放漏跑");
    scheduled::set_enabled(offline_id, false).unwrap();
}

// 后代先写 ready，稍后写 forbidden；测试既持有其真实进程句柄也检查实际副作用。
fn tree_command(root: &Path, label: &str) -> (String, PathBuf, PathBuf, PathBuf) {
    let pid = root.join(format!("{label}-pid"));
    let ready = root.join(format!("{label}-ready"));
    let forbidden = root.join(format!("{label}-late"));
    let child = format!("[IO.File]::WriteAllText({}, 'ready'); Start-Sleep -Seconds 7; [IO.File]::WriteAllText({}, 'late')", quoted(&ready), quoted(&forbidden));
    let utf16: Vec<u8> = child.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    let command = format!("$p = Start-Process -FilePath powershell.exe -ArgumentList '-NoProfile','-EncodedCommand','{encoded}' -WindowStyle Hidden -PassThru; [IO.File]::WriteAllText({}, $p.Id.ToString()); [Console]::Out.WriteLine('tree-started'); Start-Sleep -Seconds 30", quoted(&pid));
    (command, pid, ready, forbidden)
}

fn tree_handle(pid: &Path, ready: &Path) -> Process {
    wait_file(pid);
    let child = Process::open(std::fs::read_to_string(pid).unwrap().parse().unwrap());
    wait_file(ready);
    child
}

// ST-04：停止、真实秒超时和后台异常退出都终结关联子进程，不重放中断执行。
fn processes() {
    let root = root();
    let service_pid = started();
    for (label, timeout) in [("stop", None), ("timeout", Some(4)), ("interrupt", None)] {
        let (command, pid, ready, forbidden) = tree_command(&root, label);
        let mut item = task(command, &root);
        item.timeout_seconds = timeout;
        let id = scheduled::save(&item).unwrap();
        scheduled::run_now(id).unwrap();
        let running = running_record(id);
        let child = tree_handle(&pid, &ready);
        assert!(child.alive());
        if label == "stop" {
            scheduled::stop_run(id).unwrap();
        } else if label == "interrupt" {
            wait_for("中断前日志", || {
                scheduled::read_log(running.id)
                    .ok()
                    .filter(|log| log.stdout.contains("tree-started"))
            });
            Process::open(service_pid).kill();
            wait_for("异常退出后子进程结束", || {
                (!child.alive()).then_some(())
            });
            started();
        }
        let ended = finished(id, running.id);
        assert_eq!(
            ended.status,
            match label {
                "stop" => "已停止",
                "timeout" => "超时",
                _ => "中断",
            }
        );
        wait_for("关联进程终止", || (!child.alive()).then_some(()));
        if label == "timeout" {
            assert!(
                ended.duration_ms.unwrap() >= 3_000,
                "超时使用真实秒而非缩放分钟"
            );
        }
        if label == "interrupt" {
            assert!(scheduled::read_log(running.id)
                .unwrap()
                .stdout
                .contains("tree-started"));
            assert_ne!(ended.exit_code, Some(0));
            std::thread::sleep(Duration::from_millis(500));
            assert_eq!(records(id).len(), 1, "中断执行不得重放");
        }
        std::thread::sleep(Duration::from_secs(8));
        assert!(!forbidden.exists(), "关联子进程不能在停止后继续写文件");
        scheduled::delete(id).unwrap();
    }
}

fn files_containing(directory: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in walkdir::WalkDir::new(directory)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file() {
            if let Ok(bytes) = std::fs::read(entry.path()) {
                if bytes.windows(needle.len()).any(|window| window == needle) {
                    found.push(entry.into_path());
                }
            }
        }
    }
    found
}

// ST-01 / ST-05：通过真实后台验证 Shell 命令行和两个日志通道的中文。
fn shells() {
    let root = root();
    started();
    let program_dir = root.join("program path with spaces");
    std::fs::create_dir_all(&program_dir).unwrap();
    let program = program_dir.join("cmd.exe");
    let system_root = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
    std::fs::copy(system_root.join("System32").join("cmd.exe"), &program).unwrap();

    let run = |item: &Task, stdout: &str, stderr: &str| {
        let id = scheduled::save(item).unwrap();
        scheduled::run_now(id).unwrap();
        let record = wait_for("Shell 命令执行完成", || {
            records(id).into_iter().find(|r| r.status != "运行中")
        });
        assert_eq!(record.trigger, "手动");
        assert_eq!(record.status, "成功");
        assert_eq!(record.exit_code, Some(0));
        let log = scheduled::read_log(record.id).unwrap();
        assert_eq!(log.stdout, stdout);
        assert_eq!(log.stderr, stderr);
        scheduled::delete(id).unwrap();
    };

    let mut cmd = task(
        format!("\"{}\" /d /c echo \"quoted argument\"", program.display()),
        &root,
    );
    cmd.shell = "cmd.exe".into();
    run(&cmd, "\"quoted argument\"\r\n", "");

    cmd.command = "echo 中文标准输出&1>&2 echo 中文标准错误".into();
    run(&cmd, "中文标准输出\r\n", "中文标准错误\r\n");

    let powershell = task(
        "[Console]::Out.Write('中文标准输出'); [Console]::Error.Write('中文标准错误')".into(),
        &root,
    );
    run(&powershell, "中文标准输出", "中文标准错误");
}

// ST-05：真实运行中的两个通道、全局尾部截断和 100 次淘汰。
fn logs() {
    let root = root();
    started();
    let gate = root.join("log-gate");
    let tail_gate = root.join("log-tail-gate");
    let release = root.join("log-release");
    let command = format!("[Console]::Out.WriteLine('live-out'); [Console]::Error.WriteLine('live-err'); while (!(Test-Path -LiteralPath {})) {{ Start-Sleep -Milliseconds 50 }}; [Console]::Out.Write(('x' * 1100000)); while (!(Test-Path -LiteralPath {})) {{ Start-Sleep -Milliseconds 50 }}; [Console]::Error.WriteLine('tail-err'); while (!(Test-Path -LiteralPath {})) {{ Start-Sleep -Milliseconds 50 }}", quoted(&gate), quoted(&tail_gate), quoted(&release));
    let id = scheduled::save(&task(command, &root)).unwrap();
    scheduled::run_now(id).unwrap();
    let running = running_record(id);
    let live = wait_for("运行中双通道输出", || {
        scheduled::read_log(running.id)
            .ok()
            .filter(|log| log.stdout.contains("live-out") && log.stderr.contains("live-err"))
    });
    assert_eq!(live.record.status, "运行中");
    assert!(live.record.ended_at.is_none());
    std::fs::write(gate, "go").unwrap();
    wait_for("接收超限输出", || {
        scheduled::read_log(running.id)
            .ok()
            .filter(|log| log.record.truncated)
    });
    std::fs::write(tail_gate, "go").unwrap();
    let tail = wait_for("运行中截断", || {
        scheduled::read_log(running.id)
            .ok()
            .filter(|log| log.record.truncated && log.stderr.contains("tail-err"))
    });
    assert!(tail.stdout.len() + tail.stderr.len() <= 1_000_000);
    assert!(!tail.stdout.contains("live-out"));
    std::fs::write(release, "done").unwrap();
    assert_eq!(finished(id, running.id).status, "成功");
    let tail = scheduled::read_log(running.id).unwrap();
    assert!(tail.record.truncated && tail.stderr.contains("tail-err"));
    assert!(tail.stdout.len() + tail.stderr.len() <= 1_000_000);

    let input = root.join("unique-output.txt");
    let needle = format!("old-log-marker-{}", uuid::Uuid::new_v4());
    std::fs::write(&input, &needle).unwrap();
    let mut retention = task(
        format!(
            "[Console]::Out.Write([IO.File]::ReadAllText({}))",
            quoted(&input)
        ),
        &root,
    );
    let retention_id = scheduled::save(&retention).unwrap();
    let preexisting: std::collections::HashSet<_> = walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .collect();
    scheduled::run_now(retention_id).unwrap();
    let old = wait_for("首条保留记录", || {
        records(retention_id)
            .into_iter()
            .find(|r| r.status != "运行中")
    });
    assert_eq!(old.status, "成功");
    assert!(scheduled::read_log(old.id)
        .unwrap()
        .stdout
        .contains(&needle));
    // 排除源输入，只追踪后台生成的含旧输出文件，不依赖具体日志文件布局。
    std::fs::remove_file(&input).unwrap();
    let old_files: Vec<_> = files_containing(&root, needle.as_bytes())
        .into_iter()
        .filter(|path| !preexisting.contains(path))
        .collect();
    assert!(!old_files.is_empty(), "应存在旧运行的持久日志");
    retention.id = retention_id;
    retention.command = "[Console]::Out.Write('new')".into();
    scheduled::save(&retention).unwrap();
    let mut previous_id = old.id;
    for index in 0..100 {
        scheduled::run_now(retention_id).unwrap();
        previous_id = wait_for("后续执行完成", || {
            let history = records(retention_id);
            let latest = history.first()?;
            (latest.id != previous_id
                && latest.status != "运行中"
                && history.len() >= (index + 2).min(100))
            .then_some(latest.id)
        });
    }
    let history = records(retention_id);
    assert_eq!(history.len(), 100);
    assert!(!history.iter().any(|record| record.id == old.id));
    assert!(
        scheduled::read_log(old.id).is_err(),
        "淘汰记录不能再读取日志"
    );
    for file in old_files {
        let remains = match std::fs::read(&file) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("无法检查旧日志 {}: {error}", file.display()),
        };
        assert!(
            !remains
                .windows(needle.len())
                .any(|window| window == needle.as_bytes()),
            "旧日志未清理：{}",
            file.display()
        );
    }
}
