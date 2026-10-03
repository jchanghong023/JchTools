//! 布局保持的纯文本重建（对应冻结仓库 `src/textsnap/layout.py`）。
//!
//! 纯逻辑、不依赖 Qt 与 Paddle：输入 [`RecognizedSpan`]（crate::types），
//! 输出唯一的布局忠实文本与非内容统计。字符宽度属性取自
//! `crate::ucd_tables`（Python 3.13 unicodedata / Unicode 15.1 生成，与
//! oracle 同源）。本模块内联了 `src/textsnap/geometry.py` 中 layout 所需的
//! 三个几何辅助（`quad_bounds` / `quad_dimensions` / `quad_baseline`），
//! 语义与源码逐行一致，避免跨模块耦合。

use crate::types::{Quad, RecognizedSpan};
use crate::ucd_tables::{has_combining_class, is_wide_or_fullwidth};

/// 行归属的垂直重叠阈值（Python `LINE_VERTICAL_OVERLAP`）。
pub const LINE_VERTICAL_OVERLAP: f64 = 0.45;
/// 基线差归行时正文中位字高的倍数（Python `BASELINE_HEIGHT_FACTOR`）。
pub const BASELINE_HEIGHT_FACTOR: f64 = 0.5;

/// 非内容统计（对应 `domain.py::LayoutStats`，非负测量值）。
///
/// 计数字段为无符号天然非负；两个测量字段仅由 [`build_layout`] 以
/// 非负回退链构造，不会出现负值（Python 侧的 `__post_init__` 校验在此
/// 由类型与构造约定承担）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutStats {
    pub input_spans: usize,
    pub output_spans: usize,
    pub line_count: usize,
    pub grid_cell_width: f64,
    pub row_step: f64,
}

/// 布局保持的最终纯文本输出及其非内容统计（`domain.py::LayoutResult`）。
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutResult {
    pub text: String,
    pub stats: LayoutStats,
}

/// Python `_round_nonnegative_half_up`：floor(x+0.5)，负数报错。
///
/// # Panics
/// 输入为负时 panic（对应 Python 抛出 `ValueError`）；调用点均以
/// `max(0.0, …)` 保证非负，实际不可达。
// [quality-baseline approved 2026-10-03] 冻结规格镜像豁免（前提不可达），经用户裁定保留
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn round_nonnegative_half_up(value: f64) -> usize {
    assert!(
        value >= 0.0,
        "layout rounding input must be non-negative: {value}"
    );
    // 先加 0.5 再 floor 保证半值向上舍入；结果为整数值且非负，as usize
    // 至多饱和，不会回绕。
    (value + 0.5).floor() as usize
}

/// 把控制字符替换为空格，不改写普通 Unicode（layout.py:23-26）。
///
/// Python `unicodedata.category(c) == "Cc"` 即 Unicode 通用类别 Cc，
/// 与 [`char::is_control`]（U+0000..=U+001F、U+007F..=U+009F）完全一致。
pub fn sanitize_ocr_text(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// 单个字符的半宽格数（附录 B）：有组合类计 0；EAW 为 W/F 计 2；其余计 1。
pub fn char_cell_width(c: char) -> usize {
    if has_combining_class(c) {
        0
    } else if is_wide_or_fullwidth(c) {
        2
    } else {
        1
    }
}

/// 统计文本占用的半宽格数（ASCII/半宽 1，CJK/全宽 2，组合字符 0）。
pub fn display_cell_width(text: &str) -> usize {
    text.chars().map(char_cell_width).sum()
}

/// 已排序切片上的 Python `statistics.median`：奇数取中位，偶数取中间两值均值。
fn median_sorted(sorted: &[f64]) -> f64 {
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        f64::midpoint(sorted[mid - 1], sorted[mid])
    }
}

/// 任意切片的中位数：排序后按 [`median_sorted`] 取值。
fn median_of(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    median_sorted(&sorted)
}

/// Python `_trimmed_median`：只取正值；n≥5 时两端各去 max(1, floor(n×0.1))
/// 项（仅当 trim×2 < n 才裁）再取中位数；无正值返回 0.0（调用点回退）。
fn trimmed_median(values: &[f64]) -> f64 {
    let mut positive: Vec<f64> = values.iter().copied().filter(|v| *v > 0.0).collect();
    if positive.is_empty() {
        return 0.0;
    }
    positive.sort_by(f64::total_cmp);
    let len = positive.len();
    let window: &[f64] = if len >= 5 {
        // Python int(len*0.1)：0.1 存储略大于真值，乘积严格 ≥ len/10 且
        // 未跨过下一整数，故 floor 恒等于整数除法 len/10。
        let trim = (len / 10).max(1);
        if trim * 2 < len {
            &positive[trim..len - trim]
        } else {
            &positive
        }
    } else {
        &positive
    };
    median_sorted(window)
}

/// Python 浮点 `or` 的真值回退：0.0/-0.0 视为无效取回退值。
/// （NaN 不可能：输入坐标经校验有限。）
fn or_fallback(value: f64, fallback: f64) -> f64 {
    if value == 0.0 {
        fallback
    } else {
        value
    }
}

/// 四顶点 x/y 的最小/最大值（geometry.py `quad_bounds`）。
fn quad_bounds(quad: &Quad) -> (f64, f64, f64, f64) {
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for &(x, y) in quad {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    (min_x, min_y, max_x, max_y)
}

/// 框尺寸（宽, 高），负值截为 0（geometry.py `quad_dimensions`）。
fn quad_dimensions(quad: &Quad) -> (f64, f64) {
    let (left, top, right, bottom) = quad_bounds(quad);
    ((right - left).max(0.0), (bottom - top).max(0.0))
}

/// 基线近似：取最高两顶点 y 的均值（geometry.py `quad_baseline`：降序排序
/// 后前两项的中位数，即两项均值）。
fn quad_baseline(quad: &Quad) -> f64 {
    let mut ys: Vec<f64> = quad.iter().map(|point| point.1).collect();
    ys.sort_by(f64::total_cmp);
    f64::midpoint(ys[ys.len() - 1], ys[ys.len() - 2])
}

/// 参与行聚类的单框条目（Python `_LayoutItem`；span 以 quad 保留）。
#[derive(Debug, Clone)]
struct LayoutItem {
    text: String,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
    baseline: f64,
    quad: Quad,
}

impl LayoutItem {
    fn height(&self) -> f64 {
        self.bottom - self.top
    }
}

/// 一条输出行（Python `_TextLine`）：top/bottom/baseline 取成员中位数。
#[derive(Debug, Clone)]
struct TextLine {
    items: Vec<LayoutItem>,
}

impl TextLine {
    fn top(&self) -> f64 {
        median_of(&self.collect_key(|item| item.top))
    }

    fn bottom(&self) -> f64 {
        median_of(&self.collect_key(|item| item.bottom))
    }

    fn baseline(&self) -> f64 {
        median_of(&self.collect_key(|item| item.baseline))
    }

    fn center_y(&self) -> f64 {
        // f64::midpoint 与 Python `(top + bottom) * 0.5` 逐位一致（除以 2
        // 精确；仅当相加溢出为 inf 时不同，坐标域内不可达）。
        f64::midpoint(self.top(), self.bottom())
    }

    fn collect_key(&self, key: impl Fn(&LayoutItem) -> f64) -> Vec<f64> {
        self.items.iter().map(key).collect()
    }
}

/// 条目与行的垂直重叠比；分母 = min(条目高, 行高)（`_line_overlap`）。
fn line_overlap(item: &LayoutItem, line: &TextLine) -> f64 {
    let overlap = (item.bottom.min(line.bottom()) - item.top.max(line.top())).max(0.0);
    let smaller_height = item.height().min(line.bottom() - line.top());
    if smaller_height <= 0.0 {
        return 0.0;
    }
    overlap / smaller_height
}

/// 行聚类（`_cluster_lines`）：条目按（顶部、基线、左边）稳定排序后逐个
/// 归入候选行——候选条件为垂直重叠 ≥ [`LINE_VERTICAL_OVERLAP`] 或基线差
/// ≤ 正文字高 × [`BASELINE_HEIGHT_FACTOR`]；先取重叠更大者、再取基线差
/// 更小者。行按（中心 y、最左 x）排序，行内按（左边、右边）排序。
fn cluster_lines(mut items: Vec<LayoutItem>, body_height: f64) -> Vec<TextLine> {
    items.sort_by(|a, b| {
        a.top
            .total_cmp(&b.top)
            .then(a.baseline.total_cmp(&b.baseline))
            .then(a.left.total_cmp(&b.left))
    });
    let mut lines: Vec<TextLine> = Vec::new();
    for item in items {
        let mut matches: Vec<(f64, f64, usize)> = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let overlap = line_overlap(&item, line);
            let baseline_difference = (item.baseline - line.baseline()).abs();
            if overlap >= LINE_VERTICAL_OVERLAP
                || baseline_difference <= body_height * BASELINE_HEIGHT_FACTOR
            {
                matches.push((overlap, -baseline_difference, index));
            }
        }
        // Python `sort(key=(重叠, -基线差), reverse=True)`：重叠更大优先，
        // 其次基线差更小；稳定排序保持平局时的既有顺序。
        matches.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.total_cmp(&a.1)));
        if let Some(&(_, _, best)) = matches.first() {
            lines[best].items.push(item);
        } else {
            lines.push(TextLine { items: vec![item] });
        }
    }
    lines.sort_by(|a, b| {
        a.center_y()
            .total_cmp(&b.center_y())
            .then(min_left(a).total_cmp(&min_left(b)))
    });
    for line in &mut lines {
        line.items
            .sort_by(|a, b| a.left.total_cmp(&b.left).then(a.right.total_cmp(&b.right)));
    }
    lines
}

/// 横向半宽网格估算（`_estimate_grid_width`）：框宽 ÷ 显示格数取正值样本的
/// 截尾中位数，无有效值回退 1.0。
fn estimate_grid_width(items: &[LayoutItem]) -> f64 {
    let mut estimates: Vec<f64> = Vec::new();
    for item in items {
        let cell_count = display_cell_width(&item.text);
        let width = item.right - item.left;
        if cell_count > 0 && width > 0.0 {
            // 格数为显示单元数（≤ 文本长度量级），转 f64 不损失精度。
            // [quality-baseline approved 2026-10-03] 冻结规格镜像豁免，经用户裁定保留
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            estimates.push(width / cell_count as f64);
        }
    }
    or_fallback(trimmed_median(&estimates), 1.0)
}

/// 纵向行距回退：正文高 × 1.4，无有效正文高时 1.0。
fn row_step_fallback(body_height: f64) -> f64 {
    if body_height > 0.0 {
        body_height * 1.4
    } else {
        1.0
    }
}

/// 纵向行距估算（`_estimate_row_step`）：相邻行正基线差剔除 > 正文高×3 后
/// 取截尾中位数；不足两行或无有效值时回退 [`row_step_fallback`]。
fn estimate_row_step(lines: &[TextLine], body_height: f64) -> f64 {
    if lines.len() < 2 {
        return row_step_fallback(body_height);
    }
    let differences: Vec<f64> = lines
        .windows(2)
        .filter_map(|pair| {
            let (previous, current) = (&pair[0], &pair[1]);
            (current.baseline() > previous.baseline())
                .then(|| current.baseline() - previous.baseline())
        })
        .collect();
    let typical: Vec<f64> = differences
        .into_iter()
        .filter(|difference| body_height <= 0.0 || *difference <= body_height * 3.0)
        .collect();
    or_fallback(trimmed_median(&typical), row_step_fallback(body_height))
}

/// 渲染单行（`_render_line`）：绝对位置、与前框的物理间隔、不覆盖已输出
/// 字符三个条件同时满足；最后去掉尾随空格。
fn render_line(line: &TextLine, left_origin: f64, grid_width: f64) -> String {
    let mut out = String::new();
    let mut cursor: usize = 0;
    let mut occupied_right: Option<f64> = None;
    for item in &line.items {
        let mut target_column =
            round_nonnegative_half_up((item.left - left_origin).max(0.0) / grid_width);
        if let Some(occupied) = occupied_right {
            let gap_columns =
                round_nonnegative_half_up((item.left - occupied).max(0.0) / grid_width);
            target_column = target_column.max(cursor + gap_columns);
        }
        target_column = target_column.max(cursor);
        if target_column > cursor {
            let spaces = " ".repeat(target_column - cursor);
            out.push_str(&spaces);
            cursor = target_column;
        }
        out.push_str(&item.text);
        cursor += display_cell_width(&item.text);
        occupied_right = Some(match occupied_right {
            Some(occupied) => occupied.max(item.right),
            None => item.right,
        });
    }
    // Python `.rstrip(" ")`：仅去掉尾随半宽空格（按字节截断安全，空格为
    // 单字节且位于串尾）。
    out.truncate(out.trim_end_matches(' ').len());
    out
}

/// 构建唯一的布局忠实纯文本表示（`build_layout`）。
///
/// 剔除空文本与退化框（右≤左或下≤上）；行聚类、网格与行距估算、渲染均
/// 与冻结 Python 源逐行为对应，数值语义逐位一致。
pub fn build_layout(spans: &[RecognizedSpan]) -> LayoutResult {
    let mut items: Vec<LayoutItem> = Vec::new();
    for span in spans {
        if span.text().is_empty() {
            continue;
        }
        let text = sanitize_ocr_text(span.text());
        let (left, top, right, bottom) = quad_bounds(span.quad());
        if right <= left || bottom <= top {
            continue;
        }
        items.push(LayoutItem {
            quad: *span.quad(),
            text,
            left,
            top,
            right,
            bottom,
            baseline: quad_baseline(span.quad()),
        });
    }

    let input_spans = spans.len();
    let output_spans = items.len();
    if items.is_empty() {
        return LayoutResult {
            text: String::new(),
            stats: LayoutStats {
                input_spans,
                output_spans: 0,
                line_count: 0,
                grid_cell_width: 0.0,
                row_step: 0.0,
            },
        };
    }

    let heights: Vec<f64> = items
        .iter()
        .map(|item| quad_dimensions(&item.quad).1)
        .collect();
    let body_height = or_fallback(trimmed_median(&heights), 1.0);
    let grid_width = estimate_grid_width(&items);
    let lines = cluster_lines(items, body_height);
    let row_step = estimate_row_step(&lines, body_height);
    let left_origin = items_left_origin(&lines);
    let line_count = lines.len();

    let mut output_rows: Vec<String> = Vec::new();
    let mut previous_row: Option<usize> = None;
    let first_baseline = lines[0].baseline();
    for line in &lines {
        let estimated_row =
            round_nonnegative_half_up((line.baseline() - first_baseline).max(0.0) / row_step);
        let row = match previous_row {
            Some(prev) => estimated_row.max(prev + 1),
            None => estimated_row,
        };
        let blanks = match previous_row {
            Some(prev) => row - prev - 1,
            // Python 初值为 -1：row - (-1) - 1 == row。
            None => row,
        };
        output_rows.extend(std::iter::repeat_n(String::new(), blanks));
        output_rows.push(render_line(line, left_origin, grid_width));
        previous_row = Some(row);
    }

    LayoutResult {
        text: output_rows.join("\n"),
        stats: LayoutStats {
            input_spans,
            output_spans,
            line_count,
            grid_cell_width: grid_width,
            row_step,
        },
    }
}

/// 单行的最左 x（`min(item.left for item in line.items)`）；空行以 +∞ 参与
/// （min 以 +∞ 为单位元，不影响跨行结果）。
fn min_left(line: &TextLine) -> f64 {
    line.items
        .iter()
        .map(|item| item.left)
        .fold(f64::INFINITY, f64::min)
}
/// 全部条目的最左 x（`min(item.left for item in items)`）；调用时条目非空。
/// 逐行求 min 再跨行求 min 与一次遍历等价（min 可结合，坐标已校验有限）。
fn items_left_origin(lines: &[TextLine]) -> f64 {
    lines.iter().map(min_left).fold(f64::INFINITY, f64::min)
}

#[cfg(test)]
mod tests {
    // 覆盖 O-27/O-28：冻结仓库 tests/test_layout.py 全部 12 例逐字翻译。
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Python 测试的 `_span`：轴对齐构造框 + 双 0.9 分数 + 0° 旋转。
    fn span(text: &str, left: f64, top: f64, right: f64, bottom: f64) -> RecognizedSpan {
        RecognizedSpan::new(
            [(left, top), (right, top), (right, bottom), (left, bottom)],
            text.to_string(),
            0.9,
            0.9,
            0,
        )
        .unwrap()
    }

    // 覆盖 O-27
    #[test]
    fn scrambled_input_is_sorted_by_coordinates() {
        let spans = [
            span("World", 70.0, 0.0, 120.0, 20.0),
            span("second", 0.0, 30.0, 60.0, 50.0),
            span("Hello", 0.0, 0.0, 50.0, 20.0),
        ];
        assert_eq!(build_layout(&spans).text, "Hello  World\nsecond");
    }

    // 覆盖 O-28
    #[test]
    fn cjk_and_ascii_use_half_width_grid() {
        let spans = [
            span("中文", 0.0, 0.0, 40.0, 20.0),
            span("ABCD", 60.0, 0.0, 100.0, 20.0),
        ];
        let result = build_layout(&spans);
        assert_eq!(display_cell_width("中文"), 4);
        assert_eq!(result.text, "中文  ABCD");
    }

    // 覆盖 O-28
    #[test]
    fn code_indentation_and_internal_spaces_are_preserved() {
        let spans = [
            span("root", 0.0, 0.0, 40.0, 10.0),
            span("if  value:", 20.0, 20.0, 120.0, 30.0),
            span("return", 40.0, 40.0, 100.0, 50.0),
        ];
        assert_eq!(
            build_layout(&spans).text,
            r"root
  if  value:
    return"
        );
    }

    // 覆盖 O-27
    #[test]
    fn same_y_columns_stay_on_one_output_line() {
        let spans = [
            span("left", 0.0, 0.0, 40.0, 10.0),
            span("right", 300.0, 0.0, 350.0, 10.0),
            span("below", 0.0, 20.0, 50.0, 30.0),
        ];
        let text = build_layout(&spans).text;
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("left"));
        assert!(lines[0].ends_with("right"));
        assert!(lines[0].find("right").unwrap() > 20);
    }

    // 覆盖 O-28
    #[test]
    fn shared_margin_is_cropped_but_relative_indent_remains() {
        let spans = [
            span("first", 100.0, 50.0, 150.0, 60.0),
            span("indented", 120.0, 70.0, 200.0, 80.0),
        ];
        assert_eq!(
            build_layout(&spans).text,
            r"first
  indented"
        );
    }

    // 覆盖 O-28
    #[test]
    fn large_vertical_gap_becomes_blank_rows() {
        let spans = [
            span("top", 0.0, 0.0, 30.0, 10.0),
            span("bottom", 0.0, 60.0, 60.0, 70.0),
        ];
        let text = build_layout(&spans).text;
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines[0], "top");
        assert_eq!(lines[lines.len() - 1], "bottom");
        assert!(lines.iter().filter(|line| line.is_empty()).count() >= 2);
    }

    // 覆盖 O-28
    #[test]
    fn collision_pushes_later_text_right() {
        let spans = [
            span("abcdef", 0.0, 0.0, 60.0, 10.0),
            span("X", 40.0, 0.0, 50.0, 10.0),
        ];
        assert_eq!(build_layout(&spans).text, "abcdefX");
    }

    // 覆盖 O-28
    #[test]
    fn half_cell_spacing_rounds_up_instead_of_to_even() {
        let spans = [
            span("A", 0.0, 0.0, 10.0, 10.0),
            span("B", 25.0, 0.0, 35.0, 10.0),
        ];
        assert_eq!(build_layout(&spans).text, "A  B");
    }

    // 覆盖 O-28
    #[test]
    fn physical_gap_survives_mixed_box_cell_widths() {
        let spans = [
            span("AB", 0.0, 0.0, 15.0, 10.0),
            span("X", 20.0, 0.0, 30.0, 10.0),
        ];
        assert_eq!(build_layout(&spans).text, "AB X");
    }

    // 覆盖 O-28
    #[test]
    fn trailing_spaces_and_final_newline_are_not_added() {
        let spans = [span("value   ", 0.0, 0.0, 80.0, 10.0)];
        let result = build_layout(&spans).text;
        assert_eq!(result, "value");
        assert!(!result.ends_with('\n'));
    }

    // 覆盖 O-28
    #[test]
    fn controls_are_safely_replaced() {
        assert_eq!(sanitize_ocr_text("a\tb\nc\x00d"), "a b c d");
    }

    // 覆盖 O-28
    #[test]
    fn empty_input_has_zero_stats() {
        let result = build_layout(&[]);
        assert_eq!(result.text, "");
        assert_eq!(result.stats.output_spans, 0);
        assert_eq!(result.stats.line_count, 0);
    }
}
