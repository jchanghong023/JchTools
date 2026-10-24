//! 处理范围剪枝与边界口径：ScopeFilter 与 special_prefixes（扫描、确认框清点
//! X-02 与整理收尾实空清理共用的同一套判定）、program_self_directory_hint
//! （S-05 提示文案）与 ensure_git_boundary_checked（H-06 收口）。

use crate::{fsutil, rules};
use anyhow::{bail, Result};
use std::{fs, path::Path};

/// S-05：扫描根位于程序自身目录内的提示文案。目录参数来自 canonicalize，
/// 展示必须剥除 Windows 扩展路径前缀（与 fsutil H-06 拒绝信息同口径）。
pub fn program_self_directory_hint(executable_dir: &Path) -> String {
    format!(
        "所选目录位于程序自身目录（{}）内；为避免误处理程序文件，扫描会跳过这个目录里的条目",
        crate::platform::display_path_text(&executable_dir.to_string_lossy())
    )
}

pub fn ensure_git_boundary_checked(error: Option<&str>) -> Result<()> {
    if let Some(error) = error {
        bail!("无法完成必要的 Git 边界判定（H-06），拒绝继续处理：{error}");
    }
    Ok(())
}

/// 处理范围剪枝口径：扫描、确认框清点（X-02）与整理收尾实空清理共用同一套判定，
/// 三处不会各自漂移出不同范围。Git 整树排除（H-06）由调用方在枚举目录前单独判定。
pub struct ScopeFilter<'a> {
    pub excluded: &'a rules::Exclusions,
    pub include_hidden: bool,
    pub include_system: bool,
    /// 「解压失败」暂存区：扫描与清点整树跳过（C-09/X-07）；收尾实空清理不跳过
    /// （该目录实际为空时仍按 H-05 清理，但其内容一律不碰）。
    pub quarantine: Option<&'a str>,
    /// 状态目录/程序目录位于选定根内的相对路径前缀（剪枝其子树）。
    pub state_prefix: Option<String>,
    pub exe_prefix: Option<String>,
    /// S-05：根包含 Windows 系统目录时该子树的相对路径前缀（整树剪枝并提示）。
    pub protected_prefix: Option<String>,
    /// 选定根本身位于状态目录或程序目录内：整棵树按旧口径全部剪枝。
    pub root_under_special: bool,
}
impl ScopeFilter<'_> {
    /// `name` 仅非 Windows 的点开头隐藏判定使用（P-07：产品仅在 Windows 构建）。
    // [quality-baseline approved 2026-10-03] 平台口径参数，经用户裁定保留
    #[cfg_attr(windows, allow(unused_variables))]
    pub fn prunes(&self, child_rel: &str, name: &str, metadata: &fs::Metadata) -> bool {
        if self.root_under_special || fsutil::is_link(metadata) {
            return true;
        }
        for prefix in [
            self.state_prefix.as_deref(),
            self.exe_prefix.as_deref(),
            self.protected_prefix.as_deref(),
        ] {
            if prefix.is_some_and(|p| child_rel == p || child_rel.starts_with(&format!("{p}/"))) {
                return true;
            }
        }
        if let Some(quarantine) = self.quarantine {
            // C-09/X-07：「解压失败」容器按任意层级的路径组件名整树剪枝（不区分大小写）——
            // 子目录单独解压会按 X-06 在子目录里留下隔离容器，整理父目录时同样保留，
            // 其内容不参与去重、归类、清理，也不计入确认框清点。
            // 容器名本身没有大小写变体，ASCII 折叠与 archive.rs 的组件名判定同口径。
            if child_rel
                .split('/')
                .any(|component| component.eq_ignore_ascii_case(quarantine))
            {
                return true;
            }
        }
        // 附录 C：匹配对象没有末尾分隔符，不虚构目录形态扩大排除范围。
        if self.excluded.is_match(child_rel) {
            return true;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if !self.include_hidden && metadata.file_attributes() & 2 != 0 {
                return true;
            }
            if !self.include_system && metadata.file_attributes() & 4 != 0 {
                return true;
            }
        }
        #[cfg(not(windows))]
        if !self.include_hidden && name.starts_with('.') {
            return true;
        }
        false
    }
}

/// 状态目录/程序目录的剪枝口径换算：把「选定根内的特殊目录」折算成相对前缀
/// （条目都以相对路径比较，免去逐条目拼绝对路径），并给出「选定根本身位于特殊
/// 目录内（含重合）」标志——此时整棵树按旧口径全部剪枝。scan 与 count_archives
/// 共用本函数，保证确认框清点（X-02）与正式扫描是同一套过滤口径。
pub fn special_prefixes(
    root: &Path,
    state: Option<&Path>,
    executable_dir: Option<&Path>,
) -> (Option<String>, Option<String>, bool) {
    let prefix_under_root = |special: &Path| -> Option<String> {
        if !special.starts_with(root) {
            return None;
        }
        match fsutil::relative_string(root, special) {
            Ok(rel) if rel.is_empty() => None, // 根自身即特殊目录：走整树剪枝
            Ok(rel) => Some(rel),
            Err(_) => None,
        }
    };
    (
        state.and_then(prefix_under_root),
        executable_dir.and_then(prefix_under_root),
        state.is_some_and(|dir| root.starts_with(dir))
            || executable_dir.is_some_and(|dir| root.starts_with(dir)),
    )
}
