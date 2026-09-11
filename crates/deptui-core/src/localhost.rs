//! "Is this ssh destination actually *this* machine?"
//!
//! Running the TUI on the agent's own host used to be a dead end: the
//! agent client shells out to `ssh <node> deptui-agent …`, and a host
//! almost never has an ssh key authorized to itself — the scan came
//! back `Permission denied (publickey)` and the agent looked absent.
//! Nothing about that call needs a network hop, so the answer is to
//! recognise the destination as local and run the CLI directly.
//!
//! Locality is decided about the *target*, not about what happens to
//! be listening: cheap literals first (`localhost`, loopback, this
//! machine's hostname), then a DNS resolve whose addresses are tested
//! by trying to `bind()` them — an address only binds on the host that
//! owns it, which covers nodes named by IP or by an alias this machine
//! answers to. Decisions are cached per target; a machine's own
//! addresses do not move under a running TUI, and the resolve is on
//! the path of every agent verb.

use std::collections::HashMap;
use std::net::{IpAddr, TcpListener};
use std::sync::{Mutex, OnceLock};

/// The host part of an ssh destination: `user@host`, `host:port`, and
/// bracketed IPv6 (`[::1]:22`) all reduce to the bare host.
pub fn host_part(target: &str) -> &str {
    let after_user = target.rsplit_once('@').map_or(target, |(_, h)| h);
    if let Some(rest) = after_user.strip_prefix('[') {
        // `[::1]` / `[::1]:22` — the brackets exist precisely because
        // the colons inside are not a port separator.
        return rest.split_once(']').map_or(rest, |(h, _)| h);
    }
    match after_user.split_once(':') {
        // A bare IPv6 literal has several colons and no port.
        Some(_) if after_user.matches(':').count() > 1 => after_user,
        Some((h, _)) => h,
        None => after_user,
    }
}

/// This machine's hostname, as the kernel reports it.
pub fn local_hostname() -> Option<&'static str> {
    static HOSTNAME: OnceLock<Option<String>> = OnceLock::new();
    HOSTNAME
        .get_or_init(|| {
            let mut buf = vec![0u8; 256];
            // SAFETY: writing at most `buf.len()` bytes into our own
            // allocation; the result is NUL-terminated on success.
            let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
            if rc != 0 {
                return None;
            }
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            buf.truncate(end);
            String::from_utf8(buf).ok().filter(|s| !s.is_empty())
        })
        .as_deref()
}

/// First DNS label, lowercased: `box.lan` and `box` are the same
/// machine as far as an ssh destination is concerned.
fn short(name: &str) -> String {
    name.split('.').next().unwrap_or(name).to_ascii_lowercase()
}

/// Does this address belong to a local interface? `bind()` is the
/// authority — it fails with `EADDRNOTAVAIL` for any address this host
/// does not own — and it is a purely local syscall: nothing is sent,
/// nothing is accepted, and the ephemeral port is released with the
/// listener.
fn ip_is_local(ip: IpAddr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    TcpListener::bind((ip, 0)).is_ok()
}

fn cache() -> &'static Mutex<HashMap<String, bool>> {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Is `target` (an ssh destination) this machine?
pub async fn is_local_target(target: &str) -> bool {
    if let Some(hit) = cached_verdict(target) {
        return hit;
    }
    let verdict = decide(host_part(target)).await;
    cache()
        .lock()
        .expect("cache mutex")
        .insert(target.to_string(), verdict);
    verdict
}

/// The verdict already reached for `target`, or `None` if nothing has
/// asked yet. For callers that cannot wait on a resolver — the
/// renderer, which runs every frame and must not block on DNS.
pub fn cached_verdict(target: &str) -> Option<bool> {
    cache().lock().expect("cache mutex").get(target).copied()
}

async fn decide(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") || lower == "localhost.localdomain" {
        return true;
    }
    if let Ok(ip) = lower.parse::<IpAddr>() {
        return ip_is_local(ip);
    }
    if let Some(me) = local_hostname() {
        if lower == me.to_ascii_lowercase() || short(host) == short(me) {
            return true;
        }
    }
    // Not a name we recognise on sight: ask the resolver, then let the
    // kernel say whether any answer is ours. Port 0 keeps this a pure
    // name lookup — no connection is attempted.
    match tokio::net::lookup_host((host, 0)).await {
        Ok(addrs) => addrs.into_iter().any(|a| ip_is_local(a.ip())),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_part_strips_user_and_port() {
        assert_eq!(host_part("host"), "host");
        assert_eq!(host_part("me@host"), "host");
        assert_eq!(host_part("me@host:2222"), "host");
        assert_eq!(host_part("host:2222"), "host");
        assert_eq!(host_part("root@10.0.0.5"), "10.0.0.5");
    }

    #[test]
    fn host_part_handles_ipv6() {
        assert_eq!(host_part("::1"), "::1");
        assert_eq!(host_part("[::1]"), "::1");
        assert_eq!(host_part("[fe80::1]:22"), "fe80::1");
        assert_eq!(host_part("me@[::1]:22"), "::1");
    }

    #[tokio::test]
    async fn loopback_names_and_addresses_are_local() {
        assert!(is_local_target("localhost").await);
        assert!(is_local_target("root@localhost").await);
        assert!(is_local_target("127.0.0.1").await);
        assert!(is_local_target("127.0.0.53").await);
        assert!(is_local_target("[::1]:22").await);
    }

    #[tokio::test]
    async fn own_hostname_is_local() {
        let me = local_hostname().expect("a hostname");
        assert!(is_local_target(me).await);
        assert!(is_local_target(&format!("root@{me}")).await);
        // Case and domain suffix are not what makes it a different box.
        assert!(is_local_target(&format!("{}.example.invalid", short(me))).await);
    }

    #[tokio::test]
    async fn foreign_names_and_addresses_are_not_local() {
        // `.invalid` never resolves (RFC 2606), so this exercises the
        // resolver-failure arm without touching the network.
        assert!(!is_local_target("no-such-host.invalid").await);
        // A documentation address nothing can be bound to.
        assert!(!is_local_target("deploy@192.0.2.1").await);
        assert!(!is_local_target("").await);
    }
}
