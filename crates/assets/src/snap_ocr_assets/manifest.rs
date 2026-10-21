use crate::asset_util::{valid_component_tag, validate_relative_path};
use serde::Deserialize;
use std::fmt::Write;
use std::fs;
use std::path::Path;

const MANIFEST: &str = include_str!(concat!(env!("OUT_DIR"), "/snap-ocr-assets.json"));
pub(super) const DATA_DIRECTORY: &str = "snap-ocr";
/// worker 固定版本目录：资产清单 worker 条目的 install_path 必须落在该目录下
/// （服务进程从这条固定路径启动，见 gui.rs 的服务监督线程）。
const WORKER_VERSION: &str = "v0.1.2";
pub(super) const WORKER_EXE_NAME: &str = "snap-ocr-worker.exe";
/// 构建期占位标记：打包阶段回填 worker 的真实 size/sha256 后删除该状态。
const WORKER_STATUS_PENDING: &str = "pending-build";

#[derive(Debug, Deserialize)]
pub(super) struct SnapAssetManifest {
    pub(super) schema_version: u32,
    pub(super) assets: Vec<SnapAsset>,
    pub(super) workers: Vec<SnapWorker>,
    /// Xberg 推理组件包（XB-10：xberg.exe、O-07 截图模型集与 onnxruntime，
    /// 来源为固定发布 zip）。清单未接入该条目时为 None，组件缺失时如实报告。
    #[serde(default)]
    pub(super) xberg_inference: Option<SnapInferencePack>,
}

/// 推理组件包条目：tag 决定安装目录 `xberg-inference/<tag>/`，其余字段与
/// 普通归档资产同构，复用同一安装与校验路径。
#[derive(Debug, Deserialize)]
pub(super) struct SnapInferencePack {
    pub(super) tag: String,
    #[serde(flatten)]
    pub(super) asset: SnapAsset,
}

#[derive(Debug, Deserialize)]
pub(super) struct SnapAsset {
    pub(super) id: String,
    pub(super) url: String,
    pub(super) archive_type: String,
    pub(super) install_path: Option<String>,
    pub(super) size_bytes: u64,
    pub(super) sha256: String,
    #[serde(default)]
    pub(super) members: Vec<SnapMember>,
    pub(super) license: SnapLicense,
}

impl SnapAsset {
    /// O-05/XB-25：字体和许可证编入随包 worker，不再依赖用户字体缓存。
    pub(super) fn is_embedded(&self) -> bool {
        matches!(
            self.id.as_str(),
            "noto-sans-mono-cjk-sc" | "noto-cjk-ofl-license"
        )
    }
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct SnapMember {
    pub(super) path: String,
    pub(super) install_path: String,
    pub(super) size_bytes: u64,
    pub(super) sha256: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct SnapLicense {
    pub(super) component: String,
    pub(super) license: String,
    pub(super) source: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct SnapWorker {
    pub(super) id: String,
    #[serde(default)]
    pub(super) status: String,
    pub(super) url: String,
    pub(super) archive_type: String,
    pub(super) install_path: String,
    pub(super) size_bytes: u64,
    pub(super) sha256: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub(super) members: Vec<SnapMember>,
    pub(super) license: SnapLicense,
}

impl SnapWorker {
    /// 构建期占位条目：摘要与大小尚未回填，不得尝试下载（O-09：未提供可获取的
    /// 正确字节前不得宣称初始化链路可用）。
    pub(super) fn is_pending(&self) -> bool {
        self.status == WORKER_STATUS_PENDING
    }
}

/// 解析内置截图 OCR 资产清单，并校验 schema、条目与安装路径约束。
/// 此处不写入 tag 标记，也不按清单 tag 拒绝用户选择的共享推理组件版本。
pub(super) fn load_manifest() -> Result<SnapAssetManifest, String> {
    let manifest: SnapAssetManifest = serde_json::from_str(MANIFEST)
        .map_err(|error| format!("截图 OCR 资产清单无效：{error}"))?;
    if manifest.schema_version != 1 {
        return Err(format!("不支持的资产清单版本：{}", manifest.schema_version));
    }
    if manifest.workers.len() != 1 {
        return Err("截图 OCR 工作进程清单必须且只能有一个条目".to_string());
    }
    for asset in &manifest.assets {
        validate_asset(asset)?;
    }
    let worker = &manifest.workers[0];
    if worker.id.is_empty() || worker.url.is_empty() || worker.install_path.is_empty() {
        return Err("截图 OCR 工作进程清单条目不完整".to_string());
    }
    if worker.is_pending() {
        // 构建期占位：允许加载（页面如实显示未就绪），但禁止通过结构校验混入真下载。
        if worker.size_bytes != 0 || worker.sha256 != "PENDING-BUILD" {
            return Err(format!(
                "工作进程 {} 的占位条目必须保持 size=0 且 sha256=PENDING-BUILD",
                worker.id
            ));
        }
    } else {
        if worker.status != "ok" {
            return Err(format!(
                "工作进程 {} 的状态无效：{}",
                worker.id, worker.status
            ));
        }
        if worker.size_bytes == 0 || worker.sha256.len() != 64 || worker.archive_type != "file" {
            return Err(format!("工作进程 {} 的清单不完整", worker.id));
        }
        let expected = format!("worker/{WORKER_VERSION}/{WORKER_EXE_NAME}").replace('\\', "/");
        let normalized = worker.install_path.replace('\\', "/");
        if normalized != expected {
            return Err(format!(
                "工作进程 {} 的安装路径必须是 {expected}，实际为 {normalized}",
                worker.id
            ));
        }
        validate_relative_path(&worker.install_path)?;
    }
    if let Some(pack) = &manifest.xberg_inference {
        if !valid_component_tag(&pack.tag) {
            return Err("推理组件包 tag 不合法".to_string());
        }
        validate_asset(&pack.asset)?;
        if pack.asset.archive_type != "zip" || pack.asset.members.is_empty() {
            return Err(format!(
                "推理组件包 {} 必须是带成员清单的 zip 归档",
                pack.asset.id
            ));
        }
        for member in &pack.asset.members {
            let under_tag = member.install_path.replace('\\', "/");
            if !under_tag.starts_with(&format!("xberg-inference/{}/", pack.tag)) {
                return Err(format!(
                    "推理组件包 {} 成员 {} 必须安装在 xberg-inference/{}/ 之下",
                    pack.asset.id, member.install_path, pack.tag
                ));
            }
        }
    }
    Ok(manifest)
}

fn validate_asset(asset: &SnapAsset) -> Result<(), String> {
    if asset.id.is_empty()
        || asset.url.is_empty()
        || asset.size_bytes == 0
        || asset.sha256.len() != 64
        || asset.archive_type.is_empty()
    {
        return Err(format!("资产 {} 的清单不完整", asset.id));
    }
    if let Some(path) = &asset.install_path {
        validate_relative_path(path)?;
    }
    if asset.archive_type == "file" && asset.install_path.is_none() {
        return Err(format!("文件资产 {} 缺少安装路径", asset.id));
    }
    if asset.archive_type != "file" && asset.members.is_empty() {
        return Err(format!("归档资产 {} 没有成员清单", asset.id));
    }
    for member in &asset.members {
        validate_relative_path(&member.path)?;
        validate_relative_path(&member.install_path)?;
        if member.size_bytes == 0 || member.sha256.len() != 64 {
            return Err(format!(
                "资产 {} 成员 {} 的清单不完整",
                asset.id, member.path
            ));
        }
    }
    Ok(())
}

pub(super) fn write_notice(path: &Path, manifest: &SnapAssetManifest) -> Result<(), String> {
    let mut text = String::from("# JchTools 截图 OCR 可选组件许可证\n\n");
    text.push_str(
        "截图字体和许可证内置于随包后台程序；Xberg 引擎、模型与推理运行库使用设置页配置的共享目录。\n\n",
    );
    for asset in &manifest.assets {
        let _ = writeln!(
            &mut text,
            "- {}：{}，{}，来源 {}",
            asset.license.component,
            asset.license.license,
            asset.archive_type,
            asset.license.source
        );
    }
    // XB-10/O-11：推理组件包的许可条目与 markdown 侧 notice 口径对齐——许可
    // 文件本身随组件树安装（合规不依赖 notice），此处列出条目并指向安装目录，
    // 与只遍历 assets + workers 的旧写法区分（修复前推理组件在 notice 中缺席）。
    if let Some(pack) = &manifest.xberg_inference {
        let _ = writeln!(
            &mut text,
            "- {}：{}，{}，来源 {}（随组件树安装于 xberg-inference/{}/，许可文件随附）",
            pack.asset.license.component,
            pack.asset.license.license,
            pack.asset.archive_type,
            pack.asset.license.source,
            pack.tag
        );
    }
    for worker in &manifest.workers {
        let state = if worker.is_pending() {
            "构建期占位（打包阶段回填）"
        } else {
            worker.status.as_str()
        };
        let _ = writeln!(
            &mut text,
            "\n## {}\n\n状态：{}\n\n- {}：{}，来源 {}",
            worker.id,
            state,
            worker.license.component,
            worker.license.license,
            worker.license.source
        );
    }
    fs::write(path, text).map_err(|error| format!("写入许可证 notice 失败：{error}"))
}
