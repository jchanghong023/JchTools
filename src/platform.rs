use crate::{config::DeleteMode, control::Control, fsutil, model::Snapshot};
use anyhow::{bail, Context, Result};
use std::{fs, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteResult {
    Kept,
    Recycled,
    /// 文件已离开原位置，但无法确认真的进入了回收站（shell 在超容量、
    /// 无回收站卷等情形下可能直接销毁并报成功）。按永久删除如实记账，不虚报「已回收」。
    RecycledUnverified,
    Permanent,
}
#[derive(Debug)]
pub enum RecycleFailure {
    Cancelled,
    Failed(String),
    /// 回收接口本身不可用（如调用线程 COM 已初始化为 MTA）：文件仍完好地留在原位，
    /// 与 Failed 的关键区别是绝不允许降级为永久删除（S-02：降级只留给「回收失败」）。
    Unavailable(String),
}
/// 界面/日志展示用：去掉 Windows 扩展路径前缀，避免用户看到 `\\?\D:\...`。
pub fn display_path_text(path: &str) -> String {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(local) = path.strip_prefix(r"\\?\") {
        local.to_string()
    } else {
        path.to_string()
    }
}
/// 界面展示用：把纳秒时间戳（可能为负）转成本地可读时间；不可表示时回落到原始数字。
pub fn display_time_text(ns: i64) -> String {
    let seconds = ns.div_euclid(1_000_000_000);
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| ns.to_string())
}
/// Injectable for tests: tests never need to touch the user's real Recycle Bin.
pub trait Recycler: Send + Sync {
    fn recycle(&self, path: &Path) -> std::result::Result<(), RecycleFailure>;
    /// 回收站条目计数（按卷）；返回 None 表示该后端/卷无法校验。
    fn bin_count(&self, volume: &Path) -> Option<i64> {
        let _ = volume;
        None
    }
}
pub struct NativeRecycler;
impl Recycler for NativeRecycler {
    fn recycle(&self, path: &Path) -> std::result::Result<(), RecycleFailure> {
        native_recycle(path)
    }
    fn bin_count(&self, volume: &Path) -> Option<i64> {
        bin_item_count(volume)
    }
}
/// 取路径所在卷的回收站查询根：本地盘为 `X:\`，UNC 为 `\\server\share\`；无法识别返回 None。
#[cfg(windows)]
fn volume_root(path: &Path) -> Option<std::path::PathBuf> {
    let text = path.to_str()?;
    let text = if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(local) = text.strip_prefix(r"\\?\") {
        local.to_string()
    } else {
        text.to_string()
    };
    if let Some(rest) = text.strip_prefix(r"\\") {
        let mut parts = rest.split('\\');
        let server = parts.next()?;
        let share = parts.next()?;
        if server.is_empty() || share.is_empty() {
            return None;
        }
        return Some(std::path::PathBuf::from(format!(r"\\{server}\{share}\")));
    }
    let mut chars = text.chars();
    let letter = chars.next()?;
    if !letter.is_ascii_alphabetic() || chars.next() != Some(':') {
        return None;
    }
    Some(std::path::PathBuf::from(format!("{letter}:\\")))
}
/// 当前回收站内的条目数；查询失败（无回收站的卷等）返回 None。
#[cfg(windows)]
fn bin_item_count(volume: &Path) -> Option<i64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::{SHQueryRecycleBinW, SHQUERYRBINFO};
    let mut info: SHQUERYRBINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHQUERYRBINFO>() as u32;
    let wide: Vec<u16> = volume.as_os_str().encode_wide().chain(Some(0)).collect();
    let hr = unsafe { SHQueryRecycleBinW(windows::core::PCWSTR(wide.as_ptr()), &mut info) };
    hr.ok().map(|_| info.i64NumItems)
}
#[cfg(not(windows))]
fn bin_item_count(_volume: &Path) -> Option<i64> {
    None
}
#[cfg(not(windows))]
fn volume_root(_path: &Path) -> Option<std::path::PathBuf> {
    None
}
#[cfg(windows)]
fn native_recycle(path: &Path) -> std::result::Result<(), RecycleFailure> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::PCWSTR,
        Win32::{System::Com::*, UI::Shell::*},
    };
    let perform = || -> windows::core::Result<()> {
        // Runs on the file-operation worker, not the UI thread. Balanced COM lifetime.
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        }
        struct ComGuard;
        impl Drop for ComGuard {
            fn drop(&mut self) {
                unsafe {
                    CoUninitialize();
                }
            }
        }
        let _guard = ComGuard;
        // 无损宽字符路径：避免 to_string_lossy 把未配对 UTF-16 代理项换成 U+FFFD 后误操作。
        let mut units: Vec<u16> = path.as_os_str().encode_wide().collect();
        // 剥掉 \\?\ 或 \\?\UNC\ 扩展前缀，保持与旧实现相同的解析名形态。
        const Q: &[u16] = &[0x5c, 0x5c, 0x3f, 0x5c]; // \\?\
        const UNC: &[u16] = &[0x5c, 0x5c, 0x3f, 0x5c, 0x55, 0x4e, 0x43, 0x5c]; // \\?\UNC\
        if units.starts_with(UNC) {
            let mut stripped = vec![0x5c, 0x5c];
            stripped.extend_from_slice(&units[UNC.len()..]);
            units = stripped;
        } else if units.starts_with(Q) {
            units.drain(..Q.len());
        }
        units.push(0);
        let wide = units;
        unsafe {
            let operation: IFileOperation =
                CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)?;
            operation.SetOperationFlags(
                FOF_NO_UI | FOF_NO_CONNECTED_ELEMENTS | FOFX_RECYCLEONDELETE | FOFX_EARLYFAILURE,
            )?;
            let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)?;
            operation.DeleteItem(&item, None)?;
            operation.PerformOperations()?;
            if operation.GetAnyOperationsAborted()?.as_bool() {
                // Shell aborted without a specific error: conservatively treat as cancellation.
                // Never infer permanent-delete permission from an ambiguous abort status.
                return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x800704C7u32 as i32,
                )));
            }
        }
        Ok(())
    };
    match perform() {
        Ok(()) => Ok(()),
        Err(error)
            if [0x800704C7u32, 0x80270000, 0x80004004].contains(&(error.code().0 as u32)) =>
        {
            Err(RecycleFailure::Cancelled)
        }
        // RPC_E_CHANGED_MODE：调用线程已被初始化为 MTA，STA 回收接口用不了。此时
        // 文件未受任何影响；按「接口不可用」失败且不降级，防止未来有人把删除搬到
        // GUI/OLE 线程时文件被静默永久删除（engine 在专属工作线程调用，正常不触发）。
        Err(error) if error.code().0 as u32 == 0x80010106 => {
            Err(RecycleFailure::Unavailable(error.to_string()))
        }
        Err(error) => Err(RecycleFailure::Failed(error.to_string())),
    }
}
#[cfg(not(windows))]
fn native_recycle(path: &Path) -> std::result::Result<(), RecycleFailure> {
    trash::delete(path).map_err(|e| RecycleFailure::Failed(e.to_string()))
}
/// 安全删除一个已规划的路径：拒绝链接/非空目录，校验扫描快照未变化；
/// 回收模式按「条目计数验证 → 未验证 → （可选）降级永久删除」的顺序处理，
/// 用户取消绝不降级。返回值区分「已回收 / 回收但未验证 / 永久删除 / 保留」。
pub fn remove(
    path: &Path,
    expected: Option<&Snapshot>,
    mode: DeleteMode,
    fallback: bool,
    control: &Control,
    recycler: &dyn Recycler,
) -> Result<DeleteResult> {
    control.checkpoint()?;
    if mode == DeleteMode::Keep {
        return Ok(DeleteResult::Kept);
    }
    let meta = fs::symlink_metadata(path)?;
    if fsutil::is_link(&meta) {
        bail!("拒绝删除链接 / reparse point");
    }
    if let Some(expected) = expected {
        fsutil::unchanged(path, expected)?;
    }
    if meta.is_dir() && fs::read_dir(path)?.next().is_some() {
        bail!("目录不是空目录，不会递归删除用户目录");
    }
    if mode == DeleteMode::Recycle {
        // 回收 API 对目录会整树入站：删除前再钉一次类型/链接/空目录，缩小 TOCTOU。
        control.check_cancelled()?;
        let meta2 = fs::symlink_metadata(path)?;
        if fsutil::is_link(&meta2) {
            bail!("拒绝删除链接 / reparse point");
        }
        if meta2.is_dir() && fs::read_dir(path)?.next().is_some() {
            bail!("目录不是空目录，不会递归删除用户目录");
        }
        let volume = volume_root(path);
        let before = volume.as_deref().and_then(|v| recycler.bin_count(v));
        match recycler.recycle(path) {
            Ok(()) => {
                if path.try_exists()? {
                    bail!("回收站接口返回后文件仍存在，未判定删除成功");
                }
                // 「文件消失」≠「进了回收站」：shell 在回收站超容量、无回收站卷等情形下
                // 可能直接销毁并报成功。只有回收站条目数确实增加时才记「已回收」，
                // 否则如实记为未验证，避免给用户可恢复的错觉。
                let after = volume.as_deref().and_then(|v| recycler.bin_count(v));
                let verified =
                    matches!((before, after), (Some(before), Some(after)) if after > before);
                return Ok(if verified {
                    DeleteResult::Recycled
                } else {
                    DeleteResult::RecycledUnverified
                });
            }
            Err(RecycleFailure::Cancelled) => bail!("回收站操作被取消，不会降级为永久删除"),
            Err(RecycleFailure::Unavailable(reason)) => {
                bail!("回收站接口在当前线程不可用，已保留文件：{reason}")
            }
            Err(RecycleFailure::Failed(reason)) => {
                control.check_cancelled()?;
                // A backend can report an error after moving an item. Never delete a new replacement.
                if !path.try_exists()? {
                    // 「报错但文件已消失」最常见于移动入站成功后才报错：能用计数确认入站的
                    // 仍记「已回收」，确认不了才按未验证处理，不夸大也不虚报。
                    let after = volume.as_deref().and_then(|v| recycler.bin_count(v));
                    let verified =
                        matches!((before, after), (Some(before), Some(after)) if after > before);
                    return Ok(if verified {
                        DeleteResult::Recycled
                    } else {
                        DeleteResult::RecycledUnverified
                    });
                }
                if !fallback {
                    bail!("回收失败，已保留文件：{reason}");
                }
                if let Some(expected) = expected {
                    fsutil::unchanged(path, expected)?;
                }
                // 降级永久删除前重检类型/链接/空目录，避免用陈旧 meta 选错 API 或误删非空树。
                let meta3 = fs::symlink_metadata(path)?;
                if fsutil::is_link(&meta3) {
                    bail!("拒绝删除链接 / reparse point");
                }
                if meta3.is_dir() && fs::read_dir(path)?.next().is_some() {
                    bail!("目录不是空目录，不会递归删除用户目录");
                }
                control.check_cancelled()?;
                if meta3.is_dir() {
                    fs::remove_dir(path).context("删除空目录失败")?;
                } else {
                    fs::remove_file(path)
                        .context("永久删除失败（未自动提升权限或修改只读属性）")?;
                }
                return Ok(DeleteResult::Permanent);
            }
        }
    }
    control.check_cancelled()?;
    // 永久删除前再确认一次（防检查后类型被替换）。
    let meta2 = fs::symlink_metadata(path)?;
    if fsutil::is_link(&meta2) {
        bail!("拒绝删除链接 / reparse point");
    }
    if meta2.is_dir() && fs::read_dir(path)?.next().is_some() {
        bail!("目录不是空目录，不会递归删除用户目录");
    }
    if meta2.is_dir() {
        fs::remove_dir(path).context("删除空目录失败")?;
    } else {
        fs::remove_file(path).context("永久删除失败（未自动提升权限或修改只读属性）")?;
    }
    Ok(DeleteResult::Permanent)
}

#[cfg(test)]
mod tests {
    use super::{display_path_text, display_time_text};

    #[test]
    fn display_path_text_strips_extended_prefix() {
        assert_eq!(display_path_text(r"\\?\D:\testzip"), r"D:\testzip");
        assert_eq!(
            display_path_text(r"\\?\UNC\server\share"),
            r"\\server\share"
        );
        assert_eq!(display_path_text(r"D:\plain"), r"D:\plain");
    }

    #[test]
    fn display_time_text_formats_epoch_and_falls_back_on_overflow() {
        // 任意本地时区下都应格式化为日期时间（含 - 与 :），而不是回落数字。
        let formatted = display_time_text(0);
        assert!(
            formatted.contains('-') && formatted.contains(':'),
            "0ns 应格式化为日期时间：{formatted}"
        );
        // i64::MAX 纳秒约 2262 年，仍在 chrono 范围内：格式化成功即可。
        assert!(display_time_text(i64::MAX).contains('-'));
        // 负时间戳（如 1601 Windows FILETIME 原点之前）不得 panic。
        let _ = display_time_text(i64::MIN);
    }

    // 覆盖 S-02
    #[cfg(windows)]
    #[test]
    fn volume_root_covers_local_and_unc() {
        use super::volume_root;
        use std::path::PathBuf;
        assert_eq!(
            volume_root(std::path::Path::new(r"\\?\D:\testzip\a.txt")),
            Some(PathBuf::from(r"D:\"))
        );
        assert_eq!(
            volume_root(std::path::Path::new(r"D:\testzip\a.txt")),
            Some(PathBuf::from(r"D:\"))
        );
        assert_eq!(
            volume_root(std::path::Path::new(r"\\server\share\x.txt")),
            Some(PathBuf::from(r"\\server\share\"))
        );
        assert_eq!(
            volume_root(std::path::Path::new(r"\\?\UNC\server\share\x.txt")),
            Some(PathBuf::from(r"\\server\share\"))
        );
        // UNC 缺 share、非盘符路径 → 无法定位回收站卷。
        assert_eq!(volume_root(std::path::Path::new(r"\\server")), None);
        assert_eq!(volume_root(std::path::Path::new("/unix-like/path")), None);
    }
}
