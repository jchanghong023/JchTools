use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeleteMode { Keep, Recycle, Permanent }
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeleteChoice { Global, Keep, Recycle, Permanent }
impl DeleteChoice {
    pub fn resolve(self, global: DeleteMode) -> DeleteMode {
        match self { Self::Global => global, Self::Keep => DeleteMode::Keep,
            Self::Recycle => DeleteMode::Recycle, Self::Permanent => DeleteMode::Permanent }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeepPolicy { Newest, Oldest, Largest, Smallest, ShortestName }
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy { Ask, Overwrite, Skip, Newest, Largest, KeepBoth }
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClassifyMode { Off, Extension, Category, Date, Custom }
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DuplicateAction { Delete, Hardlink }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub recursive: bool,
    pub include_hidden: bool,
    pub include_system: bool,
    pub exclusions: String,
    pub extract: bool,
    pub nested_archives: bool,
    pub archive_delete: DeleteChoice,
    /// 解压冲突策略。默认 Newest（与历史行为一致）：判定采用新文件时会删除/覆盖
    /// 已有文件，解压侧对每次此类替换写带策略名的明确警告日志；希望绝不覆盖的
    /// 用户应显式选择 Skip。
    pub extract_conflict: ConflictPolicy,
    pub max_depth: u32,
    /// 单包条目数上限。默认 100 万：足以覆盖正常压缩包，同时约束异常包的
    /// inode/内存放大；超大合法包可在高级设置中调高。
    pub max_entries: u64,
    /// 单包展开总量上限（GiB）。0 = 不额外限制。注意：无 Size 元数据的流式包
    /// （gzip/bzip2/xz 等）在 archive.rs 中另有内置硬顶（STREAM_UNPACKED_CAP_GIB），
    /// 本项只能收紧、不能放宽该硬顶。
    pub max_unpacked_gib: u64,
    pub max_file_gib: u64,
    pub max_ratio: u64,
    pub reserve_gib: u64,
    pub dedup_same_name: bool,
    pub dedup_copy_names: bool,
    pub dedup_other_names: bool,
    pub keep_duplicate: KeepPolicy,
    pub duplicate_action: DuplicateAction,
    pub duplicate_delete: DeleteChoice,
    pub same_name_same_size: bool,
    pub same_size_keep: KeepPolicy,
    pub same_name_different_size: bool,
    pub different_size_keep: KeepPolicy,
    /// 版本比较范围。true = 仅同目录（默认，最保守）；gui.rs 勾选「允许跨目录」时
    /// 会把此项写成 false（界面取反显示）。引擎 planner.rs：true → 按 lower(rel)
    /// 同目录分组，false → 按 lower(name) 全局按文件名分组。
    pub conflict_scope_directory: bool,
    pub conflict_delete: DeleteChoice,
    pub classify: ClassifyMode,
    pub output_dir: String,
    pub preserve_structure: bool,
    pub custom_categories: String,
    pub large_files: bool,
    pub large_threshold_gib: u64,
    pub merge_directories: bool,
    pub flatten_single_child: bool,
    pub clean_empty_dirs: bool,
    pub clean_junk: bool,
    pub clean_temp: bool,
    pub clean_zero: bool,
    pub cleanup_delete: DeleteChoice,
    pub clean_copy_name: bool,
    pub normalize_names: bool,
    pub detect_type: bool,
    pub fix_extension: bool,
    pub global_delete: DeleteMode,
    pub recycle_fallback: bool,
    pub hash_workers: usize,
    pub theme: String,
}
/// 默认的自定义分类规则串：界面用它判断「用户是否改过这一项」，
/// 避免在比较处重复构造同一份字面量。
pub const DEFAULT_CUSTOM_CATEGORIES:&str="文档=pdf,doc,docx,txt,md,xls,xlsx,ppt,pptx;图片=jpg,jpeg,png,webp;视频=mp4,mkv,avi,mov";
impl Default for Config {
    fn default() -> Self {
        Self {
            recursive: true, include_hidden: false, include_system: false,
            exclusions: ".git/**;node_modules/**;$RECYCLE.BIN/**;System Volume Information/**;.svn/**;.hg/**;.vs/**;.idea/**;AppData/**;ProgramData/**;Program Files/**;Program Files (x86)/**;Program Files (Arm)/**;Windows/**;Windows.old/**;$Windows.~BT/**;$Windows.~WS/**;WindowsApps/**;Packages/**;Recovery/**;PerfLogs/**;Config.Msi/**;SoftwareDistribution/**;Application Data/**;Local Settings/**;Temp/**;Tmp/**;Cookies/**;Recent/**;OneDrive/**".into(),
            extract: true, nested_archives: true, archive_delete: DeleteChoice::Global,
            // 默认 Newest 与历史行为一致；涉及删除时解压侧写带策略名的警告日志。
            extract_conflict: ConflictPolicy::Newest, max_depth: 16,
            // 100 万条目足够覆盖正常压缩包，同时约束异常包的条目放大；超大合法包可调高。
            max_entries: 1_000_000,
            // 0 = 不额外限制。无 Size 元数据的流式包在 archive.rs 另有内置 50 GiB 硬顶。
            max_unpacked_gib: 0, max_file_gib: 0, max_ratio: 10_000, reserve_gib: 1,
            dedup_same_name: true, dedup_copy_names: true, dedup_other_names: true,
            keep_duplicate: KeepPolicy::Newest, duplicate_action: DuplicateAction::Delete,
            duplicate_delete: DeleteChoice::Global,
            // 版本淘汰（同名但内容不同）默认关闭：它按名称启发式删除文件，与
            // 「只删已证实重复内容」的去重规则不同，必须由用户显式开启。
            same_name_same_size: false, same_size_keep: KeepPolicy::Newest,
            same_name_different_size: false, different_size_keep: KeepPolicy::Newest,
            conflict_scope_directory: true, conflict_delete: DeleteChoice::Global,
            classify: ClassifyMode::Category, output_dir: String::new(), preserve_structure: true,
            custom_categories: DEFAULT_CUSTOM_CATEGORIES.into(),
            large_files: false, large_threshold_gib: 1, merge_directories: false,
            flatten_single_child: false, clean_empty_dirs: true, clean_junk: true,
            clean_temp: false, clean_zero: false, cleanup_delete: DeleteChoice::Global,
            clean_copy_name: true, normalize_names: true, detect_type: false, fix_extension: false,
            global_delete: DeleteMode::Recycle, recycle_fallback: true, hash_workers: 2,
            theme: "system".into(),
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        if !(1..=16).contains(&self.hash_workers) { bail!("Hash 工作线程必须在 1～16 之间"); }
        if !(1..=64).contains(&self.max_depth) { bail!("嵌套层数必须在 1～64 之间"); }
        if self.max_entries == 0 { bail!("压缩包条目上限不能为 0"); }
        for number in [self.max_unpacked_gib, self.max_file_gib, self.reserve_gib, self.large_threshold_gib] {
            number.checked_mul(1 << 30).context("容量设置超出范围")?;
        }
        if !self.output_dir.is_empty() { crate::fsutil::validate_component(&self.output_dir)?; }
        if self.fix_extension && !self.detect_type { bail!("修正扩展名需要先开启真实类型检测"); }
        if !["system", "light", "dark"].contains(&self.theme.as_str()) { bail!("主题参数无效"); }
        crate::rules::build_exclusions(&self.exclusions)?;
        crate::rules::parse_categories(&self.custom_categories)?;
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path)?;
        if meta.len() > 256 * 1024 { bail!("配置文件超过 256 KiB"); }
        let value = Self::from_json_text(&String::from_utf8(std::fs::read(path)?).context("配置文件不是 UTF-8")?)?;
        value.validate()?;
        Ok(value)
    }
    /// 历史版本已删除的设置键：旧配置文件与旧任务库仍带着它们，反序列化前剥除，
    /// 否则 deny_unknown_fields 会把旧数据整体判成非法配置。
    /// verify_bytes（删除前逐字节复核）已写死为始终开启。
    const REMOVED_FIELDS: &[&str] = &["hash_algorithm", "verify_bytes"];
    pub fn from_json_text(text: &str) -> Result<Self> {
        // 某些编辑器会写出带 UTF-8 BOM 的文件；serde_json 不接受，解析前剥掉。
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        let mut value: serde_json::Value = serde_json::from_str(text)?;
        if let Some(map) = value.as_object_mut() {
            for key in Self::REMOVED_FIELDS { map.remove(*key); }
        }
        Ok(serde_json::from_value(value)?)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        crate::fsutil::write_json_atomic(path, self)
    }
    pub fn set_json(&mut self, key: &str, value: serde_json::Value) -> Result<()> {
        let mut data = serde_json::to_value(&*self)?;
        let map = data.as_object_mut().context("配置不是对象")?;
        if !map.contains_key(key) { bail!("未知设置：{key}"); }
        map.insert(key.to_string(), value);
        *self = serde_json::from_value(data)?;
        Ok(())
    }
    pub fn destructive_warning(&self) -> String {
        fn label(mode: DeleteMode) -> &'static str { match mode { DeleteMode::Keep=>"保留",DeleteMode::Recycle=>"回收站",DeleteMode::Permanent=>"永久删除" } }
        format!("原压缩包：{}；重复文件：{}；冲突文件：{}；清理文件：{}。\n回收失败后永久删除：{}（用户取消不会触发降级）。没有自动回滚；请确认目录和规则。",
            label(self.archive_delete.resolve(self.global_delete)),label(self.duplicate_delete.resolve(self.global_delete)),
            label(self.conflict_delete.resolve(self.global_delete)),label(self.cleanup_delete.resolve(self.global_delete)),
            if self.recycle_fallback { "已开启" } else { "已关闭" })
    }
}
pub fn state_dir() -> Result<PathBuf> {
    let dirs = directories_next::ProjectDirs::from("", "", "JchTools").context("无法确定用户数据目录")?;
    Ok(dirs.data_local_dir().to_path_buf())
}
