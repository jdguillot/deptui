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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::mpsc;
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

impl ProfileSel {
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
                ssh_user: if ok { Some(override_user.clone()) } else { None },
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

pub struct DeployHandle {
    pub rx: mpsc::Receiver<LogLine>,
    /// Background task that owns the child. Drop or `.abort()` to cancel.
    pub task: JoinHandle<()>,
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
    let task = tokio::spawn(async move {
        if let Err(e) = run_inner(req, tx.clone(), stdin_rx, sudo_password).await {
            let _ = tx.send(LogLine::Error(format!("{e:#}"))).await;
        }
    });
    DeployHandle {
        rx,
        task,
        stdin_tx,
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
) -> Result<()> {
    let plan = req.plan();
    let multi = plan.len() > 1;

    // One receiver, several sequential children. Sharing it behind a mutex
    // lets each child's writer task take it in turn instead of the first
    // one consuming it for good.
    let stdin_rx = stdin_rx.map(|r| Arc::new(TokioMutex::new(r)));

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

        code = run_one(&req, inv, &tx, stdin_rx.clone(), sudo_password.as_ref()).await?;
        if code != 0 {
            break;
        }
    }

    let _ = tx.send(LogLine::Exit(code)).await;
    Ok(())
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
            let mut reader = BufReader::new(master);
            let mut line_buf: Vec<u8> = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                match reader.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if byte[0] == b'\n' {
                            let s = String::from_utf8_lossy(&line_buf);
                            let line = s.trim_end_matches('\r').to_string();
                            line_buf.clear();
                            if line.is_empty() {
                                continue;
                            }
                            if tx_pty
                                .blocking_send(LogLine::Stderr(strip_ansi(&line)))
                                .is_err()
                            {
                                break;
                            }
                        } else {
                            line_buf.push(byte[0]);
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
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if tx_out
                .send(LogLine::Stdout(strip_ansi(&line)))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let interactive_sudo = req.toggles.interactive_sudo;
    let tx_err = tx.clone();
    let stderr_task = tokio::spawn(async move {
        if interactive_sudo {
            read_stderr_interactive(stderr, tx_err).await;
        } else {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if tx_err
                    .send(LogLine::Stderr(strip_ansi(&line)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });

    let status = child.wait().await.context("waiting for `deploy`")?;
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
async fn read_stderr_interactive_impl<R: tokio::io::AsyncRead + Unpin>(
    source: R,
    tx: mpsc::Sender<LogLine>,
) {
    let mut reader = BufReader::new(source);
    let mut line_buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                if byte[0] == b'\n' {
                    // Complete line — strip CR if present.
                    let s = String::from_utf8_lossy(&line_buf);
                    let line = s.trim_end_matches('\r').to_string();
                    line_buf.clear();
                    if line.is_empty() {
                        continue;
                    }
                    if tx.send(LogLine::Stderr(strip_ansi(&line))).await.is_err() {
                        break;
                    }
                } else {
                    line_buf.push(byte[0]);
                    // After each byte, check whether the accumulated buffer
                    // looks like a password prompt waiting for input.
                    let s = String::from_utf8_lossy(&line_buf);
                    if is_sudo_prompt(s.as_ref()) {
                        let prompt = s.to_string();
                        line_buf.clear();
                        if tx.send(LogLine::SudoPrompt(strip_ansi(&prompt))).await.is_err() {
                            break;
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
    // Flush any remaining partial line that didn't end with a newline.
    if !line_buf.is_empty() {
        let s = String::from_utf8_lossy(&line_buf).to_string();
        let _ = tx.send(LogLine::Stderr(strip_ansi(&s))).await;
    }
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

    fn req(profile: ProfileSel, override_user: Option<&str>, profiles: Vec<ProfileInfo>) -> DeployRequest {
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
        }
    }

    fn sys_and_home() -> Vec<ProfileInfo> {
        vec![
            ProfileInfo { name: "system".into(), user: Some("root".into()) },
            ProfileInfo { name: "home".into(), user: Some("jd".into()) },
        ]
    }

    // ---- ssh-user override planning ----

    #[test]
    fn plan_without_override_is_one_untouched_invocation() {
        let plan = req(ProfileSel::All, None, sys_and_home()).plan();
        assert_eq!(
            plan,
            vec![Invocation { suffix: "".into(), ssh_user: None, override_withheld: false }]
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
                Invocation { suffix: ".system".into(), ssh_user: Some("root".into()), override_withheld: false },
                Invocation { suffix: ".home".into(), ssh_user: None, override_withheld: true },
            ]
        );
    }

    #[test]
    fn plan_keeps_single_invocation_when_override_fits_everything() {
        // ssh as the home profile's own user: no sudo, nothing to lose.
        let plan = req(ProfileSel::All, Some("jd"), sys_and_home()).plan();
        assert_eq!(
            plan,
            vec![Invocation { suffix: "".into(), ssh_user: Some("jd".into()), override_withheld: false }]
        );
    }

    #[test]
    fn plan_withholds_override_for_home_only_selection() {
        let plan = req(ProfileSel::Home, Some("root"), sys_and_home()).plan();
        assert_eq!(
            plan,
            vec![Invocation { suffix: ".home".into(), ssh_user: None, override_withheld: true }]
        );
    }

    #[test]
    fn plan_applies_override_for_system_only_selection() {
        let plan = req(ProfileSel::System, Some("root"), sys_and_home()).plan();
        assert_eq!(
            plan,
            vec![Invocation { suffix: ".system".into(), ssh_user: Some("root".into()), override_withheld: false }]
        );
    }

    #[test]
    fn plan_falls_back_when_profiles_unknown() {
        // Discovery gave us nothing — honour the override rather than
        // silently dropping what the user asked for.
        let plan = req(ProfileSel::All, Some("root"), Vec::new()).plan();
        assert_eq!(
            plan,
            vec![Invocation { suffix: "".into(), ssh_user: Some("root".into()), override_withheld: false }]
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
        let root = ProfileInfo { name: "system".into(), user: Some("root".into()) };
        let jd = ProfileInfo { name: "home".into(), user: Some("jd".into()) };
        let none = ProfileInfo { name: "x".into(), user: None };
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
        };
        assert_eq!(req.target_with(&req.plan()[0].suffix), "/home/me/dotfiles#myhost");
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
        };
        assert_eq!(req.target_with(&req.plan()[0].suffix), "github:me/dotfiles#laptop.home");
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
