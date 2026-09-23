//! 「递归解压」与「目录整理」两个工具的引擎独立于 GUI（P-02）。
//! 软件完全离线（P-03）：不发起任何网络请求、不上传文件、日志或路径。
pub mod archive;
pub mod config;
pub mod control;
pub mod convert;
pub mod db;
pub mod engine;
pub mod engine_bundle;
pub mod fsutil;
pub mod functional;
pub mod hash_cache;
pub mod hashing;
pub mod model;
pub mod perf;
pub mod planner;
pub mod platform;
pub mod process;
pub mod registry;
pub mod rules;

/// GUI 组装层（Slint 界面状态与回调装配）；仅在 gui 特性下编译。
#[cfg(feature = "gui")]
pub mod gui;
