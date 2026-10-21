use jchtools_assets::asset_util::{
    state_dir_asset_root, valid_component_tag, validate_relative_path, InferenceManifest,
};
use serde::{Deserialize, Serialize};
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) const MANIFEST: &str = include_str!("../../../../resources/markdown-assets.json");
const DATA_DIRECTORY: &str = "markdown-assets";
pub(crate) const XBERG_TAG: &str = "v2026.10.19-234944-local.cad2c07bfe";

#[derive(Debug, Deserialize)]
pub(super) struct AssetManifest {
    schema_version: u32,
    pub(super) xberg: XbergManifest,
    /// Xberg 推理组件包（XB-10 双轨：媒体转录所需的 xberg.exe、模型与原生
    /// 运行库，来源为固定发布 zip）。清单未接入该条目时为 None，组件缺失
    /// 时如实报告未配置。
    pub(super) xberg_inference: Option<InferenceManifest>,
}

#[derive(Debug, Deserialize)]
pub(super) struct XbergManifest {
    tag: String,
    archive_url: String,
    archive_size_bytes: u64,
    pub(super) archive_sha256: String,
    pub(super) members: Vec<AssetFile>,
    pub(super) licenses: Vec<LicenseEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct AssetFile {
    pub(super) path: String,
    pub(super) size_bytes: u64,
    pub(super) sha256: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct LicenseEntry {
    component: String,
    license: String,
    source: String,
}

pub(super) fn load_manifest() -> Result<AssetManifest, String> {
    parse_manifest(MANIFEST)
}

/// 解析并校验资产清单文本：各段自身完整性 + 段间交叉一致性。拆出纯文本
/// 入参是为了让篡改清单（段间漂移）可用单测覆盖（内置常量无法在运行期改写）。
pub(super) fn parse_manifest(text: &str) -> Result<AssetManifest, String> {
    let manifest: AssetManifest =
        serde_json::from_str(text).map_err(|error| format!("转 Markdown 资产清单无效：{error}"))?;
    if manifest.schema_version != 1 {
        return Err(format!("不支持的资产清单版本：{}", manifest.schema_version));
    }
    if manifest.xberg.tag != XBERG_TAG {
        return Err(format!(
            "Xberg 版本与代码固定版本不一致：{}",
            manifest.xberg.tag
        ));
    }
    if manifest.xberg.archive_url.is_empty()
        || manifest.xberg.archive_size_bytes == 0
        || manifest.xberg.archive_sha256.len() != 64
    {
        return Err("Xberg 固定版本归档元数据不完整".to_string());
    }
    for member in &manifest.xberg.members {
        validate_relative_path(&member.path)?;
    }
    if let Some(inference) = &manifest.xberg_inference {
        if !valid_component_tag(&inference.tag)
            || inference.url.is_empty()
            || inference.size_bytes == 0
            || inference.sha256.len() != 64
        {
            return Err("Xberg 推理组件包元数据不完整".to_string());
        }
        if inference.members.is_empty() {
            return Err("Xberg 推理组件包成员清单为空".to_string());
        }
        for member in &inference.members {
            validate_relative_path(&member.path)?;
            validate_relative_path(&member.install_path)?;
        }
        // 段间交叉校验：两段描述同一发布物（设置页下载 xberg 段、媒体成员
        // 校验用 xberg_inference 段），四值必须一致；漂移会让下载 tag 与媒体
        // 成员校验口径分叉，按无效清单拒绝，不静默放行。
        let fixed = &manifest.xberg;
        if inference.tag != fixed.tag
            || inference.url != fixed.archive_url
            || inference.size_bytes != fixed.archive_size_bytes
            || inference.sha256 != fixed.archive_sha256
        {
            return Err(
                "资产清单 xberg 与 xberg_inference 段元数据不一致：两段必须指向同一发布物（tag/URL/大小/SHA-256）"
                    .to_string(),
            );
        }
    }
    Ok(manifest)
}

pub(super) fn asset_root() -> PathBuf {
    if cfg!(test) || cfg!(feature = "test-hooks") {
        if let Some(path) = std::env::var_os("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT") {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                return path;
            }
        }
    }
    state_dir_asset_root(DATA_DIRECTORY)
}

pub(super) fn write_notice(path: &Path, manifest: &AssetManifest) -> Result<(), String> {
    let mut text = String::from("# JchTools 转 Markdown 可选组件许可证\n\n");
    text.push_str("文档与截图基础运行资产随应用提供；FFmpeg 和音频模型仅在设置页主动下载。各模型与推理运行库的许可随运行目录自带。\n\n");
    for license in &manifest.xberg.licenses {
        let _ = writeln!(
            &mut text,
            "- {}：{}，{}",
            license.component, license.license, license.source
        );
    }
    fs::write(path, text).map_err(|error| format!("写入许可证 notice 失败：{error}"))
}
