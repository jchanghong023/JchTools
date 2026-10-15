use crate::{Category, ErrorCode};
#[cfg(not(windows))]
use crate::posix::{message, is_would_block};
#[cfg(not(windows))]
pub(crate) use crate::posix::get_last_error;
use crate::types::c_int;

use core::ptr;

/// System error category, suitable for all environments.
///
/// On UNIX system it is equivalent of [Posix](struct.PosixCategory.html)
///
/// On Windows it uses winapi error functions
pub static SYSTEM_CATEGORY: Category = Category {
    name: "OSError",
    message,
    equivalent,
    is_would_block,
};

fn equivalent(code: c_int, other: &ErrorCode) -> bool {
    ptr::eq(&SYSTEM_CATEGORY, other.category()) && code == other.raw_code()
}

#[cfg(windows)]
#[inline]
pub(crate) fn get_last_error() -> c_int {
    unsafe {
        GetLastError() as c_int
   }
}

#[cfg(windows)]
fn message(code: c_int, out: &mut crate::MessageBuf) -> &str {
    const FORMAT_MESSAGE_FROM_SYSTEM: crate::types::c_ulong = 0x00001000;
    const FORMAT_MESSAGE_IGNORE_INSERTS: crate::types::c_ulong = 0x00000200;
    const FMT_FLAGS: crate::types::c_ulong = FORMAT_MESSAGE_IGNORE_INSERTS | FORMAT_MESSAGE_FROM_SYSTEM;
    let mut buffer = [core::mem::MaybeUninit::<u16>::uninit(); crate::MESSAGE_BUF_SIZE * 2];

    message_with(
        code,
        out,
        &mut buffer,
        |buffer| {
            let len = unsafe {
                FormatMessageW(FMT_FLAGS, ptr::null(), code as crate::types::c_ulong, 0, buffer.as_mut_ptr() as *mut u16, buffer.len() as _, ptr::null_mut())
            };
            (len, if len == 0 { get_last_error() } else { 0 })
        },
    )
}

#[cfg(windows)]
fn message_with<'out>(
    code: c_int,
    out: &'out mut crate::MessageBuf,
    buff: &mut [core::mem::MaybeUninit<u16>],
    format: impl FnOnce(&mut [core::mem::MaybeUninit<u16>]) -> (u32, c_int),
) -> &'out str {
    use core::slice;

    const CP_UTF8: crate::types::c_ulong = 65001;
    let (len, error) = format(buff);
    if len == 0 {
        match error {
            // 失败时没有有效的初始化长度，不得转换该缓冲区。
            122 => return crate::utils::write_message_buf(out, "<Truncated>"),
            317 => return crate::posix::message(code, out),
            _ => return crate::utils::write_message_buf(out, crate::FAIL_ERROR_FORMAT),
        }
    }

    let res = unsafe {
        WideCharToMultiByte(CP_UTF8, 0,
                            buff.as_ptr() as _, len as _,
                            out.as_mut_ptr() as *mut i8, out.len() as _,
                            ptr::null(), ptr::null_mut())
    };
    match res {
        0 => match get_last_error() {
            122 => crate::utils::write_message_buf(out, "<Truncated>"),
            _ => crate::utils::write_fallback_code(out, code),
        }
        len => {
            let out = unsafe {
                slice::from_raw_parts(out.as_ptr() as *const u8, len as _)
            };
            let actual_len = if let Some(null_idx) = out.iter().position(|b| *b == b'\0' || *b == b'\r') {
                null_idx
            } else {
                len as usize
            };
            unsafe {
                core::str::from_utf8_unchecked(
                    slice::from_raw_parts(out.as_ptr(), actual_len)
                )
            }
        }
    }
}

#[cfg(windows)]
#[inline]
fn is_would_block(code: c_int) -> bool {
    code == 10035 || crate::posix::is_would_block(code)
}

#[cfg(windows)]
extern "system" {
    fn GetLastError() -> crate::types::c_ulong;
    fn FormatMessageW(dwFlags: crate::types::c_ulong, lpSource: *const u8, dwMessageId: crate::types::c_ulong, dwLanguageId: crate::types::c_ulong, lpBuffer: *mut u16, nSize: crate::types::c_ulong, Arguments: *mut i8) -> u32;
    fn WideCharToMultiByte(page: crate::types::c_uint, flags: crate::types::c_ulong, wide_str: *const u16, wide_str_len: c_int, multi_str: *mut i8, multi_str_len: c_int, default_char: *const i8, used_default_char: *mut bool) -> c_int;
}

// 本模块测试只针对 Windows 的 FormatMessageW/UTF-8 转换路径。
#[cfg(all(test, windows))]
mod tests {
    use core::mem::MaybeUninit;

    #[test]
    fn insufficient_format_buffer_never_exposes_failed_output() {
        // 全部为合法 UTF-16，且 256 字节输出足够容纳 128 字节哨兵。
        // 旧失败分支将通过真实 WideCharToMultiByte 泄露此正文，不触发 UB。
        let mut buffer = [MaybeUninit::new(u16::from(b'X')); 128];
        let mut out = [MaybeUninit::uninit(); crate::MESSAGE_BUF_SIZE];
        let text = super::message_with(5, &mut out, &mut buffer, |_| (0, 122));
        assert_eq!(text, "<Truncated>");
        assert!(!text.contains('X'));
    }

    #[test]
    fn successful_format_preserves_text_and_trims_cr() {
        let mut buffer = [
            MaybeUninit::new(u16::from(b'o')),
            MaybeUninit::new(u16::from(b'k')),
            MaybeUninit::new(u16::from(b'\r')),
            MaybeUninit::new(u16::from(b'\n')),
        ];
        let mut out = [MaybeUninit::uninit(); crate::MESSAGE_BUF_SIZE];
        let text = super::message_with(5, &mut out, &mut buffer, |_| (4, 0));
        assert_eq!(text, "ok");
    }

    #[test]
    fn native_conversion_overflow_keeps_existing_truncated_fallback() {
        // 128 个汉字的 UTF-8 超过 256 字节，真实转换必须报告缓冲不足。
        let mut buffer = [MaybeUninit::new('中' as u16); 128];
        let mut out = [MaybeUninit::uninit(); crate::MESSAGE_BUF_SIZE];
        let text = super::message_with(5, &mut out, &mut buffer, |_| (128, 0));
        assert_eq!(text, "<Truncated>");
    }
}
