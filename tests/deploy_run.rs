//! Integration tests for `deploy::run` using a PATH-shim `deploy` binary.
//!
//! Each test creates a tiny shell script that pretends to be the `deploy`
//! command. The script inspects `$@` to verify the flags we expect, writes
//! canned output to stdout/stderr, and exits with a controlled code. This
//! exercises the full `deploy::run` → `LogLine` channel pipeline without
//! touching a real Nix/deploy-rs installation.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use deptui::askpass::AskpassEnv;
use deptui::deploy::{self, DeployRequest, LogLine, Mode, ProfileSel, Toggles};
use deptui::ssh::SshOverride;
use serial_test::serial;
use tempfile::TempDir;

/// Install a `deploy` shim that echoes to stdout/stderr and exits.
fn install_deploy_shim(script_body: &str) -> TempDir {
    let dir = TempDir::new().expect("create temp dir");
    let shim = dir.path().join("deploy");
    fs::write(&shim, format!("#!/bin/sh\n{script_body}\n")).expect("write deploy shim");
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).expect("chmod deploy shim");
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    std::env::set_var("PATH", path);
    dir
}

/// Drain all `LogLine`s from a deploy handle and return them.
async fn collect_lines(mut handle: deploy::DeployHandle) -> Vec<LogLine> {
    let mut lines = Vec::new();
    while let Some(line) = handle.rx.recv().await {
        lines.push(line);
    }
    lines
}

fn basic_request() -> DeployRequest {
    DeployRequest {
        flake: ".".into(),
        node: "myhost".into(),
        profile: ProfileSel::All,
        mode: Mode::Switch,
        toggles: Toggles::default(),
        ssh_override: SshOverride::default(),
        askpass: AskpassEnv {
            script_path: "/dev/null".into(),
            socket_path: "/dev/null".into(),
        },
        profiles: Vec::new(),
        extra_build_args: Vec::new(),
        seed: None,
        node_info: None,
    }
}

#[tokio::test]
#[serial]
async fn successful_deploy_streams_lines() {
    let _dir = install_deploy_shim(
        r#"echo "building closure"
echo "activating profile" >&2
echo "done"
exit 0"#,
    );

    let handle = deploy::run(basic_request(), None);
    let lines = collect_lines(handle).await;

    // Should have at least stdout lines + an Exit(0).
    let stdout_lines: Vec<_> = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert!(stdout_lines.contains(&"building closure"));
    assert!(stdout_lines.contains(&"done"));

    let stderr_lines: Vec<_> = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stderr(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert!(stderr_lines.contains(&"activating profile"));

    let exit = lines.iter().find_map(|l| match l {
        LogLine::Exit(code) => Some(*code),
        _ => None,
    });
    assert_eq!(exit, Some(0));
}

#[tokio::test]
#[serial]
async fn failed_deploy_reports_exit_code() {
    let _dir = install_deploy_shim(
        r#"echo "error: activation failed" >&2
exit 42"#,
    );

    let handle = deploy::run(basic_request(), None);
    let lines = collect_lines(handle).await;

    let exit = lines.iter().find_map(|l| match l {
        LogLine::Exit(code) => Some(*code),
        _ => None,
    });
    assert_eq!(exit, Some(42));
}

#[tokio::test]
#[serial]
async fn boot_mode_flag() {
    // The shim dumps all args to stdout so we can verify --boot was passed.
    let _dir = install_deploy_shim(
        r#"for arg in "$@"; do echo "ARG:$arg"; done
exit 0"#,
    );

    let mut req = basic_request();
    req.mode = Mode::Boot;
    let handle = deploy::run(req, None);
    let lines = collect_lines(handle).await;

    let all_stdout: String = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all_stdout.contains("ARG:--boot"), "expected --boot in: {all_stdout}");
}

#[tokio::test]
#[serial]
async fn dry_run_mode_flag() {
    let _dir = install_deploy_shim(
        r#"for arg in "$@"; do echo "ARG:$arg"; done
exit 0"#,
    );

    let mut req = basic_request();
    req.mode = Mode::DryRun;
    let handle = deploy::run(req, None);
    let lines = collect_lines(handle).await;

    let all_stdout: String = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_stdout.contains("ARG:--dry-activate"),
        "expected --dry-activate in: {all_stdout}"
    );
}

#[tokio::test]
#[serial]
async fn toggle_flags_emitted_correctly() {
    let _dir = install_deploy_shim(
        r#"for arg in "$@"; do echo "ARG:$arg"; done
exit 0"#,
    );

    let mut req = basic_request();
    req.toggles.skip_checks = true;
    req.toggles.magic_rollback = false; // differs from default → emit
    req.toggles.auto_rollback = false; // differs from default → emit
    req.toggles.remote_build = true;

    let handle = deploy::run(req, None);
    let lines = collect_lines(handle).await;

    let all_stdout: String = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all_stdout.contains("ARG:-s"), "expected -s flag: {all_stdout}");
    assert!(
        all_stdout.contains("ARG:--magic-rollback"),
        "expected --magic-rollback: {all_stdout}"
    );
    assert!(
        all_stdout.contains("ARG:--auto-rollback"),
        "expected --auto-rollback: {all_stdout}"
    );
    assert!(
        all_stdout.contains("ARG:--remote-build"),
        "expected --remote-build: {all_stdout}"
    );
}

#[tokio::test]
#[serial]
async fn ssh_override_flags() {
    let _dir = install_deploy_shim(
        r#"for arg in "$@"; do echo "ARG:$arg"; done
exit 0"#,
    );

    let mut req = basic_request();
    req.ssh_override = SshOverride {
        hostname: Some("10.0.0.5".into()),
        user: Some("admin".into()),
        identity: Some("/home/me/.ssh/id_ed25519".into()),
        extra_opts: Some("Port=2222".into()),
    };

    let handle = deploy::run(req, None);
    let lines = collect_lines(handle).await;

    let all_stdout: String = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all_stdout.contains("ARG:--hostname"), "expected --hostname: {all_stdout}");
    assert!(all_stdout.contains("ARG:10.0.0.5"), "expected IP: {all_stdout}");
    assert!(all_stdout.contains("ARG:--ssh-user"), "expected --ssh-user: {all_stdout}");
    assert!(all_stdout.contains("ARG:admin"), "expected user: {all_stdout}");
    assert!(all_stdout.contains("ARG:--ssh-opts"), "expected --ssh-opts: {all_stdout}");
}

#[tokio::test]
#[serial]
async fn ansi_stripped_from_output() {
    let _dir = install_deploy_shim(
        r#"printf '\033[32mgreen text\033[0m\n'
exit 0"#,
    );

    let handle = deploy::run(basic_request(), None);
    let lines = collect_lines(handle).await;

    let stdout: Vec<_> = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    // The ANSI should be stripped — no \x1b in the output.
    for line in &stdout {
        assert!(
            !line.contains('\x1b'),
            "ANSI escape leaked through: {line:?}"
        );
    }
    assert!(
        stdout.iter().any(|l| l.contains("green text")),
        "expected 'green text' in: {stdout:?}"
    );
}

#[tokio::test]
#[serial]
async fn profile_suffix_in_target() {
    let _dir = install_deploy_shim(
        r#"for arg in "$@"; do echo "ARG:$arg"; done
exit 0"#,
    );

    let mut req = basic_request();
    req.profile = ProfileSel::System;
    let handle = deploy::run(req, None);
    let lines = collect_lines(handle).await;

    let all_stdout: String = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_stdout.contains("ARG:.#myhost.system"),
        "expected .#myhost.system in: {all_stdout}"
    );
}

/// Cancelling must take down the whole process group, not just the
/// direct `deploy` child. The shim forks a background sleeper — standing
/// in for the `nix` builders and `ssh` that a real deploy leaves behind —
/// and reports its pid so the test can check it actually died.
#[tokio::test]
#[serial]
async fn cancel_kills_the_whole_process_group() {
    let _dir = install_deploy_shim(
        r#"sleep 60 &
echo "GRANDCHILD:$!"
sleep 60"#,
    );

    let mut handle = deploy::run(basic_request(), None);

    // Wait for the shim to report the pid of its background process.
    let mut grandchild = None;
    while let Some(line) = handle.rx.recv().await {
        if let LogLine::Stdout(s) = line {
            if let Some(pid) = s.strip_prefix("GRANDCHILD:") {
                grandchild = pid.trim().parse::<i32>().ok();
                break;
            }
        }
    }
    let grandchild = grandchild.expect("shim reported no background pid");

    // Alive before the cancel, or the test proves nothing.
    assert_eq!(
        unsafe { libc::kill(grandchild, 0) },
        0,
        "background process {grandchild} should be running before cancel",
    );

    handle.cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), handle.task).await;

    // `kill(pid, 0)` returns -1/ESRCH once the process is gone. The shim
    // is not our child, so there is no zombie to keep the pid alive.
    let mut gone = false;
    for _ in 0..100 {
        if unsafe { libc::kill(grandchild, 0) } != 0 {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        gone,
        "background process {grandchild} survived the cancel — the deploy's \
process group was not signalled",
    );
}

/// `--build-arg` values must land after deploy-rs's `--` separator, or
/// they'd be parsed as deploy-rs's own flags instead of nix build args.
#[tokio::test]
#[serial]
async fn extra_build_args_are_passed_after_the_separator() {
    let _dir = install_deploy_shim(r#"for arg in "$@"; do echo "ARG:$arg"; done"#);

    let mut req = basic_request();
    req.extra_build_args = vec![
        "--option".into(),
        "extra-substituters".into(),
        "https://cuda".into(),
    ];
    let lines = collect_lines(deploy::run(req, None)).await;
    let all: Vec<String> = lines
        .iter()
        .filter_map(|l| match l {
            LogLine::Stdout(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let joined = all.join("\n");
    assert!(joined.contains("ARG:--"), "missing separator: {joined}");
    assert!(joined.contains("ARG:extra-substituters"), "{joined}");

    let sep = all.iter().position(|a| a == "ARG:--").expect("separator");
    let opt = all
        .iter()
        .position(|a| a == "ARG:extra-substituters")
        .expect("build arg");
    assert!(sep < opt, "build args must follow the separator: {joined}");
}
