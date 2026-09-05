//! deptui-agent — background auto-deploy daemon for deploy-rs flakes.
//!
//! Watches git repositories for updates (branch head or moving tag) and
//! pushes them to configured hosts via `deploy`, reusing deptui-core's
//! deploy runner. One binary is both the daemon (`run`) and its local
//! client (`status`, `kick`, …) — which is also the remote-control
//! story: `ssh host deptui-agent status --json`.
//!
//! See docs/agent-design.md for the full contract.

mod api;
mod client;
mod config;
mod daemon;
mod gitwatch;
mod notify;
mod runner;
mod state;
mod wire;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use config::AgentConfig;

#[derive(Debug, Parser)]
#[command(name = "deptui-agent", version, about)]
struct Cli {
    /// Config file. Defaults to $DEPTUI_AGENT_CONFIG, then
    /// /etc/deptui-agent/config.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Agent control socket (client verbs). Defaults to the config's
    /// `socket`, then /run/deptui-agent/agent.sock.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon (systemd ExecStart).
    Run {
        /// Override the config's state_dir.
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Poll watches once, deploy what's due, and exit — the cron/timer
    /// escape hatch. No daemon or socket involved.
    Check {
        /// Only this watch.
        #[arg(long)]
        watch: Option<String>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Verify every configured target is reachable non-interactively;
    /// exit non-zero when one is not.
    Validate {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Agent and per-host status.
    Status {
        /// Raw JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
    /// Recent deploy runs.
    History {
        #[arg(long)]
        watch: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print the captured log of a run (default: the newest).
    Log {
        watch: String,
        #[arg(long)]
        run: Option<u64>,
    },
    /// Ask the daemon to poll now.
    Kick {
        #[arg(long)]
        watch: Option<String>,
    },
    /// Pause automatic deploys (globally, or one --watch / --host).
    Pause {
        #[arg(long)]
        watch: Option<String>,
        #[arg(long)]
        host: Option<String>,
    },
    /// Resume automatic deploys.
    Resume {
        #[arg(long)]
        watch: Option<String>,
        #[arg(long)]
        host: Option<String>,
    },
    /// Force-deploy one host at the watch's last-seen revision,
    /// bypassing pause flags and the failed-at marker.
    Deploy {
        host: String,
        #[arg(long)]
        watch: Option<String>,
    },
    /// Stream the daemon's live run log (NDJSON-ish plain lines).
    Tail,
}

fn config_path(cli: &Cli) -> PathBuf {
    cli.config
        .clone()
        .or_else(|| std::env::var_os("DEPTUI_AGENT_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(config::DEFAULT_CONFIG_PATH))
}

/// Socket resolution for client verbs: --socket, else the config file
/// when it exists, else the well-known default.
fn socket_path(cli: &Cli) -> PathBuf {
    if let Some(s) = &cli.socket {
        return s.clone();
    }
    let cfg = config_path(cli);
    if cfg.exists() {
        if let Ok(c) = AgentConfig::load(&cfg) {
            return c.socket;
        }
    }
    PathBuf::from(config::DEFAULT_SOCKET_PATH)
}

fn load_config(cli: &Cli, state_dir: Option<PathBuf>) -> Result<AgentConfig> {
    let path = config_path(cli);
    let mut cfg = AgentConfig::load(&path)?;
    if let Some(dir) = state_dir {
        cfg.state_dir = dir;
    }
    if cfg.watches.is_empty() {
        bail!("{} configures no watches — nothing to do", path.display());
    }
    Ok(cfg)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    match cli.cmd {
        Command::Run { ref state_dir } => {
            let cfg = Arc::new(load_config(&cli, state_dir.clone())?);
            let daemon = daemon::Daemon::new(cfg.clone())?;
            let api_state = api::ApiState {
                cmd_tx: daemon.handle(),
                log_tx: daemon.log_tx.clone(),
                token: None,
            };
            let unix = {
                let cfg = cfg.clone();
                let st = api_state.clone();
                tokio::spawn(async move { api::serve_unix(&cfg, st).await })
            };
            if let Some(listen) = &cfg.listen {
                let token = std::fs::read_to_string(&listen.token_file)
                    .with_context(|| {
                        format!("reading listen.token_file {}", listen.token_file.display())
                    })?
                    .trim()
                    .to_string();
                if token.is_empty() {
                    bail!("listen.token_file {} is empty", listen.token_file.display());
                }
                let addr = listen.addr.clone();
                let st = api_state.clone();
                tokio::spawn(async move {
                    if let Err(e) = api::serve_tcp(&addr, token, st).await {
                        tracing::error!("TCP listener failed: {e:#}");
                    }
                });
            }
            let result = daemon.run().await;
            unix.abort();
            result
        }
        Command::Check {
            ref watch,
            ref state_dir,
        } => check_once(&cli, watch.clone(), state_dir.clone()).await,
        Command::Validate { ref state_dir } => validate(&cli, state_dir.clone()).await,
        Command::Status { json } => {
            let socket = socket_path(&cli);
            let status: wire::AgentStatus = client::get_json(&socket, "/status").await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_status(&status);
            }
            Ok(())
        }
        Command::History { ref watch, json } => {
            let socket = socket_path(&cli);
            let path = match watch {
                Some(w) => format!("/history?watch={}", urlencode(w)),
                None => "/history".to_string(),
            };
            let runs: Vec<wire::RunSummary> = client::get_json(&socket, &path).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&runs)?);
            } else {
                print_history(&runs);
            }
            Ok(())
        }
        Command::Log { ref watch, run } => {
            let socket = socket_path(&cli);
            let mut path = format!("/log?watch={}", urlencode(watch));
            if let Some(id) = run {
                path.push_str(&format!("&run={id}"));
            }
            let lines: Vec<String> = client::get_json(&socket, &path).await?;
            for l in lines {
                println!("{l}");
            }
            Ok(())
        }
        Command::Kick { ref watch } => {
            simple_post(&cli, "/kick", &[("watch", watch.as_deref())]).await
        }
        Command::Pause {
            ref watch,
            ref host,
        } => {
            simple_post(
                &cli,
                "/pause",
                &[("watch", watch.as_deref()), ("host", host.as_deref())],
            )
            .await
        }
        Command::Resume {
            ref watch,
            ref host,
        } => {
            simple_post(
                &cli,
                "/resume",
                &[("watch", watch.as_deref()), ("host", host.as_deref())],
            )
            .await
        }
        Command::Deploy {
            ref host,
            ref watch,
        } => {
            simple_post(
                &cli,
                "/deploy",
                &[("host", Some(host.as_str())), ("watch", watch.as_deref())],
            )
            .await
        }
        Command::Tail => {
            let socket = socket_path(&cli);
            client::tail(&socket, |line| println!("{line}")).await
        }
    }
}

async fn simple_post(cli: &Cli, path: &str, params: &[(&str, Option<&str>)]) -> Result<()> {
    let socket = socket_path(cli);
    let mut url = path.to_string();
    let mut sep = '?';
    for (k, v) in params {
        if let Some(v) = v {
            url.push(sep);
            url.push_str(&format!("{k}={}", urlencode(v)));
            sep = '&';
        }
    }
    let reply: wire::OkReply = client::post_json(&socket, &url).await?;
    println!("{}", reply.message);
    Ok(())
}

/// Query-string escaping for the few reserved bytes a watch/host name
/// could carry. Names are validated [A-Za-z0-9_-], so this is belt and
/// braces for host names typed at the CLI.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn fmt_time(t: u64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(t as i64, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => t.to_string(),
    }
}

fn short(rev: &str) -> &str {
    &rev[..rev.len().min(12)]
}

fn print_status(s: &wire::AgentStatus) {
    println!(
        "deptui-agent {}{}",
        s.version,
        if s.paused { " [PAUSED]" } else { "" }
    );
    for w in &s.watches {
        let pause = if w.paused { " [paused]" } else { "" };
        println!("\nwatch {} — {} ({}){}", w.name, w.repo, w.ref_label, pause);
        if let Some(rev) = &w.last_seen {
            println!("  last seen: {}", short(rev));
        }
        if let Some(r) = &w.running {
            println!(
                "  RUNNING: {} since {} ({})",
                short(&r.rev),
                fmt_time(r.started),
                r.trigger
            );
        } else if let Some(t) = w.next_poll {
            println!("  next poll: {}", fmt_time(t));
        }
        for h in &w.hosts {
            let mut bits = Vec::new();
            if h.paused {
                bits.push("paused".to_string());
            }
            if let (Some(rev), Some(t)) = (&h.deployed_rev, h.deployed_time) {
                bits.push(format!("deployed {} at {}", short(rev), fmt_time(t)));
            }
            if let (Some(rev), Some(t)) = (&h.failed_rev, h.failed_time) {
                bits.push(format!("FAILED {} at {}", short(rev), fmt_time(t)));
            }
            if let Some(u) = &h.unreachable {
                bits.push(format!("unreachable: {u}"));
            }
            if bits.is_empty() {
                bits.push("never deployed".to_string());
            }
            println!("  {}: {}", h.name, bits.join("; "));
        }
    }
}

fn print_history(runs: &[wire::RunSummary]) {
    if runs.is_empty() {
        println!("no runs recorded");
        return;
    }
    for r in runs {
        let outcome = if r.hosts.iter().any(|h| h.outcome == "failed") {
            "FAILED"
        } else {
            "ok"
        };
        let hosts: Vec<String> = r
            .hosts
            .iter()
            .map(|h| format!("{}:{}", h.host, h.outcome))
            .collect();
        println!(
            "#{} {} {} {} ({}) — {} [{}]",
            r.id,
            r.watch,
            short(&r.rev),
            fmt_time(r.started),
            r.trigger,
            outcome,
            hosts.join(", "),
        );
    }
}

/// `check`: poll once, deploy inline, save state, exit non-zero when a
/// host failed.
async fn check_once(cli: &Cli, only: Option<String>, state_dir: Option<PathBuf>) -> Result<()> {
    let cfg = load_config(cli, state_dir)?;
    if let Some(w) = &only {
        if !cfg.watches.iter().any(|x| &x.name == w) {
            bail!("unknown watch `{w}`");
        }
    }
    let mut state = state::AgentState::load(&cfg.state_dir)?;
    let (log_tx, _keep) = tokio::sync::broadcast::channel(64);
    let mut any_failed = false;
    for w in &cfg.watches {
        if let Some(o) = &only {
            if &w.name != o {
                continue;
            }
        }
        if state.paused
            || state
                .watches
                .get(&w.name)
                .map(|x| x.paused)
                .unwrap_or(false)
        {
            println!("{}: paused, skipping", w.name);
            continue;
        }
        let rev = match gitwatch::ls_remote(&w.repo, &w.refspec()).await {
            Ok(Some(rev)) => rev,
            Ok(None) => {
                eprintln!("{}: {} not found", w.name, w.refspec());
                continue;
            }
            Err(e) => {
                eprintln!("{}: poll failed: {e:#}", w.name);
                any_failed = true;
                continue;
            }
        };
        state.watch_mut(&w.name).last_seen = Some(rev.clone());
        let ws = state.watches.get(&w.name).unwrap();
        let hosts: Vec<String> = w
            .hosts
            .keys()
            .filter(|h| {
                let hs = ws.hosts.get(*h).cloned().unwrap_or_default();
                !hs.paused
                    && hs.deployed.as_ref().map(|s| s.rev.as_str()) != Some(rev.as_str())
                    && hs.failed.as_ref().map(|s| s.rev.as_str()) != Some(rev.as_str())
            })
            .cloned()
            .collect();
        if hosts.is_empty() {
            println!("{}: up to date at {}", w.name, short(&rev));
            state.save(&cfg.state_dir)?;
            continue;
        }
        let run_id = state.take_run_id();
        let plan = runner::RunPlan {
            run_id,
            rev: rev.clone(),
            trigger: "check".to_string(),
            hosts,
        };
        let record = runner::execute(&cfg.state_dir, w, &cfg.notify, plan, &log_tx).await;
        let time = record.finished.unwrap_or_else(state::now_unix);
        for hr in &record.hosts {
            let hs = state
                .watch_mut(&w.name)
                .hosts
                .entry(hr.host.clone())
                .or_default();
            match hr.outcome.as_str() {
                "ok" => {
                    hs.deployed = Some(state::Stamp {
                        rev: rev.clone(),
                        time,
                    });
                    hs.failed = None;
                }
                "failed" => {
                    any_failed = true;
                    hs.failed = Some(state::FailStamp {
                        rev: rev.clone(),
                        time,
                        message: hr.message.clone().unwrap_or_default(),
                    });
                }
                _ => {}
            }
        }
        for line in &record.log {
            println!("{line}");
        }
        state.push_run(&w.name, record);
        state.save(&cfg.state_dir)?;
    }
    if any_failed {
        std::process::exit(1);
    }
    Ok(())
}

/// `validate`: resolve each watch's current revision, discover its
/// nodes, and probe every configured host non-interactively.
async fn validate(cli: &Cli, state_dir: Option<PathBuf>) -> Result<()> {
    let cfg = load_config(cli, state_dir)?;
    let mut failures = 0u32;
    for w in &cfg.watches {
        let rev = match gitwatch::ls_remote(&w.repo, &w.refspec()).await {
            Ok(Some(rev)) => rev,
            Ok(None) => {
                eprintln!("{}: {} not found in {}", w.name, w.refspec(), w.repo);
                failures += 1;
                continue;
            }
            Err(e) => {
                eprintln!("{}: cannot poll {}: {e:#}", w.name, w.repo);
                failures += 1;
                continue;
            }
        };
        let dir = match gitwatch::ensure_checkout(&cfg.state_dir, &w.name, &w.repo, &rev).await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("{}: checkout failed: {e:#}", w.name);
                failures += 1;
                continue;
            }
        };
        let flake_ref = dir.to_string_lossy().to_string();
        let nodes = match deptui_core::flake::discover(&flake_ref).await {
            Ok(n) => n,
            Err(e) => {
                eprintln!("{}: discovery failed: {e:#}", w.name);
                failures += 1;
                continue;
            }
        };
        for (host, hc) in &w.hosts {
            let Some(node) = nodes.iter().find(|n| n.name == *host) else {
                eprintln!("{}: host `{host}` not in deploy.nodes", w.name);
                failures += 1;
                continue;
            };
            let override_ = hc.ssh_override();
            let target = deptui_core::host::build_ssh_target(node, "system", &override_);
            match daemon::check_reachable(&target, &override_).await {
                Ok(()) => println!("{}: {host} ({target}) ok", w.name),
                Err(e) => {
                    eprintln!("{}: {host} ({target}) UNREACHABLE: {e}", w.name);
                    failures += 1;
                }
            }
        }
    }
    if failures > 0 {
        bail!("{failures} validation failure(s)");
    }
    println!("all targets reachable");
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // The daemon logs to stderr; under systemd that's the journal.
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
