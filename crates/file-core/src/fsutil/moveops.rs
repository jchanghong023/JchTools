use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::time::UNIX_EPOCH;

pub(super) fn log_filesystem_result<T>(
    operation: &'static str,
    started: std::time::Instant,
    result: &Result<T>,
) {
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    match result {
        Ok(_) => tracing::debug!(
            event = "filesystem_operation_completed",
            operation,
            elapsed_ms,
            "文件系统操作完成"
        ),
        Err(error) => {
            let io = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<std::io::Error>());
            tracing::error!(
                event = "filesystem_operation_failed",
                operation,
                elapsed_ms,
                error_type = if io.is_some() {
                    "io"
                } else {
                    "filesystem_boundary"
                },
                error_code = io.and_then(std::io::Error::raw_os_error),
                "文件系统操作失败"
            );
        }
    }
}

/// User-file moves are strictly no-replace and never copy data across volumes.
pub fn rename_noreplace(source: &Path, target: &Path) -> Result<()> {
    let span = jchtools_foundation::logging::operation_span("fsutil", "rename_noreplace");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::debug!(event = "filesystem_rename_started", "不覆盖改名开始");
    let result: Result<()> = (|| {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
            let s: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
            let t: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: s/t 都是以 NUL 结尾的 UTF-16 缓冲区；MoveFileExW 只在调用期间读取这两个指针。
            if unsafe { MoveFileExW(s.as_ptr(), t.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("移动失败（不会覆盖或跨卷复制）");
            }
            Ok(())
        }
        #[cfg(target_os = "linux")]
        {
            use std::{ffi::CString, os::unix::ffi::OsStrExt};
            let s = CString::new(source.as_os_str().as_bytes())?;
            let t = CString::new(target.as_os_str().as_bytes())?;
            // SAFETY: s/t 是合法 CString（路径不含 NUL，构造失败会提前返回）；
            // renameat2 按 libc 约定传 AT_FDCWD + RENAME_NOREPLACE，内核侧不保留指针。
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    libc::AT_FDCWD,
                    s.as_ptr(),
                    libc::AT_FDCWD,
                    t.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if rc == 0 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            // 老内核/受限 seccomp 可能没有 renameat2：回退到与其它 Unix 相同的硬链接路径，
            // 目标已存在时 hard_link 失败，仍保持不覆盖语义。
            if err.raw_os_error() != Some(libc::ENOSYS) {
                return Err(err).context("不覆盖移动失败");
            }
            fs::hard_link(source, target)?;
            if let Err(e) = fs::remove_file(source) {
                let _ = fs::remove_file(target);
                return Err(e.into());
            }
            Ok(())
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            // Safe file-only fallback. No existing target can be overwritten.
            fs::hard_link(source, target)?;
            if let Err(e) = fs::remove_file(source) {
                let _ = fs::remove_file(target);
                return Err(e.into());
            }
            Ok(())
        }
    })();
    log_filesystem_result("rename", started, &result);
    result
}
/// S-01：不覆盖复制（跨卷移动的落盘核心）。目标已存在时按 `AlreadyExists`
/// 失败；只有 `create_new` 成功后才拥有目标，后续失败只清理这个新建副本。
pub(super) fn copy_noreplace(source: &Path, target: &Path) -> std::io::Result<()> {
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    let copied = std::io::copy(&mut input, &mut output).and_then(|_| output.sync_all());
    // 即使复制失败也先关闭输出；不能从错误种类推断目标是否由本次创建。
    let result = copied.and(close_file(output));
    if result.is_err() {
        if let Err(cleanup) = fs::remove_file(target) {
            tracing::warn!(
                event = "filesystem_move_cleanup_failed",
                stage = "copy_rollback",
                error_code = cleanup.raw_os_error(),
                error_type = "io",
                "跨卷失败副本清理失败"
            );
        }
    }
    result
}

#[cfg(windows)]
fn close_file(file: File) -> std::io::Result<()> {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::Foundation::CloseHandle;

    let handle = file.into_raw_handle();
    // SAFETY: handle 由 File 转移所有权，本函数只关闭一次且不再访问。
    if unsafe { CloseHandle(handle) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn close_file(file: File) -> std::io::Result<()> {
    drop(file);
    Ok(())
}
/// FILETIME 换算核心（100ns 单位、1601 纪元）：接受相对 UNIX 纪元的偏移
/// （Ok = 1970 之后，Err = 1970 之前的时长）。1601-1970 的负偏移受检折算；
/// 早于 1601-01-01（FILETIME 合法下界）下溢返回 Err 明确报错，不得钳制成 1970
/// 或其他静默值（S-01 忠实移动：时间写不回去就必须如实失败并保留源项）；
/// 超出 u64 上限的远未来值钳制到 u64::MAX（SetFileTime 会拒绝非法值）。
/// 独立成纯函数便于直接测试越界值——SystemTime 在 Windows 上无法表示早于
/// 1601 的时刻（checked_sub 返回 None），越界分支无法经 SystemTime 构造。
#[cfg(windows)]
pub(super) fn offset_to_filetime(
    offset: Result<std::time::Duration, std::time::Duration>,
) -> Result<windows_sys::Win32::Foundation::FILETIME> {
    use windows_sys::Win32::Foundation::FILETIME;
    const EPOCH_DELTA_100NS: u64 = 116_444_736_000_000_000;
    let units = match offset {
        Ok(delta) => {
            let total = u128::from(EPOCH_DELTA_100NS) + delta.as_nanos() / 100;
            u64::try_from(total).unwrap_or(u64::MAX)
        }
        Err(before) => u128::from(EPOCH_DELTA_100NS)
            .checked_sub(before.as_nanos() / 100)
            .and_then(|units| u64::try_from(units).ok())
            .context("文件时间早于 1601-01-01，超出 FILETIME 可表示范围")?,
    };
    Ok(FILETIME {
        dwLowDateTime: u32::try_from(units % (1u64 << 32)).unwrap_or(0),
        dwHighDateTime: u32::try_from(units >> 32).unwrap_or(0),
    })
}
/// 把 SystemTime 换算为 FILETIME；负偏移（1601-1970）按 [`offset_to_filetime`]
/// 受检折算，越界（早于 1601）返回 Err。
#[cfg(windows)]
fn systemtime_to_filetime(
    time: std::time::SystemTime,
) -> Result<windows_sys::Win32::Foundation::FILETIME> {
    offset_to_filetime(match time.duration_since(UNIX_EPOCH) {
        Ok(delta) => Ok(delta),
        Err(error) => Err(error.duration()),
    })
}
/// S-01：用户文件的最终移动入口。同卷走不覆盖改名（Windows 同卷改名天然保留创建时间，
/// 满足 S-01 忠实移动要求）；确因跨文件系统失败时按「不覆盖完整复制 → 设置创建/修改
/// 时间 → 删除源项」执行，复制、写时间或删除任一失败都保留源项并如实报错。
pub fn move_file_preserving_times(source: &Path, target: &Path) -> Result<()> {
    let span = jchtools_foundation::logging::operation_span("fsutil", "move_file");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::debug!(event = "filesystem_move_started", "保留时间移动开始");
    let result: Result<()> = (|| {
        match rename_noreplace(source, target) {
            Ok(()) => Ok(()),
            Err(error) => {
                let cross_volume = error
                    .root_cause()
                    .downcast_ref::<std::io::Error>()
                    .and_then(std::io::Error::raw_os_error)
                    .is_some_and(|code| {
                        // 17 = Windows ERROR_NOT_SAME_DEVICE；18 = POSIX EXDEV。
                        code == 17 || code == 18
                    });
                if !cross_volume {
                    return Err(error);
                }
                tracing::warn!(
                    event = "filesystem_move_fallback",
                    stage = "cross_volume_copy",
                    error_code = error
                        .chain()
                        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
                        .and_then(std::io::Error::raw_os_error),
                    "跨卷改名不可用，按既有策略复制并保留时间"
                );
                tracing::debug!(
                    event = "filesystem_move_stage_started",
                    stage = "source_metadata",
                    "跨卷移动读取源文件时间信息开始"
                );
                let metadata =
                    fs::symlink_metadata(source).context("无法读取跨卷移动源文件属性")?;
                #[cfg(windows)]
                let created = Some(
                    metadata
                        .created()
                        .context("跨卷移动无法读取源文件创建时间")?,
                );
                #[cfg(not(windows))]
                let created = metadata.created().ok();
                let modified = Some(
                    metadata
                        .modified()
                        .context("跨卷移动无法读取源文件修改时间")?,
                );
                // S-01：复制核心在 create_new 成功后才取得副本清理所有权；
                // 源打开或目标创建失败不能导致调用方删除既有目标。
                tracing::debug!(
                    event = "filesystem_move_stage_started",
                    stage = "copy",
                    "跨卷副本写入开始"
                );
                if let Err(error) = copy_noreplace(source, target) {
                    let context = if error.kind() == std::io::ErrorKind::AlreadyExists {
                        format!("跨卷移动目标已存在，不覆盖既有文件（{}）", target.display())
                    } else {
                        "跨卷复制失败，源文件已保留".to_string()
                    };
                    return Err(anyhow::Error::new(error).context(context));
                }
                tracing::debug!(
                    event = "filesystem_move_stage_started",
                    stage = "restore_times",
                    "跨卷副本时间恢复开始"
                );
                if let Err(error) = set_created_and_modified(target, created, modified) {
                    if let Err(cleanup) = fs::remove_file(target) {
                        tracing::warn!(
                            event = "filesystem_move_cleanup_failed",
                            stage = "times_rollback",
                            error_code = cleanup.raw_os_error(),
                            error_type = "io",
                            "跨卷失败副本清理失败"
                        );
                    }
                    return Err(error.context("跨卷副本时间设置失败，已保留源文件与副本状态"));
                }
                tracing::debug!(
                    event = "filesystem_move_stage_started",
                    stage = "source_delete",
                    "跨卷源文件删除开始"
                );
                fs::remove_file(source)
                    .with_context(|| format!("源删除失败；两份均保留：{}", source.display()))?;
                Ok(())
            }
        }
    })();
    log_filesystem_result("move", started, &result);
    result
}
/// 把创建/修改时间写回文件（S-01 跨卷复制后必须恢复创建时间，保证 C-21 幂等）。
fn set_created_and_modified(
    path: &Path,
    created: Option<std::time::SystemTime>,
    modified: Option<std::time::SystemTime>,
) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, SetFileTime, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING,
        };
        let created_ft = created.map(systemtime_to_filetime).transpose()?;
        let modified_ft = modified.map(systemtime_to_filetime).transpose()?;
        let path16: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: path16 是 NUL 结尾的 UTF-16 缓冲区；句柄在本函数内关闭，指针不出作用域。
        let handle = unsafe {
            CreateFileW(
                path16.as_ptr(),
                FILE_WRITE_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error()).context("打开文件以写回时间失败");
        }
        let creation_ptr = created_ft
            .as_ref()
            .map_or(std::ptr::null(), std::ptr::from_ref::<FILETIME>);
        let modified_ptr = modified_ft
            .as_ref()
            .map_or(std::ptr::null(), std::ptr::from_ref::<FILETIME>);
        // SAFETY: handle 有效；两个指针指向本函数栈上的 FILETIME 或为 NULL（表示不修改）。
        let ok = unsafe { SetFileTime(handle, creation_ptr, std::ptr::null(), modified_ptr) };
        let set_error = if ok == 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };
        // SAFETY: 关闭本函数打开的句柄。
        let close_ok = unsafe { CloseHandle(handle) };
        if let Some(error) = set_error {
            return Err(error).context("写回创建/修改时间失败");
        }
        if close_ok == 0 {
            return Err(std::io::Error::last_os_error()).context("关闭文件时间句柄失败");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let file = fs::OpenOptions::new().write(true).open(path)?;
        if let Some(modified) = modified {
            file.set_modified(modified)?;
        }
        Ok(())
    }
}
/// 设置文件/目录的创建时间（S-01 跨卷复制写回创建时间的同一底层能力；
/// 测试用它伪造创建日期）。
pub fn set_created_time(path: &Path, created: std::time::SystemTime) -> Result<()> {
    set_created_and_modified(path, Some(created), None)
}
