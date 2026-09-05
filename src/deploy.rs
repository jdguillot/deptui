//! Wrapper around the `deploy` binary from serokell/deploy-rs.
//!
//! Each [`run`] call spawns `deploy` and forwards each stdout/stderr line
//! through an async channel so the TUI can render a live log. Cancellation
//! is achieved by dropping the join handle and killing the child via the
//! returned [`DeployHandle`].

use std::os::unix::io::{FromRawFd, RawFd};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use zeroize::Zeroizing;

use crate::askpass::AskpassEnv;
use crate::ssh::SshOverride;

/// What kind of activation deploy-rs should perform on the remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `switch-to-configuration switch` — apply immediately. Default.
    Switch,
    /// `--boot`: install the new generation as default but don't activate
    /// it until the next reboot.
    Boot,
    /// `--dry-activate`: build + diff only, no real activation.
    DryRun,
}

/// Which deploy-rs profiles to push for the selected node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSel {
    /// Both `system` and `home`. Equivalent to omitting the profile suffix.
    All,
    /// `system` only — NixOS host config.
    System,
    /// `home` only — home-manager.
    Home,
}

impl Mode {
    /// Human-readable label shared by log lines, the details pane, and
    /// the confirm popup.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Switch => "switch",
            Mode::Boot => "boot",
            Mode::DryRun => "dry-run",
        }
    }
}

impl ProfileSel {
    /// Human-readable label shared by log lines, the details pane, and
    /// the confirm popup.
    pub fn label(self) -> &'static str {
        match self {
            ProfileSel::All => "all",
            ProfileSel::System => "system",
            ProfileSel::Home => "home",
        }
    }

    fn target_suffix(self) -> &'static str {
        match self {
            ProfileSel::All => "",
            ProfileSel::System => ".system",
            ProfileSel::Home => ".home",
        }
    }
}

/// Boolean flags the user can toggle from the TUI. These all map directly
/// to deploy-rs CLI flags. We only emit a flag when the value differs
/// from deploy-rs's own default so the flake's `deploy.nodes.<name>`
/// settings stay authoritative for the un-overridden cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toggles {
    /// `-s, --skip-checks` — skip the pre-deploy `nix flake check`.
    pub skip_checks: bool,
    /// `--magic-rollback <bool>`. deploy-rs default is `true`.
    pub magic_rollback: bool,
    /// `--auto-rollback <bool>`. deploy-rs default is `true`.
    pub auto_rollback: bool,
    /// `--remote-build` — perform the build on the target host.
    pub remote_build: bool,
    /// `--interactive-sudo true`. When enabled, the TUI pre-prompts for
    /// the sudo password before spawning the child and pre-writes it
    /// into a PTY that backs the child's controlling tty. This is
    /// required because deploy-rs reads the password locally via
    /// `rpassword::prompt_password`, which opens `/dev/tty`; without a
    /// PTY the `setsid()` pre-exec hook leaves the child with no
    /// controlling terminal and rpassword would fail silently.
    pub interactive_sudo: bool,
}

/// One row of the toggle table — the single source of truth for the
/// TUI's toggle strip. `app` flips fields through `toggle` and logs
/// `name`; `ui` renders `short_label` (and derives the strip's width
/// from it) plus `help` in the help popup; `TOGGLES.len()` is the
/// count. Only the CLI flag each field maps to lives elsewhere
/// (`run_one`), because the only-emit-if-changed rule needs the
/// deploy-rs defaults for context.
pub struct ToggleDef {
    /// Name used in the "toggle flipped" log line.
    pub name: &'static str,
    /// Compact label shown in the toggles strip.
    pub short_label: &'static str,
    /// Description for the help popup.
    pub help: &'static str,
    /// Extra log line pushed when the toggle turns on, for toggles
    /// whose effect deserves a one-line explanation.
    pub on_hint: Option<&'static str>,
    pub get: fn(&Toggles) -> bool,
    /// Flip the field and return the new value.
    pub toggle: fn(&mut Toggles) -> bool,
}

pub const TOGGLES: &[ToggleDef] = &[
    ToggleDef {
        name: "skip-checks",
        short_label: "skip-checks",
        help: "skip-checks — skip the pre-deploy `nix flake check`",
        on_hint: None,
        get: |t| t.skip_checks,
        toggle: |t| {
            t.skip_checks = !t.skip_checks;
            t.skip_checks
        },
    },
    ToggleDef {
        name: "magic-rollback",
        short_label: "magic-rb",
        help: "magic-rollback — wait for confirmation, auto-roll-back on timeout (default ON)",
        on_hint: None,
        get: |t| t.magic_rollback,
        toggle: |t| {
            t.magic_rollback = !t.magic_rollback;
            t.magic_rollback
        },
    },
    ToggleDef {
        name: "auto-rollback",
        short_label: "auto-rb",
        help: "auto-rollback — roll back if activation fails (default ON)",
        on_hint: None,
        get: |t| t.auto_rollback,
        toggle: |t| {
            t.auto_rollback = !t.auto_rollback;
            t.auto_rollback
        },
    },
    ToggleDef {
        name: "remote-build",
        short_label: "remote-build",
        help: "remote-build — perform the build on the target host",
        on_hint: None,
        get: |t| t.remote_build,
        toggle: |t| {
            t.remote_build = !t.remote_build;
            t.remote_build
        },
    },
    ToggleDef {
        name: "interactive-sudo",
        short_label: "int-sudo",
        help: "interactive-sudo — TUI will prompt for the sudo password securely (masked input)",
        on_hint: Some("  interactive-sudo: TUI will prompt securely when sudo asks for a password"),
        get: |t| t.interactive_sudo,
        toggle: |t| {
            t.interactive_sudo = !t.interactive_sudo;
            t.interactive_sudo
        },
    },
];

impl Default for Toggles {
    fn default() -> Self {
        // Match deploy-rs's own defaults so an "untouched" toggles state
        // is a no-op compared to running `deploy` directly.
        Self {
            skip_checks: false,
            magic_rollback: true,
            auto_rollback: true,
            remote_build: false,
            interactive_sudo: false,
        }
    }
}

/// What we know about one of the node's deploy-rs profiles, in the order
/// deploy-rs pushes them. Used to decide whether the ssh-user override can
/// safely be applied to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileInfo {
    /// Profile name as it appears in `deploy.nodes.<node>.profiles`.
    pub name: String,
    /// The user the profile is activated as (`profiles.<n>.user`).
    pub user: Option<String>,
}

/// One `deploy` child process: which flake target suffix to push, and which
/// `--ssh-user` (if any) to pass for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// `""`, `".system"`, `".home"` — appended to `<flake>#<node>`.
    pub suffix: String,
    /// Value for `--ssh-user`, or `None` to leave the flake's own
    /// `sshUser` in charge.
    pub ssh_user: Option<String>,
    /// True when the ssh-user override was deliberately withheld here
    /// because applying it would break the profile's activation.
    pub override_withheld: bool,
}

/// Store paths to pull into the target's store from caches the deploy is
/// about to add, before the build runs.
///
/// This is the additive fix for the substituter trap (see
/// `host::seed_substituters`): purely a store population step, so
/// there is nothing to roll back if the deploy fails or is cancelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedPlan {
    pub substituters: Vec<String>,
    pub keys: Vec<String>,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DeployRequest {
    pub flake: String,
    pub node: String,
    pub profile: ProfileSel,
    pub mode: Mode,
    pub toggles: Toggles,
    /// Per-host SSH override. Empty/default means "no override, use the
    /// flake / ssh_config as-is".
    pub ssh_override: SshOverride,
    /// Askpass environment for SSH_ASKPASS integration.
    pub askpass: AskpassEnv,
    /// The node's profiles in deploy order. Empty when discovery didn't
    /// report any, in which case we fall back to the old single-invocation
    /// behaviour rather than guessing.
    pub profiles: Vec<ProfileInfo>,
    /// Extra arguments forwarded to `nix build` via deploy-rs's
    /// `-- <EXTRA_BUILD_ARGS>…` tail. Empty for the common case.
    pub extra_build_args: Vec<String>,
    /// Cache seeding to perform before the first build. `None` when no
    /// substituter drift was detected (or nothing was known about it).
    pub seed: Option<SeedPlan>,
    /// The node, needed to build the ssh target for the seeding step.
    pub node_info: Option<crate::flake::Node>,
}

/// Can the ssh-user override be applied to this profile without breaking
/// its activation?
///
/// `--ssh-user` is a *global* deploy-rs flag, but the activation user is
/// per-profile. When they differ deploy-rs runs `sudo -u <user>` from the
/// ssh login. That is fine for escalating to root, which needs no session
/// of its own — but `sudo -u <human>` from someone else's login inherits no
/// D-Bus and no `XDG_RUNTIME_DIR`, so home-manager's `systemctl --user`
/// step fails and the whole deploy rolls back.
fn override_fits(profile: &ProfileInfo, override_user: &str) -> bool {
    match profile.user.as_deref() {
        // Nothing to sudo to, or we're already that user.
        None => true,
        Some(u) if u == override_user => true,
        // Escalation to root needs no user session.
        Some("root") => true,
        Some(_) => false,
    }
}

impl DeployRequest {
    fn target_with(&self, suffix: &str) -> String {
        format!("{}#{}{}", self.flake, self.node, suffix)
    }

    /// Which profiles the current selection actually pushes.
    fn selected_profiles(&self) -> Vec<&ProfileInfo> {
        self.profiles
            .iter()
            .filter(|p| match self.profile {
                ProfileSel::All => true,
                ProfileSel::System => p.name == "system",
                ProfileSel::Home => p.name == "home",
            })
            .collect()
    }

    /// Expand this request into the `deploy` invocations to run, in order.
    ///
    /// Normally that's exactly one. It becomes several only when an
    /// ssh-user override is set and the selected profiles disagree about
    /// whether it fits — a single `--ssh-user` cannot serve both, so each
    /// profile gets its own child with the right login user.
    pub fn plan(&self) -> Vec<Invocation> {
        let whole = self.profile.target_suffix().to_string();

        let Some(override_user) = self.ssh_override.user.clone() else {
            return vec![Invocation {
                suffix: whole,
                ssh_user: None,
                override_withheld: false,
            }];
        };

        let selected = self.selected_profiles();

        // Discovery told us nothing — keep the previous behaviour rather
        // than silently dropping what the user asked for.
        if selected.is_empty() {
            return vec![Invocation {
                suffix: whole,
                ssh_user: Some(override_user),
                override_withheld: false,
            }];
        }

        let fits: Vec<bool> = selected
            .iter()
            .map(|p| override_fits(p, &override_user))
            .collect();

        if fits.iter().all(|ok| *ok) {
            return vec![Invocation {
                suffix: whole,
                ssh_user: Some(override_user),
                override_withheld: false,
            }];
        }
        if fits.iter().all(|ok| !*ok) {
            return vec![Invocation {
                suffix: whole,
                ssh_user: None,
                override_withheld: true,
            }];
        }

        selected
            .iter()
            .zip(fits)
            .map(|(p, ok)| Invocation {
                suffix: format!(".{}", p.name),
                ssh_user: if ok {
                    Some(override_user.clone())
                } else {
                    None
                },
                override_withheld: !ok,
            })
            .collect()
    }
}

/// A line of output emitted by the running `deploy` process. We tag the
/// stream so the TUI can colourise stderr differently if it wants to.
#[derive(Debug, Clone)]
pub enum LogLine {
    Stdout(String),
    Stderr(String),
    /// Remote sudo is waiting for a password (detected as a partial line on
    /// stderr matching a password-prompt pattern). The TUI should display a
    /// masked input widget and write the password via [`DeployHandle::stdin_tx`].
    SudoPrompt(String),
    /// Final exit code; the channel closes after this.
    Exit(i32),
    /// Spawn or wait failure.
    Error(String),
}

/// Stops a running deploy and everything it spawned.
///
/// Aborting the [`DeployHandle::task`] is *not* enough: `kill_on_drop`
/// only signals the direct `deploy` child, and because we `setsid()` in
/// `pre_exec` that child leads its own process group. Its descendants —
/// `nix` and the builders it forks, `ssh` — survive the leader and keep
/// burning CPU long after the user pressed `x`. Cancelling through this
/// handle signals the whole group instead, and does it from inside the
/// deploy task while it still owns the child, so the pid can't be
/// recycled between the TERM and the KILL.
#[derive(Debug, Clone)]
pub struct DeployCanceller {
    tx: watch::Sender<bool>,
}

impl DeployCanceller {
    /// Ask the running deploy to stop. Idempotent; safe to call when
    /// nothing is running.
    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }
}

pub struct DeployHandle {
    pub rx: mpsc::Receiver<LogLine>,
    /// Background task that owns the child. Drop to detach; call
    /// [`DeployHandle::cancel`] first so the process group is torn down.
    pub task: JoinHandle<()>,
    /// Stops the deploy and its whole process group.
    pub cancel: DeployCanceller,
    /// When `interactive_sudo` is enabled, send the sudo password here and
    /// it will be written to the child's piped stdin followed by a newline.
    /// `None` when the deploy was started without `--interactive-sudo`.
    /// Dropping this sender signals the stdin-writer task to exit.
    pub stdin_tx: Option<mpsc::Sender<String>>,
}

/// Spawn `deploy` for the given request and return a streaming handle.
///
/// When `req.toggles.interactive_sudo` is true, `sudo_password` should
/// contain the pre-collected sudo password. It is written into the
/// allocated PTY master so deploy-rs's `rpassword::prompt_password`
/// call (which reads from `/dev/tty`) receives it immediately — without
/// this, the `setsid()` pre-exec would leave the child with no
/// controlling terminal and rpassword would error out, causing the
/// remote sudo to run with an empty password.
pub fn run(req: DeployRequest, sudo_password: Option<Zeroizing<String>>) -> DeployHandle {
    let (tx, rx) = mpsc::channel(256);
    // Create the stdin channel only when interactive_sudo is enabled so
    // we don't allocate it for the common case.
    let interactive_sudo = req.toggles.interactive_sudo;
    let (stdin_tx, stdin_rx) = if interactive_sudo {
        let (s, r) = mpsc::channel::<String>(4);
        (Some(s), Some(r))
    } else {
        (None, None)
    };
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        if let Err(e) = run_inner(req, tx.clone(), stdin_rx, sudo_password, cancel_rx).await {
            let _ = tx.send(LogLine::Error(format!("{e:#}"))).await;
        }
    });
    DeployHandle {
        rx,
        task,
        cancel: DeployCanceller { tx: cancel_tx },
        stdin_tx,
    }
}

/// How long the deploy tree gets to wind down after `SIGTERM` before we
/// escalate to `SIGKILL`. Cancellation always takes at least this long
/// (see the comment in `run_one`), so keep it short enough that quitting
/// mid-deploy doesn't feel hung.
const CANCEL_GRACE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Send `sig` to the process group led by `pid`.
///
/// The `deploy` child is always a process-group leader (see the
/// `setsid()` hook in `AskpassEnv::pre_exec_setsid`), so its pid is also
/// its pgid and one `killpg` reaches every process it forked.
fn signal_group(pid: u32, sig: libc::c_int) {
    if pid == 0 {
        return;
    }
    // SAFETY: `killpg` with a valid signal number. A stale pgid yields
    // ESRCH, which we ignore.
    unsafe {
        libc::killpg(pid as libc::pid_t, sig);
    }
}

/// Resolve once the cancel flag flips to `true`. Never resolves if the
/// sender is dropped, which is what we want inside a `select!`.
async fn cancelled(rx: &mut watch::Receiver<bool>) {
    if rx.wait_for(|v| *v).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Allocate a pseudo-terminal pair. Returns `(master_fd, slave_fd)`.
/// Both ends are opened; the caller is responsible for closing them.
fn open_pty() -> std::io::Result<(RawFd, RawFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: `openpty` writes two valid fds into the out-params and
    // returns -1 on error. We pass null for the remaining optional
    // arguments (name buffer, termios, winsize).
    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((master, slave))
}

async fn run_inner(
    req: DeployRequest,
    tx: mpsc::Sender<LogLine>,
    stdin_rx: Option<mpsc::Receiver<String>>,
    sudo_password: Option<Zeroizing<String>>,
    cancel: watch::Receiver<bool>,
) -> Result<()> {
    let plan = req.plan();
    let multi = plan.len() > 1;

    // One receiver, several sequential children. Sharing it behind a mutex
    // lets each child's writer task take it in turn instead of the first
    // one consuming it for good.
    let stdin_rx = stdin_rx.map(|r| Arc::new(TokioMutex::new(r)));

    // Seed the target's store before anything is built. Done here rather
    // than in the App so it is naturally sequenced ahead of the first
    // child and shares the deploy's cancellation.
    if let (Some(seed), Some(node)) = (req.seed.clone(), req.node_info.clone()) {
        if *cancel.borrow() {
            return Ok(());
        }
        run_seed(&req, &node, &seed, &tx, cancel.clone()).await;
    }

    let mut code = 0;
    for (idx, inv) in plan.iter().enumerate() {
        if inv.override_withheld {
            if let Some(user) = &req.ssh_override.user {
                let profile = if inv.suffix.is_empty() {
                    "this deploy".to_string()
                } else {
                    format!("`{}`", inv.suffix.trim_start_matches('.'))
                };
                let _ = tx
                    .send(LogLine::Stderr(format!(
                        "• ssh-user override `{user}` not applied to {profile}: deploy-rs \
would log in as `{user}` and then `sudo -u` the profile's own user, which \
inherits no D-Bus session — home-manager's `systemctl --user` step fails \
there. Using the flake's sshUser for it instead."
                    )))
                    .await;
            }
        }
        if multi {
            let _ = tx
                .send(LogLine::Stdout(format!(
                    "• [{}/{}] deploying {}{}",
                    idx + 1,
                    plan.len(),
                    req.node,
                    inv.suffix
                )))
                .await;
        }

        code = run_one(
            &req,
            inv,
            &tx,
            stdin_rx.clone(),
            sudo_password.as_ref(),
            cancel.clone(),
        )
        .await?;
        // A non-zero code covers cancellation too (the child dies from a
        // signal), so this also stops us starting the next profile after
        // the user pressed `x`.
        if code != 0 || *cancel.borrow() {
            break;
        }
    }

    let _ = tx.send(LogLine::Exit(code)).await;
    Ok(())
}

/// Run the cache-seeding step, forwarding its progress into the log.
///
/// Failures are reported and then ignored: seeding is an optimisation,
/// and a deploy that would have compiled anyway is not made worse by a
/// cache that turned out to be unreachable.
async fn run_seed(
    req: &DeployRequest,
    node: &crate::flake::Node,
    seed: &SeedPlan,
    tx: &mpsc::Sender<LogLine>,
    mut cancel: watch::Receiver<bool>,
) {
    let (prog_tx, mut prog_rx) = mpsc::channel::<crate::host::ProgressLine>(64);
    let forward_tx = tx.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(line) = prog_rx.recv().await {
            if forward_tx.send(LogLine::Stdout(line.text)).await.is_err() {
                break;
            }
        }
    });

    let work = crate::host::seed_substituters(
        node,
        &req.ssh_override,
        &req.askpass,
        &seed.substituters,
        &seed.keys,
        &seed.paths,
        prog_tx,
    );

    let result = tokio::select! {
        r = work => Some(r),
        _ = cancelled(&mut cancel) => None,
    };
    let _ = forwarder.await;

    match result {
        None => {
            let _ = tx
                .send(LogLine::Stderr("! cache seeding cancelled".to_string()))
                .await;
        }
        Some(Ok(outcome)) => {
            let _ = tx
                .send(LogLine::Stdout(format!(
                    "• [seed] {} path(s) copied into the target's store, {} not in the new cache(s)",
                    outcome.copied, outcome.missed
                )))
                .await;
            if outcome.skipped > 0 {
                // Never let a capped run read as a complete one.
                let _ = tx
                    .send(LogLine::Stderr(format!(
                        "! [seed] {} further path(s) were not attempted (per-deploy cap)",
                        outcome.skipped
                    )))
                    .await;
            }
            if outcome.copied == 0 {
                let _ = tx
                    .send(LogLine::Stderr(
                        "! [seed] nothing was copied — the cache may not have these paths, or \
the ssh user may not be in the target's `trusted-users` (nix ignores the key option \
silently for untrusted users)"
                            .to_string(),
                    ))
                    .await;
            }
        }
        Some(Err(e)) => {
            let _ = tx
                .send(LogLine::Stderr(format!(
                    "! [seed] failed, continuing without it: {e:#}"
                )))
                .await;
        }
    }
}

/// Spawn a single `deploy` child for one [`Invocation`] and stream its
/// output. Returns the child's exit code; the caller owns the final
/// [`LogLine::Exit`].
async fn run_one(
    req: &DeployRequest,
    inv: &Invocation,
    tx: &mpsc::Sender<LogLine>,
    stdin_rx: Option<Arc<TokioMutex<mpsc::Receiver<String>>>>,
    sudo_password: Option<&Zeroizing<String>>,
    mut cancel: watch::Receiver<bool>,
) -> Result<i32> {
    let mut cmd = Command::new("deploy");
    cmd.arg(req.target_with(&inv.suffix));

    // Mode → activation flag.
    match req.mode {
        Mode::Switch => {}
        Mode::Boot => {
            cmd.arg("--boot");
        }
        Mode::DryRun => {
            cmd.arg("--dry-activate");
        }
    }

    // User toggles. Only emit a flag when it differs from the deploy-rs
    // default; otherwise we'd silently shadow the flake's settings.
    let t = req.toggles;
    if t.skip_checks {
        cmd.arg("-s");
    }
    if !t.magic_rollback {
        cmd.args(["--magic-rollback", "false"]);
    }
    if !t.auto_rollback {
        cmd.args(["--auto-rollback", "false"]);
    }
    if t.remote_build {
        cmd.arg("--remote-build");
    }
    if t.interactive_sudo {
        cmd.args(["--interactive-sudo", "true"]);
    }

    // Per-host SSH override → --hostname / --ssh-user / --ssh-opts.
    // The user comes from the invocation rather than the override itself:
    // `--ssh-user` applies to every profile in one `deploy` run, so
    // `plan()` may have withheld it here. See `override_fits`.
    if let Some(host) = &req.ssh_override.hostname {
        cmd.args(["--hostname", host]);
    }
    if let Some(user) = &inv.ssh_user {
        cmd.args(["--ssh-user", user]);
    }
    if let Some(opts) = req.ssh_override.deploy_ssh_opts() {
        cmd.args(["--ssh-opts", &opts]);
    }

    // deploy-rs takes extra `nix build` arguments after a `--`
    // separator, so this has to stay the last thing appended.
    if !req.extra_build_args.is_empty() {
        cmd.arg("--");
        cmd.args(&req.extra_build_args);
    }

    // When interactive_sudo is enabled, pipe stdin so the TUI can forward
    // the sudo password to the child. Otherwise null it out to prevent the
    // child from accidentally reading from the terminal.
    if stdin_rx.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("NO_COLOR", "1");

    // SSH_ASKPASS: use the app-level askpass env so SSH prompts are
    // routed through the TUI instead of corrupting the terminal.
    req.askpass.apply(&mut cmd);
    AskpassEnv::pre_exec_setsid(&mut cmd);

    // If interactive_sudo is on, allocate a PTY and wire the slave side
    // up as the child's controlling tty. deploy-rs reads the sudo
    // password locally via `rpassword::prompt_password`, which opens
    // `/dev/tty`. Without a controlling tty that open fails (ENXIO) and
    // deploy-rs proceeds with an empty password — hence the remote
    // "no password was provided" failures. By pre-feeding the password
    // into the PTY master we also avoid a visible terminal prompt flash
    // and let the TUI stay in charge of the UX.
    let (pty_master, pty_slave_fd) = if t.interactive_sudo {
        let (master_fd, slave_fd) = open_pty().context("allocating pty for interactive sudo")?;

        // SAFETY: `ioctl(fd, TIOCSCTTY, 0)` runs in the child after
        // `fork`, in the session created by the earlier `setsid()`
        // pre_exec hook. The slave fd was inherited across the fork,
        // so it's valid here. After the ioctl the child has a
        // controlling terminal and `/dev/tty` resolves to this PTY.
        unsafe {
            cmd.pre_exec(move || {
                if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The slave fd is no longer needed in the child — the
                // kernel keeps the terminal as controlling tty
                // regardless. Close it so we don't leak an extra fd
                // into every exec'd binary.
                libc::close(slave_fd);
                Ok(())
            });
        }

        // Pre-write the password + newline so rpassword reads it the
        // moment it opens /dev/tty. This is safe because the master
        // end buffers until the child reads it.
        // SAFETY: `from_raw_fd` takes ownership of the master fd.
        let mut master_file = unsafe { std::fs::File::from_raw_fd(master_fd) };
        if let Some(pw) = sudo_password {
            use std::io::Write;
            let _ = master_file.write_all(pw.as_bytes());
            let _ = master_file.write_all(b"\n");
            let _ = master_file.flush();
        }
        (Some(master_file), Some(slave_fd))
    } else {
        (None, None)
    };

    let mut child: Child = cmd.spawn().context("spawning `deploy`")?;

    // The slave fd now lives in the child; close our parent-side copy
    // so the PTY master sees EOF when the child exits.
    if let Some(fd) = pty_slave_fd {
        // SAFETY: closing our own dup of the slave fd. The child holds
        // its own fd (duplicated across fork) and will close it via
        // the pre_exec hook above.
        unsafe {
            libc::close(fd);
        }
    }

    // Drain the PTY master: forward any bytes deploy-rs writes to
    // /dev/tty (e.g. its "You will now be prompted for the sudo
    // password" banner) into the log. If the child produces no
    // output on the PTY the task just blocks until EOF.
    if let Some(master) = pty_master {
        let tx_pty = tx.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::{BufReader, Read};
            let mut reader = BufReader::with_capacity(READ_CHUNK, master);
            let mut line_buf: Vec<u8> = Vec::with_capacity(256);
            let mut chunk = [0u8; READ_CHUNK];
            'outer: loop {
                let n = match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                for &b in &chunk[..n] {
                    if b == b'\n' || b == b'\r' {
                        if line_buf.is_empty() {
                            continue;
                        }
                        let line = strip_ansi(&String::from_utf8_lossy(&line_buf));
                        line_buf.clear();
                        if line.trim().is_empty() {
                            continue;
                        }
                        if tx_pty.blocking_send(LogLine::Stderr(line)).is_err() {
                            break 'outer;
                        }
                    } else {
                        line_buf.push(b);
                        if line_buf.len() >= MAX_LINE {
                            let line = strip_ansi(&String::from_utf8_lossy(&line_buf));
                            line_buf.clear();
                            if tx_pty.blocking_send(LogLine::Stderr(line)).is_err() {
                                break 'outer;
                            }
                        }
                    }
                }
            }
        });
    }

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    // When interactive_sudo is enabled, spawn a task that reads password
    // strings from the channel and writes them (plus a newline) to the
    // child's piped stdin.
    if let (Some(child_stdin), Some(rx_arc)) = (child.stdin.take(), stdin_rx) {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut stdin = child_stdin;
            let mut rx = rx_arc.lock().await;
            while let Some(password) = rx.recv().await {
                // Write the password followed by a newline. A write error
                // means this child is gone — stop, so the next invocation's
                // task can take the receiver.
                if stdin.write_all(password.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
            // rx closed (sender dropped) or child gone → release the lock.
        });
    }

    let tx_out = tx.clone();
    let stdout_task = tokio::spawn(async move {
        forward_lines(stdout, tx_out, false).await;
    });

    let interactive_sudo = req.toggles.interactive_sudo;
    let tx_err = tx.clone();
    let stderr_task = tokio::spawn(async move {
        if interactive_sudo {
            read_stderr_interactive(stderr, tx_err).await;
        } else {
            forward_lines(stderr, tx_err, true).await;
        }
    });

    // The child leads its own process group, so `pid == pgid`.
    let pgid = child.id().unwrap_or(0);

    let status = tokio::select! {
        r = child.wait() => r.context("waiting for `deploy`")?,
        _ = cancelled(&mut cancel) => {
            let _ = tx
                .send(LogLine::Stderr(
                    "! stopping deploy — signalling the build process group".to_string(),
                ))
                .await;
            // SIGTERM first so `nix` gets a chance to unwind its builds;
            // signalling `deploy` alone would leave them running.
            signal_group(pgid, libc::SIGTERM);
            // Sleep rather than race `child.wait()`: waiting *reaps* the
            // leader, and the instant it is reaped its pid — which is
            // also the pgid — becomes available for reuse, so the
            // follow-up SIGKILL could land on an unrelated process
            // group. Holding the child unreaped keeps it a zombie, and a
            // zombie member keeps the pgid reserved for us.
            tokio::time::sleep(CANCEL_GRACE).await;
            // Catches anything that ignored SIGTERM or was forked after
            // it. A no-op (ESRCH) once the group is already empty.
            signal_group(pgid, libc::SIGKILL);
            child.wait().await.context("waiting for `deploy`")?
        }
    };

    let _ = stdout_task.await;
    let _ = stderr_task.await;

    Ok(status.code().unwrap_or(-1))
}

/// Remove ANSI terminal control sequences from a captured line.
///
/// `NO_COLOR=1` in the spawned environment tames `deploy-rs` itself,
/// but the nested `nix` / `nix-daemon` / `ssh` children don't all
/// honour it — in particular, remote `nix build` output that arrives
/// through ssh carries SGR colour codes, OSC title updates, cursor
/// moves, and the occasional raw ESC that ratatui's `Paragraph`
/// widget will happily render as literal bytes. When those bytes mix
/// into a `Line`, ratatui's width accounting drifts and individual
/// characters get dropped from the visible text (the classic
/// `dotfiles` → `dotf les` corruption).
///
/// We strip the common offenders here so every line that reaches the
/// TUI is plain utf-8 text:
///   - CSI sequences: `ESC [` … final byte in `0x40..=0x7e`
///   - OSC sequences: `ESC ]` … terminated by `BEL` or `ESC \\`
///   - Bare control bytes `\x00..=\x08`, `\x0b..=\x1f`, `\x7f`
///     except `\t` (tab, 0x09), which we keep verbatim
///
/// Line endings are already stripped by the line-buffered reader.
fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            // ESC — try to classify the sequence that follows.
            if i + 1 >= bytes.len() {
                i += 1;
                continue;
            }
            match bytes[i + 1] {
                // CSI: ESC [ params final
                b'[' => {
                    let mut j = i + 2;
                    while j < bytes.len() {
                        let c = bytes[j];
                        if (0x40..=0x7e).contains(&c) {
                            j += 1;
                            break;
                        }
                        j += 1;
                    }
                    i = j;
                }
                // OSC: ESC ] … BEL | ESC \
                b']' => {
                    let mut j = i + 2;
                    while j < bytes.len() {
                        if bytes[j] == 0x07 {
                            j += 1;
                            break;
                        }
                        if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                            j += 2;
                            break;
                        }
                        j += 1;
                    }
                    i = j;
                }
                // Two-byte escape: ESC <char>
                _ => {
                    i += 2;
                }
            }
            continue;
        }
        // Keep tabs and printable bytes; drop other control bytes.
        if b == b'\t' || b >= 0x20 && b != 0x7f {
            // Push as many contiguous printable bytes as possible in
            // one shot to keep the utf-8 sequences intact.
            let start = i;
            while i < bytes.len() {
                let c = bytes[i];
                if c == 0x1b || (c < 0x20 && c != b'\t') || c == 0x7f {
                    break;
                }
                i += 1;
            }
            out.push_str(std::str::from_utf8(&bytes[start..i]).unwrap_or(""));
            continue;
        }
        i += 1;
    }
    out
}
/// Read from the child's stderr byte-by-byte so we can detect partial-line
/// sudo prompts (which are written without a trailing newline).
///
/// Complete lines (ending with `\n`) are forwarded as [`LogLine::Stderr`].
/// Partial lines that match a known sudo / password prompt pattern are
/// forwarded as [`LogLine::SudoPrompt`] so the TUI can open the masked
/// input widget.
///
/// We use a `BufReader` wrapper so the actual `read` syscalls are buffered
/// even though we consume one byte at a time from it in userspace.
async fn read_stderr_interactive(stderr: ChildStderr, tx: mpsc::Sender<LogLine>) {
    read_stderr_interactive_impl(stderr, tx).await;
}

/// Inner implementation — generic over any `AsyncRead + Unpin` so it can be
/// tested without a live child process.
///
/// Reads in chunks (not one byte per `await`) and only re-scans the
/// accumulated buffer for a prompt when the byte just pushed could
/// actually terminate one. The previous version ran
/// `String::from_utf8_lossy` over the whole partial line after *every*
/// byte, which is quadratic — a few long `nix` lines were enough to
/// starve the reader and make the log look frozen.
async fn read_stderr_interactive_impl<R: tokio::io::AsyncRead + Unpin>(
    source: R,
    tx: mpsc::Sender<LogLine>,
) {
    let mut reader = BufReader::with_capacity(READ_CHUNK, source);
    let mut line_buf: Vec<u8> = Vec::with_capacity(256);
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break, // EOF
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        for &b in &chunk[..n] {
            // `\r` terminates a line too: `nix` repaints progress with a
            // bare carriage return, and treating it as ordinary content
            // buffers the output until some later `\n` that may never come.
            if b == b'\n' || b == b'\r' {
                if !flush_line(&mut line_buf, &tx, true).await {
                    return;
                }
                continue;
            }
            line_buf.push(b);
            // A prompt always ends in `:` or `: `, so only those two bytes
            // can complete one. Anything else can't, and re-scanning would
            // be wasted work.
            if b == b':'
                || (b == b' ' && line_buf.len() >= 2 && line_buf[line_buf.len() - 2] == b':')
            {
                let s = String::from_utf8_lossy(&line_buf);
                if is_sudo_prompt(s.as_ref()) {
                    let prompt = strip_ansi(s.as_ref());
                    line_buf.clear();
                    if tx.send(LogLine::SudoPrompt(prompt)).await.is_err() {
                        return;
                    }
                }
            } else if line_buf.len() >= MAX_LINE && !flush_line(&mut line_buf, &tx, true).await {
                return;
            }
        }
    }
    // Flush any remaining partial line that didn't end with a newline.
    let _ = flush_line(&mut line_buf, &tx, true).await;
}

/// Size of both the `BufReader` capacity and the userspace read buffer
/// used by the output forwarders.
const READ_CHUNK: usize = 8192;

/// Hard cap on how much a single unterminated line may buffer before we
/// forward it anyway. Without this a child that emits a large blob with
/// no line terminator would grow the buffer without bound and show
/// nothing in the TUI until EOF.
const MAX_LINE: usize = 16 * 1024;

/// Forward a child output stream to the log channel, one line at a time.
///
/// Deliberately byte-oriented instead of `AsyncBufReadExt::lines()`:
///
/// - `lines()` yields `Err` on the first byte sequence that isn't valid
///   UTF-8, and the `while let Ok(Some(_))` loop that consumed it ended
///   there — silently swallowing the rest of the deploy's output *and*
///   dropping the pipe, which SIGPIPEs the child mid-deploy. Nested
///   `nix` / `ssh` output is not guaranteed to be UTF-8, so decode
///   lossily and keep reading.
/// - `lines()` splits on `\n` only. Progress output that repaints with a
///   bare `\r` never produced a line at all, so the log appeared to stall
///   on whichever package was building.
async fn forward_lines<R: tokio::io::AsyncRead + Unpin>(
    source: R,
    tx: mpsc::Sender<LogLine>,
    is_err: bool,
) {
    let mut reader = BufReader::with_capacity(READ_CHUNK, source);
    let mut line_buf: Vec<u8> = Vec::with_capacity(256);
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        for &b in &chunk[..n] {
            if b == b'\n' || b == b'\r' {
                if !flush_line(&mut line_buf, &tx, is_err).await {
                    return;
                }
            } else {
                line_buf.push(b);
                if line_buf.len() >= MAX_LINE && !flush_line(&mut line_buf, &tx, is_err).await {
                    return;
                }
            }
        }
    }
    let _ = flush_line(&mut line_buf, &tx, is_err).await;
}

/// Send `buf` as one log line and clear it. Blank lines are dropped —
/// this is what keeps a `\r\n` pair from producing a spurious empty
/// entry now that both bytes terminate a line. Returns `false` when the
/// receiver is gone and the caller should stop reading.
async fn flush_line(buf: &mut Vec<u8>, tx: &mpsc::Sender<LogLine>, is_err: bool) -> bool {
    if buf.is_empty() {
        return true;
    }
    let text = strip_ansi(&String::from_utf8_lossy(buf));
    buf.clear();
    if text.trim().is_empty() {
        return true;
    }
    let line = if is_err {
        LogLine::Stderr(text)
    } else {
        LogLine::Stdout(text)
    };
    tx.send(line).await.is_ok()
}

/// Return `true` when `s` looks like a sudo/SSH password or passphrase
/// prompt that the process is waiting for — i.e. a partial line (no `\n`)
/// that ends with a colon (optionally followed by a space) and that
/// contains the keyword `password` or `passphrase`.
///
/// This is only ever called on partial (no-newline) lines accumulated from
/// a child's stderr; complete lines ending with `\n` are forwarded as
/// `LogLine::Stderr` before reaching this check. Because the child blocks
/// waiting for input when it writes a prompt without a trailing newline,
/// a partial line ending in `: ` that contains a password keyword is
/// overwhelmingly a real prompt.
///
/// Recognised patterns (case-insensitive):
/// - `[sudo] password for root: `
/// - `Password:`
/// - `Enter passphrase for key '…': `
fn is_sudo_prompt(s: &str) -> bool {
    let ends_like_prompt = s.ends_with(": ") || s.ends_with(':');
    if !ends_like_prompt {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    lower.contains("password") || lower.contains("passphrase")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_askpass() -> AskpassEnv {
        AskpassEnv {
            script_path: "/dev/null".into(),
            socket_path: "/dev/null".into(),
        }
    }

    fn req(
        profile: ProfileSel,
        override_user: Option<&str>,
        profiles: Vec<ProfileInfo>,
    ) -> DeployRequest {
        DeployRequest {
            flake: ".".into(),
            node: "host".into(),
            profile,
            mode: Mode::Switch,
            toggles: Toggles::default(),
            ssh_override: SshOverride {
                user: override_user.map(str::to_string),
                ..Default::default()
            },
            askpass: dummy_askpass(),
            profiles,
            extra_build_args: Vec::new(),
            seed: None,
            node_info: None,
        }
    }

    fn sys_and_home() -> Vec<ProfileInfo> {
        vec![
            ProfileInfo {
                name: "system".into(),
                user: Some("root".into()),
            },
            ProfileInfo {
                name: "home".into(),
                user: Some("jd".into()),
            },
        ]
    }

    // ---- ssh-user override planning ----

    #[test]
    fn plan_without_override_is_one_untouched_invocation() {
        let plan = req(ProfileSel::All, None, sys_and_home()).plan();
        assert_eq!(
            plan,
            vec![Invocation {
                suffix: "".into(),
                ssh_user: None,
                override_withheld: false
            }]
        );
    }

    #[test]
    fn plan_splits_when_override_fits_only_some_profiles() {
        // The regression this fixes: `--ssh-user root` is global, so the
        // home profile would be activated via `sudo -u jd` from root's
        // login — no session bus, home-manager activation fails, and
        // magic-rollback reverts the system profile too.
        let plan = req(ProfileSel::All, Some("root"), sys_and_home()).plan();
        assert_eq!(
            plan,
            vec![
                Invocation {
                    suffix: ".system".into(),
                    ssh_user: Some("root".into()),
                    override_withheld: false
                },
                Invocation {
                    suffix: ".home".into(),
                    ssh_user: None,
                    override_withheld: true
                },
            ]
        );
    }

    #[test]
    fn plan_keeps_single_invocation_when_override_fits_everything() {
        // ssh as the home profile's own user: no sudo, nothing to lose.
        let plan = req(ProfileSel::All, Some("jd"), sys_and_home()).plan();
        assert_eq!(
            plan,
            vec![Invocation {
                suffix: "".into(),
                ssh_user: Some("jd".into()),
                override_withheld: false
            }]
        );
    }

    #[test]
    fn plan_withholds_override_for_home_only_selection() {
        let plan = req(ProfileSel::Home, Some("root"), sys_and_home()).plan();
        assert_eq!(
            plan,
            vec![Invocation {
                suffix: ".home".into(),
                ssh_user: None,
                override_withheld: true
            }]
        );
    }

    #[test]
    fn plan_applies_override_for_system_only_selection() {
        let plan = req(ProfileSel::System, Some("root"), sys_and_home()).plan();
        assert_eq!(
            plan,
            vec![Invocation {
                suffix: ".system".into(),
                ssh_user: Some("root".into()),
                override_withheld: false
            }]
        );
    }

    #[test]
    fn plan_falls_back_when_profiles_unknown() {
        // Discovery gave us nothing — honour the override rather than
        // silently dropping what the user asked for.
        let plan = req(ProfileSel::All, Some("root"), Vec::new()).plan();
        assert_eq!(
            plan,
            vec![Invocation {
                suffix: "".into(),
                ssh_user: Some("root".into()),
                override_withheld: false
            }]
        );
    }

    #[test]
    fn plan_preserves_profile_order() {
        // `profiles` arrives in deploy order; the split must not reorder it.
        let mut reversed = sys_and_home();
        reversed.reverse();
        let plan = req(ProfileSel::All, Some("root"), reversed).plan();
        let suffixes: Vec<&str> = plan.iter().map(|i| i.suffix.as_str()).collect();
        assert_eq!(suffixes, vec![".home", ".system"]);
    }

    #[test]
    fn override_fits_rules() {
        let root = ProfileInfo {
            name: "system".into(),
            user: Some("root".into()),
        };
        let jd = ProfileInfo {
            name: "home".into(),
            user: Some("jd".into()),
        };
        let none = ProfileInfo {
            name: "x".into(),
            user: None,
        };
        // Escalating to root needs no session of its own.
        assert!(override_fits(&root, "jd"));
        // Already the right user.
        assert!(override_fits(&jd, "jd"));
        // No activation user to sudo to.
        assert!(override_fits(&none, "root"));
        // The broken case.
        assert!(!override_fits(&jd, "root"));
    }

    // ---- ProfileSel ----

    #[test]
    fn target_suffix_all() {
        assert_eq!(ProfileSel::All.target_suffix(), "");
    }

    #[test]
    fn target_suffix_system() {
        assert_eq!(ProfileSel::System.target_suffix(), ".system");
    }

    #[test]
    fn target_suffix_home() {
        assert_eq!(ProfileSel::Home.target_suffix(), ".home");
    }

    // ---- DeployRequest::target ----

    #[test]
    fn deploy_target_all_profiles() {
        let req = DeployRequest {
            flake: "/home/me/dotfiles".into(),
            node: "myhost".into(),
            profile: ProfileSel::All,
            mode: Mode::Switch,
            toggles: Toggles::default(),
            ssh_override: SshOverride::default(),
            askpass: dummy_askpass(),
            profiles: Vec::new(),
            extra_build_args: Vec::new(),
            seed: None,
            node_info: None,
        };
        assert_eq!(
            req.target_with(&req.plan()[0].suffix),
            "/home/me/dotfiles#myhost"
        );
    }

    #[test]
    fn deploy_target_system_only() {
        let req = DeployRequest {
            flake: ".".into(),
            node: "server1".into(),
            profile: ProfileSel::System,
            mode: Mode::Boot,
            toggles: Toggles::default(),
            ssh_override: SshOverride::default(),
            askpass: dummy_askpass(),
            profiles: Vec::new(),
            extra_build_args: Vec::new(),
            seed: None,
            node_info: None,
        };
        assert_eq!(req.target_with(&req.plan()[0].suffix), ".#server1.system");
    }

    #[test]
    fn deploy_target_home_only() {
        let req = DeployRequest {
            flake: "github:me/dotfiles".into(),
            node: "laptop".into(),
            profile: ProfileSel::Home,
            mode: Mode::DryRun,
            toggles: Toggles::default(),
            ssh_override: SshOverride::default(),
            askpass: dummy_askpass(),
            profiles: Vec::new(),
            extra_build_args: Vec::new(),
            seed: None,
            node_info: None,
        };
        assert_eq!(
            req.target_with(&req.plan()[0].suffix),
            "github:me/dotfiles#laptop.home"
        );
    }

    // ---- Toggles ----

    #[test]
    fn toggles_default_matches_deploy_rs() {
        let t = Toggles::default();
        assert!(!t.skip_checks);
        assert!(t.magic_rollback);
        assert!(t.auto_rollback);
        assert!(!t.remote_build);
        assert!(!t.interactive_sudo);
    }

    // ---- strip_ansi (existing + new) ----

    #[test]
    fn strips_csi_color_sequences() {
        let input = "\x1b[38;5;120mhello\x1b[0m world";
        assert_eq!(strip_ansi(input), "hello world");
    }

    #[test]
    fn preserves_utf8_and_tabs() {
        let input = "→ deploying\t/home/jdguillot/.dotfiles";
        assert_eq!(strip_ansi(input), "→ deploying\t/home/jdguillot/.dotfiles");
    }

    #[test]
    fn strips_osc_title_sequence() {
        let input = "\x1b]0;title\x07after";
        assert_eq!(strip_ansi(input), "after");
    }

    #[test]
    fn strips_bare_esc_and_control_bytes() {
        // \x05 (ENQ) is a bare control byte → stripped.
        // \x1b consumes the next byte as a two-byte escape → " " eaten.
        // \x7f (DEL) → stripped.
        let input = "warn\x05ing \x1b ok\x7f";
        assert_eq!(strip_ansi(input), "warning ok");
    }

    // ---- is_sudo_prompt ----

    #[test]
    fn sudo_prompt_bracket_form() {
        assert!(is_sudo_prompt("[sudo] password for root: "));
    }

    #[test]
    fn sudo_prompt_bare_colon() {
        assert!(is_sudo_prompt("Password:"));
    }

    #[test]
    fn sudo_prompt_passphrase() {
        assert!(is_sudo_prompt(
            "Enter passphrase for key '/home/me/.ssh/id_ed25519': "
        ));
    }

    #[test]
    fn sudo_prompt_not_triggered_by_regular_line() {
        // keyword present but line doesn't end with ":"
        assert!(!is_sudo_prompt("incorrect password supplied"));
        // ends with ":" but no password keyword
        assert!(!is_sudo_prompt("activating profile:"));
        // ends with "password:" — counts as a prompt because the process
        // would only write a no-newline partial line while waiting for input.
        // This is intentional: a false positive here is a recoverable UX
        // issue (user presses Esc), while a false negative would silently hang.
        assert!(is_sudo_prompt("Password:"));
    }

    // ---- forward_lines ----

    async fn run_forward(input: &[u8]) -> Vec<String> {
        use tokio::io::AsyncWriteExt;
        let (mut w, r) = tokio::io::duplex(4096);
        // The writer runs concurrently: inputs larger than the duplex
        // buffer would otherwise block before the reader ever starts.
        let data = input.to_vec();
        let writer = tokio::spawn(async move {
            let _ = w.write_all(&data).await;
            drop(w); // signal EOF
        });
        let (tx, mut rx) = mpsc::channel(64);
        let reader = tokio::spawn(async move { forward_lines(r, tx, false).await });

        let mut out = Vec::new();
        while let Some(line) = rx.recv().await {
            match line {
                LogLine::Stdout(s) => out.push(s),
                other => panic!("unexpected {other:?}"),
            }
        }
        writer.await.unwrap();
        reader.await.unwrap();
        out
    }

    #[tokio::test]
    async fn forward_lines_splits_on_newline() {
        assert_eq!(run_forward(b"one\ntwo\n").await, vec!["one", "two"]);
    }

    #[tokio::test]
    async fn forward_lines_survives_invalid_utf8() {
        // The old `lines()`-based reader returned Err here and stopped
        // forwarding for the rest of the deploy — the log "just stopped".
        let out = run_forward(b"before\n\xff\xfe bad\nafter\n").await;
        assert_eq!(out.first().map(String::as_str), Some("before"));
        assert_eq!(
            out.last().map(String::as_str),
            Some("after"),
            "output after a non-UTF-8 byte must still be forwarded: {out:?}",
        );
    }

    #[tokio::test]
    async fn forward_lines_treats_carriage_return_as_a_line_break() {
        // nix repaints build progress with a bare `\r`. Buffering those
        // until a `\n` that never arrives looked like a stalled log.
        assert_eq!(
            run_forward(b"building foo\rbuilding bar\r").await,
            vec!["building foo", "building bar"],
        );
    }

    #[tokio::test]
    async fn forward_lines_crlf_makes_one_entry() {
        assert_eq!(run_forward(b"one\r\ntwo\r\n").await, vec!["one", "two"]);
    }

    #[tokio::test]
    async fn forward_lines_flushes_unterminated_tail() {
        assert_eq!(
            run_forward(b"no trailing newline").await,
            vec!["no trailing newline"]
        );
    }

    #[tokio::test]
    async fn forward_lines_caps_a_terminator_free_stream() {
        // Without a cap this buffers forever and shows nothing until EOF.
        let blob = vec![b'x'; MAX_LINE * 2 + 10];
        let out = run_forward(&blob).await;
        assert!(
            out.len() >= 2,
            "expected the blob to be chunked, got {} entries",
            out.len()
        );
        assert!(out.iter().all(|l| l.len() <= MAX_LINE));
    }

    // ---- read_stderr_interactive_impl ----

    async fn run_stderr_reader(input: &[u8]) -> Vec<LogLine> {
        use tokio::io::AsyncWriteExt;
        let (mut w, r) = tokio::io::duplex(1024);
        w.write_all(input).await.unwrap();
        drop(w); // signal EOF
        let (tx, mut rx) = mpsc::channel(32);
        read_stderr_interactive_impl(r, tx).await;
        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }
        lines
    }

    #[tokio::test]
    async fn stderr_impl_complete_lines_become_stderr() {
        let lines = run_stderr_reader(b"deploying\nactivating\n").await;
        assert!(matches!(&lines[0], LogLine::Stderr(s) if s == "deploying"));
        assert!(matches!(&lines[1], LogLine::Stderr(s) if s == "activating"));
    }

    #[tokio::test]
    async fn stderr_impl_sudo_prompt_becomes_sudo_prompt() {
        let lines = run_stderr_reader(b"[sudo] password for root: ").await;
        // The prompt fires when ":" is seen; the trailing " " may arrive
        // as a separate Stderr entry. The important invariant is that the
        // first line is a SudoPrompt containing the key phrase.
        assert!(
            matches!(&lines[0], LogLine::SudoPrompt(s) if s.contains("password for root")),
            "expected SudoPrompt containing 'password for root', got: {lines:?}",
        );
    }

    #[tokio::test]
    async fn stderr_impl_partial_line_without_prompt_flushed_as_stderr() {
        // A partial line (no newline) that doesn't match a sudo prompt is
        // flushed as Stderr at EOF.
        let lines = run_stderr_reader(b"no newline here").await;
        assert!(matches!(&lines[0], LogLine::Stderr(s) if s == "no newline here"));
    }

    #[tokio::test]
    async fn stderr_impl_mixed_output() {
        // Complete line followed by a sudo prompt (no trailing newline).
        let lines = run_stderr_reader(b"activating profile\n[sudo] password for root: ").await;
        assert!(matches!(&lines[0], LogLine::Stderr(s) if s == "activating profile"));
        assert!(matches!(&lines[1], LogLine::SudoPrompt(_)));
    }

    #[tokio::test]
    async fn stderr_impl_empty_input() {
        let lines = run_stderr_reader(b"").await;
        assert!(lines.is_empty());
    }
}
