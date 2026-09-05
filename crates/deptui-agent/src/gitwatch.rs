//! Update detection and checkout management.
//!
//! Detection is a bare `git ls-remote` — no clone needed to notice a
//! head move. Deploys never run from a human's working tree: the agent
//! keeps its own clone per watch under `<state_dir>/clones/<name>` and
//! checks out the exact detected commit, detached.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

/// Resolve the commit a refspec currently points at, or `None` when the
/// ref doesn't exist. For tags, prefers the peeled `^{}` line so an
/// annotated tag resolves to the commit, not the tag object.
pub async fn ls_remote(repo: &str, refspec: &str) -> Result<Option<String>> {
    let out = git(
        None,
        &["ls-remote", repo, refspec, &format!("{refspec}^{{}}")],
    )
    .await
    .with_context(|| format!("polling {repo} for {refspec}"))?;
    let mut plain = None;
    let mut peeled = None;
    for line in out.lines() {
        let mut it = line.split_whitespace();
        let (Some(hash), Some(name)) = (it.next(), it.next()) else {
            continue;
        };
        if name == refspec {
            plain = Some(hash.to_string());
        } else if name == format!("{refspec}^{{}}") {
            peeled = Some(hash.to_string());
        }
    }
    Ok(peeled.or(plain))
}

/// Directory of the agent's private clone for a watch.
pub fn clone_dir(state_dir: &Path, watch: &str) -> PathBuf {
    state_dir.join("clones").join(watch)
}

/// Make `<state_dir>/clones/<watch>` an up-to-date checkout of `rev` and
/// return its path. Clones on first use; fetches and detaches otherwise.
pub async fn ensure_checkout(
    state_dir: &Path,
    watch: &str,
    repo: &str,
    rev: &str,
) -> Result<PathBuf> {
    let dir = clone_dir(state_dir, watch);
    if !dir.join(".git").exists() {
        std::fs::create_dir_all(dir.parent().unwrap())
            .with_context(|| format!("creating {}", dir.parent().unwrap().display()))?;
        git(
            None,
            &["clone", "--no-checkout", repo, &dir.to_string_lossy()],
        )
        .await
        .with_context(|| format!("cloning {repo}"))?;
    }
    // Fetch the specific commit when the server allows it; fall back to a
    // full ref fetch for servers without allow*SHA1InWant.
    if git(Some(&dir), &["fetch", "--quiet", "origin", rev])
        .await
        .is_err()
    {
        git(Some(&dir), &["fetch", "--quiet", "--tags", "origin"])
            .await
            .with_context(|| format!("fetching {repo}"))?;
    }
    git(
        Some(&dir),
        &["checkout", "--quiet", "--force", "--detach", rev],
    )
    .await
    .with_context(|| format!("checking out {rev} in {}", dir.display()))?;
    Ok(dir)
}

/// Run git, surfacing stderr in the error (house rule: never swallow a
/// child's stderr).
async fn git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.arg("-C").arg(d);
    }
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Never let git block on a credential prompt — the agent is
        // headless; a repo that needs interactive auth is a config error.
        .env("GIT_TERMINAL_PROMPT", "0");
    let out = cmd.output().await.context("spawning git")?;
    if !out.status.success() {
        bail!(
            "git {} failed ({}): {}",
            args.first().copied().unwrap_or(""),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end against a real local git repo: detect, checkout,
    /// detect the moved head, re-checkout.
    #[tokio::test]
    async fn detects_and_checks_out_local_repo() {
        let repo = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let rp = repo.path().to_string_lossy().to_string();
        let sh = |cmd: &str| {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(repo.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{cmd}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        sh("git init -q -b main . && echo one > f && git add f && git commit -qm one");

        let rev1 = ls_remote(&rp, "refs/heads/main").await.unwrap().unwrap();
        let dir = ensure_checkout(state.path(), "w", &rp, &rev1)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("f")).unwrap().trim(),
            "one"
        );

        sh("echo two > f && git add f && git commit -qm two && git tag -a prod -m p");
        let rev2 = ls_remote(&rp, "refs/heads/main").await.unwrap().unwrap();
        assert_ne!(rev1, rev2);
        // Annotated tag resolves to the commit (peeled), same as the branch.
        let tagrev = ls_remote(&rp, "refs/tags/prod").await.unwrap().unwrap();
        assert_eq!(tagrev, rev2);

        let dir = ensure_checkout(state.path(), "w", &rp, &rev2)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("f")).unwrap().trim(),
            "two"
        );

        // Unknown ref is None, not an error.
        assert!(ls_remote(&rp, "refs/heads/nope").await.unwrap().is_none());
    }
}
