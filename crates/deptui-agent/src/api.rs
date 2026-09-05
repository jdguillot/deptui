//! The control API. Two servers share one router core:
//!
//! - the Unix socket carries the full surface (status, history, log,
//!   tail, kick, pause/resume, deploy) — group-gated by socket mode;
//! - the optional TCP listener exposes *only* `POST /kick` and
//!   `GET /status`, behind a bearer token. Worst case for a leaked
//!   token: an attacker triggers a check of a repo the agent already
//!   trusts.

use std::convert::Infallible;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::config::AgentConfig;
use crate::daemon::{Cmd, Scope};
use crate::wire::{ErrorReply, OkReply};

#[derive(Clone)]
pub struct ApiState {
    pub cmd_tx: mpsc::Sender<Cmd>,
    pub log_tx: broadcast::Sender<String>,
    /// Bearer token required on this listener (TCP only).
    pub token: Option<Arc<String>>,
}

type ApiError = (StatusCode, Json<ErrorReply>);

fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, Json(ErrorReply { error: msg.into() }))
}

fn gone() -> ApiError {
    err(StatusCode::SERVICE_UNAVAILABLE, "daemon is shutting down")
}

/// Send a command and await its reply through the oneshot it carries.
async fn ask<T>(
    tx: &mpsc::Sender<Cmd>,
    make: impl FnOnce(oneshot::Sender<T>) -> Cmd,
) -> Result<T, ApiError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(make(reply_tx)).await.map_err(|_| gone())?;
    reply_rx.await.map_err(|_| gone())
}

fn ok_or_400(result: Result<String, String>) -> Result<Json<OkReply>, ApiError> {
    match result {
        Ok(message) => Ok(Json(OkReply { ok: true, message })),
        Err(e) => Err(err(StatusCode::BAD_REQUEST, e)),
    }
}

#[derive(Deserialize)]
struct WatchParam {
    watch: Option<String>,
}

#[derive(Deserialize)]
struct PauseParams {
    watch: Option<String>,
    host: Option<String>,
}

#[derive(Deserialize)]
struct ApproveParams {
    host: String,
    watch: Option<String>,
    #[serde(default)]
    revoke: bool,
}

#[derive(Deserialize)]
struct LogParams {
    watch: String,
    run: Option<u64>,
}

async fn get_status(State(s): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
    let status = ask(&s.cmd_tx, Cmd::Status).await?;
    Ok(Json(status))
}

async fn get_history(
    State(s): State<ApiState>,
    Query(p): Query<WatchParam>,
) -> Result<impl IntoResponse, ApiError> {
    let result = ask(&s.cmd_tx, |reply| Cmd::History {
        watch: p.watch,
        reply,
    })
    .await?;
    match result {
        Ok(runs) => Ok(Json(runs)),
        Err(e) => Err(err(StatusCode::BAD_REQUEST, e)),
    }
}

async fn get_log(
    State(s): State<ApiState>,
    Query(p): Query<LogParams>,
) -> Result<impl IntoResponse, ApiError> {
    let result = ask(&s.cmd_tx, |reply| Cmd::RunLog {
        watch: p.watch,
        run: p.run,
        reply,
    })
    .await?;
    match result {
        Ok(lines) => Ok(Json(lines)),
        Err(e) => Err(err(StatusCode::NOT_FOUND, e)),
    }
}

async fn get_tail(State(s): State<ApiState>) -> Sse<impl SseStream<'static>> {
    let rx = s.log_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(line) => Some(Ok::<_, Infallible>(SseEvent::default().data(line))),
        // Lagged receivers skip; the tail is best-effort by design.
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// The Sse return type needs a nameable Stream; alias via trait object.
trait SseStream<'a>: tokio_stream::Stream<Item = Result<SseEvent, Infallible>> + Send + 'a {}
impl<'a, T> SseStream<'a> for T where
    T: tokio_stream::Stream<Item = Result<SseEvent, Infallible>> + Send + 'a
{
}

async fn post_kick(
    State(s): State<ApiState>,
    Query(p): Query<WatchParam>,
) -> Result<Json<OkReply>, ApiError> {
    let result = ask(&s.cmd_tx, |reply| Cmd::Kick {
        watch: p.watch,
        reply,
    })
    .await?;
    ok_or_400(result)
}

fn pause_scope(p: PauseParams) -> Result<Scope, ApiError> {
    match (p.watch, p.host) {
        (None, None) => Ok(Scope::Global),
        (Some(w), None) => Ok(Scope::Watch(w)),
        (None, Some(h)) => Ok(Scope::Host(h)),
        (Some(_), Some(_)) => Err(err(
            StatusCode::BAD_REQUEST,
            "pass either watch or host, not both",
        )),
    }
}

async fn post_pause(
    State(s): State<ApiState>,
    Query(p): Query<PauseParams>,
) -> Result<Json<OkReply>, ApiError> {
    let scope = pause_scope(p)?;
    let result = ask(&s.cmd_tx, |reply| Cmd::SetPaused {
        scope,
        paused: true,
        reply,
    })
    .await?;
    ok_or_400(result)
}

async fn post_resume(
    State(s): State<ApiState>,
    Query(p): Query<PauseParams>,
) -> Result<Json<OkReply>, ApiError> {
    let scope = pause_scope(p)?;
    let result = ask(&s.cmd_tx, |reply| Cmd::SetPaused {
        scope,
        paused: false,
        reply,
    })
    .await?;
    ok_or_400(result)
}

async fn post_cancel(State(s): State<ApiState>) -> Result<Json<OkReply>, ApiError> {
    let result = ask(&s.cmd_tx, |reply| Cmd::CancelRun { reply }).await?;
    ok_or_400(result)
}

async fn post_approve(
    State(s): State<ApiState>,
    Query(p): Query<ApproveParams>,
) -> Result<Json<OkReply>, ApiError> {
    let result = ask(&s.cmd_tx, |reply| Cmd::Approve {
        watch: p.watch,
        host: p.host,
        revoke: p.revoke,
        reply,
    })
    .await?;
    ok_or_400(result)
}

/// Bearer-token check for the TCP listener.
async fn require_token(
    State(s): State<ApiState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Some(expected) = &s.token else {
        return next.run(req).await;
    };
    let authed = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()))
        .unwrap_or(false);
    if authed {
        next.run(req).await
    } else {
        err(StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The full control surface, served on the Unix socket.
fn full_router(state: ApiState) -> Router {
    Router::new()
        .route("/status", get(get_status))
        .route("/history", get(get_history))
        .route("/log", get(get_log))
        .route("/log/tail", get(get_tail))
        .route("/kick", post(post_kick))
        .route("/pause", post(post_pause))
        .route("/resume", post(post_resume))
        .route("/approve", post(post_approve))
        // Cancel is a control verb: Unix socket only, never on the TCP
        // kick router.
        .route("/cancel", post(post_cancel))
        .with_state(state)
}

/// Kick + status only, for the TCP listener.
fn kick_router(state: ApiState) -> Router {
    Router::new()
        .route("/status", get(get_status))
        .route("/kick", post(post_kick))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_token,
        ))
        .with_state(state)
}

/// Bind the Unix socket (replacing a stale one) and serve until the
/// process exits. Socket mode 0660: the group is the access-control
/// list.
pub async fn serve_unix(cfg: &AgentConfig, state: ApiState) -> Result<()> {
    let path = &cfg.socket;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket dir {}", parent.display()))?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("removing stale socket {}", path.display()))
        }
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
        .with_context(|| format!("chmod 0660 {}", path.display()))?;
    tracing::info!("control API listening on {}", path.display());
    axum::serve(listener, full_router(state))
        .await
        .context("serving the Unix control API")
}

/// Bind the optional TCP kick listener.
pub async fn serve_tcp(addr: &str, token: String, state: ApiState) -> Result<()> {
    let state = ApiState {
        token: Some(Arc::new(token)),
        ..state
    };
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding TCP listener on {addr}"))?;
    tracing::info!("kick/status API listening on {addr} (token-gated)");
    axum::serve(listener, kick_router(state))
        .await
        .context("serving the TCP kick API")
}
