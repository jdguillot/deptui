//! SSH_ASKPASS mechanism for interactive SSH/sudo authentication.
//!
//! SSH reads passwords from `/dev/tty` by default, which corrupts the TUI.
//! To intercept these prompts we:
//!
//! 1. Set `SSH_ASKPASS` to a wrapper script that invokes our own binary in
//!    `--askpass` mode.
//! 2. Set `SSH_ASKPASS_REQUIRE=prefer` and detach the child from its
//!    controlling terminal (`setsid`) so SSH is forced to use the askpass
//!    program instead of `/dev/tty`.
//! 3. The askpass helper connects to a Unix-domain socket, sends the prompt
//!    text, and reads the password back.
//! 4. The TUI's [`AskpassServer`] accepts connections on that socket and
//!    forwards prompts/responses through async channels.
//!
//! ## Client mode
//!
//! When the binary is invoked as `deptui --askpass <prompt>`, [`run_client`]
//! handles the short-lived helper side of the protocol.

use std::io::{BufRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

/// Longest prompt we will read from a helper. SSH prompts are one short
/// line; anything larger is a bug or a hostile client, and reading it
/// unbounded would let a single connection exhaust memory.
const MAX_PROMPT_BYTES: u64 = 4096;

/// How long a connected helper has to send its prompt line. The accept
/// loop is deliberately serial (one password dialog at a time), so a
/// client that connects and never writes would otherwise wedge the whole
/// askpass mechanism for the rest of the session.
const PROMPT_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Strip a prompt down to something safe to render in the TUI.
///
/// The prompt is attacker-influenced in the general case (it embeds host
/// names and key paths), and it is drawn straight into a ratatui popup.
/// Control bytes there would do exactly what they do in the deploy log:
/// desynchronise ratatui's width accounting and corrupt the screen. Tabs
/// go too — the popup is a single line.
fn sanitise_prompt(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_PROMPT_LEN)
        .collect();
    let trimmed = out.trim_end();
    if trimmed.len() != out.len() {
        out.truncate(trimmed.len());
    }
    out
}

/// Character cap for a rendered prompt, applied after control bytes are
/// removed. The popup only has one line to give it.
const MAX_PROMPT_LEN: usize = 512;

/// Single-quote a string for POSIX `sh`.
///
/// Used for the binary path baked into the wrapper script. The path comes
/// from `current_exe()`, but "unlikely to contain a quote" is not a
/// security property — quoting it correctly is.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Lightweight, cloneable handle that any spawned [`Command`] can use to
/// opt-in to the askpass mechanism. Holds only the paths — no channels,
/// no listener — so it's cheap to send into background tasks.
///
/// Use [`AskpassEnv::apply`] to stamp the required environment variables
/// onto a [`tokio::process::Command`] and [`AskpassEnv::pre_exec_setsid`]
/// to detach from the controlling terminal.
#[derive(Debug, Clone)]
pub struct AskpassEnv {
    pub script_path: PathBuf,
    pub socket_path: PathBuf,
}

impl AskpassEnv {
    /// Set `SSH_ASKPASS`, `SSH_ASKPASS_REQUIRE`, `DISPLAY`, and
    /// `DEPTUI_ASKPASS_SOCK` on the given command.
    pub fn apply(&self, cmd: &mut tokio::process::Command) {
        cmd.env("SSH_ASKPASS", &self.script_path);
        cmd.env("SSH_ASKPASS_REQUIRE", "prefer");
        cmd.env("DISPLAY", "deptui");
        cmd.env("DEPTUI_ASKPASS_SOCK", &self.socket_path);
    }

    /// Install a `pre_exec` hook that calls `setsid(2)` so the child
    /// has no controlling terminal — forcing SSH to use the askpass
    /// program instead of `/dev/tty`.
    ///
    /// # Safety
    /// `setsid()` is async-signal-safe and has no preconditions beyond
    /// being called after `fork` (which `pre_exec` guarantees).
    pub fn pre_exec_setsid(cmd: &mut tokio::process::Command) {
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
}

/// Server half — lives for the lifetime of the `App`, accepts connections
/// from askpass helper processes spawned by SSH.
pub struct AskpassServer {
    /// Paths needed by child processes (cheap to clone into tasks).
    pub env: AskpassEnv,
    /// Keep the temp dir alive; it is removed on drop.
    _dir: tempfile::TempDir,
    listener: UnixListener,
}

impl AskpassServer {
    /// Create a new server: temp dir, Unix socket, and wrapper script.
    pub fn new() -> Result<Self> {
        let dir = tempfile::tempdir().context("creating askpass temp dir")?;
        let socket_path = dir.path().join("askpass.sock");
        let script_path = dir.path().join("deptui-askpass");

        let exe = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .context("resolving own binary path for SSH_ASKPASS")?;

        // Wrapper script: SSH calls this with the prompt as $1.
        let script = format!(
            "#!/bin/sh\nexec {} --askpass \"$@\"\n",
            shell_quote(&exe.to_string_lossy())
        );
        std::fs::write(&script_path, &script).context("writing the askpass wrapper script")?;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .context("locking down the askpass wrapper script")?;

        let listener =
            UnixListener::bind(&socket_path).context("binding askpass Unix socket")?;
        // The 0700 temp dir is the real boundary — nobody else can even
        // traverse into it — but the socket's own mode is left to the
        // umask by `bind`, and a lax umask would make it group-writable.
        // Anything that can connect here gets a password dialog put in
        // front of the user, so say 0600 explicitly rather than relying
        // on the environment.
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
            .context("locking down the askpass socket")?;

        Ok(Self {
            env: AskpassEnv {
                script_path,
                socket_path,
            },
            _dir: dir,
            listener,
        })
    }

    /// Run the accept loop. Each connection is one askpass invocation
    /// (one password prompt from SSH or sudo).
    ///
    /// `prompt_tx` — sends the prompt text to the TUI.
    /// `password_rx` — receives the user's password from the TUI.
    ///
    /// The loop exits when either channel closes or the listener errors.
    pub async fn serve(
        &self,
        prompt_tx: mpsc::Sender<String>,
        mut password_rx: mpsc::Receiver<Zeroizing<String>>,
    ) {
        loop {
            let (stream, _) = match self.listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };

            let (reader, mut writer) = stream.into_split();
            // Bounded *and* time-limited: this loop handles one dialog at
            // a time, so a client that floods or stalls here would take
            // the whole mechanism down with it.
            let mut reader = BufReader::new(reader).take(MAX_PROMPT_BYTES);
            let mut raw = String::new();
            let read = tokio::time::timeout(
                PROMPT_READ_TIMEOUT,
                reader.read_line(&mut raw),
            )
            .await;
            if !matches!(read, Ok(Ok(_))) {
                continue;
            }
            let prompt = sanitise_prompt(&raw);
            if prompt.is_empty() {
                continue;
            }

            // Forward the prompt to the TUI.
            if prompt_tx.send(prompt).await.is_err() {
                break;
            }

            // Wait for the password from the TUI. The App always replies
            // — with an empty string if the user dismissed the dialog —
            // because a silent dismissal here would block this loop
            // forever and kill askpass for the rest of the session.
            match password_rx.recv().await {
                Some(password) => {
                    let _ = writer.write_all(password.as_bytes()).await;
                    let _ = writer.write_all(b"\n").await;
                    let _ = writer.shutdown().await;
                }
                None => break, // TUI closed the channel.
            }
        }
    }
}

/// Client half — called when the binary is invoked as `deptui --askpass`.
///
/// SSH passes the prompt text as the first positional argument. We connect
/// to the TUI's Unix socket, send the prompt, read the password, and print
/// it to stdout (which SSH reads).
pub fn run_client(prompt: &str) -> Result<()> {
    let sock_path = std::env::var("DEPTUI_ASKPASS_SOCK")
        .context("DEPTUI_ASKPASS_SOCK not set — not running inside deptui?")?;

    let stream = std::os::unix::net::UnixStream::connect(&sock_path)
        .with_context(|| format!("connecting to askpass socket at {sock_path}"))?;

    let mut writer = std::io::BufWriter::new(&stream);
    writeln!(writer, "{prompt}")?;
    writer.flush()?;

    stream
        .shutdown(std::net::Shutdown::Write)
        .context("shutting down write half")?;

    let mut reader = std::io::BufReader::new(&stream);
    let mut password = String::new();
    reader
        .read_line(&mut password)
        .context("reading password from askpass server")?;

    // Print the password for SSH to read (no trailing newline — SSH adds one).
    let password = password.trim_end_matches('\n');
    print!("{password}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitise_prompt_strips_control_bytes() {
        // A prompt embeds host names and key paths; escape sequences in
        // it would desynchronise ratatui exactly like they do in the log.
        let dirty = "Enter passphrase\x1b[2Jfor key\x07 '/k': \n";
        let clean = sanitise_prompt(dirty);
        assert!(!clean.contains('\x1b'), "{clean:?}");
        assert!(!clean.contains('\x07'), "{clean:?}");
        assert!(!clean.contains('\n'), "{clean:?}");
        assert!(clean.starts_with("Enter passphrase"), "{clean:?}");
        assert!(clean.ends_with("'/k':"), "{clean:?}");
    }

    #[test]
    fn sanitise_prompt_caps_length() {
        let clean = sanitise_prompt(&"x".repeat(MAX_PROMPT_LEN * 4));
        assert_eq!(clean.len(), MAX_PROMPT_LEN);
    }

    #[test]
    fn sanitise_prompt_rejects_control_only_input() {
        assert!(sanitise_prompt("\x1b\x07\n").is_empty());
    }

    #[test]
    fn wrapper_script_quotes_the_binary_path() {
        assert_eq!(shell_quote("/usr/bin/deptui"), "'/usr/bin/deptui'");
        assert_eq!(
            shell_quote("/tmp/it's here/deptui"),
            r#"'/tmp/it'\''s here/deptui'"#
        );
    }

    #[test]
    fn wrapper_script_survives_a_hostile_binary_path() {
        // Round-trip through a real `sh` — the script is `exec`'d by
        // ssh, so a quoting bug is a code-execution bug.
        let path = "/tmp/a'; touch /tmp/deptui-askpass-escape; echo '";
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", shell_quote(path)))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), path);
        assert!(!std::path::Path::new("/tmp/deptui-askpass-escape").exists());
    }

    #[tokio::test]
    async fn socket_and_script_are_owner_only() {
        let server = AskpassServer::new().unwrap();
        for path in [&server.env.socket_path, &server.env.script_path] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode & 0o077,
                0,
                "{} is accessible beyond its owner: {mode:o}",
                path.display()
            );
        }
    }

    /// A helper that connects and then goes silent must not wedge the
    /// accept loop — it is serial, so one stalled client would kill
    /// askpass for the rest of the session.
    #[tokio::test(start_paused = true)]
    async fn a_silent_client_does_not_wedge_the_accept_loop() {
        let server = AskpassServer::new().unwrap();
        let sock = server.env.socket_path.clone();
        let (prompt_tx, mut prompt_rx) = mpsc::channel::<String>(4);
        let (password_tx, password_rx) = mpsc::channel::<Zeroizing<String>>(4);
        let task = tokio::spawn(async move { server.serve(prompt_tx, password_rx).await });

        // Connect and say nothing; hold the stream open.
        let stalled = tokio::net::UnixStream::connect(&sock).await.unwrap();

        // Past the read timeout, the loop must have moved on.
        tokio::time::advance(PROMPT_READ_TIMEOUT * 2).await;

        // A well-behaved client now gets served.
        let mut good = tokio::net::UnixStream::connect(&sock).await.unwrap();
        good.write_all(b"Password: \n").await.unwrap();
        let prompt = tokio::time::timeout(Duration::from_secs(5), prompt_rx.recv())
            .await
            .expect("accept loop was wedged by the silent client")
            .expect("channel closed");
        assert_eq!(prompt, "Password:");

        password_tx.send(Zeroizing::new("pw".into())).await.unwrap();
        drop(stalled);
        task.abort();
    }

    #[tokio::test]
    async fn askpass_server_client_roundtrip() {
        let server = AskpassServer::new().unwrap();
        let sock_path = server.env.socket_path.clone();

        let (prompt_tx, mut prompt_rx) = mpsc::channel::<String>(4);
        let (password_tx, password_rx) = mpsc::channel::<Zeroizing<String>>(4);

        let server_task = tokio::spawn(async move {
            server.serve(prompt_tx, password_rx).await;
        });

        let client_task = tokio::task::spawn_blocking({
            let sock_path = sock_path.clone();
            move || {
                let stream = std::os::unix::net::UnixStream::connect(&sock_path).unwrap();
                let mut writer = std::io::BufWriter::new(&stream);
                writeln!(writer, "Enter passphrase for key: ").unwrap();
                writer.flush().unwrap();
                stream
                    .shutdown(std::net::Shutdown::Write)
                    .unwrap();

                let mut reader = std::io::BufReader::new(&stream);
                let mut password = String::new();
                reader.read_line(&mut password).unwrap();
                password.trim_end().to_string()
            }
        });

        let prompt = prompt_rx.recv().await.unwrap();
        assert!(prompt.contains("passphrase"));
        password_tx
            .send(Zeroizing::new("hunter2".into()))
            .await
            .unwrap();

        let got = client_task.await.unwrap();
        assert_eq!(got, "hunter2");

        server_task.abort();
    }
}
