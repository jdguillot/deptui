//! Wire types of the deptui-agent control API. Everything here is
//! `Serialize + Deserialize`: the agent speaks them as JSON on its
//! socket, and both `deptui-agent <verb> --json` and the TUI's remote
//! agent client parse them back. Living in core keeps the two sides of
//! the contract compiled from one definition.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatus {
    pub version: String,
    /// Global pause flag.
    pub paused: bool,
    /// The agent's ssh public key (its default identity), when it has
    /// one — what you authorize on the targets. Serving it over the
    /// API means `deptui-agent pubkey` (and the TUI) report the
    /// DAEMON's identity, not the invoking user's.
    #[serde(default)]
    pub pubkey: Option<String>,
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
    /// What ssh last said when the agent could not get in. Cleared by
    /// a successful deploy, so it never contradicts `deployed_*`.
    #[serde(default)]
    pub unreachable: Option<String>,
    /// Set when the pre-deploy probe could not get in (catch-up
    /// pending): the revision waiting to land and when that was.
    #[serde(default)]
    pub offline_rev: Option<String>,
    #[serde(default)]
    pub offline_time: Option<u64>,
    /// With `offline_rev`: the host answered and refused the agent
    /// (auth or host key) — it is up, the lockout needs a human.
    /// Absent from older agents, which report every miss as down.
    #[serde(default)]
    pub offline_denied: bool,
    /// First-encounter hold: the host runs something other than the
    /// watched revision and the agent refused to deploy over it.
    #[serde(default)]
    pub held_rev: Option<String>,
    #[serde(default)]
    pub held_time: Option<u64>,
    /// The human ok'd taking the next update round (adoption pending).
    #[serde(default)]
    pub approved: bool,
}

/// Per-host outcome inside a run: `"ok"`, `"adopted"`, `"held"`,
/// `"offline"`, `"denied"`, `"failed"`, `"cancelled"`, or `"skipped"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostRun {
    pub host: String,
    pub outcome: String,
    #[serde(default)]
    pub message: Option<String>,
}

/// One run in `GET /history` — a run record minus its log, which is
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
