use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeleteMode {
    Keep,
    Permanent,
}
/// 各类功能的删除方式覆盖（S-02：只在「保留」与「永久删除」之间选择，无回收站选项；
/// 默认 Global 表示跟随「全局默认删除方式」）。各清理类别各自独立覆盖，互不影响。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeleteChoice {
    Global,
    Keep,
    Permanent,
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
/// C-03 保留者策略：只有最新（mtime 降序）/最旧（mtime 升序）/最短名称三种；
/// 不提供最大/最小（内容相同的文件大小相同，R-02）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeepPolicy {
    Newest,
    Oldest,
    ShortestName,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub recursive: bool,
    pub include_hidden: bool,
    pub include_system: bool,
    pub exclusions: String,
    pub max_depth: u32,
    /// 单包条目数上限（X-08）。默认 100 万；不允许 0 关闭。
    pub max_entries: u64,
    /// 最大展开比例（X-08）：解码输出累计逻辑字节 ÷ 输入卷集合总字节，默认 10000，0 关闭。
    pub max_ratio: u64,
    /// 磁盘预留字节数（X-08）：可用空间 − 预计新增占用 ≥ 预留；0 不取消磁盘写入失败处理。
    pub reserve_bytes: u64,
    pub dedup_same_name: bool,
    pub dedup_copy_names: bool,
    pub dedup_other_names: bool,
    pub keep_duplicate: KeepPolicy,
    /// C-06 大文件单独归类：命中文件优先进入「大文件」大类（大文件/功能分类）。
    pub large_files: bool,
    /// C-06 大文件阈值（字节），≥ 阈值命中；≥1。
    pub large_threshold_bytes: u64,
    pub clean_junk: bool,
    /// 系统附属文件清理的删除方式（C-08：涉及删除的清理项各自独立覆盖，默认跟随全局）。
    pub junk_delete: DeleteChoice,
    pub clean_temp: bool,
    /// 临时与备份文件清理的删除方式（C-08 独立覆盖）。
    pub temp_delete: DeleteChoice,
    pub clean_zero: bool,
    /// 零字节文件清理的删除方式（C-08 独立覆盖）。
    pub zero_delete: DeleteChoice,
    /// C-08 副本后缀清理：末尾 `(N)` 转为 `_N`，`- Copy` / `副本` 标记移除。
    pub clean_copy_name: bool,
    /// C-08 NFC 与连续空白规范化（同一开关同时启停两项）。
    pub normalize_names: bool,
    /// 影子键：与 fix_extension 同值（规则面板一行驱动；保留是为 static_check 的
    /// config_schema 互锁 hidden 集合，引擎读取一律以 fix_extension 为准）。
    pub detect_type: bool,
    /// C-08 按内容签名修正错误扩展名（默认关；判定边界见附录 B）。
    pub fix_extension: bool,
    pub global_delete: DeleteMode,
    pub hash_workers: usize,
    pub theme: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            recursive: true,
            include_hidden: true,
            include_system: true,
            // S-04：默认覆盖所选目录内的全部资料（含隐藏与系统属性），不因普通目录名自动漏处理；
            // 排除规则默认留空，只有用户显式填写才缩小范围（Git 排除不受本项与开关影响）。
            exclusions: String::new(),
            max_depth: 16,
            max_entries: 1_000_000,
            max_ratio: 10_000,
            reserve_bytes: 1024 * 1024 * 1024,
            dedup_same_name: true,
            dedup_copy_names: true,
            dedup_other_names: false,
            keep_duplicate: KeepPolicy::Newest,
            large_files: false,
            large_threshold_bytes: 1024 * 1024 * 1024,
            clean_junk: true,
            junk_delete: DeleteChoice::Global,
            clean_temp: false,
            temp_delete: DeleteChoice::Global,
            clean_zero: false,
            zero_delete: DeleteChoice::Global,
            clean_copy_name: true,
            normalize_names: true,
            detect_type: false,
            fix_extension: false,
            global_delete: DeleteMode::Permanent,
            hash_workers: 6,
            theme: "system".into(),
        }
    }
}
impl Config {
    /// 附录 D 的数字范围校验：非法值、溢出或超范围一律报错，不自动截断或钳制。
    pub fn validate(&self) -> Result<()> {
        if !(1..=16).contains(&self.hash_workers) {
            bail!("Hash 工作线程必须在 1～16 之间");
        }
        if !(1..=64).contains(&self.max_depth) {
            bail!("嵌套层数必须在 1～64 之间");
        }
        if self.max_entries == 0 || self.max_entries > 1_000_000_000 {
            bail!("压缩包条目上限必须在 1～1000000000 之间（不允许 0）");
        }
        if self.max_ratio > 1_000_000_000 {
            bail!("最大展开比例必须在 0～1000000000 之间（0 为关闭）");
        }
        i64::try_from(self.reserve_bytes).context("磁盘预留超出范围")?;
        if self.large_threshold_bytes == 0 || i64::try_from(self.large_threshold_bytes).is_err() {
            bail!("大文件阈值必须至少 1 字节且不超出 64 位范围");
        }
        if !["system", "light", "dark"].contains(&self.theme.as_str()) {
            bail!("主题参数无效");
        }
        crate::rules::build_exclusions(&self.exclusions)?;
        Ok(())
    }
    /// 规则仅会话内生效（R-01：不落盘、不导入导出）；配置只随任务库序列化保存，
    /// 不再提供独立的配置文件读写（P-06 移除 CLI 后无任何产品侧调用方）。
    /// 历史版本已删除的设置键：旧配置与旧任务库仍带着它们，反序列化前剥除，
    /// 否则 deny_unknown_fields 会把旧数据整体判成非法配置。
    /// verify_bytes（删除前逐字节复核）已随 S-03 整体移除；same_name_* / conflict_scope
    /// 是已按 C-02 禁止移除的同名版本取舍开关；extract 键随两工具拆分移除（X-01）。
    /// 2026-09 本轮移除：classify / output_dir / preserve_structure / custom_categories /
    /// merge_directories / flatten_single_child / duplicate_action / duplicate_delete /
    /// max_unpacked_gib / max_file_gib（X-08 不再设单包/单文件体积上限）与
    /// large_threshold_gib / reserve_gib（容量项统一改为字节；旧 GiB 值不换算，
    /// 按默认值重新开始，R-04 规则变化后必须重新分析）。
    /// 注意：nested_archives / archive_delete / extract_conflict / conflict_delete /
    /// clean_empty_dirs 是 H-07/X-04/X-05/R-02 的显式切割，历史上就不在兼容剥除清单。
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
        "classify",
        "output_dir",
        "preserve_structure",
        "custom_categories",
        "merge_directories",
        "flatten_single_child",
        "duplicate_action",
        "duplicate_delete",
        "max_unpacked_gib",
        "max_file_gib",
        "large_threshold_gib",
        "reserve_gib",
    ];
    /// 旧任务库可能保存着已移除的「回收站」取值。S-02 之后删除方式只在「保留」与
    /// 「永久删除」之间选择，历史值按「永久删除」读取，否则旧任务库会因枚举缺项
    /// 整体判成非法配置而无法打开。
    const DELETE_MODE_KEYS: &[&str] =
        &["global_delete", "junk_delete", "temp_delete", "zero_delete"];
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
        let mut config: Self = serde_json::from_value(value)?;
        // 影子键与 fix_extension 恒同值（面板一个开关驱动两字段）。
        config.detect_type = config.fix_extension;
        Ok(config)
    }
    pub fn set_json(&mut self, key: &str, value: serde_json::Value) -> Result<()> {
        let mut data = serde_json::to_value(&*self)?;
        let map = data.as_object_mut().context("配置不是对象")?;
        if !map.contains_key(key) {
            bail!("未知设置：{key}");
        }
        map.insert(key.to_string(), value);
        *self = serde_json::from_value(data)?;
        self.detect_type = self.fix_extension;
        Ok(())
    }
    /// 二次确认框的破坏性说明（S-02：删除一律为永久删除、不可由本软件恢复，任务确认时
    /// 必须明确告知）。按 C-08 逐项如实描述：关闭的清理类别显示「不清理」，删除方式按
    /// 各自覆盖解析；空目录清理不可关闭（H-05/C-07），固定永久删除；C-04 副本处置随
    /// 全局文件删除方式；C-14 Git 项目整体归类是固定移动（不改写项目内部）。
    pub fn destructive_warning(&self) -> String {
        fn label(mode: DeleteMode) -> &'static str {
            match mode {
                DeleteMode::Keep => "保留",
                DeleteMode::Permanent => "永久删除",
            }
        }
        fn category(enabled: bool, choice: DeleteChoice, global: DeleteMode) -> &'static str {
            if !enabled {
                return "不清理";
            }
            label(choice.resolve(global))
        }
        format!(
            "重复副本：{}；系统附属文件：{}；临时与备份文件：{}；零字节文件：{}；空目录：永久删除（不可关闭）；Git 项目：整体移入「Git项目集合」（固定行为）。\n删除一律为永久删除，不经回收站、不可由本软件恢复；用户取消不会触发任何删除。没有自动回滚；请确认目录和规则。",
            label(self.global_delete),
            category(self.clean_junk, self.junk_delete, self.global_delete),
            category(self.clean_temp, self.temp_delete, self.global_delete),
            category(self.clean_zero, self.zero_delete, self.global_delete),
        )
    }
}
pub fn state_dir() -> Result<PathBuf> {
    let dirs =
        directories_next::ProjectDirs::from("", "", "JchTools").context("无法确定用户数据目录")?;
    Ok(dirs.data_local_dir().to_path_buf())
}
