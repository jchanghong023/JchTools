//! 覆盖 CA-02～CA-08：真实 Windows DPAPI、隔离 HKCU 与独立内部后台，不显示桌面。
#![cfg(windows)] // P-07：产品仅支持 Windows，使用真实 CurrentUser DPAPI。
use fs2::FileExt;
use jchtools_codeagent::codeagent::{self, CheckResult};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{LocalFree, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::Security::Cryptography::{
    CryptBinaryToStringW, CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    CRYPT_STRING_BASE64, CRYPT_STRING_NOCRLF,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY_CURRENT_USER, KEY_ALL_ACCESS, KEY_QUERY_VALUE, REG_SZ,
};

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn protect(value: impl AsRef<[u8]>) -> Vec<u8> {
    let value = value.as_ref();
    let input = CRYPT_INTEGER_BLOB {
        cbData: value.len() as u32,
        pbData: value.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    assert_ne!(
        unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        },
        0
    );
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe { LocalFree(output.pbData.cast()) };
    bytes
}
fn base64(value: &[u8]) -> String {
    let flags = CRYPT_STRING_BASE64 | CRYPT_STRING_NOCRLF;
    let mut size = 0;
    assert_ne!(
        unsafe {
            CryptBinaryToStringW(
                value.as_ptr(),
                value.len() as u32,
                flags,
                std::ptr::null_mut(),
                &mut size,
            )
        },
        0
    );
    let mut output = vec![0u16; size as usize];
    assert_ne!(
        unsafe {
            CryptBinaryToStringW(
                value.as_ptr(),
                value.len() as u32,
                flags,
                output.as_mut_ptr(),
                &mut size,
            )
        },
        0
    );
    let end = output
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(output.len());
    String::from_utf16(&output[..end]).unwrap()
}
fn credentials(path: &Path, bytes: &[u8]) {
    std::fs::write(
        path,
        serde_json::to_vec(&serde_json::json!({
            "idaasOAuth": { "xAuthToken": base64(bytes) }
        }))
        .unwrap(),
    )
    .unwrap();
}
fn registry_value(subkey: &str) -> (u32, Vec<u16>) {
    let mut key = std::ptr::null_mut();
    assert_eq!(
        unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide(subkey).as_ptr(),
                0,
                KEY_QUERY_VALUE,
                &mut key,
            )
        },
        ERROR_SUCCESS
    );
    let mut kind = 0;
    let mut size = 0;
    let name = wide("JCH_CODEAGENT3_X_AUTH_TOKEN");
    assert_eq!(
        unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null(),
                &mut kind,
                std::ptr::null_mut(),
                &mut size,
            )
        },
        ERROR_SUCCESS
    );
    let mut value = vec![0u16; (size as usize).div_ceil(2)];
    assert_eq!(
        unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null(),
                &mut kind,
                value.as_mut_ptr().cast(),
                &mut size,
            )
        },
        ERROR_SUCCESS
    );
    unsafe { RegCloseKey(key) };
    (kind, value)
}
fn wait_for(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("隔离 CodeAgent 后台未达到预期状态");
}
struct Isolation {
    root: tempfile::TempDir,
    registry: String,
}
impl Drop for Isolation {
    fn drop(&mut self) {
        let _ = codeagent::stop();
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&self.registry).as_ptr()) };
    }
}

/// 真实短命宿主：父用例启动本测试入口，启动独立服务后正常退出。
#[test]
fn codeagent_spawning_host() {
    if std::env::var_os("JCHTOOLS_CODEAGENT_HOST_TEST").is_some() {
        let executable = std::env::var_os("JCHTOOLS_CODEAGENT_TEST_EXE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_JchTools")));
        let status = codeagent::ensure_started_for_test(executable).unwrap_or_else(|error| {
            let classification = match error.as_str() {
                "读取 CodeAgent 后台运行状态失败" => "lock_status_failed",
                "CodeAgent 后台尚未就绪，请重新打开页面" => "service_not_ready",
                _ => "startup_failed",
            };
            if let Some(root) = std::env::var_os("JCHTOOLS_TEST_STATE_DIR") {
                let _ = std::fs::write(
                    PathBuf::from(root).join("codeagent-host-error"),
                    classification,
                );
            }
            panic!("隔离 CodeAgent 宿主启动失败");
        });
        assert!(status.running && status.pid.is_some());
    }
}

#[test]
fn codeagent_native_registry_and_detached_service() {
    let root = tempfile::tempdir().unwrap();
    let registry = format!(
        "Software\\JchTools\\Tests\\CodeAgent\\{}",
        uuid::Uuid::new_v4()
    );
    let source = root.path().join("credentials.json");
    std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.path());
    std::env::set_var("JCHTOOLS_TEST_CODEAGENT_CREDENTIALS", &source);
    std::env::set_var("JCHTOOLS_TEST_CODEAGENT_REGISTRY", &registry);
    let isolated = Isolation { root, registry };
    // CA-07：非 Unicode 的隔离注册表变量仍表示尝试隔离，不能回退真实凭据。
    {
        use std::os::windows::ffi::OsStringExt;
        std::env::remove_var("JCHTOOLS_TEST_CODEAGENT_CREDENTIALS");
        std::env::set_var(
            "JCHTOOLS_TEST_CODEAGENT_REGISTRY",
            std::ffi::OsString::from_wide(&[0xd800]),
        );
        let result =
            codeagent::ensure_started_for_test(PathBuf::from(env!("CARGO_BIN_EXE_JchTools")));
        assert!(
            matches!(result, Err(error) if error == "CodeAgent 测试隔离配置不完整，拒绝访问真实凭据或注册表")
        );
        std::env::set_var("JCHTOOLS_TEST_CODEAGENT_CREDENTIALS", &source);
        std::env::set_var("JCHTOOLS_TEST_CODEAGENT_REGISTRY", &isolated.registry);
    }
    // CA-03 / CA-07：用户环境键尚不存在时，失败不创建键，成功首次写入应创建。
    let missing_registry = format!("{}\\first-run", isolated.registry);
    std::env::set_var("JCHTOOLS_TEST_CODEAGENT_REGISTRY", &missing_registry);
    credentials(&source, b"wrong ciphertext");
    assert_eq!(codeagent::check_once(), CheckResult::DecryptFailed);
    let mut missing_key = std::ptr::null_mut();
    assert_eq!(
        unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide(&missing_registry).as_ptr(),
                0,
                KEY_QUERY_VALUE,
                &mut missing_key,
            )
        },
        ERROR_FILE_NOT_FOUND
    );
    let first_token = "synthetic-first-run";
    credentials(&source, &protect(first_token));
    assert_eq!(codeagent::check_once(), CheckResult::Updated);
    let first_value = registry_value(&missing_registry);
    assert!(first_value.0 == REG_SZ && first_value.1 == wide(first_token));
    assert_eq!(codeagent::check_once(), CheckResult::Unchanged);
    std::env::set_var("JCHTOOLS_TEST_CODEAGENT_REGISTRY", &isolated.registry);
    let mut key = std::ptr::null_mut();
    assert_eq!(
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                wide(&isolated.registry).as_ptr(),
                0,
                std::ptr::null(),
                0,
                KEY_ALL_ACCESS,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            )
        },
        ERROR_SUCCESS
    );
    let initial = wide("synthetic-preserved");
    assert_eq!(
        unsafe {
            RegSetValueExW(
                key,
                wide("JCH_CODEAGENT3_X_AUTH_TOKEN").as_ptr(),
                0,
                REG_SZ,
                initial.as_ptr().cast(),
                (initial.len() * 2) as u32,
            )
        },
        ERROR_SUCCESS
    );
    unsafe { RegCloseKey(key) };

    let token = "synthetic-中文-🔒";
    let cipher = protect(token);
    assert!(codeagent::decrypt_token(&cipher).is_ok_and(|value| value == token));
    credentials(&source, b"wrong ciphertext");
    assert_eq!(codeagent::check_once(), CheckResult::DecryptFailed);
    let before = registry_value(&isolated.registry);
    assert!(before.0 == REG_SZ && before.1 == initial);
    // CA-02 / CA-07：严格 UTF-8、读取及 JSON 失败都保留现有值，不传播正文。
    credentials(&source, &protect([0xff]));
    assert_eq!(codeagent::check_once(), CheckResult::InvalidUtf8);
    assert!(registry_value(&isolated.registry) == before);
    std::fs::write(&source, b"{invalid json").unwrap();
    assert_eq!(codeagent::check_once(), CheckResult::DecodeFailed);
    assert!(registry_value(&isolated.registry) == before);
    std::fs::remove_file(&source).unwrap();
    assert_eq!(codeagent::check_once(), CheckResult::ReadFailed);
    assert!(registry_value(&isolated.registry) == before);
    credentials(&source, &cipher);
    assert_eq!(codeagent::check_once(), CheckResult::Updated);
    let updated = registry_value(&isolated.registry);
    assert!(updated.0 == REG_SZ && updated.1 == wide(token));
    assert_eq!(codeagent::check_once(), CheckResult::Unchanged);
    assert_eq!(codeagent::load_interval().unwrap(), 60);
    for invalid in ["0", "-1", "1.5", "", "18446744073709551616"] {
        assert!(codeagent::save_interval(invalid).is_err());
    }
    codeagent::save_interval(&u64::MAX.to_string()).unwrap();
    assert_eq!(codeagent::load_interval().unwrap(), u64::MAX);
    codeagent::save_interval("1").unwrap();
    // CA-05：Windows 原生锁冲突必须同时被状态探测和重复服务入口识别。
    {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(isolated.root.path().join("codeagent-service.lock"))
            .unwrap();
        // CA-05：只读观察者不代表服务存活；状态探测不能争抢后台的独占锁。
        FileExt::try_lock_shared(&lock).unwrap();
        assert!(
            !codeagent::status().unwrap().running,
            "共享只读观察者不能被误判为独占后台"
        );
        FileExt::unlock(&lock).unwrap();
        lock.try_lock_exclusive().unwrap();
        let state = codeagent::status().unwrap();
        assert!(state.running && state.pid.is_none());
        assert!(codeagent::serve().is_ok());
    }
    assert!(!codeagent::status().unwrap().running);
    // CA-05：首次启动时 SQLite 不可用，仍启动独立后台并维护隔离凭据；
    // 存储恢复后补写状态和已保存的间隔，不要求重新打开页面。
    let database = isolated.root.path().join("config.sqlite3");
    let saved_database = isolated.root.path().join("config.sqlite3.saved");
    std::fs::rename(&database, &saved_database).unwrap();
    std::fs::create_dir(&database).unwrap();
    let outage_token = "synthetic-startup-outage";
    credentials(&source, &protect(outage_token));
    assert!(
        codeagent::ensure_started_for_test(PathBuf::from(env!("CARGO_BIN_EXE_JchTools"))).is_err(),
        "存储不可用时不能声称安全状态已读取"
    );
    wait_for(|| registry_value(&isolated.registry).1 == wide(outage_token));
    std::fs::remove_dir(&database).unwrap();
    std::fs::rename(&saved_database, &database).unwrap();
    wait_for(|| {
        codeagent::status().is_ok_and(|state| {
            state.running
                && state.pid.is_some()
                && state.last_check.is_some()
                && state.interval_seconds == 1
        })
    });
    codeagent::stop().unwrap();
    wait_for(|| codeagent::status().is_ok_and(|state| !state.running));
    credentials(&source, &cipher);
    assert_eq!(codeagent::check_once(), CheckResult::Updated);
    assert!(registry_value(&isolated.registry) == updated);
    let mut host = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "codeagent_spawning_host", "--nocapture"])
        .env("JCHTOOLS_CODEAGENT_HOST_TEST", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(|| host.try_wait().unwrap().is_some());
    // 独立后台可能继承宿主句柄；只等待宿主退出，不等待输出管道 EOF。
    let exit = host.wait().unwrap();
    let marker = std::fs::read_to_string(isolated.root.path().join("codeagent-host-error"))
        .unwrap_or_default();
    let classification = match marker.as_str() {
        "lock_status_failed" => "后台锁状态探测失败",
        "service_not_ready" => "后台尚未就绪",
        "startup_failed" => "后台启动失败",
        _ => "未提供去敏错误分类",
    };
    assert!(exit.success(), "隔离宿主失败（{exit}）：{classification}");
    wait_for(|| codeagent::status().is_ok_and(|state| state.last_check.is_some()));
    let state = codeagent::status().unwrap();
    assert!(state.running && state.pid.is_some());
    let first = state.last_check.clone();
    let pid = state.pid;
    // CA-05：SQLite 争用跨过 5 秒 busy_timeout 与轮询边界，后台仍持锁。
    // 直接 ReadFile 观察独占锁，不通过同样被 SQLite 阻塞的状态查询判断存活。
    let mut service_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(isolated.root.path().join("codeagent-service.lock"))
        .unwrap();
    let storage = rusqlite::Connection::open(isolated.root.path().join("config.sqlite3")).unwrap();
    storage.execute_batch("BEGIN EXCLUSIVE").unwrap();
    // CA-04 / CA-05：争用期间仍按上次有效的 1 秒间隔维护凭据，而不只保留锁。
    let contention_tokens = ["synthetic-contention-first", "synthetic-contention-second"];
    credentials(&source, &protect(contention_tokens[0]));
    std::thread::sleep(Duration::from_secs(3));
    let first_maintained = registry_value(&isolated.registry).1 == wide(contention_tokens[0]);
    credentials(&source, &protect(contention_tokens[1]));
    std::thread::sleep(Duration::from_secs(3));
    let second_maintained = registry_value(&isolated.registry).1 == wide(contention_tokens[1]);
    std::thread::sleep(Duration::from_secs(6));
    let lock_observation = service_lock.read(&mut [0]);
    storage.execute_batch("ROLLBACK").unwrap();
    drop(storage);
    credentials(&source, &cipher);
    assert!(
        first_maintained && second_maintained,
        "共享 SQLite 争用阻塞了 CodeAgent 的凭据检查周期"
    );
    assert!(
        lock_observation.is_err_and(|error| {
            error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
        }),
        "CodeAgent 后台在 SQLite 争用期间释放了独占锁"
    );
    wait_for(|| codeagent::status().is_ok_and(|state| state.running && state.pid == pid));
    // 宿主已经退出，但后台仍完成下一个真实检查周期。
    wait_for(|| codeagent::status().is_ok_and(|state| state.running && state.last_check != first));
    wait_for(|| registry_value(&isolated.registry) == updated);
    let mut duplicate = Command::new(env!("CARGO_BIN_EXE_JchTools"))
        .arg("--codeagent-service")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(|| duplicate.try_wait().unwrap().is_some());
    assert!(duplicate.wait().unwrap().success());
    assert_eq!(codeagent::status().unwrap().pid, pid);
    assert_eq!(
        codeagent::ensure_started_for_test(PathBuf::from(env!("CARGO_BIN_EXE_JchTools")))
            .unwrap()
            .pid,
        pid
    );
    codeagent::save_interval("2").unwrap();
    wait_for(|| codeagent::status().is_ok_and(|state| state.interval_seconds == 2));
    assert_eq!(codeagent::load_interval().unwrap(), 2);
    credentials(&source, b"wrong ciphertext");
    wait_for(|| codeagent::status().is_ok_and(|state| state.result == CheckResult::DecryptFailed));
    assert!(registry_value(&isolated.registry) == updated);
    credentials(&source, &protect("synthetic-rotated"));
    wait_for(|| registry_value(&isolated.registry).1 == wide("synthetic-rotated"));
    let safe_status = serde_json::to_string(&codeagent::status().unwrap()).unwrap();
    assert!(
        !safe_status.contains(token)
            && !safe_status.contains("synthetic-rotated")
            && !safe_status.contains(outage_token)
            && contention_tokens
                .iter()
                .all(|value| !safe_status.contains(value))
    );
    codeagent::stop().unwrap();
    wait_for(|| codeagent::status().is_ok_and(|state| !state.running));
    let logs = isolated.root.path().join("logs");
    if logs.exists() {
        for entry in std::fs::read_dir(logs).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let text = std::fs::read_to_string(path).unwrap();
                assert!(
                    !text.contains(token)
                        && !text.contains("synthetic-rotated")
                        && !text.contains(outage_token)
                        && contention_tokens.iter().all(|value| !text.contains(value))
                );
            }
        }
    }
}

fn review_remediation_child(test: &str) -> bool {
    if std::env::var("JCHTOOLS_CODEAGENT_REVIEW_CASE").as_deref() == Ok(test) {
        return true;
    }
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("JCHTOOLS_CODEAGENT_REVIEW_CASE", test)
        .env_remove("JCHTOOLS_CODEAGENT_HOST_TEST")
        .status()
        .unwrap();
    assert!(result.success(), "隔离回归子进程失败：{test}");
    false
}

fn review_remediation_isolation() -> (Isolation, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("credentials.json");
    let registry = format!(
        "Software\\JchTools\\Tests\\CodeAgent\\review-remediation-{}",
        uuid::Uuid::new_v4()
    );
    // 每个场景在独立测试进程中设置环境，不与既有测试争抢全局配置。
    std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.path());
    std::env::set_var("JCHTOOLS_TEST_CODEAGENT_CREDENTIALS", &source);
    std::env::set_var("JCHTOOLS_TEST_CODEAGENT_REGISTRY", &registry);
    (Isolation { root, registry }, source)
}

fn review_remediation_run(command: &mut Command) -> std::process::ExitStatus {
    review_remediation_run_logged(command, None)
}

fn review_remediation_print_command_log(path: &Path) {
    use std::io::{Read, Seek, SeekFrom};
    // 子进程直接写文件而非 pipe；失败时只展示末尾 64 KiB，避免阻塞或无界读取。
    let output = (|| -> std::io::Result<Vec<u8>> {
        let mut file = std::fs::File::open(path)?;
        let start = file.metadata()?.len().saturating_sub(64 * 1024);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(64 * 1024).read_to_end(&mut bytes)?;
        Ok(bytes)
    })();
    match output {
        Ok(bytes) => eprintln!(
            "编译日志 {}（末尾至多 64 KiB）：\n{}",
            path.display(),
            String::from_utf8_lossy(&bytes)
        ),
        Err(error) => eprintln!("无法读取编译日志 {}：{error}", path.display()),
    }
}

fn review_remediation_run_logged(
    command: &mut Command,
    log: Option<&Path>,
) -> std::process::ExitStatus {
    use std::os::windows::process::CommandExt;
    command.stdin(Stdio::null());
    if let Some(path) = log {
        let output = std::fs::File::create(path).unwrap();
        command
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output));
    } else {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let mut child = command
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW；不进入 GUI 路由。
        .spawn()
        .unwrap_or_else(|error| panic!("无法启动隔离命令 {command:?}：{error}"));
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            if !status.success() {
                if let Some(path) = log {
                    eprintln!("编译子进程退出：{status}，真实退出码：{:?}", status.code());
                    review_remediation_print_command_log(path);
                }
            }
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let status = child.wait();
            if let Some(path) = log {
                eprintln!("编译子进程超过 90 秒，终止后等待结果：{status:?}");
                review_remediation_print_command_log(path);
            }
            panic!("隔离内部命令或安装工具超时，已终止调用进程");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn review_remediation_p3_22_saved_interval_is_not_effective_interval() {
    let name = "review_remediation_p3_22_saved_interval_is_not_effective_interval";
    if !review_remediation_child(name) {
        return;
    }
    let (isolated, _) = review_remediation_isolation();
    codeagent::save_interval("2").unwrap();
    let connection =
        rusqlite::Connection::open(isolated.root.path().join("config.sqlite3")).unwrap();
    let mut stored = codeagent::Status {
        running: true,
        pid: Some(123),
        interval_seconds: 60,
        last_check: Some("synthetic-safe-check-time".into()),
        result: CheckResult::Unchanged,
    };
    connection
        .execute(
            "INSERT INTO app_settings(key,value) VALUES('codeagent_status',?1)",
            [serde_json::to_string(&stored).unwrap()],
        )
        .unwrap();
    // P3-22：同一真实数据库的已保存值 2 与后台有效值 60 必须分别返回。
    let (state, saved) = codeagent::status_with_saved_interval().unwrap();
    assert_eq!(state.interval_seconds, 60, "后台有效值不能被已保存值覆盖");
    assert_eq!(saved, 2);
    assert!(
        !state.running && state.pid.is_none(),
        "陈旧状态不能伪报后台存活"
    );
    assert_eq!(state.result, CheckResult::Unchanged);
    assert_eq!(state.last_check, stored.last_check);
    stored.interval_seconds = 0;
    connection
        .execute(
            "UPDATE app_settings SET value=?1 WHERE key='codeagent_status'",
            [serde_json::to_string(&stored).unwrap()],
        )
        .unwrap();
    let (state, saved) = codeagent::status_with_saved_interval().unwrap();
    assert_eq!(state.interval_seconds, 2);
    assert_eq!(saved, 2);
}

#[test]
fn review_remediation_p3_29_maintenance_releases_detached_image() {
    let name = "review_remediation_p3_29_maintenance_releases_detached_image";
    if !review_remediation_child(name) {
        return;
    }
    let (isolated, source) = review_remediation_isolation();
    let token = "synthetic-review-remediation-maintenance";
    credentials(&source, &protect(token));
    codeagent::save_interval("1").unwrap();
    let image = isolated.root.path().join("JchTools.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_JchTools"), &image).unwrap();
    // CA-08：未运行/失效请求幂等；旧请求不能让之后正常启动的服务立即退出。
    let stop_request = isolated.root.path().join("codeagent-service.stop");
    std::fs::write(&stop_request, b"stale").unwrap();
    for _ in 0..2 {
        assert!(review_remediation_run(
            Command::new(&image).args(["--codeagent-service", "--stop"]),
        )
        .success());
    }
    std::fs::write(&stop_request, b"stale").unwrap();
    let host = review_remediation_run(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "codeagent_spawning_host", "--nocapture"])
            .env("JCHTOOLS_CODEAGENT_HOST_TEST", "1")
            .env("JCHTOOLS_CODEAGENT_TEST_EXE", &image),
    );
    assert!(host.success(), "临时运行镜像的正常宿主必须成功退出");
    let first = codeagent::status().unwrap();
    assert!(first.running && first.pid.is_some());
    // P3-29 / CA-05：宿主已退出，后台仍执行后续检查，不得把常驻当成缺陷。
    wait_for(|| {
        codeagent::status().is_ok_and(|state| {
            state.running && state.pid == first.pid && state.last_check != first.last_check
        })
    });
    assert_eq!(registry_value(&isolated.registry), (REG_SZ, wide(token)));
    assert!(
        std::fs::OpenOptions::new()
            .write(true)
            .open(&image)
            .is_err(),
        "运行镜像必须真实被独立后台占用"
    );
    // CA-08 / P3-29：维护退出不依赖 SQLite 即时可写；等待自然退出后才能换/删 EXE。
    let storage = rusqlite::Connection::open(isolated.root.path().join("config.sqlite3")).unwrap();
    storage.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let stopped =
        review_remediation_run(Command::new(&image).args(["--codeagent-service", "--stop"]));
    assert!(stopped.success(), "维护命令必须等待原后台退出并报告成功");
    storage.execute_batch("ROLLBACK").unwrap();
    drop(storage);
    assert!(
        !codeagent::status().unwrap().running,
        "维护命令返回后原后台必须已经释放单实例锁"
    );
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(isolated.root.path().join("codeagent-service.lock"))
        .unwrap();
    lock.try_lock_exclusive().unwrap();
    FileExt::unlock(&lock).unwrap();
    // 不仅查询状态：安装替换/卸载删除需要真实可写、可删除的运行镜像。
    let replacement = b"synthetic replacement image";
    std::fs::write(&image, replacement).unwrap();
    assert_eq!(std::fs::read(&image).unwrap().as_slice(), replacement);
    // 立即覆盖已证明运行镜像释放；覆盖后的新文件仍可能短暂遇到 Windows 共享冲突。
    // 仅等待共享冲突消退，不放宽覆盖断言，也不吞掉其它删除错误。
    wait_for(|| match std::fs::remove_file(&image) {
        Ok(()) => true,
        Err(error) if error.raw_os_error() == Some(32) => false,
        Err(error) => panic!("删除已替换的隔离测试镜像失败：{error}"),
    });
    assert!(!image.exists());
}

struct ReviewRemediationInstaller {
    root: PathBuf,
    app_id: String,
}

impl ReviewRemediationInstaller {
    fn prepare(root: &Path) -> Self {
        let fixture = Self {
            root: root.to_owned(),
            app_id: format!("JchTools-review-remediation-{}", uuid::Uuid::new_v4()),
        };
        std::fs::create_dir(root.join("package")).unwrap();
        std::fs::create_dir(root.join("setup")).unwrap();
        std::fs::copy(
            env!("CARGO_BIN_EXE_JchTools"),
            root.join("package/JchTools.exe"),
        )
        .unwrap();
        // 不调用 OCR；这个惰性载荷只证明 WindowsApps 写入确实隔离在临时根。
        std::fs::write(
            root.join("package/jchocr.exe"),
            b"synthetic isolated OCR payload",
        )
        .unwrap();
        // CA-08 只验安装维护链路：补齐安装器要求的合成基础布局，不作为 OCR 验收证据。
        let runtime = root.join("package/xberg");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::write(
            runtime.join("xberg.exe"),
            b"synthetic isolated Xberg payload",
        )
        .unwrap();
        std::fs::write(
            runtime.join("jchtools-xberg-release.json"),
            br#"{"fixture":"codeagent installer lifecycle; not OCR evidence"}"#,
        )
        .unwrap();
        fixture.build();
        fixture
    }

    fn build(&self) {
        let compiler = std::env::var_os("JCHTOOLS_TEST_ISCC")
            .map(PathBuf::from)
            .or_else(|| {
                ["PROGRAMFILES(X86)", "PROGRAMFILES"]
                    .into_iter()
                    .filter_map(std::env::var_os)
                    .map(|root| PathBuf::from(root).join("Inno Setup 6/ISCC.exe"))
                    .find(|path| path.is_file())
            })
            .or_else(|| {
                std::env::var_os("LOCALAPPDATA")
                    .map(|root| PathBuf::from(root).join("Programs/Inno Setup 6/ISCC.exe"))
                    .filter(|path| path.is_file())
            })
            .unwrap_or_else(|| PathBuf::from("ISCC.exe"));
        let log = self.root.join("iscc-build.log");
        let compiled = review_remediation_run_logged(
            Command::new(&compiler)
                .arg("/Q")
                .arg(format!(
                    "/DSourceDir={}",
                    self.root.join("package").display()
                ))
                .arg(format!("/DOutputDir={}", self.root.join("setup").display()))
                .arg(format!("/DReviewRemediationRoot={}", self.root.display()))
                .arg(format!("/DReviewRemediationAppId={}", self.app_id))
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("installer/JchTools.iss")),
            Some(&log),
        );
        assert!(
            compiled.success(),
            "前提失败：ISCC 编译真实隔离安装脚本失败（尚未安装或启动 legacy）；编译器={}，退出状态={compiled}，真实退出码={:?}，日志={}",
            compiler.display(),
            compiled.code(),
            log.display(),
        );
    }

    fn install(&self, log: &str) -> std::process::ExitStatus {
        review_remediation_run(&mut self.install_command(log))
    }

    fn install_command(&self, log: &str) -> Command {
        let mut command = Command::new(self.root.join("setup/JchTools-Setup-x64.exe"));
        command
            .args([
                "/VERYSILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/NOCLOSEAPPLICATIONS",
                "/NORESTARTAPPLICATIONS",
                "/NOICONS",
                "/TASKS=",
            ])
            .arg(format!("/DIR={}", self.root.join("installed").display()))
            .arg(format!("/LOG={}", self.root.join(log).display()));
        command
    }

    fn uninstall(&self) -> std::process::ExitStatus {
        review_remediation_run(
            Command::new(self.root.join("installed/unins000.exe"))
                .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
                .arg(format!(
                    "/LOG={}",
                    self.root.join("uninstall.log").display()
                )),
        )
    }

    fn start_service(&self) {
        let state =
            codeagent::ensure_started_for_test(self.root.join("installed/JchTools.exe")).unwrap();
        assert!(state.running && state.pid.is_some());
        wait_for(|| {
            codeagent::status()
                .is_ok_and(|state| state.running && state.result == CheckResult::Unchanged)
        });
    }
}

impl Drop for ReviewRemediationInstaller {
    fn drop(&mut self) {
        // 失败也只清理本场景：生产维护路径释放镜像，再运行随机 AppId 的卸载器。
        let _ = codeagent::stop();
        let uninstaller = self.root.join("installed/unins000.exe");
        if uninstaller.is_file() {
            use std::os::windows::process::CommandExt;
            if let Ok(mut child) = Command::new(uninstaller)
                .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
                .creation_flags(0x0800_0000)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                let deadline = Instant::now() + Duration::from_secs(20);
                while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let key = format!(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{}_is1",
            self.app_id
        );
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&key).as_ptr()) };
    }
}

#[test]
fn review_remediation_p3_29_installer_upgrade_stops_original_service() {
    let name = "review_remediation_p3_29_installer_upgrade_stops_original_service";
    if !review_remediation_child(name) {
        return;
    }
    let (isolated, source) = review_remediation_isolation();
    credentials(&source, &protect("synthetic-review-upgrade"));
    codeagent::save_interval("1").unwrap();
    let installer = ReviewRemediationInstaller::prepare(isolated.root.path());
    assert!(installer.install("initial-install.log").success());
    assert_eq!(
        std::fs::read(isolated.root.path().join("WindowsApps/jchocr.exe"))
            .unwrap()
            .as_slice(),
        b"synthetic isolated OCR payload"
    );
    installer.start_service();
    let payload = isolated.root.path().join("package/JchTools.exe");
    let mut replacement = std::fs::read(&payload).unwrap();
    replacement.extend_from_slice(b"synthetic-review-remediation-upgrade-overlay");
    std::fs::write(&payload, &replacement).unwrap();
    installer.build();
    // P3-29：真实安装器执行升级；禁用 RM 代停，必须由安装脚本请求生产维护退出。
    let upgraded = installer.install("upgrade-install.log");
    assert!(
        upgraded.success(),
        "安装器必须通过维护退出完成被占用 EXE 的升级"
    );
    assert!(
        !codeagent::status().unwrap().running,
        "安装替换前必须停止原后台，而非仅安排重启或忽略占用"
    );
    assert_eq!(
        std::fs::read(isolated.root.path().join("installed/JchTools.exe")).unwrap(),
        replacement,
        "升级必须真实替换运行镜像"
    );
}

#[test]
fn review_remediation_p3_29_uninstaller_stops_original_service() {
    let name = "review_remediation_p3_29_uninstaller_stops_original_service";
    if !review_remediation_child(name) {
        return;
    }
    let (isolated, source) = review_remediation_isolation();
    credentials(&source, &protect("synthetic-review-uninstall"));
    codeagent::save_interval("1").unwrap();
    let installer = ReviewRemediationInstaller::prepare(isolated.root.path());
    assert!(installer.install("initial-install.log").success());
    installer.start_service();
    // P3-29：真实 Inno 卸载消费者，不能用直接调用 stop 冒充卸载链路。
    let removed = installer.uninstall();
    assert!(removed.success(), "卸载器必须通过维护退出成功删除程序");
    assert!(
        !codeagent::status().unwrap().running,
        "卸载完成后原后台不得继续持有运行镜像"
    );
    assert!(
        !isolated.root.path().join("installed/JchTools.exe").exists(),
        "卸载必须真实删除主 EXE，而非留下占用文件"
    );
    assert!(!isolated.root.path().join("WindowsApps/jchocr.exe").exists());
}

/// 只拥有本用例直接创建的子进程；失败展开时也回收排队的服务/安装器。
struct ReviewRemediationProcess(std::process::Child);

impl ReviewRemediationProcess {
    fn spawn(command: &mut Command) -> Self {
        use std::os::windows::process::CommandExt;
        Self(
            command
                .creation_flags(0x0800_0000)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    fn wait(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::WaitForSingleObject,
        };
        let result = unsafe {
            WaitForSingleObject(
                self.0.as_raw_handle().cast(),
                u32::try_from(timeout.as_millis()).unwrap(),
            )
        };
        match result {
            WAIT_OBJECT_0 => Some(self.0.wait().unwrap()),
            WAIT_TIMEOUT => None,
            _ => panic!("等待本用例子进程句柄失败"),
        }
    }
}

impl Drop for ReviewRemediationProcess {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

/// 外部旧程序边界，不复刻旧产品：认识 service、忽略 stop，直到测试主动放行。
struct ReviewRemediationLegacy {
    image: PathBuf,
    started: PathBuf,
    release: PathBuf,
    released: bool,
}

impl ReviewRemediationLegacy {
    fn prepare(root: &Path) -> Self {
        let fixture = Self {
            image: root.join("legacy-fixture.exe"),
            started: root.join("legacy-started"),
            release: root.join("legacy-release"),
            released: false,
        };
        let source = root.join("legacy-fixture.rs");
        std::fs::write(
            &source,
            r#"#![windows_subsystem = "windows"]
fn main() {
    if std::env::args().nth(1).as_deref() != Some("--codeagent-service") {
        std::process::exit(2);
    }
    let root = std::path::PathBuf::from(
        std::env::var_os("JCHTOOLS_TEST_STATE_DIR").expect("isolated fixture root"),
    );
    let release = root.join("legacy-release");
    std::fs::write(root.join("legacy-starting"), std::process::id().to_string()).unwrap();
    std::fs::rename(root.join("legacy-starting"), root.join("legacy-started")).unwrap();
    while !release.exists() && root.exists() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
"#,
        )
        .unwrap();
        let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let compiled = review_remediation_run(
            Command::new(compiler)
                .arg("--edition=2021")
                .arg("--crate-name=codeagent_legacy_boundary")
                .arg(&source)
                .arg("-o")
                .arg(&fixture.image),
        );
        assert!(compiled.success(), "前提失败：必须能编译受控旧程序边界");
        fixture
    }

    fn release_and_wait(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::{
            Foundation::WAIT_OBJECT_0,
            System::Threading::{
                OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
                PROCESS_TERMINATE,
            },
        };
        // 先打开仍等 release 的 fixture，再放行，避免 PID 复用误指向其他进程。
        let process = std::fs::read_to_string(&self.started)
            .ok()
            .and_then(|pid| pid.parse::<u32>().ok())
            .and_then(|pid| {
                let handle =
                    unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
                (!handle.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(handle.cast()) })
            });
        let _ = std::fs::write(&self.release, b"release");
        if let Some(process) = process {
            let handle = process.as_raw_handle().cast();
            if unsafe { WaitForSingleObject(handle, 10_000) } != WAIT_OBJECT_0 {
                // 仅失败清理本测试合成 fixture，绝不用于停止产品后台。
                unsafe {
                    TerminateProcess(handle, 1);
                    WaitForSingleObject(handle, 10_000);
                }
            }
        }
    }
}

impl Drop for ReviewRemediationLegacy {
    fn drop(&mut self) {
        self.release_and_wait();
    }
}

#[test]
fn review_remediation_p3_29_cold_legacy_upgrade_does_not_launch_old_image() {
    let name = "review_remediation_p3_29_cold_legacy_upgrade_does_not_launch_old_image";
    if !review_remediation_child(name) {
        return;
    }
    let (isolated, _) = review_remediation_isolation();
    let installer = ReviewRemediationInstaller::prepare(isolated.root.path());
    assert!(installer.install("initial-install.log").success());
    let mut legacy = ReviewRemediationLegacy::prepare(isolated.root.path());
    let installed = isolated.root.path().join("installed/JchTools.exe");
    std::fs::copy(&legacy.image, &installed).unwrap();
    assert!(
        !codeagent::status().unwrap().running,
        "旧版本必须是 cold 状态"
    );
    let replacement = std::fs::read(isolated.root.path().join("package/JchTools.exe")).unwrap();
    let mut upgrade =
        ReviewRemediationProcess::spawn(&mut installer.install_command("cold-legacy-upgrade.log"));
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut launched_legacy = false;
    let upgraded = loop {
        if legacy.started.exists() {
            launched_legacy = true;
            // 错误实现会卡在 ewWaitUntilTerminated；主动放行再断言，红证不能留下进程。
            legacy.release_and_wait();
        }
        if let Some(status) = upgrade.wait(Duration::from_millis(50)) {
            break status;
        }
        assert!(Instant::now() < deadline, "隔离旧版升级未在有界时间内完成");
    };
    legacy.release_and_wait();
    assert!(upgraded.success(), "cold 旧版本升级必须正常完成");
    assert_eq!(
        std::fs::read(&installed).unwrap(),
        replacement,
        "必须真正换为新镜像"
    );
    assert_eq!(
        std::fs::read(isolated.root.path().join("WindowsApps/jchocr.exe")).unwrap(),
        b"synthetic isolated OCR payload",
    );
    assert!(
        !launched_legacy && !legacy.started.exists(),
        "安装维护不得执行不支持 stop 协议的旧镜像并把 cold 旧后台启动"
    );
}

#[test]
fn review_remediation_p3_29_service_start_exits_while_maintenance_lock_is_held() {
    let name = "review_remediation_p3_29_service_start_exits_while_maintenance_lock_is_held";
    if !review_remediation_child(name) {
        return;
    }
    let (isolated, _) = review_remediation_isolation();
    let image = isolated.root.path().join("JchTools.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_JchTools"), &image).unwrap();
    // 与 stop() 相同的真实 Windows 控制锁；不用替身或 sleep 推测进入维护窗口。
    let control = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(isolated.root.path().join("codeagent-service-control.lock"))
        .unwrap();
    control.lock_exclusive().unwrap();
    let mut service =
        ReviewRemediationProcess::spawn(Command::new(&image).arg("--codeagent-service"));
    // 等待的是真实进程退出句柄；控制锁始终未释放，不接受维护后排队复活。
    let exited_while_locked = service.wait(Duration::from_secs(5));
    assert!(
        exited_while_locked.is_some(),
        "服务启动必须在维护锁仍持有时退出，不能等待锁释放后成为新后台"
    );
    assert!(exited_while_locked.unwrap().success());
    assert!(!codeagent::status().unwrap().running);
    FileExt::unlock(&control).unwrap();
    drop(control);
    // 已等待直接入口进程真正退出，后续不存在靠短 sleep 猜测的启动窗口。
    assert!(!codeagent::status().unwrap().running);
    assert!(!isolated.root.path().join("codeagent-service.pid").exists());
    let instance = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(isolated.root.path().join("codeagent-service.lock"))
        .unwrap();
    instance.try_lock_exclusive().unwrap();
    FileExt::unlock(&instance).unwrap();
    std::fs::write(&image, b"maintenance released image").unwrap();
    std::fs::remove_file(&image).unwrap();
}
