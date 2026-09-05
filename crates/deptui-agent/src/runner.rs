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
    pub hosts: Vec<PlanHost>,
}

#[derive(Debug, Clone)]
pub struct PlanHost {
    pub name: String,
    /// First encounter: this agent has never deployed the host, so it
    /// must adopt (probe; deploy only if the target already matches or
    /// the host opted into bootstrap deploys), never blind-deploy —
    /// a repo that is *behind* the host would otherwise be rolled
    /// forward onto it as a rollback.
    pub adopt: bool,
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
    /// First encounter and the target already runs the watched
    /// revision's closure: recorded as deployed, nothing pushed.
    Adopted,
    /// First encounter and the target runs something else: refused to
    /// deploy over it. The message says what differed.
    Held {
        message: String,
    },
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

/// Human summary of a run's per-host outcomes — every category that
/// occurred, not just ok/failed ("0 ok, 0 failed" for a run that held
/// one host buried the actual result).
pub fn summarize_outcomes(hosts: &[HostRun]) -> String {
    let count = |what: &str| hosts.iter().filter(|h| h.outcome == what).count();
    let mut parts = Vec::new();
    for what in [
        "ok",
        "adopted",
        "held",
        "offline",
        "failed",
        "cancelled",
        "skipped",
    ] {
        let n = count(what);
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    }
    if parts.is_empty() {
        "nothing to do".to_string()
    } else {
        parts.join(", ")
    }
}

/// Does this deploy target the machine the agent itself runs on? A
/// self-deploy whose update changes deptui-agent.service can stop the
/// agent mid-run (see the module's restartOnUpdate option); the run
/// log flags it so the journal explains itself when that happens.
fn is_self_target(local_hostname: &str, node_hostname: &str) -> bool {
    if local_hostname.is_empty() {
        return false;
    }
    node_hostname == local_hostname
        || node_hostname == "localhost"
        || node_hostname.strip_suffix(".localdomain") == Some(local_hostname)
        || node_hostname
            .split('.')
            .next()
            .is_some_and(|short| short == local_hostname)
}

fn local_hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buffer and length are valid; gethostname NUL-terminates
    // on success (we also cap at the buffer's end defensively).
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
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
                    host: host.name.clone(),
                    outcome: "failed".into(),
                    message: Some(msg.clone()),
                    target: None,
                });
                notify::dispatch(
                    notify_cfg,
                    Event::new(
                        "failure",
                        &watch.name,
                        Some(&host.name),
                        &plan.rev,
                        msg.clone(),
                    ),
                );
            }
            record.finished = Some(now_unix());
            return record;
        }
    };

    let mut cancelled = false;
    for ph in &plan.hosts {
        let host = &ph.name;
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
            ph.adopt,
            &plan.rev,
            notify_cfg,
            cancel_rx.clone(),
            |line| log(&mut record, line),
        )
        .await;
        match outcome {
            Ok(DeployOutcome::Adopted) => {
                log(
                    &mut record,
                    format!(
                        "[{}] {host}: already running {short} — adopted without deploying",
                        watch.name
                    ),
                );
                record.hosts.push(HostRun {
                    host: host.clone(),
                    outcome: "adopted".into(),
                    message: None,
                    target: None,
                });
            }
            Ok(DeployOutcome::Held { message }) => {
                log(
                    &mut record,
                    format!(
                        "[{}] {host}: HELD — {message} (deptui-agent deploy {host} adopts it)",
                        watch.name
                    ),
                );
                record.hosts.push(HostRun {
                    host: host.clone(),
                    outcome: "held".into(),
                    message: Some(message.clone()),
                    target: None,
                });
                notify::dispatch(
                    notify_cfg,
                    Event::new("held", &watch.name, Some(host), &plan.rev, message),
                );
            }
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
    let summary = summarize_outcomes(&record.hosts);
    log(
        &mut record,
        format!("[{}] run #{} finished: {summary}", watch.name, plan.run_id),
    );
    record
}

#[allow(clippy::too_many_arguments)]
async fn deploy_host(
    watch: &WatchConfig,
    flake_ref: &str,
    nodes: &[flake::Node],
    host: &str,
    adopt: bool,
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

    if is_self_target(&local_hostname(), &node.hostname) {
        log(format!(
            "[{host}] note: this deploy targets the agent's own host — if it changes \
             deptui-agent.service and the module's restartOnUpdate is enabled, the \
             activation will stop this agent mid-deploy"
        ));
    }

    // First encounter (unless the host opted into bootstrap deploys):
    // probe instead of deploying. A fresh agent state says nothing
    // about the *host* — it may already run this revision (adopt), or
    // something newer the repo hasn't caught up with (hold; a blind
    // deploy here is a rollback).
    if adopt && !hc.bootstrap_deploys() {
        let override_ = hc.ssh_override();
        let askpass = askpass_disabled();
        let profiles: Vec<String> = node
            .ordered_profiles()
            .into_iter()
            .filter(|p| {
                match hc
                    .profile_sel()
                    .unwrap_or(deptui_core::deploy::ProfileSel::All)
                {
                    deptui_core::deploy::ProfileSel::All => true,
                    deptui_core::deploy::ProfileSel::System => p == "system",
                    deptui_core::deploy::ProfileSel::Home => p == "home",
                }
            })
            .collect();
        for profile in &profiles {
            match deptui_core::host::check_profile_up_to_date(
                flake_ref, node, profile, &override_, &askpass,
            )
            .await
            {
                Ok(check) if check.up_to_date => continue,
                Ok(check) => {
                    let what = if check.not_deployed {
                        format!("profile `{profile}` has never been deployed there")
                    } else {
                        format!("profile `{profile}` differs from the watched revision")
                    };
                    return Ok(DeployOutcome::Held { message: what });
                }
                Err(e) => {
                    return Ok(DeployOutcome::Held {
                        message: format!("first-encounter probe failed: {e:#}"),
                    });
                }
            }
        }
        return Ok(DeployOutcome::Adopted);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_summary_names_every_category() {
        let hr = |outcome: &str| HostRun {
            host: "h".into(),
            outcome: outcome.into(),
            message: None,
            target: None,
        };
        assert_eq!(summarize_outcomes(&[]), "nothing to do");
        assert_eq!(summarize_outcomes(&[hr("held")]), "1 held");
        assert_eq!(
            summarize_outcomes(&[hr("ok"), hr("ok"), hr("offline"), hr("failed")]),
            "2 ok, 1 offline, 1 failed"
        );
    }

    #[test]
    fn self_target_detection() {
        assert!(is_self_target("ryzn-server", "ryzn-server"));
        assert!(is_self_target("ryzn-server", "ryzn-server.lan"));
        assert!(is_self_target("ryzn-server", "localhost"));
        assert!(!is_self_target("ryzn-server", "web.lan"));
        assert!(!is_self_target("", "web"));
    }

    #[test]
    fn local_hostname_is_nonempty_here() {
        assert!(!local_hostname().is_empty());
    }
}
