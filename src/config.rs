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
pub enum HashAlgorithm { Blake3, Sha256, Md5 }
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
    pub extract_conflict: ConflictPolicy,
    pub max_depth: u32,
    pub max_entries: u64,
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
    pub hash_algorithm: HashAlgorithm,
    pub verify_bytes: bool,
    pub same_name_same_size: bool,
    pub same_size_keep: KeepPolicy,
    pub same_name_different_size: bool,
    pub different_size_keep: KeepPolicy,
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
impl Default for Config {
    fn default() -> Self {
        Self {
            recursive: true, include_hidden: false, include_system: false,
            exclusions: ".git/**;node_modules/**;$RECYCLE.BIN/**;System Volume Information/**;.svn/**;.hg/**;.vs/**;.idea/**;AppData/**;ProgramData/**;Program Files/**;Program Files (x86)/**;Program Files (Arm)/**;Windows/**;Windows.old/**;$Windows.~BT/**;$Windows.~WS/**;WindowsApps/**;Packages/**;Recovery/**;PerfLogs/**;Config.Msi/**;SoftwareDistribution/**;Application Data/**;Local Settings/**;Temp/**;Tmp/**;Cookies/**;Recent/**;OneDrive/**".into(),
            extract: true, nested_archives: true, archive_delete: DeleteChoice::Global,
            extract_conflict: ConflictPolicy::Newest, max_depth: 16, max_entries: 10_000_000,
            max_unpacked_gib: 0, max_file_gib: 0, max_ratio: 10_000, reserve_gib: 1,
            dedup_same_name: true, dedup_copy_names: true, dedup_other_names: true,
            keep_duplicate: KeepPolicy::Newest, duplicate_action: DuplicateAction::Delete,
            duplicate_delete: DeleteChoice::Global, hash_algorithm: HashAlgorithm::Blake3,
            verify_bytes: true, same_name_same_size: true, same_size_keep: KeepPolicy::Newest,
            same_name_different_size: true, different_size_keep: KeepPolicy::Newest,
            conflict_scope_directory: true, conflict_delete: DeleteChoice::Global,
            classify: ClassifyMode::Category, output_dir: String::new(), preserve_structure: true,
            custom_categories: "文档=pdf,doc,docx,txt,md,xls,xlsx,ppt,pptx;图片=jpg,jpeg,png,webp;视频=mp4,mkv,avi,mov".into(),
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
        let value: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        value.validate()?;
        Ok(value)
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
