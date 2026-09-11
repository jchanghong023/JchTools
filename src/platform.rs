use crate::{config::DeleteMode, control::Control, fsutil, model::Snapshot};
use anyhow::{bail, Context, Result};
use std::{fs, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteResult { Kept, Recycled, Permanent }
#[derive(Debug)]
pub enum RecycleFailure { Cancelled, Failed(String) }
/// Injectable for tests: tests never need to touch the user's real Recycle Bin.
pub trait Recycler: Send + Sync {
    fn recycle(&self, path: &Path) -> std::result::Result<(), RecycleFailure>;
}
pub struct NativeRecycler;
impl Recycler for NativeRecycler {
    fn recycle(&self, path: &Path) -> std::result::Result<(), RecycleFailure> { native_recycle(path) }
}
#[cfg(windows)]
fn native_recycle(path: &Path) -> std::result::Result<(), RecycleFailure> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{core::PCWSTR, Win32::{System::Com::*, UI::Shell::*}};
    let perform = || -> windows::core::Result<()> {
        // Runs on the file-operation worker, not the UI thread. Balanced COM lifetime.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?; }
        struct ComGuard;
        impl Drop for ComGuard { fn drop(&mut self) { unsafe { CoUninitialize(); } } }
        let _guard = ComGuard;
        let mut text = path.as_os_str().to_string_lossy().into_owned();
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") { text = format!(r"\\{unc}"); }
        else if let Some(local) = text.strip_prefix(r"\\?\") { text = local.to_string(); }
        let wide: Vec<u16> = std::ffi::OsStr::new(&text).encode_wide().chain(Some(0)).collect();
        unsafe {
            let operation: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)?;
            operation.SetOperationFlags(FOF_NO_UI | FOF_NO_CONNECTED_ELEMENTS | FOFX_RECYCLEONDELETE | FOFX_EARLYFAILURE)?;
            let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)?;
            operation.DeleteItem(&item, None)?;
            operation.PerformOperations()?;
            if operation.GetAnyOperationsAborted()?.as_bool() {
                // Shell aborted without a specific error: conservatively treat as cancellation.
                // Never infer permanent-delete permission from an ambiguous abort status.
                return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x800704C7u32 as i32)));
            }
        }
        Ok(())
    };
    match perform() {
        Ok(()) => Ok(()),
        Err(error) if [0x800704C7u32,0x80270000,0x80004004].contains(&(error.code().0 as u32)) => Err(RecycleFailure::Cancelled),
        Err(error) => Err(RecycleFailure::Failed(error.to_string())),
    }
}
#[cfg(not(windows))]
fn native_recycle(path: &Path) -> std::result::Result<(), RecycleFailure> {
    trash::delete(path).map_err(|e| RecycleFailure::Failed(e.to_string()))
}
pub fn remove(
    path: &Path, expected: Option<&Snapshot>, mode: DeleteMode, fallback: bool,
    control: &Control, recycler: &dyn Recycler,
) -> Result<DeleteResult> {
    control.checkpoint()?;
    if mode == DeleteMode::Keep { return Ok(DeleteResult::Kept); }
    let meta = fs::symlink_metadata(path)?;
    if fsutil::is_link(&meta) { bail!("拒绝删除链接 / reparse point"); }
    if let Some(expected) = expected { fsutil::unchanged(path, expected)?; }
    if meta.is_dir() && fs::read_dir(path)?.next().is_some() { bail!("目录不是空目录，不会递归删除用户目录"); }
    if mode == DeleteMode::Recycle {
        match recycler.recycle(path) {
            Ok(()) => {
                if path.try_exists()? { bail!("回收站接口返回后文件仍存在，未判定删除成功"); }
                return Ok(DeleteResult::Recycled);
            }
            Err(RecycleFailure::Cancelled) => bail!("回收站操作被取消，不会降级为永久删除"),
            Err(RecycleFailure::Failed(reason)) => {
                control.check_cancelled()?;
                // A backend can report an error after moving an item. Never delete a new replacement.
                if !path.try_exists()? { return Ok(DeleteResult::Recycled); }
                if !fallback { bail!("回收失败，已保留文件：{reason}"); }
                if let Some(expected) = expected { fsutil::unchanged(path, expected)?; }
            }
        }
    }
    control.check_cancelled()?;
    if meta.is_dir() { fs::remove_dir(path).context("删除空目录失败")?; }
    else { fs::remove_file(path).context("永久删除失败（未自动提升权限或修改只读属性）")?; }
    Ok(DeleteResult::Permanent)
}
