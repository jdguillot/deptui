//! The agent client's transport choice, with PATH shims standing in for
//! `deptui-agent` and `ssh`.
//!
//! The regression this guards is issue #2: running the TUI on the
//! agent's own host made the client `ssh <this machine>`, which almost
//! never has a key authorized to itself, so the agent looked absent.
//! The `ssh` shim here fails the way a real one does — if a local
//! target ever reaches it again, these tests say so.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use deptui::agentclient;
use deptui::askpass::AskpassEnv;
use serial_test::serial;
use tempfile::TempDir;

const STATUS_JSON: &str = r#"{"version":"9.9.9","paused":false,"pubkey":null,"watches":[]}"#;

/// Install both shims on PATH: a `deptui-agent` that answers, and an
/// `ssh` that refuses like a host with no key to itself.
fn install_shims() -> TempDir {
    let dir = TempDir::new().expect("create temp dir");
    let agent = dir.path().join("deptui-agent");
    fs::write(
        &agent,
        format!("#!/bin/sh\nprintf '%s' '{STATUS_JSON}'\nexit 0\n"),
    )
    .expect("write agent shim");
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).expect("chmod agent shim");

    let ssh = dir.path().join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\necho 'Permission denied (publickey).' >&2\nexit 255\n",
    )
    .expect("write ssh shim");
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).expect("chmod ssh shim");

    std::env::set_var(
        "PATH",
        format!(
            "{}:{}",
            dir.path().display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    dir
}

fn askpass() -> AskpassEnv {
    AskpassEnv {
        script_path: "/dev/null".into(),
        socket_path: "/dev/null".into(),
    }
}

#[tokio::test]
#[serial]
async fn local_targets_skip_ssh() {
    let _shims = install_shims();
    let me = deptui::localhost::local_hostname().expect("a hostname");
    for target in ["localhost", "127.0.0.1", me, &format!("root@{me}")] {
        assert!(agentclient::is_local(target).await, "{target} is this box");
        let status = agentclient::probe(target)
            .await
            .unwrap_or_else(|e| panic!("probe {target}: {e:#}"));
        assert_eq!(status.version, "9.9.9");
        let status = agentclient::fetch_status(target, &askpass())
            .await
            .unwrap_or_else(|e| panic!("status {target}: {e:#}"));
        assert_eq!(status.version, "9.9.9");
    }
}

#[tokio::test]
#[serial]
async fn remote_targets_still_go_over_ssh() {
    let _shims = install_shims();
    // A documentation address: never this machine, never resolved.
    let err = agentclient::probe("deploy@192.0.2.1")
        .await
        .expect_err("the ssh shim refuses");
    assert!(
        format!("{err:#}").contains("Permission denied"),
        "expected the ssh shim's refusal, got: {err:#}"
    );
    assert!(!agentclient::is_local("deploy@192.0.2.1").await);
}

#[tokio::test]
#[serial]
async fn a_missing_local_cli_says_so() {
    // No shims: `deptui-agent` is not on PATH. The message has to name
    // the actual problem — an ssh-style error here would send the user
    // hunting for keys that were never involved.
    let dir = TempDir::new().expect("create temp dir");
    let saved = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", dir.path());
    let err = agentclient::probe("localhost")
        .await
        .expect_err("no binary");
    std::env::set_var("PATH", saved);
    let msg = format!("{err:#}");
    assert!(msg.contains("not on PATH"), "got: {msg}");
}
