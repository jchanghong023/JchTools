use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeleteMode {
    Keep,
    Permanent,
}
/// 各类功能的删除方式覆盖（S-02：只在「保留」与「永久删除」之间选择，无回收站选项）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeleteChoice {
    Global,
    Keep,
    Permanent,
}
/// 递归解压工具的原包处置（X-05/R-02）：只属于解压工具，默认直接永久删除，
/// 不提供「跟随全局」（成功解包的结果不依赖其它工具的全局删除口径）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveDispose {
    Keep,
    Permanent,
}
impl ArchiveDispose {
    pub fn resolve(self) -> DeleteMode {
        match self {
            Self::Keep => DeleteMode::Keep,
            Self::Permanent => DeleteMode::Permanent,
        }
    }
}
impl DeleteChoice {
    pub fn resolve(self, global: DeleteMode) -> DeleteMode {
        match self {
            Self::Global => global,
            Self::Keep => DeleteMode::Keep,
            Self::Permanent => DeleteMode::Permanent,
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeepPolicy {
    Newest,
    Oldest,
    Largest,
    Smallest,
    ShortestName,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    Ask,
    Overwrite,
    Skip,
    Newest,
    Largest,
    KeepBoth,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClassifyMode {
    Off,
    Extension,
    Category,
    Date,
    Custom,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DuplicateAction {
    Delete,
    Hardlink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub recursive: bool,
    pub include_hidden: bool,
    pub include_system: bool,
    pub exclusions: String,
    pub nested_archives: bool,
    pub archive_delete: ArchiveDispose,
    /// 解压冲突策略。默认 Newest（与历史行为一致）：判定采用新文件时会删除/覆盖
    /// 已有文件，解压侧对每次此类替换写带策略名的明确警告日志；希望绝不覆盖的
    /// 用户应显式选择 Skip。
    pub extract_conflict: ConflictPolicy,
    pub max_depth: u32,
    /// 单包条目数上限。默认 100 万：足以覆盖正常压缩包，同时约束异常包的
    /// inode/内存放大；超大合法包可在高级设置中调高。
    pub max_entries: u64,
    /// 单包展开总量上限（GiB）。0 = 不额外限制（R-02）；对流式包（gzip/bzip2/xz 等
    /// 无 Size 元数据格式）同样按本项执行，不再有额外内置硬顶。
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
    /// 解压覆盖旧文件时的删除方式（X-04：淘汰旧文件必须走用户选择的删除策略）。
    /// 同名但内容不同的「版本取舍」已按 C-02 禁止并移除，本字段只服务解压冲突覆盖。
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
    pub hash_workers: usize,
    pub theme: String,
}
/// 默认的自定义分类规则串：界面用它判断「用户是否改过这一项」，
/// 避免在比较处重复构造同一份字面量。
pub const DEFAULT_CUSTOM_CATEGORIES: &str =
    "文档=pdf,doc,docx,txt,md,xls,xlsx,ppt,pptx;图片=jpg,jpeg,png,webp;视频=mp4,mkv,avi,mov";
impl Default for Config {
    fn default() -> Self {
        Self {
            recursive: true, include_hidden: false, include_system: false,
            exclusions: ".git/**;node_modules/**;$RECYCLE.BIN/**;System Volume Information/**;.svn/**;.hg/**;.vs/**;.idea/**;AppData/**;ProgramData/**;Program Files/**;Program Files (x86)/**;Program Files (Arm)/**;Windows/**;Windows.old/**;$Windows.~BT/**;$Windows.~WS/**;WindowsApps/**;Packages/**;Recovery/**;PerfLogs/**;Config.Msi/**;SoftwareDistribution/**;Application Data/**;Local Settings/**;Temp/**;Tmp/**;Cookies/**;Recent/**;OneDrive/**".into(),
            nested_archives: true, archive_delete: ArchiveDispose::Permanent,
            // 默认 Newest 与历史行为一致；涉及删除时解压侧写带策略名的警告日志。
            extract_conflict: ConflictPolicy::Newest, max_depth: 16,
            // 100 万条目足够覆盖正常压缩包，同时约束异常包的条目放大；超大合法包可调高。
            max_entries: 1_000_000,
            // 0 = 不额外限制。流式包（无 Size 元数据）在解压期间按本项累计检查（archive.rs）。
            max_unpacked_gib: 0, max_file_gib: 0, max_ratio: 10_000, reserve_gib: 1,
            dedup_same_name: true, dedup_copy_names: true, dedup_other_names: false,
            keep_duplicate: KeepPolicy::Newest, duplicate_action: DuplicateAction::Delete,
            duplicate_delete: DeleteChoice::Global,
            conflict_delete: DeleteChoice::Global,
            classify: ClassifyMode::Category, output_dir: String::new(), preserve_structure: true,
            custom_categories: DEFAULT_CUSTOM_CATEGORIES.into(),
            large_files: false, large_threshold_gib: 1, merge_directories: false,
            flatten_single_child: false, clean_empty_dirs: true, clean_junk: true,
            clean_temp: false, clean_zero: false, cleanup_delete: DeleteChoice::Global,
            clean_copy_name: true, normalize_names: true, detect_type: false, fix_extension: false,
            global_delete: DeleteMode::Permanent, hash_workers: 6,
            theme: "system".into(),
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        if !(1..=16).contains(&self.hash_workers) {
            bail!("Hash 工作线程必须在 1～16 之间");
        }
        if !(1..=64).contains(&self.max_depth) {
            bail!("嵌套层数必须在 1～64 之间");
        }
        if self.max_entries == 0 {
            bail!("压缩包条目上限不能为 0");
        }
        for number in [
            self.max_unpacked_gib,
            self.max_file_gib,
            self.reserve_gib,
            self.large_threshold_gib,
        ] {
            number.checked_mul(1 << 30).context("容量设置超出范围")?;
        }
        if !self.output_dir.is_empty() {
            crate::fsutil::validate_component(&self.output_dir)?;
        }
        if self.fix_extension && !self.detect_type {
            bail!("修正扩展名需要先开启真实类型检测");
        }
        if !["system", "light", "dark"].contains(&self.theme.as_str()) {
            bail!("主题参数无效");
        }
        crate::rules::build_exclusions(&self.exclusions)?;
        crate::rules::parse_categories(&self.custom_categories)?;
        Ok(())
    }
    /// 规则仅会话内生效（R-01：不落盘、不导入导出）；配置只随任务库序列化保存，
    /// 不再提供独立的配置文件读写（P-06 移除 CLI 后无任何产品侧调用方）。
    /// 历史版本已删除的设置键：旧配置与旧任务库仍带着它们，反序列化前剥除，
    /// 否则 deny_unknown_fields 会把旧数据整体判成非法配置。
    /// verify_bytes（删除前逐字节复核）已写死为始终开启；
    /// 5 个 same_name_*/conflict_scope 键是已按 C-02 禁止移除的同名版本取舍开关；
    /// extract 键随两工具拆分移除（解压职责整体移交「递归解压」工具，X-01）。
    const REMOVED_FIELDS: &[&str] = &[
        "hash_algorithm",
        "verify_bytes",
        "recycle_fallback",
        "same_name_same_size",
        "same_size_keep",
        "same_name_different_size",
        "different_size_keep",
        "conflict_scope_directory",
        "extract",
    ];
    /// 旧任务库可能保存着已移除的「回收站」取值。S-02 之后删除方式只在「保留」与
    /// 「永久删除」之间选择，历史值按「永久删除」读取，否则旧任务库会因枚举缺项
    /// 整体判成非法配置而无法打开。
    const DELETE_MODE_KEYS: &[&str] = &[
        "global_delete",
        "duplicate_delete",
        "cleanup_delete",
        "conflict_delete",
        "archive_delete",
    ];
    pub fn from_json_text(text: &str) -> Result<Self> {
        // 某些编辑器会写出带 UTF-8 BOM 的文件；serde_json 不接受，解析前剥掉。
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        let mut value: serde_json::Value = serde_json::from_str(text)?;
        if let Some(map) = value.as_object_mut() {
            for key in Self::REMOVED_FIELDS {
                map.remove(*key);
            }
            for key in Self::DELETE_MODE_KEYS {
                if map.get(*key).and_then(|v| v.as_str()) == Some("recycle") {
                    map.insert(
                        (*key).to_string(),
                        serde_json::Value::String("permanent".into()),
                    );
                }
            }
        }
        Ok(serde_json::from_value(value)?)
    }
    pub fn set_json(&mut self, key: &str, value: serde_json::Value) -> Result<()> {
        let mut data = serde_json::to_value(&*self)?;
        let map = data.as_object_mut().context("配置不是对象")?;
        if !map.contains_key(key) {
            bail!("未知设置：{key}");
        }
        map.insert(key.to_string(), value);
        *self = serde_json::from_value(data)?;
        Ok(())
    }
    /// 二次确认框的破坏性说明（S-02：删除一律为永久删除、不可由本软件恢复，
    /// 任务确认时必须明确告知）。
    pub fn destructive_warning(&self) -> String {
        fn label(mode: DeleteMode) -> &'static str {
            match mode {
                DeleteMode::Keep => "保留",
                DeleteMode::Permanent => "永久删除",
            }
        }
        format!("原压缩包：{}；重复文件：{}；解压覆盖旧文件：{}；清理文件：{}。\n删除一律为永久删除，不经回收站、不可由本软件恢复；用户取消不会触发任何删除。没有自动回滚；请确认目录和规则。",
            label(self.archive_delete.resolve()),label(self.duplicate_delete.resolve(self.global_delete)),
            label(self.conflict_delete.resolve(self.global_delete)),label(self.cleanup_delete.resolve(self.global_delete)))
    }
}
pub fn state_dir() -> Result<PathBuf> {
    let dirs =
        directories_next::ProjectDirs::from("", "", "JchTools").context("无法确定用户数据目录")?;
    Ok(dirs.data_local_dir().to_path_buf())
}
