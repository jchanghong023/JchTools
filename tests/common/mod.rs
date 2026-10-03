//! 引擎集成测试共用工具：跨进程会话锁。
//!
//! 生产语义是每登录会话至多一个 Xberg（xberg_runtime_windows 的进程扫描执法）。
//! cargo test 会并行运行多个测试二进制，各自派生真实代理与引擎时天然互斥——
//! 用 `.tmp/` 下的锁文件把「需要会话引擎」的测试串行化，互斥由文件系统保证，
//! 不依赖时序运气。锁文件陈旧（超过 3 分钟无人续期）时视为持有者已死，直接抢占。

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// 仓库根（编译期定位 rustc 夹具源与 .tmp）。
const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// 会话锁守卫：Drop 时删除锁文件。
pub(super) struct SessionLock {
    path: PathBuf,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 取会话锁：最多等 180 秒；锁文件存在且超过 3 分钟视为陈旧，直接覆盖抢占。
///
/// # Panics
/// 等满 180 秒仍拿不到锁（前一个持有测试可能卡死）， panic 并提示清理
/// `.tmp/engine-session.lock`。
pub(super) fn session_lock() -> SessionLock {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".tmp/engine-session.lock");
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return SessionLock { path },
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path).is_ok_and(|metadata| {
                    metadata.modified().is_ok_and(|modified| {
                        modified
                            .elapsed()
                            .is_ok_and(|age| age > Duration::from_secs(180))
                    })
                });
                if stale {
                    // 陈旧锁：持有者已死（崩溃/被杀），抢占。
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
            }
            Err(_) => {}
        }
        assert!(
            Instant::now() < deadline,
            "引擎会话锁等待超时：请人工清理 .tmp/engine-session.lock 后重跑"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// 清场前的廉价探测（2026-10-03 计时优化：全量 PowerShell 扫杀每轮约 2s，而
/// 孤儿回收 Job 落地后测试自身残留趋零）：按映像名粗查三个目标进程，全部缺席
/// 即跳过全量清场。探测命中只会多走原全量路径（其内仍按命令行特征过滤，语义
/// 不变）——例如用户自己的 GUI 在跑时照旧全量清场。
fn any_engine_image_alive() -> bool {
    ["JchTools.exe", "snap-ocr-worker.exe", "xberg.exe"]
        .iter()
        .any(|image| {
            std::process::Command::new("tasklist")
                .args(["/FI", &format!("IMAGENAME eq {image}"), "/FO", "CSV", "/NH"])
                .output()
                .is_ok_and(|out| {
                    out.status.success() && String::from_utf8_lossy(&out.stdout).contains(image)
                })
        })
}

/// 清理本会话残留的引擎与代理（锁内调用）：上一轮失败的孤儿会占住
/// 会话单引擎执法，让下一轮从头就「已有 Xberg」。只按命令行特征匹配
/// 本项目派生的进程（`--xberg-broker`）与引擎映像名，不触碰其他进程。
pub(super) fn cleanup_stray_engines() {
    if !any_engine_image_alive() {
        return;
    }
    // 0) 先停截图服务本体：XB-22 常驻看护会把被杀的代理与引擎按自己的
    //    周期重新拉起，只杀代理/引擎永远赢不了（实测重跑 60s 忙碌超时）；
    //    与下方同口径，仅结束本项目派生的服务进程。GUI 打开时会按 XB-22
    //    重新拉起服务，dev 机测试窗口期内不保留。
    let stop_service = "Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'snap-ocr-worker.exe' -and $_.CommandLine -match '--service' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }";
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            stop_service,
        ])
        .output();
    std::thread::sleep(Duration::from_millis(300));
    // 1) 再杀「托管代理」：主程序代理与截图服务代理（后者按 XB-22 常驻保活，
    //    会把被杀的引擎立刻重新拉起，必须先于引擎处理）。
    let script = "Get-CimInstance Win32_Process | Where-Object { ($_.Name -eq 'JchTools.exe' -or $_.Name -eq 'snap-ocr-worker.exe') -and $_.CommandLine -match '--xberg-broker' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }";
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .output();
    std::thread::sleep(Duration::from_millis(400));
    // 2) 清引擎，两遍（防监督者在窗口期重启）。
    for _ in 0..2 {
        let _ = std::process::Command::new("taskkill")
            .args(["/IM", "xberg.exe", "/F"])
            .output();
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// 重入层标记：持有这些标记的进程是外层测试进程的 re-exec 子层，外层已持有
/// 回收 Job（子层经派生自动归属同一 Job）；子层若再自建 Job，其退出会关闭自有
/// Job 句柄、误杀必须比它活得久的常驻代理（capture-pipe 等测试钉死的语义），
/// 因此跳过自建。
fn reexec_layer() -> bool {
    std::env::var_os("JT_XBERG_PIPE_MID").is_some()
        || std::env::var_os("JCHTOOLS_SHARED_CLIENT_ROOT").is_some()
        || std::env::var_os("JT_BACKGROUND_CASE").is_some()
}

/// 回收 Job 句柄（isize 规避裸句柄的 Send 限制；故意静态持有到进程退出，
/// 句柄随进程关闭即触发 KILL_ON_JOB_CLOSE 全树回收）。
static REAPER_JOB: OnceLock<isize> = OnceLock::new();

/// 测试进程孤儿回收根修（缺陷 2026-10-03 两次复现：`--xberg-broker` 代理无
/// 自退条件，扫描式清场与收尾 PID 杀灭均可漏杀，漏杀者存活并锁住构建产物）：
/// 把当前测试进程挂进 JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE 的 Job Object。此后
/// 本测试二进制派生的全部后代（re-exec 层、代理、引擎）经派生自动归属；测试
/// 二进制无论以何种方式退出（正常、panic、超时被 taskkill /T），Windows 连根
/// 回收整棵进程树——漏杀在结构上不可能发生。嵌套 Job 在 Win8+ 合法（CI 步骤
/// 外层 Job 不受影响）；任一调用失败时打印警告并降级为既有扫描清场，不使
/// 测试失败。
pub(super) fn ensure_child_reaper() {
    if reexec_layer() {
        return;
    }
    REAPER_JOB.get_or_init(|| {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: 创建未命名 Job Object，不传任何外部指针。
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            eprintln!("警告：孤儿回收 Job 创建失败，降级为既有扫描清场");
            return 0;
        }
        // SAFETY: JOBOBJECT_EXTENDED_LIMIT_INFORMATION 是纯 C POD 结构，全零初始化合法。
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: job 为刚创建的有效句柄；limits 为本栈纯数据结构，仅本次调用读取。
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(std::mem::size_of_val(&limits)).unwrap(),
            )
        };
        if configured == 0 {
            eprintln!("警告：孤儿回收 Job 参数设置失败，降级为既有扫描清场");
            // SAFETY: job 尚未归属任何进程且仅本函数持有，关闭无回收副作用。
            unsafe { CloseHandle(job) };
            return 0;
        }
        // SAFETY: 无参调用，返回本进程伪句柄，不涉及外部资源。
        let current = unsafe { GetCurrentProcess() };
        // SAFETY: 把自身进程挂入刚配置的 Job；嵌套归属在 Win8+ 合法，失败仅降级。
        let assigned = unsafe { AssignProcessToJobObject(job, current) };
        if assigned == 0 {
            eprintln!("警告：测试进程挂入回收 Job 失败，降级为既有扫描清场");
            // SAFETY: 归属未生效时 job 无成员且仅本函数持有，关闭无副作用。
            unsafe { CloseHandle(job) };
            return 0;
        }
        job as isize
    });
}

/// 模拟引擎编译产物（按源文件分缓存）：每测试二进制只 rustc 编译一次。
/// 2026-10-03 计时优化：此前每个用例现编一次（rustc 约 1.5~2s），引擎套件受
/// 会话锁串行，成本 1:1 计入墙钟。产物按进程 PID 落在 .tmp/mock-engines/，
/// 用例侧经 [`mock_engine_copy`] 复制到自己的临时根，互不写同一个文件。
static MOCK_ENGINE_SHARED: OnceLock<PathBuf> = OnceLock::new();
static MOCK_ENGINE_LEGACY: OnceLock<PathBuf> = OnceLock::new();

/// 把模拟引擎（`tests/fixtures/` 下按源文件名区分）编译一次并复制到 `dest`。
pub(super) fn mock_engine_copy(source: &str, dest: &Path) {
    let cache = if source.ends_with("shared_xberg_nocap.rs") {
        &MOCK_ENGINE_LEGACY
    } else {
        &MOCK_ENGINE_SHARED
    };
    let compiled = cache.get_or_init(|| compile_mock_engine(source));
    std::fs::copy(compiled, dest).unwrap_or_else(|error| {
        panic!(
            "复制模拟引擎失败（{source} -> {}）：{error}",
            dest.display()
        )
    });
}

/// 编译模拟引擎到 .tmp/mock-engines/<pid>-<stem>.exe（cwd 固定为仓库根，
/// 与夹具源相对路径的既有口径一致）。
fn compile_mock_engine(source: &str) -> PathBuf {
    let stem = Path::new(source)
        .file_stem()
        .map_or_else(|| "mock".into(), |name| name.to_string_lossy().into_owned());
    let out = Path::new(ROOT)
        .join(".tmp/mock-engines")
        .join(format!("{}-{stem}.exe", std::process::id()));
    let _ = std::fs::create_dir_all(out.parent().unwrap());
    let status = std::process::Command::new("rustc")
        .current_dir(ROOT)
        .args(["--edition=2021", source, "-o"])
        .arg(&out)
        .status()
        .unwrap_or_else(|error| panic!("无法启动 rustc（{source}）：{error}"));
    assert!(status.success(), "模拟引擎编译失败（{source}）：{status:?}");
    out
}
