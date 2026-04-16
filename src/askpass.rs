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

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

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
            "#!/bin/sh\nexec '{}' --askpass \"$@\"\n",
            exe.display()
        );
        std::fs::write(&script_path, &script)?;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))?;

        let listener =
            UnixListener::bind(&socket_path).context("binding askpass Unix socket")?;

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
        mut password_rx: mpsc::Receiver<String>,
    ) {
        loop {
            let (stream, _) = match self.listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };

            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut prompt = String::new();
            if reader.read_line(&mut prompt).await.is_err() {
                continue;
            }
            let prompt = prompt.trim_end().to_string();
            if prompt.is_empty() {
                continue;
            }

            // Forward the prompt to the TUI.
            if prompt_tx.send(prompt).await.is_err() {
                break;
            }

            // Wait for the password from the TUI.
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

    #[tokio::test]
    async fn askpass_server_client_roundtrip() {
        let server = AskpassServer::new().unwrap();
        let sock_path = server.env.socket_path.clone();

        let (prompt_tx, mut prompt_rx) = mpsc::channel::<String>(4);
        let (password_tx, password_rx) = mpsc::channel::<String>(4);

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
        password_tx.send("hunter2".into()).await.unwrap();

        let got = client_task.await.unwrap();
        assert_eq!(got, "hunter2");

        server_task.abort();
    }
}
