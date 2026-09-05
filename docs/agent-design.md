# deptui-agent — settled design

Outcome of the design-grilling session on 2026-09-04. Every decision below
was explicitly confirmed; treat this as the contract for implementation.

**Status: implemented.** The workspace split, the agent (daemon, API,
CLI, notifications, offline catch-up), the TUI integration (agent view
on `a`, badges, confirm-popup pause), and the packaging
(`packages.deptui-agent`, `nixosModules.deptui-agent`,
`contrib/deptui-agent.service`) all landed. This document remains the
rationale record; CLAUDE.md carries the working invariants.

## Shape

- Cargo **workspace**, three crates, lockstep versioning, one Cargo.lock:
  - `deptui-core` — deploy runner, flake discovery, host probes, ssh.
    Extracted from the existing `deploy.rs` / `flake.rs` / `host.rs` /
    `probe.rs` / `ssh.rs` (all already headless and channel-based).
    Serde `Serialize` derives added to core types (`Report`, `Node`,
    `BuildPlan`, `PkgDiff`, `SshOverride`, …) for the API.
  - `deptui` — the existing ratatui frontend.
  - `deptui-agent` — the daemon.
- **Repo renamed `deploy-rs-tui` → `deptui`** (GitHub rename + local dir +
  flake description + README/CLAUDE.md/CI references), as its own
  housekeeping commit *before* the workspace split so the split diff stays
  readable. `deptui-core` is an internal seam, never published separately.

## Agent behavior

- **Long-running daemon** (`deptui-agent run`), internal scheduler.
  Cadence per watch: `interval = "15m"` *or* a cron expression
  (`cron` crate) — mutually exclusive keys. No systemd-calendar syntax.
- **Watches are a list.** Each watch: repo (remote URL or local path),
  ref (branch head or a *moving named tag*; pattern/semver tags are a
  future extension), host set, per-host deploy-rs flags.
- Update detection = `git ls-remote` head comparison. Deploys run from the
  agent's **private clone** at the detected commit — never from a human's
  working tree, never dirty-tree-triggered.
- Hosts deploy **sequentially**. Updates arriving mid-run **coalesce to the
  newest** head. **No same-commit retry**: a failed host is parked
  (failed-at-C) until a new commit, a kick, or a force-deploy.
- **Offline catch-up** (per-host `catch_up`, default on): before each
  deploy the runner probes the target (BatchMode ssh). A host that is
  down gets outcome `offline` — a *pending update*, not a parked
  failure. The daemon re-probes the stored ssh target at the watch's
  `offline_recheck` cadence (default 2m) and, the moment the host
  answers, triggers a "catch-up" poll so normal eligibility deploys the
  coalesced newest revision. Markers persist in state, so rechecks
  resume across agent restarts. `catch_up = false` restores
  attempt-and-park.
  `auto-rollback` / `magic-rollback` follow deploy-rs defaults but are
  exposed as per-host settings. `interactive_sudo` is **rejected** in agent
  config (headless — no PTY/askpass path).
- **State**: JSON under `/var/lib/deptui-agent` (systemd `StateDirectory=`,
  `--state-dir` for manual runs). Schema-versioned; deletable at worst cost
  of one redundant check. Holds last-seen/last-deployed/failed-at per
  host, pause flags (runtime state that overlays config and survives
  restarts), and history: last ~50 runs per watch, per-run log capped at
  2000 lines (matching the TUI cap).
- **Startup/reload validation**: check each target's non-interactive
  reachability; on failure warn + mark unreachable + notify, keep running.
  `--validate` mode exits non-zero for CI/module assertions.
- `deptui-agent check [--watch NAME] [--once]` — oneshot poll+deploy escape
  hatch for cron/timer purists, no daemon required.

## API & control plane

- **HTTP/JSON on a Unix socket** `/run/deptui-agent/agent.sock`
  (`RuntimeDirectory=`, mode 0660, group-gated). Server: axum (SSE built
  in, Unix-socket support).
- Endpoints (names bikesheddable): `GET /status`, `GET /history`,
  `GET /log/tail` (SSE), `POST /kick` (`?watch=`), `POST /pause` /
  `POST /resume` (global / `?host=` / `?watch=`), `POST /deploy` (force one
  host).
- **Optional TCP listener** exposes *only* kick + status, bearer-token
  gated (`tokenFile`, secret never in config/store). Worst-case token leak
  = attacker triggers a check of a repo already trusted. Kick is a pure
  "check now" — the API never names refs/commits (no command API in v1).
- Config file (TOML) is the source of truth; **no remote config editing**
  (NixOS-managed config is store-read-only anyway). Remote control =
  runtime state only: pause/resume, kick, force-deploy, status/history/tail.

## Identity & privileges

- Default: dedicated **`deptui-agent` system user** + group; humans who may
  control the agent join the group (module sugar: `users = [...]`).
  Config/module option to run as an **existing user** instead for the
  simple reuse-my-account case.
- Own ssh key (path option; agenix/sops-friendly). Targets must accept
  **non-interactive activation** (root deploy user or NOPASSWD sudo) —
  documented requirement, validated at startup. Private repos: plain
  git-over-ssh via the agent user's ssh config; nothing custom.
- No local root needed: nix-daemon does the building.

## Notifications

1. TUI surfaces agent state (badges + failure notice; see below).
2. `on_failure = "cmd {host} {rev} …"` hook command (spawn, substitution
   vars) — the user plugs in ntfy/sendmail/anything.
3. Built-in webhook: **ntfy** (title/body/priority) + **generic JSON POST**
   (documented schema: event, watch, host, rev, outcome, message).
   Failure always; start/success opt-in. No per-platform Slack/Discord
   formatters, no email pipeline.

## TUI integration

- deptui grows client-side persistence: `~/.config/deptui/config.toml`
  (XDG) with **named agents** (`[agents.NAME] ssh = "user@host"`), one
  default. (Door open for later persisting ssh overrides etc. — out of
  scope here.)
- Transport: **`ssh <host> deptui-agent <verb> --json`** exec (not socket
  forwarding); log tail = `deptui-agent tail` streaming NDJSON over the
  same ssh exec. The CLI verbs double as the scripting interface and the
  GH-Action-over-ssh interface.
- **`a`** opens a full-screen agent view (screen swap, not a pane).
  To free `a`, profile selection changes shape: **`s` and `h` become
  independent toggles** (system on/off, home on/off) instead of the
  tri-state `s`/`h`/`a` selection — both on covers what `a` (all) did;
  toggling the last one off is refused (at least one profile stays
  selected). Agent view:
  connection status, per-watch ref/last-seen commit, per-host last
  deploy/state (ok/failed/paused), pause/resume/kick/force-deploy keys,
  log tail reusing the job-log rendering machinery. Picker line at top when
  several agents are configured.
- Ambient: agent-managed hosts get a distinct **glyph** badge in the host
  row (glyph before colour, per house rules), red/`!` variant on agent
  failure; one-line failure notice in the info row.
- **Manual-deploy race (Q28 = b)**: starting a manual deploy of an
  agent-managed host warns and offers a one-key "pause agent" in the
  confirm popup. No hard lock, no routing manual deploys through the agent.

## Packaging

- Flake outputs: `packages.deptui` (default), `packages.deptui-agent`
  (wrapped with deploy-rs, nix, openssh, **git** — git is a new runtime
  dep, the codebase's first git shell-out), `nixosModules.deptui-agent`.
- Module shape: RFC-42 — `enable`, freeform `settings` via
  `pkgs.formats.toml` (every TOML key has a Nix equivalent by
  construction), typed `watches.<name>.…` merged into settings, plus
  `tokenFile`, `sshKeyFile`, `openFirewall` (default false), user/group
  overrides. Module writes TOML to the store, passes `--config`; secrets
  only ever by file path.
- Plain `.service` file shipped for non-NixOS. CI builds both packages.
- README gets an example GitHub Actions kick snippet (curl the TCP kick
  endpoint, or ssh + `deptui-agent kick`); **no** published marketplace
  Action.
