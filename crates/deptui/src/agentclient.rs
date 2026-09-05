//! Remote agent client: `ssh <target> deptui-agent <verb> --json`.
//!
//! Plain ssh exec on purpose — no socket forwarding lifecycle, and any
//! scripting user gets exactly the interface the TUI uses. Password
//! prompts route through the app's SSH_ASKPASS server like every other
//! ssh the TUI spawns, so key-less setups still work interactively.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::askpass::AskpassEnv;
use deptui_core::agentwire;

const VERB_TIMEOUT: Duration = Duration::from_secs(30);

/// One ssh at a time. Every verb, probe, and backfill fetch takes this
/// gate: firing them in parallel meant one 1Password/ssh-agent
/// signature prompt per connection, all at once — an unanswerable
/// prompt storm. Serialized, the first connection authenticates once
/// and multiplexing (below) makes the rest free. The long-lived tail
/// is exempt (it is a single connection).
static SSH_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

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

async fn run_verb(target: &str, askpass: &AskpassEnv, verb_args: &[&str]) -> Result<Vec<u8>> {
    let _gate = SSH_GATE.acquire().await.expect("gate never closed");
    let out = tokio::time::timeout(
        VERB_TIMEOUT,
        ssh_command(target, askpass, verb_args).output(),
    )
    .await
    .map_err(|_| anyhow!("timed out talking to the agent on {target}"))?
    .with_context(|| format!("spawning ssh to {target}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("{}", stderr.trim()))
            .with_context(|| format!("agent call on {target} failed ({})", out.status));
    }
    Ok(out.stdout)
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

/// Spawn `deptui-agent tail` over ssh, invoking `on_line` per log line
/// until the returned task is aborted (kill_on_drop tears the ssh
/// child down with it) or the stream ends.
pub fn spawn_tail(
    target: String,
    askpass: AskpassEnv,
    on_line: impl Fn(String) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut cmd = ssh_command(&target, &askpass, &["tail"]);
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
/// verbs this runs with BatchMode — a host that would ask for a
/// password just isn't discoverable, rather than popping N password
/// prompts during a scan — and a short timeout, since it fans out
/// over every deploy node.
pub async fn probe(target: &str) -> Result<agentwire::AgentStatus> {
    // Serialized like the verbs: a parallel scan across N nodes asked
    // the user's ssh agent N times at once. BatchMode stops password
    // prompts but not agent-signature authorizations.
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
    let out = tokio::time::timeout(Duration::from_secs(8), cmd.output())
        .await
        .map_err(|_| anyhow!("probe timed out"))?
        .context("spawning ssh")?;
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
