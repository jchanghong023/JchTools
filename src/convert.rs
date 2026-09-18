//! 平台宽度转换收口：全仓 `as` 数值转换只允许出现在本模块（与 Win32 FFI 边界），
//! 其余代码一律用 TryFrom/Into，避免静默截断。
//!
//! 依据：本项目只在 64 位平台构建与测试（Windows x64、Linux x86_64/arm64 CI），
//! u64 与 usize 同宽、usize 值域非负且小于 i64 上限，下列转换在本项目支持的
//! 全部平台上恒无损。若未来增加 32 位目标，必须删除本模块并逐点改为显式检查。

/// u64 → usize（64 位平台上双射）。
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub fn u64_as_usize(value: u64) -> usize {
    value as usize
}

/// usize → u64（任意平台上无损）。
#[allow(clippy::cast_possible_wrap)]
pub fn usize_as_u64(value: usize) -> u64 {
    value as u64
}

/// usize → i64（64 位平台上无损；usize 值域非负）。
#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
pub fn usize_as_i64(value: usize) -> i64 {
    value as i64
}
