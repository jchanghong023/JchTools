//! 截图 OCR worker 库面：paddlex/TextSnap 的 Rust 推理适配层（O-23/O-24/O-26）。
//!
//! - [`det_paddlex`]：检测前后处理（resize_type0/归一化/DB 后处理，
//!   与固定 PaddleX 模型的 OpenCV 路径对照）。
//! - [`rec_paddlex`]：识别链（RecResizeImg/归一化序/CTC 解码，字典 18710 类）。
//! - [`image_ops`]：BGR 容器与 OpenCV 图像算子（INTER_LINEAR/CUBIC/LANCZOS4、
//!   warpPerspective+REPLICATE、rot90）。
//! - [`pipeline_backend`]：`OcrBackend` 真实实现（DetPaddlex + RecPaddlex 常驻
//!   会话；仅 oracle 对照启用逐调用记录）。
//!
//! - [`result_window`]：Slint 结果窗（O-21/O-22：等宽只读、复制全部后关窗、
//!   关闭清文本）。
//!
//! - [`capture_win`]：GDI 截图与冻结框选（O-17/O-18）。
//! - [`service`]：独立后台服务（O-11～O-16：单实例/托盘/热键/管道/常驻模型）。
//!
//! bin 目标只接收服务内部启动参数，不提供用户命令行产品。

pub mod capture_win;
pub mod det_paddlex;
pub mod image_ops;
pub mod pipeline_backend;
pub mod rec_paddlex;
pub mod result_window;
pub mod service;
