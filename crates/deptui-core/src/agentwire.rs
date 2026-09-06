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

/// Why a pending host could not be reached, as the agent's probe
/// classified ssh's stderr. `Down` is the sleeping host the pending
/// machinery was built for; the other two mean the host *answered*
/// on port 22 and the problem is on the ssh layer — something for a
/// human, not a wait — so no view may draw them as asleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OfflineKind {
    /// Name resolution, connect, or route failure: nobody answered.
    #[default]
    Down,
    /// sshd answered and rejected the agent (auth or host key).
    Denied,
    /// TCP connected but the ssh handshake never completed: no banner
    /// within the timeout, or the connection was closed/reset during
    /// identification. A hung or overloaded sshd.
    Stalled,
}

impl OfflineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Denied => "denied",
            Self::Stalled => "stalled",
        }
    }

    /// The per-host run outcome string that records this kind.
    pub fn outcome(self) -> &'static str {
        match self {
            Self::Down => "offline",
            Self::Denied => "denied",
            Self::Stalled => "stalled",
        }
    }

    /// Inverse of [`Self::outcome`]; `None` for non-pending outcomes.
    pub fn from_outcome(outcome: &str) -> Option<Self> {
        match outcome {
            "offline" => Some(Self::Down),
            "denied" => Some(Self::Denied),
            "stalled" => Some(Self::Stalled),
            _ => None,
        }
    }
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
    /// (auth or host key). Superseded by `offline_kind`; kept so a
    /// 0.18 agent's verdict still reads (see [`Self::pending_kind`]).
    #[serde(default)]
    pub offline_denied: bool,
    /// With `offline_rev`: what kind of miss the probe saw. Absent
    /// from agents before 0.19, which only knew down vs denied.
    #[serde(default)]
    pub offline_kind: Option<OfflineKind>,
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

impl HostStatus {
    /// The kind of the pending miss, if an update is pending. Reads
    /// `offline_kind` and falls back to the older `offline_denied`
    /// flag, so the TUI needs no version gate for this field.
    pub fn pending_kind(&self) -> Option<OfflineKind> {
        self.offline_rev.as_ref()?;
        Some(self.offline_kind.unwrap_or(if self.offline_denied {
            OfflineKind::Denied
        } else {
            OfflineKind::Down
        }))
    }
}

/// Per-host outcome inside a run: `"ok"`, `"adopted"`, `"held"`,
/// `"offline"`, `"denied"`, `"stalled"`, `"failed"`, `"cancelled"`,
/// or `"skipped"` (see [`OfflineKind::outcome`] for the pending
/// three).
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
