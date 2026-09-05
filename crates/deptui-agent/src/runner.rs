//! One deploy run: checkout the detected revision, discover its
//! `deploy.nodes`, push every eligible host sequentially. Shared by the
//! daemon loop (which spawns it) and `check --once` (which awaits it
//! inline), so both paths cannot drift.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use tokio::sync::{broadcast, watch};

use std::process::Stdio;
use std::time::Duration;

use deptui_core::askpass::AskpassEnv;
use deptui_core::deploy::{self, DeployRequest, LogLine, ProfileInfo};
use deptui_core::flake;
use deptui_core::host::build_ssh_target;

use crate::config::{NotifyConfig, WatchConfig};
use crate::notify::{self, Event};
use crate::state::{now_unix, HostRun, RunRecord, MAX_RUN_LOG};

/// Hosts to push in this run and why the others were left out is the
/// caller's business — the runner deploys exactly what it's given.
pub struct RunPlan {
    pub run_id: u64,
    pub rev: String,
    pub trigger: String,
    /// Node names, in order.
    pub hosts: Vec<String>,
}

/// How one host ended, when the deploy machinery itself didn't error.
pub enum DeployOutcome {
    Deployed,
    /// The host was down before we started and `catch_up` is on: the
    /// update stays pending and the daemon re-probes `target`.
    Offline {
        target: String,
        message: String,
    },
    /// The user cancelled the run while this host was deploying; the
    /// process group has been torn down.
    Cancelled,
}

/// BatchMode reachability probe, mirroring what a deploy will need.
/// Shared by the pre-deploy catch-up probe, the daemon's startup
/// validation, its offline rechecks, and the `validate` subcommand.
pub async fn check_reachable(
    target: &str,
    override_: &deptui_core::ssh::SshOverride,
) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"]);
    for arg in override_.ssh_args() {
        cmd.arg(arg);
    }
    cmd.arg(target)
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(15), cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => Ok(()),
        Ok(Ok(out)) => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        Ok(Err(e)) => Err(format!("spawning ssh: {e}")),
        Err(_) => Err("ssh probe timed out".to_string()),
    }
}

/// The agent never answers an ssh prompt: SSH_ASKPASS points at
/// `/bin/false`, so anything that would ask for a password or
/// passphrase fails fast instead of hanging a headless daemon.
fn askpass_disabled() -> AskpassEnv {
    AskpassEnv {
        script_path: "/bin/false".into(),
        socket_path: "/dev/null".into(),
    }
}

/// Execute a run. Log lines stream into `log_tx` (for SSE tails) as
/// they happen and are captured (capped) into the returned record.
pub async fn execute(
    state_dir: &Path,
    watch: &WatchConfig,
    notify_cfg: &NotifyConfig,
    plan: RunPlan,
    log_tx: &broadcast::Sender<String>,
    cancel_rx: watch::Receiver<bool>,
) -> RunRecord {
    let mut record = RunRecord {
        id: plan.run_id,
        rev: plan.rev.clone(),
        trigger: plan.trigger.clone(),
        started: now_unix(),
        finished: None,
        hosts: Vec::new(),
        log: Vec::new(),
    };
    let log = |record: &mut RunRecord, line: String| {
        let _ = log_tx.send(line.clone());
        tracing::info!(watch = %watch.name, "{line}");
        if record.log.len() < MAX_RUN_LOG {
            record.log.push(line);
        }
    };

    let short = &plan.rev[..plan.rev.len().min(12)];
    log(
        &mut record,
        format!(
            "[{}] run #{} ({}): deploying {} to {} host(s)",
            watch.name,
            plan.run_id,
            plan.trigger,
            short,
            plan.hosts.len()
        ),
    );

    // Checkout + discovery happen once per run, not per host.
    let setup = async {
        let dir = crate::gitwatch::ensure_checkout(state_dir, &watch.name, &watch.repo, &plan.rev)
            .await?;
        let flake_ref = dir
            .to_str()
            .ok_or_else(|| anyhow!("clone path is not valid UTF-8"))?
            .to_string();
        let nodes = flake::discover(&flake_ref)
            .await
            .with_context(|| format!("discovering deploy.nodes in {flake_ref}"))?;
        Ok::<_, anyhow::Error>((flake_ref, nodes))
    };
    let (flake_ref, nodes) = match setup.await {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("run setup failed: {e:#}");
            log(&mut record, format!("[{}] {msg}", watch.name));
            for host in &plan.hosts {
                record.hosts.push(HostRun {
                    host: host.clone(),
                    outcome: "failed".into(),
                    message: Some(msg.clone()),
                    target: None,
                });
                notify::dispatch(
                    notify_cfg,
                    Event::new("failure", &watch.name, Some(host), &plan.rev, msg.clone()),
                );
            }
            record.finished = Some(now_unix());
            return record;
        }
    };

    let mut cancelled = false;
    for host in &plan.hosts {
        // Cancelled between hosts (or before the first): everything
        // not yet started is parked at this revision too — "stop this
        // deploy" must not quietly resume at the next poll.
        if cancelled || *cancel_rx.borrow() {
            record.hosts.push(HostRun {
                host: host.clone(),
                outcome: "cancelled".into(),
                message: Some("run cancelled before this host started".into()),
                target: None,
            });
            continue;
        }
        let outcome = deploy_host(
            watch,
            &flake_ref,
            &nodes,
            host,
            &plan.rev,
            notify_cfg,
            cancel_rx.clone(),
            |line| log(&mut record, line),
        )
        .await;
        match outcome {
            Ok(DeployOutcome::Cancelled) => {
                cancelled = true;
                log(
                    &mut record,
                    format!("[{}] {host}: cancelled by user", watch.name),
                );
                // Deliberate user action — parked, but no failure
                // notification spam.
                record.hosts.push(HostRun {
                    host: host.clone(),
                    outcome: "cancelled".into(),
                    message: Some("cancelled by user".into()),
                    target: None,
                });
            }
            Ok(DeployOutcome::Offline { target, message }) => {
                log(
                    &mut record,
                    format!(
                        "[{}] {host}: offline ({target}) — update pending until it answers",
                        watch.name
                    ),
                );
                record.hosts.push(HostRun {
                    host: host.clone(),
                    outcome: "offline".into(),
                    message: Some(message),
                    target: Some(target),
                });
            }
            Ok(DeployOutcome::Deployed) => {
                log(
                    &mut record,
                    format!("[{}] {host}: deployed {short}", watch.name),
                );
                record.hosts.push(HostRun {
                    host: host.clone(),
                    outcome: "ok".into(),
                    message: None,
                    target: None,
                });
                notify::dispatch(
                    notify_cfg,
                    Event::new(
                        "success",
                        &watch.name,
                        Some(host),
                        &plan.rev,
                        format!("{host} deployed {short}"),
                    ),
                );
            }
            Err(e) => {
                let msg = format!("{e:#}");
                log(
                    &mut record,
                    format!("[{}] {host}: FAILED — {msg}", watch.name),
                );
                record.hosts.push(HostRun {
                    host: host.clone(),
                    outcome: "failed".into(),
                    message: Some(msg.clone()),
                    target: None,
                });
                notify::dispatch(
                    notify_cfg,
                    Event::new("failure", &watch.name, Some(host), &plan.rev, msg),
                );
            }
        }
    }

    record.finished = Some(now_unix());
    let ok = record.hosts.iter().filter(|h| h.outcome == "ok").count();
    let failed = record
        .hosts
        .iter()
        .filter(|h| h.outcome == "failed")
        .count();
    log(
        &mut record,
        format!(
            "[{}] run #{} finished: {ok} ok, {failed} failed",
            watch.name, plan.run_id,
        ),
    );
    record
}

#[allow(clippy::too_many_arguments)]
async fn deploy_host(
    watch: &WatchConfig,
    flake_ref: &str,
    nodes: &[flake::Node],
    host: &str,
    rev: &str,
    notify_cfg: &NotifyConfig,
    mut cancel_rx: watch::Receiver<bool>,
    mut log: impl FnMut(String),
) -> Result<DeployOutcome> {
    let hc = watch
        .hosts
        .get(host)
        .ok_or_else(|| anyhow!("host `{host}` is not configured in watch `{}`", watch.name))?;
    let node = nodes.iter().find(|n| n.name == host).ok_or_else(|| {
        anyhow!(
            "node `{host}` not found in deploy.nodes at {}",
            &rev[..rev.len().min(12)]
        )
    })?;

    // Offline catch-up: probe before deploying so a sleeping host is a
    // pending update, not a parked failure. With catch_up off the
    // deploy is attempted regardless and a dead host fails normally.
    if hc.catch_up() {
        let override_ = hc.ssh_override();
        let target = build_ssh_target(node, "system", &override_);
        if let Err(message) = check_reachable(&target, &override_).await {
            return Ok(DeployOutcome::Offline { target, message });
        }
    }

    notify::dispatch(
        notify_cfg,
        Event::new(
            "start",
            &watch.name,
            Some(host),
            rev,
            format!("deploying {host}"),
        ),
    );

    let req = DeployRequest {
        flake: flake_ref.to_string(),
        node: host.to_string(),
        profile: hc.profile_sel()?,
        mode: hc.deploy_mode()?,
        toggles: hc.toggles(),
        ssh_override: hc.ssh_override(),
        askpass: askpass_disabled(),
        profiles: node
            .ordered_profiles()
            .into_iter()
            .map(|name| {
                let user = node.profiles.get(&name).and_then(|p| p.user.clone());
                ProfileInfo { name, user }
            })
            .collect(),
        extra_build_args: hc.extra_build_args.clone(),
        seed: None,
        node_info: Some(node.clone()),
    };

    let mut handle = deploy::run(req, None);
    let mut exit: Option<i32> = None;
    let mut spawn_error: Option<String> = None;
    let mut user_cancelled = *cancel_rx.borrow();
    if user_cancelled {
        handle.cancel.cancel();
    }
    loop {
        tokio::select! {
            line = handle.rx.recv() => {
                let Some(line) = line else { break };
                match line {
                    LogLine::Stdout(s) | LogLine::Stderr(s) => log(format!("[{host}] {s}")),
                    LogLine::SudoPrompt(_) => {
                        // Headless: there is nobody to answer. Cancel the whole
                        // process group rather than letting the child hang.
                        log(format!(
                            "[{host}] remote sudo asked for a password — cancelling (agent deploys \
                             must be non-interactive)"
                        ));
                        handle.cancel.cancel();
                    }
                    LogLine::Exit(code) => exit = Some(code),
                    LogLine::Error(e) => spawn_error = Some(e),
                }
            }
            // User cancel: tear the process group down, then keep
            // draining — run_one still owns the child and finishes the
            // TERM → grace → KILL sequence before the channel closes.
            changed = cancel_rx.changed(), if !user_cancelled => {
                if changed.is_ok() && *cancel_rx.borrow() {
                    user_cancelled = true;
                    log(format!("[{host}] cancelling — signalling the deploy's process group"));
                    handle.cancel.cancel();
                }
            }
        }
    }
    if user_cancelled {
        return Ok(DeployOutcome::Cancelled);
    }
    if let Some(e) = spawn_error {
        return Err(anyhow!("deploy failed to run: {e}"));
    }
    match exit {
        Some(0) => Ok(DeployOutcome::Deployed),
        Some(code) => Err(anyhow!("deploy exited with code {code}")),
        None => Err(anyhow!("deploy ended without an exit status")),
    }
}
