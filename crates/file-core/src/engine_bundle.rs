//! 7-Zip 引擎定位：随包目录优先，其次使用内嵌副本（首次使用时释放到用户数据目录）。
//!
//! 候选顺序（E-02）：
//! 1. `<exe 目录>/resources/7zip/`：发布包与开发环境的显式引擎；
//! 2. 已释放的内嵌副本：`<用户数据目录>/JchTools/engine/<版本-哈希>/` 及其下的
//!    `release-*` 独立释放目录；
//!    固定目录本身被文件占用时，独立副本保存在同级 `<版本-哈希>-releases/` 下。
//! 3. 从 EXE 内嵌的压缩数据释放：固定位置只缺文件时补齐；固定位置被无效文件占用时，
//!    释放到新的独立自有位置（`release-*` 子目录），不删占用文件。
//!
//! 每个候选必须在适用清单声明的全部必需文件上通过「存在 + sha256 一致」校验后才可使用；
//! 内嵌构建以 EXE 内清单校验，未内嵌构建只接受随包 `manifest.json` 声明且通过哈希校验的引擎。
//! 缺失清单或校验失败的候选绝不执行，经受控诊断日志记录原因并继续下一顺位。
//! 无可信清单时没有可用引擎；全部候选不可用时按 E-05 口径报错停止，
//! 错误信息逐候选给出原因。

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_engine.rs"));
}
mod locate;
mod release;

pub use self::release::release;
pub use locate::{bundled_dir, embedded_available, embedded_dir, resolve_executable};

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
mod tests;
