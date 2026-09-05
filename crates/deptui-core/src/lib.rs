//! Headless core of deptui: everything that shells out to `nix`,
//! `deploy`, and `ssh` without touching a terminal. The TUI (`deptui`)
//! and the daemon (`deptui-agent`) are both thin frontends over these
//! modules; nothing in here may depend on ratatui, crossterm, or any
//! other UI concern.

pub mod askpass;
pub mod deploy;
pub mod flake;
pub mod host;
pub mod probe;
pub mod ssh;
