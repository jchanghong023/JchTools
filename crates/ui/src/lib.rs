//! Pure generated Slint interface; business changes do not invalidate UI code generation.
// Slint 1.17.1 emits unwrap/todo and public helpers in generated code; the exemption
// belongs only to this generated-code crate, never to GUI callback implementation.
#![allow(clippy::unwrap_used, clippy::todo, unreachable_pub)]

#[cfg(feature = "gui")]
slint::include_modules!();
