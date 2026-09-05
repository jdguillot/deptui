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
    /// success. A host failed at revision R is skipped until a newer
    /// revision arrives (or a human force-deploys it).
    #[serde(default)]
    pub failed: Option<FailStamp>,
    /// Non-empty when the startup reachability probe failed; the message
    /// is what ssh said.
    #[serde(default)]
    pub unreachable: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stamp {
    pub rev: String,
    pub time: u64,
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
    /// `"ok"`, `"failed"`, or `"skipped"`.
    pub outcome: String,
    #[serde(default)]
    pub message: Option<String>,
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
            Ok(s) if s.schema == SCHEMA => Ok(s),
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
        assert_eq!(s2.watches["infra"].hosts["web"].deployed.as_ref().unwrap().rev, "abc");
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
