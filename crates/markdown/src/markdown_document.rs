//! Xberg 文档到 Markdown 的本地 Rust 薄适配器（2026-10-04 零配置改造）。
//!
//! 这个模块只调用已初始化的 `xberg.exe`（经 XB-14 唯一共享代理），不启动 HTTP
//! 服务，也不依赖 Python。请求零配置：只传 `command` 与 `path`——页数自动分流
//! （`auto_fast_pages`，阈值内置于引擎）、嵌入文档合入与 Markdown 规范化全部
//! 由引擎完成。适配器只负责解析 `document.content`（引擎最终 Markdown）、
//! 执行 Xberg 仓 FORK.md 公布的唯一调用方落盘步骤（给正文图片引用加媒体目录
//! 前缀并登记待落盘字节）以及如实转达 `warnings`；媒体引用改写保留代码及
//! alt 字面文本，目录中的 Markdown 字符引用按 URL 编码。不重复实现引擎效果。

mod convert;
mod links;
mod media;
#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
mod tests;

pub use convert::{convert, convert_cancelable, Deadline, DocumentOutput, MediaFile};

#[cfg(test)]
use serde_json::Value;

#[cfg(test)]
use {
    convert::parse_document_response,
    media::{decode_base64, image_bytes},
};
