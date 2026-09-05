# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust workspace wrapping [serokell/deploy-rs](https://github.com/serokell/deploy-rs).
It does not reimplement deploy-rs — it shells out to the `deploy` binary
and to `nix` / `ssh` / `git`. Three crates under `crates/`:

- **`deptui-core`** — everything headless: the deploy runner, flake
  discovery, host probes, probe plumbing, ssh overrides, askpass, and
  the agent API wire types (`agentwire`). Nothing here may depend on
  ratatui/crossterm.
- **`deptui`** — the ratatui frontend. Its `lib.rs` re-exports the core
  modules under the same names, so `crate::host::…`-style paths work
  throughout the TUI code.
- **`deptui-agent`** — the auto-deploy daemon (see
  `docs/agent-design.md`, the confirmed design contract): watches git
  repos, deploys via the core runner, serves a control API on a Unix
  socket (plus an optional token-gated TCP kick/status listener), and
  is its own CLI client (`ssh host deptui-agent <verb> --json` is the
  TUI's remote-control transport). The TUI finds agents by probing
  `deploy.nodes` (BatchMode `deptui-agent status --json`); the client
  settings file only pins agents that aren't deploy nodes. All real
  agent configuration lives on the agent host.

## Common commands

All of these assume you're inside the dev shell (`nix develop`) so that
`cargo`, `rustc`, `deploy`, `nix`, and `ssh` are on `PATH`.

| task                  | command                                       |
| --------------------- | --------------------------------------------- |
| dev shell             | `nix develop`                                 |
| build                 | `cargo build`                                 |
| release build         | `cargo build --release`                       |
| run against a flake   | `cargo run -- /path/to/flake`                 |
| run against cwd       | `cargo run`                                   |
| lint                  | `cargo clippy --all-targets -- -D warnings`   |
| audit                 | `cargo audit` (clean as of the last dep bump) |
| format                | `cargo fmt`                                   |
| nix build             | `nix build`                                   |

| test (all)            | `cargo test`                                  |
| test (one crate)      | `cargo test -p deptui-agent`                   |
| test (integration)    | `cargo test --test '*'`                        |
| nix build (agent)     | `nix build .#deptui-agent`                     |

### Test suite

**Unit tests** (`cargo test --lib`, ~80 tests) live inside `#[cfg(test)]`
blocks in the source modules:

- `ssh.rs` — `SshOverride` accessors, `ssh_args`, `deploy_ssh_opts`,
  `summary`.
- `host.rs` — `split_name_version`, `parsed_paths_equivalent`,
  `compute_version_diff` (typed `PkgChange`s + their `render()`),
  `bucket_paths_by_name`, `join_versions`, `parse_closure_size`,
  `build_ssh_target`.
- `deploy.rs` — `strip_ansi`, `ProfileSel::target_suffix`,
  `DeployRequest::target`, `Toggles::default`.
- `flake.rs` — `Node::has_system`, `Node::has_home`, JSON deserialisation.
- `joblog.rs` — the job-log host filter and the char-selection column
  bounds shared by highlight + yank.
- `theme.rs` — the `NO_COLOR` / `TERM=dumb` decision as a pure function,
  and the `Monochrome` pass (colour cleared, filled cells reversed).
- `app.rs` — `App::new` defaults, key handling (quit confirmation,
  navigation, toggles, mode selection, help popup, global search
  navigation), `push_log` cap, override management, `FocusPane`
  layout rows, and probe-report policy (`apply_probe`): the
  size → pkg-diff and drift → build-plan chains, error invalidation,
  progress lines landing host-tagged.
- `ui.rs` — `style_entry` round-trips a `host::ProgressLine` (the
  styled text equals the canonical text the producer formatted).

**Integration tests** (`tests/`) exercise the process-spawning and
rendering paths:

- `tests/flake_discover.rs` — mock `nix` binary returns canned JSON;
  covers success, empty nodes, eval failure, and malformed JSON.
- `tests/deploy_run.rs` — mock `deploy` binary; covers stdout/stderr
  streaming, exit code propagation, mode flags (`--boot`,
  `--dry-activate`), toggle flags, SSH override flags, ANSI stripping,
  profile suffix in the target string, extra build args landing after
  `--`, and that cancelling kills the whole process group.
- `tests/ui_render.rs` — renders the real widget tree into ratatui's
  `TestBackend`. Nothing else covers `ui::draw`, where a panic or a
  layout regression only shows up in front of the user. Covers the
  default screen, awkward and sub-minimum terminal sizes, every popup,
  the windowed job log's tail and scroll-back, help scrolling and its
  in-place clamp, that the password widget renders only mask characters,
  the below-minimum resize message (and that 80x24 exactly still draws
  the real layout), and that each reachability state renders its own
  glyph in the host row.

- `crates/deptui-agent/tests/agent_e2e.rs` — drives the real
  `deptui-agent` binary (`CARGO_BIN_EXE_`) against a local git repo
  with `nix`/`deploy`/`ssh` PATH shims passed via the child's env (no
  global PATH mutation, so no `#[serial]`): deploy-on-update,
  idempotence, failure parking (no same-commit retry), offline
  catch-up (pending, not parked; deploys on return; `catch_up = false`
  opt-out), and the daemon's socket API (status/pause/kick/history,
  1s-recheck catch-up, SIGTERM). Test git helpers run with
  `GIT_CONFIG_GLOBAL/SYSTEM=/dev/null` — the host's commit-signing
  config once leaked in and failed intermittently.
- `tests/no_color.rs` — deliberately its own binary: `theme::monochrome`
  caches in a `OnceLock`, so the environment has to be set before
  anything in the process asks. Asserts a full frame comes out with every
  fg/bg reset and the filled chips fallen back to reverse video.

Integration tests use `serial_test` to serialize because they mutate the
process-global `$PATH`. When adding more, follow the same pattern:
install a shim, mark `#[serial]`, keep the `TempDir` alive for the test
duration.

## Architecture

The flow is `flake → nodes → status → user action → deploy`.

```
                ┌─────────────┐
                │   main.rs   │  parse CLI, init tracing, init terminal
                └──────┬──────┘
                       ▼
                ┌─────────────┐
                │  flake.rs   │  `nix eval --json` of deploy.nodes
                └──────┬──────┘
                       ▼
                ┌─────────────┐
                │   app.rs    │  state, input modes, tokio::select! loop
                └──┬───┬───┬──┘
        events     │   │   │   background tasks
        ┌──────────┘   │   └───────────────┐
        ▼              ▼                   ▼
   ┌─────────┐   ┌──────────┐         ┌──────────┐
   │event.rs │   │ probe.rs │         │deploy.rs │
   │keys+tick│   │ one spawn│         │spawns    │
   │         │   │ + report │         │`deploy`  │
   └─────────┘   │ path for │         └────┬─────┘
                 │ every    │              │
                 │ probe    │              │
                 └────┬─────┘              │
                      ▼                    │
                 ┌──────────┐              │
                 │ host.rs  │              │
                 │ tcp+ssh+ │              │
                 │ nix      │              │
                 └────┬─────┘              │
                      └──────┬──────┬──────┘
                             ▼      ▼
                          ┌──────────┐
                          │ ssh.rs   │  SshOverride struct shared by
                          │          │  status checks + deploy runner
                          └──────────┘
        │
        ▼
   ┌─────────┐   ┌──────────┐
   │  ui.rs  │◄──│joblog.rs │  log entry type + window rules shared
   │ratatui  │   │          │  by app.rs (keys/yank) and ui.rs (paint)
   └────┬────┘   └──────────┘
        ▼
   ┌─────────┐
   │theme.rs │  semantic colour slots + NO_COLOR pass
   └─────────┘
```

Key invariants worth knowing before touching the code:

- **`flake::discover` is shallow on purpose.** It applies a Nix function
  that strips `path` from each profile, so we don't force evaluation of
  every NixOS module just to draw the host list. If you add a field to
  `Node`/`Profile`, also add it to the `--apply` expression in
  `flake.rs`.
- **Every background probe goes through `probe::spawn`.** `probe.rs`
  owns the plumbing all probes share: the typed progress channel, the
  forwarder that relays `host::ProgressLine`s into the status channel,
  drain-before-verdict ordering, and error stringification. `app.rs`
  decides *when* to probe (`spawn_probe`) and folds results in
  `apply_probe` — which is where probe *policy* lives: which cached
  extras a result invalidates, and which follow-ups it chains into
  (size → pkg diff, remote drift → build plan). Reports are plain
  values, so that policy has unit tests: construct a `probe::Report`,
  call `apply_probe`, assert on the state.
- **`host::check_online` is the only "always-on" background work.** It
  runs once at startup and again on every `r` keypress. The `r` keypress
  also re-runs `flake::discover` so newly-added nodes in the flake appear
  without restarting. Everything more expensive (`u`, deploy itself) is
  lazy and user-triggered.
- **`host::check_profile_up_to_date` resolves the deploy-rs wrapper
  to its toplevel** before comparing against the remote's
  `/run/current-system`. Stripping `/activate` alone isn't enough —
  that yields the activation *wrapper* (e.g.
  `…-nixos-system-<host>-…-activate-path`), whose store hash differs
  from the toplevel (`…-nixos-system-<host>-…`) the remote symlink
  actually points at. The wrapper's direct references include the
  toplevel; `resolve_local_toplevel_quiet` picks it out by parsed
  name match. The fallback `parsed_paths_equivalent` compares
  `<name, version>` pairs when the wrapper isn't in the local store.
- **`app::App::run` is one `tokio::select!`** over three sources: term
  events, background status updates, and live deploy log lines. The
  deploy arm is handled with `recv_deploy`, which yields a
  never-resolving future when no `DeploySession` is running so the
  `select!` arm just stays pending. Tick events skip the draw pass when
  `has_inflight_work()` is false (no spinners to animate), so idle CPU
  is near zero.
- **The deploy log is the only mutable buffer that grows.** It's capped
  at 2000 lines in `App::push_log`. If you add other long-lived buffers,
  cap them too.
- **Log throughput is decoupled from frame rate.** `deploy` can emit
  output far faster than a full ratatui pass completes. The deploy arm
  of the `select!` drains up to `MAX_LOG_DRAIN` already-queued lines
  before painting, and rate-limits its own repaints to
  `MIN_FRAME_INTERVAL` *while a deploy is running* — the 120ms tick is
  what guarantees the skipped frame still lands, so the rate limit must
  stay off once `deploy_task` is `None`. Without this, one draw per line
  made the renderer the bottleneck, the bounded channel filled, and
  back-pressure stalled the child's pipe: the log appeared to freeze
  mid-deploy.
- **`draw_job_log` only builds the lines that can be on screen.** It
  windows `tagged` to the last `job_log_scroll + viewport + 2` entries.
  The two pieces of state that accumulate across the whole pane —
  `size_locals` and the search `match_counter` — are caught up with a
  cheap prefix scan over the skipped entries (driven by
  `LogEntry.kind`, not text parsing). Anything else that accumulates
  left-to-right must be added to that pre-scan too. Inside the window,
  the per-entry accumulator update is hoisted *above* the
  visual-selection branches; keep it there — a branch that returns
  early without it desyncs the size deltas of every later line.
- **Job-log lines are typed at the producer.** `LogEntry.kind:
  host::LogKind` is set when the line is created — the
  `host::ProgressLine` constructors format the canonical text and the
  kind from the same values — and `ui::style_entry` styles from the
  kind. The renderer never re-parses prose. New probe output that
  needs styling gets a `LogKind` variant and a `ProgressLine`
  constructor, not a parser.
- **Child output is read byte-wise, never via `lines()`.**
  `deploy::forward_lines` decodes lossily and treats both `\n` and `\r`
  as terminators. `AsyncBufReadExt::lines()` returns `Err` on the first
  non-UTF-8 byte (nested `nix`/`ssh` output is not guaranteed UTF-8),
  which silently ended the reader — dropping the rest of the deploy's
  output *and* the pipe, SIGPIPE-ing the child. It also ignores `\r`,
  so progress repaints buffered forever.
- **Modes map directly to deploy-rs flags:**
  `Switch` → no flag, `Boot` → `--boot`, `DryRun` → `--dry-activate`.
  Don't try to emulate `Boot` by SSH-ing manually — deploy-rs already
  supports it.
- **Toggles only emit a flag when they differ from the deploy-rs
  default.** This is on purpose: the flake's `deploy.nodes` settings
  stay authoritative until the user actively flips a switch. If you add
  a toggle, decide its default to match deploy-rs and follow the same
  "only-emit-if-changed" rule in `deploy::run_inner`. The TUI side of a
  toggle lives in one table: `deploy::TOGGLES` holds the name, strip
  label, help text, on-hint, and accessors — `TOGGLE_COUNT`, the strip
  rendering, its width maths, and the help popup all derive from it.
- **`SshOverride` is the single source of truth** for both status
  checks (`host::ssh_capture`) and the deploy runner. If you add a new
  field, update `ssh_args()` (per-token argv for ssh);
  `deploy_ssh_opts()` (the joined string for `--ssh-opts`) derives
  from it.
- **The host-list `[ssh]` marker is driven by `SshOverride::is_active`.**
  When clearing the last field of an override, also remove the entry
  from `App.overrides` so the marker disappears — this is what
  `handle_key_edit_override` does.
- **The mouse adds reach, not abilities.** `ui::draw` rebuilds
  `App.mouse` (a `MouseMap` of inner rects + linear hit ranges for the
  strip chips) every frame, and `handle_mouse` routes every hit
  through the same handlers the keyboard uses (`activate_toggle`,
  `activate_command`, `move_selection`, `scroll_job_log`). The hit
  ranges are derived by the same width maths as the spans
  (`toggle_hit_ranges` / `command_hit_ranges`, which
  `commands_content_width` is itself derived from) — change a chip's
  format and the ranges follow or the render test fails. Clicks are
  inert under modal popups and in the agent view; a frame that draws
  a different layout must clear the map first.
- **App input mode is a state machine, not just a flag.** Key dispatch
  in `app::App::handle_key` first short-circuits Ctrl-C and the help
  popup, then routes by `InputMode`. Adding a new modal mode means
  adding a new variant *and* a new dispatch arm.
- **Quitting requires confirmation.** Both `q` and `Ctrl-C` enter
  `InputMode::ConfirmQuit` instead of setting `should_quit` directly.
  The popup warns when a deploy is running. Pressing `y`/Enter confirms,
  `n`/Esc cancels. A second `Ctrl-C` while the popup is showing
  confirms immediately (the short-circuit at the top of `handle_key`).
- **Cancelling signals the process group, not just the child.**
  `kill_on_drop(true)` is set on the spawned `deploy` Command, but it
  only reaches the direct child — and since `pre_exec` calls `setsid()`,
  that child *leads its own process group*, so the `nix` builders and
  `ssh` it forked survive it and keep burning CPU. `deploy::run` returns
  a `DeployCanceller`; `App::cancel_deploy` (key `x`) and the quit path
  call it and then **detach** the task rather than aborting it, because
  the teardown (`killpg` SIGTERM → 3s grace → `killpg` SIGKILL) runs
  inside `run_one` while it still owns the child. Owning the child is
  what makes the escalation safe: the pid stays reserved, so the delayed
  SIGKILL can't land on a recycled pgid. Aborting the task instead would
  drop the child mid-sequence and orphan the group again.
- **`NO_COLOR=1`** is set on the spawned `deploy` so its output stays
  legible when forwarded line-by-line into ratatui. Additionally,
  `deploy.rs::strip_ansi` removes any ANSI escape sequences (CSI, OSC,
  bare ESC, control bytes) that leak through from nested `nix`/`ssh`
  children — without this, ratatui's width accounting drifts and
  characters get dropped from the visible text. It also drops emoji
  variation selectors (VS15/VS16): deploy-rs's `ℹ️`-style prefixes
  render 2 columns where width models may count 1, which shifted the
  rest of the row and left the line's last character ghosted in a cell
  ratatui never repaints.
- **`Shift+U` chains size + package diff.** There is no separate `p`
  keybind. When a `SizeProbe` Ok arrives in `apply_status`, it
  auto-triggers `spawn_pkg_diff_for_profile` for the same
  `(node, profile)`. This keeps the details pane populated in one
  gesture.
- **Extras (size, pkg_diff) are cleared** both when `u` re-runs (in
  `apply_status` → `UpdateProbe` Ok) and when a deploy succeeds (the
  `LogLine::Exit` handler resets `ProfileExtra` to default). This
  prevents stale numbers from lingering after the closure changes.
- **Build-plan preflight mirrors deploy-rs's own build command.**
  `host::check_build_plan` (`Shift+P`) dry-runs the flake attribute for a
  local build, but for `--remote-build` it resolves `…path.drvPath`,
  `nix copy --derivation` it to the target, then dry-runs
  `<drv>^out --eval-store auto --store ssh-ng://<target>`. Using the
  local form for a remote build reports this machine's store and is
  simply the wrong answer. `parse_dry_run` reads nix's *human* output —
  treat it as best-effort and keep it total: unrecognised sections are
  ignored rather than failing the preflight. Cached plans are cleared on
  a successful deploy (same place the extras are), because they describe
  the closure that was just pushed.
- **Cache seeding is additive on purpose.** `host::seed_substituters`
  copies store paths into the target *before* the build instead of
  rewriting its `nix.conf`. That choice is the whole safety story: there
  is no remote file to restore, so failure, cancellation, and a killed
  process all leave the target in a valid state. It runs inside
  `deploy::run_inner` (before the first invocation) rather than in the
  App, so it is sequenced ahead of the build and shares the deploy's
  cancellation. Copy one path per `nix copy` — batching fails wholesale
  when any single path is absent from the cache, which is the normal
  case. Everything interpolated into the remote script goes through
  `shell_quote`; the round-trip test against a real `sh` is the guard.
  Seeding needs *both* the drift result and the build plan, so
  `CacheDriftProbe` chains into `refresh_build_plan_for_node` — without
  the plan there are no paths and `seed_plan_for` says so rather than
  seeding blind.
- **`--build-arg` is inert for remote builds when it configures
  substitution.** `inert_remote_build_args` names those options in the
  log before the deploy starts. deploy-rs forwards `-- <args>` to a
  *local* nix client even under `--remote-build`, so
  `extra-substituters` there configures the wrong side of the ssh-ng
  store boundary and nix reports nothing at all. Extra build args must
  stay the last thing appended in `run_one` — they follow `--`.
- **Substituter drift compares against the *building* store.**
  `host::check_substituter_drift` (`Shift+C`) evaluates
  `nixosConfigurations.<node>.config.nix.settings` — an eval, never a
  build — and diffs it against `nix config show`. Which side it reads is
  the entire point: a `--remote-build` fetches through the *target's*
  `nix-daemon`, a local build through *this machine's* nix. Getting that
  backwards makes the check worse than useless, because `--option
  extra-substituters` passed to a remote build is silently inert (it
  configures the local client; the ssh-ng store boundary is where the
  fetching decision actually happens). Read `nix.settings` via the
  `--apply` in `SETTINGS_APPLY`, which folds in the `extra-*` spellings —
  reading `substituters` alone misses how most people add a cache.
  `user_is_trusted` covers the companion trap: a deploy ssh user outside
  `trusted-users` makes nix ignore substituter overrides with no warning
  at all.
- **Content-only change detection.** When `check_package_diff` finds no
  name+version differences but the store-path sets still diverge, it
  returns a typed `PkgDiff` whose first change is
  `PkgChange::ContentOnly` (plus sample basenames). The details pane
  branches on `PkgDiff::is_content_only()` — no prefix sniffing — and
  renders a yellow "packages identical, content differs" badge instead
  of the misleading green "packages identical".
- **Scroll clamping happens before the title chip reads it.** `draw_job_log`
  computes inner dimensions and runs `compute_tail_scroll_offset` (which
  clamps in place) before constructing the `[↑N]` chip. This prevents a
  one-frame flash of a stale value when holding `k` past the top.
  (`draw_details` no longer has a scrollable log.)
- **Search highlight: active match is cyan**, all other matches are
  magenta. `highlight_match` takes a `current_match` (1-based global
  index from `log_search_stats`) and a `&mut match_counter` to
  distinguish the active hit across the entire pane.
- **`v`/`V` are global too.** Visual selection used to require the job
  log to already have focus, so the obvious gesture right after a deploy
  (focus still on Hosts) did nothing at all. Both keys now focus the pane
  first. `Esc` unwinds one step per press: selection first, then search.
- **`/`, `n`/`N`, and `Esc` are global search keys.** The early key
  dispatch in `handle_key_normal` catches `/` (open search), `n`/`N`
  (next/prev match), and `Esc` (clear search) before pane-specific arms
  fire, so search works identically regardless of which pane has focus.
  Search always targets the job log — it is the only searchable pane,
  so there is no target indirection.
- **Job log is filtered to the active host set.** When any hosts are
  marked (space bar), the job log shows only their entries. With no marks,
  it shows only the selected host's entries. Untagged entries (host:
  None — key hints, cancellations, agent acks) always show: this pane
  is the only place they can appear, and hiding them made `a` look
  like a dead key when no agent was configured. The filter is implemented
  once — `joblog::filtered_indices` — and both key handling and
  `draw_job_log` call it. The char-selection column slice is likewise
  shared (`joblog::char_selection_bounds`), so the highlight and the
  yank cannot disagree about what is selected.
- **Layout is 2-column.** Left column (35%) is vertically split: hosts on
  top, details on bottom. Right column (65%) is the job log. The details
  pane holds only the summary + extras; it is display-only and not a
  focus stop — Tab cycles toggles → hosts → job log → commands, and the
  pane-jump keys are `f`/`p`/`t`/`c`. Commands/info row is below both
  columns: commands left (60%), info right (40%). The command buttons
  are packed onto rows at button boundaries (`layout_commands`) — the
  strip grows to fit every visible button rather than wrapping or
  clipping, and the mouse hit ranges derive from the same layout.
- **Per-profile state is a map, not field pairs.**
  `HostStatus.profiles: BTreeMap<String, ProfileStatus>` holds the
  update state, extras, and build plan for every profile a node
  declares. Use `profile()`/`profile_mut()`; there are no
  `system_*`/`home_*` field pairs and no `match profile.as_str()`
  with a silent `_` arm — a profile name beyond `system`/`home`
  participates everywhere automatically.
- **A running deploy batch is one `Option<DeploySession>`.** The
  child's channel/task/canceller/stdin, the current host, and the
  queue with its mode/profile/progress live in one struct on `App`.
  Every teardown path (exit, spawn error, cancel, quit) *takes* the
  session and reads what it needs off the taken value. New in-flight
  deploy state belongs in the session, so teardown can't forget it.
- **The askpass server must always be answered.** `AskpassServer::serve`
  handles one dialog at a time and blocks on `password_rx.recv()`.
  Dismissing a `PromptSource::Askpass` prompt therefore sends an *empty*
  password rather than nothing: staying silent used to wedge the loop
  forever, so the ssh child waited on the helper, the helper waited on
  the server, and every later prompt in the session was dead. Any new
  exit path out of that prompt has to reply too. The read side is
  bounded (`MAX_PROMPT_BYTES`) and time-limited (`PROMPT_READ_TIMEOUT`)
  for the same reason — one stalled or flooding client would otherwise
  take the whole mechanism down.
- **Prompts are sanitised before they reach a widget.**
  `askpass::sanitise_prompt` strips control bytes and caps the length.
  A prompt embeds host names and key paths, and it is drawn straight
  into a ratatui popup — escape sequences there corrupt the screen the
  same way they do in the deploy log.
- **Passwords never live in a plain `String`.** The typing buffer is
  `app::SecretBuf`: it reserves capacity up front and grows by hand so
  `String::push` can't strand un-wiped copies of the prefix on the heap,
  it wipes on drop via `Zeroizing`, and it redacts its own `Debug` —
  `InputMode` derives `Debug`, so one stray `tracing::debug!` of the
  input state would otherwise write the plaintext to `--log-file`. The
  askpass channel carries `Zeroizing<String>` for the same reason. If
  you add a new path that touches a password, keep it inside these
  types.
- **A panic must restore the terminal.** `ui::init` installs a hook that
  calls `ui::restore` before chaining to the previous hook. Without it a
  panic unwinds past `main`'s `restore()` and leaves the user in raw
  mode inside the alternate screen — no echo, no line editing, and the
  panic message written to a screen that is about to be discarded.
- **SSH_ASKPASS is always active during deploys and probes.** The
  deploy child and all SSH-spawning probe tasks run in a new session
  (`setsid`) with `SSH_ASKPASS` pointing at our own binary in
  `--askpass` mode. SSH password / passphrase prompts are relayed
  over a Unix-domain socket to the TUI, which shows a centered
  popup dialog (`InputMode::PasswordPrompt` with `source: Askpass`).
  The askpass server is app-level (created once in `App::run`) and
  lives in `askpass.rs`.
- **`--interactive-sudo` uses a PTY + pre-prompt.** deploy-rs reads
  the sudo password locally via `rpassword::prompt_password`, which
  opens `/dev/tty`. We always call `setsid()` in `pre_exec` (to
  force SSH to honour `SSH_ASKPASS`), and that would normally strip
  the child of any controlling tty and make rpassword's open fail —
  so when toggle 5 is on, `deploy::run_inner` allocates a PTY via
  `libc::openpty`, registers a second `pre_exec` hook that calls
  `TIOCSCTTY` on the slave (after `setsid`) to make it the
  controlling terminal, and pre-writes the password + `\n` to the
  master so rpassword reads it the moment it opens `/dev/tty`. The
  TUI pops a pre-deploy password popup (`PromptSource::SudoPre`) via
  `handle_key_confirm_deploy` before spawning; on Enter the password
  is cached AND passed as the second arg of `deploy::run(req, pw)`
  so subsequent hosts in a batch re-use it. Any output deploy-rs
  writes to `/dev/tty` (its "You will now be prompted…" banner) is
  drained on a `spawn_blocking` task and forwarded as
  `LogLine::Stderr`. The legacy stderr-based detection
  (`read_stderr_interactive` / `PromptSource::Sudo`) is kept for
  defense-in-depth but rarely fires in practice because the prompt
  now goes to the PTY, not stderr.
- **Password caching within a deploy action.** After the first
  password entry, subsequent prompts within the same action are
  auto-replied from an in-memory cache (`cached_password`). The
  cache is cleared when the action ends (success, failure, cancel)
  or when a new action starts. The cached buffer is `mlock`'d to
  prevent swapping to disk, `zeroize`'d on clear, and core dumps
  are disabled at startup via `setrlimit(RLIMIT_CORE, 0)`. Never
  written to disk or logs.

## Versioning & releases

- **One version, one place:** `[workspace.package] version` in the
  root `Cargo.toml`. Members inherit it (`version.workspace = true`)
  and `flake.nix` reads it via `fromTOML` — never hardcode a version
  anywhere else.
- Pre-1.0 SemVer: **minor** for features (and anything breaking —
  called out in the changelog), **patch** for fixes. Keybinding and
  agent config/API changes count as user-facing surface.
- **Every completed batch of work ends in a release.** When a feature
  or fix (or a session's worth of them) is done — tests green, clippy
  clean, committed — cut the release right then: fold the changelog,
  bump the version, tag. Do not leave finished work sitting on an old
  version number; "the app still says 0.1.0" was the failure mode
  this rule exists to prevent.
- **Fixes ship immediately as their own patch release** — before, and
  never bundled with, unrelated features waiting in `[Unreleased]`.
  A fix the user needs on their machines must not carry feature risk
  along; features wait for their own minor. (User-confirmed policy.)
- **CHANGELOG.md discipline:** user-visible changes land in the
  `[Unreleased]` section *in the same commit* that makes them. A
  release = move `[Unreleased]` under a dated version heading, bump
  the workspace version, commit ("Release vX.Y.Z"), then tag:
  `git tag -a vX.Y.Z -m "vX.Y.Z"` and push with `--follow-tags`.
- The TUI and agent are released in lockstep (one workspace version).
  `deptui-agent status` reports its version over the wire; when the
  two ever need to skew, gate on that field rather than inventing a
  second version.

## Project conventions

- **The agent is headless: everything it runs must be non-interactive.**
  `SSH_ASKPASS=/bin/false`, `GIT_TERMINAL_PROMPT=0`, sudo prompts
  cancel the process group, `interactive_sudo` is rejected at config
  load. If you add a new child invocation to the agent, give it the
  same treatment — a hung prompt in a daemon is a silent outage.
- **Agent state has one writer.** The daemon task owns `AgentState`;
  API handlers talk to it over the `Cmd` mpsc channel and runs report
  back the same way (mirroring the TUI's "the channel is the seam").
  Don't hand `&mut` state to a spawned task.
- **Offline ≠ failed.** A host down at deploy time gets outcome
  `offline` (pending, re-probed at `offline_recheck`, deployed on
  return); a real deploy failure parks the host until a new revision.
  Keep the two paths distinct — collapsing them re-introduces either
  retry storms or missed catch-ups.
- **Pause is not stop.** Pause gates *future* polls; `cancel`
  (`POST /cancel`, CLI `cancel`, TUI `x` in the agent view) is what
  stops a run in flight — it signals the deploy's process group via
  the runner's watch-channel and parks every host the run covered at
  that revision (outcome `cancelled`, failed-stamp message says so),
  so the run doesn't quietly resume at the next poll. No failure
  notification fires for a user cancel.
- **The TCP listener is kick+status only.** The full control surface
  stays on the Unix socket (group-gated, 0660). Never mount another
  route on the TCP router.
- **Commit messages must not contain Claude session links.** No
  `Claude-Session:` trailers or `claude.ai/code/session_…` URLs —
  they are workstation-local noise in a public history. (A
  `Co-Authored-By` credit line is fine.)
- The project shells out heavily. Treat `nix`, `deploy`, and `ssh` as
  load-bearing dependencies — every code path that touches them should
  surface stderr to the user, not swallow it.
- **Every spawned child gets `stdin(Stdio::null())` and
  `kill_on_drop(true)`.** Null stdin because a child that inherits the
  terminal steals the TUI's keystrokes; `kill_on_drop` because probe
  tasks are cancelled by dropping their future, and without it the ssh
  or nix process behind them keeps running. The deploy child is the one
  exception on stdin: it pipes when `--interactive-sudo` is on.
- **Drain stdout and stderr concurrently.** `ssh_capture` uses
  `tokio::join!` over both pipes. Reading one to EOF first deadlocks as
  soon as the child fills the other's 64 KiB pipe buffer — it blocks
  writing, so it never closes the pipe being read. `Command::output()`
  already does this correctly; hand-rolled spawns do not.
- Errors that originate from external tools should be wrapped with
  `anyhow::Context` describing *what we were doing*, not *what tool we
  ran* (e.g. `discovering deploy.nodes`, not `running nix eval`).
- Nothing on the UI thread may block for an unbounded time. The
  clipboard yank is the cautionary tale: `xclip` stays in the foreground
  *owning* the X selection until another client claims it, so
  `Child::wait()` on it never returns and the whole TUI wedged.
  `yank_to_clipboard` hands over the text, waits only long enough to
  catch an immediate failure, and otherwise leaves the helper detached.
- Don't print to stdout/stderr from the main thread once the TUI is up
  — it will corrupt the alternate screen. Use `--log-file` and tracing
  if you need diagnostics.
- **`ui::render` is the only way to paint a frame.** It wraps
  `terminal.draw` in a terminal synchronized update (`CSI ? 2026 h` /
  `l`) so a repaint can't land mid-diff and tear — visible mostly while
  the job log scrolls. The `SyncUpdate` guard ends the update on drop, so
  a failed draw can't leave the terminal holding its output. Calling
  `terminal.draw(|f| ui::draw(f, app))` directly skips all of that;
  `ui::draw` stays public only for the render tests, which drive a
  `TestBackend` with no terminal to synchronize.
- **`ui.rs` never names a colour.** Every style pulls a *role* from
  `theme.rs` — `FOCUS`, `KEY`, `WARNING`, `BUSY`, `ERROR`, `SUCCESS`,
  `ACCENT`, `BRAND`, `MUTED`, `ON_ACCENT`, … . Several of those are the
  same ANSI colour today; they stay separate constants because they
  answer different questions and a future theme may answer them
  differently. The slots are all 16-colour ANSI names on purpose: those
  are *relative*, so the user's terminal theme decides what "yellow" is
  and the UI sits correctly on light and dark backgrounds.
- **`NO_COLOR` is honoured by a post-pass, not by the slots.**
  `theme::Monochrome` is rendered last over `frame.area()` and strips
  fg/bg from every cell, turning any cell that had a background into
  reverse video so filled chips survive as chips. Neutering the slots
  themselves would instead dissolve every chip into body text. This
  handles the decorative half only — see the next point for the rest.
- **Colour is never the only signal.** Each reachability state has its
  own glyph (`●` online, `○` offline, `·` unknown), the way the
  `sys:`/`home:` badges already did. A red/green pair is the textbook
  deuteranopia failure, and under `NO_COLOR` three identically-shaped
  dots collapse into one. Any new state indicator needs a distinct glyph
  before it needs a colour.
- **Below 80x24 the UI refuses to draw.** `ui::draw` gates on
  `MIN_WIDTH`/`MIN_HEIGHT` and renders `draw_too_small` instead. The
  numbers are real, not ceremonial: the details pane alone is 13 rows,
  and at 35% of under 80 columns a host row no longer fits its badges.
  `draw_too_small` has to survive a 1x1 area — no borders, no centring
  maths that can underflow — because a resize can put it there.
- The host badges (`sys:✓` / `sys:↑` / `sys:—` / `sys:!` / `sys:?` / `sys:-`),
  the reachability dots (`●` / `○` / `·`), and the colors are part of the
  user-facing contract — see README. Keep them consistent if you change
  rendering.
