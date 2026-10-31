//! 截图 OCR 纯逻辑核心（O 分区，`openspec/specs/screenshot-ocr/spec.md`）。
//!
//! 冻结的 TextSnap Layout（Python）是历史来源；已批准的 Xberg 行为差异
//! 以当前 O-23～O-28 与附录 B 为准。同一组构造输入须得到规格定义的
//! 确定结果。本 crate 不依赖 GUI、Win32 或推理运行库，workspace 内没有
//! 任何产品消费者；识别已迁至 Xberg，本 crate 仅为纯逻辑规格参照物。
//! 本 crate 测试不能证明真实引擎已通过验收；删除前须先确认对应规格约束
//! 已由其他执法点承接。
//!
//! 模块划分与冻结仓库 `src/textsnap/` 一一对应：
//! - [`types`] ↔ domain.py（数据边界与校验）
//! - [`tiling`] ↔ tiling.py（瓦片生成、全局坐标映射、内部边缘度量）
//! - [`geometry`] ↔ geometry.py（四边形几何：面积、交集、IoU、垂直重叠、基线）
//! - [`detection`] ↔ detection.py（接缝合并、同行探入式合并、传递去重）
//! - [`orientation`] ↔ orientation.py（方向重试策略与平局决胜）
//! - [`layout`] ↔ layout.py（行聚类、Unicode 宽度、网格布局输出）
//! - [`pipeline`] ↔ ocr.py 的可注入编排段（识别批次、密集代码拉伸重试、
//!   方向重试、取消检查点；识别与图像后端经 trait 注入）

pub mod detection;
pub mod geometry;
pub mod layout;
pub mod orientation;
pub mod pipeline;
pub mod tiling;
pub mod types;
pub mod ucd_tables;
