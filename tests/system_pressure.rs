//! SPL：真实Windows计数器、独立进程生命周期和实际文件失败，不启动桌面。
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // 隔离测试夹具失败即断言失败。

use jchtools_system_pressure as pressure;
use pressure::output_path;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const SCENARIO: &str = "JCHTOOLS_PRESSURE_SCENARIO";

fn wait_for<T>(description: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if let Some(result) = check() {
            return result;
        }
        assert!(Instant::now() < deadline, "等待超时：{description}");
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn csv(minimum_samples: usize) -> String {
    wait_for("完整CSV采样窗口", || {
        let text = std::fs::read_to_string(output_path()).ok()?;
        if !text.ends_with('\n') {
            return None;
        }
        let rows: Vec<Vec<&str>> = text
            .lines()
            .skip(1)
            .map(|line| line.split(',').collect())
            .collect();
        if rows.is_empty() || rows.iter().any(|row| row.len() != 7) {
            return None;
        }
        let samples: BTreeSet<_> = rows.iter().map(|row| row[0]).collect();
        (samples.len() >= minimum_samples).then_some(text)
    })
}

struct ServiceGuard;
impl Drop for ServiceGuard {
    fn drop(&mut self) {
        let _ = pressure::stop();
    }
}

fn subprocess(scenario: &str) {
    let directory = tempfile::tempdir().unwrap();
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "scenario_entry", "--nocapture"])
        .env(SCENARIO, scenario)
        .env("JCHTOOLS_TEST_STATE_DIR", directory.path().join("state"))
        .env(
            "JCHTOOLS_TEST_SYSTEM_PRESSURE_OUTPUT",
            directory.path().join("pressure.csv"),
        )
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "隔离场景 {scenario} 失败：\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

// SPL-04：非共享写入错误仍必须报告原路径并停止正常状态。
#[test]
fn initial_and_later_write_failures_report_path_and_do_not_claim_running() {
    subprocess("write_failure");
}

// SPL-02/SPL-04：占用只阻止落盘，后台跨多个采样周期继续采集，释放后补写。
#[test]
fn occupied_csv_keeps_running_and_flushes_samples_collected_during_retry() {
    subprocess("occupied_csv");
}

// SPL-02/SPL-04：首次占用也能就绪，重复开始不重启，仍被占用时可以停止。
#[test]
fn initially_occupied_csv_starts_once_preserves_samples_and_stops_while_locked() {
    subprocess("initially_occupied_csv");
}

#[test]
fn scenario_entry() {
    match std::env::var(SCENARIO).as_deref() {
        Ok("write_failure") => write_failure(),
        Ok("occupied_csv") => occupied_csv(),
        Ok("initially_occupied_csv") => initially_occupied_csv(),
        Ok(other) => panic!("未知隔离场景 {other}"),
        Err(_) => {}
    }
}

fn write_failure() {
    let _guard = ServiceGuard;
    let path = output_path();
    std::fs::create_dir(&path).unwrap();
    let error =
        pressure::start_for_test(PathBuf::from(env!("CARGO_BIN_EXE_JchTools"))).unwrap_err();
    assert!(
        error.contains(path.to_str().unwrap()),
        "开始写失败必须包含原路径：{error}"
    );
    let failed = pressure::status().unwrap();
    assert!(!failed.running);
    assert_eq!(failed.error.as_deref(), Some(error.as_str()));
    assert!(path.is_dir(), "不可写时不能回退到其他输出");
    std::fs::remove_dir(&path).unwrap();
    assert!(
        pressure::start_for_test(PathBuf::from(env!("CARGO_BIN_EXE_JchTools")))
            .unwrap()
            .running
    );
    let _ = csv(1);
    // 把同一路径变为目录，制造非共享错误；占用重试不应吞掉这类失败。
    wait_for("将运行中的CSV路径替换为不可写目录", || {
        if path.is_dir() {
            return Some(());
        }
        let _ = std::fs::remove_file(&path);
        std::fs::create_dir(&path).ok()
    });
    let failed = wait_for("后续非共享写盘失败被真实状态呈现", || {
        let state = pressure::status().ok()?;
        (!state.running && state.error.is_some()).then_some(state)
    });
    assert!(failed.error.unwrap().contains(path.to_str().unwrap()));
    let stopped = pressure::stop().unwrap();
    assert!(!stopped.running);
    assert!(path.is_dir(), "非共享写失败后不能改目录或自行重启");
    assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
    assert_only_original_output();
}

fn assert_retrying(state: &pressure::Snapshot) {
    assert!(state.running, "CSV被占用时后台必须仍在运行：{state:?}");
    let warning = state.error.as_deref().expect("占用期间必须呈现重试说明");
    assert!(
        warning.contains(output_path().to_str().unwrap()),
        "重试说明必须包含实际输出路径：{warning}"
    );
    assert!(
        ["占用", "共享", "锁定"]
            .iter()
            .any(|word| warning.contains(word))
            && ["重试", "再试", "等待"]
                .iter()
                .any(|word| warning.contains(word)),
        "必须明确说明文件占用及继续重试，而非致命退出：{warning}"
    );
}

fn service_pid() -> u32 {
    let root = PathBuf::from(std::env::var_os("JCHTOOLS_TEST_STATE_DIR").unwrap());
    let pid = std::fs::read_to_string(root.join("system-pressure.ready"))
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert_ne!(pid, 0, "就绪文件必须标识真实后台进程");
    pid
}

fn assert_only_original_output() {
    let path = output_path();
    let entries: BTreeSet<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(
        entries,
        BTreeSet::from([
            path,
            PathBuf::from(std::env::var_os("JCHTOOLS_TEST_STATE_DIR").unwrap()),
        ]),
        "不能换目录、生成后备CSV或另开输出文件"
    );
}

fn buffered_timestamps(
    text: &str,
    held_from: chrono::DateTime<chrono::Utc>,
    released_at: chrono::DateTime<chrono::Utc>,
    minimum: usize,
) -> BTreeSet<chrono::DateTime<chrono::Utc>> {
    let samples: BTreeSet<_> = text
        .lines()
        .skip(1)
        .map(|line| {
            let columns: Vec<_> = line.split(',').collect();
            let timestamp = chrono::DateTime::parse_from_rfc3339(columns[0])
                .unwrap()
                .with_timezone(&chrono::Utc);
            let memory = columns[2].parse::<f64>().expect("占用期间仍采集真实内存");
            assert!((0.0..=100.0).contains(&memory));
            timestamp
        })
        .filter(|timestamp| *timestamp >= held_from && *timestamp < released_at)
        .collect();
    assert!(
        samples.len() >= minimum,
        "必须补写占用期间的多个真实时间样本，不能只在恢复后写一条：{samples:?}"
    );
    let timestamps: Vec<_> = samples.iter().collect();
    for pair in timestamps.windows(2) {
        assert!(
            (*pair[1] - *pair[0]).num_milliseconds() >= 1800,
            "重复开始不能新增并行采样，也不能恢复时突发伪造历史采样"
        );
    }
    samples
}

fn occupied_csv() {
    use std::os::windows::fs::OpenOptionsExt;
    let _guard = ServiceGuard;
    let path = output_path();
    let started = pressure::start_for_test(PathBuf::from(env!("CARGO_BIN_EXE_JchTools"))).unwrap();
    assert!(started.running && started.error.is_none());
    let _ = csv(1);
    let pid = service_pid();
    // 允许读、不共享写和删除，真实阻止原位写入及替换文件绕过占用。
    let reader = wait_for("以只共享读取方式占用CSV", || {
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .ok()
    });
    let held_from = chrono::Utc::now();
    let retained = std::fs::read_to_string(&path).unwrap();
    let first_timestamp = retained.lines().nth(1).unwrap().split(',').next().unwrap();
    let retrying = wait_for("占用错误被真实状态呈现", || {
        let state = pressure::status().ok()?;
        state.error.is_some().then_some(state)
    });
    assert_retrying(&retrying);
    for _ in 0..3 {
        // 固定等待仅用于跨过真实的2秒采样周期。
        std::thread::sleep(Duration::from_millis(2300));
        assert_retrying(&pressure::status().unwrap());
        assert_eq!(service_pid(), pid, "重试期间后台进程不能退出或重启");
        let repeated =
            pressure::start_for_test(PathBuf::from(env!("CARGO_BIN_EXE_JchTools"))).unwrap();
        assert_retrying(&repeated);
        assert_eq!(service_pid(), pid, "重复开始必须复用同一个后台");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), retained);
    }
    assert_only_original_output();
    let released_at = chrono::Utc::now();
    drop(reader);
    wait_for("释放占用后清除重试告警", || {
        let state = pressure::status().ok()?;
        (state.running && state.error.is_none()).then_some(())
    });
    let recovered = csv(4);
    assert!(
        recovered.contains(first_timestamp),
        "重试不能丢弃占用前采样"
    );
    let buffered = buffered_timestamps(&recovered, held_from, released_at, 3);
    for row in recovered.lines().skip(1) {
        let columns: Vec<_> = row.split(',').collect();
        let timestamp = chrono::DateTime::parse_from_rfc3339(columns[0])
            .unwrap()
            .with_timezone(&chrono::Utc);
        if buffered.contains(&timestamp) {
            let cpu = columns[1]
                .parse::<f64>()
                .expect("占用时仍应采集间隔CPU数据");
            assert!((0.0..=100.0).contains(&cpu));
        }
    }
    assert_eq!(service_pid(), pid, "释放占用后仍必须是原后台");
    assert_only_original_output();
    assert!(!pressure::stop().unwrap().running);
}

fn initially_occupied_csv() {
    use std::os::windows::fs::OpenOptionsExt;
    let _guard = ServiceGuard;
    let path = output_path();
    let retained = "首次启动前已存在的文件\n";
    std::fs::write(&path, retained).unwrap();
    let reader = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&path)
        .unwrap();
    let held_from = chrono::Utc::now();
    let started = pressure::start_for_test(PathBuf::from(env!("CARGO_BIN_EXE_JchTools")))
        .expect("首次写入遇到共享占用也必须返回就绪，而非杀死后台");
    let first_returned_at = chrono::Utc::now();
    assert_retrying(&started);
    let pid = service_pid();
    for _ in 0..2 {
        std::thread::sleep(Duration::from_millis(2300));
        assert_retrying(&pressure::status().unwrap());
        let repeated =
            pressure::start_for_test(PathBuf::from(env!("CARGO_BIN_EXE_JchTools"))).unwrap();
        assert_retrying(&repeated);
        assert_eq!(service_pid(), pid, "首次占用时重复开始不能重启后台");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), retained);
    }
    let released_at = chrono::Utc::now();
    drop(reader);
    wait_for("首次占用释放后清除告警并继续运行", || {
        let state = pressure::status().ok()?;
        (state.running && state.error.is_none()).then_some(())
    });
    let recovered = csv(3);
    let buffered = buffered_timestamps(&recovered, held_from, released_at, 3);
    assert!(
        buffered.first().unwrap() <= &first_returned_at,
        "重复开始必须保留首次返回前已采集但尚未写出的样本"
    );
    let first_row: Vec<_> = recovered.lines().nth(1).unwrap().split(',').collect();
    assert_eq!(first_row[1], "N/A", "首次缓冲样本不能被新一轮采集替换");
    assert_eq!(service_pid(), pid);
    // 在同一路径再次占用并等到真实写入失败，验证停止不会等待解锁。
    let reader = wait_for("停止前再次占用CSV", || {
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .ok()
    });
    let retained = std::fs::read_to_string(&path).unwrap();
    let retrying = wait_for("停止前真实进入占用重试", || {
        let state = pressure::status().ok()?;
        state.error.is_some().then_some(state)
    });
    assert_retrying(&retrying);
    assert_eq!(service_pid(), pid);
    let stopped = pressure::stop().expect("仍持有占用句柄时必须能停止后台");
    assert!(!stopped.running);
    assert!(!pressure::status().unwrap().running);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), retained);
    drop(reader);
    std::thread::sleep(Duration::from_millis(2300));
    assert!(!pressure::status().unwrap().running);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        retained,
        "停止后即使解除占用也不能继续落盘或重新启动"
    );
    assert_only_original_output();
}
