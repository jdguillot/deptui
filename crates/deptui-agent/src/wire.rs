//! Wire types shared by the API server and the CLI client. Everything
//! here is `Serialize + Deserialize`: the server speaks them as JSON
//! and `deptui-agent status --json` / the TUI parse them back.

use serde::{Deserialize, Serialize};

use crate::state::{HostRun, RunRecord};

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatus {
    pub version: String,
    /// Global pause flag.
    pub paused: bool,
    pub watches: Vec<WatchStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchStatus {
    pub name: String,
    pub repo: String,
    /// `"branch main"` / `"tag prod"`.
    pub ref_label: String,
    pub paused: bool,
    /// Head of the watched ref at the last poll.
    #[serde(default)]
    pub last_seen: Option<String>,
    /// Unix time of the next scheduled poll, if the daemon is running
    /// one.
    #[serde(default)]
    pub next_poll: Option<u64>,
    /// Set while a deploy run of this watch is in flight.
    #[serde(default)]
    pub running: Option<RunningInfo>,
    pub hosts: Vec<HostStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningInfo {
    pub rev: String,
    pub started: u64,
    pub trigger: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostStatus {
    pub name: String,
    pub paused: bool,
    #[serde(default)]
    pub deployed_rev: Option<String>,
    #[serde(default)]
    pub deployed_time: Option<u64>,
    #[serde(default)]
    pub failed_rev: Option<String>,
    #[serde(default)]
    pub failed_time: Option<u64>,
    #[serde(default)]
    pub failed_message: Option<String>,
    #[serde(default)]
    pub unreachable: Option<String>,
}

/// One run in `GET /history` — a [`RunRecord`] minus its log, which is
/// fetched separately via `GET /log`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub watch: String,
    pub id: u64,
    pub rev: String,
    pub trigger: String,
    pub started: u64,
    #[serde(default)]
    pub finished: Option<u64>,
    pub hosts: Vec<HostRun>,
    /// Number of captured log lines (retrievable via `/log`).
    pub log_lines: usize,
}

impl RunSummary {
    pub fn from_record(watch: &str, r: &RunRecord) -> Self {
        Self {
            watch: watch.to_string(),
            id: r.id,
            rev: r.rev.clone(),
            trigger: r.trigger.clone(),
            started: r.started,
            finished: r.finished,
            hosts: r.hosts.clone(),
            log_lines: r.log.len(),
        }
    }
}

/// Reply for the POST verbs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OkReply {
    pub ok: bool,
    pub message: String,
}

/// Error body for non-2xx replies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorReply {
    pub error: String,
}
