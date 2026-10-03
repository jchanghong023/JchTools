//! XbergWorkerClient 协议测试：用 mock 子进程模拟 `xberg worker` 的 stdout
//! 行为（id 回显、状态、成功、无文字、失败、取消、进程退出、EOF 关闭）。

// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use snap_ocr_worker::xberg_worker::{ClientError, SnapshotState, XbergWorkerClient};

const MOCK_EXE: &str = env!("CARGO_BIN_EXE_mock-xberg-worker");

fn spawn_mock(mode: &str) -> XbergWorkerClient {
    let mut command = Command::new(MOCK_EXE);
    command
        .arg(mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    match XbergWorkerClient::spawn_command(command) {
        Ok(client) => client,
        Err(error) => panic!("mock 子进程应能启动：{error}"),
    }
}

fn cancel_after(flag: std::sync::Arc<AtomicBool>, delay: Duration) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        flag.store(true, Ordering::Release);
    })
}

// 覆盖 XB-05：id 回显与响应关联；成功文本与状态查询的映射。
#[test]
fn state_and_recognition_round_trip() {
    let mut client = spawn_mock("ok");

    assert_eq!(
        client.snapshot_state().expect("状态查询应成功"),
        SnapshotState::Ready
    );

    let text = client
        .recognize(b"fake png bytes", &AtomicBool::new(false))
        .expect("识别应成功");
    assert_eq!(text.as_deref(), Some("MOCK 布局文本"));

    // 串行复用同一子进程：第二个请求仍得到正确 id 关联的响应。
    let again = client
        .recognize(b"second", &AtomicBool::new(false))
        .expect("第二次识别应成功");
    assert_eq!(again.as_deref(), Some("MOCK 布局文本"));

    client
        .shutdown(Duration::from_secs(5))
        .expect("关闭应干净退出");
}

// 覆盖 XB-07/O-30：无文字图片是成功响应（ok:true + text:"" + no_text）。
#[test]
fn empty_text_maps_to_none() {
    let mut client = spawn_mock("ok");
    // 客户端对字节做 base64：base64("NOTEXT") == "Tk9URVhU"，命中 mock 的无文字标记。
    let text = client
        .recognize(b"NOTEXT", &AtomicBool::new(false))
        .expect("无文字图不应报错");
    assert_eq!(text, None, "空文本应映射为 None");
    client
        .shutdown(Duration::from_secs(5))
        .expect("关闭应干净退出");
}

// 覆盖 XB-07：状态查询的 error 态如实透出，不冒称就绪。
#[test]
fn error_state_is_reported_with_summary() {
    let mut client = spawn_mock("state-error");
    match client.snapshot_state() {
        Ok(SnapshotState::Error(message)) => {
            assert!(message.contains("asset missing"), "unexpected: {message}");
        }
        Ok(state) => panic!("应报告 error 状态，实际 {state:?}"),
        Err(error) => panic!("状态查询本身应成功：{error}"),
    }
    client
        .shutdown(Duration::from_secs(5))
        .expect("关闭应干净退出");
}

// 覆盖 XB-06/XB-07（修复1回归）：失败响应映射为可分类的 Backend 错误，且
// 结构化 `error_kind` 必须透传——真实 Xberg 的错误文本是英文（如 "snapshot
// model asset missing"），服务端 warm_up 分类依赖 kind 而非中文子串；修复前
// 客户端只取 `error` 文本、丢弃 kind，分类启发式必然落空（本测试无法编译）。
#[test]
fn failure_response_maps_to_backend_error() {
    let mut client = spawn_mock("fail");
    let error = client
        .recognize(b"fake png bytes", &AtomicBool::new(false))
        .expect_err("失败响应应返回错误");
    match error {
        ClientError::Backend { message, kind } => {
            assert!(message.contains("asset mismatch"), "unexpected: {message}");
            assert_eq!(
                kind.as_deref(),
                Some("asset_invalid"),
                "error_kind 应原样透传（与 Xberg snapshot_ocr.rs 的 5 值对齐），实际 {kind:?}"
            );
        }
        other => panic!("应为 Backend 错误，实际 {other:?}"),
    }
    // 单请求失败不结束批次：后续请求仍被应答（WORKER.md 故障隔离）。
    assert_eq!(
        client.snapshot_state().expect("失败后进程仍在服务"),
        SnapshotState::Ready
    );
    client
        .shutdown(Duration::from_secs(5))
        .expect("关闭应干净退出");
}

// 覆盖 XB-08/O-19（语义修正版，2026-09-29）：取消立即返回；随后 abort() 走
// 「终止进程」路径——不等慢响应完成即杀死子进程，被终止的客户端立即可诊断
// （ProcessExited），新请求不会再排在被放弃的旧请求之后；重载即重新 spawn。
// 旧断言「取消后 client 保持可用」不再成立：stdio 单连接无法只取消单请求，
// 合同 XB-08 选择终止进程，由服务侧重载恢复识别能力。
#[test]
fn cancel_returns_promptly_and_abort_terminates_child_immediately() {
    let mut client = spawn_mock("slow");
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let _watchdog = cancel_after(std::sync::Arc::clone(&cancel), Duration::from_millis(200));
    let started = std::time::Instant::now();
    let error = client
        .recognize(b"first", &cancel)
        .expect_err("取消后应返回错误");
    assert!(
        matches!(error, ClientError::Cancelled),
        "应为取消错误，实际 {error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "取消应立即生效，而不是等待响应（耗时 {:?}）",
        started.elapsed()
    );

    // abort 在被放弃的慢响应（还剩约 800ms）完成前返回：进程被立即终止，
    // 旧请求随之死亡，这就是「新任务不被旧请求阻塞」的进程级保证。
    let aborted_at = std::time::Instant::now();
    client.abort();
    assert!(
        aborted_at.elapsed() < Duration::from_millis(500),
        "abort 不应等待在途慢响应（耗时 {:?}）",
        aborted_at.elapsed()
    );

    // 被终止的客户端立即可诊断：后续请求马上失败，不会排队等旧响应。
    let failed_at = std::time::Instant::now();
    let error = client
        .recognize(b"second", &AtomicBool::new(false))
        .expect_err("被终止的客户端应立即报错");
    assert!(
        matches!(error, ClientError::ProcessExited(_)),
        "应为子进程退出错误，实际 {error:?}"
    );
    assert!(
        failed_at.elapsed() < Duration::from_millis(300),
        "死亡客户端的诊断不应等待（耗时 {:?}）",
        failed_at.elapsed()
    );

    // 重载路径 = 重新 spawn：新客户端的请求不被旧请求拖累，识别照常成功。
    let mut fresh = spawn_mock("ok");
    let text = fresh
        .recognize(b"second", &AtomicBool::new(false))
        .expect("重载后识别应正常");
    assert_eq!(text.as_deref(), Some("MOCK 布局文本"));
    fresh
        .shutdown(Duration::from_secs(5))
        .expect("新客户端关闭应干净退出");
    client
        .shutdown(Duration::from_secs(5))
        .expect("已终止客户端的关闭应成功");
}

// 覆盖 O-13/O-30（E'低危4 回归）：半行截断必须升级为进程退出分类。mock 写出
// 完整 JSON 但不带换行终止后立即退出——协议帧残缺 + 进程已死，连接不可复用；
// 若把无终止符的残行当有效响应收纳，会得到 Backend 分类，服务误以为连接仍可用，
// 对同一连接 retry 必然无效（半死连接）。期望分类：ProcessExited。
#[test]
fn truncated_response_line_is_reported_as_process_exit() {
    let mut client = spawn_mock("half-line");
    let error = client
        .recognize(b"payload", &AtomicBool::new(false))
        .expect_err("截断响应应返回错误");
    assert!(
        matches!(error, ClientError::ProcessExited(_)),
        "半行截断应升级为进程退出分类，实际 {error:?}"
    );
    client
        .shutdown(Duration::from_secs(5))
        .expect("已退出子进程的关闭应成功");
}

// 覆盖 XB-08/O-13（修复2回归）：挂起子进程（活着但不读 stdin/不写 stdout）
// 必须被请求级超时解救——注入 300ms 超时后，识别在超时+余量内返回
// ClientError::Timeout，服务同路径 abort 终止 mock 进程；修复前无超时机制，
// wait_response 无 deadline 轮询、永久阻塞（本测试无法编译）。
#[test]
fn write_timeout_cancels_blocked_stdin_write() {
    // 覆盖写阶段看门狗：payload 超过管道缓冲且子进程不读 stdin，写入阻塞在
    // 内核；看门狗在超时后 CancelIoEx 解除阻塞，按请求级超时返回（而非永久
    // 挂死在 write_all）。
    let mut client = spawn_mock("hang").with_request_timeout(Duration::from_millis(300));
    let started = std::time::Instant::now();
    let oversized = vec![0u8; 512 * 1024];
    let error = client
        .recognize(&oversized, &AtomicBool::new(false))
        .expect_err("写阻塞应按请求级超时返回错误");
    assert!(
        matches!(error, ClientError::Timeout),
        "写阻塞应被看门狗解救并按超时分类，实际 {error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "写阻塞应在注入超时+余量内返回，实际耗时 {:?}",
        started.elapsed()
    );
}

#[test]
fn request_timeout_frees_hanging_recognition_and_child_is_terminated() {
    let mut client = spawn_mock("hang").with_request_timeout(Duration::from_millis(300));
    let started = std::time::Instant::now();
    let error = client
        .recognize(b"payload", &AtomicBool::new(false))
        .expect_err("挂起子进程应按超时返回错误");
    assert!(
        matches!(error, ClientError::Timeout),
        "应为请求级超时错误，实际 {error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "识别应在注入超时+余量内返回，实际耗时 {:?}",
        started.elapsed()
    );

    // 服务侧同路径处理（worker 线程对 TimedOut 调 abort）：挂起进程被终止，
    // 不得残留（mock 长眠 60 秒，只有显式终止会让它先死）。
    let pid = client.child_id();
    client.abort();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    #[cfg(windows)]
    while process_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "超时 abort 后 mock 子进程（PID {pid}）仍存活：挂起进程未被终止"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    client
        .shutdown(Duration::from_secs(5))
        .expect("已终止客户端的关闭应成功");
}

// 覆盖 O-13：子进程意外退出被如实报告（携带退出码），供服务给出重试入口。
#[test]
fn unexpected_process_exit_is_reported() {
    let mut client = spawn_mock("exit-after-first");
    let text = client
        .recognize(b"first", &AtomicBool::new(false))
        .expect("第一条请求应正常应答");
    assert_eq!(text.as_deref(), Some("MOCK 布局文本"));

    let error = client
        .recognize(b"second", &AtomicBool::new(false))
        .expect_err("子进程退出后应报错");
    assert!(
        matches!(error, ClientError::ProcessExited(Some(3))),
        "应报告退出码 3，实际 {error:?}"
    );
    client
        .shutdown(Duration::from_secs(5))
        .expect("已退出的子进程关闭应成功");
}

// 覆盖 O-16：shutdown 关闭 stdin 后等子进程退出；Ok 即表示子进程已被回收。
#[test]
fn shutdown_closes_stdin_and_child_exits_cleanly() {
    let mut client = spawn_mock("ok");
    assert_eq!(
        client.snapshot_state().expect("状态查询应成功"),
        SnapshotState::Ready
    );
    // mock 在 stdin EOF 后以退出码 0 退出：Ok(()) 证明等到了正常退出而不是强杀。
    client
        .shutdown(Duration::from_secs(5))
        .expect("关闭应干净退出");
}

// 覆盖 XB-04/O-29：请求行是单行 JSON，id 由调用方生成且逐请求递增。
#[test]
fn requests_carry_unique_ids_as_single_lines() {
    let mut client = spawn_mock("state-uninit");
    // 状态查询在识别前是 uninitialized（模型懒加载，预热后 ready）。
    assert_eq!(
        client.snapshot_state().expect("状态查询应成功"),
        SnapshotState::Uninitialized
    );
    let text = client
        .recognize(b"warmup", &AtomicBool::new(false))
        .expect("预热识别应成功");
    assert_eq!(text.as_deref(), Some("MOCK 布局文本"));
    assert_eq!(
        client.snapshot_state().expect("状态查询应成功"),
        SnapshotState::Ready,
        "预热后模型状态应为 ready（O-13）"
    );
    client
        .shutdown(Duration::from_secs(5))
        .expect("关闭应干净退出");
}

// 防回归：stdin 写入失败（无管道）被诊断为进程退出/通信错误，而非 panic。
#[test]
fn spawn_without_stdio_is_rejected() {
    let mut command = Command::new(MOCK_EXE);
    command.arg("ok").stdin(Stdio::null()).stdout(Stdio::null());
    let error = XbergWorkerClient::spawn_command(command).expect_err("缺少标准流应启动失败");
    assert!(error.contains("标准流"), "unexpected: {error}");
    let _ = std::io::stdout().flush();
}

// E-1 回归：xberg.exe 是控制台子系统（CUI）程序，本服务是 GUI 子系统、无控制台；
// spawn_command 若不带 CREATE_NO_WINDOW，Windows 会为子进程新建常驻黑窗（任务栏
// 可见，误关即杀死推理子进程）。mock 的 console-probe 模式探测自身 GetConsoleWindow：
// 未加标志时继承/新建控制台 → "console-present"；带 CREATE_NO_WINDOW 时控制台句柄
// 未设置 → "no-console"。
#[test]
fn spawned_child_runs_without_console_window() {
    let mut client = spawn_mock("console-probe");
    let text = client
        .recognize(b"probe", &AtomicBool::new(false))
        .expect("探测请求应成功");
    assert_eq!(
        text.as_deref(),
        Some("no-console"),
        "经 spawn_command 启动的子进程不应持有控制台窗口（CREATE_NO_WINDOW 未生效）"
    );
    client
        .shutdown(Duration::from_secs(5))
        .expect("关闭应干净退出");
}

// 在临时目录布置一个「形似 Xberg 组件目录」的最小布局：mock 充当 xberg.exe，
// 运行库与模型目录用空占位（spawn 只组装启动配置，不读取内容）。
fn lay_out_component_dir(dir: &std::path::Path) {
    std::fs::copy(MOCK_EXE, dir.join("xberg.exe")).expect("应能布置 mock xberg.exe");
    std::fs::write(dir.join("onnxruntime.dll"), b"").expect("应能布置运行库占位文件");
    std::fs::create_dir_all(dir.join("models").join("snapshot-ocr")).expect("应能布置模型目录占位");
}

// B'-1 回归（XB-04 口径）：生产 spawn() 启动的推理子进程必须带上与
// src/markdown.rs `media_worker_environment` 同口径的离线开关——worker 内部
// 可能存在 HF hub 回退，缺开关即把截图 OCR 推理子进程置于离线防线之外。
// mock 以 env-probe 模式回报自身实际环境；修复前 HF_HUB_OFFLINE 等为
// `<unset>`，本测试应失败（红）。
#[test]
fn production_spawn_injects_offline_switch_environment() {
    let component = tempfile::tempdir().expect("应能创建临时组件目录");
    lay_out_component_dir(component.path());
    // 生产 spawn 不传模式参数，经继承环境让 mock 走 env-probe 探针（显式参数
    // 模式优先于该变量，并行运行的其余 mock 测试不受影响）。
    std::env::set_var("MOCK_XBERG_MODE", "env-probe");
    let client =
        XbergWorkerClient::spawn(component.path()).expect("生产 spawn 应能从 mock 组件目录启动");
    std::env::remove_var("MOCK_XBERG_MODE");
    let mut client = client;

    let report = client
        .recognize(b"probe", &AtomicBool::new(false))
        .expect("env-probe 请求应成功")
        .unwrap_or_default();
    let mut vars = std::collections::HashMap::<String, String>::new();
    for pair in report.split(';') {
        let Some((key, value)) = pair.split_once('=') else {
            panic!("env-probe 回报格式应为 k=v，实际 {pair:?}");
        };
        vars.insert(key.to_owned(), value.to_owned());
    }
    let mut expected = std::collections::HashMap::<&str, &str>::from([
        ("HF_HUB_OFFLINE", "1"),
        ("HUGGINGFACE_HUB_OFFLINE", "1"),
        ("TRANSFORMERS_OFFLINE", "1"),
        ("HF_DATASETS_OFFLINE", "1"),
        ("NO_COLOR", "1"),
        ("XBERG_ORT_EP", "cpu"),
        ("XBERG_MAX_CONCURRENT_REQUESTS", "1"),
    ]);
    // 先断言键集合完整，避免逐项断言时缺多个键只报第一个。
    for key in expected.keys() {
        assert!(
            vars.get(*key).map(String::as_str) != Some("<unset>"),
            "推理子进程环境缺少 {key}（XB-04 离线开关未注入），实际回报：{report}"
        );
    }
    for (key, want) in expected.drain() {
        assert_eq!(
            vars.get(key).map(String::as_str),
            Some(want),
            "{key} 取值应与 markdown 侧离线口径一致，实际回报：{report}"
        );
    }
    let want_dll = component.path().join("onnxruntime.dll");
    assert_eq!(
        vars.get("ORT_DYLIB_PATH").map(String::as_str),
        Some(want_dll.to_string_lossy().as_ref()),
        "ORT_DYLIB_PATH 应指向组件目录内运行库，实际回报：{report}"
    );
    client
        .shutdown(Duration::from_secs(5))
        .expect("关闭应干净退出");
}

// E2'-1 回归（O-16「释放模型」）：服务进程在「强制退出」（std::process::exit(0)，
// 不运行任何 Drop/清理）或被外部杀死后，卡在不可中断推理中的推理子进程必须
// 由内核终结，不得成为无窗口孤儿。复现方式：重入本测试二进制里的 helper
// 「测试」，由它 spawn mock(hang) 后立即 exit(0)；外层轮询 mock PID 的存活性。
// 修复前（spawn 不挂 kill-on-close Job）：mock 孤儿存活，本测试失败（红）。
#[cfg(windows)]
#[test]
fn abrupt_service_death_terminates_inference_child() {
    let mut helper = Command::new(std::env::current_exe().expect("应能定位本测试二进制"))
        .args(["--exact", "--nocapture", "force_exit_helper"])
        .env(FORCE_EXIT_HELPER_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("应能重入测试二进制执行强退复现");
    let mut helper_stdout = helper.stdout.take().expect("应能读取 helper stdout");
    // 不用 wait_with_output：mock 孙进程可能经句柄继承握着 helper 的 stdout
    // 写端，EOF 要等 mock 自己退出才出现；逐行读到 PID 标记即继续。
    let pid_line = {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(&mut helper_stdout);
        let mut found = None;
        for line in reader.lines() {
            let Ok(line) = line else {
                break;
            };
            if let Some(pid) = line.strip_prefix("JT_SNAP_CHILD_PID=") {
                found = Some(pid.to_owned());
                break;
            }
        }
        found.unwrap_or_else(|| panic!("helper 应回报 mock 子进程 PID"))
    };
    drop(helper_stdout);
    let status = helper.wait().expect("强退复现 helper 应能退出");
    assert!(
        status.success(),
        "helper 应以退出码 0 结束（exit(0) 模拟强退），实际 {status}"
    );
    let pid: u32 = pid_line
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("PID 应为数字，实际 {pid_line:?}"));

    // 服务进程死亡后，内核应很快终结其推理子进程（Job 句柄随进程回收而关闭）。
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while process_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "服务进程强退后推理子进程（PID {pid}）仍存活：kill-on-close 兜底未生效，\
             xberg.exe 将以 CREATE_NO_WINDOW 孤儿常驻（违背 O-16 释放模型）"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

// 上面的复现 helper：仅在被父测试以环境变量重入时执行，正常测试运行为空操作。
// 模拟服务进程的强退路径——spawn 成功后立即 exit(0)，不触发任何 Drop/清理。
#[test]
fn force_exit_helper() {
    const CHILD_PID_PREFIX: &str = "JT_SNAP_CHILD_PID=";
    if std::env::var_os(FORCE_EXIT_HELPER_ENV).is_none() {
        return;
    }
    let mut command = Command::new(MOCK_EXE);
    command
        .arg("hang")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let client = match XbergWorkerClient::spawn_command(command) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("helper：mock 子进程启动失败：{error}");
            std::process::exit(2);
        }
    };
    println!("{CHILD_PID_PREFIX}{}", client.child_id());
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

/// 环境标记：父测试以该变量重入 `force_exit_helper`，执行真实的强退复现分支。
const FORCE_EXIT_HELPER_ENV: &str = "JT_SNAP_FORCE_EXIT_HELPER";

/// 以 PID 探测进程是否仍存活（打开同步句柄短等：超时=存活，已触发=已退出）。
#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    const WAIT_TIMEOUT: u32 = 258;
    // SAFETY: 只请求同步权限打开测试进程句柄；失败返回 null 即视为已退出。
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        // 进程已不存在（同用户测试进程，权限不足情形可忽略）。
        return false;
    }
    // SAFETY: 句柄刚打开且仅在本函数使用，短等待不产生副作用。
    let waited = unsafe { WaitForSingleObject(handle, 200) };
    // SAFETY: 等待结束即关闭句柄，此后不再使用。
    unsafe { CloseHandle(handle) };
    waited == WAIT_TIMEOUT
}
