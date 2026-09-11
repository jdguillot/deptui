//! Agent client: `deptui-agent <verb> --json`, over ssh or right here.
//!
//! Plain ssh exec on purpose — no socket forwarding lifecycle, and any
//! scripting user gets exactly the interface the TUI uses. Password
//! prompts route through the app's SSH_ASKPASS server like every other
//! ssh the TUI spawns, so key-less setups still work interactively.
//!
//! When the destination *is* this machine the ssh hop is skipped and
//! the CLI runs directly against the agent's Unix socket. A host
//! seldom has a key authorized to itself, so running the TUI on the
//! agent's own host otherwise reported `Permission denied (publickey)`
//! and no agent at all — see [`deptui_core::localhost`].

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::askpass::AskpassEnv;
use deptui_core::{agentwire, localhost};

const VERB_TIMEOUT: Duration = Duration::from_secs(30);

/// One ssh at a time. Every verb, probe, and backfill fetch takes this
/// gate: firing them in parallel meant one 1Password/ssh-agent
/// signature prompt per connection, all at once — an unanswerable
/// prompt storm. Serialized, the first connection authenticates once
/// and multiplexing (below) makes the rest free. The long-lived tail
/// is exempt (it is a single connection), and so is the local
/// transport — it authenticates nothing.
static SSH_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// The target string the local transport is addressed by when the
/// caller has no node of its own to name (the discovery scan's extra
/// candidate). Any local-looking destination works; this one needs no
/// resolver.
pub const LOCAL_TARGET: &str = "localhost";

/// How a verb reaches the agent. Decided per target by
/// [`localhost::is_local_target`] and never by what answers: a local
/// agent that is merely down must say "is deptui-agent running?"
/// rather than fall back to an ssh hop that cannot work either.
enum Transport {
    /// Run the CLI here; it talks to the agent's Unix socket.
    Local,
    /// `ssh <target> deptui-agent …`.
    Ssh,
}

async fn transport(target: &str) -> Transport {
    if localhost::is_local_target(target).await {
        Transport::Local
    } else {
        Transport::Ssh
    }
}

/// Is the agent at `target` reached without ssh? The view says so, so
/// that "no password prompt happened" is explained rather than odd.
pub async fn is_local(target: &str) -> bool {
    localhost::is_local_target(target).await
}

/// SSH connection reuse: the first connection to an agent becomes a
/// control master and later commands multiplex over it — no new
/// authentication, so the user's ssh agent is asked exactly once per
/// host per ControlPersist window.
fn multiplex_args() -> Vec<String> {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    vec![
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        format!("ControlPath={dir}/deptui-ssh-%C"),
        "-o".into(),
        "ControlPersist=60s".into(),
    ]
}

fn local_command(verb_args: &[&str]) -> Command {
    let mut cmd = Command::new("deptui-agent");
    cmd.args(verb_args);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

fn ssh_command(target: &str, askpass: &AskpassEnv, verb_args: &[&str]) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(multiplex_args());
    cmd.args(["-o", "ConnectTimeout=10"])
        .arg(target)
        .arg("deptui-agent");
    cmd.args(verb_args);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    askpass.apply(&mut cmd);
    // New session so ssh honours SSH_ASKPASS instead of grabbing the
    // TUI's terminal — same reasoning as the probes.
    AskpassEnv::pre_exec_setsid(&mut cmd);
    cmd
}

/// Spawning failed before the agent was ever reached. "No such file"
/// locally means the CLI is missing, which is worth saying outright:
/// the TUI and the agent ship as one workspace version, so a machine
/// with one usually has the other.
fn local_spawn_error(e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        return anyhow!("deptui-agent is not on PATH (the agent runs on this machine)");
    }
    anyhow!(e)
}

fn check_status(out: std::process::Output, label: &str) -> Result<Vec<u8>> {
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("{}", stderr.trim()))
            .with_context(|| format!("agent call on {label} failed ({})", out.status));
    }
    Ok(out.stdout)
}

async fn run_verb(target: &str, askpass: &AskpassEnv, verb_args: &[&str]) -> Result<Vec<u8>> {
    match transport(target).await {
        Transport::Local => {
            let out = tokio::time::timeout(VERB_TIMEOUT, local_command(verb_args).output())
                .await
                .map_err(|_| anyhow!("timed out talking to the agent on this machine"))?
                .map_err(local_spawn_error)
                .context("running deptui-agent here")?;
            check_status(out, "this machine")
        }
        Transport::Ssh => {
            let _gate = SSH_GATE.acquire().await.expect("gate never closed");
            let out = tokio::time::timeout(
                VERB_TIMEOUT,
                ssh_command(target, askpass, verb_args).output(),
            )
            .await
            .map_err(|_| anyhow!("timed out talking to the agent on {target}"))?
            .with_context(|| format!("spawning ssh to {target}"))?;
            check_status(out, target)
        }
    }
}

pub async fn fetch_status(target: &str, askpass: &AskpassEnv) -> Result<agentwire::AgentStatus> {
    let bytes = run_verb(target, askpass, &["status", "--json"]).await?;
    serde_json::from_slice(&bytes).context("parsing agent status JSON")
}

/// Run a mutating verb (`kick`, `pause`, `resume`, `deploy …`) and
/// return the agent's human ack line.
pub async fn op(target: &str, askpass: &AskpassEnv, verb_args: &[&str]) -> Result<String> {
    let bytes = run_verb(target, askpass, verb_args).await?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

/// Spawn `deptui-agent tail` (locally or over ssh), invoking `on_line`
/// per log line until the returned task is aborted (kill_on_drop tears
/// the child down with it) or the stream ends.
pub fn spawn_tail(
    target: String,
    askpass: AskpassEnv,
    on_line: impl Fn(String) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut cmd = match transport(&target).await {
            Transport::Local => local_command(&["tail"]),
            Transport::Ssh => ssh_command(&target, &askpass, &["tail"]),
        };
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                on_line(format!("! tail failed to start: {e}"));
                return;
            }
        };
        let Some(stdout) = child.stdout.take() else {
            return;
        };
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            on_line(line);
        }
        // EOF: the agent went away or the connection dropped. The task
        // ends; the screen keeps whatever tail it has.
        let _ = child.wait().await;
    })
}

/// Discovery probe: is `target` running an agent? Unlike the normal
/// verbs the ssh form runs with BatchMode — a host that would ask for
/// a password just isn't discoverable, rather than popping N password
/// prompts during a scan — and a short timeout, since it fans out over
/// every deploy node. The local form needs neither.
pub async fn probe(target: &str) -> Result<agentwire::AgentStatus> {
    let out = match transport(target).await {
        Transport::Local => tokio::time::timeout(
            Duration::from_secs(8),
            local_command(&["status", "--json"]).output(),
        )
        .await
        .map_err(|_| anyhow!("probe timed out"))?
        .map_err(local_spawn_error)
        .context("running deptui-agent here")?,
        Transport::Ssh => {
            // Serialized like the verbs: a parallel scan across N nodes
            // asked the user's ssh agent N times at once. BatchMode
            // stops password prompts but not agent-signature
            // authorizations.
            let _gate = SSH_GATE.acquire().await.expect("gate never closed");
            let mut cmd = Command::new("ssh");
            cmd.args(multiplex_args());
            cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=4"])
                .arg(target)
                .arg("deptui-agent")
                .args(["status", "--json"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            tokio::time::timeout(Duration::from_secs(8), cmd.output())
                .await
                .map_err(|_| anyhow!("probe timed out"))?
                .context("spawning ssh")?
        }
    };
    if !out.status.success() {
        return Err(anyhow!("{}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    serde_json::from_slice(&out.stdout).context("parsing agent status JSON")
}

/// Recent runs from the agent's stored history (newest first, as the
/// agent returns them). Used by the agent log's backfill.
pub async fn fetch_history(
    target: &str,
    askpass: &AskpassEnv,
) -> Result<Vec<agentwire::RunSummary>> {
    let bytes = run_verb(target, askpass, &["history", "--json"]).await?;
    serde_json::from_slice(&bytes).context("parsing agent history JSON")
}

/// The captured log of one stored run, one line per element.
pub async fn fetch_run_log(
    target: &str,
    askpass: &AskpassEnv,
    watch: &str,
    run: u64,
) -> Result<Vec<String>> {
    let run_s = run.to_string();
    let bytes = run_verb(target, askpass, &["log", watch, "--run", &run_s]).await?;
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::to_string)
        .collect())
}
