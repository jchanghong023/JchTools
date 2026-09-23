//! C-05 功能分类：只用「文件名 + 整理时当前可见的紧邻原父目录名」的确定性统计聚类。
//! 纯本地、纯 CPU、离线（P-03）：不读取文件正文、不联网、不使用任何模型。
//!
//! 管线（合同 C-05「特征构造 / 候选与评分 / 聚类 / 数量上限与兜底」）：
//! 文件名（去扩展名）+ 父目录名 → NFKC 与噪声剔除 → token（英文词/缩写 + 4-gram、
//! 中文 2/3-gram）→ 全局文档频率加权（越常见越低；原文大写缩写设权重下限，见
//! `ACRONYM_WEIGHT_FLOOR`）→ 倒排索引产生候选对（稀有优先、受预算约束）
//! → 综合相似度（加权 Dice ≈55% + 整串相似度 ≈25% + 父目录相似度 ≈20%）
//! → 连边（高阈值；文件名或父目录共享强 token 时中阈值）
//! → 并查集聚类（中阈值只允许单文件并入，防链式误聚类）
//! → 组内公共短语命名（含子串变体覆盖，AAMBA 计入 AMBA）
//! → 每大类至多 10 个功能目录 + 固定兜底「其他」。
//!
//! 确定性：所有影响输出的迭代按稳定序（字符串 / 覆盖数 / 分数 + 字典序决胜），
//! 相似度评分按候选对索引并行回收；相同输入集合与相同在位目录集合必然得到相同结果，
//! 不受传入顺序、哈希表顺序或 Rayon 调度影响。

use crate::{fsutil, rules};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use unicode_normalization::UnicodeNormalization;

/// 固定兜底功能目录（C-05）：收纳无法可靠分类的文件，不计入 10 个上限。
pub const FALLBACK_DIR: &str = "其他";
/// 每个大类下自动生成的功能分类目录上限（C-05）。
pub const MAX_FUNCTIONAL_DIRS: usize = 10;
/// 候选对全局预算：倒排索引按稀有 token 优先累计，超出预算的更高频 token 不再生成候选。
const MAX_CANDIDATE_PAIRS: u64 = 4_000_000;
/// 高置信度连边阈值（C-05「连边本身使用较高阈值」）。
const EDGE_STRONG: f64 = 0.60;
/// 共享强 token 时的中阈值（宁可少合并，不要错误合并）：强 token 本身是语义门槛，
/// 分数下限只排除其余部分完全无关的对。
const EDGE_MEDIUM: f64 = 0.18;
/// token「强」的全局区分度门槛（平滑 idf）；原文大写缩写不设该门槛（天然强特征）。
const STRONG_IDF: f64 = 4.0;
/// 原文大写缩写的权重下限：技术缩写即使在本集合内常见（如整组都关于 MBIST），
/// 仍保持高信息量——纯 idf 会把它压到接近零，导致「整组同主题」反而无法成组。
const ACRONYM_WEIGHT_FLOOR: f64 = 1.5;
const NAME_DICE_WEIGHT: f64 = 0.55;
const STRING_WEIGHT: f64 = 0.25;
const PARENT_WEIGHT: f64 = 0.20;
/// 功能目录名的 UTF-16 单元上限（C-05：名称简短、可读）。
const NAME_UNIT_LIMIT: usize = 24;
/// 短语进入命名的覆盖率门槛分子：≥3/4 成员（见 name_group 的整数实现），且至少 2 个成员。
const PHRASE_COVERAGE_NUM: usize = 3;
const PHRASE_COVERAGE_DEN: usize = 4;
/// 第二命名片段的得分比例门槛：与首选得分相当（且覆盖数相同）才并列（AMBA_APB）。
const SECOND_PHRASE_RATIO: f64 = 0.80;

/// 文档频率高于 `files/8`（且集合 >64 个文件）的 token 不作为候选生成依据
/// （区分度不足，仍参与评分与命名）。小集合不设该过滤。
fn df_cap(files: usize) -> u64 {
    if files <= 64 {
        return u64::MAX;
    }
    (files as u64) / 8
}

/// 分类输入：一个大类内的存活文件。
#[derive(Clone, Debug)]
pub struct FuncFile {
    pub id: i64,
    /// 相对所选根的完整路径（`/` 分隔）；仅用于稳定排序与结果关联，不参与特征。
    pub rel: String,
    /// 当前文件名（含扩展名；扩展名在入口处剥离——扩展名是大类信息，不是功能信息）。
    pub name: String,
    /// 紧邻原父目录名；根下文件为空串。
    pub parent: String,
}

/// 低价值噪声词（C-05 特征构造）：高频低区分度或纯语法助词，从分类特征中剔除。
/// 只影响特征，不改实际文件名。
const STOPWORDS_ASCII: &[&str] = &["final", "copy"];
const STOPWORDS_CJK: &[&str] = &[
    "副本",
    "最终版",
    "最新版",
    "版本",
    "文档",
    "资料",
    "说明",
    "介绍",
    "流程",
    "最终",
    "其他",
    "文件",
    "的",
    "与",
    "及",
    "和",
    "或",
];

fn is_cjk(ch: char) -> bool {
    matches!(ch,
        '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'
        | '\u{3040}'..='\u{30FF}' | '\u{20000}'..='\u{2A6DF}')
}

fn is_ascii_word(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_alphabetic())
}

/// 按 ASCII 字母段内的大小写切换拆驼峰；全大写且长度 ≥2 的段是缩写（强特征）。
/// `JTAG2APB` 先在字母数字边界分成 `JTAG`、`APB` 两段（数字段为噪声丢弃）。
fn split_ascii_word(run: &[char]) -> Vec<(String, bool)> {
    let mut humps: Vec<Vec<char>> = Vec::new();
    for (i, &ch) in run.iter().enumerate() {
        let boundary = i > 0
            && ch.is_uppercase()
            && (run[i - 1].is_lowercase()
                || run.get(i + 1).is_some_and(|next| next.is_lowercase()));
        if boundary || humps.is_empty() {
            humps.push(Vec::new());
        }
        if let Some(hump) = humps.last_mut() {
            hump.push(ch);
        }
    }
    humps
        .into_iter()
        .map(|chars| {
            let acronym = chars.len() >= 2 && chars.iter().all(|c| c.is_uppercase());
            (
                chars.into_iter().collect::<String>().to_lowercase(),
                acronym,
            )
        })
        .collect()
}

/// 把中文段按停用词切成子段：停用词位置是短语边界（防「版本/资料」把无关字连成短语）。
fn split_cjk_segment(chars: &[char]) -> Vec<Vec<char>> {
    let mut keep: Vec<bool> = vec![true; chars.len()];
    for word in STOPWORDS_CJK {
        let pattern: Vec<char> = word.chars().collect();
        if pattern.is_empty() || pattern.len() > chars.len() {
            continue;
        }
        for start in 0..=(chars.len() - pattern.len()) {
            if chars[start..start + pattern.len()] == pattern[..] {
                for slot in keep.iter_mut().skip(start).take(pattern.len()) {
                    *slot = false;
                }
            }
        }
    }
    let mut segments: Vec<Vec<char>> = Vec::new();
    let mut current: Vec<char> = Vec::new();
    for (i, &ch) in chars.iter().enumerate() {
        if keep[i] {
            current.push(ch);
        } else if !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// 一个规范化分段（顺序保留）：ascii 词或中文子段。
enum Piece {
    Word(String, bool),
    Cjk(Vec<char>),
}

/// 特征规范化：NFKC → 大小写折叠 → 分段；纯数字段（日期/流水号/版本号/重名后缀）
/// 与停用词剔除；分隔符（含标点、空白、括号）是分段边界。
fn pieces_of(raw: &str) -> Vec<Piece> {
    let normalized: String = raw.nfkc().collect();
    let chars: Vec<char> = normalized.chars().collect();
    let mut pieces: Vec<Piece> = Vec::new();
    let mut letters: Vec<char> = Vec::new();
    let mut cjk: Vec<char> = Vec::new();
    fn flush_letters(letters: &mut Vec<char>, pieces: &mut Vec<Piece>) {
        if letters.is_empty() {
            return;
        }
        for (word, acronym) in split_ascii_word(letters) {
            if word.len() >= 2 && !STOPWORDS_ASCII.contains(&word.as_str()) {
                pieces.push(Piece::Word(word, acronym));
            }
        }
        letters.clear();
    }
    fn flush_cjk(cjk: &mut Vec<char>, pieces: &mut Vec<Piece>) {
        if cjk.is_empty() {
            return;
        }
        for segment in split_cjk_segment(cjk) {
            pieces.push(Piece::Cjk(segment));
        }
        cjk.clear();
    }
    for &ch in &chars {
        if ch.is_ascii_alphabetic() {
            flush_cjk(&mut cjk, &mut pieces);
            letters.push(ch);
        } else if ch.is_ascii_digit() {
            // 纯数字段（日期、流水号、版本号、自动重名后缀）整体丢弃。
            flush_letters(&mut letters, &mut pieces);
            flush_cjk(&mut cjk, &mut pieces);
        } else if is_cjk(ch) {
            flush_letters(&mut letters, &mut pieces);
            cjk.push(ch);
        } else {
            flush_letters(&mut letters, &mut pieces);
            flush_cjk(&mut cjk, &mut pieces);
        }
    }
    flush_letters(&mut letters, &mut pieces);
    flush_cjk(&mut cjk, &mut pieces);
    pieces
}

/// 串表：字符串 → 稳定 id。id 只作键；一切对外排序按字符串本身，输入先按 rel 稳定排序，
/// 因此相同输入集合得到相同 id 分配。
struct Interner {
    map: BTreeMap<String, u32>,
    names: Vec<String>,
}
impl Interner {
    fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            names: Vec::new(),
        }
    }
    // 串表条目数受内存约束远小于 u32 上限（每条目一个字符串）；截断不可能发生。
    #[allow(clippy::cast_possible_truncation)]
    fn intern(&mut self, value: &str) -> u32 {
        if let Some(id) = self.map.get(value) {
            return *id;
        }
        let id = self.names.len() as u32;
        self.names.push(value.to_string());
        self.map.insert(value.to_string(), id);
        id
    }
    fn text(&self, id: u32) -> &str {
        self.names.get(id as usize).map_or("", String::as_str)
    }
}

/// 一个字符串（文件名主体或父目录名）的原始特征。
struct RawPart {
    /// 相似度 token（英文词 + 4-gram、中文 2/3-gram；不含单个中文字）。
    tokens: BTreeSet<u32>,
    /// 命名单元分段（ascii 词为 1 单元；中文按单字，分段即短语边界）。
    runs: Vec<Vec<u32>>,
    /// 该部分以原文大写缩写形式写出的 token。
    acronyms: BTreeSet<u32>,
    /// ascii 词的 3～6 字符子串（覆盖 AAMBA 对 AMBA 一类拼写变体的命名覆盖率）。
    substrings: BTreeSet<u32>,
    /// 噪声剔除后的规范化串（整串相似度用）。
    text: String,
}

fn extract_part(raw: &str, interner: &mut Interner) -> RawPart {
    let pieces = pieces_of(raw);
    let mut tokens = BTreeSet::new();
    let mut acronyms = BTreeSet::new();
    let mut substrings = BTreeSet::new();
    let mut runs: Vec<Vec<u32>> = Vec::new();
    for piece in &pieces {
        match piece {
            Piece::Word(word, acronym) => {
                let id = interner.intern(word);
                tokens.insert(id);
                if *acronym {
                    acronyms.insert(id);
                }
                // 长词附带 4-gram（相似度）与 3～6 字符子串（命名覆盖）：识别拼写变体。
                let chars: Vec<char> = word.chars().collect();
                if chars.len() >= 5 {
                    for start in 0..=(chars.len() - 4) {
                        let gram: String = chars[start..start + 4].iter().collect();
                        tokens.insert(interner.intern(&gram));
                    }
                }
                for len in 3..=6 {
                    if chars.len() > len {
                        for start in 0..=(chars.len() - len) {
                            let part: String = chars[start..start + len].iter().collect();
                            substrings.insert(interner.intern(&part));
                        }
                    }
                }
                runs.push(vec![id]);
            }
            Piece::Cjk(segment) => {
                let ids: Vec<u32> = segment
                    .iter()
                    .map(|&c| interner.intern(&c.to_string()))
                    .collect();
                if segment.len() >= 2 {
                    for start in 0..=(segment.len() - 2) {
                        let gram: String = segment[start..start + 2].iter().collect();
                        tokens.insert(interner.intern(&gram));
                    }
                }
                if segment.len() >= 3 {
                    for start in 0..=(segment.len() - 3) {
                        let gram: String = segment[start..start + 3].iter().collect();
                        tokens.insert(interner.intern(&gram));
                    }
                }
                runs.push(ids);
            }
        }
    }
    let text = pieces
        .iter()
        .map(|piece| match piece {
            Piece::Word(word, _) => word.clone(),
            Piece::Cjk(segment) => segment.iter().collect::<String>(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    RawPart {
        tokens,
        runs,
        acronyms,
        substrings,
        text,
    }
}

/// 一个文件的完整特征。
struct Features {
    name: RawPart,
    parent: RawPart,
    /// token 权重（平滑 idf，缩写按下限与加成）。
    name_weights: BTreeMap<u32, f64>,
    parent_weights: BTreeMap<u32, f64>,
}

/// 计数转 f64 做平滑 idf；usize 在 2^53 内精度无损。
#[allow(clippy::cast_precision_loss)]
fn as_f64(value: usize) -> f64 {
    value as f64
}

/// 加权 Sørensen-Dice（C-05：关键词重叠为主通道）。
fn weighted_dice(a: &BTreeMap<u32, f64>, b: &BTreeMap<u32, f64>) -> f64 {
    let sum_a: f64 = a.values().sum();
    let sum_b: f64 = b.values().sum();
    if sum_a <= 0.0 || sum_b <= 0.0 {
        return 0.0;
    }
    let mut shared = 0.0;
    for (key, value) in a {
        if let Some(other) = b.get(key) {
            shared += value.min(*other);
        }
    }
    2.0 * shared / (sum_a + sum_b)
}

/// 轻量并查集（C-05：确定性聚类；不为此引入 petgraph）。
struct UnionFind {
    parent: Vec<u32>,
    size: Vec<u32>,
}
impl UnionFind {
    // 文件索引数量受内存约束远小于 u32 上限；截断不可能发生。
    #[allow(clippy::cast_possible_truncation)]
    fn new(count: usize) -> Self {
        Self {
            parent: (0..count as u32).collect(),
            size: vec![1; count],
        }
    }
    fn find(&mut self, mut node: u32) -> u32 {
        while self.parent[node as usize] != node {
            self.parent[node as usize] = self.parent[self.parent[node as usize] as usize];
            node = self.parent[node as usize];
        }
        node
    }
    /// 合并两个节点；`allow_group_merge=false` 时两个已成组之间不合并（防链式误聚类）。
    fn union(&mut self, a: u32, b: u32, allow_group_merge: bool) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        if !allow_group_merge && self.size[ra as usize] > 1 && self.size[rb as usize] > 1 {
            return;
        }
        let (big, small) = if self.size[ra as usize] >= self.size[rb as usize] {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small as usize] = big;
        self.size[big as usize] += self.size[small as usize];
    }
}

fn contains_subsequence(haystack: &[u32], needle: &[u32]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

/// C-05 功能目录名称生成：组内高权重、高覆盖、高区分度的公共短语。
/// 候选短语 = 各成员分段内的连续单元子序列，覆盖率 ≥3/4（且 ≥2 成员，ascii 单字
/// 片段计子串变体覆盖：AAMBA 计入 AMBA）；保留不被其他合格短语包含的极大短语；
/// 得分 = 单元（有效覆盖口径的）平滑 idf 之和，ascii 缩写单元 ×2。取最高分短语；
/// 覆盖数相同、同为拉丁词且得分达首选 80% 时以 `_` 连接取两个（AMBA_APB），中文只取一个。
/// 权重并列按渲染串字典序决胜；名称经附录 B 合法性处理、限 24 个 UTF-16 单元。
fn name_group(
    features: &[Features],
    interner: &Interner,
    token_acronym: &BTreeSet<u32>,
    unit_eff_df: &BTreeMap<u32, u32>,
    total: usize,
    members: &[u32],
    use_parent: bool,
) -> Option<(String, f64, Vec<u32>)> {
    let part_of = |member: u32| -> &RawPart {
        let feature = &features[member as usize];
        if use_parent {
            &feature.parent
        } else {
            &feature.name
        }
    };
    let idf_of = |id: u32| -> f64 {
        let df = unit_eff_df.get(&id).copied().unwrap_or(1);
        (1.0 + as_f64(total) / f64::from(df.max(1))).ln()
    };
    // 覆盖率门槛 ceil(0.75·n) 的整数形式（≥3/4 成员，且至少 2 个成员）。
    let threshold = (members.len() * PHRASE_COVERAGE_NUM)
        .div_ceil(PHRASE_COVERAGE_DEN)
        .max(2);
    if members.len() < threshold {
        return None;
    }
    // 短语 → 覆盖成员数（每成员至多计一次）。单字 ascii 短语的覆盖含子串变体：
    // 成员自身没有该词、但其词内 3～6 字符子串包含它时也计入（AAMBA 计入 AMBA）；
    // 子串本身不作为独立候选短语（LOGO 不因 log 子串得名）。
    let mut coverage: BTreeMap<Vec<u32>, usize> = BTreeMap::new();
    for &member in members {
        let part = part_of(member);
        let mut seen: BTreeSet<Vec<u32>> = BTreeSet::new();
        for run in &part.runs {
            let span = run.len().min(NAME_UNIT_LIMIT);
            for start in 0..run.len() {
                for len in 1..=span.min(run.len() - start) {
                    seen.insert(run[start..start + len].to_vec());
                }
            }
        }
        for phrase in seen {
            *coverage.entry(phrase).or_insert(0) += 1;
        }
    }
    {
        // 单字 ascii 短语：逐成员补计子串变体覆盖。
        let singles: Vec<(Vec<u32>, usize)> = coverage
            .iter()
            .filter(|(phrase, _)| phrase.len() == 1)
            .map(|(phrase, count)| (phrase.clone(), *count))
            .collect();
        for (phrase, base) in singles {
            let id = phrase[0];
            let mut count = base;
            for &member in members {
                let part = part_of(member);
                if part.runs.iter().any(|run| run.contains(&id)) {
                    continue; // 已计入。
                }
                if part.substrings.contains(&id) {
                    count += 1;
                }
            }
            coverage.insert(phrase, count);
        }
    }
    let mut qualifying: Vec<(Vec<u32>, usize)> = coverage
        .into_iter()
        .filter(|(_, count)| *count >= threshold)
        .collect();
    if qualifying.is_empty() {
        return None;
    }
    // 极大化（长度降序保证容器先保留）：去掉被其他合格短语整体包含的短语。
    qualifying.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
    let mut maximal: Vec<(Vec<u32>, usize)> = Vec::new();
    'phrase: for (phrase, count) in qualifying {
        for (kept, _) in &maximal {
            if kept.len() > phrase.len() && contains_subsequence(kept, &phrase) {
                continue 'phrase;
            }
        }
        maximal.push((phrase, count));
    }
    let render = |units: &[u32]| -> String {
        units
            .iter()
            .map(|&id| {
                let text = interner.text(id);
                if is_ascii_word(text) {
                    text.to_uppercase()
                } else {
                    text.to_string()
                }
            })
            .collect::<String>()
    };
    let score_of = |units: &[u32]| -> f64 {
        units
            .iter()
            .map(|&id| {
                let base = idf_of(id);
                let boosted = is_ascii_word(interner.text(id)) && token_acronym.contains(&id);
                base * if boosted { 2.0 } else { 1.0 }
            })
            .sum()
    };
    // 排序：得分降序、渲染串字典序升序（稳定 tie-break）。
    maximal.sort_by(|a, b| {
        score_of(&b.0)
            .partial_cmp(&score_of(&a.0))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(render(&a.0).cmp(&render(&b.0)))
    });
    let (top_units, top_count) = maximal.first().cloned()?;
    let top_score = score_of(&top_units);
    let top_ascii = top_units.len() == 1 && is_ascii_word(interner.text(top_units[0]));
    let mut chosen: Vec<Vec<u32>> = vec![top_units.clone()];
    if top_ascii {
        // 第二个片段：覆盖数与首选相同（都能代表全组）、同为拉丁词、得分达首选 80%。
        if let Some(second) = maximal
            .iter()
            .skip(1)
            .filter(|(units, count)| {
                *count == top_count
                    && units.len() == 1
                    && is_ascii_word(interner.text(units[0]))
                    && score_of(units) >= SECOND_PHRASE_RATIO * top_score
            })
            .map(|(units, _)| units.clone())
            .next()
        {
            chosen.push(second);
        }
    }
    let mut name = chosen
        .iter()
        .map(|units| render(units))
        .collect::<Vec<_>>()
        .join("_");
    if name.encode_utf16().count() > NAME_UNIT_LIMIT {
        // 加入第二片段超长时只保留首选（简短优先）。
        name = render(&top_units);
    }
    name = rules::truncate_utf16_units(&name, NAME_UNIT_LIMIT);
    let legal = rules::legalize_derived(&name);
    if legal.trim_matches('_').is_empty() {
        return None;
    }
    let mut units = top_units;
    for extra in chosen.iter().skip(1) {
        units.extend_from_slice(extra);
    }
    Some((legal, top_score, units))
}

/// C-05：对一个大类内的文件计算功能分类目录名。
/// `files` 任意顺序传入（内部按 rel 稳定排序）；`incumbents` 是该大类当前已存在的
/// 功能目录名（在位者优先的稳定性 tie-break）。返回 (文件 id, 功能目录名)。
pub fn functional_dirs(files: &[FuncFile], incumbents: &BTreeSet<String>) -> Vec<(i64, String)> {
    let mut ordered: Vec<(i64, &str, String, String)> = files
        .iter()
        .map(|file| {
            // 扩展名是大类信息（附录 A），不是功能信息：入口处剥离。
            let (stem, _) = fsutil::split_compound_name(&file.name);
            (
                file.id,
                file.rel.as_str(),
                stem.to_string(),
                file.parent.clone(),
            )
        })
        .collect::<Vec<_>>();
    ordered.sort_by(|a, b| a.1.cmp(b.1));
    let count = ordered.len();
    if count == 0 {
        return Vec::new();
    }
    let mut interner = Interner::new();
    // 阶段 1：特征提取，每个文件恰好一次（文件名主体 + 紧邻父目录名）。
    let mut raws: Vec<(RawPart, RawPart)> = Vec::with_capacity(count);
    for (_, _, stem, parent) in &ordered {
        let name = extract_part(stem, &mut interner);
        let parent = extract_part(parent, &mut interner);
        raws.push((name, parent));
    }
    // 阶段 2：全局文档频率（token 与命名单元分别统计；越常见权重越低）。
    let mut token_df: BTreeMap<u32, u32> = BTreeMap::new();
    let mut unit_df: BTreeMap<u32, u32> = BTreeMap::new();
    let mut token_acronym: BTreeSet<u32> = BTreeSet::new();
    for (name, parent) in &raws {
        for id in name.tokens.union(&parent.tokens) {
            *token_df.entry(*id).or_insert(0) += 1;
        }
        for id in name.acronyms.union(&parent.acronyms) {
            token_acronym.insert(*id);
        }
        let units: BTreeSet<u32> = name
            .runs
            .iter()
            .chain(parent.runs.iter())
            .flatten()
            .copied()
            .collect();
        for id in units {
            *unit_df.entry(id).or_insert(0) += 1;
        }
    }
    // 有效覆盖频率：单元本身出现、或作为某成员 ascii 词的子串出现（AAMBA 计入 AMBA）。
    let mut unit_eff_df: BTreeMap<u32, u32> = unit_df.clone();
    for (name, parent) in &raws {
        let parts = [name, parent];
        let covered: BTreeSet<u32> = parts
            .iter()
            .flat_map(|part| part.substrings.iter().copied())
            .collect();
        for id in covered {
            *unit_eff_df.entry(id).or_insert(0) += 1;
        }
    }
    let total = count;
    let idf = |id: u32| -> f64 {
        let df = token_df.get(&id).copied().unwrap_or(1);
        (1.0 + as_f64(total) / f64::from(df.max(1))).ln()
    };
    // 权重：平滑 idf；全局缩写 token 设下限（整组同主题时仍保持高信息量），
    // 本文件以缩写形式写出再 ×2。
    let base_of = |id: u32| -> f64 {
        if token_acronym.contains(&id) {
            idf(id).max(ACRONYM_WEIGHT_FLOOR)
        } else {
            idf(id)
        }
    };
    let features: Vec<Features> = raws
        .into_iter()
        .map(|(name, parent)| {
            let acronym_of = |id: u32| name.acronyms.contains(&id) || parent.acronyms.contains(&id);
            let name_weights = name
                .tokens
                .iter()
                .map(|&id| (id, base_of(id) * if acronym_of(id) { 2.0 } else { 1.0 }))
                .collect();
            let parent_weights = parent
                .tokens
                .iter()
                .map(|&id| (id, base_of(id) * if acronym_of(id) { 2.0 } else { 1.0 }))
                .collect();
            Features {
                name,
                parent,
                name_weights,
                parent_weights,
            }
        })
        .collect();
    // 阶段 3：倒排索引 → 候选对（稀有 token 优先；高频过滤与全局预算约束，禁止 O(N²)）。
    let cap = df_cap(count);
    let mut postings: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (index, feature) in features.iter().enumerate() {
        let mut tokens: Vec<u32> = feature
            .name
            .tokens
            .union(&feature.parent.tokens)
            .copied()
            .collect();
        tokens.sort_unstable();
        // 文件索引数量受内存约束远小于 u32 上限；截断不可能发生。
        #[allow(clippy::cast_possible_truncation)]
        let file_index = index as u32;
        for id in tokens {
            postings.entry(id).or_default().push(file_index);
        }
    }
    let mut indexable: Vec<(u32, u64)> = postings
        .iter()
        .filter(|(_, list)| list.len() >= 2 && (list.len() as u64) <= cap)
        .map(|(&id, list)| (id, (list.len() * (list.len() - 1) / 2) as u64))
        .collect();
    indexable.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    let mut budget = MAX_CANDIDATE_PAIRS;
    for (id, need) in indexable {
        if need > budget {
            break;
        }
        budget -= need;
        if let Some(list) = postings.get(&id) {
            for i in 0..list.len() {
                for j in (i + 1)..list.len() {
                    pairs.push((list[i].min(list[j]), list[i].max(list[j])));
                }
            }
        }
    }
    drop(postings);
    pairs.sort_unstable();
    pairs.dedup();
    // 阶段 4：候选对精确评分（并行按索引回收；评分与调度无关）。
    // 强 token = 原文大写缩写（任一文件）或全局区分度 idf ≥ 4。
    let strong_token = |id: u32| -> bool { token_acronym.contains(&id) || idf(id) >= STRONG_IDF };
    let edges: Vec<(u32, u32, f64, u8)> = pairs
        .par_iter()
        .filter_map(|&(a, b)| {
            let (fa, fb) = (&features[a as usize], &features[b as usize]);
            let name_dice = weighted_dice(&fa.name_weights, &fb.name_weights);
            let str_sim = strsim::jaro_winkler(&fa.name.text, &fb.name.text)
                .max(strsim::normalized_levenshtein(&fa.name.text, &fb.name.text));
            let parent_dice = weighted_dice(&fa.parent_weights, &fb.parent_weights);
            let score = NAME_DICE_WEIGHT * name_dice
                + STRING_WEIGHT * str_sim
                + PARENT_WEIGHT * parent_dice;
            if score >= EDGE_STRONG {
                return Some((a, b, score, 0));
            }
            // 名称重叠覆盖率：共享 token 权重占较小一方名称总重的比例。中文短语没有
            // 缩写形式、小集合内 idf 也到不了强门槛，靠覆盖率识别「测试流程/流程指导」
            // 一类共享主题（≥ 一半名称质量来自公共短语才连边，宁可少合并）。
            let shared_name: f64 = fa
                .name_weights
                .iter()
                .filter_map(|(key, value)| fb.name_weights.get(key).map(|other| value.min(*other)))
                .sum();
            let min_name_sum = fa
                .name_weights
                .values()
                .sum::<f64>()
                .min(fb.name_weights.values().sum::<f64>());
            let name_coverage = if min_name_sum > 0.0 {
                shared_name / min_name_sum
            } else {
                0.0
            };
            let shared_strong_name = fa
                .name_weights
                .keys()
                .any(|&id| fb.name_weights.contains_key(&id) && strong_token(id));
            if (shared_strong_name || name_coverage >= 0.5) && score >= EDGE_MEDIUM {
                return Some((a, b, score, 1));
            }
            let shared_strong_parent = fa
                .parent_weights
                .keys()
                .any(|&id| fb.parent_weights.contains_key(&id) && strong_token(id));
            if shared_strong_parent && parent_dice >= 0.5 && score >= EDGE_MEDIUM {
                return Some((a, b, score, 2));
            }
            None
        })
        .collect();
    // 阶段 5：确定性并查集（分数降序、对偶升序；中阈值只允许单文件并入既有组）。
    let mut ordered_edges = edges;
    ordered_edges.sort_by(|x, y| {
        y.2.partial_cmp(&x.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(x.0.cmp(&y.0))
            .then(x.1.cmp(&y.1))
    });
    let mut uf = UnionFind::new(count);
    let mut best_edge: BTreeMap<(u32, u32), f64> = BTreeMap::new();
    for &(a, b, score, kind) in &ordered_edges {
        let key = (a.min(b), a.max(b));
        match best_edge.get(&key) {
            Some(best) if *best >= score => {}
            _ => {
                best_edge.insert(key, score);
            }
        }
        uf.union(a, b, kind == 0);
    }
    let mut components: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    // 文件索引数量受内存约束远小于 u32 上限；截断不可能发生。
    #[allow(clippy::cast_possible_truncation)]
    let indices: Vec<u32> = (0..count as u32).collect();
    for index in indices {
        components.entry(uf.find(index)).or_default().push(index);
    }
    // 阶段 6：组命名、保留优先级、超上限并入或兜底。
    struct Group {
        members: Vec<u32>,
        confidence: f64,
        phrase_score: f64,
        phrase_units: Vec<u32>,
        name: String,
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut pending: Vec<u32> = Vec::new();
    for (_, members) in components {
        if members.len() < 2 {
            pending.extend(members);
            continue;
        }
        // 组内置信度：成员到组内最佳边的平均分（连边保守，天然有界）。
        let mut confidence = 0.0;
        for &member in &members {
            let best = members
                .iter()
                .filter(|&&other| other != member)
                .filter_map(|&other| best_edge.get(&(member.min(other), member.max(other))))
                .copied()
                .fold(0.0_f64, f64::max);
            confidence += best;
        }
        confidence /= as_f64(members.len());
        let named = name_group(
            &features,
            &interner,
            &token_acronym,
            &unit_eff_df,
            total,
            &members,
            false,
        )
        .or_else(|| {
            name_group(
                &features,
                &interner,
                &token_acronym,
                &unit_eff_df,
                total,
                &members,
                true,
            )
        });
        match named {
            Some((name, phrase_score, phrase_units)) => groups.push(Group {
                members,
                confidence,
                phrase_score,
                phrase_units,
                name,
            }),
            // 无明确公共主题的组不生成独立目录：成员进入兜底流程（C-05）。
            None => pending.extend(members),
        }
    }
    // 保留优先级：覆盖文件数 ↓、置信度 ↓、核心词区分度 ↓、在位者优先、名称字典序 ↑。
    groups.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then(
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(
                b.phrase_score
                    .partial_cmp(&a.phrase_score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(
                incumbents
                    .contains(&b.name)
                    .cmp(&incumbents.contains(&a.name)),
            )
            .then(a.name.cmp(&b.name))
    });
    let retained: Vec<&Group> = groups.iter().take(MAX_FUNCTIONAL_DIRS).collect();
    let mut assignment: Vec<Option<String>> = vec![None; count];
    for group in &retained {
        for &member in &group.members {
            assignment[member as usize] = Some(group.name.clone());
        }
    }
    // 未入选组、单文件与无主题组成员：与已保留组共享强 token（含子串变体）时并入
    // （能可靠合并），否则进入「其他」（不确定时不猜测、不为一文件一目录消耗名额）。
    let strong_unit = |member: u32, unit: u32| -> bool {
        let feature = &features[member as usize];
        let acronym =
            feature.name.acronyms.contains(&unit) || feature.parent.acronyms.contains(&unit);
        if acronym {
            return true;
        }
        let df = unit_eff_df.get(&unit).copied().unwrap_or(1);
        (1.0 + as_f64(total) / f64::from(df.max(1))).ln() >= STRONG_IDF
    };
    for &member in &pending {
        for group in &retained {
            if group
                .phrase_units
                .iter()
                .any(|&unit| strong_unit(member, unit))
            {
                assignment[member as usize] = Some(group.name.clone());
                break;
            }
        }
    }
    ordered
        .iter()
        .enumerate()
        .map(|(index, (id, _, _, _))| {
            (
                *id,
                assignment[index]
                    .clone()
                    .unwrap_or_else(|| FALLBACK_DIR.to_string()),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(files: &[(&str, &str)]) -> Vec<(i64, String)> {
        let inputs: Vec<FuncFile> = files
            .iter()
            .enumerate()
            .map(|(i, (name, parent))| FuncFile {
                id: i64::try_from(i).unwrap() + 1,
                rel: format!("{parent}/{name}"),
                name: name.to_string(),
                parent: parent.to_string(),
            })
            .collect();
        functional_dirs(&inputs, &BTreeSet::new())
    }
    fn dir_of(result: &[(i64, String)], id: i64) -> &str {
        result
            .iter()
            .find(|(key, _)| *key == id)
            .map_or("?", |(_, value)| value.as_str())
    }
    fn with_incumbents(files: &[(&str, &str)], incumbents: &[&str]) -> Vec<(i64, String)> {
        let inputs: Vec<FuncFile> = files
            .iter()
            .enumerate()
            .map(|(i, (name, parent))| FuncFile {
                id: i64::try_from(i).unwrap() + 1,
                rel: format!("{parent}/{name}"),
                name: name.to_string(),
                parent: parent.to_string(),
            })
            .collect();
        let set: BTreeSet<String> = incumbents.iter().map(ToString::to_string).collect();
        functional_dirs(&inputs, &set)
    }

    // 覆盖 C-05（明显相同功能：共同 MBIST 强 token 成组）
    #[test]
    fn mbist_pair_groups_together() {
        let result = run(&[
            ("05_10 SMS Mbist 介绍.docx", ""),
            ("05_22 Tessent_MBIST_RTL流程指导.docx", ""),
        ]);
        assert_eq!(dir_of(&result, 1), "MBIST");
        assert_eq!(dir_of(&result, 2), "MBIST");
    }

    // 覆盖 C-05（拼写变体 + 混排：AMBA/APB/JTAG 相关四个文件同组，名称 AMBA_APB）
    #[test]
    fn amba_family_groups_with_two_token_name() {
        let result = run(&[
            ("amba_apb_protocol_spec.pdf", ""),
            ("AAMBA3apb.pdf", ""),
            ("AMBA总线基础(2013)[1].pptx", ""),
            ("JTAG2APB案例.docx", ""),
        ]);
        for id in 1..=4 {
            assert_eq!(dir_of(&result, id), "AMBA_APB", "id={id}");
        }
    }

    // 覆盖 C-05（字符串长得像但语义 token 不足：不得仅凭前缀合并）
    #[test]
    fn scan_prefix_does_not_merge() {
        let result = run(&[("scan_report.pdf", ""), ("scanner_driver.pdf", "")]);
        assert_eq!(dir_of(&result, 1), FALLBACK_DIR);
        assert_eq!(dir_of(&result, 2), FALLBACK_DIR);
    }

    // 覆盖 C-05（中英文混合；噪声（日期/版本/重名后缀）不得破坏聚类）
    #[test]
    fn mixed_language_and_noise_still_group() {
        let noisy = run(&[
            ("MBIST介绍 (1).pdf", ""),
            ("MBIST介绍_final.pdf", ""),
            ("2026-05_MBIST介绍_v2.pdf", ""),
        ]);
        for id in 1..=3 {
            assert_eq!(dir_of(&noisy, id), "MBIST", "噪声不得破坏聚类：id={id}");
        }
        let mixed = run(&[
            ("Tessent_MBIST流程指导.pdf", ""),
            ("MBIST 测试介绍.docx", ""),
            ("mbist_rtl_flow.txt", ""),
        ]);
        for id in 1..=3 {
            assert_eq!(dir_of(&mixed, id), "MBIST", "中英混排：id={id}");
        }
    }

    // 覆盖 C-05（粗粒度：附加描述不得派生新目录，全部归 MBIST）
    #[test]
    fn coarse_grained_single_dir() {
        let result = run(&[
            ("Tessent_MBIST_RTL流程.pdf", ""),
            ("MBIST算法介绍.pdf", ""),
            ("MBIST Repair Guide.pdf", ""),
            ("MBIST仿真方法.docx", ""),
        ]);
        for id in 1..=4 {
            assert_eq!(dir_of(&result, id), "MBIST", "id={id}");
        }
    }

    // 覆盖 C-05（父目录共享强 token 增强关联；父目录名参与命名兜底）
    #[test]
    fn strong_parent_token_joins_uninformative_names() {
        let result = run(&[("intro.pdf", "MBIST"), ("flow.docx", "MBIST")]);
        assert_eq!(dir_of(&result, 1), "MBIST");
        assert_eq!(dir_of(&result, 2), "MBIST");
        // 普通目录名（非强 token）不产生合并。
        let weak = run(&[("intro.pdf", "新建文件夹"), ("flow.docx", "新建文件夹")]);
        assert_eq!(dir_of(&weak, 1), FALLBACK_DIR);
        assert_eq!(dir_of(&weak, 2), FALLBACK_DIR);
    }

    // 覆盖 C-05（单文件与其他文件无公共主题 → 兜底「其他」）
    #[test]
    fn singletons_fall_back() {
        let result = run(&[("completely_unknown.pdf", ""), ("random_note.docx", "")]);
        assert_eq!(dir_of(&result, 1), FALLBACK_DIR);
        assert_eq!(dir_of(&result, 2), FALLBACK_DIR);
    }

    // 覆盖 C-05（数量上限：12+ 个候选组只保留 10 个功能目录；「其他」不计入上限；
    // 未入选的小组与保留组共享核心 token 时并入，否则进入「其他」）
    #[test]
    fn cap_ten_functional_dirs() {
        let themes = [
            "MBIST",
            "ATPG",
            "SCAN",
            "AMBA_APB",
            "JTAG",
            "TESSENT",
            "DFT",
            "RTL",
            "PROTOCOL",
            "VERIFICATION",
            "LITHO",
            "PACKAGE",
        ];
        let mut files: Vec<(String, String)> = Vec::new();
        for (rank, theme) in themes.iter().enumerate() {
            let copies = 6 - (rank / 3);
            for n in 0..copies {
                files.push((format!("{theme}_guide_{n}.pdf"), String::new()));
            }
        }
        let litho_intro_id = i64::try_from(files.len()).unwrap() + 1;
        files.push(("LITHO_intro.pdf".to_string(), String::new()));
        let result = run(&files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect::<Vec<_>>());
        let mut dirs: BTreeSet<String> = result.iter().map(|(_, d)| d.clone()).collect();
        assert!(
            dirs.remove(FALLBACK_DIR),
            "「其他」必须存在（未入选且无法并入的小组进入兜底）"
        );
        assert!(
            dirs.len() <= MAX_FUNCTIONAL_DIRS,
            "功能目录不得超过 10 个：{dirs:?}"
        );
        assert_eq!(dirs.len(), 10, "恰好保留 10 个：{dirs:?}");
        if dirs.contains("LITHO") {
            assert_eq!(
                dir_of(&result, litho_intro_id),
                "LITHO",
                "共享核心 token 必须并入"
            );
        } else {
            assert_eq!(
                dir_of(&result, litho_intro_id),
                FALLBACK_DIR,
                "保留组不存在时进入「其他」"
            );
        }
    }

    // 覆盖 C-05（稳定性：达到 10 个后新增冷门文件不挤掉在位组——在位者优先决胜）
    #[test]
    fn incumbents_win_ties_over_new_candidates() {
        let themes = [
            "MBIST", "ATPG", "SCAN", "AMBA_APB", "JTAG", "TESSENT", "DFT", "RTL", "PROTOCOL",
            "ZEBRA",
        ];
        let mut files: Vec<(String, String)> = Vec::new();
        for theme in themes {
            for n in 0..2 {
                files.push((format!("{theme}_guide_{n}.pdf"), String::new()));
            }
        }
        let themed_count = files.len();
        for n in 0..2 {
            files.push((format!("ALPHA_guide_{n}.pdf"), String::new()));
        }
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let result = run(&borrowed);
        let mut fresh_dirs: BTreeSet<String> = result.iter().map(|(_, d)| d.clone()).collect();
        fresh_dirs.remove(FALLBACK_DIR);
        assert_eq!(
            fresh_dirs.len(),
            10,
            "全新目录同样恰好保留 10 个：{fresh_dirs:?}"
        );
        // 有在位信息时：在位 10 组全部保留，ALPHA 落选、其文件进入「其他」。
        let incumbent_list: Vec<String> = themes.iter().map(ToString::to_string).collect();
        let incumbent_refs: Vec<&str> = incumbent_list.iter().map(String::as_str).collect();
        let with_incumbents = with_incumbents(&borrowed, &incumbent_refs);
        let mut dirs: BTreeSet<String> = with_incumbents.iter().map(|(_, d)| d.clone()).collect();
        dirs.remove(FALLBACK_DIR);
        assert_eq!(dirs.len(), 10);
        assert!(dirs.contains("ZEBRA"), "在位组并列时优先保留：{dirs:?}");
        assert!(!dirs.contains("ALPHA"), "新候选组无优势不得替换在位组");
        for (key, dir) in &with_incumbents {
            if dir == FALLBACK_DIR {
                assert!(
                    *key > i64::try_from(themed_count).unwrap(),
                    "只有 ALPHA 文件进入「其他」"
                );
            }
        }
    }

    // 覆盖 C-05（确定性：不同输入顺序得到完全一致的分组与命名）
    #[test]
    fn input_order_does_not_change_result() {
        let mut files: Vec<FuncFile> = Vec::new();
        let mut id = 0i64;
        for theme in ["MBIST", "AMBA_APB", "SCAN", "ATPG", "JTAG", "DFT"] {
            for n in 0..3 {
                id += 1;
                files.push(FuncFile {
                    id,
                    rel: format!("dir{n}/{theme}_doc_{n}.pdf"),
                    name: format!("{theme}_doc_{n}.pdf"),
                    parent: format!("dir{n}"),
                });
            }
        }
        for n in 0..20 {
            id += 1;
            files.push(FuncFile {
                id,
                rel: format!("misc/杂项文件_{n}.txt"),
                name: format!("杂项文件_{n}.txt"),
                parent: "misc".to_string(),
            });
        }
        let mut baseline = functional_dirs(&files, &BTreeSet::new());
        baseline.sort_by_key(|(key, _)| *key);
        // 逆序输入：分组与命名必须逐 id 一致（并行调度由 rayon 决定，结果不受影响）。
        let mut shuffled = files.clone();
        shuffled.reverse();
        let mut reversed = functional_dirs(&shuffled, &BTreeSet::new());
        reversed.sort_by_key(|(key, _)| *key);
        assert_eq!(baseline, reversed, "输入顺序不得影响功能分类结果");
        // 交错打乱再验证一次。
        let interleaved: Vec<FuncFile> = (0..files.len())
            .map(|i| files[(i * 7 + 3) % files.len()].clone())
            .collect();
        let mut mixed = functional_dirs(&interleaved, &BTreeSet::new());
        mixed.sort_by_key(|(key, _)| *key);
        assert_eq!(baseline, mixed, "任意输入顺序不得影响功能分类结果");
    }

    // 覆盖 C-05（大规模合成集合不退化成 O(N²)：候选预算 + 高频 token 过滤下快速完成）
    #[test]
    fn large_synthetic_set_completes_without_full_pairwise() {
        let mut files: Vec<FuncFile> = Vec::new();
        for i in 0..4000u32 {
            let (name, parent) = if i % 50 == 0 {
                (format!("common_note_{i}.txt"), "共享".to_string())
            } else {
                (format!("doc_{i}.txt"), format!("theme{}", i % 97))
            };
            files.push(FuncFile {
                id: i64::from(i),
                rel: format!("{parent}/{name}"),
                name,
                parent,
            });
        }
        let start = std::time::Instant::now();
        let result = functional_dirs(&files, &BTreeSet::new());
        let elapsed = start.elapsed();
        assert_eq!(result.len(), files.len());
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "4000 文件应在预算内完成（实际 {elapsed:?}）"
        );
    }
}
