//! Update detection and checkout management.
//!
//! Detection is a bare `git ls-remote` — no clone needed to notice a
//! head move. Deploys never run from a human's working tree: the agent
//! keeps its own clone per watch under `<state_dir>/clones/<name>` and
//! checks out the exact detected commit, detached.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::process::Command;

use crate::config::WatchConfig;

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

/// Make `<state_dir>/clones/<name>` an up-to-date, *prepared* checkout
/// of `rev` and return its path. Clones on first use; fetches and
/// detaches otherwise; then applies the watch's repo preparations —
/// the git-crypt unlock and the `post_checkout` hook — so every caller
/// (runner, oneshot check, validate) gets an identical working tree.
pub async fn ensure_checkout(state_dir: &Path, watch: &WatchConfig, rev: &str) -> Result<PathBuf> {
    let dir = clone_dir(state_dir, &watch.name);
    let repo = &watch.repo;
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
    // An already-unlocked clone smudges during the checkout below via
    // the filter config git-crypt wrote — which records the *absolute
    // path* of the binary that unlocked. Re-pin it to PATH resolution
    // first, or a GC'd store path there fails every later checkout.
    let unlocked = |dir: &Path| dir.join(".git/git-crypt/keys/default").exists();
    if watch.git_crypt_key_file.is_some() && unlocked(&dir) {
        repin_git_crypt_filters(&dir).await?;
    }
    git(
        Some(&dir),
        &["checkout", "--quiet", "--force", "--detach", rev],
    )
    .await
    .with_context(|| format!("checking out {rev} in {}", dir.display()))?;
    if let Some(key) = &watch.git_crypt_key_file {
        if !unlocked(&dir) {
            if !key.exists() {
                bail!(
                    "git_crypt_key_file {} does not exist (is the secret provisioned on \
                     this host, readable by the agent user?)",
                    key.display()
                );
            }
            git_crypt(&dir, &["unlock", &key.to_string_lossy()])
                .await
                .with_context(|| {
                    format!(
                        "unlocking git-crypt in {} (the key must be an exported symmetric \
                         key from `git-crypt export-key`; GPG mode is not supported — the \
                         agent is headless)",
                        dir.display()
                    )
                })?;
            repin_git_crypt_filters(&dir).await?;
        }
    }
    if let Some(cmd) = &watch.post_checkout {
        run_post_checkout(&dir, cmd).await?;
    }
    Ok(dir)
}

/// Point the git-crypt filter config at plain `git-crypt`, resolved
/// via PATH (the agent's wrapper provides it), instead of whatever
/// absolute path the unlock recorded.
async fn repin_git_crypt_filters(dir: &Path) -> Result<()> {
    for (key, value) in [
        ("filter.git-crypt.smudge", "\"git-crypt\" smudge"),
        ("filter.git-crypt.clean", "\"git-crypt\" clean"),
        ("diff.git-crypt.textconv", "\"git-crypt\" diff"),
    ] {
        git(Some(dir), &["config", key, value]).await?;
    }
    Ok(())
}

/// Run git-crypt inside the clone (it has no `-C`; cwd is the repo).
async fn git_crypt(dir: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git-crypt");
    cmd.current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = cmd
        .output()
        .await
        .context("spawning git-crypt (is it installed alongside the agent?)")?;
    if !out.status.success() {
        bail!(
            "git-crypt {} failed ({}): {}",
            args.first().copied().unwrap_or(""),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The watch's `post_checkout` hook, in the fresh checkout. Headless
/// like every other child: prompts fail fast, a hang is killed at the
/// timeout, stderr surfaces in the error.
async fn run_post_checkout(dir: &Path, cmd: &str) -> Result<()> {
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("SSH_ASKPASS", "/bin/false");
    let out = tokio::time::timeout(Duration::from_secs(600), c.output())
        .await
        .map_err(|_| anyhow!("post_checkout hook timed out after 10 minutes: {cmd}"))?
        .context("spawning post_checkout hook")?;
    if !out.status.success() {
        bail!(
            "post_checkout hook failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Is `ancestor` reachable from `descendant` in the watch's clone?
/// Backs the drift guard's escape hatch: a host manually deployed to a
/// commit that IS in the watched history is a legitimate deploy, not
/// work to protect. Any failure — rev unknown to the clone, not a
/// commit, git error — is `false`: unverifiable means hold.
pub async fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> bool {
    git(
        Some(dir),
        &["merge-base", "--is-ancestor", ancestor, descendant],
    )
    .await
    .is_ok()
}

/// Run git, surfacing stderr in the error (house rule: never swallow a
/// child's stderr).
async fn git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.arg("-C").arg(d);
    } else {
        // Anchor repo-less invocations (ls-remote, clone) at / — git
        // walks the cwd looking for a repository even for remote-only
        // commands, and `sudo -u deptui-agent` from someone's
        // unreadable home dir made that walk fail before the network
        // was ever touched.
        cmd.current_dir("/");
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
        let wcfg: WatchConfig = toml::from_str(&format!(
            "name = \"w\"\nrepo = \"{rp}\"\nbranch = \"main\"\ninterval = \"1h\"\n"
        ))
        .unwrap();

        let rev1 = ls_remote(&rp, "refs/heads/main").await.unwrap().unwrap();
        let dir = ensure_checkout(state.path(), &wcfg, &rev1).await.unwrap();
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

        let dir = ensure_checkout(state.path(), &wcfg, &rev2).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("f")).unwrap().trim(),
            "two"
        );

        // post_checkout runs inside the fresh checkout; a failing hook
        // fails the preparation with its stderr attached.
        let hooked: WatchConfig = toml::from_str(&format!(
            "name = \"h\"\nrepo = \"{rp}\"\nbranch = \"main\"\ninterval = \"1h\"\n\
             post_checkout = \"touch hook-ran\"\n"
        ))
        .unwrap();
        let hdir = ensure_checkout(state.path(), &hooked, &rev2).await.unwrap();
        assert!(hdir.join("hook-ran").exists(), "hook must run in the clone");
        let failing: WatchConfig = toml::from_str(&format!(
            "name = \"fh\"\nrepo = \"{rp}\"\nbranch = \"main\"\ninterval = \"1h\"\n\
             post_checkout = \"echo boom >&2; exit 3\"\n"
        ))
        .unwrap();
        let err = ensure_checkout(state.path(), &failing, &rev2)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("boom"), "{err:#}");

        // Unknown ref is None, not an error.
        assert!(ls_remote(&rp, "refs/heads/nope").await.unwrap().is_none());

        // Ancestry, in the agent's clone: rev1 precedes rev2, a rev is
        // its own ancestor, and garbage is unverifiable → false.
        assert!(is_ancestor(&dir, &rev1, &rev2).await);
        assert!(is_ancestor(&dir, &rev2, &rev2).await);
        assert!(!is_ancestor(&dir, &rev2, &rev1).await);
        assert!(!is_ancestor(&dir, "deadbeef", &rev2).await);
    }
}
