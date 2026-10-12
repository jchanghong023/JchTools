//! 覆盖 CA-02～CA-07：真实 Windows DPAPI、隔离 HKCU 与独立内部后台，不显示桌面。
#![cfg(windows)] // P-07：产品仅支持 Windows，使用真实 CurrentUser DPAPI。
use fs2::FileExt;
use jchtools::codeagent::{self, CheckResult};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
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
        let _ = std::fs::write(self.root.path().join("codeagent-test-stop"), b"stop");
        let deadline = Instant::now() + Duration::from_secs(5);
        while codeagent::status().is_ok_and(|state| state.running) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&self.registry).as_ptr()) };
    }
}

/// 真实短命宿主：父用例启动本测试入口，启动独立服务后正常退出。
#[test]
fn codeagent_spawning_host() {
    if std::env::var_os("JCHTOOLS_CODEAGENT_HOST_TEST").is_some() {
        let status =
            codeagent::ensure_started_for_test(PathBuf::from(env!("CARGO_BIN_EXE_JchTools")))
                .unwrap_or_else(|error| {
                    let classification = match error.as_str() {
                        "读取 CodeAgent 后台运行状态失败" => "lock_status_failed",
                        "CodeAgent 后台尚未就绪，请重新打开页面" => {
                            "service_not_ready"
                        }
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
    std::thread::sleep(Duration::from_secs(12));
    let lock_observation = service_lock.read(&mut [0]);
    storage.execute_batch("ROLLBACK").unwrap();
    drop(storage);
    assert!(
        lock_observation.is_err_and(|error| {
            error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
        }),
        "CodeAgent 后台在 SQLite 争用期间释放了独占锁"
    );
    wait_for(|| codeagent::status().is_ok_and(|state| state.running && state.pid == pid));
    // 宿主已经退出，但后台仍完成下一个真实检查周期。
    wait_for(|| codeagent::status().is_ok_and(|state| state.running && state.last_check != first));
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
    assert!(!safe_status.contains(token) && !safe_status.contains("synthetic-rotated"));
    std::fs::write(isolated.root.path().join("codeagent-test-stop"), b"stop").unwrap();
    wait_for(|| codeagent::status().is_ok_and(|state| !state.running));
    let logs = isolated.root.path().join("logs");
    if logs.exists() {
        for entry in std::fs::read_dir(logs).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let text = std::fs::read_to_string(path).unwrap();
                assert!(!text.contains(token) && !text.contains("synthetic-rotated"));
            }
        }
    }
}
