//! deptui-agent — background auto-deploy daemon for deploy-rs flakes.
//!
//! Watches git repositories for updates (branch head or moving tag) and
//! pushes them to configured hosts via `deploy`, reusing deptui-core's
//! deploy runner. See docs/agent-design.md for the full contract.

mod config;
mod gitwatch;
mod notify;
mod runner;
mod state;
mod wire;

fn main() {
    eprintln!("deptui-agent: daemon loop not yet wired (foundation modules only)");
    std::process::exit(1);
}
