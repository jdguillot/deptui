//! Library re-exports for integration tests and the binary. The TUI's
//! own modules live here; the headless machinery (deploy runner, flake
//! discovery, probes, ssh) lives in `deptui-core` and is re-exported
//! under the same names so `crate::host::…`-style paths keep working
//! throughout the TUI code.

pub use deptui_core::{askpass, deploy, flake, host, probe, ssh};

pub mod app;
pub mod event;
pub mod joblog;
pub mod theme;
pub mod ui;
