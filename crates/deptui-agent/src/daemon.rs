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
use crate::state::{now_unix, AgentState, FailStamp, Stamp};
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
    ForceDeploy {
        watch: Option<String>,
        host: String,
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
    /// Watches that asked for a poll while a run was in flight
    /// (coalescing) or explicitly via kick.
    pending: Vec<(String, String)>, // (watch, trigger)
    pub log_tx: broadcast::Sender<String>,
    cmd_tx: mpsc::Sender<Cmd>,
    cmd_rx: mpsc::Receiver<Cmd>,
}

impl Daemon {
    pub fn new(cfg: Arc<AgentConfig>) -> Result<Self> {
        let state = AgentState::load(&cfg.state_dir)?;
        let mut cadences = BTreeMap::new();
        let mut next_poll = BTreeMap::new();
        let now = Instant::now();
        for w in &cfg.watches {
            let cadence = w.cadence()?; // validated at load; keep the Result anyway
                                        // First poll happens shortly after startup rather than a
                                        // full interval later — an agent restart shouldn't delay an
                                        // already-due update.
            next_poll.insert(w.name.clone(), now + Duration::from_secs(5));
            cadences.insert(w.name.clone(), cadence);
        }
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let (log_tx, _) = broadcast::channel(1024);
        Ok(Self {
            cfg,
            state,
            cadences,
            next_poll,
            recheck_at: BTreeMap::new(),
            running: None,
            pending: Vec::new(),
            log_tx,
            cmd_tx,
            cmd_rx,
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

    /// The soonest scheduled poll or offline recheck, for the select!
    /// sleep.
    fn earliest_poll(&self) -> Instant {
        self.next_poll
            .values()
            .chain(self.recheck_at.values())
            .min()
            .copied()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600))
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
                }
                _ = tokio::time::sleep_until(deadline) => {
                    self.poll_due().await;
                    self.recheck_due().await;
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
                    Err(msg) => {
                        tracing::warn!("{host} ({target}) unreachable: {msg}");
                        entry.unreachable = Some(msg.clone());
                        notify::dispatch(
                            &self.cfg.notify,
                            Event::new("unreachable", &w.name, Some(host), "", msg),
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
            Cmd::ForceDeploy { watch, host, reply } => {
                let _ = reply.send(self.force_deploy(watch, host));
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
        let msg = match scope {
            Scope::Global => {
                self.state.paused = paused;
                format!("agent {verb}")
            }
            Scope::Watch(w) => {
                if self.watch_cfg(&w).is_none() {
                    return Err(format!("unknown watch `{w}`"));
                }
                self.state.watch_mut(&w).paused = paused;
                format!("watch `{w}` {verb}")
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
                format!("host `{h}` {verb} in {}", watches.join(", "))
            }
        };
        self.save_state();
        Ok(msg)
    }

    /// Force-deploy one host at the watch's last-seen revision,
    /// bypassing pause flags and the failed-at marker.
    fn force_deploy(&mut self, watch: Option<String>, host: String) -> Result<String, String> {
        if self.running.is_some() {
            return Err("a run is already in progress".to_string());
        }
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
        let Some(wcfg) = self.watch_cfg(&watch_name) else {
            return Err(format!("unknown watch `{watch_name}`"));
        };
        if !wcfg.hosts.contains_key(&host) {
            return Err(format!(
                "host `{host}` is not configured in watch `{watch_name}`"
            ));
        }
        let Some(rev) = self
            .state
            .watches
            .get(&watch_name)
            .and_then(|w| w.last_seen.clone())
        else {
            return Err(format!(
                "watch `{watch_name}` has no known revision yet — kick it first"
            ));
        };
        let run_id = self.state.take_run_id();
        self.spawn_run(
            &watch_name,
            RunPlan {
                run_id,
                rev: rev.clone(),
                trigger: format!("deploy {host}"),
                hosts: vec![host.clone()],
            },
        );
        self.save_state();
        Ok(format!(
            "deploying {host} at {} (run #{run_id})",
            &rev[..rev.len().min(12)]
        ))
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
            let mut still_down = false;
            for (host, stamp) in &offline {
                let override_ = overrides.get(host).cloned().unwrap_or_default();
                match check_reachable(&stamp.target, &override_).await {
                    Ok(()) => back.push(host.clone()),
                    Err(_) => still_down = true,
                }
            }
            if !back.is_empty() {
                let ws = self.state.watch_mut(&watch);
                for host in &back {
                    if let Some(hs) = ws.hosts.get_mut(host) {
                        hs.offline = None;
                    }
                }
                self.save_state();
                tracing::info!(
                    "watch {watch}: {} back online — catching up",
                    back.join(", ")
                );
                self.request_poll(&watch, "catch-up").await;
            }
            if still_down {
                self.schedule_recheck(&watch);
            }
        }
    }

    /// Poll one watch now; start a run when there's something to do.
    async fn request_poll(&mut self, watch: &str, trigger: &str) {
        if self.running.is_some() {
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
            // waits for a new one (or a force-deploy).
            if hs.failed.as_ref().map(|s| s.rev.as_str()) == Some(rev.as_str()) {
                continue;
            }
            hosts.push(name.clone());
        }
        if hosts.is_empty() {
            if changed {
                tracing::info!(
                    "watch {watch}: {} needs no deploys",
                    &rev[..rev.len().min(12)]
                );
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
        let task = tokio::spawn(async move {
            let record =
                runner::execute(&state_dir, &watch_owned, &notify_cfg, plan, &log_tx).await;
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
            _task: task,
        });
    }

    async fn finish_run(&mut self, watch: String, record: crate::state::RunRecord) {
        let rev = record.rev.clone();
        let time = record.finished.unwrap_or_else(now_unix);
        {
            let ws = self.state.watch_mut(&watch);
            for hr in &record.hosts {
                let hs = ws.hosts.entry(hr.host.clone()).or_default();
                match hr.outcome.as_str() {
                    "ok" => {
                        hs.deployed = Some(Stamp {
                            rev: rev.clone(),
                            time,
                        });
                        hs.failed = None;
                        hs.offline = None;
                    }
                    "failed" => {
                        hs.failed = Some(FailStamp {
                            rev: rev.clone(),
                            time,
                            message: hr.message.clone().unwrap_or_default(),
                        });
                        hs.offline = None;
                    }
                    "offline" => {
                        hs.offline = Some(crate::state::OfflineStamp {
                            rev: rev.clone(),
                            time,
                            target: hr.target.clone().unwrap_or_default(),
                        });
                    }
                    _ => {}
                }
            }
        }
        let had_offline = record.hosts.iter().any(|h| h.outcome == "offline");
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
