//! The daemon orchestrator: one task owns all mutable state and serves
//! commands from the API over an mpsc channel, mirroring the TUI's
//! "channel is the seam" convention. Deploy runs execute on a spawned
//! task so status/pause/tail stay responsive mid-deploy; state writes
//! all happen here, single-writer.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use deptui_core::host::build_ssh_target;

use crate::config::{AgentConfig, Cadence, WatchConfig};
use crate::notify::{self, Event};
use crate::runner::{self, check_reachable, RunPlan};
use crate::state::{now_unix, AgentState};
use crate::wire;

/// Commands the API (and CLI verbs behind it) can send.
pub enum Cmd {
    Status(oneshot::Sender<wire::AgentStatus>),
    History {
        watch: Option<String>,
        reply: oneshot::Sender<Result<Vec<wire::RunSummary>, String>>,
    },
    RunLog {
        watch: String,
        /// Defaults to the newest run.
        run: Option<u64>,
        reply: oneshot::Sender<Result<Vec<String>, String>>,
    },
    Kick {
        watch: Option<String>,
        reply: oneshot::Sender<Result<String, String>>,
    },
    SetPaused {
        scope: Scope,
        paused: bool,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Grant (or revoke) the standing ok for a held/unadopted host:
    /// the next update round may deploy it, moving it off any
    /// generation made outside the watched repo. The agent never
    /// deploys immediately on this — immediacy is the TUI's job.
    Approve {
        watch: Option<String>,
        host: String,
        revoke: bool,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Run the validation walk in the daemon's own context (clones,
    /// identity, ssh config all belong to it — not to whoever invoked
    /// the CLI over ssh). Refused while a run is in flight; polls are
    /// deferred while it runs.
    Validate {
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Internal: the spawned validation finished.
    ValidateDone,
    /// Stop the run in flight: signals the deploy's process group and
    /// parks everything the run was going to do at that revision.
    CancelRun {
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Internal: a spawned run finished.
    RunDone {
        watch: String,
        record: crate::state::RunRecord,
    },
}

#[derive(Debug, Clone)]
pub enum Scope {
    Global,
    Watch(String),
    Host(String),
}

struct RunningRun {
    watch: String,
    rev: String,
    trigger: String,
    started: u64,
    run_id: u64,
    cancel_tx: tokio::sync::watch::Sender<bool>,
    _task: JoinHandle<()>,
}

pub struct Daemon {
    cfg: Arc<AgentConfig>,
    state: AgentState,
    cadences: BTreeMap<String, Cadence>,
    /// Next scheduled poll per watch.
    next_poll: BTreeMap<String, Instant>,
    /// Next offline-host re-probe per watch, present only while some
    /// host of that watch has a pending (offline) update.
    recheck_at: BTreeMap<String, Instant>,
    running: Option<RunningRun>,
    /// A validation walk is in flight; polls queue behind it so git
    /// operations on the clones can't race.
    validating: bool,
    /// Self-restart-on-update: when the installed unit's ExecStart
    /// names a different binary than the one running, exit cleanly at
    /// the next idle moment and let systemd (Restart=always) start
    /// the new version. Enabled by DEPTUI_AGENT_SELF_RESTART.
    self_restart: bool,
    next_self_check: Instant,
    /// Raw bytes of the config file as loaded at startup — the
    /// baseline the self-restart check compares against.
    config_snapshot: Option<Vec<u8>>,
    /// Set when an update was detected while busy — re-checked as
    /// soon as the daemon goes idle.
    restart_wanted: bool,
    /// Watches that asked for a poll while a run was in flight
    /// (coalescing) or explicitly via kick.
    pending: Vec<(String, String)>, // (watch, trigger)
    pub log_tx: broadcast::Sender<String>,
    cmd_tx: mpsc::Sender<Cmd>,
    cmd_rx: mpsc::Receiver<Cmd>,
    /// Our default identity's public half, read once at startup.
    pubkey: Option<String>,
}

impl Daemon {
    pub fn new(cfg: Arc<AgentConfig>) -> Result<Self> {
        let state = AgentState::load(&cfg.state_dir)?;
        let mut cadences = BTreeMap::new();
        let mut next_poll = BTreeMap::new();
        let now = Instant::now();
        for w in &cfg.watches {
            let cadence = w.cadence()?; // validated at load; keep the Result anyway
                                        // The first poll follows the configured cadence. Starting
                                        // (or restarting) the agent is NOT a deploy trigger — only
                                        // the schedule, a kick, and offline catch-up are; an eager
                                        // startup poll deployed the instant a fresh agent came up,
                                        // which is exactly when a human least expects it.
            let first = match &cadence {
                Cadence::Every(d) => now + *d,
                Cadence::Cron(sched) => {
                    let cnow = chrono::Utc::now();
                    match sched.after(&cnow).next() {
                        Some(t) => now + (t - cnow).to_std().unwrap_or(Duration::from_secs(60)),
                        None => now + Duration::from_secs(86_400 * 365),
                    }
                }
            };
            next_poll.insert(w.name.clone(), first);
            cadences.insert(w.name.clone(), cadence);
        }
        let config_snapshot = cfg.source_path.as_ref().and_then(|p| std::fs::read(p).ok());
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let (log_tx, _) = broadcast::channel(1024);
        let pubkey = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .map(|h| h.join(".ssh/id_ed25519.pub"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string());
        Ok(Self {
            cfg,
            state,
            cadences,
            next_poll,
            recheck_at: BTreeMap::new(),
            running: None,
            validating: false,
            self_restart: std::env::var_os("DEPTUI_AGENT_SELF_RESTART").is_some(),
            next_self_check: now + self_check_interval(),
            config_snapshot,
            restart_wanted: false,
            pending: Vec::new(),
            log_tx,
            cmd_tx,
            cmd_rx,
            pubkey,
        })
    }

    pub fn handle(&self) -> mpsc::Sender<Cmd> {
        self.cmd_tx.clone()
    }

    fn watch_cfg(&self, name: &str) -> Option<&WatchConfig> {
        self.cfg.watches.iter().find(|w| w.name == name)
    }

    fn save_state(&self) {
        if let Err(e) = self.state.save(&self.cfg.state_dir) {
            tracing::error!("saving state failed: {e:#}");
        }
    }

    /// Reschedule the next poll of `watch` from its cadence.
    fn reschedule(&mut self, watch: &str) {
        let Some(cadence) = self.cadences.get(watch) else {
            return;
        };
        let next = match cadence {
            Cadence::Every(d) => Instant::now() + *d,
            Cadence::Cron(sched) => {
                let now = chrono::Utc::now();
                match sched.after(&now).next() {
                    Some(t) => {
                        let delta = (t - now).to_std().unwrap_or(Duration::from_secs(60));
                        Instant::now() + delta
                    }
                    // A cron with no future firing (e.g. a past year
                    // field): park it far away instead of spinning.
                    None => Instant::now() + Duration::from_secs(86_400 * 365),
                }
            }
        };
        self.next_poll.insert(watch.to_string(), next);
    }

    fn idle(&self) -> bool {
        self.running.is_none() && !self.validating && self.pending.is_empty()
    }

    /// Periodic update check: returns true when the daemon should exit
    /// (cleanly) so systemd restarts it with the new binary *or* the
    /// new config — activation deliberately never restarts the unit
    /// (self-deploy safety), so a changed config would otherwise sit
    /// unread in a store path the running process never looks at.
    /// Busy → remember and hand over the moment the work finishes.
    fn self_check_due(&mut self) -> bool {
        if !self.self_restart || Instant::now() < self.next_self_check {
            return false;
        }
        self.next_self_check = Instant::now() + self_check_interval();
        let unit = std::env::var("DEPTUI_AGENT_UNIT")
            .unwrap_or_else(|_| "/etc/systemd/system/deptui-agent.service".to_string());
        let unit = std::path::Path::new(&unit);
        let what = if updated_exe(unit).is_some() {
            "binary"
        } else if self.updated_config(unit) {
            "config"
        } else {
            return false;
        };
        if self.idle() {
            tracing::info!(
                "updated agent {what} detected — exiting for systemd to restart into it"
            );
            return true;
        }
        tracing::info!("updated agent {what} detected — restarting once the current work finishes");
        self.restart_wanted = true;
        false
    }

    /// Has the config the unit would start with diverged from what this
    /// process loaded at startup? Content compare, not path compare: a
    /// NixOS switch puts a *new store path* in ExecStart's `--config`,
    /// an in-place edit keeps the path — both must hand over. An
    /// unreadable file never triggers (a half-provisioned boot must not
    /// restart-loop).
    fn updated_config(&self, unit_path: &std::path::Path) -> bool {
        let Some(loaded) = &self.config_snapshot else {
            return false;
        };
        let target = exec_start_config(unit_path).or_else(|| self.cfg.source_path.clone());
        let Some(target) = target else {
            return false;
        };
        match std::fs::read(&target) {
            Ok(now) => now != *loaded,
            Err(_) => false,
        }
    }

    /// The soonest scheduled poll or offline recheck, for the select!
    /// sleep.
    fn earliest_poll(&self) -> Instant {
        let base = self
            .next_poll
            .values()
            .chain(self.recheck_at.values())
            .min()
            .copied()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        if self.self_restart {
            base.min(self.next_self_check)
        } else {
            base
        }
    }

    /// Arm (or re-arm) the offline recheck timer for a watch.
    fn schedule_recheck(&mut self, watch: &str) {
        let Some(wcfg) = self.watch_cfg(watch) else {
            return;
        };
        let cadence = wcfg
            .offline_recheck()
            .unwrap_or_else(|_| Duration::from_secs(120));
        self.recheck_at
            .insert(watch.to_string(), Instant::now() + cadence);
    }

    pub async fn run(mut self) -> Result<()> {
        tracing::info!(
            "deptui-agent {} up: {} watch(es), socket {}",
            wire::AGENT_VERSION,
            self.cfg.watches.len(),
            self.cfg.socket.display()
        );
        self.startup_validation().await;
        // Offline markers survive restarts; resume their rechecks.
        let resumed: Vec<String> = self
            .state
            .watches
            .iter()
            .filter(|(_, ws)| ws.hosts.values().any(|h| h.offline.is_some()))
            .map(|(name, _)| name.clone())
            .collect();
        for w in resumed {
            self.schedule_recheck(&w);
        }
        loop {
            let deadline = self.earliest_poll();
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    self.handle_cmd(cmd).await;
                    if self.restart_wanted && self.idle() {
                        tracing::info!(
                            "updated agent binary detected — restarting into it now that the run is done"
                        );
                        break;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    self.poll_due().await;
                    self.recheck_due().await;
                    if self.self_check_due() {
                        break;
                    }
                }
                _ = shutdown_signal() => {
                    tracing::info!("shutting down");
                    break;
                }
            }
        }
        self.save_state();
        Ok(())
    }

    /// Warn early about targets that will fail at deploy time: try a
    /// BatchMode ssh true against every configured host we can resolve.
    /// Needs a clone to discover node hostnames, so watches never
    /// cloned yet are skipped — their first run surfaces the problem.
    async fn startup_validation(&mut self) {
        for w in &self.cfg.watches {
            let dir = crate::gitwatch::clone_dir(&self.cfg.state_dir, &w.name);
            if !dir.join(".git").exists() {
                continue;
            }
            let Some(flake_ref) = dir.to_str().map(str::to_string) else {
                continue;
            };
            let nodes = match deptui_core::flake::discover(&flake_ref).await {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!("startup validation: discovery in {} failed: {e:#}", w.name);
                    continue;
                }
            };
            for (host, hc) in &w.hosts {
                let Some(node) = nodes.iter().find(|n| n.name == *host) else {
                    continue;
                };
                let target = build_ssh_target(node, "system", &hc.ssh_override());
                let result = check_reachable(&target, &hc.ssh_override()).await;
                let entry = self
                    .state
                    .watch_mut(&w.name)
                    .hosts
                    .entry(host.clone())
                    .or_default();
                match result {
                    Ok(()) => entry.unreachable = None,
                    Err(u) => {
                        tracing::warn!("{host} ({target}) {}", u.headline());
                        entry.unreachable = Some(u.message.clone());
                        // A standing pending marker learns the fresh
                        // verdict: a host that came up but now refuses
                        // us must stop being drawn as asleep.
                        if let Some(off) = entry.offline.as_mut() {
                            off.denied = u.denied;
                        }
                        notify::dispatch(
                            &self.cfg.notify,
                            Event::new("unreachable", &w.name, Some(host), "", u.headline()),
                        );
                    }
                }
            }
        }
        self.save_state();
    }

    async fn handle_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Status(reply) => {
                let _ = reply.send(self.status());
            }
            Cmd::History { watch, reply } => {
                let _ = reply.send(self.history(watch));
            }
            Cmd::RunLog { watch, run, reply } => {
                let _ = reply.send(self.run_log(&watch, run));
            }
            Cmd::Kick { watch, reply } => {
                let _ = reply.send(self.kick(watch).await);
            }
            Cmd::SetPaused {
                scope,
                paused,
                reply,
            } => {
                let _ = reply.send(self.set_paused(scope, paused));
            }
            Cmd::Approve {
                watch,
                host,
                revoke,
                reply,
            } => {
                let _ = reply.send(self.approve(watch, host, revoke));
            }
            Cmd::Validate { reply } => {
                if self.running.is_some() {
                    let _ = reply.send(Err(
                        "a run is in flight — validate after it finishes".to_string()
                    ));
                } else if self.validating {
                    let _ = reply.send(Err("a validation is already running".to_string()));
                } else {
                    self.validating = true;
                    let cfg = self.cfg.clone();
                    let cmd_tx = self.cmd_tx.clone();
                    tokio::spawn(async move {
                        let (report, failures) = crate::validation_report(&cfg).await;
                        let _ = reply.send(if failures == 0 {
                            Ok(report)
                        } else {
                            Err(format!("{report}\n{failures} validation failure(s)"))
                        });
                        let _ = cmd_tx.send(Cmd::ValidateDone).await;
                    });
                }
            }
            Cmd::ValidateDone => {
                self.validating = false;
                let pending = std::mem::take(&mut self.pending);
                for (w, trigger) in pending {
                    self.request_poll(&w, &trigger).await;
                    if self.running.is_some() {
                        return;
                    }
                }
            }
            Cmd::CancelRun { reply } => {
                let result = match &self.running {
                    Some(r) => {
                        let _ = r.cancel_tx.send(true);
                        Ok(format!(
                            "cancelling run #{} of watch `{}` — hosts it would have \
                             deployed stay parked at {} until a new revision, a kick \
                             after one, or a force-deploy",
                            r.run_id,
                            r.watch,
                            &r.rev[..r.rev.len().min(12)]
                        ))
                    }
                    None => Err("no run in progress".to_string()),
                };
                let _ = reply.send(result);
            }
            Cmd::RunDone { watch, record } => self.finish_run(watch, record).await,
        }
    }

    fn status(&self) -> wire::AgentStatus {
        let watches = self
            .cfg
            .watches
            .iter()
            .map(|w| {
                let ws = self.state.watches.get(&w.name);
                let now_inst = Instant::now();
                let next_poll = self.next_poll.get(&w.name).map(|t| {
                    let secs = t.saturating_duration_since(now_inst).as_secs();
                    now_unix() + secs
                });
                let running = self
                    .running
                    .as_ref()
                    .filter(|r| r.watch == w.name)
                    .map(|r| wire::RunningInfo {
                        rev: r.rev.clone(),
                        started: r.started,
                        trigger: r.trigger.clone(),
                    });
                let hosts = w
                    .hosts
                    .keys()
                    .map(|h| {
                        let hs = ws.and_then(|w| w.hosts.get(h)).cloned().unwrap_or_default();
                        wire::HostStatus {
                            name: h.clone(),
                            paused: hs.paused,
                            deployed_rev: hs.deployed.as_ref().map(|s| s.rev.clone()),
                            deployed_time: hs.deployed.as_ref().map(|s| s.time),
                            failed_rev: hs.failed.as_ref().map(|s| s.rev.clone()),
                            failed_time: hs.failed.as_ref().map(|s| s.time),
                            failed_message: hs.failed.as_ref().map(|s| s.message.clone()),
                            unreachable: hs.unreachable.clone(),
                            offline_rev: hs.offline.as_ref().map(|o| o.rev.clone()),
                            offline_time: hs.offline.as_ref().map(|o| o.time),
                            offline_denied: hs.offline.as_ref().is_some_and(|o| o.denied),
                            held_rev: hs.held.as_ref().map(|s| s.rev.clone()),
                            held_time: hs.held.as_ref().map(|s| s.time),
                            approved: hs.approved,
                        }
                    })
                    .collect();
                wire::WatchStatus {
                    name: w.name.clone(),
                    repo: w.repo.clone(),
                    ref_label: w.ref_label(),
                    paused: ws.map(|w| w.paused).unwrap_or(false),
                    last_seen: ws.and_then(|w| w.last_seen.clone()),
                    next_poll,
                    running,
                    hosts,
                }
            })
            .collect();
        wire::AgentStatus {
            version: wire::AGENT_VERSION.to_string(),
            paused: self.state.paused,
            pubkey: self.pubkey.clone(),
            watches,
        }
    }

    fn history(&self, watch: Option<String>) -> Result<Vec<wire::RunSummary>, String> {
        let mut out = Vec::new();
        for (name, ws) in &self.state.watches {
            if let Some(w) = &watch {
                if w != name {
                    continue;
                }
            }
            for r in &ws.history {
                out.push(crate::wire::summary_from_record(name, r));
            }
        }
        if let Some(w) = &watch {
            if !self.state.watches.contains_key(w) && self.watch_cfg(w).is_none() {
                return Err(format!("unknown watch `{w}`"));
            }
        }
        out.sort_by_key(|r| std::cmp::Reverse(r.started));
        Ok(out)
    }

    fn run_log(&self, watch: &str, run: Option<u64>) -> Result<Vec<String>, String> {
        let ws = self
            .state
            .watches
            .get(watch)
            .ok_or_else(|| format!("unknown watch `{watch}`"))?;
        let record = match run {
            Some(id) => ws.history.iter().find(|r| r.id == id),
            None => ws.history.back(),
        };
        record
            .map(|r| r.log.clone())
            .ok_or_else(|| "no such run".to_string())
    }

    async fn kick(&mut self, watch: Option<String>) -> Result<String, String> {
        let names: Vec<String> = match watch {
            Some(w) => {
                if self.watch_cfg(&w).is_none() {
                    return Err(format!("unknown watch `{w}`"));
                }
                vec![w]
            }
            None => self.cfg.watches.iter().map(|w| w.name.clone()).collect(),
        };
        let mut kicked = Vec::new();
        for name in names {
            self.request_poll(&name, "kick").await;
            kicked.push(name);
        }
        Ok(format!("kicked: {}", kicked.join(", ")))
    }

    fn set_paused(&mut self, scope: Scope, paused: bool) -> Result<String, String> {
        let verb = if paused { "paused" } else { "resumed" };
        // Pause gates *future* polls only; a run in flight keeps going
        // by design. Say so, or pause reads as a broken stop button.
        let running_note = if paused && self.running.is_some() {
            " (a run is in flight and will finish — use `cancel` to stop it)"
        } else {
            ""
        };
        let msg = match scope {
            Scope::Global => {
                self.state.paused = paused;
                format!("agent {verb}{running_note}")
            }
            Scope::Watch(w) => {
                if self.watch_cfg(&w).is_none() {
                    return Err(format!("unknown watch `{w}`"));
                }
                self.state.watch_mut(&w).paused = paused;
                format!("watch `{w}` {verb}{running_note}")
            }
            Scope::Host(h) => {
                let watches: Vec<String> = self
                    .cfg
                    .watches
                    .iter()
                    .filter(|w| w.hosts.contains_key(&h))
                    .map(|w| w.name.clone())
                    .collect();
                if watches.is_empty() {
                    return Err(format!("no watch configures host `{h}`"));
                }
                for w in &watches {
                    self.state
                        .watch_mut(w)
                        .hosts
                        .entry(h.clone())
                        .or_default()
                        .paused = paused;
                }
                format!("host `{h}` {verb} in {}{running_note}", watches.join(", "))
            }
        };
        self.save_state();
        Ok(msg)
    }

    /// Record (or revoke) the adoption ok. No run starts here: the
    /// approval is consumed by the next scheduled poll, kick, or
    /// offline catch-up that finds the host eligible.
    fn approve(
        &mut self,
        watch: Option<String>,
        host: String,
        revoke: bool,
    ) -> Result<String, String> {
        let watch_name = match watch {
            Some(w) => w,
            None => {
                let mut owners = self
                    .cfg
                    .watches
                    .iter()
                    .filter(|w| w.hosts.contains_key(&host))
                    .map(|w| w.name.clone());
                let first = owners
                    .next()
                    .ok_or_else(|| format!("no watch configures host `{host}`"))?;
                if owners.next().is_some() {
                    return Err(format!(
                        "host `{host}` appears in multiple watches — pass --watch"
                    ));
                }
                first
            }
        };
        if !self
            .cfg
            .watches
            .iter()
            .any(|w| w.name == watch_name && w.hosts.contains_key(&host))
        {
            return Err(format!(
                "host `{host}` is not configured in watch `{watch_name}`"
            ));
        }
        let hs = self
            .state
            .watch_mut(&watch_name)
            .hosts
            .entry(host.clone())
            .or_default();
        let msg = if revoke {
            if !hs.approved {
                return Err(format!("`{host}` has no pending approval"));
            }
            hs.approved = false;
            format!("approval revoked — `{host}` holds again")
        } else if hs.approved {
            format!("`{host}` is already approved for the next update round")
        } else {
            hs.approved = true;
            format!(
                "approved: `{host}` takes the next update round — this will move it off \
                 any generation made outside the watched repo (revoke with --revoke)"
            )
        };
        self.save_state();
        Ok(msg)
    }

    async fn poll_due(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self
            .next_poll
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(n, _)| n.clone())
            .collect();
        for name in due {
            self.reschedule(&name);
            self.request_poll(&name, "poll").await;
        }
    }

    /// Re-probe the offline hosts of every watch whose recheck timer is
    /// due. A host that answers gets its marker cleared and the watch a
    /// "catch-up" poll — normal eligibility then deploys the coalesced
    /// newest revision. Hosts still down re-arm the timer.
    async fn recheck_due(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self
            .recheck_at
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(n, _)| n.clone())
            .collect();
        for watch in due {
            self.recheck_at.remove(&watch);
            let offline: Vec<(String, crate::state::OfflineStamp)> = self
                .state
                .watches
                .get(&watch)
                .map(|ws| {
                    ws.hosts
                        .iter()
                        .filter_map(|(h, hs)| hs.offline.clone().map(|o| (h.clone(), o)))
                        .collect()
                })
                .unwrap_or_default();
            if offline.is_empty() {
                continue;
            }
            let overrides: BTreeMap<String, deptui_core::ssh::SshOverride> = self
                .watch_cfg(&watch)
                .map(|w| {
                    w.hosts
                        .iter()
                        .map(|(h, hc)| (h.clone(), hc.ssh_override()))
                        .collect()
                })
                .unwrap_or_default();
            let mut back = Vec::new();
            let mut still_down: Vec<(String, crate::runner::Unreachable)> = Vec::new();
            for (host, stamp) in &offline {
                let override_ = overrides.get(host).cloned().unwrap_or_default();
                match check_reachable(&stamp.target, &override_).await {
                    Ok(()) => back.push(host.clone()),
                    Err(u) => still_down.push((host.clone(), u)),
                }
            }
            {
                // Every probe's verdict lands in state: a host that
                // answers sheds its marker and ssh's last words; one
                // that doesn't keeps the freshest reason (and whether
                // it is a lockout now rather than a sleep).
                let ws = self.state.watch_mut(&watch);
                for host in &back {
                    if let Some(hs) = ws.hosts.get_mut(host) {
                        hs.offline = None;
                        hs.unreachable = None;
                    }
                }
                for (host, u) in &still_down {
                    if let Some(hs) = ws.hosts.get_mut(host) {
                        if let Some(off) = hs.offline.as_mut() {
                            off.denied = u.denied;
                        }
                        hs.unreachable = Some(u.message.clone());
                    }
                }
            }
            if !back.is_empty() {
                self.save_state();
                tracing::info!(
                    "watch {watch}: {} back online — catching up",
                    back.join(", ")
                );
                self.request_poll(&watch, "catch-up").await;
            }
            if !still_down.is_empty() {
                self.save_state();
                self.schedule_recheck(&watch);
            }
        }
    }

    /// Poll one watch now; start a run when there's something to do.
    async fn request_poll(&mut self, watch: &str, trigger: &str) {
        if self.running.is_some() || self.validating {
            // Coalesce: remember the watch, re-poll when the run ends.
            if !self.pending.iter().any(|(w, _)| w == watch) {
                self.pending.push((watch.to_string(), trigger.to_string()));
            }
            return;
        }
        if self.state.paused {
            tracing::debug!("skipping poll of {watch}: agent paused");
            return;
        }
        if self
            .state
            .watches
            .get(watch)
            .map(|w| w.paused)
            .unwrap_or(false)
        {
            tracing::debug!("skipping poll of {watch}: watch paused");
            return;
        }
        let Some(wcfg) = self.watch_cfg(watch) else {
            return;
        };
        let refspec = wcfg.refspec();
        let rev = match crate::gitwatch::ls_remote(&wcfg.repo, &refspec).await {
            Ok(Some(rev)) => rev,
            Ok(None) => {
                tracing::warn!("watch {watch}: {refspec} not found in {}", wcfg.repo);
                return;
            }
            Err(e) => {
                tracing::warn!("watch {watch}: poll failed: {e:#}");
                return;
            }
        };
        let ws = self.state.watch_mut(watch);
        let changed = ws.last_seen.as_deref() != Some(rev.as_str());
        ws.last_seen = Some(rev.clone());

        // Which hosts need this revision?
        let wcfg = self.watch_cfg(watch).expect("checked above");
        let mut hosts = Vec::new();
        let ws = self.state.watches.get(watch).expect("just created");
        for name in wcfg.hosts.keys() {
            let hs = ws.hosts.get(name).cloned().unwrap_or_default();
            if hs.paused {
                continue;
            }
            if hs.deployed.as_ref().map(|s| s.rev.as_str()) == Some(rev.as_str()) {
                continue;
            }
            // No same-commit retry: a host that failed at this revision
            // waits for a new one — unless the human approved, which
            // buys exactly one more round (consumed by that round's
            // outcome, so a persistent failure can't retry-storm).
            // Without this, a host cancelled at the tip revision was
            // stuck: approve did nothing and no force-deploy exists.
            if !hs.approved && hs.failed.as_ref().map(|s| s.rev.as_str()) == Some(rev.as_str()) {
                continue;
            }
            // Held at this revision: the human hasn't blessed the agent
            // for this host yet. A *new* revision re-probes (the host
            // may have caught up), same one waits — unless the human
            // approved, which is exactly the blessing.
            if !hs.approved && hs.held.as_ref().map(|s| s.rev.as_str()) == Some(rev.as_str()) {
                continue;
            }
            hosts.push(crate::runner::PlanHost {
                name: name.clone(),
                // Unadopted = never successfully deployed by this
                // agent. A past failure or cancel does NOT count as
                // adoption — losing hold protection over a cancelled
                // rollback attempt would be exactly backwards. An
                // approval turns the probe-and-hold into a real deploy.
                adopt: !hs.approved && hs.deployed.is_none(),
                // Approval bypasses the drift guard the same way it
                // bypasses the holds: empty baseline = guard off for
                // the one round the approval buys.
                recorded_toplevels: if hs.approved {
                    Default::default()
                } else {
                    hs.deployed_toplevels.clone()
                },
            });
        }
        if hosts.is_empty() {
            let short = &rev[..rev.len().min(12)];
            if changed {
                tracing::info!("watch {watch}: {short} needs no deploys");
            }
            // A kick that ends in "nothing to do" must say so in the
            // tail — an ack in the footer plus a silent log reads as a
            // dead button in the TUI. Scheduled polls stay quiet
            // unless the head actually moved.
            if trigger != "poll" || changed {
                let ws = self.state.watches.get(watch);
                let mut parts: Vec<String> = Vec::new();
                if let Some(wcfg) = self.watch_cfg(watch) {
                    for name in wcfg.hosts.keys() {
                        let Some(hs) = ws.and_then(|w| w.hosts.get(name)) else {
                            continue;
                        };
                        let at = |s: &Option<crate::state::Stamp>| {
                            s.as_ref().map(|s| s.rev.as_str()) == Some(rev.as_str())
                        };
                        let why = if hs.paused {
                            "paused"
                        } else if at(&hs.deployed) {
                            "already deployed"
                        } else if hs.failed.as_ref().map(|s| s.rev.as_str()) == Some(rev.as_str()) {
                            "parked at this revision (approve, or push a new commit)"
                        } else if at(&hs.held) {
                            "held (approve to let the next round deploy)"
                        } else {
                            continue;
                        };
                        parts.push(format!("{name}: {why}"));
                    }
                }
                let detail = if parts.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", parts.join("; "))
                };
                let _ = self.log_tx.send(format!(
                    "[{watch}] {trigger}: {short} needs no deploys{detail}"
                ));
            }
            self.save_state();
            return;
        }
        let run_id = self.state.take_run_id();
        self.spawn_run(
            watch,
            RunPlan {
                run_id,
                rev,
                trigger: trigger.to_string(),
                hosts,
            },
        );
        self.save_state();
    }

    fn spawn_run(&mut self, watch: &str, plan: RunPlan) {
        let wcfg = self.watch_cfg(watch).expect("caller verified");
        let watch_owned = wcfg.clone();
        let notify_cfg = self.cfg.notify.clone();
        let state_dir: PathBuf = self.cfg.state_dir.clone();
        let log_tx = self.log_tx.clone();
        let cmd_tx = self.cmd_tx.clone();
        let name = watch.to_string();
        let rev = plan.rev.clone();
        let trigger = plan.trigger.clone();
        let run_id = plan.run_id;
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            let record = runner::execute(
                &state_dir,
                &watch_owned,
                &notify_cfg,
                plan,
                &log_tx,
                cancel_rx,
            )
            .await;
            let _ = cmd_tx
                .send(Cmd::RunDone {
                    watch: name,
                    record,
                })
                .await;
        });
        self.running = Some(RunningRun {
            watch: watch.to_string(),
            rev,
            trigger,
            started: now_unix(),
            run_id,
            cancel_tx,
            _task: task,
        });
    }

    async fn finish_run(&mut self, watch: String, record: crate::state::RunRecord) {
        let rev = record.rev.clone();
        let time = record.finished.unwrap_or_else(now_unix);
        {
            let ws = self.state.watch_mut(&watch);
            for hr in &record.hosts {
                ws.hosts
                    .entry(hr.host.clone())
                    .or_default()
                    .apply_outcome(hr, &rev, time);
            }
        }
        let had_offline = record
            .hosts
            .iter()
            .any(|h| matches!(h.outcome.as_str(), "offline" | "denied"));
        self.state.push_run(&watch, record);
        self.running = None;
        self.save_state();
        if had_offline {
            self.schedule_recheck(&watch);
        }
        // Coalesce: whatever queued up while we were deploying gets its
        // poll now — at most one deploy pipeline runs at a time.
        let pending = std::mem::take(&mut self.pending);
        for (w, trigger) in pending {
            self.request_poll(&w, &trigger).await;
            if self.running.is_some() {
                // A new run started; the rest stays queued.
                return;
            }
        }
    }
}

fn self_check_interval() -> Duration {
    std::env::var("DEPTUI_AGENT_SELF_CHECK_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(60))
}

/// The binary the INSTALLED unit would start, when it differs from the
/// one running. NixOS activation swaps the unit file but deliberately
/// leaves the running agent alone (self-deploy safety); this is how
/// the agent notices and hands over.
fn updated_exe(unit_path: &std::path::Path) -> Option<std::path::PathBuf> {
    let tokens = exec_start_tokens(unit_path)?;
    let expected = std::fs::canonicalize(tokens.first()?).ok()?;
    let current = std::fs::canonicalize("/proc/self/exe").ok()?;
    (!same_install(&expected, &current)).then_some(expected)
}

/// The whitespace-split ExecStart argv of the installed unit.
fn exec_start_tokens(unit_path: &std::path::Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(unit_path).ok()?;
    let line = text
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with("ExecStart="))?;
    Some(
        line.strip_prefix("ExecStart=")?
            .split_whitespace()
            .map(str::to_string)
            .collect(),
    )
}

/// The `--config <path>` the installed unit would start with, when it
/// names one.
fn exec_start_config(unit_path: &std::path::Path) -> Option<std::path::PathBuf> {
    let tokens = exec_start_tokens(unit_path)?;
    let idx = tokens.iter().position(|t| t == "--config")?;
    tokens.get(idx + 1).map(std::path::PathBuf::from)
}

/// Whether the unit's ExecStart and the running process come from the
/// same install. Compared by parent directory, not by file: the flake
/// wraps the binary (`wrapProgram`), so ExecStart names a wrapper
/// script at `$out/bin/deptui-agent` that execs
/// `$out/bin/.deptui-agent-wrapped` — the running exe never equals the
/// wrapper path, and a file comparison declared "updated binary" on
/// every check, restarting the agent once a minute forever.
fn same_install(expected: &std::path::Path, current: &std::path::Path) -> bool {
    expected == current
        || matches!((expected.parent(), current.parent()), (Some(a), Some(b)) if a == b)
}

/// SIGTERM (systemd stop) or ctrl-c.
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("installing SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{exec_start_config, same_install};
    use std::path::Path;

    #[test]
    fn exec_start_config_finds_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("a.service");
        std::fs::write(
            &unit,
            "[Service]\nExecStart=/nix/store/x/bin/deptui-agent --config /nix/store/y-config.toml run\n",
        )
        .unwrap();
        assert_eq!(
            exec_start_config(&unit),
            Some("/nix/store/y-config.toml".into())
        );
        std::fs::write(&unit, "[Service]\nExecStart=/bin/foo run\n").unwrap();
        assert_eq!(exec_start_config(&unit), None);
    }

    #[test]
    fn same_install_same_file() {
        let p = Path::new("/nix/store/aaa-deptui-agent-0.14.0/bin/deptui-agent");
        assert!(same_install(p, p));
    }

    #[test]
    fn same_install_wrapper_and_wrapped_sibling() {
        assert!(same_install(
            Path::new("/nix/store/aaa-deptui-agent-0.14.0/bin/deptui-agent"),
            Path::new("/nix/store/aaa-deptui-agent-0.14.0/bin/.deptui-agent-wrapped"),
        ));
    }

    #[test]
    fn same_install_different_store_paths() {
        assert!(!same_install(
            Path::new("/nix/store/bbb-deptui-agent-0.14.1/bin/deptui-agent"),
            Path::new("/nix/store/aaa-deptui-agent-0.14.0/bin/.deptui-agent-wrapped"),
        ));
    }
}
