# Changelog

All notable changes to deptui are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions
follow [SemVer](https://semver.org) with pre-1.0 semantics (minor =
features, patch = fixes; breaking changes are called out explicitly).
The workspace version in `Cargo.toml` is the single source of truth —
the flake reads it, both binaries report it via `--version`, and every
release is tagged `vX.Y.Z`.

## [Unreleased]

## [0.2.0] — 2026-09-05

The auto-deploy agent release. deptui grows from a single-binary TUI
into a workspace shipping two binaries.

### Added

- **`deptui-agent`** — a daemon that watches git repositories (branch
  head or moving tag; interval or cron cadence) and deploys updates to
  configured hosts via deploy-rs, from its own private clone. Per-host
  deploy-rs flag overrides follow the TUI's only-emit-if-changed rule.
- Agent control API on a Unix socket (status, history, run log, SSE
  tail, kick, pause/resume, force-deploy, cancel) plus an optional
  token-gated TCP listener exposing only kick + status for CI.
- The agent binary is its own CLI client (`run`, `check`, `validate`,
  `status`, `history`, `log`, `kick`, `pause`, `resume`, `deploy`,
  `cancel`, `tail`) — `ssh host deptui-agent <verb> --json` is the
  remote-control transport the TUI uses.
- **Offline catch-up**: a host that is down when an update arrives is
  pending, not failed — the agent re-probes it (`offline_recheck`,
  default 2m) and deploys the moment it answers. `catch_up = false`
  per host opts out.
- **Cancel**: `deptui-agent cancel` / `POST /cancel` / `x` in the TUI
  agent view stops a run in flight (kills the deploy's process group)
  and parks its hosts at that revision. Pause acks now point at it.
- Failure notifications: `on_failure` hook command with shell-quoted
  substitution, plus built-in ntfy and generic-JSON webhooks
  (failure always; start/success opt-in).
- TUI agent integration: `a` opens a full-screen agent view (status,
  live log tail, kick/pause/cancel/force-deploy); `[agent]` /
  `[agent!]` / `[agent~]` host badges; a title-bar notice on agent
  deploy failures; the confirm-deploy popup warns about agent-managed
  hosts and offers a one-key agent pause.
- `~/.config/deptui/config.toml` — deptui's first client settings file
  (named agents, `default_agent`).
- Packaging: `packages.deptui-agent`, `nixosModules.deptui-agent`
  (freeform `settings` + typed watches/listen/sshKeyFile/user options),
  and `contrib/deptui-agent.service` for non-NixOS.

### Changed

- **Breaking (keybindings):** profile selection reshaped — `s` and `h`
  are now independent system/home toggles (both on = the old "all");
  `a` now opens the agent view instead of selecting all profiles.
- The crate is now a three-member workspace: `deptui-core` (headless
  deploy/probe machinery + agent wire types), `deptui` (TUI),
  `deptui-agent`.
- The job log now always shows untagged app-level messages (hints,
  cancellations, agent acks); they were previously filtered out and
  effectively invisible.

### Fixed

- Pressing `a` with no agent configured did nothing; it now opens the
  agent view with setup instructions and surfaces settings-file parse
  errors instead of silently falling back to empty settings.

## [0.1.0]

Initial release: the ratatui TUI around serokell/deploy-rs — flake
discovery, host reachability and update probes, closure-size and
package diffs, build-plan preflight, substituter-drift checking with
additive cache seeding, SSH overrides, askpass integration,
interactive sudo, batch deploys with cancellation, and NO_COLOR /
accessibility support.
