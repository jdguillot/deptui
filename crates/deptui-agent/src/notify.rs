//! Failure (and opt-in start/success) notifications.
//!
//! Two channels, both fire-and-forget: a hook command the user supplies
//! (`sh -c`, with the event exported as environment variables and
//! substituted into `{placeholders}`), and a webhook POST — ntfy-shaped
//! or a documented generic JSON body. Notification failures are logged,
//! never fatal: the deploy outcome is already decided by the time we
//! get here.

use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;

use crate::config::{NotifyConfig, WebhookKind};

/// One notifiable occurrence. `kind` is part of the public webhook
/// schema: `start`, `success`, `failure`, or `unreachable`.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub kind: String,
    pub watch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub rev: String,
    pub message: String,
    pub time: u64,
}

impl Event {
    pub fn new(kind: &str, watch: &str, host: Option<&str>, rev: &str, message: String) -> Self {
        Self {
            kind: kind.to_string(),
            watch: watch.to_string(),
            host: host.map(str::to_string),
            rev: rev.to_string(),
            message,
            time: crate::state::now_unix(),
        }
    }

    fn title(&self) -> String {
        let host = self.host.as_deref().unwrap_or("-");
        match self.kind.as_str() {
            "failure" => format!("deploy failed: {host} ({})", self.watch),
            "success" => format!("deployed: {host} ({})", self.watch),
            "start" => format!("deploying: {host} ({})", self.watch),
            "unreachable" => format!("unreachable: {host} ({})", self.watch),
            _ => format!("{}: {host} ({})", self.kind, self.watch),
        }
    }
}

/// Dispatch an event to every configured channel. Spawned tasks own
/// their errors; the caller never waits on delivery.
pub fn dispatch(cfg: &NotifyConfig, event: Event) {
    if event.kind == "failure" || event.kind == "unreachable" {
        if let Some(cmd) = &cfg.on_failure {
            spawn_hook(cmd.clone(), event.clone());
        }
    }
    if let Some(url) = &cfg.url {
        if cfg.wants_event(&event.kind) {
            let token = cfg
                .token_file
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|t| t.trim().to_string());
            spawn_webhook(url.clone(), cfg.webhook_kind(), token, event);
        }
    }
}

fn spawn_hook(template: String, event: Event) {
    tokio::spawn(async move {
        let cmd = substitute(&template, &event);
        let result = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .env("DEPTUI_EVENT", &event.kind)
            .env("DEPTUI_WATCH", &event.watch)
            .env("DEPTUI_HOST", event.host.as_deref().unwrap_or(""))
            .env("DEPTUI_REV", &event.rev)
            .env("DEPTUI_MESSAGE", &event.message)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await;
        match result {
            Ok(out) if !out.status.success() => tracing::warn!(
                "on_failure hook exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!("on_failure hook failed to spawn: {e}"),
        }
    });
}

/// Replace `{event}`, `{watch}`, `{host}`, `{rev}`, `{message}` with
/// single-quoted (shell-safe) values.
fn substitute(template: &str, event: &Event) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    template
        .replace("{event}", &quote(&event.kind))
        .replace("{watch}", &quote(&event.watch))
        .replace("{host}", &quote(event.host.as_deref().unwrap_or("")))
        .replace("{rev}", &quote(&event.rev))
        .replace("{message}", &quote(&event.message))
}

fn spawn_webhook(url: String, kind: WebhookKind, token: Option<String>, event: Event) {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("webhook client build failed: {e}");
                return;
            }
        };
        let mut req = match kind {
            WebhookKind::Ntfy => {
                let priority = if event.kind == "failure" {
                    "high"
                } else {
                    "default"
                };
                client
                    .post(&url)
                    .header("Title", event.title())
                    .header("Priority", priority)
                    .body(event.message.clone())
            }
            WebhookKind::Json => client.post(&url).json(&event),
        };
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        match req.send().await {
            Ok(resp) if !resp.status().is_success() => {
                tracing::warn!("webhook to {url} returned {}", resp.status())
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("webhook to {url} failed: {e}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitution_is_shell_quoted() {
        let e = Event::new(
            "failure",
            "infra",
            Some("web"),
            "abc",
            "boom 'quoted'; rm -rf /".into(),
        );
        let cmd = substitute("notify {host} {message}", &e);
        assert_eq!(cmd, r#"notify 'web' 'boom '\''quoted'\''; rm -rf /'"#);
    }

    #[test]
    fn titles_name_host_and_watch() {
        let e = Event::new("failure", "infra", Some("web"), "abc", String::new());
        assert_eq!(e.title(), "deploy failed: web (infra)");
    }
}
