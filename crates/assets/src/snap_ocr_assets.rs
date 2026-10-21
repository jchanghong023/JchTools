//! 截图 OCR（O 分区）的可选资产初始化与就绪检查。
//!
//! 资产清单（resources/snap-ocr-assets.json）编译进主程序：结果窗专用字体为
//! 固定版本、固定来源、固定 SHA-256（O-05/O-09）；识别用的模型、推理运行库与
//! `xberg.exe` 从应用级 SQLite 保存的共享目录读取，按截图场景固定清单校验。
//! 只在用户于图形界面主动初始化时联网下载（O-06/O-10），主程序包不携带这些重资产。
//! 初始化遵循 staging → 校验 → 原子落位：取消或失败删除本轮 staging，不覆盖
//! 已经校验通过的完整资产；重试时已验证资产直接复用，不重复下载。
//! 下载/校验/原子落位与推理组件包安装核心与转 Markdown 共用
//! [`crate::asset_util`]，两侧行为同源。
//! P-10：初始化、资产下载和发布元数据共用 operation span；每次 GET 的 request_id
//! 关联逐跳代理选择、重定向和直连回退。日志只记录安全 URL、分类码与计数，
//! 不记录响应正文、完整错误文本、路径内容或认证信息；UI 错误文案保持原样。

mod download;
mod install;
mod manifest;
mod metadata;
mod readiness;

pub use download::download_asset;
pub use install::initialize;
pub use metadata::fetch_release_json;
pub use readiness::{
    asset_root, pipe_name, readiness, worker_install_path, xberg_inference_ready,
    xberg_inference_root,
};

#[cfg(test)]
use crate::asset_util::finalize_staging;

#[cfg(test)]
use {
    download::{
        checked_download_total, download_asset_with, download_attempt, download_request,
        NetworkDownloader,
    },
    install::initialize_staged,
    manifest::{
        load_manifest, write_notice, SnapAsset, SnapAssetManifest, SnapInferencePack, SnapLicense,
        SnapWorker,
    },
    metadata::fetch_release_json_with_proxy,
    readiness::{
        local_asset_readiness, readiness_inference_pack, verify_bundled_worker, worker_readiness,
        worker_ready, worker_repair_message, xberg_layout_ready,
    },
};

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
mod tests_initialize;
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
mod tests_readiness;
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
mod tests_support;
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
mod tests_transport;
