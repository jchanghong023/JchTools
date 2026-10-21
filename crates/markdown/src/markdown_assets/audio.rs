use jchtools_assets::asset_util::{
    ensure_not_cancelled, valid_component_tag, validate_relative_path, verify_file_with_cancel,
    AssetDownloader,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use uuid::Uuid;

use super::manifest::AssetFile;
use super::runtime::runtime_dir;

pub(super) const XBERG_RELEASE_RECEIPT: &str = "jchtools-xberg-release.json";
pub(super) const XBERG_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/jchanghong023/xberg/releases/latest";
pub(super) const XBERG_TARGET: &str = "x86_64-pc-windows-msvc";
pub(super) const XBERG_COMPONENT_MANIFEST: &str = "xberg-components-x86_64-pc-windows-msvc.json";
pub(super) const XBERG_ARCHIVE_PREFIX: &str = "xberg-cli-x86_64-pc-windows-msvc";
pub(super) const AUDIO_ARCHIVE: &str = "xberg-audio-assets-x86_64-pc-windows-msvc.zip";
static OPTIONAL_INSTALL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 生产下载器：只访问清单固定地址（T-21），HTTP 原语复用截图 OCR 侧的
/// 流式下载器（P-03 联网边界测试限制 `ureq` 只出现在两个资产模块内）。
struct NetworkDownloader;

impl AssetDownloader for NetworkDownloader {
    fn download(
        &mut self,
        url: &str,
        destination: &Path,
        expected_size: u64,
        expected_sha256: &str,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(String),
    ) -> Result<(), String> {
        let mut sink = |message: String| progress(message);
        jchtools_assets::snap_ocr_assets::download_asset(
            url,
            destination,
            expected_size,
            expected_sha256,
            cancel,
            &mut sink,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct ReleaseAsset {
    pub(super) archive: String,
    pub(super) url: String,
    pub(super) size_bytes: u64,
    pub(super) sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct FeatureComponent {
    pub(super) id: String,
    pub(super) archive: String,
    pub(super) requires: Vec<String>,
    pub(super) size_bytes: u64,
    pub(super) sha256: String,
    pub(super) members: Vec<AssetFile>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct AudioAsset {
    #[serde(flatten)]
    pub(super) component: FeatureComponent,
    pub(super) url: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct ComponentReceipt {
    pub(super) owner: String,
    pub(super) tag: String,
    pub(super) source: String,
    pub(super) manifest: ReleaseAsset,
    pub(super) components: Vec<FeatureComponent>,
    pub(super) audio_assets: AudioAsset,
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn optional_audio_path(path: &str) -> bool {
    path.starts_with("ffmpeg/")
        || path.starts_with("models/vad/")
        || path.strip_prefix("models/").is_some_and(|tail| {
            tail.split_once('/')
                .is_some_and(|(directory, _)| directory.starts_with("sense_voice_"))
        })
        || path.starts_with("samples/")
}

fn validate_member(member: &AssetFile) -> Result<(), String> {
    validate_relative_path(&member.path)?;
    // Windows 路径别名、ADS、大小写冲突不能借用音频清单覆盖基础成员。
    if member.path.contains('\\')
        || !valid_digest(&member.sha256)
        || member.path.split('/').any(|part| {
            let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
            part.ends_with(['.', ' '])
                || part.chars().any(|c| c < ' ' || "<>\"|?*".contains(c))
                || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || (stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        })
    {
        return Err(format!("Xberg 收据成员路径或摘要无效：{}", member.path));
    }
    Ok(())
}

fn load_receipt(root: &Path) -> Result<ComponentReceipt, String> {
    let bytes = fs::read(root.join(XBERG_RELEASE_RECEIPT))
        .map_err(|error| format!("无法读取随包 Xberg 发布收据，请修复安装包：{error}"))?;
    let receipt: ComponentReceipt = serde_json::from_slice(&bytes)
        .map_err(|error| format!("随包 Xberg 发布收据无效：{error}"))?;
    let release_url = |archive: &str| {
        format!(
            "https://github.com/jchanghong023/xberg/releases/download/{}/{archive}",
            receipt.tag
        )
    };
    if receipt.owner != "JchTools-bundled-xberg-v1"
        || receipt.source != XBERG_LATEST_RELEASE_URL
        || !valid_component_tag(&receipt.tag)
        || receipt.manifest.archive != XBERG_COMPONENT_MANIFEST
        || receipt.manifest.url != release_url(XBERG_COMPONENT_MANIFEST)
        || receipt.manifest.size_bytes == 0
        || !valid_digest(&receipt.manifest.sha256)
        || receipt.components.len() != 2
    {
        return Err("随包 Xberg 发布收据来源、版本或基础组件无效".into());
    }
    let mut ids = BTreeSet::new();
    let mut base_paths: BTreeMap<String, &AssetFile> = BTreeMap::new();
    for component in &receipt.components {
        if !matches!(component.id.as_str(), "document" | "snapshot")
            || !ids.insert(component.id.as_str())
            || !component.requires.is_empty()
            || component.archive != format!("xberg-{}-{XBERG_TARGET}.zip", component.id)
            || component.size_bytes == 0
            || !valid_digest(&component.sha256)
            || component.members.is_empty()
        {
            return Err("随包 Xberg 基础组件记录无效".into());
        }
        let mut own_paths = BTreeSet::new();
        for member in &component.members {
            validate_member(member)?;
            let key = member.path.to_ascii_lowercase();
            if optional_audio_path(&key)
                || key == XBERG_RELEASE_RECEIPT
                || !own_paths.insert(key.clone())
                || base_paths
                    .get(&key)
                    .is_some_and(|previous| *previous != member)
            {
                return Err("随包 Xberg 基础成员冲突或混入可选音频资产".into());
            }
            base_paths.insert(key, member);
        }
    }
    let audio = &receipt.audio_assets.component;
    if audio.id != "audio-assets"
        || audio.archive != AUDIO_ARCHIVE
        || receipt.audio_assets.url != release_url(AUDIO_ARCHIVE)
        || !audio.requires.is_empty()
        || audio.size_bytes == 0
        || !valid_digest(&audio.sha256)
        || audio.members.is_empty()
    {
        return Err("随包 Xberg 音频收据与锁定发布资产不一致".into());
    }
    let mut audio_paths = BTreeSet::new();
    for member in &audio.members {
        validate_member(member)?;
        let key = member.path.to_ascii_lowercase();
        if !optional_audio_path(&member.path)
            || base_paths.contains_key(&key)
            || !audio_paths.insert(key)
        {
            return Err("音频收据包含基础文件、未知目录或重复成员，拒绝安装".into());
        }
    }
    Ok(receipt)
}

fn safe_member_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    validate_relative_path(relative)?;
    let mut current = root.to_path_buf();
    for segment in std::iter::once(None).chain(relative.split('/').map(Some)) {
        if let Some(segment) = segment {
            current.push(segment);
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                let mut linked = metadata.file_type().is_symlink();
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    linked |= metadata.file_attributes() & 0x400 != 0;
                }
                if linked {
                    return Err("音频资产路径包含符号链接或重解析点，拒绝安装".into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查音频资产路径失败：{error}")),
        }
    }
    Ok(current)
}

fn verify_member(root: &Path, member: &AssetFile, cancel: &AtomicBool) -> Result<(), String> {
    verify_file_with_cancel(
        &safe_member_path(root, &member.path)?,
        member.size_bytes,
        &member.sha256,
        cancel,
    )
}

pub(super) fn verify_component(
    root: &Path,
    component: &FeatureComponent,
    cancel: &AtomicBool,
) -> Result<(), String> {
    for member in &component.members {
        ensure_not_cancelled(cancel)?;
        verify_member(root, member, cancel)?;
    }
    Ok(())
}

fn collect_files(root: &Path, paths: &mut BTreeSet<String>, relative: &Path) -> Result<(), String> {
    for entry in
        fs::read_dir(root.join(relative)).map_err(|error| format!("读取解包目录失败：{error}"))?
    {
        let entry = entry.map_err(|error| format!("读取解包成员失败：{error}"))?;
        let path = relative.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|error| format!("读取解包成员类型失败：{error}"))?;
        if kind.is_dir() {
            collect_files(root, paths, &path)?;
        } else if kind.is_file() {
            paths.insert(
                path.to_str()
                    .ok_or("组件路径无法编码为 Unicode")?
                    .replace('\\', "/"),
            );
        } else {
            return Err("组件归档包含非普通文件".into());
        }
    }
    Ok(())
}

/// 只在用户主动操作时下载当前生效引擎锁定的音频附加包，不查询最新发布。
pub fn download_audio_runtime(
    cancel: &AtomicBool,
    progress: impl FnMut(String),
) -> Result<PathBuf, String> {
    download_audio_runtime_with(cancel, progress, &mut NetworkDownloader)
}

/// 注入真实资产供给器；来源、归档、成员与落位仍经过完整生产校验。
#[cfg(any(test, feature = "test-hooks"))]
pub fn download_audio_runtime_with_downloader(
    cancel: &AtomicBool,
    progress: impl FnMut(String),
    downloader: &mut dyn AssetDownloader,
) -> Result<PathBuf, String> {
    download_audio_runtime_with(cancel, progress, downloader)
}

fn download_audio_runtime_with(
    cancel: &AtomicBool,
    mut progress: impl FnMut(String),
    downloader: &mut dyn AssetDownloader,
) -> Result<PathBuf, String> {
    let _guard = OPTIONAL_INSTALL.lock().map_err(|_| "音频安装锁不可用")?;
    ensure_not_cancelled(cancel)?;
    let root = runtime_dir()?;
    let receipt = load_receipt(&root)?;
    #[cfg(any(test, feature = "test-hooks"))]
    audio_download_test_barrier(cancel, &mut progress)?;
    let audio = &receipt.audio_assets.component;
    if verify_component(&root, audio, cancel).is_ok() {
        ensure_not_cancelled(cancel)?;
        progress(format!("已复用 {} 的完整音频资产，无需下载", receipt.tag));
        return Ok(root);
    }
    ensure_not_cancelled(cancel)?;
    with_audio_staging(&root, &mut progress, |staging, progress| {
        progress(format!("正在下载 {} 的可选音频资产…", receipt.tag));
        let archive = staging.join(AUDIO_ARCHIVE);
        downloader.download(
            &receipt.audio_assets.url,
            &archive,
            audio.size_bytes,
            &audio.sha256,
            cancel,
            progress,
        )?;
        verify_file_with_cancel(&archive, audio.size_bytes, &audio.sha256, cancel)?;
        let extracted = staging.join("extracted");
        fs::create_dir(&extracted).map_err(|error| format!("创建音频解包目录失败：{error}"))?;
        jchtools_assets::asset_util::extract_zip_safely(&archive, &extracted, cancel)?;
        let mut actual = BTreeSet::new();
        collect_files(&extracted, &mut actual, Path::new(""))?;
        let expected = audio
            .members
            .iter()
            .map(|member| format!("{XBERG_ARCHIVE_PREFIX}/{}", member.path))
            .collect::<BTreeSet<_>>();
        if actual != expected {
            return Err("音频归档成员与锁定收据不一致，拒绝安装".into());
        }
        let source = extracted.join(XBERG_ARCHIVE_PREFIX);
        verify_component(&source, audio, cancel)?;
        install_audio_members(&root, &source, &audio.members, staging, cancel, || {
            // 同根补入资产不切换来源、不重启正在执行的唯一 worker。
            ensure_not_cancelled(cancel)
        })?;
        progress("可选音频资产已安装，现有文档与截图任务不受影响".into());
        Ok(root.clone())
    })
}

/// 真实 GUI 取消路径的离线屏障：仅隔离测试构建可启用，不供应伪造资产或成功结果。
#[cfg(any(test, feature = "test-hooks"))]
fn audio_download_test_barrier(
    cancel: &AtomicBool,
    progress: &mut impl FnMut(String),
) -> Result<(), String> {
    let Some(marker) = std::env::var_os("JCHTOOLS_TEST_AUDIO_DOWNLOAD_BARRIER") else {
        return Ok(());
    };
    let state = std::env::var_os("JCHTOOLS_TEST_STATE_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("音频下载测试屏障必须使用隔离状态目录")?;
    let marker = PathBuf::from(marker);
    let parent = marker.parent().ok_or("音频下载测试屏障缺少父目录")?;
    let state =
        fs::canonicalize(state).map_err(|error| format!("解析测试状态目录失败：{error}"))?;
    let parent =
        fs::canonicalize(parent).map_err(|error| format!("解析测试屏障目录失败：{error}"))?;
    if !marker.is_absolute() || !parent.starts_with(&state) {
        return Err("音频下载测试屏障必须位于隔离状态目录内".into());
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .map_err(|error| format!("创建音频下载测试屏障失败：{error}"))?;
    progress("测试音频下载已进入可取消等待，未发出网络请求".into());
    loop {
        ensure_not_cancelled(cancel)?;
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn with_audio_staging<T>(
    root: &Path,
    progress: &mut impl FnMut(String),
    operation: impl FnOnce(&Path, &mut dyn FnMut(String)) -> Result<T, String>,
) -> Result<T, String> {
    let staging = root.join(format!(".audio-staging-{}", Uuid::new_v4()));
    fs::create_dir(&staging).map_err(|error| format!("创建音频暂存目录失败：{error}"))?;
    let result = operation(&staging, progress);
    // 回滚失败的备份必须保留供恢复，绝不以清理吞掉原文件。
    if !staging.join("recovery-required").exists() {
        if let Err(error) = fs::remove_dir_all(&staging) {
            tracing::warn!(event = "audio_staging_cleanup_failed", error_kind = ?error.kind(), "音频暂存目录清理失败");
            progress(format!("警告：音频暂存目录清理失败：{error}"));
        }
    }
    result
}

/// 成员事务只写附加包路径；有效现有成员原位复用，失败恢复原有无效成员。
pub(super) fn install_audio_members(
    root: &Path,
    source: &Path,
    members: &[AssetFile],
    staging: &Path,
    cancel: &AtomicBool,
    commit: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let mut installed = Vec::new();
    let mut backups = Vec::new();
    let mut directories = Vec::new();
    let result = (|| {
        for (index, member) in members.iter().enumerate() {
            ensure_not_cancelled(cancel)?;
            if !optional_audio_path(&member.path) {
                return Err("拒绝通过音频安装覆盖基础文件".into());
            }
            if verify_member(root, member, cancel).is_ok() {
                continue;
            }
            ensure_not_cancelled(cancel)?;
            verify_member(source, member, cancel)?;
            let target = safe_member_path(root, &member.path)?;
            let parent = target.parent().ok_or("音频成员缺少父目录")?;
            let mut missing = Vec::new();
            let mut cursor = parent;
            while !cursor.exists() {
                missing.push(cursor.to_path_buf());
                cursor = cursor.parent().ok_or("音频目录缺少父目录")?;
            }
            for directory in missing.into_iter().rev() {
                fs::create_dir(&directory)
                    .map_err(|error| format!("创建音频成员目录失败：{error}"))?;
                directories.push(directory);
            }
            let prepared = staging.join(format!("member-{index}"));
            fs::copy(safe_member_path(source, &member.path)?, &prepared)
                .map_err(|error| format!("暂存音频成员失败：{error}"))?;
            // Windows FlushFileBuffers 需要写权限；仅打开本次自有暂存文件。
            File::options()
                .write(true)
                .open(&prepared)
                .and_then(|file| file.sync_all())
                .map_err(|error| format!("同步音频暂存成员失败：{error}"))?;
            verify_file_with_cancel(&prepared, member.size_bytes, &member.sha256, cancel)?;
            ensure_not_cancelled(cancel)?;
            safe_member_path(root, &member.path)?;
            if target.exists() {
                if !target.is_file() {
                    return Err("音频目标被非普通文件占用".into());
                }
                let backup = staging.join(format!("backup-{index}"));
                fs::rename(&target, &backup)
                    .map_err(|error| format!("备份已有音频成员失败：{error}"))?;
                backups.push((target.clone(), backup));
            }
            // 同一文件系统内以 create-new 语义发布，绝不覆盖并发出现的文件。
            fs::hard_link(&prepared, &target)
                .map_err(|error| format!("安装音频成员失败：{error}"))?;
            installed.push(target);
        }
        ensure_not_cancelled(cancel)?;
        commit()
    })();
    if let Err(error) = result {
        let mut recovery = Vec::new();
        for path in installed.iter().rev() {
            if let Err(error) = fs::remove_file(path) {
                recovery.push(format!("删除本轮音频成员失败：{error}"));
            }
        }
        for (target, backup) in backups.iter().rev() {
            if let Err(error) = fs::hard_link(backup, target) {
                recovery.push(format!("恢复原音频成员失败：{error}"));
            }
        }
        for directory in directories.iter().rev() {
            let _ = fs::remove_dir(directory);
        }
        if !recovery.is_empty() {
            let _ = fs::write(staging.join("recovery-required"), b"keep backups");
            return Err(format!(
                "{error}；{}；原文件备份保留于 {}",
                recovery.join("；"),
                staging.display()
            ));
        }
        return Err(error);
    }
    Ok(())
}

/// 离线迁移兼容音频成员后，交由 broker 在空闲边界提交唯一随包运行目录。
/// 迁移失败保留并返回旧生效目录，不把已就绪媒体静默变为不可用。
pub fn migrate_bundled_runtime(
    cancel: &AtomicBool,
    progress: impl FnMut(String),
) -> Result<PathBuf, String> {
    migrate_bundled_runtime_with_activation(cancel, progress, |root, preserve_media| {
        jchtools_xberg::xberg_runtime::activate_bundled_runtime(root, preserve_media)
    })
}

pub(super) fn migrate_bundled_runtime_with_activation(
    cancel: &AtomicBool,
    mut progress: impl FnMut(String),
    mut activate: impl FnMut(&Path, bool) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let _guard = OPTIONAL_INSTALL.lock().map_err(|_| "音频迁移锁不可用")?;
    let previous = jchtools_foundation::xberg_settings::effective_directory()?;
    if let Some(warning) = jchtools_foundation::xberg_settings::settings()?.migration_warning {
        progress(warning);
        return previous.ok_or_else(|| "旧配置未保存，且没有可继续使用的运行目录".into());
    }
    let old_media_ready = previous
        .as_ref()
        .is_some_and(|old| jchtools_xberg::xberg_runtime::validate_assets(old, "media").is_ok());
    let mut uncertain_commit = false;
    let attempt = (|| {
        ensure_not_cancelled(cancel)?;
        let bundled = jchtools_foundation::xberg_settings::bundled_directory()?
            .ok_or("未找到随包 Xberg 目录，请修复安装包")?;
        let bundled =
            fs::canonicalize(&bundled).map_err(|error| format!("解析随包运行目录失败：{error}"))?;
        let receipt = load_receipt(&bundled)?;
        jchtools_xberg::xberg_runtime::validate_assets(&bundled, "document")?;
        jchtools_xberg::xberg_runtime::validate_assets(&bundled, "snapshot")?;
        if previous.as_ref() == Some(&bundled) {
            ensure_not_cancelled(cancel)?;
            activate(&bundled, old_media_ready)?;
            return Ok(bundled);
        }
        with_audio_staging(&bundled, &mut progress, |staging, progress| {
            let mut compatible = Vec::new();
            for member in &receipt.audio_assets.component.members {
                ensure_not_cancelled(cancel)?;
                if verify_member(&bundled, member, cancel).is_ok() {
                    continue;
                }
                ensure_not_cancelled(cancel)?;
                if let Some(old) = &previous {
                    if verify_member(old, member, cancel).is_ok() {
                        compatible.push(member.clone());
                        continue;
                    }
                }
                ensure_not_cancelled(cancel)?;
                if old_media_ready {
                    return Err(format!(
                        "旧目录音频已就绪，但成员与目标发布不兼容：{}；保留旧来源",
                        member.path
                    ));
                }
            }
            let source = previous.as_deref().unwrap_or(&bundled);
            progress(format!(
                "离线迁移 {} 项目标发布兼容的音频成员",
                compatible.len()
            ));
            let mut uncertain = None;
            install_audio_members(&bundled, source, &compatible, staging, cancel, || {
                ensure_not_cancelled(cancel)?;
                if old_media_ready {
                    jchtools_xberg::xberg_runtime::validate_assets(&bundled, "media")?;
                }
                match activate(&bundled, old_media_ready) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        match jchtools_foundation::xberg_settings::effective_directory() {
                            Ok(Some(active)) if active == bundled => {
                                tracing::warn!(
                                    event = "bundled_runtime_commit_reply_lost",
                                    "来源已提交但代理确认失败，保留目标音频资产"
                                );
                                Ok(())
                            }
                            Ok(_) => Err(error),
                            Err(read_error) => {
                                // IPC 确认丢失且配置暂不可读时不能删除可能已生效的媒体文件。
                                uncertain_commit = true;
                                uncertain = Some(format!("来源提交状态暂不可确认，已保留目标音频资产：{error}；{read_error}"));
                                Ok(())
                            }
                        }
                    }
                }
            })?;
            if let Some(error) = uncertain {
                return Err(error);
            }
            Ok(bundled.clone())
        })
    })();
    match attempt {
        Ok(root) => Ok(root),
        Err(error) => {
            tracing::warn!(
                event = "bundled_runtime_migration_deferred",
                cancelled = cancel.load(std::sync::atomic::Ordering::Acquire),
                "随包运行目录迁移未获确认，保留既有文件与来源记录"
            );
            if uncertain_commit {
                progress(error.clone());
                return Err(error);
            }
            if let Some(previous) = previous {
                progress(format!("随包迁移未提交，继续使用原运行目录：{error}"));
                Ok(previous)
            } else {
                Err(error)
            }
        }
    }
}
