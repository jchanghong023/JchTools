//! `protocol` 内联测试模块（管道身份摘要、监听恢复与控制请求期限回归）；自 protocol.rs 搬移并顶格化。

use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;

use super::{serve_named, Command};

static NEXT_PIPE: AtomicU64 = AtomicU64::new(0);

// F40 / O-11 / O-14 / XB-22：同一令牌用户及登录会话不能因继承环境
// 改名而变成另一个控制端点。只计算名称，不打开用户管道或单实例锁。
#[test]
fn review_f40_pipe_identity_ignores_inherited_user_environment(
) -> Result<(), Box<dyn std::error::Error>> {
    const CHILD: &str = "JCHTOOLS_REVIEW_F40_IDENTITY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "service::protocol::tests::review_f40_pipe_identity_ignores_inherited_user_environment",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env_remove("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT")
            .status()?;
        assert!(status.success(), "同一令牌用户的控制端点不应受继承环境影响");
        return Ok(());
    }
    // 环境修改只发生在本测试独占子进程；没有并行测试或后台服务。
    std::env::set_var("USERNAME", "review-f40-first-display-name");
    std::env::set_var("USERPROFILE", r"C:\review-f40-first-profile");
    let endpoint = super::pipe_name()?;
    use sha2::Digest as _;
    let token_key = jchtools_foundation::process_identity::user_session_key()?;
    let digest = hex::encode(sha2::Sha256::digest(token_key.as_bytes()));
    assert_eq!(
        endpoint,
        format!(r"\\.\pipe\jchtools-snap-ocr-{}", &digest[..16]),
        "生产端点必须与 GUI 使用同一令牌身份摘要"
    );
    for (username, profile) in [
        (
            Some("review-f40-second-display-name"),
            Some(r"C:\review-f40-first-profile"),
        ),
        (None, Some(r"C:\review-f40-first-profile")),
        (None, Some(r"C:\review-f40-second-profile")),
        (None, None),
    ] {
        for (name, value) in [("USERNAME", username), ("USERPROFILE", profile)] {
            if let Some(value) = value {
                std::env::set_var(name, value);
            } else {
                std::env::remove_var(name);
            }
        }
        assert_eq!(
            super::pipe_name()?,
            endpoint,
            "相同令牌与 SessionId 必须连接原截图服务，而不是创建第二个端点"
        );
    }
    Ok(())
}

#[derive(Clone)]
struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// 覆盖 O-14 / XB-22：监听调用前已关闭的客户端不能耗尽唯一控制管道实例。
#[test]
fn client_closed_before_accept_does_not_block_next_request(
) -> Result<(), Box<dyn std::error::Error>> {
    let name = format!(
        r"\\.\pipe\jchtools-snap-ocr-test-{}-{}",
        std::process::id(),
        NEXT_PIPE.fetch_add(1, Ordering::Relaxed)
    );
    let (commands_tx, commands_rx) = std::sync::mpsc::channel();
    // 零容量握手把服务线程停在 CreateNamedPipeW 与 ConnectNamedPipe 之间。
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
    let server_name = name.clone();
    std::thread::spawn(move || {
        serve_named(
            &server_name,
            &commands_tx,
            &ready_tx,
            Duration::from_secs(2),
        );
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let early_client = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
        {
            Ok(pipe) => break pipe,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    };
    drop(early_client);
    ready_rx.recv_timeout(Duration::from_secs(2))??;
    std::thread::spawn(move || {
        if let Ok(Command::Pipe(_, response, _)) = commands_rx.recv() {
            let _ = response.send(json!({"ok":true}));
        }
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut next = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
        {
            Ok(pipe) => break pipe,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    };
    next.write_all(b"{\"command\":\"ping\"}\n")?;
    let mut response = String::new();
    BufReader::new(&mut next).read_line(&mut response)?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response)?["ok"],
        true
    );
    Ok(())
}

// 覆盖 O-11：单个无响应客户端不能让常驻服务的控制入口永久失效。
#[test]
fn unfinished_client_does_not_block_next_control_request() -> Result<(), Box<dyn std::error::Error>>
{
    // tracing 的 callsite interest 是进程级缓存。唯一局部 subscriber 存在时，
    // 并行测试的无 subscriber 线程首次登记同一控制事件或 operation span，
    // 可把 interest 缓存为 never，使本线程的事件或关联 ID 不可见。
    // 与日志测试同样隔离整个真实管道场景，不改变服务期限或放宽终态断言。
    const CHILD: &str = "JCHTOOLS_OCR_CONTROL_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "service::protocol::tests::unfinished_client_does_not_block_next_control_request",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .status()?;
        assert!(
            status.success(),
            "隔离进程中的真实控制管道与诊断断言必须通过"
        );
        return Ok(());
    }

    let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = LogCapture(logs.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || capture.clone())
        .finish();
    let name = format!(
        r"\\.\pipe\jchtools-snap-ocr-test-{}-{}",
        std::process::id(),
        NEXT_PIPE.fetch_add(1, Ordering::Relaxed)
    );
    let (commands_tx, commands_rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let server_name = name.clone();
    std::thread::spawn(move || {
        tracing::subscriber::with_default(subscriber, || {
            serve_named(
                &server_name,
                &commands_tx,
                &ready_tx,
                Duration::from_millis(300),
            );
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(2))??;
    std::thread::spawn(move || {
        while let Ok(Command::Pipe(_, response, _)) = commands_rx.recv() {
            let _ = response.send(json!({"ok":true}));
        }
    });

    let mut unfinished = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&name)?;
    unfinished.write_all(b"{\"command\":\"ping\"")?;
    std::thread::sleep(Duration::from_millis(50));

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut next = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
        {
            Ok(pipe) => break pipe,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    };
    // 读完响应后仍保持客户端开启，等服务端刷新并记录终态再关闭，
    // 避免关闭管道与 FlushFileBuffers 竞态把已读取的响应判为交付失败。
    next.write_all(b"{\"command\":\"ping\",\"diagnostic_id\":\"test-control-123\",\"private\":\"DO_NOT_LOG_OCR_BODY\"}\n")?;
    let mut response = String::new();
    BufReader::new(&mut next).read_line(&mut response)?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response)?["ok"],
        true
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response)?["diagnostic_id"],
        "test-control-123"
    );
    let observing = Instant::now();
    let deadline = observing + Duration::from_secs(2);
    let captured = loop {
        let captured = String::from_utf8(
            logs.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )?;
        if captured.lines().any(|line| {
            line.contains("ocr_control_request_completed") && line.contains("test-control-123")
        }) {
            break captured;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "截图控制日志终态未到达：observation_elapsed_ms={} request_timeout_ms=300 response={response:?}\n{captured}",
                observing.elapsed().as_millis()
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(next);
    drop(unfinished);

    // 完整请求的客户端若不读取响应，FlushFileBuffers 也不能永久占住服务。
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut unread = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
        {
            Ok(pipe) => break pipe,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    };
    unread.write_all(b"{\"command\":\"ping\"}\n")?;
    std::thread::sleep(Duration::from_millis(50));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut after_unread = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
        {
            Ok(pipe) => break pipe,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    };
    after_unread.write_all(b"{\"command\":\"ping\"}\n")?;
    response.clear();
    BufReader::new(&mut after_unread).read_line(&mut response)?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response)?["ok"],
        true
    );
    // P-10：成功终态须携带本请求关联、真实客户端 PID 与耗时；
    // 不锁定内部解码步骤的事件清单。控制入口的恢复仍由上述真实响应证明。
    assert!(captured.lines().any(|line| {
        line.contains("test-control-123")
            && line.contains(&format!("peer_pid={}", std::process::id()))
            && line.contains("elapsed_ms=")
    }));
    assert!(captured.lines().any(|line| {
        line.contains("stage=\"read_deadline\"") && line.contains("error_type=\"timeout\"")
    }));
    assert!(!captured.contains("DO_NOT_LOG_OCR_BODY"));
    Ok(())
}
