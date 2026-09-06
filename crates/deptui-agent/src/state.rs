//! Persistent agent state: cache-plus-runtime-state in one JSON file.
//!
//! Deletable at worst cost of one redundant re-deploy check. Pause
//! flags live here (not in config) so they survive restarts and work
//! even when the config is NixOS-managed and read-only. Written
//! atomically (tmp + rename) so a crash mid-write can't truncate it.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use deptui_core::agentwire::OfflineKind;
use serde::{Deserialize, Serialize};

/// Bump when the layout changes incompatibly; older files are discarded
/// with a warning rather than misread.
const SCHEMA: u32 = 1;
/// Runs kept per watch.
pub const MAX_HISTORY: usize = 50;
/// Log lines kept per run — matches the TUI's job-log cap.
pub const MAX_RUN_LOG: usize = 2000;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AgentState {
    #[serde(default)]
    pub schema: u32,
    /// Global pause: no automatic deploys at all.
    #[serde(default)]
    pub paused: bool,
    /// Monotonic run id source.
    #[serde(default)]
    pub next_run_id: u64,
    #[serde(default)]
    pub watches: BTreeMap<String, WatchState>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct WatchState {
    #[serde(default)]
    pub paused: bool,
    /// Commit the watched ref pointed at when we last looked.
    #[serde(default)]
    pub last_seen: Option<String>,
    #[serde(default)]
    pub hosts: BTreeMap<String, HostState>,
    /// Most recent runs, newest last.
    #[serde(default)]
    pub history: VecDeque<RunRecord>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct HostState {
    #[serde(default)]
    pub paused: bool,
    /// Last successfully deployed revision.
    #[serde(default)]
    pub deployed: Option<Stamp>,
    /// Set when the last deploy of this host failed; cleared by the next
    /// success, and by a later round that leaves the host *pending*
    /// (offline/denied) — that round is by definition a newer
    /// revision or an approval, either of which ends the park, and a
    /// stale "failed at R" next to "pending at S" left the user unable
    /// to tell which one was current.
    #[serde(default)]
    pub failed: Option<FailStamp>,
    /// What ssh last said when it could not get in: set by the startup
    /// probe, by an offline/denied run outcome, and by a recheck that
    /// still fails; cleared by a recheck that answers and by any
    /// successful deploy. Never stale next to a fresh `deployed`.
    #[serde(default)]
    pub unreachable: Option<String>,
    /// The human's standing ok for a held/unadopted host: at the next
    /// update round the agent may deploy — accepting that this moves
    /// the host off whatever generation was made outside the watched
    /// repo. Consumed by the first successful deploy; revocable until
    /// then.
    #[serde(default)]
    pub approved: bool,
    /// First-encounter hold: the agent found this host running
    /// something *other* than the watched revision and refused to
    /// deploy over it. Cleared by a successful deploy (force or
    /// bootstrap) or a later equality adoption.
    #[serde(default)]
    pub held: Option<Stamp>,
    /// Set when the pre-deploy probe could not get in and `catch_up`
    /// is on: the update is pending, the daemon re-probes `target` at
    /// the watch's `offline_recheck` cadence and deploys the moment the
    /// host lets it in. Cleared by any later success or real failure.
    #[serde(default)]
    pub offline: Option<OfflineStamp>,
    /// What the agent's last successful deploy left on the host, per
    /// profile (remote store path read back after activation). The
    /// drift guard compares against this before the next deploy; an
    /// out-of-band change holds instead of overwriting. Empty = guard
    /// disarmed (pre-guard state files, or the read-back failed) —
    /// re-armed by the next successful deploy.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deployed_toplevels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stamp {
    pub rev: String,
    pub time: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfflineStamp {
    pub rev: String,
    pub time: u64,
    /// Resolved `user@host` ssh target, stored so rechecks don't need a
    /// clone + discovery round-trip.
    pub target: String,
    /// Down, or answered-and-refused, or answered-and-hung. Same
    /// pending machinery for all three, different story for the
    /// human: only `Down` resolves itself.
    #[serde(default)]
    pub kind: OfflineKind,
}

impl HostState {
    /// Fold one run outcome into the host's standing state. The single
    /// place the daemon and the oneshot `check` agree on what a
    /// success clears, what a failure parks, and what a pending round
    /// supersedes.
    pub fn apply_outcome(&mut self, hr: &HostRun, rev: &str, time: u64) {
        match hr.outcome.as_str() {
            "ok" | "adopted" => {
                self.deployed = Some(Stamp {
                    rev: rev.to_string(),
                    time,
                });
                self.failed = None;
                self.offline = None;
                self.held = None;
                // We just got in; whatever ssh said before is history.
                self.unreachable = None;
                // The ok was for this update; consumed.
                self.approved = false;
                self.deployed_toplevels = hr.toplevels.clone();
            }
            "held" => {
                self.held = Some(Stamp {
                    rev: rev.to_string(),
                    time,
                });
            }
            "failed" => {
                self.failed = Some(FailStamp {
                    rev: rev.to_string(),
                    time,
                    message: hr.message.clone().unwrap_or_default(),
                });
                self.offline = None;
                // The approval bought this round; a standing ok
                // surviving a failure would retry every poll.
                self.approved = false;
            }
            "offline" | "denied" | "stalled" => {
                self.offline = Some(OfflineStamp {
                    rev: rev.to_string(),
                    time,
                    target: hr.target.clone().unwrap_or_default(),
                    kind: OfflineKind::from_outcome(&hr.outcome).unwrap_or_default(),
                });
                // Reaching this round means the park at the older
                // revision is over (see `failed`).
                self.failed = None;
                self.unreachable = hr.message.clone();
            }
            // A cancel parks the host exactly like a failure — the run
            // must not quietly resume at the next poll — but the
            // message tells the user it was their call.
            "cancelled" => {
                self.failed = Some(FailStamp {
                    rev: rev.to_string(),
                    time,
                    message: hr.message.clone().unwrap_or_else(|| "cancelled".into()),
                });
                self.offline = None;
                self.approved = false;
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailStamp {
    pub rev: String,
    pub time: u64,
    pub message: String,
}

/// One deploy run of a watch (one detected update, all its hosts).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: u64,
    pub rev: String,
    /// What started it: `"poll"`, `"kick"`, or `"deploy <host>"`.
    pub trigger: String,
    pub started: u64,
    #[serde(default)]
    pub finished: Option<u64>,
    #[serde(default)]
    pub hosts: Vec<HostRun>,
    /// Combined log of the whole run, capped at [`MAX_RUN_LOG`].
    #[serde(default)]
    pub log: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostRun {
    pub host: String,
    /// `"ok"`, `"failed"`, `"offline"`, or `"skipped"`.
    pub outcome: String,
    #[serde(default)]
    pub message: Option<String>,
    /// For `"offline"`: the resolved ssh target, so the daemon can
    /// store it in [`OfflineStamp`] without re-resolving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// For `"ok"`/`"adopted"`: the remote profile paths the run left
    /// (or found) active, recorded into
    /// [`HostState::deployed_toplevels`]. Empty when the read-back
    /// failed — the guard disarms rather than holding on stale data.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub toplevels: BTreeMap<String, String>,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl AgentState {
    pub fn watch_mut(&mut self, name: &str) -> &mut WatchState {
        self.watches.entry(name.to_string()).or_default()
    }

    pub fn take_run_id(&mut self) -> u64 {
        self.next_run_id += 1;
        self.next_run_id
    }

    /// Append a finished run, pruning history beyond [`MAX_HISTORY`].
    pub fn push_run(&mut self, watch: &str, record: RunRecord) {
        let w = self.watch_mut(watch);
        w.history.push_back(record);
        while w.history.len() > MAX_HISTORY {
            w.history.pop_front();
        }
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let path = state_path(dir);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    schema: SCHEMA,
                    ..Default::default()
                })
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        match serde_json::from_str::<AgentState>(&text) {
            Ok(mut s) if s.schema == SCHEMA => {
                s.drop_superseded_parks();
                Ok(s)
            }
            Ok(s) => {
                tracing::warn!(
                    "discarding state file with schema {} (expected {SCHEMA})",
                    s.schema
                );
                Ok(Self {
                    schema: SCHEMA,
                    ..Default::default()
                })
            }
            Err(e) => {
                tracing::warn!("discarding unreadable state file: {e}");
                Ok(Self {
                    schema: SCHEMA,
                    ..Default::default()
                })
            }
        }
    }

    /// A `failed` stamp next to a pending `offline` stamp is the older
    /// of the two: the pending round could only start because a newer
    /// revision arrived or the human approved. Files written before
    /// `apply_outcome` cleared it on the way in still carry both.
    fn drop_superseded_parks(&mut self) {
        for ws in self.watches.values_mut() {
            for hs in ws.hosts.values_mut() {
                if hs.offline.is_some() {
                    hs.failed = None;
                }
            }
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating state dir {}", dir.display()))?;
        let path = state_path(dir);
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).context("serialising agent state")?;
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("renaming state file into place at {}", path.display()))?;
        Ok(())
    }
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("state.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(outcome: &str, message: Option<&str>) -> HostRun {
        HostRun {
            host: "web".into(),
            outcome: outcome.into(),
            message: message.map(str::to_string),
            target: Some("root@web.lan".into()),
            toplevels: Default::default(),
        }
    }

    #[test]
    fn pending_round_supersedes_older_park() {
        let mut hs = HostState::default();
        hs.apply_outcome(&run("failed", Some("boom")), "aaaa", 1);
        assert_eq!(hs.failed.as_ref().unwrap().rev, "aaaa");

        // A newer revision finds the host down: pending, park over.
        hs.apply_outcome(&run("offline", Some("Connection refused")), "bbbb", 2);
        assert!(hs.failed.is_none(), "stale failure survived: {hs:?}");
        let off = hs.offline.as_ref().unwrap();
        assert_eq!(off.rev, "bbbb");
        assert_eq!(off.kind, OfflineKind::Down);
        assert_eq!(hs.unreachable.as_deref(), Some("Connection refused"));

        // Locked out: same pending shape, flagged so it isn't drawn
        // as a sleeping host.
        hs.apply_outcome(
            &run("denied", Some("Permission denied (publickey)")),
            "bbbb",
            3,
        );
        assert_eq!(hs.offline.as_ref().unwrap().kind, OfflineKind::Denied);
        hs.apply_outcome(&run("stalled", Some("banner exchange")), "bbbb", 3);
        assert_eq!(hs.offline.as_ref().unwrap().kind, OfflineKind::Stalled);
        hs.apply_outcome(
            &run("denied", Some("Permission denied (publickey)")),
            "bbbb",
            3,
        );
        assert_eq!(
            hs.unreachable.as_deref(),
            Some("Permission denied (publickey)")
        );

        // A success clears every standing complaint, ssh's included.
        hs.apply_outcome(&run("ok", None), "bbbb", 4);
        assert!(hs.offline.is_none());
        assert!(hs.unreachable.is_none());
        assert_eq!(hs.deployed.as_ref().unwrap().rev, "bbbb");
    }

    #[test]
    fn failure_after_pending_parks_and_clears_pending() {
        let mut hs = HostState::default();
        hs.apply_outcome(&run("offline", Some("down")), "aaaa", 1);
        hs.apply_outcome(&run("failed", Some("boom")), "aaaa", 2);
        assert!(hs.offline.is_none());
        assert_eq!(hs.failed.as_ref().unwrap().message, "boom");
    }

    #[test]
    fn load_drops_park_superseded_by_pending() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = AgentState::load(dir.path()).unwrap();
        s.watch_mut("infra").hosts.insert(
            "web".into(),
            HostState {
                failed: Some(FailStamp {
                    rev: "aaaa".into(),
                    time: 1,
                    message: "boom".into(),
                }),
                offline: Some(OfflineStamp {
                    rev: "bbbb".into(),
                    time: 2,
                    target: "root@web.lan".into(),
                    kind: OfflineKind::Down,
                }),
                ..Default::default()
            },
        );
        s.save(dir.path()).unwrap();
        let s2 = AgentState::load(dir.path()).unwrap();
        let hs = &s2.watches["infra"].hosts["web"];
        assert!(hs.failed.is_none());
        assert_eq!(hs.offline.as_ref().unwrap().rev, "bbbb");
    }

    #[test]
    fn roundtrip_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = AgentState::load(dir.path()).unwrap();
        assert!(s.watches.is_empty());
        s.paused = true;
        s.watch_mut("infra").last_seen = Some("abc".into());
        s.watch_mut("infra").hosts.insert(
            "web".into(),
            HostState {
                deployed: Some(Stamp {
                    rev: "abc".into(),
                    time: 1,
                }),
                ..Default::default()
            },
        );
        s.save(dir.path()).unwrap();
        let s2 = AgentState::load(dir.path()).unwrap();
        assert!(s2.paused);
        assert_eq!(s2.watches["infra"].last_seen.as_deref(), Some("abc"));
        assert_eq!(
            s2.watches["infra"].hosts["web"]
                .deployed
                .as_ref()
                .unwrap()
                .rev,
            "abc"
        );
    }

    #[test]
    fn schema_mismatch_discards() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("state.json"),
            r#"{"schema": 999, "paused": true}"#,
        )
        .unwrap();
        let s = AgentState::load(dir.path()).unwrap();
        assert!(!s.paused, "stale-schema state must be discarded");
    }

    #[test]
    fn history_is_pruned() {
        let mut s = AgentState::default();
        for i in 0..(MAX_HISTORY as u64 + 7) {
            let id = s.take_run_id();
            s.push_run(
                "w",
                RunRecord {
                    id,
                    rev: format!("r{i}"),
                    trigger: "poll".into(),
                    started: i,
                    finished: Some(i),
                    hosts: vec![],
                    log: vec![],
                },
            );
        }
        let h = &s.watches["w"].history;
        assert_eq!(h.len(), MAX_HISTORY);
        assert_eq!(h.front().unwrap().rev, "r7");
    }
}
