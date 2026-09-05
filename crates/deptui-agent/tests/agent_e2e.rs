//! End-to-end tests of the deptui-agent binary: a real local git repo,
//! PATH-shim `nix` and `deploy` binaries, and the actual executable via
//! `CARGO_BIN_EXE_deptui-agent`. Covers the oneshot `check` pipeline
//! (deploy on update, park on failure, no same-commit retry) and the
//! daemon's socket API driven through the client verbs.
//!
//! Shims go to the child's PATH only (no process-global mutation), so
//! no `#[serial]` is needed here.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const NODES_JSON: &str =
    r#"{"web":{"hostname":"web.lan","sshUser":"root","profiles":{"system":{"user":null}}}}"#;

struct Env {
    _shims: TempDir,
    shim_path: String,
    repo: TempDir,
    state: TempDir,
    config_path: PathBuf,
    /// Every `deploy` invocation appends its argv here.
    deploy_log: PathBuf,
    /// While this file exists, the `ssh` shim reports the host down.
    down_marker: PathBuf,
}

fn setup(deploy_exit: i32) -> Env {
    // The general tests predate first-encounter adoption and exercise
    // the deploy pipeline itself; opting into bootstrap deploys keeps
    // them meaningful. Adoption has its own tests below.
    setup_with(deploy_exit, "bootstrap = \"deploy\"\n")
}

fn setup_with(deploy_exit: i32, extra_host_cfg: &str) -> Env {
    let shims = TempDir::new().unwrap();
    let deploy_log = shims.path().join("deploy-calls.log");

    // `nix` shim: flake discovery JSON on stdout.
    let nix = shims.path().join("nix");
    fs::write(&nix, format!("#!/bin/sh\nprintf '%s' '{NODES_JSON}'\n")).unwrap();
    fs::set_permissions(&nix, fs::Permissions::from_mode(0o755)).unwrap();

    // `deploy` shim: record argv, emit a line, controlled exit.
    let deploy = shims.path().join("deploy");
    fs::write(
        &deploy,
        format!(
            "#!/bin/sh\necho \"$@\" >> {}\necho deploying\nexit {deploy_exit}\n",
            deploy_log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&deploy, fs::Permissions::from_mode(0o755)).unwrap();

    // `ssh` shim: up unless the down-marker exists. The catch-up probe
    // (and the daemon's rechecks) go through this.
    let down_marker = shims.path().join("host-down");
    let ssh = shims.path().join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nif [ -e {} ]; then echo 'Connection refused' >&2; exit 255; fi\nexit 0\n",
            down_marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();

    let shim_path = format!(
        "{}:{}",
        shims.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // A real git repo to watch.
    let repo = TempDir::new().unwrap();
    git(repo.path(), &["init", "-q", "-b", "main", "."]);
    fs::write(repo.path().join("flake.nix"), "{}").unwrap();
    git(repo.path(), &["add", "flake.nix"]);
    git(repo.path(), &["commit", "-qm", "one"]);

    let state = TempDir::new().unwrap();
    let config_path = state.path().join("config.toml");
    fs::write(
        &config_path,
        format!(
            r#"
state_dir = "{state}"
socket = "{state}/agent.sock"

[[watch]]
name = "infra"
repo = "{repo}"
branch = "main"
interval = "1h"
offline_recheck = "1s"

[watch.hosts.web]
{extra_host_cfg}
"#,
            state = state.path().display(),
            repo = repo.path().display(),
        ),
    )
    .unwrap();

    Env {
        _shims: shims,
        shim_path,
        repo,
        state,
        config_path,
        deploy_log,
        down_marker,
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn agent(env: &Env, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_deptui-agent"))
        .arg("--config")
        .arg(&env.config_path)
        .args(args)
        .env("PATH", &env.shim_path)
        .output()
        .unwrap()
}

fn deploy_calls(env: &Env) -> Vec<String> {
    fs::read_to_string(&env.deploy_log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn check_deploys_update_then_is_idempotent() {
    let env = setup(0);

    let out = agent(&env, &["check"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = deploy_calls(&env);
    assert_eq!(calls.len(), 1, "one deploy for the initial revision");
    assert!(
        calls[0].contains("#web"),
        "deploy target names the node: {}",
        calls[0]
    );
    // The flake ref is the agent's private clone, not the watched repo.
    let clone = env.state.path().join("clones/infra");
    assert!(calls[0].starts_with(&format!("{}#web", clone.display())));
    assert!(env.repo.path().join(".git").exists());

    // Same revision again: nothing to do.
    let out = agent(&env, &["check"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("up to date"));
    assert_eq!(deploy_calls(&env).len(), 1, "no second deploy");

    // A new commit deploys again.
    fs::write(env.repo.path().join("flake.nix"), "{ two = 1; }").unwrap();
    git(env.repo.path(), &["add", "flake.nix"]);
    git(env.repo.path(), &["commit", "-qm", "two"]);
    let out = agent(&env, &["check"]);
    assert!(out.status.success());
    assert_eq!(deploy_calls(&env).len(), 2);
}

#[test]
fn failed_deploy_parks_host_until_new_revision() {
    let env = setup(1);

    let out = agent(&env, &["check"]);
    assert!(!out.status.success(), "check exits non-zero on failure");
    assert_eq!(deploy_calls(&env).len(), 1);

    // Same revision: parked, not retried.
    let out = agent(&env, &["check"]);
    assert!(out.status.success(), "parked host is not an error");
    assert_eq!(deploy_calls(&env).len(), 1, "no same-commit retry");

    // New revision: retried (and fails again — still one new call).
    fs::write(env.repo.path().join("flake.nix"), "{ two = 1; }").unwrap();
    git(env.repo.path(), &["add", "flake.nix"]);
    git(env.repo.path(), &["commit", "-qm", "two"]);
    let out = agent(&env, &["check"]);
    assert!(!out.status.success());
    assert_eq!(deploy_calls(&env).len(), 2);

    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(env.state.path().join("state.json")).unwrap())
            .unwrap();
    let failed = &state["watches"]["infra"]["hosts"]["web"]["failed"];
    assert!(failed["rev"].is_string(), "failed marker recorded: {state}");
}

#[test]
fn daemon_serves_status_pause_kick_over_socket() {
    let env = setup(0);
    let socket = env.state.path().join("agent.sock");

    let mut daemon = Command::new(env!("CARGO_BIN_EXE_deptui-agent"))
        .arg("--config")
        .arg(&env.config_path)
        .arg("run")
        .env("PATH", &env.shim_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let wait_for = |what: &str, mut cond: Box<dyn FnMut() -> bool>| {
        let start = Instant::now();
        while !cond() {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    wait_for("socket", Box::new(|| socket.exists()));

    // Status over the socket (client verb → HTTP → daemon → back).
    let out = agent(&env, &["status", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["watches"][0]["name"], "infra");

    // Pause globally; status reflects it.
    let out = agent(&env, &["pause"]);
    assert!(out.status.success());
    let out = agent(&env, &["status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["paused"], true);
    let out = agent(&env, &["resume"]);
    assert!(out.status.success());

    // The startup poll (or our kick) deploys the initial revision.
    let out = agent(&env, &["kick"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    {
        let env = &env;
        wait_for(
            "deploy to run",
            Box::new(move || {
                let out = agent(env, &["status", "--json"]);
                let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
                status["watches"][0]["hosts"][0]["deployed_rev"].is_string()
            }),
        );
    }
    assert!(!deploy_calls(&env).is_empty());

    // History knows about the run.
    let out = agent(&env, &["history", "--json"]);
    let runs: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        runs.as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "{runs}"
    );

    // Unknown watch is a clean client-side error, not a daemon crash.
    let out = agent(&env, &["kick", "--watch", "nope"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown watch"));

    // SIGTERM: clean shutdown.
    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let start = Instant::now();
    loop {
        if let Some(st) = daemon.try_wait().unwrap() {
            assert!(st.success(), "daemon exited {st:?}");
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "daemon ignored SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn offline_host_is_pending_not_failed_and_catches_up() {
    let env = setup(0);
    fs::write(&env.down_marker, "").unwrap();

    // Down at deploy time: not an error, no deploy attempt, no parking.
    let out = agent(&env, &["check"]);
    assert!(
        out.status.success(),
        "offline is pending, not failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("offline"));
    assert_eq!(deploy_calls(&env).len(), 0, "no deploy while down");
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(env.state.path().join("state.json")).unwrap())
            .unwrap();
    let host = &state["watches"]["infra"]["hosts"]["web"];
    assert!(host["failed"].is_null(), "not parked: {state}");
    assert_eq!(host["offline"]["target"], "root@web.lan");
    assert!(host["offline"]["rev"].is_string());

    // Back online: the pending update lands and the marker clears.
    fs::remove_file(&env.down_marker).unwrap();
    let out = agent(&env, &["check"]);
    assert!(out.status.success());
    assert_eq!(deploy_calls(&env).len(), 1);
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(env.state.path().join("state.json")).unwrap())
            .unwrap();
    let host = &state["watches"]["infra"]["hosts"]["web"];
    assert!(host["offline"].is_null(), "marker cleared: {state}");
    assert!(host["deployed"]["rev"].is_string());
}

#[test]
fn catch_up_off_attempts_deploy_while_down() {
    let env = setup_with(0, "bootstrap = \"deploy\"\ncatch_up = false\n");
    fs::write(&env.down_marker, "").unwrap();

    // No pre-probe: the deploy is attempted regardless (our shim
    // "succeeds" — the point is that the attempt happened).
    let out = agent(&env, &["check"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(deploy_calls(&env).len(), 1, "deploy attempted while down");
}

#[test]
fn daemon_recheck_deploys_when_host_returns() {
    let env = setup(0);
    fs::write(&env.down_marker, "").unwrap();
    let socket = env.state.path().join("agent.sock");

    let mut daemon = Command::new(env!("CARGO_BIN_EXE_deptui-agent"))
        .arg("--config")
        .arg(&env.config_path)
        .arg("run")
        .env("PATH", &env.shim_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let wait_for = |what: &str, mut cond: Box<dyn FnMut() -> bool + '_>| {
        let start = Instant::now();
        while !cond() {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    wait_for("socket", Box::new(|| socket.exists()));

    let host_field = |field: &str| -> serde_json::Value {
        let out = agent(&env, &["status", "--json"]);
        let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        status["watches"][0]["hosts"][0][field].clone()
    };

    let out = agent(&env, &["kick"]);
    assert!(out.status.success());
    wait_for(
        "offline marker",
        Box::new(|| host_field("offline_rev").is_string()),
    );
    assert_eq!(deploy_calls(&env).len(), 0);

    // Host comes back: the 1s recheck notices and the update lands
    // without any human involvement.
    fs::remove_file(&env.down_marker).unwrap();
    wait_for(
        "catch-up deploy",
        Box::new(|| host_field("deployed_rev").is_string()),
    );
    assert!(!deploy_calls(&env).is_empty());
    assert!(host_field("offline_rev").is_null());

    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let _ = daemon.wait();
}

/// Cancel stops a run in flight: the deploy's process group dies, the
/// host parks at that revision (message says "cancelled", no failure
/// spam), and the next poll does NOT resume the run.
#[test]
fn cancel_stops_a_running_deploy_and_parks_the_host() {
    let env = setup(0);
    // Slow deploy: writes a start marker, then hangs far longer than
    // the test — only a real process-group kill ends it.
    let deploy = env._shims.path().join("deploy");
    let started = env._shims.path().join("deploy-started");
    fs::write(
        &deploy,
        format!(
            "#!/bin/sh\necho \"$@\" >> {}\ntouch {}\nsleep 300\n",
            env.deploy_log.display(),
            started.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&deploy, fs::Permissions::from_mode(0o755)).unwrap();

    let socket = env.state.path().join("agent.sock");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_deptui-agent"))
        .arg("--config")
        .arg(&env.config_path)
        .arg("run")
        .env("PATH", &env.shim_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let wait_for = |what: &str, mut cond: Box<dyn FnMut() -> bool + '_>| {
        let start = Instant::now();
        while !cond() {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    wait_for("socket", Box::new(|| socket.exists()));

    // No run yet: cancel is a clean error, not a crash.
    let out = agent(&env, &["cancel"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no run in progress"));

    let out = agent(&env, &["kick"]);
    assert!(out.status.success());
    wait_for("deploy to start", Box::new(|| started.exists()));

    let out = agent(&env, &["cancel"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("cancelling run"));

    // The run ends promptly (TERM → 3s grace → KILL is the ceiling,
    // not the 300s sleep), and the host is parked with a message that
    // says cancelled, not a fake failure.
    {
        let env = &env;
        wait_for(
            "run to finish as cancelled",
            Box::new(move || {
                let out = agent(env, &["status", "--json"]);
                let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
                let host = &status["watches"][0]["hosts"][0];
                host["failed_message"]
                    .as_str()
                    .is_some_and(|m| m.contains("cancelled"))
            }),
        );
    }
    let out = agent(&env, &["status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        status["watches"][0]["hosts"][0]["deployed_rev"].is_null(),
        "cancelled deploy must not count as deployed: {status}"
    );

    // Parked: another kick at the same revision starts nothing.
    let calls_before = deploy_calls(&env).len();
    let out = agent(&env, &["kick"]);
    assert!(out.status.success());
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        deploy_calls(&env).len(),
        calls_before,
        "same-revision kick after cancel must not redeploy"
    );

    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let _ = daemon.wait();
}

/// An agent with no watches yet must still start and serve its API —
/// the install-first, configure-later flow (and TUI discovery) depend
/// on it. The oneshot verbs still refuse.
#[test]
fn daemon_runs_and_answers_with_zero_watches() {
    let state = TempDir::new().unwrap();
    let config_path = state.path().join("config.toml");
    fs::write(
        &config_path,
        format!(
            "state_dir = \"{0}\"\nsocket = \"{0}/agent.sock\"\n",
            state.path().display()
        ),
    )
    .unwrap();
    let socket = state.path().join("agent.sock");

    let agent = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_deptui-agent"))
            .arg("--config")
            .arg(&config_path)
            .args(args)
            .output()
            .unwrap()
    };

    // check refuses: nothing to do is an error for a oneshot.
    let out = agent(&["check"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no watches"));

    // The daemon serves.
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_deptui-agent"))
        .arg("--config")
        .arg(&config_path)
        .arg("run")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !socket.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "daemon with zero watches must still bind its socket"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = agent(&["status", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["watches"].as_array().map(Vec::len), Some(0));

    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let _ = daemon.wait();
}

/// First-encounter adoption (the unintended-rollback fix): a fresh
/// agent must never blind-deploy a host it has never touched. The
/// probe (shimmed to fail here → "differs") holds instead, `check`
/// still exits 0, and the same revision is not re-probed.
#[test]
fn first_encounter_holds_instead_of_deploying() {
    let env = setup_with(0, "");

    let out = agent(&env, &["check"]);
    assert!(
        out.status.success(),
        "held is not a failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("HELD"));
    assert_eq!(deploy_calls(&env).len(), 0, "no deploy on first encounter");
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(env.state.path().join("state.json")).unwrap())
            .unwrap();
    let host = &state["watches"]["infra"]["hosts"]["web"];
    assert!(host["held"]["rev"].is_string(), "{state}");
    assert!(host["deployed"].is_null());

    // Same revision again: parked, no re-probe spam.
    let out = agent(&env, &["check"]);
    assert!(out.status.success());
    assert_eq!(deploy_calls(&env).len(), 0);
}

/// The daemon does not poll at startup — only the schedule, a kick,
/// or offline catch-up trigger runs. A kick on a fresh agent holds;
/// approval makes the *next* round deploy (never immediately).
#[test]
fn daemon_waits_for_cadence_and_approval_takes_next_round() {
    let env = setup_with(0, "");
    let socket = env.state.path().join("agent.sock");

    let mut daemon = Command::new(env!("CARGO_BIN_EXE_deptui-agent"))
        .arg("--config")
        .arg(&env.config_path)
        .arg("run")
        .env("PATH", &env.shim_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let wait_for = |what: &str, mut cond: Box<dyn FnMut() -> bool + '_>| {
        let start = Instant::now();
        while !cond() {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    wait_for("socket", Box::new(|| socket.exists()));

    let host_field = |field: &str| -> serde_json::Value {
        let out = agent(&env, &["status", "--json"]);
        let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        status["watches"][0]["hosts"][0][field].clone()
    };

    // No startup poll: several seconds in, nothing has happened (the
    // watch interval is 1h).
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(deploy_calls(&env).len(), 0, "startup must not poll");
    assert!(host_field("held_rev").is_null(), "startup must not probe");

    // kick → first encounter → held, still no deploy.
    let out = agent(&env, &["kick"]);
    assert!(out.status.success());
    wait_for(
        "hold marker",
        Box::new(|| host_field("held_rev").is_string()),
    );
    assert_eq!(deploy_calls(&env).len(), 0);

    // Approval alone deploys nothing — the ok is for the next round.
    let out = agent(&env, &["approve", "web", "--watch", "infra"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("next update round"));
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        deploy_calls(&env).len(),
        0,
        "approve must not deploy immediately"
    );

    // Revoke + re-approve round-trips.
    let out = agent(&env, &["approve", "web", "--watch", "infra", "--revoke"]);
    assert!(out.status.success());
    let out = agent(&env, &["approve", "web", "--watch", "infra"]);
    assert!(out.status.success());

    // The next round (a kick here; a scheduled poll in real life)
    // consumes the approval and deploys — the same revision the hold
    // parked, which approval unlocks.
    let out = agent(&env, &["kick"]);
    assert!(out.status.success());
    wait_for(
        "approved deploy",
        Box::new(|| host_field("deployed_rev").is_string()),
    );
    assert_eq!(deploy_calls(&env).len(), 1);
    assert!(host_field("held_rev").is_null(), "hold cleared by adoption");
    assert_eq!(
        host_field("approved"),
        serde_json::Value::Bool(false),
        "approval consumed"
    );

    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let _ = daemon.wait();
}
