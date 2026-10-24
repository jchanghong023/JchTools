//! CA-02～CA-08：独立本地凭据维护；SQLite 仅保存间隔和去敏状态。
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fs2::FileExt;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{LocalFree, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::Security::Cryptography::{
    CryptStringToBinaryW, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    CRYPT_STRING_BASE64,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

const VALUE_NAME: &str = "JCH_CODEAGENT3_X_AUTH_TOKEN";
type Result<T, E = String> = std::result::Result<T, E>;

/// 仅这些静态分类可以进入数据库、GUI 与日志，绝不传递凭据解析器错误正文。
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum CheckResult {
    #[default]
    Waiting,
    Updated,
    Unchanged,
    ReadFailed,
    DecodeFailed,
    DecryptFailed,
    InvalidUtf8,
    RegistryFailed,
}
impl CheckResult {
    pub fn text(self) -> &'static str {
        match self {
            Self::Waiting => "等待首次检查",
            Self::Updated => "已更新注册表 Token",
            Self::Unchanged => "Token 未变化，无需写入",
            Self::ReadFailed => "读取凭据失败；保留已有 Token",
            Self::DecodeFailed => "凭据格式或 Base64 无效；保留已有 Token",
            Self::DecryptFailed => "CurrentUser DPAPI 解密失败；保留已有 Token",
            Self::InvalidUtf8 => "解密结果不是有效 UTF-8；保留已有 Token",
            Self::RegistryFailed => "注册表访问失败；保留已有 Token",
        }
    }
    pub fn failed(self) -> bool {
        !matches!(self, Self::Waiting | Self::Updated | Self::Unchanged)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Status {
    pub running: bool,
    pub pid: Option<u32>,
    pub interval_seconds: u64,
    pub last_check: Option<String>,
    pub result: CheckResult,
}

fn db() -> Result<rusqlite::Connection> {
    jchtools_foundation::xberg_settings::open(&jchtools_foundation::xberg_settings::state_dir()?)
        .map_err(|_| "CodeAgent 配置数据库不可用".into())
}

// CA-05：后台存储操作不等待 SQLite 争用，凭据检查不依赖存储即时可用。
fn service_db() -> Result<rusqlite::Connection> {
    jchtools_foundation::xberg_settings::open_with_timeout(
        &jchtools_foundation::xberg_settings::state_dir()?,
        Duration::ZERO,
    )
    .map_err(|_| "CodeAgent 配置数据库不可用".into())
}

pub fn load_interval() -> Result<u64> {
    read_interval(&db()?)
}

fn load_service_interval() -> Result<u64> {
    read_interval(&service_db()?)
}

fn read_interval(connection: &rusqlite::Connection) -> Result<u64> {
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM app_settings WHERE key='codeagent_interval_seconds'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| "读取 CodeAgent 检查间隔失败")?;
    value.map_or(Ok(60), |value| parse_interval(&value))
}

pub fn parse_interval(value: &str) -> Result<u64> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("检查间隔必须为正整数秒".into());
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| "检查间隔必须为有效的正整数秒".into())
}

pub fn save_interval(value: &str) -> Result<()> {
    let seconds = parse_interval(value)?;
    db()?.execute(
        "INSERT INTO app_settings(key,value) VALUES('codeagent_interval_seconds',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [seconds.to_string()],
    ).map_err(|_| "保存 CodeAgent 检查间隔失败")?;
    tracing::info!(
        component = "codeagent",
        event = "codeagent_interval_saved",
        seconds,
        "检查间隔已保存"
    );
    Ok(())
}

fn lock_file() -> Result<File> {
    named_lock_file("codeagent-service.lock")
}

fn named_lock_file(name: &str) -> Result<File> {
    let root = jchtools_foundation::xberg_settings::state_dir()?;
    std::fs::create_dir_all(&root).map_err(|_| "创建 CodeAgent 状态目录失败")?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(name))
        .map_err(|_| "打开 CodeAgent 单实例锁失败".into())
}

fn is_lock_contended(error: &std::io::Error) -> bool {
    // Windows 的 LockFileEx 返回 ERROR_LOCK_VIOLATION；不假设它映射为 WouldBlock。
    error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

/// 尝试对锁文件取得独占锁：争用属预期情形，按传入事件记录后返回 Ok(None)；
/// 其他失败返回 Err；取得成功返回 Ok(Some(file))，锁的持有期由调用方决定。
fn try_acquire_lock(
    file: File,
    contended_event: &'static str,
    contended_message: &str,
    failure_message: &'static str,
) -> Result<Option<File>> {
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if is_lock_contended(&error) => {
            tracing::info!(
                component = "codeagent",
                event = contended_event,
                "{}",
                contended_message
            );
            Ok(None)
        }
        Err(_) => Err(failure_message.into()),
    }
}

fn is_running() -> Result<bool> {
    let mut lock = lock_file()?;
    // Windows ReadFile 遇到后台独占锁会失败（空文件亦如此），但读取本身不持锁。
    match lock.read(&mut [0]) {
        Ok(_) => Ok(false),
        Err(error) if is_lock_contended(&error) => Ok(true),
        Err(_) => Err("读取 CodeAgent 后台运行状态失败".into()),
    }
}

fn control_lock_file() -> Result<File> {
    named_lock_file("codeagent-service-control.lock")
}

// CA-08：启动清理旧请求与维护请求串行，避免请求在启动握手中丢失。
fn control_lock() -> Result<File> {
    let lock = control_lock_file()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(lock),
            Err(error) if is_lock_contended(&error) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return Err("取得 CodeAgent 维护锁失败".into()),
        }
    }
}

fn remove_stop_request() -> Result<()> {
    let path = jchtools_foundation::xberg_settings::state_dir()?.join("codeagent-service.stop");
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("清理 CodeAgent 维护请求失败".into()),
    }
}

/// CA-08：只请求本应用后台自然退出，不依赖 SQLite，不终止任何进程。
/// 返回成功前等待持锁进程退出及单实例锁释放，安装器随后才可替换/删除镜像。
pub fn stop() -> Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };

    #[cfg(feature = "test-hooks")]
    test_isolation()?;
    let _control = control_lock()?;
    if !is_running()? {
        return remove_stop_request();
    }
    let root = jchtools_foundation::xberg_settings::state_dir()?;
    let pid = std::fs::read_to_string(root.join("codeagent-service.pid"))
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid != 0)
        .ok_or("读取 CodeAgent 后台进程身份失败")?;
    // SAFETY: PID 由持锁的本应用实例发布；只申请同步权限，不修改或终止进程。
    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    let process = if raw.is_null() {
        if std::io::Error::last_os_error().raw_os_error()
            != Some(ERROR_INVALID_PARAMETER.cast_signed())
        {
            return Err("打开 CodeAgent 后台退出等待句柄失败".into());
        }
        None // 在读取 PID 后已经自然退出。
    } else {
        // SAFETY: 本函数拥有 OpenProcess 返回的唯一有效句柄。
        Some(unsafe { OwnedHandle::from_raw_handle(raw.cast()) })
    };
    std::fs::write(root.join("codeagent-service.stop"), b"stop")
        .map_err(|_| "发送 CodeAgent 维护请求失败")?;
    if let Some(process) = process {
        // SAFETY: OwnedHandle 保证句柄在整个有界等待期间有效。
        if unsafe { WaitForSingleObject(process.as_raw_handle().cast(), 15_000) } != WAIT_OBJECT_0 {
            return Err("等待 CodeAgent 后台自然退出失败".into());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while is_running()? {
        if Instant::now() >= deadline {
            return Err("CodeAgent 后台单实例锁尚未释放".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    remove_stop_request()
}

pub fn status() -> Result<Status> {
    let connection = db()?;
    let mut state = read_status(&connection)?;
    if state.interval_seconds == 0 {
        state.interval_seconds = read_interval(&connection)?;
    }
    Ok(state)
}

/// 同一连接读取后台有效状态和已保存间隔；有效间隔非零时不覆盖它。
pub fn status_with_saved_interval() -> Result<(Status, u64)> {
    let connection = db()?;
    let mut state = read_status(&connection)?;
    let saved = read_interval(&connection)?;
    if state.interval_seconds == 0 {
        state.interval_seconds = saved;
    }
    Ok((state, saved))
}

fn read_status(connection: &rusqlite::Connection) -> Result<Status> {
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM app_settings WHERE key='codeagent_status'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| "读取 CodeAgent 状态失败")?;
    let mut state = match value {
        Some(value) => {
            serde_json::from_str::<Status>(&value).map_err(|_| "CodeAgent 状态记录无效")?
        }
        None => Status::default(),
    };
    state.running = is_running()?;
    if !state.running {
        state.pid = None;
    }
    Ok(state)
}
fn publish(state: &Status) -> Result<()> {
    let value = serde_json::to_string(state).map_err(|_| "编码 CodeAgent 状态失败")?;
    service_db()?.execute(
        "INSERT INTO app_settings(key,value) VALUES('codeagent_status',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [value],
    ).map_err(|_| "保存 CodeAgent 状态失败")?;
    Ok(())
}

/// CA-05：调用方只启动并观察，不拥有服务；单实例的最终裁决在服务持有的文件锁。
pub fn ensure_started() -> Result<Status> {
    ensure_started_with(std::env::current_exe().map_err(|_| "定位 CodeAgent 后台程序失败")?)
}

fn ensure_started_with(executable: PathBuf) -> Result<Status> {
    // 状态存储不可用不能阻止首次启动；先只观察独占锁，再读取安全状态。
    let running = is_running()?;
    if running {
        let current = status()?;
        if current.pid.is_some() {
            return Ok(current);
        }
    }
    if !running {
        use std::os::windows::process::CommandExt;
        Command::new(executable)
            .arg("--codeagent-service")
            .creation_flags(0x0800_0000 | 0x0000_0008) // CREATE_NO_WINDOW | DETACHED_PROCESS
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "启动 CodeAgent 独立后台失败")?;
        tracing::info!(
            component = "codeagent",
            event = "codeagent_service_spawned",
            "CodeAgent 独立后台已启动"
        );
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let current = status()?;
        if current.running && current.pid.is_some() {
            return Ok(current);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("CodeAgent 后台尚未就绪，请重新打开页面".into())
}

/// test-hooks 的真实入口仍使用相同的启动路径；只允许调用方指定受检主程序。
#[cfg(feature = "test-hooks")]
pub fn ensure_started_for_test(executable: PathBuf) -> Result<Status> {
    if test_isolation()?.is_none() {
        return Err("CodeAgent 测试必须配置隔离凭据、注册表及状态目录".into());
    }
    ensure_started_with(executable)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(feature = "test-hooks")]
fn test_isolation() -> Result<Option<(PathBuf, String)>> {
    let source = std::env::var_os("JCHTOOLS_TEST_CODEAGENT_CREDENTIALS");
    let registry = std::env::var_os("JCHTOOLS_TEST_CODEAGENT_REGISTRY");
    if source.is_none() && registry.is_none() {
        return Ok(None);
    }
    // 非 Unicode 值也属于已配置隔离；转换失败必须拒绝，不能当作变量未设置。
    let registry = registry.and_then(|value| value.into_string().ok());
    let source = source.map(PathBuf::from).filter(|path| path.is_absolute());
    let root = std::env::var_os("JCHTOOLS_TEST_STATE_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    match (source, registry, root) {
        (Some(source), Some(registry), Some(_))
            if registry.starts_with("Software\\JchTools\\Tests\\CodeAgent\\")
                && !registry.contains('\0') =>
        {
            Ok(Some((source, registry)))
        }
        _ => Err("CodeAgent 测试隔离配置不完整，拒绝访问真实凭据或注册表".into()),
    }
}

fn locations() -> Result<(PathBuf, String)> {
    #[cfg(feature = "test-hooks")]
    if let Some(locations) = test_isolation()? {
        return Ok(locations);
    }
    let profile = std::env::var_os("USERPROFILE").ok_or("无法定位当前用户凭据目录")?;
    Ok((
        PathBuf::from(profile)
            .join(".cac")
            .join(".credentials.json"),
        "Environment".into(),
    ))
}

fn decode_base64(value: &str) -> std::result::Result<Vec<u8>, CheckResult> {
    let encoded = wide(value);
    let count = u32::try_from(encoded.len() - 1).map_err(|_| CheckResult::DecodeFailed)?;
    let mut size = 0;
    // SAFETY: 输入及长度对应有效 UTF-16 缓冲区；第一次只查询所需大小。
    if unsafe {
        CryptStringToBinaryW(
            encoded.as_ptr(),
            count,
            CRYPT_STRING_BASE64,
            std::ptr::null_mut(),
            &raw mut size,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
        || size == 0
    {
        return Err(CheckResult::DecodeFailed);
    }
    let mut decoded = vec![0; size as usize];
    // SAFETY: 输出缓冲区按 API 查询结果分配，size 提供真实容量。
    if unsafe {
        CryptStringToBinaryW(
            encoded.as_ptr(),
            count,
            CRYPT_STRING_BASE64,
            decoded.as_mut_ptr(),
            &raw mut size,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(CheckResult::DecodeFailed);
    }
    decoded.truncate(size as usize);
    Ok(decoded)
}

/// CA-02：只使用 CurrentUser、NULL 熵及禁止交互的 DPAPI，错误不含密文或明文。
pub fn decrypt_token(ciphertext: &[u8]) -> std::result::Result<String, CheckResult> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(ciphertext.len()).map_err(|_| CheckResult::DecryptFailed)?,
        pbData: ciphertext.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: 输入在调用期间有效；DPAPI 分配输出，稍后由 LocalFree 释放。
    if unsafe {
        CryptUnprotectData(
            &raw const input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut output,
        )
    } == 0
    {
        return Err(CheckResult::DecryptFailed);
    }
    if output.pbData.is_null() {
        return Err(CheckResult::DecryptFailed);
    }
    // SAFETY: DPAPI 输出是 cbData 字节的有效分配；释放前清除本机明文缓冲区。
    let bytes = unsafe { std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize) };
    let token = std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| CheckResult::InvalidUtf8);
    for byte in bytes {
        // SAFETY: byte 是上述有效输出缓冲区中独占借用的可写字节。
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    // SAFETY: 该指针由 DPAPI 分配，已停止访问且只释放一次。
    unsafe { LocalFree(output.pbData.cast()) };
    token
}

struct RegistryKey(HKEY);
impl Drop for RegistryKey {
    fn drop(&mut self) {
        // SAFETY: 句柄由 RegCreateKeyExW 成功打开或创建且只关闭一次。
        unsafe { RegCloseKey(self.0) };
    }
}

fn update_registry(subkey: &str, token: &str) -> std::result::Result<CheckResult, CheckResult> {
    if token.is_empty() || token.contains('\0') {
        return Err(CheckResult::RegistryFailed);
    }
    let subkey = wide(subkey);
    let name = wide(VALUE_NAME);
    let value = wide(token);
    let mut raw = std::ptr::null_mut();
    // SAFETY: 凭据已成功解密；仅打开或创建当前用户目标键，生产路径固定为 Environment。
    if unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            std::ptr::null(),
            &raw mut raw,
            std::ptr::null_mut(),
        )
    } != ERROR_SUCCESS
    {
        return Err(CheckResult::RegistryFailed);
    }
    let key = RegistryKey(raw);
    let mut kind = 0;
    let mut size = 0;
    // SAFETY: 句柄有效，名称以 NUL 结尾；本次只写入类型与所需字节数。
    let queried = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            std::ptr::null(),
            &raw mut kind,
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    if queried != ERROR_SUCCESS && queried != ERROR_FILE_NOT_FOUND {
        return Err(CheckResult::RegistryFailed);
    }
    if queried == ERROR_SUCCESS && kind == REG_SZ && size as usize == value.len() * 2 {
        let mut current = vec![0u16; value.len()];
        // SAFETY: 缓冲区按查询所得字节数分配，名称和句柄在调用期间有效。
        if unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                std::ptr::null(),
                &raw mut kind,
                current.as_mut_ptr().cast(),
                &raw mut size,
            )
        } != ERROR_SUCCESS
        {
            return Err(CheckResult::RegistryFailed);
        }
        if kind == REG_SZ && current == value {
            return Ok(CheckResult::Unchanged);
        }
    }
    let size = u32::try_from(value.len() * 2).map_err(|_| CheckResult::RegistryFailed)?;
    // SAFETY: UTF-16 REG_SZ 包含终止 NUL，size 为真实字节数。
    if unsafe { RegSetValueExW(key.0, name.as_ptr(), 0, REG_SZ, value.as_ptr().cast(), size) }
        != ERROR_SUCCESS
    {
        return Err(CheckResult::RegistryFailed);
    }
    Ok(CheckResult::Updated)
}

/// CA-03 / CA-07：全部读取与解密完成后才可能写注册表；失败不触碰已有值。
pub fn check_once() -> CheckResult {
    let span = jchtools_foundation::logging::operation_span("codeagent", "check");
    let _entered = span.enter();
    tracing::info!(event = "codeagent_check_started", "CodeAgent 检查开始");
    let result = (|| {
        let (source, registry) = locations().map_err(|_| CheckResult::ReadFailed)?;
        let bytes = std::fs::read(source).map_err(|_| CheckResult::ReadFailed)?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| CheckResult::DecodeFailed)?;
        let encoded = value
            .get("idaasOAuth")
            .and_then(|oauth| oauth.get("xAuthToken"))
            .and_then(serde_json::Value::as_str)
            .ok_or(CheckResult::DecodeFailed)?;
        let token = decrypt_token(&decode_base64(encoded)?)?;
        update_registry(&registry, &token)
    })()
    .unwrap_or_else(|result| result);
    if result.failed() {
        tracing::warn!(event = "codeagent_check_failed", result = ?result, "CodeAgent 检查失败，保留已有 Token");
    } else {
        tracing::info!(event = "codeagent_check_completed", result = ?result, "CodeAgent 检查完成");
    }
    result
}

/// CA-04 / CA-05：后台拥有锁到退出，GUI 生命周期与本循环无关；维护期间不等待重启。
pub fn serve() -> Result<()> {
    #[cfg(feature = "test-hooks")]
    test_isolation()?;
    let control = control_lock_file()?;
    let control = match try_acquire_lock(
        control,
        "codeagent_service_maintenance_active",
        "CodeAgent 正在维护，后台不启动",
        "取得 CodeAgent 维护锁失败",
    ) {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(()),
        Err(error) => return Err(error),
    };
    let lock = lock_file()?;
    let lock = match try_acquire_lock(
        lock,
        "codeagent_service_already_running",
        "CodeAgent 后台已在运行",
        "取得 CodeAgent 后台单实例锁失败",
    ) {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(()),
        Err(error) => return Err(error),
    };
    remove_stop_request()?;
    let root = jchtools_foundation::xberg_settings::state_dir()?;
    let stop_request = root.join("codeagent-service.stop");
    std::fs::write(
        root.join("codeagent-service.pid"),
        std::process::id().to_string(),
    )
    .map_err(|_| "发布 CodeAgent 后台进程身份失败")?;
    // 锁保持到服务进程真正结束（包括 main 的日志刷盘），不在 serve 返回时提前释放。
    static PROCESS_LOCK: std::sync::OnceLock<File> = std::sync::OnceLock::new();
    PROCESS_LOCK
        .set(lock)
        .map_err(|_| "CodeAgent 后台进程锁已登记")?;
    drop(control);
    let mut state = Status {
        running: true,
        pid: Some(std::process::id()),
        interval_seconds: load_service_interval().unwrap_or_else(|_| {
            // 首次读取也可能遇到 SQLite 暂时不可用；持锁按默认间隔开始，恢复后热应用。
            tracing::warn!(
                component = "codeagent",
                event = "codeagent_interval_read_failed",
                "读取初始 CodeAgent 间隔失败，暂时使用默认间隔"
            );
            60
        }),
        ..Status::default()
    };
    let mut publish_pending = publish(&state).is_err();
    if publish_pending {
        tracing::warn!(
            component = "codeagent",
            event = "codeagent_status_publish_failed",
            "发布初始 CodeAgent 状态失败，后台继续检查"
        );
    }
    tracing::info!(
        component = "codeagent",
        event = "codeagent_service_started",
        "CodeAgent 后台开始运行"
    );
    let mut last_check = None;
    loop {
        let stop_requested = stop_request.exists();
        if stop_requested {
            state.running = false;
            state.pid = None;
            // 维护退出绝不等待 SQLite 可写；锁与进程退出才是停止事实。
            let _ = publish(&state);
            tracing::info!(
                component = "codeagent",
                event = "codeagent_service_stopped",
                "CodeAgent 后台已按维护请求停止"
            );
            return Ok(());
        }
        match load_service_interval() {
            Ok(interval) => {
                publish_pending |= state.interval_seconds != interval;
                state.interval_seconds = interval;
            }
            Err(_) => {
                // 共享 SQLite 暂时不可用不能终止独立后台，保持上次有效间隔。
                tracing::warn!(
                    component = "codeagent",
                    event = "codeagent_interval_read_failed",
                    "读取 CodeAgent 间隔失败，继续使用上次有效间隔"
                );
            }
        }
        let interval = state.interval_seconds;
        if last_check.is_none_or(|last: Instant| last.elapsed().as_secs() >= interval) {
            state.result = check_once();
            state.last_check = Some(chrono::Local::now().to_rfc3339());
            last_check = Some(Instant::now());
            publish_pending = true;
        }
        // 状态写入失败不取消凭据维护；正常轮询恢复后发布最新安全状态。
        if publish_pending {
            if publish(&state).is_ok() {
                publish_pending = false;
            } else {
                tracing::warn!(
                    component = "codeagent",
                    event = "codeagent_status_publish_failed",
                    "发布 CodeAgent 状态失败，后台继续检查"
                );
            }
        }
        // 每秒读取间隔即可热应用；不重复读取或解密凭据。
        std::thread::sleep(Duration::from_secs(1));
    }
}
