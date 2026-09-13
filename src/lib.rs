//! The organizer engine is independent of the GUI. 整理引擎默认不联网；
//! 代理/网络工具页在用户点击时可发起本地探测与公网回显查询。
pub mod archive;
pub mod config;
pub mod control;
pub mod db;
pub mod engine;
pub mod engine_bundle;
pub mod fsutil;
pub mod hashing;
pub mod model;
pub mod nettest;
pub mod planner;
pub mod platform;
pub mod process;
pub mod proxy;
pub mod registry;
pub mod rules;

/// GUI 组装层（Slint 界面状态与回调装配）；仅在 gui 特性下编译。
#[cfg(feature = "gui")]
pub mod gui;
