//! Windows 用户会话身份：所有管道与单实例键共享当前进程主令牌，不读取环境变量。
use std::sync::LazyLock;

/// 返回进程生命周期内缓存的用户 SID 与令牌会话 ID；查询失败不得生成替代身份。
pub fn user_session_key() -> Result<&'static str, String> {
    static KEY: LazyLock<Result<String, String>> = LazyLock::new(query_user_session_key);
    KEY.as_deref().map_err(Clone::clone)
}

#[cfg(not(windows))]
fn query_user_session_key() -> Result<String, String> {
    Err("当前平台不支持 Windows 用户会话身份查询".into())
}

#[cfg(windows)]
fn query_user_session_key() -> Result<String, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenSessionId, TokenUser, SECURITY_MAX_SID_SIZE, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            // SAFETY: 本结构独占成功打开的令牌句柄。
            unsafe { CloseHandle(self.0) };
        }
    }
    struct SidText(*mut u16);
    impl Drop for SidText {
        fn drop(&mut self) {
            // SAFETY: 转换 API 的 LocalAlloc 结果由本结构唯一释放。
            unsafe { LocalFree(self.0.cast()) };
        }
    }
    fn failure(stage: &str) -> String {
        format!("{stage}：{}", std::io::Error::last_os_error())
    }

    let mut raw_token = std::ptr::null_mut();
    // SAFETY: 获取当前进程伪句柄，不转交所有权。
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: 当前进程伪句柄有效，输出指针可写；只查询进程主令牌，不取线程模拟身份。
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut raw_token) } == 0 {
        return Err(failure("读取 Windows 进程主令牌失败"));
    }
    let token = Token(raw_token);
    // TOKEN_USER 与最大合法 SID 的固定栈空间，usize 对齐满足 TOKEN_USER 指针要求。
    const WORDS: usize = (std::mem::size_of::<TOKEN_USER>() + SECURITY_MAX_SID_SIZE as usize)
        .div_ceil(std::mem::size_of::<usize>());
    let mut storage = [0_usize; WORDS];
    let buffer_size = u32::try_from(std::mem::size_of_val(&storage))
        .map_err(|_| "Windows 用户令牌缓冲区过大".to_string())?;
    let mut returned = 0;
    // SAFETY: token 有效，storage 正确对齐且容量为 buffer_size 字节。
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            storage.as_mut_ptr().cast(),
            buffer_size,
            &raw mut returned,
        )
    } == 0
    {
        return Err(failure("读取 Windows 用户 SID 失败"));
    }
    if (returned as usize) < std::mem::size_of::<TOKEN_USER>() || returned > buffer_size {
        return Err("Windows 用户令牌长度无效".into());
    }
    // SAFETY: TokenUser 成功返回的结构及其 SID 都属于仍存活的 storage。
    let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = std::ptr::null_mut();
    // SAFETY: 用户 SID 来自内核，输出为转换 API 分配的零终止字符串。
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut sid) } == 0 {
        return Err(failure("编码 Windows 用户 SID 失败"));
    }
    let sid = SidText(sid);
    let mut length = 0;
    // SAFETY: 转换成功保证 UTF-16 字符串以零终止，sid 持有分配直到读取结束。
    while unsafe { *sid.0.wrapping_add(length) } != 0 {
        length += 1;
    }
    // SAFETY: 前述扫描确定字符串范围，分配仍存活。
    let text = unsafe { std::slice::from_raw_parts(sid.0, length) };
    // 系统 SID 文本仅含 ASCII；一次分配同时预留会话后缀，不复制中间字符串。
    let mut key = String::with_capacity(length + 19);
    for value in text {
        let character = char::from_u32(u32::from(*value)).ok_or("Windows 用户 SID 编码无效")?;
        key.push(character);
    }
    let mut session = 0_u32;
    let session_size = u32::try_from(std::mem::size_of_val(&session))
        .map_err(|_| "Windows 会话缓冲区过大".to_string())?;
    // SAFETY: 使用同一主令牌，session 是正确大小且对齐的可写 DWORD。
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenSessionId,
            (&raw mut session).cast(),
            session_size,
            &raw mut returned,
        )
    } == 0
    {
        return Err(failure("读取 Windows 令牌会话失败"));
    }
    if returned != session_size {
        return Err("Windows 令牌会话长度无效".into());
    }
    use std::fmt::Write;
    write!(key, "-session-{session}").map_err(|_| "编码 Windows 会话身份失败")?;
    Ok(key)
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn review_f40_shared_key_is_cached_primary_token_identity() -> Result<(), String> {
        let expected = super::query_user_session_key()?;
        let first = super::user_session_key()?;
        let second = super::user_session_key()?;
        assert_eq!(first, expected);
        assert!(first.starts_with("S-1-"));
        assert!(first.contains("-session-"));
        assert!(std::ptr::eq(first.as_ptr(), second.as_ptr()));
        Ok(())
    }
}
