//! 平台宽度转换收口：全仓数值宽度转换只允许出现在本模块（与 Win32 FFI 边界），
//! 其余代码一律用 TryFrom/Into，避免静默截断。
//!
//! 依据：本项目只在 64 位 Windows 上构建与测试（合同 P-07，Windows x64），
//! u64 与 usize 同宽、usize 值域非负且小于 i64 上限，下列转换恒走 Ok 分支；
//! unwrap_or 只是假设性 32 位目标下的饱和兜底，不改变当前平台行为。

/// u64 → usize（64 位平台上双射；越界饱和到 usize::MAX）。
pub fn u64_as_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// usize → u64（任意平台上无损）。
pub fn usize_as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// usize → i64（64 位平台上无损；usize 值域非负，越界饱和到 i64::MAX）。
pub fn usize_as_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
