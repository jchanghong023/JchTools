//! 可选资产层共享的底层工具：路径安全校验、SHA-256 校验、原子落位、
//! zip 安全解包与 Xberg 推理组件包安装核心。
//!
//! 实现按本目录下的私有子模块组织：`verify`（SHA-256 摘要与文件校验）、
//! `atomic`（原子落位）、`zip_extract`（zip 安全解包）、`inference`（推理
//! 组件包安装核心）；本文件保留两个资产模块共用的初始化骨架与路径/根目录
//! 助手，并统一 re-export 全部对外条目，`crate::asset_util::X` 与外部
//! `jchtools_assets::asset_util::X` 引用路径保持不变。
//!
//! 转 Markdown（`markdown_assets`）与截图 OCR（`snap_ocr_assets`）各自维护
//! 清单与就绪口径，但落盘/校验/原子落位语义必须同源：同一份实现保证两个功能
//! 的 staging → 校验 → 原子落位、失败不动已验证资产、跨重试复用等行为
//! 完全一致（XB-09/XB-10 安装语义）。上层模块只保留各自的清单解析、
//! 就绪检查与用户指引文案；HTTP 下载原语（`ureq`）按 P-03 执法测试的
//! 文件白名单留在 `snap_ocr_assets`，本模块保持离线。

mod atomic;
mod inference;
mod verify;
mod zip_extract;

pub use atomic::atomic_replace_dir;
pub(crate) use atomic::atomic_replace_file;
#[cfg(test)]
pub(crate) use atomic::restore_backup;
#[cfg(any(test, feature = "test-hooks"))]
pub use inference::{inference_ready, install_inference_pack, resolve_component_with_tag};
pub use inference::{AssetDownloader, InferenceManifest, InferenceMember};
pub(crate) use verify::verify_file;
pub use verify::{sha256_file_inner, verify_file_with_cancel};
pub use zip_extract::extract_zip_safely;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// 组件在位校验（存在性，不做摘要）：清单声明的成员在组件目录内逐个存在，
/// 缺失时报「推理组件不完整」并指明首个缺失成员与组件目录（截图 OCR 与
/// 转 Markdown 共用同一口径与文案）。`required` 由调用方以组件目录为前缀
/// 拼接，错误文案里的相对路径分隔符因此与各调用方的 join 链保持一致。
pub fn require_component_members(component: &Path, required: &[PathBuf]) -> Result<(), String> {
    for path in required {
        if !path.is_file() {
            let relative = path.strip_prefix(component).unwrap_or(path);
            return Err(format!(
                "推理组件不完整：缺少 {}（组件目录 {}）",
                relative.display(),
                component.display()
            ));
        }
    }
    Ok(())
}

/// 资产根目录的固定回退链（不含各功能的测试覆盖变量）：用户状态目录 →
/// LOCALAPPDATA\JchTools → 系统临时目录；截图 OCR 与转 Markdown 两个资产
/// 模块共用，保证回退口径只有一处实现。
pub fn state_dir_asset_root(data_directory: &str) -> PathBuf {
    if let Ok(path) = jchtools_file_core::config::state_dir() {
        return path.join(data_directory);
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data)
            .join("JchTools")
            .join(data_directory);
    }
    std::env::temp_dir().join("JchTools").join(data_directory)
}

/// 推理组件包 tag 合法性：非空且不含路径分隔符、`..` 与冒号（防止落位到
/// 组件目录之外）。两侧清单校验共用同一谓词。
pub fn valid_component_tag(tag: &str) -> bool {
    !tag.is_empty()
        && !tag.contains('/')
        && !tag.contains('\\')
        && !tag.contains("..")
        && !tag.contains(':')
}

pub fn validate_relative_path(path: &str) -> Result<(), String> {
    let path = path.replace('\\', "/");
    let candidate = Path::new(&path);
    if path.is_empty()
        || candidate.is_absolute()
        || path.starts_with('/')
        || path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path.contains(':')
    {
        return Err(format!("资产路径不安全：{path}"));
    }
    Ok(())
}

pub fn ensure_not_cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err("用户已取消初始化".to_string())
    } else {
        Ok(())
    }
}

// ── 两资产模块共享的初始化骨架 ─────────────────────────────────────────
// markdown_assets 与 snap_ocr_assets 对同一安装语义（组件解析、staging 收尾、
// 残留兜底、成员校验）此前各留一份相同实现，仅靠注释约定同步；收敛到本模块
// 后一处修改即可同时生效（XB-09/XB-16 同口径），模块只保留各自的清单与文案。

/// 组件目录解析：只取应用 SQLite 保存的共享 Xberg 目录（XB-18/XB-19），
/// 即设置页保存的目录或产品内下载后保存的目录（2026-10-04 用户指示删除
/// `JCHTOOLS_XBERG_INFERENCE_DIR` 环境变量覆盖）。两个功能必须用同一规则
/// 解析同一安装，不允许出现第二套目录口径。
pub fn resolve_xberg_component() -> Result<PathBuf, String> {
    jchtools_foundation::xberg_settings::required()
}

/// 就绪检查的推理组件成员检查：只检查请求场景需要的成员是否在场（XB-16
/// 场景隔离，归属规则见 `xberg_runtime::asset_for_scenario`），逐成员做
/// 存在性检查（XB-09 2026-10-02 修订：运行时不比对大小与 SHA-256，用户
/// 可自行替换引擎文件；成员缺失仍明确报错），错误统一带成员相对路径。
pub fn require_inference_members_for_scenario(
    component: &Path,
    scenario: &str,
    members: &[InferenceMember],
) -> Result<(), String> {
    for member in members {
        if jchtools_xberg::xberg_runtime::asset_for_scenario(&member.install_path, scenario)
            && !component.join(&member.install_path).is_file()
        {
            return Err(format!(
                "推理组件成员 {} 缺失（组件目录 {}）",
                member.install_path,
                component.display()
            ));
        }
    }
    Ok(())
}

/// 初始化收尾：删除本轮 staging 目录，主结果优先返回。
///
/// staging 清理是尽力而为（B-1）：关键变更已成功时清理失败只经 progress
/// 发出警告、维持 Ok（Windows 上防护软件短暂持有句柄曾把成功初始化误报为
/// 失败）；残留目录由下次 initialize 入口的 [`cleanup_stale_staging_dirs`]
/// 兜底收集。
pub fn finalize_staging(
    result: Result<(), String>,
    staging: &Path,
    progress: &mut dyn FnMut(String),
) -> Result<(), String> {
    if let Err(error) = fs::remove_dir_all(staging) {
        if result.is_ok() {
            tracing::warn!(
                component = "asset_util",
                event = "asset_staging_cleanup_failed",
                stage = "finalize_staging",
                error_type = ?error.kind(),
                error_code = ?error.raw_os_error(),
                "初始化完成后清理临时目录失败"
            );
            progress(format!("警告：清理初始化临时目录失败：{error}"));
        }
    }
    result
}

/// 兜底清理资产根下历史残留的 `.staging-*` 目录（B-2），单项失败跳过继续；
/// 各模块特有的残留（旧选择临时文件、expected-tag 标记）由调用方在本函数
/// 之后自行清扫。
///
/// 并发前提：主程序单实例，各功能的初始化由 gui.rs 对应守卫串行（单初始化
/// 线程），入口处发现的 `.staging-*` 必为历史残留，不存在在途 staging 被误删
/// 的并发窗口。`.staging-*` 前缀的非目录条目不是本流程产物（staging 恒为
/// 目录），按设计跳过；资产根不存在（首次运行）时无需清理。
pub fn cleanup_stale_staging_dirs(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_staging = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".staging-"));
        if is_staging && path.is_dir() {
            // 尽力而为：残留被防护软件短暂锁定时跳过，下次初始化再试。
            let _ = fs::remove_dir_all(&path);
        }
    }
}

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
mod tests {
    use super::{atomic_replace_dir, atomic_replace_file, restore_backup, verify_file_with_cancel};
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::AtomicBool;
    #[test]
    fn restore_backup_reports_failure_and_preserves_backup() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let backup = root.path().join("backup");
        let destination = root.path().join("destination");
        fs::write(&backup, b"old-content").expect("写入备份");
        fs::create_dir(&destination).expect("创建冲突目录");

        let error =
            restore_backup(&backup, &destination, "文件").expect_err("目标冲突时恢复必须报告失败");

        assert!(error.contains("恢复旧文件失败"));
        assert!(error.contains("旧文件仍保留在"));
        assert!(backup.exists(), "恢复失败时不得丢弃备份");
        assert!(destination.is_dir(), "冲突目标必须保持不变");
    }

    // 覆盖 XB-10 / T-06 / O-09：Windows 等价成员不能覆盖已解出的成员。
    #[test]
    fn zip_case_collision_preserves_first_member() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let archive = root.path().join("assets.zip");
        let mut writer = zip::ZipWriter::new(fs::File::create(&archive).expect("创建压缩包"));
        for (name, bytes) in [("asset.bin", b"first".as_slice()), ("ASSET.BIN", b"second")] {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .expect("创建压缩包成员");
            std::io::Write::write_all(&mut writer, bytes).expect("写入成员");
        }
        writer.finish().expect("完成压缩包");
        let extracted = root.path().join("extracted");
        fs::create_dir(&extracted).expect("创建解包目录");

        let result = super::extract_zip_safely(&archive, &extracted, &AtomicBool::new(false));

        assert!(result.is_err(), "Windows 同名成员必须拒绝而非覆盖");
        assert_eq!(fs::read(extracted.join("asset.bin")).unwrap(), b"first");
    }

    // 覆盖 XB-10 / T-06 / O-09：资产成员路径不能寻址 Windows 替代数据流。
    #[test]
    fn asset_path_rejects_alternate_data_stream() {
        for path in ["dir/file:stream", "file.bin:stream", "dir/file::$DATA"] {
            assert!(
                super::validate_relative_path(path).is_err(),
                "必须拒绝替代数据流路径：{path}"
            );
        }
        assert!(super::validate_relative_path("dir/file.bin").is_ok());
    }

    // 覆盖 XB-10 / T-06 / O-09：下载归档的成员同样不能写入替代数据流。
    #[test]
    fn zip_rejects_alternate_data_stream_member() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let archive = root.path().join("assets.zip");
        let mut writer = zip::ZipWriter::new(fs::File::create(&archive).expect("创建压缩包"));
        writer
            .start_file("dir/file:stream", zip::write::SimpleFileOptions::default())
            .expect("创建压缩包成员");
        std::io::Write::write_all(&mut writer, b"stream").expect("写入成员");
        writer.finish().expect("完成压缩包");
        let extracted = root.path().join("extracted");
        fs::create_dir(&extracted).expect("创建解包目录");

        let result = super::extract_zip_safely(&archive, &extracted, &AtomicBool::new(false));

        assert!(result.is_err(), "压缩包替代数据流成员必须拒绝");
        assert!(!extracted.join("dir/file").exists(), "不能创建流的宿主文件");
    }

    #[test]
    fn verify_file_with_cancel_stops_before_hashing() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let file = root.path().join("asset.bin");
        fs::write(&file, b"asset").expect("写入资产");
        let cancel = AtomicBool::new(true);

        let error = verify_file_with_cancel(&file, 5, &"00".repeat(32), &cancel)
            .expect_err("已取消的哈希校验必须立即停止");
        assert_eq!(error, "用户已取消初始化");
    }

    // 覆盖 B-1：原子就位成功后，旧备份清理失败不得把已成功的安装误报为失败。
    // 注入方式：旧目标本身是目录时，备份（改名后的目录）无法被 remove_file
    // 删除，稳定复现「两次关键 rename 均已成功、仅清理失败」。生产触发是
    // 防护软件/索引器短暂持有句柄（不可控），此处以确定性删除失败替代；
    // 勿用句柄方案：句柄需在改名前打开，而无共享删除的句柄会先阻塞第一次
    // rename，根本走不到清理分支。
    #[test]
    fn atomic_replace_file_cleanup_failure_still_succeeds() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let destination = root.path().join("asset.bin");
        fs::create_dir(&destination).expect("旧目标预置为目录（使备份不可删除）");
        let staged = root.path().join("staged.bin");
        fs::write(&staged, b"new-asset").expect("写入新资产");

        atomic_replace_file(&staged, &destination)
            .expect("关键变更已就位时清理失败不得报失败（B-1）");

        assert_eq!(
            fs::read(&destination).expect("新资产必须已就位"),
            b"new-asset",
            "destination 必须是新资产内容"
        );
    }

    // 覆盖 B-1：目录级原子就位成功后，备份清理失败不得报失败。
    // 注入方式（Windows ACL）：拒绝内部文件的 DELETE（仅对象）、目标目录
    // 继承拒绝 DC（删除子项）。目标目录整体改名只需对象自身 DELETE 权限
    // （未被拒）仍可成功；清理时删除内部文件的两条路径（对象 DELETE /
    // 父目录 DC）均被拒 → remove_dir_all 确定性失败。
    // 平台门禁原因：注入依赖 icacls（Windows 独有）；「清理失败不误报成功」
    // 的同一语义由无门禁的 atomic_replace_file 用例覆盖（本项目按 P-07 仅
    // 支持 Windows，门禁不影响生产平台覆盖）。
    #[cfg(windows)]
    #[test]
    fn atomic_replace_dir_cleanup_failure_still_succeeds() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let destination = root.path().join("asset-dir");
        fs::create_dir_all(destination.join("nested")).expect("预置旧资产目录");
        fs::write(destination.join("nested").join("old.txt"), b"old").expect("预置旧资产文件");
        let user = std::env::var("USERNAME").expect("获取当前用户名");
        for (target, rule) in [
            (
                destination.join("nested").join("old.txt"),
                format!("{user}:(D)"),
            ),
            (destination.clone(), format!("{user}:(OI)(CI)(DC)")),
        ] {
            let output = Command::new("icacls")
                .arg(&target)
                .args(["/deny", &rule])
                .output()
                .expect("调用 icacls 注入删除失败");
            assert!(
                output.status.success(),
                "icacls 必须成功：{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let staged = root.path().join("staged-dir");
        fs::create_dir_all(&staged).expect("创建新资产目录");
        fs::write(staged.join("new.txt"), b"new-asset").expect("写入新资产");

        let result = atomic_replace_dir(&staged, &destination);
        // 无论结论如何先恢复 ACL，保证临时目录可被 tempfile 清理。
        let _ = Command::new("icacls")
            .arg(root.path())
            .args(["/reset", "/t", "/q"])
            .output();
        result.expect("关键变更已就位时清理失败不得报失败（B-1）");
        assert_eq!(
            fs::read(destination.join("new.txt")).expect("新资产必须已就位"),
            b"new-asset",
            "destination 必须是新资产内容"
        );
    }
}
