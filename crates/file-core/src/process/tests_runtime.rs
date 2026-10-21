use super::capture::read_all_capped;
use super::pipe::{is_metadata_line, pump, summarize_failure, PipeMessage};
use super::*;
use crate::control::Control;
use anyhow::bail;
use proptest::{collection, prelude::*};
use std::collections::VecDeque;
use std::io::Read;
use std::process::Command;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
fn process_tree_tmp_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|directory| directory.join("Cargo.lock").is_file())
        .expect("进程测试必须位于仓库工作区")
        .join(".tmp/parallel-review")
}

/// 关闭失败持久化：默认会在源码旁写 proptest-regressions 文件，违反仓库的 .tmp/ 规则；
/// 失败用例由 panic 消息里的 minimal failing input 直接给出，无需落盘重放。
fn config() -> ProptestConfig {
    let mut config = ProptestConfig::with_cases(256);
    config.failure_persistence = None;
    config
}

/// 行内容字符：允许任意非 CR/LF 字符（含中文、控制字符、等号等），
/// 只排除会改变分行语义的两个字符本身。
fn any_line() -> impl Strategy<Value = String> {
    collection::vec(
        any::<char>().prop_filter("排除 CR/LF", |c| *c != '\r' && *c != '\n'),
        0..16,
    )
    .prop_map(|chars| chars.into_iter().collect())
}
/// 生成无歧义的（行内容, 行结束符种子）组合：最后一段非空（「末尾空行」与
/// 「结尾终止符」在字节上不可区分），且排除「\r 分隔 + 空行 + \n 分隔」——
/// 该组合会拼接成一个 CRLF（单终止符，这是正确语义），留着会把正确合并当吞行。
/// 策略提到命名函数：proptest 语句糖的参数列表里出现闭包管道符会解析失败。
fn unambiguous_lines_and_seps() -> impl Strategy<Value = (Vec<String>, Vec<u8>)> {
    (
        collection::vec(any_line(), 1..12).prop_filter("最后一段非空", |v| {
            v.last().is_some_and(|l| !l.is_empty())
        }),
        collection::vec(any::<u8>(), 12),
    )
        .prop_filter("排除跨空行合并歧义", |(lines, seps)| {
            (1..lines.len().saturating_sub(1)).all(|j| {
                !(seps[j % seps.len()] % 3 == 2
                    && lines[j].is_empty()
                    && seps[(j + 1) % seps.len()] % 3 == 1)
            })
        })
}

// 属性化回归（2026-09-11 CRLF 缺陷）：任意 \r\n / \n / \r 混排分隔下，
// 行内容与顺序必须原样保留，不得多出空行或吞行。
proptest! {
    #![proptest_config(config())]
    #[test]
    fn pump_splits_on_any_line_ending_combination(combined in unambiguous_lines_and_seps()) {
        let (lines, seps) = combined;
        let mut data = String::new();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                data.push_str(match seps[index % seps.len()] % 3 { 0 => "\r\n", 1 => "\n", _ => "\r" });
            }
            data.push_str(line);
        }
        let (send, recv) = mpsc::sync_channel(1024);
        let handle = pump(std::io::Cursor::new(data.clone().into_bytes()), false, send);
        handle.join().unwrap();
        let mut seen = Vec::new();
        while let Ok(message) = recv.try_recv() {
            match message {
                PipeMessage::Line(err, text) => { assert!(!err); seen.push(text); }
                PipeMessage::Error(error) => panic!("合法 UTF-8 输入不应报错：{error}"),
            }
        }
        prop_assert_eq!(seen, lines, "输入：{:?}", data);
    }

    // 健壮性下界：任意字节序列（含非法 UTF-8、NUL、超长无换行）都不得让 pump panic。
    #[test]
    fn pump_never_panics_on_arbitrary_bytes(data in collection::vec(any::<u8>(), 0..4096)) {
        let (send, recv) = mpsc::sync_channel(1024);
        let handle = pump(std::io::Cursor::new(data.clone()), false, send);
        handle.join().unwrap(); // 线程内 panic 会在此暴露
        while recv.try_recv().is_ok() {}
    }
}

/// is_metadata_line：空行/普通错误行不是元数据；-slt 的 Path/Type/… 字段是元数据。
#[test]
fn is_metadata_line_classifies_common_lines() {
    assert!(!is_metadata_line(""));
    assert!(!is_metadata_line("   "));
    assert!(!is_metadata_line("ERROR: Cannot open the file as archive"));
    assert!(!is_metadata_line(
        "7-Zip [64] 23.01: Copyright (c) 1999-2023 Igor Pavlov"
    ));
    assert!(!is_metadata_line("foo = bar"), "非白名单键不得当成元数据");
    assert!(is_metadata_line("Path = archive.7z"));
    assert!(is_metadata_line("Type = 7z"));
    assert!(is_metadata_line("Physical Size = 12345"));
    assert!(is_metadata_line("Method = LZMA2:19"));
    assert!(
        is_metadata_line(" Path = leading-space-key"),
        "键两侧空白应被 trim"
    );
    // 超长行：只做键匹配，不得 panic，也不得把普通长行误判为元数据
    let long_meta = format!("Path = {}", "a".repeat(100_000));
    assert!(is_metadata_line(&long_meta));
    let long_plain = format!("{} = value", "x".repeat(10_000));
    assert!(!is_metadata_line(&long_plain));
}

/// summarize_failure：优先 stderr、过滤元数据、单行/总长截断、空输入返回空串。
#[test]
fn summarize_failure_prefers_stderr_and_truncates() {
    let mut stderr = VecDeque::new();
    let mut stdout = VecDeque::new();
    stdout.push_back("stdout only error".into());
    stderr.push_back("Path = x".into()); // 元数据必须被过滤
    stderr.push_back("ERROR: Cannot open file".into());
    let summary = summarize_failure(&stderr, &stdout);
    assert!(summary.contains("ERROR: Cannot open file"));
    assert!(
        !summary.contains("stdout only error"),
        "stderr 有可用行时不回退 stdout"
    );
    assert!(!summary.contains("Path ="), "元数据行不应进入用户可见摘要");

    // stderr 全是元数据时才回退 stdout
    let mut stderr = VecDeque::new();
    stderr.push_back("Type = 7z".into());
    let mut stdout = VecDeque::new();
    stdout.push_back("Open ERROR: invalid archive".into());
    let summary = summarize_failure(&stderr, &stdout);
    assert!(summary.contains("Open ERROR: invalid archive"));

    // 空输入
    assert_eq!(summarize_failure(&VecDeque::new(), &VecDeque::new()), "");

    // 总长截断：多行拼接超过 400 字符时截断并加省略号（单行已在 pick 阶段截到 240）
    let mut stderr = VecDeque::new();
    stderr.push_back("A".repeat(240));
    stderr.push_back("B".repeat(240));
    let summary = summarize_failure(&stderr, &VecDeque::new());
    assert_eq!(summary.chars().count(), 401, "400 字符 + 省略号");
    assert!(summary.ends_with('…'));

    // 单行截断到 240 字符
    let mut stderr = VecDeque::new();
    stderr.push_back("W".repeat(300));
    let summary = summarize_failure(&stderr, &VecDeque::new());
    assert_eq!(summary.chars().count(), 240);
}

/// pump：CRLF/LF 混排、无结尾换行、非 UTF-8 行的错误上报。
#[test]
fn pump_handles_crlf_lf_and_trailing_line() {
    let (send, recv) = mpsc::sync_channel(64);
    let data = b"line1\r\nline2\nline3".to_vec();
    let handle = pump(std::io::Cursor::new(data), false, send);
    handle.join().unwrap();
    let mut lines = Vec::new();
    while let Ok(message) = recv.try_recv() {
        match message {
            PipeMessage::Line(err, text) => {
                assert!(!err);
                lines.push(text);
            }
            PipeMessage::Error(error) => panic!("不应出现错误：{error}"),
        }
    }
    assert_eq!(lines, vec!["line1", "line2", "line3"]);
}

// 覆盖 X-08（无法解码的文件名必须报错，不得静默落盘）
#[test]
fn pump_reports_non_utf8_as_error() {
    let (send, recv) = mpsc::sync_channel(64);
    let data = vec![0xff, 0xfe, b'\n'];
    let handle = pump(std::io::Cursor::new(data), true, send);
    handle.join().unwrap();
    let mut saw_error = false;
    while let Ok(message) = recv.try_recv() {
        if let PipeMessage::Error(error) = message {
            assert!(error.contains("UTF-8"), "{error}");
            saw_error = true;
        }
    }
    assert!(saw_error, "非 UTF-8 行必须上报错误，不得静默丢弃");
}

/// read_all_capped：未超限完整读取；超限截断并标记；读错误传播。
#[test]
fn read_all_capped_truncates_and_reports_errors() {
    // 未超限
    let capture = read_all_capped(std::io::Cursor::new(b"hello".to_vec()), 64);
    assert_eq!(capture.data, b"hello");
    assert!(!capture.truncated);
    assert!(capture.error.is_none());

    // 超限：只保留前 limit 字节并标记 truncated，继续 drain 不报错
    let capture = read_all_capped(std::io::Cursor::new(vec![b'x'; 100]), 10);
    assert_eq!(capture.data.len(), 10);
    assert!(capture.truncated);
    assert!(capture.error.is_none());

    // 恰好等于 limit：不算截断
    let capture = read_all_capped(std::io::Cursor::new(vec![b'y'; 10]), 10);
    assert_eq!(capture.data.len(), 10);
    assert!(!capture.truncated);

    // 读错误必须记录，不得吞掉
    struct FailingReader;
    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("模拟读错误"))
        }
    }
    let capture = read_all_capped(FailingReader, 64);
    assert!(capture
        .error
        .as_deref()
        .is_some_and(|e| e.contains("模拟读错误")));
    assert!(capture.data.is_empty());
}

/// 空闲超时测试用的静默单进程：避免 cmd 外壳把管道句柄传给孙进程导致 join 挂死。
#[cfg(windows)]
pub(super) fn silent_hung_command() -> Command {
    let mut c = Command::new("powershell");
    c.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"]);
    c
}
#[cfg(not(windows))]
pub(super) fn silent_hung_command() -> Command {
    let mut c = Command::new("sleep");
    c.arg("60");
    c
}
// 覆盖 X-08 / H-02：流式入口取消、失败和根退出必须回收本次派生的后代。
// Windows Job 与进程句柄是验证对象，非 Windows 无对应产品行为。
#[cfg(windows)]
#[test]
fn streaming_terminal_paths_reap_owned_descendants() {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::WAIT_OBJECT_0,
        System::Threading::{
            OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
            PROCESS_TERMINATE,
        },
    };

    for mode in ["取消", "回调失败", "空闲超时", "根退出"] {
        let ending = if mode == "根退出" {
            "Start-Sleep -Seconds 1; exit 0"
        } else {
            "Start-Sleep -Seconds 60"
        };
        let script = format!(
            "$child = Start-Process -FilePath \"$PSHOME\\powershell.exe\" \
             -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 60' \
             -NoNewWindow -PassThru; [Console]::Out.WriteLine($child.Id); {ending}"
        );
        let mut command = Command::new("powershell");
        command.args(["-NoProfile", "-Command", &script]);
        let control = Control::default();
        let mut descendant = None;
        let result = run_with_idle_timeout(
            &mut command,
            &control,
            Duration::from_secs(5),
            |err, line| {
                if !err && descendant.is_none() {
                    let pid = line.trim().parse::<u32>()?;
                    // SAFETY: PID 来自本次合成进程握手，仅打开同步和失败收尾所需权限。
                    let handle =
                        unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
                    if handle.is_null() {
                        return Err(std::io::Error::last_os_error().into());
                    }
                    // SAFETY: 成功句柄的唯一所有权交给 OwnedHandle，防 PID 复用。
                    descendant = Some(unsafe { OwnedHandle::from_raw_handle(handle.cast()) });
                    match mode {
                        "取消" => control.cancel(),
                        "回调失败" => bail!("合成回调失败"),
                        _ => {}
                    }
                }
                Ok(())
            },
            || Ok(()),
        );
        let handle = descendant.expect("后代必须真实启动并完成 PID 握手");
        // SAFETY: 有界等待本用例持有的后代句柄。
        let status = unsafe { WaitForSingleObject(handle.as_raw_handle().cast(), 1000) };
        if status != WAIT_OBJECT_0 {
            // SAFETY: 只清理本用例已握手的后代，修复前失败也不留下孤儿。
            unsafe {
                TerminateProcess(handle.as_raw_handle().cast(), 1);
                WaitForSingleObject(handle.as_raw_handle().cast(), 2000);
            }
        }
        if mode == "根退出" {
            result.expect("根成功退出时不能因后代持管道误报空闲超时");
        } else {
            let error = result.expect_err("取消或失败必须返回错误");
            assert!(
                error.to_string().contains(if mode == "空闲超时" {
                    "无输出"
                } else {
                    mode
                }),
                "{mode}: {error}"
            );
        }
        assert_eq!(status, WAIT_OBJECT_0, "{mode}返回后本次后代仍存活");
    }
}

/// run_with_idle_timeout：连续无输出超过 idle 上限时应 kill 并返回错误，不得无限阻塞。
#[test]
fn run_idle_timeout_kills_silent_child() {
    let mut command = silent_hung_command();
    let control = Control::default();
    let start = Instant::now();
    let result = run_with_idle_timeout(
        &mut command,
        &control,
        Duration::from_millis(300),
        |_, _| Ok(()),
        || Ok(()),
    );
    let elapsed = start.elapsed();
    assert!(result.is_err(), "空闲超时应返回错误");
    let message = format!("{:#}", result.unwrap_err());
    assert!(
        message.contains("无输出") || message.contains("挂死"),
        "错误应说明空闲超时：{message}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "应在空闲超时后尽快返回，实际 {elapsed:?}"
    );
}

/// run_with_idle_timeout：取消路径仍可用——先启动再 cancel，应返回取消错误。
// 覆盖 C-10（用户取消在安全边界生效）
#[test]
fn run_idle_timeout_supports_cancel() {
    let mut command = silent_hung_command();
    let control = Arc::new(Control::default());
    let ctl = control.clone();
    let cancel_after = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        ctl.cancel();
    });
    let result = run_with_idle_timeout(
        &mut command,
        control.as_ref(),
        Duration::from_secs(60),
        |_, _| Ok(()),
        || Ok(()),
    );
    let _ = cancel_after.join();
    assert!(result.is_err(), "取消后应返回错误");
    let message = format!("{:#}", result.unwrap_err());
    assert!(message.contains("取消"), "应是取消错误：{message}");
}

// 覆盖 F21：总超时到达时 kill+wait 并限时收尾读线程，不得永久阻塞。
#[test]
fn run_with_timeout_kills_hung_child() {
    let mut command = silent_hung_command();
    let started = Instant::now();
    let result = run_with_timeout(&mut command, Duration::from_millis(300));
    assert!(result.is_err(), "挂死子进程应按总超时失败");
    let message = format!("{:#}", result.unwrap_err());
    assert!(message.contains("超时"), "错误应说明超时：{message}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "超时后应尽快返回，实际 {:?}",
        started.elapsed()
    );
}

// 覆盖 F21：外部取消标志在等待期间生效，先于总超时返回。
#[test]
fn run_with_timeout_cancel_stops_promptly() {
    use std::sync::atomic::{AtomicBool, Ordering};
    // Atomic 入口也必须在 spawn 前检查；不存在的程序不能掩盖预取消错误。
    let pre_cancelled = AtomicBool::new(true);
    let error = run_with_timeout_cancel(
        &mut Command::new("jchtools-must-not-spawn-pre-cancelled.exe"),
        Duration::from_secs(60),
        &pre_cancelled,
    )
    .unwrap_err();
    assert!(error.to_string().contains("取消"), "{error}");

    let mut command = silent_hung_command();
    let cancel = Arc::new(AtomicBool::new(false));
    let setter = cancel.clone();
    let cancel_after = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        let cancelled_at = Instant::now();
        setter.store(true, Ordering::Relaxed);
        cancelled_at
    });
    let started = Instant::now();
    let result = run_with_timeout_cancel(&mut command, Duration::from_secs(60), &cancel);
    let finished = Instant::now();
    let cancelled_at = cancel_after.join().unwrap();
    assert!(result.is_err(), "取消后应返回错误");
    let message = format!("{:#}", result.unwrap_err());
    assert!(message.contains("取消"), "应是取消错误：{message}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "取消后应尽快返回，总耗时 {:?}，取消至返回 {:?}",
        started.elapsed(),
        finished.saturating_duration_since(cancelled_at)
    );

    // 再从真实根/后代 PID 握手之后取消，单独测取消到整树释放；不替代上面的原 5s 断言。
    // 修复前 Atomic 入口只杀根进程，-NoNewWindow 后代继续持有 stdout/stderr 写端。
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::{
            Foundation::WAIT_OBJECT_0,
            System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
        };

        let tmp_root = process_tree_tmp_root();
        std::fs::create_dir_all(&tmp_root).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("atomic-process-tree-")
            .tempdir_in(&tmp_root)
            .unwrap();
        let pid_path = dir.path().join("ready.pid");
        let path = pid_path.to_string_lossy().replace('\'', "''");
        let script = format!(
            "$child = Start-Process -FilePath \"$PSHOME\\powershell.exe\" \
             -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 60' \
             -NoNewWindow -PassThru; \
             [IO.File]::WriteAllText('{path}', \"$PID $($child.Id)\"); \
             Start-Sleep -Seconds 60"
        );
        let mut command = Command::new("powershell");
        command.args(["-NoProfile", "-Command", &script]);
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = cancel.clone();
        let cancel_thread = thread::spawn(move || {
            let ready = (|| -> std::result::Result<[OwnedHandle; 2], String> {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Ok(text) = std::fs::read_to_string(&pid_path) {
                        let mut pids = text.split_whitespace();
                        if let (Some(root), Some(descendant), None) =
                            (pids.next(), pids.next(), pids.next())
                        {
                            if let (Ok(root), Ok(descendant)) =
                                (root.parse::<u32>(), descendant.parse::<u32>())
                            {
                                let open = |pid| {
                                    // SAFETY: 只打开本用例已握手进程的同步句柄，不继承或改状态。
                                    let handle =
                                        unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
                                    if handle.is_null() {
                                        return Err(format!(
                                            "取消前无法打开合成进程 {pid}：{}",
                                            std::io::Error::last_os_error()
                                        ));
                                    }
                                    // SAFETY: 成功句柄的唯一所有权交给 OwnedHandle，防 PID 复用。
                                    Ok(unsafe { OwnedHandle::from_raw_handle(handle.cast()) })
                                };
                                return Ok([open(root)?, open(descendant)?]);
                            }
                        }
                    }
                    if Instant::now() >= deadline {
                        return Err("合成根进程及后代未完成真实 PID 握手".into());
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            })();
            // 即使握手失败也先发取消并由宿主回收，避免失败断言留下挂死进程。
            let cancelled_at = Instant::now();
            setter.store(true, Ordering::Relaxed);
            (cancelled_at, ready)
        });
        let result = run_with_timeout_cancel(&mut command, Duration::from_secs(60), &cancel);
        let (cancelled_at, ready) = cancel_thread.join().unwrap();
        let error = result.expect_err("取消真实进程树必须返回错误");
        assert!(error.to_string().contains("取消"), "{error}");
        for handle in ready.expect("取消前必须确认根进程和后代已经真实启动") {
            // SAFETY: 仅有界等待取消前持有的合成进程同步句柄。
            let status = unsafe { WaitForSingleObject(handle.as_raw_handle().cast(), 2000) };
            assert_eq!(status, WAIT_OBJECT_0, "取消后根进程或后代仍存活");
        }
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(5),
            "取消到整树释放应保留 5s 上限，实际 {:?}",
            cancelled_at.elapsed()
        );
    }
}

// 覆盖 F21：超过捕获上限的输出标记 truncated，不得静默当完整结果。
#[cfg(windows)]
#[test]
fn run_with_timeout_marks_truncated_huge_output() {
    let mut command = Command::new("powershell");
    command.args([
        "-NoProfile",
        "-Command",
        "[Console]::Out.Write(('x' * 9437184))",
    ]);
    let output =
        run_with_timeout(&mut command, Duration::from_secs(60)).expect("大输出进程正常退出应成功");
    assert!(output.stdout_truncated, "stdout 超限必须标记截断");
    assert!(output.truncation_note().is_some());

    // C-08：只读判型入口必须支持预取消、双流独立上限及成功查询。
    let cancelled = Control::default();
    cancelled.cancel();
    let error = run_with_timeout_control_limit(
        &mut Command::new("jchtools-must-not-spawn-pre-cancelled.exe"),
        Duration::from_secs(10),
        &cancelled,
        64 * 1024,
    )
    .unwrap_err();
    assert!(error.to_string().contains("取消"), "{error}");
    for stream in ["Out", "Error"] {
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-Command",
            &format!("[Console]::{stream}.Write(('x' * 65537)); Start-Sleep -Seconds 30"),
        ]);
        let error = run_with_timeout_control_limit(
            &mut command,
            Duration::from_secs(10),
            &Control::default(),
            64 * 1024,
        )
        .unwrap_err();
        assert!(error.to_string().contains("捕获上限"), "{stream}: {error}");
    }
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-Command", "[Console]::Out.Write('ok')"]);
    let output = run_with_timeout_control_limit(
        &mut command,
        Duration::from_secs(10),
        &Control::default(),
        64 * 1024,
    )
    .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"ok");
    assert!(!output.stdout_truncated && !output.stderr_truncated);

    // C-08：真实后代不能在取消、超时或超限返回后仍存活。
    // 仅在开发临时区内生成公开合成 PID 握手，不读取用户数据或清理其他临时目录。
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::WAIT_OBJECT_0,
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    let tmp_root = process_tree_tmp_root();
    std::fs::create_dir_all(&tmp_root).unwrap();
    for mode in ["取消", "超时", "捕获上限"] {
        let dir = tempfile::Builder::new()
            .prefix("process-tree-")
            .tempdir_in(&tmp_root)
            .unwrap();
        let pid_path = dir.path().join("child.pid");
        let path = pid_path.to_string_lossy().replace('\'', "''");
        let ending = if mode == "捕获上限" {
            "[Console]::Out.Write(('x' * 65537)); Start-Sleep -Seconds 30"
        } else {
            "Start-Sleep -Seconds 30"
        };
        let script = format!(
            "$child = Start-Process -FilePath \"$PSHOME\\powershell.exe\" \
             -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 30' \
             -NoNewWindow -PassThru; \
             [IO.File]::WriteAllText('{path}', [string]$child.Id); {ending}"
        );
        let control = Arc::new(Control::default());
        let cancel_thread = if mode == "取消" {
            let control = control.clone();
            let path = pid_path.clone();
            Some(thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    if std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|pid| pid.trim().parse::<u32>().ok())
                        .is_some()
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                control.cancel();
            }))
        } else {
            None
        };
        let mut command = Command::new("powershell");
        command.args(["-NoProfile", "-Command", &script]);
        let error = run_with_timeout_control_limit(
            &mut command,
            Duration::from_secs(if mode == "取消" { 10 } else { 5 }),
            &control,
            64 * 1024,
        )
        .unwrap_err();
        if let Some(handle) = cancel_thread {
            handle.join().unwrap();
        }
        assert!(error.to_string().contains(mode), "{mode}: {error}");
        let pid: u32 = std::fs::read_to_string(&pid_path)
            .expect("后代必须真实启动并写入握手")
            .trim()
            .parse()
            .unwrap();
        // SAFETY: 仅打开本用例合成后代的同步句柄，不继承、不修改其状态。
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER.cast_signed()),
                "只能将后代 PID 已消失视为退出，不能把权限失败当成成功"
            );
        } else {
            // SAFETY: 有效同步句柄唯一所有权交给 OwnedHandle。
            let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };
            // SAFETY: 仅有界等待本用例后代的有效同步句柄，不修改进程状态。
            let status = unsafe { WaitForSingleObject(handle.as_raw_handle().cast(), 2000) };
            assert_eq!(status, WAIT_OBJECT_0, "{mode}后后代仍存活");
        }
    }
}
