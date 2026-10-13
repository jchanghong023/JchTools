//! 截图 OCR worker 库面：截图、结果窗、独立服务与共享 Xberg 推理客户端。
//!
//! 识别（瓦片/检测/识别/布局，O-23～O-28）由固定版本 Xberg 发布物承接：
//! 服务经 `shared_xberg` 作为客户端使用 `jchtools-xberg` 共享运行时；
//! 主程序与 worker 复用真实 crate 编译产物，XB-14 唯一共享进程及 O-07/XB-02 合同不变。
//!
//! - `shared_xberg`：共享 Xberg 引擎客户端（内存 PNG 识别、模型状态、
//!   取消/超时语义；错误与状态类型也定义在此）。
//! - [`result_window`]：Slint 结果窗（O-21/O-22：等宽只读、复制全部后关窗、
//!   关闭清文本）。
//! - [`capture_win`]：GDI 截图与冻结框选（O-17/O-18），产出内存 BGR 图与
//!   PNG 编码（O-29：不落盘）。
//! - [`service`]：独立后台服务（O-11～O-16：单实例/托盘/热键/管道/常驻模型）。
//!
//! bin 目标只接收服务内部启动参数，不提供用户命令行产品。

use jchtools_foundation::logging;
#[cfg(windows)]
use jchtools_foundation::xberg_settings;
use jchtools_xberg::xberg_runtime;

pub mod capture_win;
pub mod result_window;
pub mod service;
mod shared_xberg;
