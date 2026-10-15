use error_code::{ErrorCode, defs};

#[test]
fn check_would_block() {
    let mut error = ErrorCode::new_posix(defs::EAGAIN);
    assert!(error.is_would_block());
    error = ErrorCode::new_posix(defs::EWOULDBLOCK);
    assert!(error.is_would_block());

    error = ErrorCode::new_system(defs::EAGAIN);
    assert!(error.is_would_block());
    error = ErrorCode::new_system(defs::EWOULDBLOCK);
    assert!(error.is_would_block());

    #[cfg(windows)]
    {
        error = ErrorCode::new_system(10035);
        assert!(error.is_would_block());
    }
}

#[cfg(target_pointer_width = "64")]
#[test]
fn size_check_64bit() {
    //On 64bit we suffer from alignment, but Rust optimizes enums quite well so ErrorCode benefits
    //of this optimization, letting its padding to be consumed by Result
    assert_eq!(core::mem::size_of::<ErrorCode>(), 16);
    //This optimization is enabled in latest rust compiler
    //assert_eq!(mem::size_of::<Result<bool, ErrorCode>>(), 16);
}

#[test]
fn it_works() {
    let error = ErrorCode::new_posix(11);
    eprintln!("{:?}", error.to_string());
    eprintln!("{:?}", error);

    let error = ErrorCode::last_posix();
    eprintln!("{}", error);

    let error = ErrorCode::new_system(11);
    eprintln!("{:?}", error.to_string());

    let error = ErrorCode::last_system();
    eprintln!("{}", error);
}

#[test]
fn check_error_code_range() {
    for code in 0..=15999 {
        let error = ErrorCode::new_posix(code);
        eprintln!("{:?}", error.to_string());

        let error = ErrorCode::new_system(code);
        eprintln!("{:?}", error.to_string());

        if code == defs::EWOULDBLOCK || code == defs::EAGAIN {
            assert!(error.is_would_block());
        } else {
            #[cfg(windows)]
            if code == 10035 {
                assert!(error.is_would_block());
            } else {
                assert!(!error.is_would_block());
            }

            #[cfg(not(windows))]
            assert!(!error.is_would_block());
        }
    }
}

// Windows 的 std::io::Error 保存 Win32/Winsock 错误码，不是 CRT errno。
#[cfg(all(feature = "std", windows))]
#[test]
fn io_os_error_preserves_windows_system_category() {
    for code in [5, 10035] {
        let error = ErrorCode::from(std::io::Error::from_raw_os_error(code));
        assert_eq!(error, ErrorCode::new_system(code));
        assert_eq!(error.raw_code(), code);
        assert_eq!(error.to_string(), ErrorCode::new_system(code).to_string());
    }
    assert!(ErrorCode::from(std::io::Error::from_raw_os_error(10035)).is_would_block());
}

#[cfg(feature = "std")]
#[test]
fn io_error_without_os_code_keeps_existing_fallback() {
    let error = ErrorCode::from(std::io::Error::from(std::io::ErrorKind::Other));
    assert_eq!(error, ErrorCode::new_posix(-1));
}
