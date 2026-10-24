//! 资产校验：文件大小与 SHA-256 摘要核对。
//!
//! 截图 OCR 与转 Markdown 的资产就绪检查、推理组件包安装链共用同一实现：
//! `verify_file` 校验暂存/落位内容，`verify_file_with_cancel` 在大文件哈希
//! 期间按块响应取消；日志经 foundation 的 operation span（P-10），只记录
//! 分类码、错误类别与耗时。

use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) fn verify_file(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<(), String> {
    verify_file_inner(path, expected_size, expected_sha256, None)
}

/// 与 [`verify_file`] 相同，但在大文件哈希期间按块响应取消。
pub fn verify_file_with_cancel(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
    cancel: &AtomicBool,
) -> Result<(), String> {
    verify_file_inner(path, expected_size, expected_sha256, Some(cancel))
}

fn verify_file_inner(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    let span = jchtools_foundation::logging::operation_span("asset_validation", "verify_file");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::debug!(
        event = "asset_verify_started",
        expected_size,
        "资产大小与摘要校验开始"
    );
    let metadata = fs::metadata(path).map_err(|error| {
        tracing::info!(event = "asset_verify_failed", stage = "metadata", error_type = ?error.kind(), error_code = ?error.raw_os_error(), elapsed_ms = jchtools_foundation::logging::elapsed_ms(started), "资产文件元数据不可用");
        error.to_string()
    })?;
    if !metadata.is_file() {
        tracing::warn!(
            event = "asset_verify_failed",
            stage = "file_type",
            error_type = "not_regular_file",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "资产不是普通文件"
        );
        return Err("不是普通文件".to_string());
    }
    if metadata.len() != expected_size {
        tracing::warn!(
            event = "asset_verify_failed",
            stage = "size",
            error_type = "size_mismatch",
            actual_size = metadata.len(),
            expected_size,
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "资产长度不符合清单"
        );
        return Err(format!("大小 {}，预期 {expected_size}", metadata.len()));
    }
    let actual = sha256_file_inner(path, cancel).map_err(|error| {
        if error.kind() == io::ErrorKind::Interrupted {
            tracing::info!(event = "asset_verify_cancelled", stage = "hash", elapsed_ms = jchtools_foundation::logging::elapsed_ms(started), "资产摘要校验已取消");
            "用户已取消初始化".to_string()
        } else {
            tracing::warn!(event = "asset_verify_failed", stage = "hash", error_type = ?error.kind(), error_code = ?error.raw_os_error(), elapsed_ms = jchtools_foundation::logging::elapsed_ms(started), "资产摘要读取失败");
            error.to_string()
        }
    })?;
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        tracing::warn!(
            event = "asset_verify_failed",
            stage = "hash_compare",
            error_type = "hash_mismatch",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "资产摘要不符合清单"
        );
        return Err(format!("SHA256 {actual}，预期 {expected_sha256}"));
    }
    tracing::debug!(
        event = "asset_verify_completed",
        verified_bytes = metadata.len(),
        elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
        "资产大小与摘要校验通过"
    );
    Ok(())
}

pub fn sha256_file_inner(path: &Path, cancel: Option<&AtomicBool>) -> io::Result<String> {
    let mut file = File::open(path).inspect_err(|error| {
        tracing::warn!(event = "asset_hash_io_failed", stage = "file_open", error_type = ?error.kind(), error_code = ?error.raw_os_error(), "资产哈希文件打开失败");
    })?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "用户已取消初始化",
            ));
        }
        let read = file.read(&mut buffer).inspect_err(|error| {
            tracing::warn!(event = "asset_hash_io_failed", stage = "file_read", error_type = ?error.kind(), error_code = ?error.raw_os_error(), "资产哈希文件读取失败");
        })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}
