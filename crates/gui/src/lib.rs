//! GUI assembly and real tool registry, separate from the generated Slint interface.
//! Window and row types are defined by `jchtools_ui`; no UI façade is exported here.

#[cfg(feature = "gui")]
pub mod gui;
pub mod registry;
