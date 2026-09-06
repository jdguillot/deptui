# Changelog

All notable changes to deptui are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions
follow [SemVer](https://semver.org) with pre-1.0 semantics (minor =
features, patch = fixes; breaking changes are called out explicitly).
The workspace version in `Cargo.toml` is the single source of truth —
the flake reads it, both binaries report it via `--version`, and every
release is tagged `vX.Y.Z`.

## [Unreleased]

## [0.18.0] — 2026-09-06

### Changed

- **Agent view: a locked-out host is not a sleeping host.** The
  pre-deploy probe now tells *down* from *denied*: an ssh error that
  means the host answered and refused the agent (`Permission denied`,
  host-key failures, …) is outcome `denied` — same pending marker and
  recheck as offline (fix the key and the next recheck deploys), but
  drawn `⊘ ssh denied …` in full colour with its reason, badged
  `[agent⊘]` on the main screen, `SSH DENIED (host is up)` in
  `deptui-agent status`/`validate`, and it fires an `unreachable`
  notification. Before, a host that was up and rejecting the key got
  the greyed-out "asleep" row and read as down. Wire: `HostStatus`
  gains `offline_denied` (older agents report `false`).
- **One miss, one segment.** A pending host's row folds ssh's reason
  into its `offline … — <rev> pending — <reason>` segment; the separate
  `unreachable: …` segment now shows only when there is no pending
  update. The row wrapped to three states for what was one probe.
- **Failure messages keep their root cause.** The `failed <rev> — …`
  segment elides the *middle* of a long message instead of cutting at
  60 characters, so an anyhow chain shows both the step and the last
  error (`…: unable to read key file`) rather than a path prefix.

### Fixed

- **A stale failure no longer sits next to the current pending
  state.** An `offline`/`denied` outcome clears a `failed` park from an
  older revision (that round could only start because a newer
  revision or an approval ended the park), and existing state files
  carrying both are cleaned up on load. The row showed `! failed
  <old rev> …; offline … — <new rev> pending` and it was impossible to
  tell which was current; the main screen's `[agent!]` badge and
  "N host deploy(s) failed" notice counted the stale park too.
- **`unreachable` no longer contradicts `deployed`.** It was written
  only by the startup probe and never cleared, so a host deployed
  seconds ago still read `unreachable: Could not resolve hostname`.
  It is now updated by every probe miss (run outcome, recheck) and
  cleared by every success and every recheck that answers.

## [0.17.0] — 2026-09-06

### Added

- **Config changes apply themselves too.** The idle self-restart
  check now watches both halves of the installed unit's ExecStart:
  the binary (as before) and the `--config` file — compared by
  *content*, so a NixOS switch (new store path) and an in-place edit
  (same path, new bytes) both hand over at the next idle moment.
  Before, activation deliberately not restarting the unit
  (self-deploy safety) meant every agent config change silently
  didn't apply until a manual `systemctl restart deptui-agent`, with
  the daemon erroring against values already fixed on disk — the
  reported case: a corrected `git_crypt_key_file` path that the
  running daemon's stale in-memory config never saw. Same content at
  a new path does not restart. The journal names what it noticed
  ("updated agent binary/config detected").

### Changed

- Docs: getting-started gains "Updates and config changes: when they
  actually take effect" (the restart model, the
  `autoRestartWhenIdle = false` caveat, the stale-in-memory-config
  symptom); the README's agent section catches up on the drift
  guard, git-crypt watches, `post_checkout`, and the self-restart
  behavior (it still described the pre-0.14 manual-restart world);
  module option descriptions updated to match.

## [0.16.0] — 2026-09-06

### Added

- **git-crypt watches.** Per-watch `git_crypt_key_file` (config +
  module option): the agent unlocks its private clone with an
  exported symmetric key (`git-crypt export-key`; file path,
  sops/agenix-provisioned — GPG mode is deliberately unsupported in a
  headless daemon). Unlock happens once per clone; every round the
  smudge/clean filter config is re-pinned to a PATH-resolved
  `git-crypt` so a garbage-collected store path can't break later
  checkouts. `git-crypt` ships in the agent package's wrapper. A
  missing key file fails the run loudly instead of deploying
  ciphertext. This is the only place decryption *can* happen —
  git-crypt encrypts the git objects, so pointing a watch at an
  unlocked local mirror still checks out ciphertext.
- **Per-watch `post_checkout` hook** (config + module option): a
  `sh -c` command run inside the fresh checkout after every update
  (after the unlock, when both are set) — the escape hatch for `git
  lfs pull`, submodule init, and other repo preparations. Headless
  rules apply: prompts fail fast, a hang is killed after ten minutes,
  a non-zero exit fails the run's setup with the hook's stderr.
  Both run inside `ensure_checkout`, so the daemon's runs, oneshot
  `check`, and `validate` all prepare the identical tree.

## [0.15.1] — 2026-09-06

### Fixed

- Agent view: the log tail reconnects when its ssh stream ends. The
  agent restarts itself on updates (by design since 0.14.0), which
  killed the one-shot tail — after any agent update the view showed
  backfilled history and a live spinner while the current run's lines
  went nowhere until the view was reopened. The status heartbeat now
  revives a dead tail (throttled to 20s while the agent is
  unreachable).

## [0.15.0] — 2026-09-06

### Added

- **Drift guard: the agent only overwrites what it deployed.** Every
  successful deploy (and adoption) records the remote profile paths
  it left active; before the next deploy the agent re-reads them, and
  a host changed out-of-band since then is HELD instead of
  overwritten — your uncommitted or other-branch work-in-progress
  survives the weekly update. One escape hatch: when the running
  generation's `configurationRevision` (`nixos-version --json`) is a
  commit in the watched history, the change was a manual deploy of
  committed work and the update proceeds with a log note. Approval
  (`approve` / Enter) buys one round past the guard, same as the
  other holds. Per-host `drift_guard = false` (module option
  `drift_guard`) opts out. Hosts deployed before this release have no
  recorded baseline yet — the guard arms on their next successful
  deploy.

## [0.14.4] — 2026-09-06

### Fixed

- A kick (or any non-scheduled trigger) that ends in "nothing to do"
  now says so in the log tail, with the per-host reason — "parked at
  this revision (approve, or push a new commit)", "held", "already
  deployed", "paused". Before, the decision went only to the journal:
  the TUI acked the kick and then showed nothing at all, which read
  as a dead button exactly when a host was parked. Quiet scheduled
  polls stay quiet.

## [0.14.3] — 2026-09-06

### Fixed

- Agent view: a fleet taller than the watches pane was silently cut
  off at the bottom. The pane now scrolls to follow the selection
  (j/k), wrapping included.
- Agent view: with no hosts marked, the log pane now shows
  *everything* instead of filtering to the selected watch-row host —
  a running deploy's output was invisible unless you happened to have
  exactly that host selected. Space-marking hosts remains the
  explicit filter.

## [0.14.2] — 2026-09-06

### Fixed

- Agent view: the footer key hints now stack onto extra rows on
  narrow windows instead of being clipped by the fixed 3-row footer
  (the later hints, `q:close` included, went off screen). The
  approval warning footer grows with its wrapped text the same way.
- Agent view: a host parked by a cancel/failure that you then approve
  now *shows* the approval — the `↑` glyph and an "approved — takes
  the next round" segment — instead of an unchanging `!`.
- Approving a host parked by a failed or cancelled deploy at the tip
  revision actually works now: the approval buys exactly one more
  round at that revision (consumed by that round's outcome, success
  or failure, so a persistent failure cannot retry-storm). Before,
  the failed-at-this-revision skip ran unconditionally, and such a
  host was stuck until a new commit — the "force-deploy" the cancel
  message referred to did not exist.
- The HELD hint named a `d` key (TUI) and a `deptui-agent deploy`
  verb (run log) that don't exist; both now point at the real
  gesture: Enter / `deptui-agent approve`.

## [0.14.1] — 2026-09-06

### Fixed

- The self-restart-on-update check restarted the agent once a minute,
  forever: the flake wraps the binary (`wrapProgram`), so the unit's
  ExecStart names the wrapper script while the running process is the
  `.deptui-agent-wrapped` sibling — the two paths never compared
  equal, every 60s check said "updated binary", and each restart left
  a ~5s window with no control socket (the intermittent "connecting
  to the agent socket … No such file or directory" in the TUI). The
  check now compares the install (same `bin/` directory), not the
  file.

### Added

- Getting-started: the kick listener has a real section — why kicks
  (long interval + instant deploys), ssh vs TCP transport choice,
  token generation, storage with and without sops (full sops-nix
  snippet incl. owner/restartUnits), rotation needing a unit restart,
  and a warning that the listener is plain HTTP (put TLS in front
  across the internet — the old example's `https://` was aspirational
  and has been corrected).

## [0.14.0] — 2026-09-06

### Added

- **Updates apply themselves.** The daemon now notices when a deploy
  installed a newer unit/binary (60s check against the installed
  unit's ExecStart) and restarts itself at the next idle moment —
  immediately after a run finishes, so even a self-deploy's own
  update takes over right after the run that shipped it. Clean exit
  + `Restart=always`, so `systemctl stop` still stops. Module option
  `autoRestartWhenIdle` (default on) — the manual restart-once dance
  after every agent update is gone.

## [0.13.3] — 2026-09-06

### Fixed

- A CLI newer than the running daemon said "HTTP 404:" — every verb
  now explains version skew outright ("the running agent is older
  than this CLI … restart it once"), since the service deliberately
  keeps running across updates.
- A failing `validate` printed its whole report under an "Error:"
  prefix; the report is the answer, not an error wrapper — it prints
  plainly and the command exits non-zero.

## [0.13.2] — 2026-09-06

### Fixed

- `validate` over ssh checked the wrong user's world: it read the
  invoker's identity (none) and then tripped git's cross-user
  "dubious ownership" guard on the daemon's clones. Like `pubkey`,
  it now runs through the daemon (`POST /validate`, Unix socket
  only): the walk executes in the daemon's own context — its clones,
  its key, its ssh config — with polls deferred while it runs so git
  can't race; the local walk remains as the no-daemon fallback.

## [0.13.1] — 2026-09-06

### Fixed

- A definitively-failed deploy now actually ends: deploy-rs prints
  its per-node verdict ("Deployment to node X failed, rolled back…")
  *after* the rollback completes, then lingers waiting out the
  confirmation window — v0.10.0 flipped the label but the session
  stayed occupied until timeout or a manual `x`. The verdict line now
  triggers the same process-group teardown `x` runs, automatically:
  the deploy exits within moments, bookkeeping and the batch queue
  proceed normally, and the session frees for the retry.

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
