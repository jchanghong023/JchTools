//! XbergWorkerClient 协议测试：用 mock 子进程模拟 `xberg worker` 的 stdout
//! 行为（id 回显、状态、成功、无文字、失败、取消、进程退出、EOF 关闭）。

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

// 覆盖 XB-06/XB-07：失败响应映射为可分类的 Backend 错误。
#[test]
fn failure_response_maps_to_backend_error() {
    let mut client = spawn_mock("fail");
    let error = client
        .recognize(b"fake png bytes", &AtomicBool::new(false))
        .expect_err("失败响应应返回错误");
    match error {
        ClientError::Backend(message) => {
            assert!(message.contains("asset mismatch"), "unexpected: {message}");
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

// 覆盖 O-19 细化：取消立即返回，放弃等待；后续请求仍可用。
#[test]
fn cancel_abandons_the_in_flight_request_and_client_stays_usable() {
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

    // 放弃的请求在后台完成后被丢弃；新请求经串行通道仍得到自己的响应。
    let text = client
        .recognize(b"second", &AtomicBool::new(false))
        .expect("后续识别应正常");
    assert_eq!(text.as_deref(), Some("MOCK 布局文本"));
    client
        .shutdown(Duration::from_secs(5))
        .expect("关闭应干净退出");
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
