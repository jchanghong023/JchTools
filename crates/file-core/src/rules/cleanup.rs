use crate::config::{Config, DeleteChoice};
use std::path::Path;

// ---------------------------------------------------------------------------
// 清理项（C-08）
// ---------------------------------------------------------------------------

/// 清理项类别（C-08）：三类清理各自独立启停，并可独立覆盖删除方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupKind {
    Junk,
    Temp,
    Zero,
}
/// C-08：清理命中判定。返回全部命中的已启用类别（Junk / Temp / Zero 顺序），
/// 多类同时命中时由 planner 按 C-01 逐类裁决删除方式；返回空表示不清理。
pub fn cleanup_hits(rel: &str, size: u64, cfg: &Config) -> Vec<(CleanupKind, &'static str)> {
    let Some(name) = Path::new(rel).file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let file = name.to_lowercase();
    let mut hits = Vec::new();
    if cfg.clean_junk
        && (["thumbs.db", ".ds_store", "desktop.ini"].contains(&file.as_str())
            || file.starts_with("._")
            || is_inside_macosx_dir(rel))
    {
        hits.push((CleanupKind::Junk, "用户开启的垃圾文件规则"));
    }
    if cfg.clean_temp
        && (has_ext(&file, "tmp")
            || has_ext(&file, "temp")
            || has_ext(&file, "bak")
            || file.starts_with("~$"))
    {
        hits.push((CleanupKind::Temp, "用户开启的临时/备份文件规则"));
    }
    if cfg.clean_zero && size == 0 {
        hits.push((CleanupKind::Zero, "用户开启的零字节文件规则"));
    }
    hits
}
/// C-08：`__MACOSX` 的清理授权对象是目录名——只比对路径的目录组件；
/// 名为 `__MACOSX` 的普通文件自身不因此命中。
fn is_inside_macosx_dir(rel: &str) -> bool {
    let mut components = rel.split('/');
    components.next_back();
    components.any(|s| s.to_lowercase() == "__macosx")
}
/// C-08：清理命中判定与类别（首个命中类别）。裁决多类命中的删除方式须用
/// [`cleanup_hits`]（C-01：任一已启用规则请求永久删除即形成一次清理删除）。
/// planner 按类别解析删除方式（[`cleanup_delete`]），不得比较原因文案。
pub fn cleanup_reason(rel: &str, size: u64, cfg: &Config) -> Option<(CleanupKind, &'static str)> {
    cleanup_hits(rel, size, cfg).into_iter().next()
}
/// C-08：清理项的删除方式覆盖（默认跟随全局文件删除方式）。
pub fn cleanup_delete(cfg: &Config, kind: CleanupKind) -> DeleteChoice {
    match kind {
        CleanupKind::Junk => cfg.junk_delete,
        CleanupKind::Temp => cfg.temp_delete,
        CleanupKind::Zero => cfg.zero_delete,
    }
}
/// 扩展名等值判断：按最后一个点切分，首位点且无其他点的名称没有扩展名（附录 B）。
/// 输入已预先 lowercase；按字符边界切分，不会 panic。
pub(super) fn has_ext(name: &str, ext: &str) -> bool {
    matches!(name.rsplit_once('.'), Some((stem, tail)) if !stem.is_empty() && tail == ext)
}
