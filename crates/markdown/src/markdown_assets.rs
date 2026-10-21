//! 内置文档/截图运行目录与显式可选音频安装。
//! 音频只使用当前运行目录随包收据锁定的发布资产；离线迁移按目标收据逐成员
//! 校验，保留旧来源和已有有效文件，最后经共享 broker 安全提交统一来源。

mod audio;
mod manifest;
mod runtime;
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
mod tests_audio;
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

#[cfg(any(test, feature = "test-hooks"))]
pub use audio::download_audio_runtime_with_downloader;
pub use audio::{download_audio_runtime, migrate_bundled_runtime};
pub use runtime::{
    ensure_document_notice, initialize, load_saved_runtime_dir, media_component_dir, readiness,
    runtime_dir, save_runtime_dir, validate_media, validate_runtime_dir, xberg_inference_root,
};

#[cfg(test)]
use jchtools_assets::asset_util::finalize_staging;

#[cfg(test)]
use {
    audio::{
        install_audio_members, migrate_bundled_runtime_with_activation, optional_audio_path,
        verify_component, AudioAsset, ComponentReceipt, FeatureComponent, ReleaseAsset,
        AUDIO_ARCHIVE, XBERG_ARCHIVE_PREFIX, XBERG_COMPONENT_MANIFEST, XBERG_LATEST_RELEASE_URL,
        XBERG_RELEASE_RECEIPT, XBERG_TARGET,
    },
    manifest::{load_manifest, parse_manifest, write_notice, AssetFile, MANIFEST, XBERG_TAG},
    runtime::readiness_inference_pack,
};
