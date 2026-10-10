//! 工具引擎独立于 GUI（P-02）；新增「转 Markdown」按 T 分区独立接入。
//! 文件处理离线（P-03）；Git、组件初始化与独立 ACP 模型服务仅按各自合同限定联网。
pub mod acp_api;
pub mod archive;
pub mod asset_util;
pub mod config;
pub mod control;
pub mod convert;
pub mod db;
pub mod dedup_components;
pub mod engine;
pub mod engine_bundle;
pub mod fsutil;
pub mod git_tools;
pub mod hash_cache;
pub mod hashing;
pub mod logging;
pub mod markdown;
pub mod markdown_assets;
pub mod markdown_document;
pub mod md_tools;
pub mod model;
pub mod perf;
pub mod planner;
pub mod platform;
pub mod process;
pub mod registry;
pub mod rules;
pub mod snap_ocr_assets;
pub mod system_proxy;
pub mod xberg_runtime;
pub mod xberg_settings;

/// GUI 组装层（Slint 界面状态与回调装配）；仅在 gui 特性下编译。
#[cfg(feature = "gui")]
pub mod gui;
