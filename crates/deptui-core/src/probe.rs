//! Background probe orchestration.
//!
//! One interface for every host probe: pick a [`Kind`], hand it a
//! [`Ctx`] and a channel, and [`spawn`] returns one tokio task that
//! reports zero or more [`Report::Progress`] lines followed by exactly
//! one final typed [`Report`]. The plumbing every probe needs — the
//! progress channel, the forwarder task that relays progress into the
//! app's status channel, the drain-before-verdict ordering, and the
//! error stringification — lives here once. `app.rs` decides *when* to
//! probe and *what to do* with the result; `host.rs` owns *how* each
//! check talks to `nix`/`ssh`.
//!
//! The channel is the seam: `App` unit tests exercise the result
//! handling by constructing `Report` values directly, without any
//! process being spawned.

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::askpass::AskpassEnv;
use crate::flake::Node;
use crate::host::{
    self, BuildPlan, BuildSite, PkgDiff, ProfileCheck, ProgressLine, Reachability, SubstituterDrift,
};
use crate::ssh::SshOverride;

/// Everything a probe needs to reach its host. One clone per spawned
/// probe — the values are snapshots, deliberately detached from `App`
/// state so a probe in flight is unaffected by later edits.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub flake: String,
    pub node: Node,
    pub override_: SshOverride,
    pub askpass: AskpassEnv,
}

/// Which check to run. Each variant carries exactly the inputs its
/// check needs beyond the shared [`Ctx`].
#[derive(Debug, Clone)]
pub enum Kind {
    /// TCP reachability of the node's SSH endpoint.
    Reachability,
    /// Cheap-tier update check for one profile (`u`).
    Update { profile: String },
    /// Medium-tier closure-size delta (`Shift+U`). Needs the paths a
    /// prior update check resolved.
    Size {
        profile: String,
        local_path: String,
        remote_path: String,
    },
    /// Expensive-tier package diff, auto-chained after a size probe.
    PkgDiff {
        profile: String,
        local_path: String,
        remote_path: String,
    },
    /// Build-plan preflight (`Shift+P`).
    BuildPlan { profile: String, site: BuildSite },
    /// Substituter-drift check (`Shift+C`).
    CacheDrift { site: BuildSite },
}

/// The one message shape every probe reports through. Progress lines
/// arrive first (already host-tagged); the final variant matches the
/// [`Kind`] that was spawned.
#[derive(Debug)]
pub enum Report {
    Reachability {
        node: String,
        result: Reachability,
    },
    Update {
        node: String,
        profile: String,
        result: Result<ProfileCheck, String>,
    },
    /// Closure-size result: `(local_bytes, remote_bytes)`.
    Size {
        node: String,
        profile: String,
        result: Result<(u64, u64), String>,
    },
    /// Package-diff result. An empty change list = closures identical.
    PkgDiff {
        node: String,
        profile: String,
        result: Result<PkgDiff, String>,
    },
    BuildPlan {
        node: String,
        profile: String,
        result: Result<BuildPlan, String>,
    },
    CacheDrift {
        node: String,
        result: Result<SubstituterDrift, String>,
    },
    /// Typed progress line from a long-running probe, forwarded into
    /// the host-tagged log so the user sees activity instead of a
    /// silent spinner. Carries its [`crate::host::LogKind`] so the
    /// renderer styles it without parsing.
    Progress {
        node: String,
        line: ProgressLine,
    },
}

/// Spawn one probe. The returned handle is the whole lifetime of the
/// probe — aborting it (or dropping it with `kill_on_drop` children
/// inside) cancels the underlying `nix`/`ssh` processes.
///
/// `U: From<Report>` lets the caller receive reports on a channel of
/// its own wider message type without a relay task in between.
pub fn spawn<U>(kind: Kind, ctx: Ctx, tx: mpsc::Sender<U>) -> JoinHandle<()>
where
    U: From<Report> + Send + 'static,
{
    tokio::spawn(async move {
        let (prog_tx, mut prog_rx) = mpsc::channel::<ProgressLine>(64);
        let progress_tx = tx.clone();
        let progress_node = ctx.node.name.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(line) = prog_rx.recv().await {
                let _ = progress_tx
                    .send(
                        Report::Progress {
                            node: progress_node.clone(),
                            line,
                        }
                        .into(),
                    )
                    .await;
            }
        });
        let report = run(kind, &ctx, prog_tx).await;
        // Drain the progress lines before publishing the verdict so
        // they read in order — the closing "[pkg] done" line lands
        // before the inline result snaps into place.
        let _ = forwarder.await;
        let _ = tx.send(report.into()).await;
    })
}

/// Run the check a [`Kind`] names and wrap its outcome as a [`Report`].
/// Checks that take no progress channel drop theirs immediately so the
/// forwarder in [`spawn`] terminates.
async fn run(kind: Kind, ctx: &Ctx, progress: mpsc::Sender<ProgressLine>) -> Report {
    let node = ctx.node.name.clone();
    match kind {
        Kind::Reachability => {
            drop(progress);
            let result = host::check_online(&ctx.node.hostname, &ctx.override_).await;
            Report::Reachability { node, result }
        }
        Kind::Update { profile } => {
            drop(progress);
            let result = host::check_profile_up_to_date(
                &ctx.flake,
                &ctx.node,
                &profile,
                &ctx.override_,
                &ctx.askpass,
            )
            .await
            .map_err(stringify);
            Report::Update {
                node,
                profile,
                result,
            }
        }
        Kind::Size {
            profile,
            local_path,
            remote_path,
        } => {
            let result = host::check_closure_sizes(
                &host::ProfileProbe {
                    flake: &ctx.flake,
                    node: &ctx.node,
                    profile: &profile,
                    local_path: &local_path,
                    remote_path: &remote_path,
                    override_: &ctx.override_,
                    askpass: &ctx.askpass,
                },
                progress,
            )
            .await
            .map_err(stringify);
            Report::Size {
                node,
                profile,
                result,
            }
        }
        Kind::PkgDiff {
            profile,
            local_path,
            remote_path,
        } => {
            let result = host::check_package_diff(
                &host::ProfileProbe {
                    flake: &ctx.flake,
                    node: &ctx.node,
                    profile: &profile,
                    local_path: &local_path,
                    remote_path: &remote_path,
                    override_: &ctx.override_,
                    askpass: &ctx.askpass,
                },
                progress,
            )
            .await
            .map_err(stringify);
            Report::PkgDiff {
                node,
                profile,
                result,
            }
        }
        Kind::BuildPlan { profile, site } => {
            let result = host::check_build_plan(
                &ctx.flake,
                &ctx.node,
                &profile,
                site,
                &ctx.override_,
                progress,
            )
            .await
            .map_err(stringify);
            Report::BuildPlan {
                node,
                profile,
                result,
            }
        }
        Kind::CacheDrift { site } => {
            let result = host::check_substituter_drift(
                &ctx.flake,
                &ctx.node,
                site,
                &ctx.override_,
                &ctx.askpass,
                progress,
            )
            .await
            .map_err(stringify);
            Report::CacheDrift { node, result }
        }
    }
}

/// Errors cross the channel as display strings — the UI renders them,
/// it never matches on them.
fn stringify(e: anyhow::Error) -> String {
    format!("{e:#}")
}
