//! Framework-independent domain contracts for OpenSoverignBlog.
//!
//! This crate intentionally knows nothing about HTTP, SQLite, themes, model
//! providers, authentication vendors, or plugin runtimes.

pub mod ai2ai;
pub mod content;
pub mod policy;
pub mod ports;

pub use ai2ai::*;
pub use content::*;
pub use policy::*;
pub use ports::*;
