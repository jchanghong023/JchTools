//! 截图 OCR worker 库面：截图、结果窗、独立服务与 Xberg 推理客户端。
//!
//! 识别（瓦片/检测/识别/布局，O-23～O-28）由固定版本 Xberg 发布物承接：
//! 服务经 [`xberg_worker`] 以本地 stdio JSON 行协议驱动 `xberg worker`
//! 常驻子进程，行为合同（O-07 模型字节、XB-02）不变。
//!
//! - [`xberg_worker`]：`xberg worker` 子进程客户端（id 关联、状态查询、
//!   内存 PNG 识别、取消/退出语义）。
//! - [`result_window`]：Slint 结果窗（O-21/O-22：等宽只读、复制全部后关窗、
//!   关闭清文本）。
//! - [`capture_win`]：GDI 截图与冻结框选（O-17/O-18），产出内存 BGR 图与
//!   PNG 编码（O-29：不落盘）。
//! - [`service`]：独立后台服务（O-11～O-16：单实例/托盘/热键/管道/常驻模型）。
//!
//! bin 目标只接收服务内部启动参数，不提供用户命令行产品。

pub mod capture_win;
pub mod result_window;
pub mod service;
pub mod xberg_worker;
