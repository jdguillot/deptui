//! Minimal HTTP/1.1 client over the agent's Unix socket, for the CLI
//! verbs (`status`, `kick`, …). Hand-rolled on purpose: the requests
//! are tiny, `Connection: close` bounds every body, and this is also
//! the transport the TUI reaches over `ssh <host> deptui-agent <verb>
//! --json` — no TLS, no connection pooling, nothing to configure.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// One request/response exchange. `path` includes the query string.
pub async fn request(socket: &Path, method: &str, path: &str) -> Result<Response> {
    let stream = UnixStream::connect(socket).await.with_context(|| {
        format!(
            "connecting to the agent socket {} (is deptui-agent running?)",
            socket.display()
        )
    })?;
    let (read_half, mut write_half) = stream.into_split();
    let req =
        format!("{method} {path} HTTP/1.1\r\nHost: deptui-agent\r\nConnection: close\r\n\r\n");
    write_half.write_all(req.as_bytes()).await?;
    // Do NOT half-close here: hyper's default http1 config treats a
    // client FIN as connection teardown and never sends the response.
    // `Connection: close` alone bounds the exchange.

    let mut reader = BufReader::new(read_half);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).await?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status line: {status_line:?}"))?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "content-length" {
                content_length = value.parse().ok();
            } else if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
        }
    }

    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size_line = String::new();
            reader.read_line(&mut size_line).await?;
            let size = usize::from_str_radix(size_line.trim(), 16)
                .map_err(|_| anyhow!("malformed chunk size: {size_line:?}"))?;
            if size == 0 {
                break;
            }
            let mut chunk = vec![0u8; size + 2]; // + CRLF
            reader.read_exact(&mut chunk).await?;
            chunk.truncate(size);
            body.extend_from_slice(&chunk);
        }
    } else if let Some(len) = content_length {
        body.resize(len, 0);
        reader.read_exact(&mut body).await?;
    } else {
        // Connection: close — read to EOF.
        reader.read_to_end(&mut body).await?;
    }
    Ok(Response { status, body })
}

/// GET returning parsed JSON; non-2xx surfaces the server's error body.
pub async fn get_json<T: serde::de::DeserializeOwned>(socket: &Path, path: &str) -> Result<T> {
    let resp = request(socket, "GET", path).await?;
    parse_json(resp)
}

/// POST returning parsed JSON.
pub async fn post_json<T: serde::de::DeserializeOwned>(socket: &Path, path: &str) -> Result<T> {
    let resp = request(socket, "POST", path).await?;
    parse_json(resp)
}

fn parse_json<T: serde::de::DeserializeOwned>(resp: Response) -> Result<T> {
    if !(200..300).contains(&resp.status) {
        // The server sends {"error": "..."} bodies; show them plainly.
        if let Ok(err) = serde_json::from_slice::<crate::wire::ErrorReply>(&resp.body) {
            bail!("{}", err.error);
        }
        bail!(
            "agent returned HTTP {}: {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        );
    }
    serde_json::from_slice(&resp.body).context("parsing the agent's JSON reply")
}

/// Stream `GET /log/tail` (SSE), invoking `on_line` per data line until
/// the connection drops.
pub async fn tail(socket: &Path, mut on_line: impl FnMut(&str)) -> Result<()> {
    let stream = UnixStream::connect(socket).await.with_context(|| {
        format!(
            "connecting to the agent socket {} (is deptui-agent running?)",
            socket.display()
        )
    })?;
    let (read_half, mut write_half) = stream.into_split();
    let req = "GET /log/tail HTTP/1.1\r\nHost: deptui-agent\r\nAccept: text/event-stream\r\n\r\n";
    write_half.write_all(req.as_bytes()).await?;

    let mut reader = BufReader::new(read_half);
    // Skip the status line + headers.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            bail!("agent closed the tail stream before headers completed");
        }
        if line.trim_end().is_empty() {
            break;
        }
    }
    // SSE frames; chunked framing lines are hex sizes / CRLFs that never
    // start with "data:", so filtering on the prefix is sufficient here.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if let Some(data) = line.trim_end().strip_prefix("data:") {
            on_line(data.trim_start());
        }
    }
}
