use super::fold::is_link;
use crate::model::Snapshot;
use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::path::Path;
#[cfg(unix)]
use std::time::UNIX_EPOCH;

pub fn snapshot(path: &Path) -> Result<Snapshot> {
    let metadata = fs::symlink_metadata(path)?;
    snapshot_with(path, &metadata)
}
/// 与 [`snapshot`] 相同，复用调用方已取得的元数据检查普通文件与链接边界。
/// Windows 同一次句柄查询取得大小、时间、标识与链接数，避免硬链接目录项的陈旧时间；
/// Unix 继续复用目录枚举元数据，不增加扫描或元数据调用。
pub fn snapshot_with(path: &Path, metadata: &fs::Metadata) -> Result<Snapshot> {
    if is_link(metadata) || !metadata.is_file() {
        bail!("不是普通文件：{}", path.display());
    }
    #[cfg(unix)]
    let (size, modified_ns, created_ns, identity, links) = {
        use std::os::unix::fs::MetadataExt;
        let modified_ns = match metadata.modified()?.duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_nanos()).context("文件时间超出范围")?,
            Err(e) => -i64::try_from(e.duration().as_nanos()).context("文件时间超出范围")?,
        };
        // 创建时间随移动忠实保留；系统/文件系统不提供或超出范围时为 None。
        let created_ns =
            metadata
                .created()
                .ok()
                .and_then(|time| match time.duration_since(UNIX_EPOCH) {
                    Ok(delta) => i64::try_from(delta.as_nanos()).ok(),
                    Err(error) => i64::try_from(error.duration().as_nanos())
                        .ok()
                        .map(|value| -value),
                });
        (
            metadata.len(),
            modified_ns,
            created_ns,
            format!("{}:{}", metadata.dev(), metadata.ino()),
            metadata.nlink(),
        )
    };
    #[cfg(windows)]
    let (size, modified_ns, created_ns, identity, links) = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let file = File::open(path)?;
        // SAFETY: BY_HANDLE_FILE_INFORMATION 是纯 POD 结构，全零是合法初值（无必须非零的字段）。
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: file 是刚打开的有效句柄；调用只向 info 写入，不保留指针出本作用域。
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) } == 0 {
            return Err(std::io::Error::last_os_error()).context("读取 Windows 文件标识失败");
        }
        // FILETIME 为 1601 起算的 100ns 单位；在 i128 中先减纪元再乘，
        // 保留 1970 前的负时间，mtime 越界报错，ctime 越界仍按原口径为 None。
        const EPOCH_DELTA_100NS: i128 = 116_444_736_000_000_000;
        let [modified_ns, created_ns] = [info.ftLastWriteTime, info.ftCreationTime].map(|time| {
            let units = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
            i64::try_from((i128::from(units) - EPOCH_DELTA_100NS) * 100)
        });
        (
            (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
            modified_ns.context("文件时间超出范围")?,
            created_ns.ok(),
            format!(
                "{}:{}:{}",
                info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
            ),
            u64::from(info.nNumberOfLinks),
        )
    };
    Ok(Snapshot {
        size,
        modified_ns,
        created_ns,
        identity,
        links,
    })
}
