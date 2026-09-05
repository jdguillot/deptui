//! Host status checks.
//!
//! - **Online**: TCP connect to port 22 with a short timeout (no ICMP, no
//!   raw sockets, no sudo).
//! - **Update**: optional / on-demand. Builds the system profile locally
//!   (`nix path-info`-style: we ask `nix eval --raw` for the out path of
//!   the activation derivation) and compares it to the remote machine's
//!   `/run/current-system` symlink read over SSH.
//!
//! Both checks are designed to be cheap to call from the TUI's async event
//! loop.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::askpass::AskpassEnv;
use crate::flake::Node;
use crate::ssh::SshOverride;

/// What we currently know about a host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Reachability {
    #[default]
    Unknown,
    Online,
    Offline,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UpdateState {
    #[default]
    Unknown,
    UpToDate,
    NeedsUpdate,
    /// The profile symlink is absent — the host has never had this profile
    /// deployed to it. Shown as a distinct badge rather than an error so
    /// the user can tell "never deployed" from "probe failed".
    NotDeployed,
    /// We tried to check but the comparison failed (host unreachable, eval
    /// error, etc.). The string is rendered in the details pane.
    Error,
}

/// Typed classification of one job-log line. Set by the *producer* at
/// the moment the line is created, so the renderer styles from data
/// instead of re-parsing prose — the wording of the text is one
/// module's business, not an interface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LogKind {
    /// Ordinary output: deploy stdout/stderr, action banners.
    #[default]
    Plain,
    /// Dim stage note from a probe ("[pkg] listing local closure …").
    Note,
    /// "[size] local: N bytes"
    SizeLocal(u64),
    /// "[size] remote: N bytes" — pairs with the preceding `SizeLocal`
    /// for the same host so the renderer can show a delta.
    SizeRemote(u64),
    /// One package-diff line.
    Pkg(PkgChange),
    /// "[pkg] done (N change(s))"
    PkgDone(usize),
}

/// One entry of a package diff between two closures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkgChange {
    /// The version set of a package differs between the two sides.
    Updated {
        name: String,
        from: String,
        to: String,
    },
    /// Present in the local closure only.
    Added {
        name: String,
        versions: String,
    },
    /// Present in the remote closure only.
    Removed {
        name: String,
        versions: String,
    },
    /// Head line of the content-only case: every name+version matches
    /// but `count` store paths still differ (config-file rebuilds).
    ContentOnly {
        count: usize,
    },
    /// Sample basename beneath the content-only head.
    SampleAdded {
        base: String,
    },
    SampleRemoved {
        base: String,
    },
    /// "… and N more path(s)" cap line under the samples.
    More {
        count: usize,
    },
}

impl PkgChange {
    /// Canonical one-line rendering, shared by the job log and any
    /// textual summary. The renderer styles from the variant; this is
    /// what gets searched and yanked.
    pub fn render(&self) -> String {
        match self {
            PkgChange::Updated { name, from, to } => format!("{name}: {from} → {to}"),
            PkgChange::Added { name, versions } => format!("{name}: + {versions}"),
            PkgChange::Removed { name, versions } => format!("{name}: - {versions}"),
            PkgChange::ContentOnly { count } => format!(
                "(content-only) {count} path(s) differ — same package versions, different contents"
            ),
            PkgChange::SampleAdded { base } => format!("  + {base}"),
            PkgChange::SampleRemoved { base } => format!("  - {base}"),
            PkgChange::More { count } => format!("  … and {count} more path(s)"),
        }
    }
}

/// The full result of a package diff, stored on [`ProfileExtra`]. The
/// details pane branches on the typed shape — no prefix sniffing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PkgDiff {
    pub changes: Vec<PkgChange>,
}

impl PkgDiff {
    /// No differences at all — the closures are identical.
    pub fn is_identical(&self) -> bool {
        self.changes.is_empty()
    }

    /// True when nothing changed by name+version but store paths still
    /// differ. The UI renders a distinct badge for this — a change
    /// count would contradict the path lines in the job log.
    pub fn is_content_only(&self) -> bool {
        matches!(self.changes.first(), Some(PkgChange::ContentOnly { .. }))
    }

    /// Number of version-level changes (content-only detail lines do
    /// not count).
    pub fn change_count(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    PkgChange::Updated { .. } | PkgChange::Added { .. } | PkgChange::Removed { .. }
                )
            })
            .count()
    }
}

/// One line of typed probe progress. `text` is the canonical rendering
/// (searched, yanked, shown); `kind` carries the data the renderer
/// styles from. The constructors derive both from the same values, so
/// the two can never disagree.
#[derive(Debug, Clone)]
pub struct ProgressLine {
    pub text: String,
    pub kind: LogKind,
}

impl ProgressLine {
    pub fn note(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: LogKind::Note,
        }
    }

    pub fn size_local(bytes: u64) -> Self {
        Self {
            text: format!("[size] local: {bytes} bytes"),
            kind: LogKind::SizeLocal(bytes),
        }
    }

    pub fn size_remote(bytes: u64) -> Self {
        Self {
            text: format!("[size] remote: {bytes} bytes"),
            kind: LogKind::SizeRemote(bytes),
        }
    }

    pub fn pkg(change: PkgChange) -> Self {
        Self {
            text: format!("[pkg] {}", change.render()),
            kind: LogKind::Pkg(change),
        }
    }

    pub fn pkg_done(count: usize) -> Self {
        Self {
            text: format!("[pkg] done ({count} change(s))"),
            kind: LogKind::PkgDone(count),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct HostStatus {
    pub reachability: Reachability,
    /// True while the reachability TCP probe is in flight. Lets the UI
    /// show the same spinner as the update-check probes.
    pub checking_reachability: bool,
    /// Wall-clock time of the most recent successful reachability probe.
    /// Rendered in the details pane as an "up X ago" hint so the user
    /// can tell at a glance how fresh the online indicator actually is.
    pub last_online: Option<SystemTime>,
    pub last_error: Option<String>,
    /// Per-profile state, keyed by profile name. One shape for every
    /// profile a node declares — a profile beyond `system`/`home`
    /// participates in every check the moment the flake declares it.
    pub profiles: BTreeMap<String, ProfileStatus>,
    /// Result of the last substituter-drift check (`Shift+C`). `None`
    /// means the check hasn't been run for this host, which is different
    /// from "ran and found no drift" — the UI has to say which.
    pub cache_drift: Option<SubstituterDrift>,
    /// True while a substituter-drift check is in flight.
    pub checking_cache: bool,
    /// True while a build-plan preflight is in flight.
    pub checking_plan: bool,
}

impl HostStatus {
    /// A profile's state, defaulting to "never probed" for profiles
    /// that have no entry yet.
    pub fn profile(&self, name: &str) -> &ProfileStatus {
        static EMPTY: std::sync::OnceLock<ProfileStatus> = std::sync::OnceLock::new();
        self.profiles
            .get(name)
            .unwrap_or_else(|| EMPTY.get_or_init(ProfileStatus::default))
    }

    /// Mutable access to a profile's state, creating the entry on
    /// first touch.
    pub fn profile_mut(&mut self, name: &str) -> &mut ProfileStatus {
        self.profiles.entry(name.to_string()).or_default()
    }

    /// The build plans that have actually run, keyed by profile.
    pub fn build_plans(&self) -> impl Iterator<Item = (&String, &BuildPlan)> {
        self.profiles
            .iter()
            .filter_map(|(name, p)| p.build_plan.as_ref().map(|plan| (name, plan)))
    }
}

/// Everything we know about one profile of one host. One shape for
/// every profile — `system`, `home`, and anything else a flake
/// declares — so no code path can silently skip an unfamiliar name.
#[derive(Debug, Clone, Default)]
pub struct ProfileStatus {
    pub update: UpdateState,
    /// True while an update probe for this profile is in flight. The
    /// previous value of `update` is kept around so the badge can show
    /// "previous result + spinner".
    pub checking: bool,
    /// Extra information harvested during update checks (paths,
    /// activation time, closure size, package diff). Populated lazily —
    /// `u` fills in the cheap tier, `Shift+U` fills in the rest.
    pub extra: ProfileExtra,
    /// Result of the last build-plan preflight (`Shift+P`). `None`
    /// means "never run", which the UI must not render as "nothing to
    /// build".
    pub build_plan: Option<BuildPlan>,
}

/// Rich result of an update probe — always includes the store paths
/// and (when we can stat it) the remote activation time. These fields
/// come "for free" because we already ran the readlink over SSH, so
/// we surface them in the details pane whenever `u` is pressed.
#[derive(Debug, Clone)]
pub struct ProfileCheck {
    pub up_to_date: bool,
    /// True when the remote profile symlink is absent — the host has never
    /// had this profile deployed. Implies `up_to_date = false` and an
    /// empty `remote_path`. Callers should surface this as
    /// `UpdateState::NotDeployed` rather than `NeedsUpdate`.
    pub not_deployed: bool,
    pub local_path: String,
    pub remote_path: String,
    pub activation_time: Option<SystemTime>,
}

/// Extra details about a profile that the user can populate via the
/// update-check keys. `u` fills in the cheap tier (paths + activation
/// time); `U` fills in closure sizes; `p` fills in the full package
/// diff. Every field is optional so the UI can render whatever is
/// currently known without branching on tiers.
#[derive(Debug, Clone, Default)]
pub struct ProfileExtra {
    pub local_path: Option<String>,
    pub remote_path: Option<String>,
    pub activation_time: Option<SystemTime>,
    /// Closure size in bytes as reported by `nix path-info --closure-size`.
    pub local_size: Option<u64>,
    pub remote_size: Option<u64>,
    pub checking_size: bool,
    /// Typed result of the package diff. `None` means the expensive
    /// tier hasn't run; `Some` with no changes means the closures are
    /// identical — the UI renders those differently.
    pub pkg_diff: Option<PkgDiff>,
    pub checking_pkg: bool,
}

/// TCP-connect to the host's effective SSH endpoint.
///
/// Resolution order:
///   1. If the per-host override sets an explicit `hostname`, trust it
///      (the user was deliberate). Port still comes from `ssh -G`.
///   2. Otherwise run `ssh -G <hostname> [override args…]` to resolve
///      whatever `~/.ssh/config` says — this is what `ssh` would actually
///      use, so the "online" badge matches the user's real SSH setup.
///
/// Falls back to `<hostname>:22` if `ssh -G` fails for any reason.
pub async fn check_online(hostname: &str, override_: &SshOverride) -> Reachability {
    let (host, port) = resolve_ssh_endpoint(hostname, override_)
        .await
        .unwrap_or_else(|| (override_.effective_host(hostname).to_string(), 22));
    let target = format!("{host}:{port}");
    match timeout(Duration::from_secs(2), TcpStream::connect(&target)).await {
        Ok(Ok(_)) => Reachability::Online,
        _ => Reachability::Offline,
    }
}

/// Ask `ssh -G` to resolve a host the way `ssh` would: alias lookups,
/// `HostName` substitution, `Port`, all of it. Returns `None` if ssh
/// isn't on PATH, the config can't be parsed, or the relevant lines are
/// missing from the output.
async fn resolve_ssh_endpoint(hostname: &str, override_: &SshOverride) -> Option<(String, u16)> {
    let effective = override_.effective_host(hostname).to_string();
    let mut cmd = Command::new("ssh");
    cmd.arg("-G");
    // Per-host override args feed the same resolution as a real
    // connection would, so `-o Port=2222` in override opts lands in
    // the output without us having to parse `extra_opts`.
    for arg in override_.ssh_args() {
        cmd.arg(arg);
    }
    cmd.arg(&effective);
    // Null stdin so the child can never consume the TUI's keystrokes,
    // and `kill_on_drop` so the timeout below actually reaps it instead
    // of detaching it. Every other spawn in this file already does both.
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    let output = timeout(Duration::from_secs(2), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut resolved_host = effective.clone();
    let mut resolved_port: u16 = 22;
    for line in text.lines() {
        let mut parts = line.splitn(2, ' ');
        let key = parts.next().unwrap_or("");
        let val = parts.next().unwrap_or("").trim();
        match key {
            "hostname" => {
                if !val.is_empty() {
                    resolved_host = val.to_string();
                }
            }
            "port" => {
                if let Ok(p) = val.parse::<u16>() {
                    resolved_port = p;
                }
            }
            _ => {}
        }
    }
    // If the override explicitly set a hostname, trust it over whatever
    // ssh_config resolved (the user's override is the most-recent
    // intent and might be a one-shot IP). The resolved port still
    // applies.
    if let Some(explicit) = override_.hostname.as_deref() {
        resolved_host = explicit.to_string();
    }
    Some((resolved_host, resolved_port))
}

/// Compare the locally-evaluated profile out-path against the remote
/// `/run/current-system` (for `system`) or the user's `current-home`
/// (for `home`) symlink target.
///
/// `override_` is the per-host SSH override (may be empty/default), and
/// it's used both to redirect the SSH connection and to inject extra
/// `-i`/`-o` arguments.
///
/// Returns the full [`ProfileCheck`] so callers can surface the resolved
/// paths and activation time in the UI — they're essentially free
/// byproducts of the readlink we'd be running anyway.
pub async fn check_profile_up_to_date(
    flake: &str,
    node: &Node,
    profile: &str,
    override_: &SshOverride,
    askpass: &AskpassEnv,
) -> Result<ProfileCheck> {
    let local = local_profile_path(flake, &node.name, profile)
        .await
        .with_context(|| format!("evaluating local path for {}.{profile}", node.name))?;

    // Combined readlink + stat so we only pay one SSH round-trip. We
    // stat the *symlink itself*, not the resolved store path, because
    // Nix freezes store-path mtimes to 1 (epoch+1s) for reproducible
    // builds — staring the resolved path would always return "56
    // years ago". The symlink's mtime is the activation time.
    let remote_cmd = match profile {
        "system" => "readlink -f /run/current-system && stat -c %Y /run/current-system".to_string(),
        "home" => {
            r#"if [ -L ~/.local/state/nix/profiles/home-manager ]; then link=~/.local/state/nix/profiles/home-manager; elif [ -L ~/.nix-profile ]; then link=~/.nix-profile; else printf 'NOT_DEPLOYED\n'; exit 0; fi; readlink -f "$link" && stat -c %Y "$link""#.to_string()
        }
        other => return Err(anyhow!("unknown profile `{other}`")),
    };

    let target = build_ssh_target(node, profile, override_);
    let remote = ssh_capture(&target, &remote_cmd, override_, askpass).await?;

    let local_trimmed = local.trim().to_string();

    // First line is the resolved store path; second line is the mtime
    // (seconds since epoch) of the symlink on the remote. Missing
    // second line just means we couldn't stat — not fatal.
    //
    // Special case: the home-profile script emits "NOT_DEPLOYED" when
    // neither the modern nor the legacy home-manager symlink exists.
    // Return a distinct result so the caller can surface it as
    // UpdateState::NotDeployed rather than treating it as NeedsUpdate.
    let mut lines = remote.lines();
    let first_line = lines.next().unwrap_or("").trim();
    if first_line == "NOT_DEPLOYED" {
        return Ok(ProfileCheck {
            up_to_date: false,
            not_deployed: true,
            local_path: local_trimmed,
            remote_path: String::new(),
            activation_time: None,
        });
    }
    let remote_path = first_line.to_string();
    // Defensive: drop suspiciously small values. Anything before
    // 2010 (mtime < 1262304000) is almost certainly a Nix-frozen
    // mtime and not a real activation time, so we hide it rather
    // than render "56 years ago".
    let activation_time = lines
        .next()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 1_262_304_000)
        .map(|secs| std::time::UNIX_EPOCH + Duration::from_secs(secs));

    // `local_trimmed` is the deploy-rs activation *wrapper* path — its
    // store hash is distinct from the toplevel that
    // `/run/current-system` resolves to, so a naive string compare
    // against `remote_path` would *always* say "needs update" even
    // when the host is perfectly current. Resolve the wrapper to the
    // toplevel it encloses and compare that instead. When the wrapper
    // isn't in the local store (or the resolve otherwise fails) fall
    // back to a parsed name+version equality check so we don't
    // regress into the old false-positive behaviour.
    let resolved_local = resolve_local_toplevel_quiet(&local_trimmed, &remote_path).await;
    let up_to_date = match resolved_local.as_deref() {
        Some(toplevel) => toplevel == remote_path,
        None => parsed_paths_equivalent(&local_trimmed, &remote_path),
    };
    // Prefer reporting the resolved toplevel to downstream UI/probes:
    // both the closure-size and package-diff tiers want the toplevel,
    // not the wrapper. If we couldn't resolve, keep the wrapper so
    // the expensive tiers can still run their own resolution.
    let reported_local = resolved_local.unwrap_or(local_trimmed);

    Ok(ProfileCheck {
        up_to_date,
        not_deployed: false,
        local_path: reported_local,
        remote_path,
        activation_time,
    })
}

/// Heuristic fallback for when we can't resolve the local wrapper to
/// its toplevel (wrapper isn't in the local store yet, references
/// unavailable, etc.). Parses `<hash>-<name>-<version>` from each
/// basename and compares the pair after peeling any deploy-rs
/// activation suffixes from the local side. Not as strict as a path
/// equality check — two builds of the same package with different
/// inputs can share a name+version but have different hashes — but
/// matches the user's mental model of "same nixos-generation" and is
/// strictly better than the raw-path compare it replaces.
fn parsed_paths_equivalent(local_path: &str, remote_path: &str) -> bool {
    let local_base = local_path.rsplit('/').next().unwrap_or(local_path);
    let remote_base = remote_path.rsplit('/').next().unwrap_or(remote_path);
    let (mut local_name, local_ver) = split_name_version(local_base);
    let (remote_name, remote_ver) = split_name_version(remote_base);
    for suffix in ["-activate-path", "-activate-rs"] {
        if let Some(stripped) = local_name.strip_suffix(suffix) {
            local_name = stripped.to_string();
            break;
        }
    }
    !local_name.is_empty()
        && !local_ver.is_empty()
        && local_name == remote_name
        && local_ver == remote_ver
}

/// Progress-free twin of [`resolve_local_toplevel`]. The cheap-tier
/// update check (`u`) runs on every profile of every targeted host,
/// so it can't afford to thread an `mpsc::Sender` through; this
/// version silently returns `None` on any failure (wrapper not in
/// store, references unavailable, no match) and lets the caller pick
/// a fallback.
async fn resolve_local_toplevel_quiet(wrapper_path: &str, remote_path: &str) -> Option<String> {
    let remote_base = remote_path.rsplit('/').next().unwrap_or(remote_path);
    let (remote_name, _) = split_name_version(remote_base);
    if remote_name.is_empty() {
        return None;
    }
    if !std::path::Path::new(wrapper_path).exists() {
        return None;
    }
    let out = Command::new("nix-store")
        .args(["--query", "--references", wrapper_path])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let p = line.trim();
        if p.is_empty() {
            continue;
        }
        let base = p.rsplit('/').next().unwrap_or(p);
        let (name, _) = split_name_version(base);
        if name == remote_name {
            return Some(p.to_string());
        }
    }
    None
}

/// Medium-tier check: closure size delta.
///
/// Runs `nix path-info --closure-size` locally against the `local_path`
/// and again over SSH against `remote_path`, returning `(local_bytes,
/// remote_bytes)`. Both calls fail fast on non-zero exit so a missing
/// `nix` on the remote doesn't silently produce a bogus "0 B" delta.
///
/// If the local closure isn't in the store yet, we shell out to
/// `nix build` against the flake attribute to force evaluation +
/// build first (see `ensure_local_closure`). `flake` is the flake
/// reference — same value as the one passed to `check_profile_up_to_date`.
///
/// `progress` receives one human-readable line per stage — mostly so
/// the build step (which can take tens of seconds for a fresh NixOS
/// system that hasn't been built locally yet) isn't invisible to the
/// user. The channel is best-effort: a closed receiver is ignored.
/// The inputs shared by every profile-level probe.
///
/// `check_closure_sizes` and `check_package_diff` previously took the
/// same eight positional arguments in the same order — including two
/// adjacent `&str` paths, `local_path` and `remote_path`, which a call
/// site could transpose with no complaint from the compiler and a
/// thoroughly confusing result. Named fields make that mistake visible.
#[derive(Debug, Clone, Copy)]
pub struct ProfileProbe<'a> {
    /// Flake reference the profile is evaluated from.
    pub flake: &'a str,
    pub node: &'a Node,
    /// Profile name (`system`, `home`, …).
    pub profile: &'a str,
    /// Store path of the locally-built deploy-rs activation wrapper.
    pub local_path: &'a str,
    /// Store path the remote profile symlink currently resolves to.
    pub remote_path: &'a str,
    pub override_: &'a SshOverride,
    pub askpass: &'a AskpassEnv,
}

pub async fn check_closure_sizes(
    probe: &ProfileProbe<'_>,
    progress: mpsc::Sender<ProgressLine>,
) -> Result<(u64, u64)> {
    let &ProfileProbe {
        flake,
        node,
        profile,
        local_path,
        remote_path,
        override_,
        askpass,
    } = probe;
    // Step 1: make sure the deploy-rs activation wrapper is in the
    // local store. Nothing to do if it already is.
    ensure_local_closure(flake, &node.name, profile, local_path, &progress, "size").await?;
    // Step 2: resolve the wrapper down to the actual toplevel so we
    // compare apples-to-apples with the remote's /run/current-system
    // target. See `resolve_local_toplevel` for the rationale.
    let resolved_local = resolve_local_toplevel(local_path, remote_path, &progress, "size").await?;
    let _ = progress
        .send(ProgressLine::note("[size] measuring local closure …"))
        .await;
    let local_size = nix_closure_size(&resolved_local)
        .await
        .context("local `nix path-info --closure-size`")?;
    let _ = progress.send(ProgressLine::size_local(local_size)).await;
    let target = build_ssh_target(node, profile, override_);
    // Shell-quote the path defensively even though nix store paths are
    // ascii — if the user ever points at something weird we don't want
    // to explode the remote command.
    let _ = progress
        .send(ProgressLine::note(format!(
            "[size] measuring remote closure on {target} …"
        )))
        .await;
    let remote_cmd = format!("nix path-info --closure-size '{remote_path}'");
    let remote = ssh_capture(&target, &remote_cmd, override_, askpass)
        .await
        .context("remote `nix path-info --closure-size`")?;
    let remote_size = parse_closure_size(&remote)
        .ok_or_else(|| anyhow!("unparseable remote closure size: `{}`", remote.trim()))?;
    let _ = progress.send(ProgressLine::size_remote(remote_size)).await;
    Ok((local_size, remote_size))
}

/// Expensive-tier check: name+version diff between the local and the
/// remote closure.
///
/// We deliberately avoid `nix store diff-closures` here. The previous
/// implementation paid two heavy costs to use it: (1) `nix copy
/// --from ssh-ng://target <remote>` had to pull the *entire* closure
/// (every store path's actual contents) over the network into the
/// local store before the diff could run, and (2) the diff itself
/// then re-walks both closures. For a typical NixOS system that's
/// gigabytes of network transfer just to learn that openssl bumped
/// from 3.5.1 to 3.5.2.
///
/// Instead we do a metadata-only diff:
///
///   1. `nix-store --query --requisites <local_path>` locally — lists
///      every store path in the local closure (no I/O on the contents).
///   2. The same command on the remote over SSH — one round-trip,
///      typically a few hundred KB of text.
///   3. Parse `<hash>-<name>-<version>` from each store-path basename,
///      bucket by package name, and emit one line per name whose
///      version set differs (added / removed / updated).
///
/// This is dramatically faster (seconds vs minutes), at the cost of
/// not showing per-path closure-size deltas. The user explicitly
/// asked for "version change of each package" — that's exactly what
/// this surfaces.
///
/// `progress` receives one human-readable line per stage so the user
/// can see activity instead of staring at a silent spinner. The
/// channel is best-effort: a closed receiver is ignored.
pub async fn check_package_diff(
    probe: &ProfileProbe<'_>,
    progress: mpsc::Sender<ProgressLine>,
) -> Result<PkgDiff> {
    let &ProfileProbe {
        flake,
        node,
        profile,
        local_path,
        remote_path,
        override_,
        askpass,
    } = probe;
    let target = build_ssh_target(node, profile, override_);

    // Step 1: make sure the wrapper is built, then resolve the real
    // toplevel inside it — same two-step as check_closure_sizes.
    ensure_local_closure(flake, &node.name, profile, local_path, &progress, "pkg").await?;
    let resolved_local = resolve_local_toplevel(local_path, remote_path, &progress, "pkg").await?;

    // Stage 1: list the local closure. This is a pure metadata query
    // against the local store and is essentially instantaneous.
    let _ = progress
        .send(ProgressLine::note("[pkg] listing local closure …"))
        .await;
    let local_paths = nix_requisites(&resolved_local)
        .await
        .with_context(|| format!("local `nix-store --query --requisites {resolved_local}`"))?;
    let _ = progress
        .send(ProgressLine::note(format!(
            "[pkg] local closure: {} paths",
            local_paths.len()
        )))
        .await;

    // Stage 2: list the remote closure over SSH. One short ssh
    // round-trip; the response is a flat newline-separated list of
    // store paths. No actual store contents move across the wire.
    let _ = progress
        .send(ProgressLine::note(format!(
            "[pkg] listing remote closure on {target} …"
        )))
        .await;
    let remote_cmd = format!("nix-store --query --requisites '{remote_path}'");
    let remote_out = ssh_capture(&target, &remote_cmd, override_, askpass)
        .await
        .with_context(|| format!("remote `nix-store --query --requisites {remote_path}`"))?;
    let remote_paths: Vec<String> = remote_out
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let _ = progress
        .send(ProgressLine::note(format!(
            "[pkg] remote closure: {} paths",
            remote_paths.len()
        )))
        .await;

    let _ = progress
        .send(ProgressLine::note("[pkg] computing version diff …"))
        .await;
    let changes = compute_version_diff(&local_paths, &remote_paths);
    for change in &changes {
        let _ = progress.send(ProgressLine::pkg(change.clone())).await;
    }
    let _ = progress.send(ProgressLine::pkg_done(changes.len())).await;
    Ok(PkgDiff { changes })
}

/// Pure-logic version + content diff between two closure path lists.
///
/// Buckets each side by parsed `<name, version>`, walks the union, and
/// emits one [`PkgChange`] per name whose version set differs. When no
/// version changes exist but the store-path sets still diverge
/// (config-file rebuilds), emits a `ContentOnly` head followed by
/// sample basenames.
fn compute_version_diff(local_paths: &[String], remote_paths: &[String]) -> Vec<PkgChange> {
    let local_by_name = bucket_paths_by_name(local_paths);
    let remote_by_name = bucket_paths_by_name(remote_paths);

    let mut all_names: BTreeSet<&str> = BTreeSet::new();
    for k in local_by_name.keys() {
        all_names.insert(k.as_str());
    }
    for k in remote_by_name.keys() {
        all_names.insert(k.as_str());
    }

    let mut changes = Vec::<PkgChange>::new();
    for name in &all_names {
        let l = local_by_name.get(*name);
        let r = remote_by_name.get(*name);
        let change = match (l, r) {
            (Some(lv), Some(rv)) if lv == rv => continue,
            (Some(lv), Some(rv)) => PkgChange::Updated {
                name: name.to_string(),
                from: join_versions(rv),
                to: join_versions(lv),
            },
            (Some(lv), None) => PkgChange::Added {
                name: name.to_string(),
                versions: join_versions(lv),
            },
            (None, Some(rv)) => PkgChange::Removed {
                name: name.to_string(),
                versions: join_versions(rv),
            },
            (None, None) => continue,
        };
        changes.push(change);
    }

    // Content-only diff: every package name+version matches but the
    // actual store-path sets still differ (config-file rebuilds).
    if changes.is_empty() {
        let local_set: BTreeSet<&str> = local_paths.iter().map(|s| s.as_str()).collect();
        let remote_set: BTreeSet<&str> = remote_paths.iter().map(|s| s.as_str()).collect();
        let only_local: Vec<&str> = local_set.difference(&remote_set).copied().collect();
        let only_remote: Vec<&str> = remote_set.difference(&local_set).copied().collect();
        if !only_local.is_empty() || !only_remote.is_empty() {
            changes.push(PkgChange::ContentOnly {
                count: only_local.len().max(only_remote.len()),
            });
            for p in only_local.iter().take(8) {
                let base = p.rsplit('/').next().unwrap_or(p);
                changes.push(PkgChange::SampleAdded {
                    base: base.to_string(),
                });
            }
            for p in only_remote.iter().take(8) {
                let base = p.rsplit('/').next().unwrap_or(p);
                changes.push(PkgChange::SampleRemoved {
                    base: base.to_string(),
                });
            }
            let extra = only_local.len().saturating_sub(8) + only_remote.len().saturating_sub(8);
            if extra > 0 {
                changes.push(PkgChange::More { count: extra });
            }
        }
    }
    changes
}

/// Run `nix-store --query --requisites <path>` against the local
/// store and return one line per store path. Pure metadata query —
/// the caller must guarantee the path is already in the local store
/// (see `ensure_local_closure`).
///
/// `kill_on_drop(true)` is set so cancelling the awaiting future
/// (e.g. via the `x` key) actually reaps the child instead of
/// orphaning a long-running query.
async fn nix_requisites(path: &str) -> Result<Vec<String>> {
    let out = Command::new("nix-store")
        .args(["--query", "--requisites", path])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning `nix-store --query --requisites`")?;
    if !out.status.success() {
        return Err(anyhow!(
            "nix-store --query --requisites failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// Bucket a list of store paths by parsed package name. Each value is
/// the set of distinct versions seen for that name in the closure
/// (most names map to a single version; multi-output derivations and
/// inputs that pin two versions of the same library are the
/// exceptions, hence a set rather than a single string).
fn bucket_paths_by_name(paths: &[String]) -> BTreeMap<String, BTreeSet<String>> {
    let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for p in paths {
        let base = p.rsplit('/').next().unwrap_or(p);
        let (name, version) = split_name_version(base);
        if name.is_empty() {
            continue;
        }
        map.entry(name).or_default().insert(version);
    }
    map
}

/// Parse `<hash>-<name>-<version>` from a Nix store path basename.
///
/// The hash is always the first dash-separated segment (32 lowercase
/// base32 chars in modern nix); after stripping it, we walk the
/// remainder looking for the first `-<digit>` boundary, which is
/// where nixpkgs convention puts the name/version split. Edge cases
/// (`linux-6.6.114-modules`, `bash-5.2-p37`, `python3.11-pip-24.0`)
/// all parse correctly because we only split at the *first* dash
/// followed by a digit. Paths that have no version (a bare derivation
/// name like `system-path`) are returned with an empty version
/// string.
///
/// **deploy-rs wrapper suffixes.** After the initial split we peel
/// known deploy-rs suffixes (`-activate-path`, `-activate-rs`) off
/// the end of the version and glue them back onto the name. Without
/// this, `nixos-system-host-26.05.20260405.68d8aa3-activate-path`
/// parses to name=`nixos-system-host`, version=`26.05.…-activate-path`
/// — which then looks like a "different version" of the real
/// `nixos-system-host-26.05.…` path when buckets are compared, and
/// the diff shows spurious "updates" even when the closures are
/// identical.
fn split_name_version(basename: &str) -> (String, String) {
    let after_hash = match basename.find('-') {
        Some(i) => &basename[i + 1..],
        None => return (basename.to_string(), String::new()),
    };
    let bytes = after_hash.as_bytes();
    let mut split_at: Option<usize> = None;
    for i in 0..bytes.len() {
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            split_at = Some(i);
            break;
        }
    }
    let (mut name, mut version) = match split_at {
        Some(i) => (after_hash[..i].to_string(), after_hash[i + 1..].to_string()),
        None => (after_hash.to_string(), String::new()),
    };
    for suffix in ["-activate-path", "-activate-rs"] {
        if let Some(stripped) = version.strip_suffix(suffix) {
            version = stripped.to_string();
            name.push_str(suffix);
            break;
        }
    }
    (name, version)
}

/// Render a sorted version set as a comma-separated string. Empty
/// versions render as `(no version)` so the diff doesn't print bare
/// dashes for derivations that didn't carry a version (e.g.
/// `system-path`).
fn join_versions(versions: &BTreeSet<String>) -> String {
    let parts: Vec<String> = versions
        .iter()
        .map(|v| {
            if v.is_empty() {
                "(no version)".to_string()
            } else {
                v.clone()
            }
        })
        .collect();
    parts.join(", ")
}

/// Ask the local nix store for the closure size of a path. Parses the
/// last whitespace-separated column of the first output line, which is
/// what `nix path-info --closure-size` emits. Pure measurement — the
/// caller must guarantee the path is already in the local store (see
/// `ensure_local_closure`).
async fn nix_closure_size(path: &str) -> Result<u64> {
    let out = Command::new("nix")
        .args(["path-info", "--closure-size", path])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning `nix path-info`")?;
    if !out.status.success() {
        return Err(anyhow!(
            "nix path-info failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_closure_size(&text)
        .ok_or_else(|| anyhow!("unparseable local closure size: `{}`", text.trim()))
}

/// Resolve the deploy-rs activation wrapper at `wrapper_path` down to
/// the actual system/home toplevel that the remote's
/// `/run/current-system` (or `~/.local/state/nix/profiles/home-manager`)
/// symlink will end up pointing at after activation.
///
/// Why this matters: the wrapper is a thin derivation whose closure is
/// a strict superset of the toplevel's — it adds a handful of paths
/// (the `activate` script, `deploy-rs` itself, `activatable-…`
/// sub-wrappers) that never reach the deployed system. Comparing the
/// wrapper against the remote toplevel produces a noisy "5 packages
/// changed" output even when the systems are bit-identical, because
/// those wrapper-only paths look like additions on the local side.
///
/// We identify the toplevel among the wrapper's direct references by
/// matching parsed package names against the remote basename. Both
/// sides use the same naming scheme (`nixos-system-<host>`,
/// `home-manager-generation`, etc.), so the match is unambiguous.
///
/// If anything goes wrong — `nix-store --query --references` fails, no
/// reference matches, etc. — we fall back to returning the wrapper
/// path itself and log a line so the user sees why the diff is noisy.
async fn resolve_local_toplevel(
    wrapper_path: &str,
    remote_path: &str,
    progress: &mpsc::Sender<ProgressLine>,
    tag: &str,
) -> Result<String> {
    let remote_base = remote_path.rsplit('/').next().unwrap_or(remote_path);
    let (remote_name, _) = split_name_version(remote_base);
    if remote_name.is_empty() {
        return Ok(wrapper_path.to_string());
    }
    let out = Command::new("nix-store")
        .args(["--query", "--references", wrapper_path])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning `nix-store --query --references`")?;
    if !out.status.success() {
        let _ = progress
            .send(ProgressLine::note(format!(
                "[{tag}] couldn't list wrapper references, diffing against wrapper"
            )))
            .await;
        return Ok(wrapper_path.to_string());
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let p = line.trim();
        if p.is_empty() {
            continue;
        }
        let base = p.rsplit('/').next().unwrap_or(p);
        let (name, _) = split_name_version(base);
        if name == remote_name {
            let _ = progress
                .send(ProgressLine::note(format!(
                    "[{tag}] resolved local toplevel: {p}"
                )))
                .await;
            return Ok(p.to_string());
        }
    }
    let _ = progress
        .send(ProgressLine::note(format!(
            "[{tag}] no wrapper reference matched `{remote_name}`, diffing against wrapper"
        )))
        .await;
    Ok(wrapper_path.to_string())
}

/// Make sure the local closure for a deploy-rs profile exists on this
/// machine. If the store path is already present we're done; otherwise
/// we drive `nix build` against the flake attribute so Nix evaluates
/// the profile, instantiates its derivation, pulls from substituters
/// or builds locally as needed, and populates the store.
///
/// `nix-store --realise` is NOT enough here — it only works when the
/// path's corresponding `.drv` is already in the local store. In our
/// case the caller got `path` from `nix eval --raw`, which *evaluates*
/// the profile's `.path` attribute (returning a string) but never
/// instantiates a derivation on disk. Without a `.drv`, `nix-store
/// --realise` can't know how to build the closure and dies with
/// `don't know how to build these paths`. Going through `nix build
/// <flake>#deploy.nodes.<node>.profiles.<profile>.path` re-enters the
/// flake, which *does* instantiate and build.
///
/// `tag` prefixes the progress lines so the user can see which feature
/// triggered the build (`size` for closure size, `pkg` for the
/// package diff). `kill_on_drop(true)` is set so cancelling the task
/// (key `x`) tears down the builder child instead of leaving it
/// running in the background.
async fn ensure_local_closure(
    flake: &str,
    node: &str,
    profile: &str,
    path: &str,
    progress: &mpsc::Sender<ProgressLine>,
    tag: &str,
) -> Result<()> {
    if std::path::Path::new(path).exists() {
        return Ok(());
    }
    let attr = format!("{flake}#deploy.nodes.{node}.profiles.{profile}.path");
    let _ = progress
        .send(ProgressLine::note(format!(
            "[{tag}] local closure missing, building {attr} …"
        )))
        .await;
    // --no-link avoids dropping a `result` symlink in the user's cwd;
    // --print-out-paths gives us the store path nix actually settled
    // on, which we can sanity-check against `path` afterwards.
    let out = Command::new("nix")
        .args([
            "build",
            "--no-link",
            "--no-warn-dirty",
            "--print-out-paths",
            &attr,
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning `nix build`")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(
            "`nix build {attr}` failed — couldn't materialise the \
             local closure.\n\
             hint: make sure the deploy attribute is buildable from \
             this machine (try running the command above by hand to \
             see the full nix output), then retry.\n\
             nix stderr: {}",
            stderr.trim()
        ));
    }
    // Pull the (possibly multi-line) out-path list from stdout and
    // sanity-check that at least one entry matches the path we were
    // asked to ensure. The profile's `.path` attribute in deploy-rs
    // stringifies as `<outPath>/activate`, so nix build prints the
    // parent directory — the same value the caller already trimmed to.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let built: Vec<&str> = stdout
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let _ = progress
        .send(ProgressLine::note(format!(
            "[{tag}] built {} path(s): {}",
            built.len(),
            built.join(" ")
        )))
        .await;
    // After nix build, the original path should exist. If it still
    // doesn't, something upstream has a mismatched `.path` attribute
    // vs. the built derivation — surface that clearly so the user
    // doesn't get a confusing downstream error from `nix path-info`.
    if !std::path::Path::new(path).exists() {
        return Err(anyhow!(
            "`nix build {attr}` succeeded but expected store path \
             `{path}` still isn't present.\n\
             hint: the profile's `.path` attribute may not match the \
             derivation it's wrapping. nix build produced: {}",
            built.join(" ")
        ));
    }
    Ok(())
}

/// `nix path-info --closure-size` prints rows like `<path>\t<bytes>`;
/// pull the last whitespace column off the first line and parse it.
fn parse_closure_size(text: &str) -> Option<u64> {
    text.lines()
        .next()
        .and_then(|l| l.split_whitespace().last())
        .and_then(|s| s.parse().ok())
}

/// Build the `user@host` target the way `check_profile_up_to_date`
/// used to do inline. Factored out so the size/diff probes go through
/// the exact same resolution path — including the home-profile user
/// fallback — and can't drift.
pub fn build_ssh_target(node: &Node, profile: &str, override_: &SshOverride) -> String {
    let host = override_.effective_host(&node.hostname).to_string();
    let fallback_user = match profile {
        "home" => node
            .profiles
            .get("home")
            .and_then(|p| p.user.as_deref())
            .or(node.ssh_user.as_deref()),
        _ => node.ssh_user.as_deref(),
    };
    let user = override_.effective_user(fallback_user);
    match user {
        Some(u) => format!("{u}@{host}"),
        None => host,
    }
}

/// Ask Nix for the out-path of the activation derivation. This still
/// triggers evaluation (and a build of the closure if it's missing from the
/// store), so it should run in the background.
async fn local_profile_path(flake: &str, node: &str, profile: &str) -> Result<String> {
    let attr = format!("{flake}#deploy.nodes.{node}.profiles.{profile}.path");
    let output = Command::new("nix")
        .args(["eval", "--raw", "--no-warn-dirty", &attr])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning `nix eval --raw`")?;
    if !output.status.success() {
        return Err(anyhow!(
            "nix eval failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    // deploy-rs path strings end with `/activate`; strip that to get
    // the store path of the activation *wrapper*. Note this is NOT
    // the toplevel the remote's `/run/current-system` resolves to —
    // the wrapper has the toplevel as one of its references and
    // lives at a different hash. Callers that compare against the
    // remote path need to resolve the wrapper first (see
    // `resolve_local_toplevel` / `resolve_local_toplevel_quiet`).
    let raw = String::from_utf8(output.stdout).context("`nix eval --raw` returned non-utf8")?;
    Ok(raw.trim_end_matches("/activate").to_string())
}

/// Run a non-interactive ssh command and return its stdout. Errors include
/// stderr to make TUI diagnostics legible.
async fn ssh_capture(
    target: &str,
    command: &str,
    override_: &SshOverride,
    askpass: &AskpassEnv,
) -> Result<String> {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o",
        "ConnectTimeout=3",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ]);
    // Per-host overrides go *before* the target so they take precedence
    // over anything in the user's ssh_config.
    for arg in override_.ssh_args() {
        cmd.arg(arg);
    }
    cmd.arg(target);
    cmd.arg(command);
    // kill_on_drop is critical here: when the user presses `x`
    // mid-package-check, the spawned tokio task is aborted, which
    // drops the awaiting future and the Child along with it. Without
    // kill_on_drop the ssh process — and the remote nix-store command
    // it's running — would orphan and keep going.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Route SSH prompts through the TUI's askpass mechanism.
    askpass.apply(&mut cmd);
    AskpassEnv::pre_exec_setsid(&mut cmd);

    let mut child = cmd.spawn().context("spawning ssh")?;

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let mut stdout = String::new();
    let mut stderr = String::new();
    // Both pipes must be drained *concurrently*. Reading stdout to EOF
    // first deadlocks as soon as the child fills the 64 KiB stderr pipe
    // buffer: it blocks writing stderr, so it never closes stdout, so we
    // never finish the first read. Remote `nix` commands are perfectly
    // capable of producing that much stderr.
    let (out_res, err_res) = tokio::join!(
        async {
            match out_pipe.as_mut() {
                Some(s) => s.read_to_string(&mut stdout).await.map(|_| ()),
                None => Ok(()),
            }
        },
        async {
            match err_pipe.as_mut() {
                Some(s) => s.read_to_string(&mut stderr).await.map(|_| ()),
                None => Ok(()),
            }
        }
    );
    // A read error means we have partial output; the exit status below
    // still decides success, and `stderr` still carries whatever arrived.
    let _ = (out_res, err_res);

    let status = child.wait().await.context("waiting for ssh")?;
    if !status.success() {
        return Err(anyhow!("ssh `{command}` failed: {}", stderr.trim()));
    }
    Ok(stdout)
}

// ---------------------------------------------------------------------
// Substituter drift
// ---------------------------------------------------------------------

/// Where the build for a deploy will actually run — which decides whose
/// substituter list matters.
///
/// This is the whole crux of the check. deploy-rs's `--remote-build` is
/// not `ssh host nix build`; it is a *local* nix client driving a remote
/// store (`nix build … --eval-store auto --store ssh-ng://…`). Fetching
/// is therefore done by the target's daemon, out of the target's own
/// `/etc/nix/nix.conf` — which is exactly the file the deploy is about to
/// replace. Options passed to the local client don't cross that store
/// boundary. For a local build none of that applies: the build runs here,
/// against this machine's substituters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildSite {
    /// Built on this machine, then copied to the target.
    Local,
    /// Built into the target's store by its own daemon.
    Remote,
}

impl BuildSite {
    pub fn label(self) -> &'static str {
        match self {
            BuildSite::Local => "local",
            BuildSite::Remote => "remote",
        }
    }
}

/// The substituter-related settings of one nix installation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NixSubstituterConfig {
    pub substituters: Vec<String>,
    pub trusted_public_keys: Vec<String>,
    /// `trusted-users` as reported by `nix config show`. Empty when the
    /// setting wasn't present (only meaningful for the remote side).
    pub trusted_users: Vec<String>,
}

/// Result of comparing the closure we're about to deploy against the
/// nix installation that will do the building.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubstituterDrift {
    pub site: BuildSite,
    /// Caches the new config declares that the building store doesn't
    /// have yet. These are the ones this deploy cannot benefit from.
    pub added_substituters: Vec<String>,
    /// Public keys the new config adds. A cache without its key is
    /// useless, so these matter as much as the substituters.
    pub added_keys: Vec<String>,
    /// Caches the new config drops. Informational only.
    pub removed_substituters: Vec<String>,
    /// The login the remote daemon sees, and whether nix trusts it.
    /// `None` for local builds, where there is no ssh user involved.
    pub ssh_user: Option<String>,
    /// `Some(false)` means substituter overrides sent to that host would
    /// be *silently* ignored — the single most confusing failure mode
    /// here, because nix reports nothing and simply builds.
    pub ssh_user_trusted: Option<bool>,
}

impl SubstituterDrift {
    /// True when this deploy adds a cache it cannot itself use.
    pub fn has_drift(&self) -> bool {
        !self.added_substituters.is_empty() || !self.added_keys.is_empty()
    }
}

/// Split a nix.conf-style space-separated setting value.
fn split_setting(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Normalise a substituter URL for comparison. `nix config show` prints
/// `https://cache.nixos.org/` where a config may say
/// `https://cache.nixos.org` — comparing raw strings reports drift that
/// isn't there.
fn normalise_substituter(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Parse the output of `nix config show` (or the older `nix show-config`).
///
/// Lines look like `substituters = https://a/ https://b/`. Unknown keys
/// are ignored, so this stays forward-compatible with new settings.
pub fn parse_nix_config_show(text: &str) -> NixSubstituterConfig {
    let mut cfg = NixSubstituterConfig::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "substituters" => cfg.substituters = split_setting(value),
            "trusted-substituters" => {
                // Additive: a trusted-substituter is usable too.
                cfg.substituters.extend(split_setting(value));
            }
            "trusted-public-keys" => cfg.trusted_public_keys = split_setting(value),
            "trusted-users" => cfg.trusted_users = split_setting(value),
            _ => {}
        }
    }
    cfg
}

/// Is `user` (a member of `groups`) covered by a `trusted-users` list?
///
/// nix accepts both bare user names and `@group` entries, and `root` is
/// always trusted whether or not it is listed.
pub fn user_is_trusted(trusted: &[String], user: &str, groups: &[String]) -> bool {
    if user == "root" {
        return true;
    }
    trusted.iter().any(|t| match t.strip_prefix('@') {
        Some(group) => groups.iter().any(|g| g == group),
        None => t == user,
    })
}

/// Everything the remote probe returns in one round trip.
struct RemoteNixProbe {
    config: NixSubstituterConfig,
    user: String,
    groups: Vec<String>,
}

/// Marker separating the three sections of the remote probe's output.
const REMOTE_PROBE_SEP: &str = "__DEPTUI_SEP__";

/// Read the target's effective nix config plus the identity of the ssh
/// login, in a single SSH round trip.
async fn remote_nix_probe(
    target: &str,
    override_: &SshOverride,
    askpass: &AskpassEnv,
) -> Result<RemoteNixProbe> {
    // `nix config show` replaced `nix show-config` in nix 2.20; try the
    // modern spelling first and fall back so older targets still work.
    let script = format!(
        "{{ nix config show 2>/dev/null || nix show-config 2>/dev/null; }}; \
echo {REMOTE_PROBE_SEP}; id -un; echo {REMOTE_PROBE_SEP}; id -nG"
    );
    let out = ssh_capture(target, &script, override_, askpass)
        .await
        .context("reading the target's nix configuration")?;
    let mut parts = out.split(REMOTE_PROBE_SEP);
    let config = parse_nix_config_show(parts.next().unwrap_or_default());
    if config.substituters.is_empty() {
        return Err(anyhow!(
            "the target reported no `substituters` — is `nix` on the ssh user's PATH?"
        ));
    }
    let user = parts.next().unwrap_or_default().trim().to_string();
    let groups = split_setting(parts.next().unwrap_or_default());
    Ok(RemoteNixProbe {
        config,
        user,
        groups,
    })
}

/// Read this machine's effective nix config.
async fn local_nix_config() -> Result<NixSubstituterConfig> {
    let out = Command::new("sh")
        .arg("-c")
        .arg("nix config show 2>/dev/null || nix show-config")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning `nix config show`")?;
    if !out.status.success() {
        return Err(anyhow!(
            "reading the local nix configuration failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(parse_nix_config_show(&String::from_utf8_lossy(&out.stdout)))
}

/// The `nix eval --apply` function used to read the closure's declared
/// substituter settings.
///
/// `nix.settings` is a freeform attrset, so any of these four keys may be
/// missing; `or []` keeps the eval total. Reading `substituters` alone
/// would miss the `extra-substituters` spelling, which is how most people
/// actually add a cache.
const SETTINGS_APPLY: &str = "s: { \
substituters = s.substituters or []; \
extraSubstituters = s.\"extra-substituters\" or []; \
trustedPublicKeys = s.\"trusted-public-keys\" or []; \
extraTrustedPublicKeys = s.\"extra-trusted-public-keys\" or []; \
}";

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeclaredSettings {
    #[serde(default)]
    substituters: Vec<String>,
    #[serde(default)]
    extra_substituters: Vec<String>,
    #[serde(default)]
    trusted_public_keys: Vec<String>,
    #[serde(default)]
    extra_trusted_public_keys: Vec<String>,
}

/// Read `nix.settings` out of the flake's `nixosConfigurations.<node>`.
///
/// This is an *evaluation*, not a build: it costs a NixOS module eval
/// (seconds) rather than realising the system closure, which is the whole
/// point — the user wants to know what the deploy will do before paying
/// for it.
async fn declared_substituter_config(flake: &str, node: &str) -> Result<NixSubstituterConfig> {
    let attr = format!("{flake}#nixosConfigurations.{node}.config.nix.settings");
    let out = Command::new("nix")
        .args([
            "eval",
            "--json",
            "--no-warn-dirty",
            &attr,
            "--apply",
            SETTINGS_APPLY,
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning `nix eval`")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(
            "could not evaluate `nixosConfigurations.{node}.config.nix.settings` — \
deploy nodes whose name differs from their nixosConfigurations attribute aren't \
supported by this check yet: {}",
            stderr.trim()
        ));
    }
    let parsed: DeclaredSettings =
        serde_json::from_slice(&out.stdout).context("parsing `nix eval --json` output")?;
    let mut substituters = parsed.substituters;
    substituters.extend(parsed.extra_substituters);
    let mut trusted_public_keys = parsed.trusted_public_keys;
    trusted_public_keys.extend(parsed.extra_trusted_public_keys);
    Ok(NixSubstituterConfig {
        substituters,
        trusted_public_keys,
        trusted_users: Vec::new(),
    })
}

/// Which entries of `declared` are missing from `current`, compared on
/// normalised URLs but reported in their declared spelling.
fn added_entries(declared: &[String], current: &[String], normalise: bool) -> Vec<String> {
    let have: BTreeSet<String> = current
        .iter()
        .map(|s| {
            if normalise {
                normalise_substituter(s)
            } else {
                s.trim().to_string()
            }
        })
        .collect();
    let mut seen = BTreeSet::new();
    declared
        .iter()
        .filter(|d| {
            let key = if normalise {
                normalise_substituter(d)
            } else {
                d.trim().to_string()
            };
            !have.contains(&key) && seen.insert(key)
        })
        .cloned()
        .collect()
}

/// Compare a declared config against the config of the store that will
/// build, and describe the difference.
pub fn compute_substituter_drift(
    site: BuildSite,
    declared: &NixSubstituterConfig,
    current: &NixSubstituterConfig,
) -> SubstituterDrift {
    SubstituterDrift {
        site,
        added_substituters: added_entries(&declared.substituters, &current.substituters, true),
        added_keys: added_entries(
            &declared.trusted_public_keys,
            &current.trusted_public_keys,
            false,
        ),
        removed_substituters: added_entries(&current.substituters, &declared.substituters, true),
        ssh_user: None,
        ssh_user_trusted: None,
    }
}

/// Detect whether deploying `node` would add a binary cache that the
/// build itself cannot use.
///
/// Compares the closure's declared `nix.settings` against the nix
/// installation that will actually do the building — the *target's* for
/// a remote build, *this machine's* for a local one. Getting that side
/// right is the entire value of the check: a cache added to the target
/// does nothing for a local build, and options passed to a local client
/// do nothing for a remote one.
///
/// `progress` receives one line per stage; the channel is best-effort.
pub async fn check_substituter_drift(
    flake: &str,
    node: &Node,
    site: BuildSite,
    override_: &SshOverride,
    askpass: &AskpassEnv,
    progress: mpsc::Sender<ProgressLine>,
) -> Result<SubstituterDrift> {
    let _ = progress
        .send(ProgressLine::note(format!(
            "[cache] evaluating nix.settings for {} …",
            node.name
        )))
        .await;
    let declared = declared_substituter_config(flake, &node.name).await?;

    match site {
        BuildSite::Local => {
            let _ = progress
                .send(ProgressLine::note(
                    "[cache] reading this machine's nix config …",
                ))
                .await;
            let current = local_nix_config().await?;
            Ok(compute_substituter_drift(site, &declared, &current))
        }
        BuildSite::Remote => {
            let target = build_ssh_target(node, "system", override_);
            let _ = progress
                .send(ProgressLine::note(format!(
                    "[cache] reading nix config on {target} …"
                )))
                .await;
            let probe = remote_nix_probe(&target, override_, askpass).await?;
            let mut drift = compute_substituter_drift(site, &declared, &probe.config);
            drift.ssh_user_trusted = Some(user_is_trusted(
                &probe.config.trusted_users,
                &probe.user,
                &probe.groups,
            ));
            drift.ssh_user = Some(probe.user);
            Ok(drift)
        }
    }
}

// ---------------------------------------------------------------------
// Build-plan preflight
// ---------------------------------------------------------------------

/// What deploying a profile would actually cost, as reported by
/// `nix build --dry-run`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildPlan {
    /// Full `.drv` paths that would be compiled, deduplicated and in the
    /// order nix listed them. Kept as full paths rather than display
    /// names because cache seeding needs to resolve their outputs.
    pub to_build: Vec<String>,
    /// Full store paths that would be substituted.
    pub to_fetch: Vec<String>,
    /// Output paths of `to_build`, resolved from the derivations. These
    /// are what a cache could supply to make the build unnecessary, so
    /// they are what seeding copies.
    pub build_outputs: Vec<String>,
    /// Download size in bytes, when nix reported one.
    pub download_bytes: Option<u64>,
    /// Unpacked size in bytes, when nix reported one.
    pub unpacked_bytes: Option<u64>,
    /// True when nix said nothing at all needs doing.
    pub nothing_to_do: bool,
}

impl BuildPlan {
    /// A deploy that compiles anything is the case worth warning about —
    /// it's the difference between a two-minute fetch and a forty-minute
    /// build.
    pub fn builds_anything(&self) -> bool {
        !self.to_build.is_empty()
    }

    /// Readable names for the derivations that would be compiled.
    pub fn build_labels(&self) -> Vec<String> {
        self.to_build.iter().map(|p| store_path_label(p)).collect()
    }

    /// Readable names for the paths that would be fetched.
    pub fn fetch_labels(&self) -> Vec<String> {
        self.to_fetch.iter().map(|p| store_path_label(p)).collect()
    }

    /// Every store path a cache could usefully supply: the outputs of
    /// what would otherwise be compiled, plus what nix already intends
    /// to fetch (which a closer cache may serve).
    pub fn seedable_paths(&self) -> Vec<String> {
        let mut out = self.build_outputs.clone();
        out.extend(self.to_fetch.iter().cloned());
        out
    }
}

/// Strip a store path down to a readable package name: drop the
/// `/nix/store/<hash>-` prefix, the `.drv` suffix, and any output
/// selector.
fn store_path_label(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    // Output selectors come in two spellings: `path^out` (nix 2.x
    // installables) and the older `path!out`.
    let base = base.split(['^', '!']).next().unwrap_or(base);
    let base = base.strip_suffix(".drv").unwrap_or(base);
    let (name, version) = split_name_version(base);
    if version.is_empty() {
        name
    } else {
        format!("{name}-{version}")
    }
}

/// Parse a size like `1.20 MiB` / `512 KiB` / `3 bytes` into bytes.
fn parse_human_size(text: &str) -> Option<u64> {
    let mut it = text.split_whitespace();
    let value: f64 = it.next()?.parse().ok()?;
    let unit = it.next().unwrap_or("bytes");
    let mult: f64 = match unit.trim_end_matches(',') {
        "bytes" | "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * mult) as u64)
}

/// Parse the human-readable plan `nix build --dry-run` writes to stderr.
///
/// The shape we care about:
///
/// ```text
/// these 17 derivations will be built:
///   /nix/store/…-ollama-0.5.4.drv
/// these 3 paths will be fetched (1.20 MiB download, 5.00 MiB unpacked):
///   /nix/store/…-foo
/// ```
///
/// Singular variants (`this derivation will be built:`) and the
/// no-op case (no headers at all) are both handled. This is nix's
/// human output rather than a stable API, so anything unrecognised is
/// ignored instead of failing the whole preflight.
pub fn parse_dry_run(text: &str) -> BuildPlan {
    #[derive(Clone, Copy, PartialEq)]
    enum Section {
        None,
        Build,
        Fetch,
    }
    let mut plan = BuildPlan::default();
    let mut section = Section::None;
    let mut seen_build = BTreeSet::new();
    let mut seen_fetch = BTreeSet::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // Header lines end in `:` and mention build or fetch.
        if line.ends_with(':') {
            let lower = line.to_ascii_lowercase();
            if lower.contains("will be built") {
                section = Section::Build;
                continue;
            }
            if lower.contains("will be fetched") {
                section = Section::Fetch;
                // Sizes live in the parenthesised part of the header.
                if let Some(inner) = line
                    .split_once('(')
                    .and_then(|(_, rest)| rest.split_once(')'))
                    .map(|(inner, _)| inner)
                {
                    for part in inner.split(',') {
                        let part = part.trim();
                        if let Some(size) = part.strip_suffix(" download") {
                            plan.download_bytes = parse_human_size(size);
                        } else if let Some(size) = part.strip_suffix(" unpacked") {
                            plan.unpacked_bytes = parse_human_size(size);
                        }
                    }
                }
                continue;
            }
            // Some other header (e.g. "will be removed") — stop
            // attributing paths to the previous section.
            section = Section::None;
            continue;
        }
        if !line.starts_with("/nix/store/") {
            continue;
        }
        // Strip any output selector but keep the full store path — the
        // display name is derived on demand, while seeding needs the path.
        let path = line.split(['^', '!']).next().unwrap_or(line).to_string();
        match section {
            Section::Build => {
                if seen_build.insert(path.clone()) {
                    plan.to_build.push(path);
                }
            }
            Section::Fetch => {
                if seen_fetch.insert(path.clone()) {
                    plan.to_fetch.push(path);
                }
            }
            Section::None => {}
        }
    }
    plan.nothing_to_do =
        plan.to_build.is_empty() && plan.to_fetch.is_empty() && plan.download_bytes.is_none();
    plan
}

/// Evaluate the `.drv` path of a deploy-rs profile without building it.
async fn profile_drv_path(flake: &str, node: &str, profile: &str) -> Result<String> {
    let attr = format!("{flake}#deploy.nodes.{node}.profiles.{profile}.path.drvPath");
    let out = Command::new("nix")
        .args(["eval", "--raw", "--no-warn-dirty", &attr])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning `nix eval --raw` for the derivation path")?;
    if !out.status.success() {
        return Err(anyhow!(
            "could not resolve the derivation for {node}.{profile}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run `nix build --dry-run` and return its combined output.
///
/// `--dry-run` writes the plan to stderr, so both streams are captured.
async fn nix_dry_run(args: &[String]) -> Result<String> {
    let out = Command::new("nix")
        .arg("build")
        .arg("--dry-run")
        .arg("--no-warn-dirty")
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("spawning `nix build --dry-run`")?;
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        return Err(anyhow!("nix build --dry-run failed: {}", stderr.trim()));
    }
    Ok(format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        stderr
    ))
}

/// Resolve the output paths of a set of derivations.
///
/// The derivations are always present in the *local* store here: a local
/// dry-run instantiates them, and the remote path copies the derivation
/// closure to the target before dry-running, so it evaluated them
/// locally too.
async fn resolve_build_outputs(drvs: &[String]) -> Vec<String> {
    if drvs.is_empty() {
        return Vec::new();
    }
    // `nix derivation show` replaced `nix show-derivation` in nix 2.17.
    for verb in [&["derivation", "show"][..], &["show-derivation"][..]] {
        let out = Command::new("nix")
            .args(verb)
            .arg("--no-warn-dirty")
            .args(drvs)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await;
        let Ok(out) = out else { continue };
        if !out.status.success() {
            continue;
        }
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
            continue;
        };
        let Some(map) = json.as_object() else {
            continue;
        };
        let mut paths = Vec::new();
        for drv in map.values() {
            let Some(outputs) = drv.get("outputs").and_then(|o| o.as_object()) else {
                continue;
            };
            for output in outputs.values() {
                if let Some(p) = output.get("path").and_then(|p| p.as_str()) {
                    paths.push(p.to_string());
                }
            }
        }
        return paths;
    }
    Vec::new()
}

/// What a seeding attempt achieved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeedOutcome {
    /// Paths the cache had and that now exist in the target's store.
    pub copied: usize,
    /// Paths the cache didn't have. Expected and harmless — the build
    /// falls back to compiling them, exactly as it would have anyway.
    pub missed: usize,
    /// Paths that were considered but not attempted because the list was
    /// capped. Reported so a partial seed never reads as a complete one.
    pub skipped: usize,
}

/// Upper bound on how many paths one seeding pass attempts. A
/// from-scratch system closure can be thousands of entries, and each is
/// a separate `nix copy` on the target.
const MAX_SEED_PATHS: usize = 200;

/// Marker the seeding script prints per successful copy.
const SEED_OK: &str = "__DEPTUI_OK__";

/// Copy paths the new substituters can supply into the target's store,
/// *before* the deploy builds.
///
/// This is the additive remediation for the substituter trap: rather
/// than rewriting the target's `nix.conf` (which would then have to be
/// restored on every failure path), we simply put the paths in its store
/// ahead of time. The build then finds them already valid and skips
/// them. Nothing needs undoing — a store path that turns out to be
/// unnecessary is just garbage-collectable.
///
/// Each path is copied individually because `nix copy` fails the whole
/// batch when one path is absent from the cache, and absence is the
/// normal case: only some of the closure lives in any given cache.
///
/// The new cache's public key is passed with `--option
/// extra-trusted-public-keys`, which nix honours **only for a trusted
/// user**. `check_substituter_drift`'s `trusted-users` check is what
/// tells the user when that silently won't apply.
pub async fn seed_substituters(
    node: &Node,
    override_: &SshOverride,
    askpass: &AskpassEnv,
    substituters: &[String],
    keys: &[String],
    paths: &[String],
    progress: mpsc::Sender<ProgressLine>,
) -> Result<SeedOutcome> {
    if substituters.is_empty() || paths.is_empty() {
        return Ok(SeedOutcome::default());
    }
    let target = build_ssh_target(node, "system", override_);
    let skipped = paths.len().saturating_sub(MAX_SEED_PATHS);
    let attempt: Vec<&String> = paths.iter().take(MAX_SEED_PATHS).collect();

    let key_opt = if keys.is_empty() {
        String::new()
    } else {
        format!(
            " --option extra-trusted-public-keys {}",
            shell_quote(&keys.join(" "))
        )
    };

    // One ssh round trip. Per path, try each cache and stop at the first
    // that has it — so a path is copied once and reports success once,
    // rather than once per cache that happens to hold it.
    let path_list = attempt
        .iter()
        .map(|p| shell_quote(p))
        .collect::<Vec<_>>()
        .join(" ");
    let cache_list = substituters
        .iter()
        .map(|u| shell_quote(u))
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "for p in {path_list}; do \
for f in {cache_list}; do \
if nix copy --from \"$f\"{key_opt} \"$p\" >/dev/null 2>&1; then echo {SEED_OK}; break; fi; \
done; done"
    );

    let _ = progress
        .send(ProgressLine::note(format!(
            "[seed] offering {} path(s) from {} cache(s) to {target} …",
            attempt.len(),
            substituters.len()
        )))
        .await;

    let out = ssh_capture(&target, &script, override_, askpass)
        .await
        .context("seeding the target's store from the new substituter(s)")?;
    let copied = out.lines().filter(|l| l.trim() == SEED_OK).count();
    Ok(SeedOutcome {
        copied,
        missed: attempt.len().saturating_sub(copied),
        skipped,
    })
}

/// Single-quote a string for POSIX `sh`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Preflight what deploying `(node, profile)` would build and fetch.
///
/// Mirrors the way deploy-rs actually builds, because the answer differs
/// between the two modes:
///
/// - local: `nix build --dry-run <flake>#…profiles.<p>.path`
/// - remote: `nix build --dry-run <drv>^out --eval-store auto
///   --store ssh-ng://<target>`, which reports against the *target's*
///   store rather than this machine's.
///
/// The remote form needs the derivation present in the target's store
/// first, so we `nix copy --derivation` it there — the same step
/// deploy-rs performs before a remote build. That writes only a `.drv`
/// to the target's store; nothing is realised and nothing outside the
/// store is touched.
pub async fn check_build_plan(
    flake: &str,
    node: &Node,
    profile: &str,
    site: BuildSite,
    override_: &SshOverride,
    progress: mpsc::Sender<ProgressLine>,
) -> Result<BuildPlan> {
    match site {
        BuildSite::Local => {
            let attr = format!("{flake}#deploy.nodes.{}.profiles.{profile}.path", node.name);
            let _ = progress
                .send(ProgressLine::note(format!(
                    "[plan] dry-run (local store) for {profile} …"
                )))
                .await;
            let text = nix_dry_run(&[attr]).await?;
            let mut plan = parse_dry_run(&text);
            plan.build_outputs = resolve_build_outputs(&plan.to_build).await;
            Ok(plan)
        }
        BuildSite::Remote => {
            let target = build_ssh_target(node, profile, override_);
            let store = format!("ssh-ng://{target}");
            let _ = progress
                .send(ProgressLine::note(format!(
                    "[plan] resolving derivation for {profile} …"
                )))
                .await;
            let drv = profile_drv_path(flake, &node.name, profile).await?;

            // The remote store can only reason about a derivation it
            // has. deploy-rs copies it too, so this doesn't add a step
            // the real deploy wouldn't take.
            let _ = progress
                .send(ProgressLine::note(format!(
                    "[plan] copying derivation to {target} …"
                )))
                .await;
            let copy = Command::new("nix")
                .args(["copy", "--derivation", "--to", &store, &drv])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await
                .context("spawning `nix copy --derivation`")?;
            if !copy.status.success() {
                return Err(anyhow!(
                    "copying the derivation to {target} failed: {}",
                    String::from_utf8_lossy(&copy.stderr).trim()
                ));
            }

            let _ = progress
                .send(ProgressLine::note(format!(
                    "[plan] dry-run against {store} …"
                )))
                .await;
            let text = nix_dry_run(&[
                format!("{drv}^out"),
                "--eval-store".to_string(),
                "auto".to_string(),
                "--store".to_string(),
                store,
            ])
            .await?;
            let mut plan = parse_dry_run(&text);
            plan.build_outputs = resolve_build_outputs(&plan.to_build).await;
            Ok(plan)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    // ---- split_name_version ----

    #[test]
    fn split_basic_package() {
        let (name, ver) = split_name_version("abc123-openssl-3.5.1");
        assert_eq!(name, "openssl");
        assert_eq!(ver, "3.5.1");
    }

    #[test]
    fn split_no_version() {
        let (name, ver) = split_name_version("abc123-system-path");
        assert_eq!(name, "system-path");
        assert_eq!(ver, "");
    }

    #[test]
    fn split_nixos_system_with_activate_suffix() {
        // deploy-rs wrapper path
        let (name, ver) =
            split_name_version("abc123-nixos-system-myhost-26.05.20260405.68d8aa3-activate-path");
        assert_eq!(name, "nixos-system-myhost-activate-path");
        assert_eq!(ver, "26.05.20260405.68d8aa3");
    }

    #[test]
    fn split_activate_rs_suffix() {
        let (name, ver) = split_name_version("abc123-nixos-system-myhost-26.05.12345-activate-rs");
        assert_eq!(name, "nixos-system-myhost-activate-rs");
        assert_eq!(ver, "26.05.12345");
    }

    #[test]
    fn split_linux_modules() {
        let (name, ver) = split_name_version("abc123-linux-6.6.114-modules");
        assert_eq!(name, "linux");
        assert_eq!(ver, "6.6.114-modules");
    }

    #[test]
    fn split_python_package() {
        let (name, ver) = split_name_version("abc123-python3.11-pip-24.0");
        assert_eq!(name, "python3.11-pip");
        assert_eq!(ver, "24.0");
    }

    #[test]
    fn split_bash_patch() {
        let (name, ver) = split_name_version("abc123-bash-5.2-p37");
        assert_eq!(name, "bash");
        assert_eq!(ver, "5.2-p37");
    }

    #[test]
    fn split_no_hash_separator() {
        let (name, ver) = split_name_version("nohash");
        assert_eq!(name, "nohash");
        assert_eq!(ver, "");
    }

    // ---- parsed_paths_equivalent ----

    #[test]
    fn equivalent_same_toplevel() {
        assert!(parsed_paths_equivalent(
            "/nix/store/abc123-nixos-system-host-26.05.12345",
            "/nix/store/abc123-nixos-system-host-26.05.12345",
        ));
    }

    #[test]
    fn equivalent_wrapper_vs_toplevel() {
        // Wrapper has -activate-path suffix; the function should peel it.
        assert!(parsed_paths_equivalent(
            "/nix/store/xyz-nixos-system-host-26.05.12345-activate-path",
            "/nix/store/abc-nixos-system-host-26.05.12345",
        ));
    }

    #[test]
    fn not_equivalent_different_versions() {
        assert!(!parsed_paths_equivalent(
            "/nix/store/abc-nixos-system-host-26.05.11111",
            "/nix/store/abc-nixos-system-host-26.05.22222",
        ));
    }

    // ---- parse_closure_size ----

    #[test]
    fn parse_closure_size_normal() {
        let input = "/nix/store/abc-foo\t1234567890\n";
        assert_eq!(parse_closure_size(input), Some(1234567890));
    }

    #[test]
    fn parse_closure_size_spaces() {
        let input = "/nix/store/abc-foo   999\n";
        assert_eq!(parse_closure_size(input), Some(999));
    }

    #[test]
    fn parse_closure_size_empty() {
        assert_eq!(parse_closure_size(""), None);
    }

    #[test]
    fn parse_closure_size_no_number() {
        assert_eq!(parse_closure_size("garbage"), None);
    }

    // ---- join_versions ----

    #[test]
    fn join_versions_single() {
        let mut s = BTreeSet::new();
        s.insert("3.5.1".to_string());
        assert_eq!(join_versions(&s), "3.5.1");
    }

    #[test]
    fn join_versions_empty_version() {
        let mut s = BTreeSet::new();
        s.insert(String::new());
        assert_eq!(join_versions(&s), "(no version)");
    }

    #[test]
    fn join_versions_multiple() {
        let mut s = BTreeSet::new();
        s.insert("1.0".to_string());
        s.insert("2.0".to_string());
        assert_eq!(join_versions(&s), "1.0, 2.0");
    }

    // ---- bucket_paths_by_name ----

    #[test]
    fn bucket_groups_by_name() {
        let paths = vec![
            "/nix/store/aaa-openssl-3.5.1".to_string(),
            "/nix/store/bbb-openssl-3.5.2".to_string(),
            "/nix/store/ccc-bash-5.2".to_string(),
        ];
        let map = bucket_paths_by_name(&paths);
        assert_eq!(map.len(), 2);
        let openssl = map.get("openssl").unwrap();
        assert!(openssl.contains("3.5.1"));
        assert!(openssl.contains("3.5.2"));
        let bash = map.get("bash").unwrap();
        assert!(bash.contains("5.2"));
    }

    // ---- compute_version_diff ----

    #[test]
    fn diff_identical_closures() {
        let paths = vec![
            "/nix/store/aaa-openssl-3.5.1".to_string(),
            "/nix/store/bbb-bash-5.2".to_string(),
        ];
        let changes = compute_version_diff(&paths, &paths);
        assert!(changes.is_empty(), "identical closures should have no diff");
    }

    #[test]
    fn diff_version_update() {
        let local = vec!["/nix/store/aaa-openssl-3.5.2".to_string()];
        let remote = vec!["/nix/store/bbb-openssl-3.5.1".to_string()];
        let changes = compute_version_diff(&local, &remote);
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            PkgChange::Updated { name, from, to } => {
                assert_eq!(name, "openssl");
                assert_eq!(from, "3.5.1");
                assert_eq!(to, "3.5.2");
            }
            other => panic!("expected Updated, got {other:?}"),
        }
        assert_eq!(changes[0].render(), "openssl: 3.5.1 → 3.5.2");
    }

    #[test]
    fn diff_added_package() {
        let local = vec![
            "/nix/store/aaa-openssl-3.5.1".to_string(),
            "/nix/store/bbb-curl-8.0".to_string(),
        ];
        let remote = vec!["/nix/store/ccc-openssl-3.5.1".to_string()];
        let changes = compute_version_diff(&local, &remote);
        assert_eq!(changes.len(), 1);
        assert!(
            matches!(&changes[0], PkgChange::Added { name, .. } if name == "curl"),
            "{:?}",
            changes[0]
        );
        assert!(changes[0].render().starts_with("curl: +"));
    }

    #[test]
    fn diff_removed_package() {
        let local = vec!["/nix/store/aaa-openssl-3.5.1".to_string()];
        let remote = vec![
            "/nix/store/bbb-openssl-3.5.1".to_string(),
            "/nix/store/ccc-curl-8.0".to_string(),
        ];
        let changes = compute_version_diff(&local, &remote);
        assert_eq!(changes.len(), 1);
        assert!(
            matches!(&changes[0], PkgChange::Removed { name, .. } if name == "curl"),
            "{:?}",
            changes[0]
        );
        assert!(changes[0].render().starts_with("curl: -"));
    }

    #[test]
    fn diff_content_only_change() {
        // Same name+version but different store hashes → content-only.
        let local = vec!["/nix/store/aaa-openssl-3.5.1".to_string()];
        let remote = vec!["/nix/store/bbb-openssl-3.5.1".to_string()];
        let changes = compute_version_diff(&local, &remote);
        assert!(!changes.is_empty());
        assert!(
            matches!(&changes[0], PkgChange::ContentOnly { count: 1 }),
            "expected ContentOnly head, got: {:?}",
            changes[0]
        );
        assert!(changes[0].render().contains("(content-only)"));
        let diff = PkgDiff { changes };
        assert!(diff.is_content_only());
        assert_eq!(diff.change_count(), 0);
    }

    // ---- build_ssh_target ----

    #[test]
    fn build_ssh_target_no_override() {
        let node = Node {
            name: "myhost".into(),
            hostname: "myhost.example.com".into(),
            ssh_user: Some("root".into()),
            profiles: BTreeMap::new(),
            profiles_order: None,
        };
        let o = SshOverride::default();
        assert_eq!(
            build_ssh_target(&node, "system", &o),
            "root@myhost.example.com"
        );
    }

    #[test]
    fn build_ssh_target_with_override() {
        let node = Node {
            name: "myhost".into(),
            hostname: "myhost.example.com".into(),
            ssh_user: Some("root".into()),
            profiles: BTreeMap::new(),
            profiles_order: None,
        };
        let o = SshOverride {
            hostname: Some("10.0.0.5".into()),
            user: Some("admin".into()),
            ..Default::default()
        };
        assert_eq!(build_ssh_target(&node, "system", &o), "admin@10.0.0.5");
    }

    #[test]
    fn build_ssh_target_no_user() {
        let node = Node {
            name: "myhost".into(),
            hostname: "myhost.example.com".into(),
            ssh_user: None,
            profiles: BTreeMap::new(),
            profiles_order: None,
        };
        let o = SshOverride::default();
        // No user at all → bare hostname.
        assert_eq!(build_ssh_target(&node, "system", &o), "myhost.example.com");
    }

    #[test]
    fn build_ssh_target_home_profile_user() {
        use crate::flake::Profile;
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "home".into(),
            Profile {
                user: Some("jd".into()),
                ssh_user: None,
            },
        );
        let node = Node {
            name: "myhost".into(),
            hostname: "myhost.example.com".into(),
            ssh_user: None,
            profiles,
            profiles_order: None,
        };
        let o = SshOverride::default();
        // Home profile should use its own user.
        assert_eq!(build_ssh_target(&node, "home", &o), "jd@myhost.example.com");
    }
}

#[cfg(test)]
mod build_plan_tests {
    use super::*;

    const SAMPLE: &str = "\
this path will be fetched (0.02 MiB download, 0.10 MiB unpacked):
  /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1
these 2 derivations will be built:
  /nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-cuda-merged-12.4.drv
  /nix/store/cccccccccccccccccccccccccccccccc-ollama-0.5.4.drv
";

    #[test]
    fn parses_builds_and_fetches() {
        let plan = parse_dry_run(SAMPLE);
        assert_eq!(
            plan.build_labels(),
            vec!["cuda-merged-12.4", "ollama-0.5.4"]
        );
        assert_eq!(plan.fetch_labels(), vec!["hello-2.12.1"]);
        // Full paths are retained so seeding can resolve outputs.
        assert!(plan.to_build[0].starts_with("/nix/store/"));
        assert!(plan.builds_anything());
        assert!(!plan.nothing_to_do);
    }

    #[test]
    fn parses_download_and_unpacked_sizes() {
        let plan = parse_dry_run(SAMPLE);
        assert_eq!(plan.download_bytes, Some((0.02 * 1024.0 * 1024.0) as u64));
        assert_eq!(plan.unpacked_bytes, Some((0.10 * 1024.0 * 1024.0) as u64));
    }

    #[test]
    fn empty_output_means_nothing_to_do() {
        let plan = parse_dry_run("");
        assert!(plan.nothing_to_do);
        assert!(!plan.builds_anything());
    }

    #[test]
    fn handles_singular_headers() {
        let plan = parse_dry_run(
            "this derivation will be built:\n  /nix/store/dddddddddddddddddddddddddddddddd-foo-1.0.drv\n",
        );
        assert_eq!(plan.build_labels(), vec!["foo-1.0"]);
    }

    #[test]
    fn ignores_unrelated_sections() {
        // A "will be removed" block must not have its paths counted as
        // builds just because it followed a build header.
        let plan = parse_dry_run(
            "these 1 derivations will be built:\n\
  /nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-keep-1.0.drv\n\
these paths will be removed:\n\
  /nix/store/ffffffffffffffffffffffffffffffff-gone-1.0\n",
        );
        assert_eq!(plan.build_labels(), vec!["keep-1.0"]);
        assert!(plan.to_fetch.is_empty());
    }

    #[test]
    fn deduplicates_identical_paths() {
        let plan = parse_dry_run(
            "these 2 derivations will be built:\n\
  /nix/store/11111111111111111111111111111111-foo-1.0.drv\n\
  /nix/store/22222222222222222222222222222222-foo-1.0.drv\n",
        );
        // Distinct store paths for the same package are distinct
        // derivations — dedup is by path, not by display name.
        assert_eq!(plan.build_labels(), vec!["foo-1.0", "foo-1.0"]);
    }

    #[test]
    fn parses_byte_and_gib_sizes() {
        assert_eq!(parse_human_size("512 bytes"), Some(512));
        assert_eq!(parse_human_size("1 KiB"), Some(1024));
        assert_eq!(parse_human_size("1.20 GiB"), Some(1288490188));
        assert_eq!(parse_human_size("nonsense"), None);
    }

    #[test]
    fn store_path_label_strips_hash_and_drv() {
        assert_eq!(
            store_path_label("/nix/store/33333333333333333333333333333333-ollama-0.5.4.drv"),
            "ollama-0.5.4"
        );
        assert_eq!(
            store_path_label("/nix/store/44444444444444444444444444444444-ollama-0.5.4.drv^out"),
            "ollama-0.5.4"
        );
    }
}

#[cfg(test)]
mod seed_tests {
    use super::*;

    #[test]
    fn shell_quote_wraps_and_escapes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("https://a/b?c=d&e"), "'https://a/b?c=d&e'");
        // A single quote has to terminate, escape, and reopen — otherwise
        // a crafted cache URL would break out of the remote script.
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
    }

    /// The strongest check available: hand the quoted form to a real
    /// `sh` and confirm it round-trips to the original byte for byte.
    /// Cache URLs and store paths end up inside a remote shell script,
    /// so a quoting bug here is a command-injection bug.
    #[test]
    fn shell_quote_round_trips_through_sh() {
        for input in [
            "plain",
            "https://cache.nixos-cuda.org",
            "/nix/store/aaa-foo-1.0",
            "it's",
            "$(touch /tmp/deptui-should-not-exist)",
            "`id`",
            "a b\tc",
            "semi; echo pwned",
            "quote\"inside",
            "back\\slash",
        ] {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", shell_quote(input)))
                .output()
                .expect("run sh");
            assert!(out.status.success(), "sh rejected {input:?}");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                input,
                "quoting did not round-trip for {input:?}",
            );
        }
        assert!(
            !std::path::Path::new("/tmp/deptui-should-not-exist").exists(),
            "command substitution escaped the quoting",
        );
    }

    /// The remote script must attempt each path at most once per run, or
    /// `copied` over-counts and `missed` reads as zero when it isn't.
    #[test]
    fn seed_script_breaks_after_the_first_cache_that_has_a_path() {
        // Stand in for `nix` with a shim that only "has" the second
        // cache, and check exactly one OK is emitted for one path.
        let dir = tempfile::TempDir::new().unwrap();
        let shim = dir.path().join("nix");
        std::fs::write(
            &shim,
            "#!/bin/sh\n# succeed only for cache B\nfor a in \"$@\"; do \
[ \"$a\" = \"https://b\" ] && exit 0; done\nexit 1\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();

        let script = format!(
            "for p in {}; do for f in {} {}; do \
if nix copy --from \"$f\" \"$p\" >/dev/null 2>&1; then echo {SEED_OK}; break; fi; \
done; done",
            shell_quote("/nix/store/x"),
            shell_quote("https://b"),
            shell_quote("https://b"),
        );
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    dir.path().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .output()
            .unwrap();
        let hits = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.trim() == SEED_OK)
            .count();
        assert_eq!(hits, 1, "one path in two matching caches must report once");
    }

    #[tokio::test]
    async fn seeding_is_a_noop_without_caches_or_paths() {
        let node = Node {
            name: "h".into(),
            hostname: "h.lan".into(),
            ssh_user: None,
            profiles: BTreeMap::new(),
            profiles_order: None,
        };
        let askpass = AskpassEnv {
            script_path: "/dev/null".into(),
            socket_path: "/dev/null".into(),
        };
        let (tx, _rx) = mpsc::channel(4);
        // No substituters → nothing to do, and crucially no ssh at all.
        let out = seed_substituters(
            &node,
            &SshOverride::default(),
            &askpass,
            &[],
            &[],
            &["/nix/store/x".into()],
            tx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(out, SeedOutcome::default());

        // No paths → likewise.
        let out = seed_substituters(
            &node,
            &SshOverride::default(),
            &askpass,
            &["https://cache".into()],
            &[],
            &[],
            tx,
        )
        .await
        .unwrap();
        assert_eq!(out, SeedOutcome::default());
    }

    #[test]
    fn seedable_paths_covers_builds_then_fetches() {
        let plan = BuildPlan {
            to_build: vec!["/nix/store/a-foo.drv".into()],
            build_outputs: vec!["/nix/store/b-foo".into()],
            to_fetch: vec!["/nix/store/c-bar".into()],
            ..Default::default()
        };
        assert_eq!(
            plan.seedable_paths(),
            vec!["/nix/store/b-foo", "/nix/store/c-bar"]
        );
    }
}

#[cfg(test)]
mod substituter_tests {
    use super::*;

    fn cfg(subs: &[&str], keys: &[&str], trusted: &[&str]) -> NixSubstituterConfig {
        NixSubstituterConfig {
            substituters: subs.iter().map(|s| s.to_string()).collect(),
            trusted_public_keys: keys.iter().map(|s| s.to_string()).collect(),
            trusted_users: trusted.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn parses_nix_config_show() {
        let text = "\
experimental-features = nix-command flakes
substituters = https://cache.nixos.org/ https://nix-community.cachix.org
trusted-public-keys = cache.nixos.org-1:abc= nix-community.cachix.org-1:def=
trusted-users = root @wheel
max-jobs = 8
";
        let c = parse_nix_config_show(text);
        assert_eq!(
            c.substituters,
            vec![
                "https://cache.nixos.org/",
                "https://nix-community.cachix.org"
            ]
        );
        assert_eq!(c.trusted_users, vec!["root", "@wheel"]);
        assert_eq!(c.trusted_public_keys.len(), 2);
    }

    #[test]
    fn parse_ignores_unknown_and_malformed_lines() {
        let c = parse_nix_config_show("garbage\nsubstituters = https://a\n\nfuture-setting = x");
        assert_eq!(c.substituters, vec!["https://a"]);
    }

    #[test]
    fn trusted_substituters_count_as_usable() {
        let c = parse_nix_config_show("substituters = https://a\ntrusted-substituters = https://b");
        assert_eq!(c.substituters, vec!["https://a", "https://b"]);
    }

    #[test]
    fn trailing_slash_is_not_drift() {
        // `nix config show` prints the canonical trailing-slash form; a
        // config that omits it is the same cache.
        let declared = cfg(&["https://cache.nixos.org"], &[], &[]);
        let current = cfg(&["https://cache.nixos.org/"], &[], &[]);
        let d = compute_substituter_drift(BuildSite::Remote, &declared, &current);
        assert!(!d.has_drift(), "{d:?}");
    }

    #[test]
    fn detects_an_added_cache_and_key() {
        let declared = cfg(
            &["https://cache.nixos.org/", "https://cache.nixos-cuda.org"],
            &["cache.nixos.org-1:abc=", "cuda-1:xyz="],
            &[],
        );
        let current = cfg(
            &["https://cache.nixos.org/"],
            &["cache.nixos.org-1:abc="],
            &[],
        );
        let d = compute_substituter_drift(BuildSite::Remote, &declared, &current);
        assert!(d.has_drift());
        assert_eq!(d.added_substituters, vec!["https://cache.nixos-cuda.org"]);
        assert_eq!(d.added_keys, vec!["cuda-1:xyz="]);
        assert!(d.removed_substituters.is_empty());
    }

    #[test]
    fn reports_removed_caches_separately() {
        let declared = cfg(&["https://a"], &[], &[]);
        let current = cfg(&["https://a", "https://b"], &[], &[]);
        let d = compute_substituter_drift(BuildSite::Local, &declared, &current);
        assert!(!d.has_drift());
        assert_eq!(d.removed_substituters, vec!["https://b"]);
    }

    #[test]
    fn added_entries_are_deduplicated() {
        let declared = cfg(&["https://new", "https://new/"], &[], &[]);
        let current = cfg(&["https://old"], &[], &[]);
        let d = compute_substituter_drift(BuildSite::Local, &declared, &current);
        assert_eq!(d.added_substituters, vec!["https://new"]);
    }

    #[test]
    fn root_is_always_trusted() {
        assert!(user_is_trusted(&[], "root", &[]));
    }

    #[test]
    fn trusted_users_matches_bare_name_and_group() {
        let trusted = vec!["root".to_string(), "@wheel".to_string()];
        assert!(user_is_trusted(&trusted, "alice", &["wheel".to_string()]));
        assert!(!user_is_trusted(&trusted, "alice", &["users".to_string()]));
        assert!(user_is_trusted(
            &["alice".to_string()],
            "alice",
            &["users".to_string()]
        ));
    }

    #[test]
    fn untrusted_user_is_the_silent_failure_case() {
        // Regression guard for the documented trap: a non-trusted ssh
        // user makes nix ignore substituter overrides without warning.
        let trusted = vec!["root".to_string()];
        assert!(!user_is_trusted(&trusted, "deploy", &["users".to_string()]));
    }
}
