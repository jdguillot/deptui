# Changelog

All notable changes to deptui are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions
follow [SemVer](https://semver.org) with pre-1.0 semantics (minor =
features, patch = fixes; breaking changes are called out explicitly).
The workspace version in `Cargo.toml` is the single source of truth —
the flake reads it, both binaries report it via `--version`, and every
release is tagged `vX.Y.Z`.

## [Unreleased]

### Added

- Getting-started polish from the validation walk: newest
  nixos-rebuild flags first (old spellings as comments), the sudo
  rationale as a proper admonition, "deploy the targets" shown with
  the Space-mark-then-Shift+S batch flow and shell commands, and the
  restart-once note moved to troubleshooting (it only applies when
  switching a long-running agent to the generated identity — a fresh
  install never needs it).

## [0.13.0] — 2026-09-06

### Fixed

- `deptui-agent pubkey` over ssh reported the *invoking user's*
  identity (or an error), not the agent's: the daemon now serves its
  public key in `/status` and the verb asks it first, falling back to
  local files only when no daemon answers — and tells you to restart
  the service once if the running daemon predates key generation.

### Added

- Getting-started hardening from a real fresh-workflow walk:
  a Prerequisites section (deploy-rs `deploy.nodes`, flakes, ssh),
  the restart-once note for agents that were already running before
  the update (activation deliberately doesn't restart them), and the
  nixos-rebuild `--ask-elevate-password` caveat — including that the
  hint also fires on any non-zero remote exit, with the two commands
  that show whether the switch actually landed.

### Added

- Getting-started: TUI-only flake install example, the agent
  explicitly marked optional, the `hosts.<name>` keys explained
  (they must match `deploy.nodes` names), and the sudo section now
  *explains* why store-path-scoped NOPASSWD is root anyway (any user
  can materialize a matching store path) and lays out the three
  two real configurations — interactive sudo is framed as the
  consequence of rejecting both (that host stays TUI-only), not as a
  third agent option. Mermaid diagrams for the topology (direct
  deploys vs agent mode) and the per-host poll decision flow, plus
  copy-paste nixos-rebuild/deploy commands for the agent host.

## [0.12.0] — 2026-09-06

### Changed

- `services.deptui-agent.watches` is now properly typed: watches and
  their hosts are option submodules — documented, defaulted,
  type-checked at eval, and mergeable across modules — with a
  freeform fallback so schema additions still pass through. Existing
  freeform definitions keep working; unset options are scrubbed
  before TOML generation.

### Added

- docs: the NixOS sudoers recipe for targets, and why deptui ships
  no option for it (wrong machine, and store-path scoping is
  security theater — the grant belongs where you can see it).

### Added

- `docs/getting-started.md`: the new-user walkthrough — TUI in one
  command, the agent's three-step trust setup, adoption/approval
  semantics, notifications, CI kicks, and a troubleshooting table.

## [0.11.0] — 2026-09-05

### Added

- Zero-secret agent identity: with no `sshKeyFile`, the NixOS module
  generates an ed25519 keypair in the agent's state dir on first
  start (`generateSshKey`, default on, dedicated user only) — the
  private half never leaves the machine, so no sops plumbing at all.
  `deptui-agent pubkey` prints the half you authorize on targets (and
  names the passphrase trap when pointed at a locked key).
- `hostKeyChecking` module option, default `accept-new`: targets are
  trusted on first contact and pinned thereafter — no manual
  known_hosts step; changed keys are still rejected. `strict`
  restores require-pre-pinning for fleets that pin declaratively.
- `validate` now diagnoses the local identity first (missing /
  passphrase-protected / ok, printing the public key to authorize),
  so a broken key names itself instead of failing every probe with
  "Permission denied".
- README: the three-step identity quickstart.

## [0.10.2] — 2026-09-05

### Fixed

- `deptui-agent validate`/`check` over ssh work out of the box: the
  NixOS module links the generated config at
  `/etc/deptui-agent/config.toml` (the CLI's default path), and git
  invocations without a repo anchor at `/` so `sudo -u deptui-agent`
  from an unreadable home directory can't break `ls-remote` before
  the network is touched.

## [0.10.1] — 2026-09-05

### Fixed

- A passphrase-protected `sshKeyFile` made the agent silently useless
  (headless ssh skips the prompt; every host fails with a bare
  "Permission denied"). The module now checks the key at service
  start and logs a loud, actionable warning naming the fix
  (`ssh-keygen -p -N ""`) — without blocking the service, since the
  control API is still worth serving.

## [0.10.0] — 2026-09-05

### Added

- Run separators in both logs: each confirmed TUI deploy opens with a
  `━━ deploy <host> — <mode>/<profiles> ━━` header (a typed
  `LogKind::RunStart`, always visible), and each agent run starts
  with a rule line — a failed run and its retry no longer read as one
  stream.
- deploy-rs's definitive per-node verdict ("Deployment to node X
  failed, rolled back…") flips the TUI to failed *immediately* — the
  title says FAILED (rolled back) and offers `x` to skip the rest of
  the confirmation window deploy-rs insists on waiting out before
  exiting.
- The quiet stretch after a failed activation explains itself: the
  TUI announces when deploy-rs arms magic rollback ("waits its
  confirm-timeout…") and when activation errors first appear ("final
  failed status lands after the confirm-timeout") — once per host,
  so the wait before the official failure is no longer a mystery.
- Space-marked hosts in the agent view now show a `+` mark column and
  a `[N marked]` title count (the marking worked; the missing
  feedback made it look broken).
- `[profile.dev] debug = "line-tables-only"`: file:line backtraces
  kept, full DWARF dropped — roughly halves `target/` on disk and
  speeds links.

### Changed

- Main-screen job-log hints compacted (`v/V char/line select`),
  matching the agent footer's grouping.

## [0.9.1] — 2026-09-05

### Fixed

- Opening the agent view could fire an unanswerable storm of
  ssh-agent (e.g. 1Password) authorization prompts: discovery, status,
  tail, and backfill all connected in parallel, and the 5s
  auto-refresh re-fired on every failure. All agent-client ssh now
  runs through a single gate (one connection authenticates at a
  time), connections multiplex via ControlMaster/ControlPersist so
  the agent is asked once per host per minute instead of per command,
  and the auto-refresh backs off to 20s while the agent is
  unreachable.
- Offline hosts render fully greyed in the agent view — a sleeping
  host is context, not a call to action.
- Failure wording softened: "cancelled <rev>" (yellow) for
  user-cancelled runs, "failed <rev> — <reason>" (red) otherwise; no
  more shouting FAILED at a host that is merely off the watched
  branch's history.
- A past failure or cancel no longer strips a host's first-encounter
  hold protection: unadopted means "never *successfully* deployed",
  so a cancelled rollback attempt re-probes (and holds) on the next
  revision instead of blind-deploying.

## [0.9.0] — 2026-09-05

### Added

- The agent log now has full job-log parity — it *is* the job-log
  component, swapped in while the view is open: filter (selecting a
  host filters to it; `Space` marks several; watch-level lines always
  show), search (`/`, `n`/`N`), visual selection and yank (`v`/`V`,
  `y`), scrolling (`j`/`k`, `g`/`G`, wheel), drag-to-copy, and the
  search/visual/scroll title chips. `Tab` moves focus between the
  watches pane and the log. On open the log backfills from the
  agent's stored run history (capped at the usual 2000 lines), so
  search and yank cover past runs.

### Changed

- Approval moved from `y` to `Enter` in the agent view (`y` is yank
  now, matching the main screen).

### Fixed

- v0.8.0's agent view still sent the removed `deploy` verb from its
  approve key — the key was dead. (Another silently-missed patch,
  found while rewriting the handler.)

## [0.8.0] — 2026-09-05

### Changed

- **Breaking:** the agent's force-deploy is gone (`deploy` verb,
  `POST /deploy`, `d` in the TUI view). The agent schedules; immediate
  deploys belong to the TUI's main screen. In its place: **approval**
  — `deptui-agent approve HOST [--revoke]` / `POST /approve` / `y` in
  the view (with an explicit warning + confirm) marks a held or
  unadopted host as ok to take the *next* update round, accepting
  that this moves it off any generation made outside the watched
  repo. Approval is revocable until consumed by the first successful
  deploy. Approved hosts show `↑` in the view.

### Fixed

- **Erratum for 0.7.0:** the "no startup poll" change was in that
  release's notes but not in its binary (a botched patch); the agent
  still polled ~5s after start. It is actually fixed now, with the
  e2e test tightened to catch it.

## [0.7.1] — 2026-09-05

### Fixed

- The agent view kept saying "deploying" after a run had finished: it
  now auto-refreshes its status every 5s while open, the running chip
  says "running" (a first-encounter run may only probe and hold), it
  animates with the same spinner as the main screen, and run
  summaries count every outcome ("1 held", not "0 ok, 0 failed").
- Agent-view styling matches the rest of the app: focus-coloured
  watches border, key-coloured bordered footer, and semantic colours
  per host state (deployed green, FAILED red, HELD/offline yellow).
  The `[/]` agent-cycling hint only shows when more than one agent is
  configured — with a single agent the keys do nothing.

## [0.7.0] — 2026-09-05

### Changed

- **First encounters adopt instead of deploying.** A fresh agent used
  to treat every host as "needs deploy" and would happily roll a host
  *backwards* to a stale repo the moment it started. Now a host the
  agent has never deployed is probed: already running the watched
  revision → adopted silently; anything else → held + notified, until
  a force-deploy (or a matching revision) appears. Per-host
  `bootstrap = "deploy"` restores pure-GitOps first-run deploys. Held
  hosts show as `HELD` in status, `≠` in the agent view, and
  `[agent≠]` in the host list.
- **Starting the agent no longer triggers an immediate poll.** The
  first poll follows the configured cadence; only the schedule, kicks,
  and offline catch-up start runs.

## [0.6.2] — 2026-09-05

### Fixed

- Self-deploy no longer kills the agent: an agent deploying its own
  host was stopped by its own activation the moment the update
  changed deptui-agent.service — run, activation start-phase, and
  deploy-rs confirmation died with it, and the clean exit meant
  systemd never restarted it. The module now sets `restartIfChanged =
  false` (new option `restartOnUpdate` to opt back in); the running
  agent picks up its new version on the next explicit restart or
  reboot, and the run log flags deploys that target the agent's own
  host.

## [0.6.1] — 2026-09-05

### Fixed

- An agent with `enable = true` but no watches configured yet
  crash-looped under systemd ("configures no watches — nothing to
  do") — which also made it undiscoverable, since the socket never
  existed. The daemon now starts, warns, and serves its control API
  with zero watches (install first, configure later); the oneshot
  `check`/`validate` verbs still refuse.

## [0.6.0] — 2026-09-05

### Added

- Drag-to-copy: dragging across the job log (or the agent view's
  watches and live-log panes) highlights the rendered cells and puts
  them on the clipboard on release — exactly what's on screen, so
  errors can be pasted instead of screenshotted. Plain clicks keep
  their focus/select meaning.

## [0.5.1] — 2026-09-05

### Fixed

- Scan diagnostics kept the boilerplate and cut the cause: remote
  error chains are now collapsed onto one line (context — cause) and
  the empty state no longer truncates at 70 chars, so "Permission
  denied" / "No such file" / "Connection refused" survive to the
  screen — each with its remedy named (group grant vs service not
  running).

## [0.5.0] — 2026-09-05

### Fixed

- Agent discovery found nothing even with an agent running: the NixOS
  module never put the `deptui-agent` CLI on the system PATH, so
  `ssh host deptui-agent …` — the TUI's transport — failed with
  "command not found". The module now installs the package
  (`environment.systemPackages`) and gains
  `services.deptui-agent.users = [ … ]` to grant socket-group access
  to the ssh users that need it. The scan also stopped discarding its
  evidence: the empty state now lists what each deploy node actually
  said (command not found / permission denied / timeout) with the
  matching fix for each.

### Added

- Dev builds, tiered: `packages.deptui-dev` / `packages.deptui-agent-dev`
  surface cargo's dev profile to nix (debug compile, tests skipped,
  same runtime wrapping — `nix run`-able and `nix copy`-able), and
  `scripts/dev-agent` / `scripts/dev-tui` (shared `scripts/dev-push`)
  push the raw incremental debug binary to a host for the fastest
  remote loop. README gains a "Development loop" section laying out
  the tiers.

## [0.4.0] — 2026-09-05

### Added

- Agent auto-discovery: pressing `a` with no client config scans the
  flake's `deploy.nodes` (parallel, BatchMode, short timeouts) for
  hosts answering `deptui-agent status` and connects to what it
  finds. `~/.config/deptui/config.toml` is now optional — a pin for
  agents that aren't deploy nodes — and `r` rescans from the empty
  state. All agent configuration stays on the agent host.
- The TUI title bar shows the app version.

## [0.3.0] — 2026-09-05

### Added

- The probe preflights are now command buttons: `U:size`, `P:plan`,
  and `C:drift` join the commands row (clickable, like everything
  there), grouped probes → marks/profiles → deploys → ssh/agent. The
  row packs whole buttons onto extra rows instead of clipping, so
  every button stays visible (and clickable) down to 80x24.
- Mouse support: wheel-scroll the job log, the help popup, and the
  host selection; click to focus panes, select hosts, flip toggles,
  and press command buttons. `--no-mouse` opts out (mouse capture
  makes terminal-native text selection require holding Shift).
- Cancel for the agent: `deptui-agent cancel` / `POST /cancel` / `x`
  in the TUI agent view stops a deploy run in flight and parks its
  hosts at that revision. Pause acks now say a run in flight keeps
  going and point at cancel.

### Changed

- Profile selection reads as a set: the `s`/`h` command buttons carry
  on/off dots (`s:● sys h:● home`) and the details pane / confirm
  popup say `system+home` instead of `all`.

### Fixed

- Pressing `a` with no agent configured did nothing: the setup hint
  was filtered out of the job log (untagged app-level lines were
  never shown anywhere). Untagged lines now always show, and `a`
  always opens the agent view — with setup instructions and any
  settings-file parse error when no agent is configured.

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
