//! The agent's side of the control-API contract. The types themselves
//! live in `deptui_core::agentwire` so the TUI client and this server
//! are compiled from one definition; this module adds the pieces only
//! the agent needs.

pub use deptui_core::agentwire::*;

use crate::state::RunRecord;

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn summary_from_record(watch: &str, r: &RunRecord) -> RunSummary {
    RunSummary {
        watch: watch.to_string(),
        id: r.id,
        rev: r.rev.clone(),
        trigger: r.trigger.clone(),
        started: r.started,
        finished: r.finished,
        hosts: r
            .hosts
            .iter()
            .map(|h| HostRun {
                host: h.host.clone(),
                outcome: h.outcome.clone(),
                message: h.message.clone(),
            })
            .collect(),
        log_lines: r.log.len(),
    }
}
