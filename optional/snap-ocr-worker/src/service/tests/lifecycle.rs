//! //! 后台生命周期回归：登录自启解除抑制、门控取消终态、空闲保活恢复与停止诊断。
//! 从 `service.rs` 的内联测试模块原样搬移（仅调整 use 导入与自引用测试名）。

use super::*;
use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

// F37 / O-15 / XB-22 / XB-23：同一 SessionId 的下一次登录自启应解除
// 上次主动退出的持久抑制；普通保活启动不得擅自解除。只到桌面前检查点。
#[test]
#[cfg(feature = "test-hooks")]
fn review_f37_autostart_resumes_previous_session_stop() -> Result<(), Box<dyn std::error::Error>> {
    const CHILD: &str = "JCHTOOLS_REVIEW_F37_AUTOSTART_CHILD";
    if std::env::var_os(CHILD).is_none() {
        for mode in ["ordinary", "autostart"] {
            let root = tempfile::tempdir()?;
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "service::tests::lifecycle::review_f37_autostart_resumes_previous_session_stop",
                    "--nocapture",
                ])
                .env(CHILD, mode)
                .env("JCHTOOLS_TEST_STATE_DIR", root.path().join("state"))
                .env("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", root.path().join("ocr"))
                .env_remove("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT")
                .status()?;
            assert!(status.success(), "后台启动许可回归失败：{mode}");
        }
        return Ok(());
    }
    crate::xberg_runtime::background_control(true).map_err(std::io::Error::other)?;
    let state = crate::xberg_settings::state_dir().map_err(std::io::Error::other)?;
    let marker = fs::read_dir(&state)?
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("background-stopped-")
        })
        .ok_or("真实退出控制入口未创建停止标记")?
        .path();
    let autostart = std::env::var(CHILD)? == "autostart";
    REVIEW_F37_STARTUP_CHECKPOINT.with(|checkpoint| checkpoint.set(true));
    let started = super::run_service_inner(autostart);
    REVIEW_F37_STARTUP_CHECKPOINT.with(|checkpoint| checkpoint.set(false));
    assert_eq!(
        started.as_ref().map_err(String::as_str),
        Err("review_f37 startup checkpoint")
    );
    assert_eq!(
        marker.exists(),
        !autostart,
        "登录自启必须解除上次退出标记；当前 GUI 会话保活必须保留明确退出"
    );
    Ok(())
}

// F38：合成进程仅提供可门控的协议边界，不进行模型推理。
// 与共享代理回归相同：cancel ACK 不释放 snapshot，只有 release 文件才释放。
#[cfg(feature = "test-hooks")]
fn review_f38_boundary_engine(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let source = root.join("review_f38_engine.rs");
    fs::write(
        &source,
        r#"
use std::{io::{self, BufRead, Write}, path::Path, sync::{Arc, Mutex}, time::Duration};
fn field(line: &str, name: &str) -> String {
    line.split(&format!("\"{name}\":\"")).nth(1)
        .unwrap_or("").split('"').next().unwrap_or("").to_owned()
}
fn main() {
    let output = Arc::new(Mutex::new(io::stdout()));
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        let output = output.clone();
        std::thread::spawn(move || {
            let command = field(&line, "command");
            let id = field(&line, "id");
            let gated = command == "ocr_snapshot" && field(&line, "image_base64") == "wait";
            if gated {
                std::fs::write("snapshot-started", b"1").unwrap();
                while !Path::new("release-snapshot").exists() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let payload = match command.as_str() {
                "capabilities" => "\"commands\":[\"extract\",\"ocr_snapshot\",\"transcribe\",\"cancel\"],\"protocol_version\":2,\"cancellation\":\"cooperative\",\"timeout_ms\":true,\"document_snapshot_concurrent\":true,\"paddle_ocr\":true,\"transcription\":true",
                "ocr_snapshot" => "\"text\":\"snapshot\"",
                "cancel" => "\"accepted\":true",
                _ => "\"state\":\"ready\"",
            };
            let mut out = output.lock().unwrap();
            writeln!(out, "{{\"id\":\"{id}\",\"ok\":true,{payload}}}").unwrap();
            out.flush().unwrap();
            if gated { std::fs::write("snapshot-terminal", b"1").unwrap(); }
        });
    }
}
"#,
    )?;
    let compiled = std::process::Command::new("rustc")
        .arg("--edition=2021")
        .args(["-C", "target-feature=+crt-static"])
        .arg(source)
        .arg("-o")
        .arg(root.join("xberg.exe"))
        .output()?;
    assert!(
        compiled.status.success(),
        "协议边界夹具编译失败：{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    Ok(())
}

// F38 / O-19 / XB-08 / XB-17：真实代理、shared_xberg 分类及服务状态机
// 联合回归；cancel ACK 后原请求仍被门控时，不能把截图任务置为空闲。
#[test]
#[cfg(feature = "test-hooks")]
fn review_f38_cancel_keeps_busy_until_original_terminal() -> Result<(), Box<dyn std::error::Error>>
{
    use std::io::Read as _;
    use std::os::windows::process::CommandExt as _;
    const CHILD: &str = "JCHTOOLS_REVIEW_F38_TERMINAL_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let _session = super::engine_session::session_lock(&repository_root);
        let root = tempfile::tempdir()?;
        let status = review_remediation_engine_child(
            std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "service::tests::lifecycle::review_f38_cancel_keeps_busy_until_original_terminal",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("JCHTOOLS_TEST_STATE_DIR", root.path().join("state"))
                .env("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", root.path().join("ocr"))
                .env_remove("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT"),
        )?;
        assert!(status.success(), "原请求未终态时截图服务不得报告 idle");
        return Ok(());
    }
    let mut gate = [0];
    std::io::stdin().read_exact(&mut gate)?;
    assert_eq!(gate, *b"1", "必须先绑定本测试自有 Job 才能派生推理进程");
    let executable = std::env::current_exe()?;
    let broker = executable
        .parent()
        .and_then(Path::parent)
        .ok_or("测试可执行文件缺少 target 目录")?
        .join("snap-ocr-worker.exe");
    let capabilities = std::process::Command::new(&broker)
        .arg("--capabilities")
        .creation_flags(0x0800_0000)
        .output()?;
    assert!(capabilities.status.success(), "测试 broker 能力握手失败");
    let capabilities: serde_json::Value = serde_json::from_slice(&capabilities.stdout)?;
    assert_eq!(
        capabilities["test_hooks"], true,
        "必须先构建带 test-hooks 的 worker broker"
    );
    std::env::set_var("JCHTOOLS_TEST_BROKER_EXE", broker);
    let state = crate::xberg_settings::state_dir().map_err(std::io::Error::other)?;
    let engine = state
        .parent()
        .ok_or("隔离状态目录缺少父目录")?
        .join("engine");
    fs::create_dir(&engine)?;
    review_f38_boundary_engine(&engine)?;
    crate::xberg_settings::save(&engine).map_err(std::io::Error::other)?;
    let mut observer = crate::shared_xberg::SharedXbergClient::connect(&engine);
    assert_eq!(
        observer.snapshot_state()?,
        crate::shared_xberg::SnapshotState::Ready
    );
    struct ReleaseSnapshot(std::path::PathBuf);
    impl Drop for ReleaseSnapshot {
        fn drop(&mut self) {
            let _ = fs::write(self.0.join("release-snapshot"), b"1");
        }
    }
    let release = ReleaseSnapshot(engine.clone());
    let (mut service, _work_rx) = ready_busy_service(&engine);
    let (events, event_rx) = mpsc::channel();
    let component = engine.clone();
    let cancel = service.cancel.clone();
    let request = std::thread::spawn(move || {
        let mut client = crate::shared_xberg::SharedXbergClient::connect(&component);
        // 此三字节的 base64 是夹具的 wait 控制值；不冒充 PNG 或真实模型验收。
        let outcome = client
            .recognize(&[0xc1, 0xa8, 0xad], &cancel)
            .map_err(super::ocr_error);
        let _ = events.send(Command::OcrFinished(outcome, (0, 0, 8, 8)));
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !engine.join("snapshot-started").exists() {
        if Instant::now() >= deadline {
            return Err("截图原请求未进入隔离门控".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    service.cancel.store(true, Ordering::Release);
    let early = match event_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(command) => {
            service.handle(command);
            true
        }
        Err(mpsc::RecvTimeoutError::Timeout) => false,
        Err(error) => return Err(error.into()),
    };
    let before_terminal = service.pipe(&serde_json::json!({"command":"get-state"}));
    let model_before_terminal = observer.snapshot_state()?;
    let original_unfinished = !engine.join("snapshot-terminal").exists();
    drop(release);
    if !early {
        service.handle(event_rx.recv_timeout(Duration::from_secs(10))?);
    }
    request.join().map_err(|_| "截图请求线程异常")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !engine.join("snapshot-terminal").exists() {
        if Instant::now() >= deadline {
            return Err("释放后未收到原请求终态".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // 同一引擎 stdout 顺序保证这次状态响应之前原终态已由 broker 读取。
    observer.snapshot_state()?;
    crate::xberg_runtime::background_control(true).map_err(std::io::Error::other)?;
    assert!(original_unfinished, "检查点必须发生在原请求终态之前");
    assert_eq!(
        model_before_terminal,
        crate::shared_xberg::SnapshotState::Ready,
        "模型就绪与请求是否终态是两个独立事实"
    );
    assert_ne!(
        before_terminal["task"], "idle",
        "取消已确认但原请求仍在推理时必须保持 busy，不能接受下一次截图"
    );
    assert_eq!(service.status()["task"], "idle", "真实终态后才可恢复空闲");
    Ok(())
}

// 隔离测试子进程先等 stdin 放行，父进程完成 Job 绑定后才允许创建 broker。
// 回收屏障等待整个自有进程树归零，不能把 broker-stop 的 ACK 当作引擎已退出。
#[cfg(feature = "test-hooks")]
fn review_remediation_engine_child(
    command: &mut std::process::Command,
) -> Result<std::process::ExitStatus, Box<dyn std::error::Error>> {
    use std::io::Write as _;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    struct ScopedChild {
        child: Option<std::process::Child>,
        job: OwnedHandle,
        assigned: bool,
    }
    impl ScopedChild {
        fn finish(&mut self) -> std::io::Result<()> {
            if self.assigned {
                // SAFETY: 仅终结本测试创建并在放行前绑定的子进程及其后代。
                if unsafe { TerminateJobObject(self.job.as_raw_handle().cast(), 1) } == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                loop {
                    // Job 完成端口通知不保证送达；只以内核账户中的进程数为屏障。
                    // SAFETY: Win32 POD 允许清零；输出结构和可选长度指针有效。
                    let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
                        unsafe { std::mem::zeroed() };
                    // SAFETY: Job 由本守卫独占，输出缓冲区大小与结构一致。
                    if unsafe {
                        QueryInformationJobObject(
                            self.job.as_raw_handle().cast(),
                            JobObjectBasicAccountingInformation,
                            (&raw mut accounting).cast(),
                            std::mem::size_of_val(&accounting) as u32,
                            std::ptr::null_mut(),
                        )
                    } == 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    if accounting.ActiveProcesses == 0 {
                        self.assigned = false;
                        break;
                    }
                    // 没有新的业务重试或任意时限；仅让出调度等待已终结进程退场。
                    std::thread::yield_now();
                }
            }
            if let Some(child) = self.child.as_mut() {
                if child.try_wait()?.is_none() {
                    // 仅覆盖 Job 绑定失败、尚在等 stdin 放行的自有子进程。
                    child.kill()?;
                }
                child.wait()?;
            }
            drop(self.child.take());
            Ok(())
        }
    }
    impl Drop for ScopedChild {
        fn drop(&mut self) {
            if let Err(error) = self.finish() {
                // 不吞掉回收失败；无路径、环境或协议正文。
                eprintln!(
                    "隔离引擎进程树回收失败：kind={:?} code={:?}",
                    error.kind(),
                    error.raw_os_error()
                );
            }
        }
    }
    // SAFETY: 创建无继承的匿名 Job；返回有效句柄后立即交给 OwnedHandle。
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: job 来自上面的成功创建，所有权仅转移一次。
    let job = unsafe { OwnedHandle::from_raw_handle(job.cast()) };
    let mut scoped = ScopedChild {
        child: None,
        job,
        assigned: false,
    };
    // SAFETY: Win32 POD 配置允许全零初始化。
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: Job 及同步读取的配置结构均由本函数持有。
    if unsafe {
        SetInformationJobObject(
            scoped.job.as_raw_handle().cast(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    scoped.child = Some(command.stdin(std::process::Stdio::piped()).spawn()?);
    let child = scoped.child.as_mut().ok_or("隔离子进程未创建")?;
    // SAFETY: 子进程等待 stdin，不会在绑定之前派生任何 broker/引擎。
    if unsafe {
        AssignProcessToJobObject(
            scoped.job.as_raw_handle().cast(),
            child.as_raw_handle().cast(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    scoped.assigned = true;
    child
        .stdin
        .take()
        .ok_or("隔离子进程缺少放行管道")?
        .write_all(b"1")?;
    let status = child.wait()?;
    scoped.finish()?;
    Ok(status)
}

// P2-06 / O-13 / XB-22：真实 recv_timeout 空闲分支，不手工映射 LoadFailure。
// 引擎复用现有协议夹具；模型质量不在这项生命周期回归的范围内。
#[test]
#[cfg(feature = "test-hooks")]
fn review_remediation_idle_unconfigured_recovers_after_settings_save(
) -> Result<(), Box<dyn std::error::Error>> {
    const CHILD: &str = "JCHTOOLS_REVIEW_OCR_IDLE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // 仅父进程取仓库级锁；status 等待子进程停止测试引擎并退出后才释放，
        // 子分支不重入同一文件锁，避免父子互等。
        let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let _session = super::engine_session::session_lock(&repository_root);
        let temp = tempfile::tempdir()?;
        let status = review_remediation_engine_child(
            std::process::Command::new(std::env::current_exe()?)
                .args(["--exact", "service::tests::lifecycle::review_remediation_idle_unconfigured_recovers_after_settings_save", "--nocapture"])
                .env(CHILD, "1")
                .env("JCHTOOLS_TEST_STATE_DIR", temp.path().join("state"))
                .env("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", temp.path().join("ocr"))
                .env_remove("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT"),
        )?;
        assert!(status.success(), "隔离 worker 空闲保活回归失败");
        return Ok(());
    }
    // 父进程绑定自有 Job 后才放行；EOF/错误均不得启动推理子进程。
    use std::io::Read as _;
    use std::os::windows::process::CommandExt as _;
    let mut gate = [0];
    std::io::stdin().read_exact(&mut gate)?;
    assert_eq!(gate, *b"1", "隔离进程未收到父进程放行");
    // 全局事件代理只在这个独立子进程注册；其余用例保留线程局部无队列后端。
    review_remediation_testing_backend(true)?;
    let executable = std::env::current_exe()?;
    let broker = executable
        .parent()
        .and_then(Path::parent)
        .ok_or("测试可执行文件缺少 target 目录")?
        .join("snap-ocr-worker.exe");
    assert!(
        broker.is_file(),
        "先构建带 test-hooks 的 snap-ocr-worker.exe：{}",
        broker.display()
    );
    // 库测试不会重建这个独立 EXE；is_file 不代表它支持隔离状态目录。
    // 先做无副作用握手，避免旧普通构建读取真实用户配置。
    let capabilities = std::process::Command::new(&broker)
        .arg("--capabilities")
        .creation_flags(0x0800_0000)
        .output()?;
    assert!(
        capabilities.status.success(),
        "测试 broker 能力入口执行失败"
    );
    let capabilities: serde_json::Value = serde_json::from_slice(&capabilities.stdout)?;
    assert_eq!(
        capabilities["test_hooks"], true,
        "测试 broker 必须以 test-hooks 构建"
    );
    std::env::set_var("JCHTOOLS_TEST_BROKER_EXE", &broker);
    // 资产根由父进程的 TempDir 持有：必须等自有 Job 的 broker/engine 全部
    // 退出后才删除，子进程不能在 stop ACK 后提前销毁运行目录。
    let state = crate::xberg_settings::state_dir().map_err(std::io::Error::other)?;
    let temp = state
        .parent()
        .ok_or("隔离状态目录缺少父目录")?
        .join("idle-worker");
    fs::create_dir(&temp)?;
    let engine = temp.join("engine");
    fs::create_dir(&engine)?;
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/shared_xberg.rs");
    let compiled = std::process::Command::new("rustc")
        // 成员夹具包含合成 DLL；动态 CRT 会先加载同目录假 VCRUNTIME140.dll 而崩溃，
        // 尚未处理能力请求就 EOF。与现有共享引擎夹具一致，静态链接测试 CRT。
        .arg("--edition=2021")
        .args(["-C", "target-feature=+crt-static"])
        .arg(&source)
        .arg("-o")
        .arg(engine.join("xberg.exe"))
        .output()?;
    assert!(
        compiled.status.success(),
        "既有协议引擎夹具编译失败：{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let manifest: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../resources/markdown-assets.json"
    ))?;
    for member in manifest["xberg"]["members"]
        .as_array()
        .ok_or("资产清单缺少成员")?
    {
        let relative = member["path"].as_str().ok_or("资产清单路径无效")?;
        if relative != "xberg.exe" {
            let target = engine.join(relative);
            fs::create_dir_all(target.parent().ok_or("资产父目录缺失")?)?;
            fs::write(target, b"synthetic asset")?;
        }
    }
    assert!(crate::xberg_settings::load()
        .map_err(std::io::Error::other)?
        .is_none());
    let (events, event_rx) = mpsc::channel();
    let (service, work_rx) = ready_busy_service(&temp);
    struct IdleWorker {
        service: Service,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl IdleWorker {
        fn finish(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            if self.thread.is_none() {
                return Ok(());
            }
            let (closed_work, _) = mpsc::channel();
            drop(std::mem::replace(&mut self.service.work, closed_work));
            if let Some(thread) = self.thread.take() {
                thread.join().map_err(|_| "空闲推理线程异常")?;
            }
            Ok(())
        }
    }
    impl Drop for IdleWorker {
        fn drop(&mut self) {
            let _ = self.finish();
        }
    }
    let mut worker = IdleWorker {
        service,
        thread: None,
    };
    let service = &mut worker.service;
    service.phase = ActivityPhase::Idle;
    service.model = ModelState::Uninitialized;
    let root = temp.clone();
    let cancel = service.cancel.clone();
    worker.thread = Some(std::thread::spawn(move || {
        super::worker(&root, &work_rx, &events, &cancel)
    }));
    let service = &mut worker.service;
    let mut idle_states = Vec::new();
    for _ in 0..2 {
        service.handle(event_rx.recv_timeout(Duration::from_secs(10))?);
        idle_states.push(service.pipe(&serde_json::json!({"command":"get-state"})));
    }
    crate::xberg_settings::save(&engine).map_err(std::io::Error::other)?;
    // 没有 RetryModel / Work::Load：只由下一轮空闲保活发现新配置。
    let recovered = event_rx.recv_timeout(Duration::from_secs(30));
    if let Ok(command) = recovered {
        service.handle(command);
        // 字体注册使用真实 Slint spawn_local；在创建后端的测试 UI 线程
        // 消费已排队任务。Quit 排在其后，成功和模型失败均不会停在事件循环。
        slint::quit_event_loop()?;
        slint::run_event_loop_until_quit()?;
    }
    let ready = service.pipe(&serde_json::json!({"command":"get-state"}));
    worker.finish()?;
    let stopped = crate::xberg_runtime::background_control(true);
    for state in idle_states {
        assert_eq!(
            state["model"], "uninitialized",
            "两轮空闲保活都必须保留初始化入口：{state}"
        );
        assert_eq!(state["task"], "idle");
        assert!(
            state["error"]
                .as_str()
                .is_some_and(|message| message.contains("设置")),
            "缺配置必须保留设置指引：{state}"
        );
    }
    assert_eq!(
        ready["model"], "ready",
        "保存有效配置后须自动恢复，无需重试：{ready}"
    );
    assert!(ready["error"].is_null());
    stopped.map_err(std::io::Error::other)?;
    Ok(())
}

// P3-32 / P-10：真实独占锁制造停止标记写失败，检查落盘原因及跨线程关联。
#[test]
#[cfg(feature = "test-hooks")]
fn review_remediation_stop_failure_retains_safe_diagnostic_correlation(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::windows::fs::OpenOptionsExt as _;
    const CHILD: &str = "JCHTOOLS_REVIEW_OCR_STOP_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let temp = tempfile::tempdir()?;
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "service::tests::lifecycle::review_remediation_stop_failure_retains_safe_diagnostic_correlation", "--nocapture"])
            .env(CHILD, "1")
            .env("JCHTOOLS_TEST_STATE_DIR", temp.path().join("private-state-canary")
                .join("private-user-path-canary").join("OCR-body-canary"))
            .env("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", temp.path().join("ocr"))
            .env_remove("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT")
            .status()?;
        assert!(status.success(), "隔离停止诊断回归失败");
        return Ok(());
    }
    let state = crate::xberg_settings::state_dir().map_err(std::io::Error::other)?;
    crate::xberg_runtime::background_control(true).map_err(std::io::Error::other)?;
    let marker = fs::read_dir(&state)?
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("background-stopped-")
        })
        .ok_or("实际停止入口没有创建停止标记")?
        .path();
    let locked = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&marker)?;
    let expected = fs::write(&marker, b"locked")
        .err()
        .ok_or("独占锁必须阻止写入")?;
    let temp = tempfile::tempdir()?;
    let guard = crate::logging::init(temp.path()).ok_or("诊断日志初始化失败")?;
    let (events, event_rx) = mpsc::channel();
    let (mut service, work_rx) = ready_busy_service(temp.path());
    service.phase = ActivityPhase::Idle;
    service.exit_pending = Some(Instant::now() - Duration::from_secs(3));
    service.exit_stop_sent = true;
    let root = temp.path().to_path_buf();
    let cancel = service.cancel.clone();
    let worker = std::thread::spawn(move || super::worker(&root, &work_rx, &events, &cancel));
    service
        .work
        .send(Work::Stop)
        .map_err(|_| "停止指令通道已断开")?;
    let failure = event_rx.recv_timeout(Duration::from_secs(5))?;
    assert!(
        matches!(&failure, Command::WorkerStopFailed(_)),
        "锁定停止标记必须通过真实停止回执报告失败"
    );
    service.handle(failure);
    assert!(
        service.stop_retry_at.is_some(),
        "停止失败须继续保留安全重试"
    );
    let (closed_work, _) = mpsc::channel();
    drop(std::mem::replace(&mut service.work, closed_work));
    worker.join().map_err(|_| "停止测试推理线程异常")?;
    drop(locked);
    drop(guard);
    let mut logs = String::new();
    for entry in fs::read_dir(temp.path().join(crate::logging::LOG_DIR))? {
        logs.push_str(&fs::read_to_string(entry?.path())?);
    }
    let exhausted = logs
        .lines()
        .find(|line| line.contains("event=ocr_stop_exhausted"))
        .ok_or("缺少实际停止重试耗尽诊断")?;
    let correlation = exhausted
        .split_whitespace()
        .find(|field| field.starts_with("operation_id="))
        .ok_or("停止原操作缺少安全关联 ID")?;
    let scheduled = logs
        .lines()
        .find(|line| line.contains("event=ocr_stop_retry_scheduled"))
        .ok_or("缺少消费端重试诊断")?;
    assert!(
        scheduled.contains(correlation),
        "消费端必须保留停止原操作的安全关联，而非丢弃原因：{scheduled}"
    );
    // DiagnosticFields 对字符串/Debug 字段统一 JSON 引号编码；数字 Option
    // 以 Debug 字符串记录。按真实编码匹配动态 OS 原因，而非裸 key=value 假设。
    let kind = format!(
        "error_type={}",
        serde_json::to_string(&format!("{:?}", expected.kind()))?
    );
    let code = expected
        .raw_os_error()
        .ok_or("Windows 文件锁错误缺少 OS code")?;
    let code = format!(
        "error_code={}",
        serde_json::to_string(&format!("Some({code})"))?
    );
    assert!(
        logs.lines()
            .any(|line| line.contains("event=xberg_stopped_marker_failed")
                && line.contains("stage=\"stopped_marker\"")
                && line.contains("operation=\"write\"")
                && line.contains(&kind)
                && line.contains(&code)
                && line.contains(correlation)),
        "停止错误原产地必须保留安全 kind/code 及操作关联，不是仅记录固定分类"
    );
    for private in [
        "private-state-canary",
        "private-user-path-canary",
        "OCR-body-canary",
    ] {
        assert!(
            !logs.contains(private),
            "停止诊断不得泄漏路径或正文：{private}"
        );
    }
    Ok(())
}
