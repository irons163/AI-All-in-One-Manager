//! aiom — All-in-One account manager for AI coding CLIs.
//!
//! Each provider (Claude, Codex, Gemini, Grok, OpenCode, or a user-defined
//! one) is described as a set of *artifacts*: the files, JSON keys and
//! keychain items that together make up "being logged in". An account is a
//! named snapshot of those artifacts; switching swaps the live artifacts for
//! the snapshot.

pub mod apps;
pub mod artifact;
pub mod chromium;
pub mod cli;
pub mod ctx;
pub mod ops;
pub mod provider;
pub mod secrets;
pub mod store;
pub mod usage;
pub mod util;
