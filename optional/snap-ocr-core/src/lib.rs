//! 截图 OCR 纯逻辑核心（O 分区，`docs/requirements/SNAP2TEXT.md`）。
//!
//! 从冻结的 TextSnap Layout（Python）按附录 B 的确定性定义翻译而来：
//! 同一组构造坐标输入必须得到同一结果。本 crate 不依赖 GUI、Win32 或推理
//! 运行库；真实推理适配与桌面交互位于独立后台 worker（O-11）。
//!
//! 模块划分与冻结仓库 `src/textsnap/` 一一对应：
//! - [`types`] ↔ domain.py（数据边界与校验）
//! - [`tiling`] ↔ tiling.py（瓦片生成、全局坐标映射、内部边缘度量）
//! - [`geometry`] ↔ geometry.py（四边形几何：面积、交集、IoU、垂直重叠、基线）
//! - [`detection`] ↔ detection.py（接缝合并、传递去重、consolidate）
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
