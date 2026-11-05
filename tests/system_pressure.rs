//! SPL：真实Windows计数器、独立进程生命周期和实际文件失败，不启动桌面。
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // 隔离测试夹具失败即断言失败。

use jchtools_system_pressure as pressure;
use pressure::output_path;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const SCENARIO: &str = "JCHTOOLS_PRESSURE_SCENARIO";
const LAUNCHER: &str = "JCHTOOLS_PRESSURE_LAUNCHER";

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
        .env_remove(LAUNCHER)
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

// SPL-01～SPL-03：真实采集、手动启停、单实例与启动器退出后续跑。
#[test]
fn independent_process_survives_launcher_deduplicates_and_stops_without_touching_csv() {
    subprocess("lifecycle");
}

// SPL-04：首次及运行中写入失败都必须报告原路径并停止正常状态。
#[test]
fn initial_and_later_write_failures_report_path_and_do_not_claim_running() {
    subprocess("write_failure");
}

#[test]
fn launcher_entry() {
    if std::env::var_os(LAUNCHER).is_none() {
        return;
    }
    let started = pressure::start_for_test(PathBuf::from(env!("CARGO_BIN_EXE_JchTools"))).unwrap();
    assert!(started.running && started.error.is_none());
    let _ = csv(1);
    // 不调用stop；让启动它的进程真实退出，后续场景检查后台仍独立记录。
}

#[test]
fn scenario_entry() {
    match std::env::var(SCENARIO).as_deref() {
        Ok("lifecycle") => lifecycle(),
        Ok("write_failure") => write_failure(),
        Ok(other) => panic!("未知隔离场景 {other}"),
        Err(_) => {}
    }
}

fn lifecycle() {
    let _guard = ServiceGuard;
    assert!(!pressure::status().unwrap().running);
    assert!(!output_path().exists(), "查询不能自动启动或写CSV");
    let launcher = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "launcher_entry", "--nocapture"])
        .env(LAUNCHER, "1")
        .output()
        .unwrap();
    assert!(
        launcher.status.success(),
        "启动器失败：{}",
        String::from_utf8_lossy(&launcher.stderr)
    );
    assert!(
        pressure::status().unwrap().running,
        "启动器已退出，后台仍应存活"
    );
    let first = csv(1);
    let first_row: Vec<_> = first.lines().nth(1).unwrap().split(',').collect();
    assert_eq!(first_row[1], "N/A", "首样本CPU没有足够间隔数据");
    assert_eq!(
        &first_row[4..7],
        &["N/A", "N/A", "N/A"],
        "首样本磁盘不伪造零"
    );
    let initial_timestamp = first_row[0].to_owned();
    for _ in 0..3 {
        assert!(
            pressure::start_for_test(PathBuf::from(env!("CARGO_BIN_EXE_JchTools")))
                .unwrap()
                .running
        );
    }
    let recorded = csv(3);
    assert!(
        recorded.contains(&initial_timestamp),
        "重复开始不能清空既有采样或新开一轮"
    );
    let mut samples: BTreeMap<&str, BTreeSet<u32>> = BTreeMap::new();
    let mut actual_cpu = false;
    let mut actual_io = false;
    for row in recorded
        .lines()
        .skip(1)
        .map(|line| line.split(',').collect::<Vec<_>>())
    {
        let number = row[3].parse::<u32>().expect("真实物理磁盘编号");
        assert!(
            samples.entry(row[0]).or_default().insert(number),
            "同次采样同一物理盘不能按分区重复"
        );
        let memory = row[2].parse::<f64>().expect("真实物理内存使用率");
        assert!((0.0..=100.0).contains(&memory));
        if row[0] != initial_timestamp {
            let cpu = row[1].parse::<f64>().expect("完整间隔后的真实CPU使用率");
            let busy = row[4].parse::<f64>().expect("完整间隔后的物理盘繁忙率");
            let read = row[5].parse::<f64>().expect("真实磁盘读取MiB/s");
            let write = row[6].parse::<f64>().expect("真实磁盘写入MiB/s");
            assert!((0.0..=100.0).contains(&cpu));
            assert!((0.0..=100.0).contains(&busy));
            assert!(read.is_finite() && read >= 0.0 && write.is_finite() && write >= 0.0);
            actual_cpu = true;
            actual_io = true;
        }
    }
    assert!(actual_cpu && actual_io);
    let timestamps: Vec<_> = samples
        .keys()
        .map(|value| chrono::DateTime::parse_from_rfc3339(value).unwrap())
        .collect();
    for pair in timestamps.windows(2) {
        assert!(
            (pair[1] - pair[0]).num_milliseconds() >= 1800,
            "重复开始不能产生并行重复采样"
        );
    }
    let stopped = Command::new(env!("CARGO_BIN_EXE_JchTools"))
        .args(["--system-pressure-service", "--stop"])
        .status()
        .unwrap();
    assert!(
        stopped.success(),
        "内部停止入口必须等待停止再返回，不落入GUI"
    );
    assert!(!pressure::status().unwrap().running);
    let retained = std::fs::read_to_string(output_path()).unwrap();
    std::thread::sleep(Duration::from_millis(2300));
    assert_eq!(
        std::fs::read_to_string(output_path()).unwrap(),
        retained,
        "确认停止后不能继续更新，文件仍保留"
    );
    assert!(!pressure::stop().unwrap().running, "重复停止幂等");
}

fn write_failure() {
    use std::io::Read as _;
    use std::os::windows::fs::OpenOptionsExt;
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
    // 拒绝共享写入，让下一次真实周期更新产生Windows sharing violation。
    let mut exclusive_reader = wait_for("独占读取已有CSV", || {
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .ok()
    });
    let mut retained = String::new();
    exclusive_reader.read_to_string(&mut retained).unwrap();
    let failed = wait_for("后续写盘失败被真实状态呈现", || {
        let state = pressure::status().ok()?;
        (!state.running && state.error.is_some()).then_some(state)
    });
    assert!(failed.error.unwrap().contains(path.to_str().unwrap()));
    drop(exclusive_reader);
    let stopped = pressure::stop().unwrap();
    assert!(!stopped.running);
    std::thread::sleep(Duration::from_millis(2300));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        retained,
        "写失败后停止，不改目录或自行重启"
    );
}
