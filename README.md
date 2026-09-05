# deptui

A small terminal UI on top of [serokell/deploy-rs](https://github.com/serokell/deploy-rs).
It reads `deploy.nodes` from your flake, shows which hosts are reachable
and (on demand) which ones are running stale builds, and lets you push
NixOS host configs, home-manager configs, or both — either as an
immediate switch or as a new boot entry for next boot.

## Features

- Auto-discovers every entry in `deploy.nodes` from a flake.
- Per-host **online/offline** indicator (TCP probe of port 22, no ICMP /
  no sudo required).
- On-demand **update check** (`u`) that compares the locally-built
  profile store path against the remote machine's `/run/current-system`
  (system) and home-manager profile (home). Correctly resolves the
  deploy-rs activation wrapper to its toplevel so hosts that are already
  current show `✓` rather than a spurious `↑`.
- **Closure size delta + package diff** (`Shift+U`) — measures the
  local vs. remote closure sizes and then automatically runs a
  metadata-only package-version diff (no heavy `nix copy`). Detects
  content-only changes (e.g. config file edits that don't bump any
  package version) and surfaces them distinctly.
- **Multi-host operations** — mark multiple hosts with `Space`, then
  `u` / `Shift+U` / `s` / `b` / `d` operate on all marked hosts at
  once.
- Choose what to deploy: **all profiles** / **system only** / **home only**.
- Choose how to deploy: **switch** (immediate), **boot** (next boot),
  or **dry-run** (`deploy --dry-activate`, build + diff only).
- **Job log pane** with live, line-buffered, ANSI-stripped `deploy`
  output. Each host gets a coloured prefix for legible batch output.
  Cancelling kills the child cleanly.
- **Log search** (`/`) with `n`/`N` navigation, `[current/total]`
  counter in the pane title, and a distinct cyan highlight on the active
  match.
- **Per-host SSH overrides** — for nodes that aren't in your
  `~/.ssh/config`, set hostname/IP, ssh user, identity file, and extra
  `-o` options from inside the TUI. Hosts with overrides show a magenta
  `[ssh]` tag in the list.
- **Toggles** for the deploy-rs flags you reach for most:
  `--skip-checks`, `--magic-rollback`, `--auto-rollback`,
  `--remote-build`, `--interactive-sudo`. Always-visible state strip.
- **Pane-jump keys** (`f`/`p`/`t`/`c`) for instant focus on any
  pane; `Tab`/`Shift+Tab` for sequential cycling.
- **Build plan preflight** (`Shift+P`) — runs the dry-run deploy-rs
  would run and reports what will be compiled, what will be fetched, and
  how much it downloads, *before* you commit. Shown in the deploy
  confirmation too.
- **Substituter drift check** (`Shift+C`) — catches the trap where a
  deploy that *adds* a binary cache can't use it (see below).
- **Help popup** (`?`) with a full guide to every key, badge, and toggle.

- **Auto-deploy agent** (`deptui-agent`): a daemon that watches a git
  repo and pushes updates to your hosts on its own — with catch-up for
  offline hosts, failure notifications, and full control from the TUI
  (`a`). See below.

## Build plan preflight (`Shift+P`)

Runs the same dry-run deploy-rs would, and reports the plan before you
commit to it:

```
[plan] gpu.system: 2 derivation(s) will be COMPILED
[plan]   ⚒ cuda-merged-12.4
[plan]   ⚒ ollama-0.5.4
[plan] gpu.system: 41 path(s) will be fetched (1.2 GiB download)
```

It follows the node's build mode, because the answer differs:

- local build: `nix build --dry-run <flake>#deploy.nodes.<n>.profiles.<p>.path`
- remote build: `nix build --dry-run <drv>^out --eval-store auto
  --store ssh-ng://<target>`, preceded by
  `nix copy --derivation --to ssh-ng://<target>` — the remote store can
  only reason about a derivation it has, and this is the same step
  deploy-rs performs before a remote build. Nothing is realised; only a
  `.drv` is added to the target's store.

The result also appears in the deploy confirmation popup, so "this
deploy will compile ollama-cuda" arrives before you press `y` rather
than forty minutes later. A successful deploy clears the cached plan,
since it described the closure that was just pushed.

## Automatic cache seeding

When `Shift+C` finds drift on a `--remote-build` node it chains straight
into `Shift+P`, and the next deploy to that node **seeds the target's
store before building**: for every path the plan says would be compiled
or fetched, deptui asks the target to `nix copy --from <new-cache>` it.
Paths the cache has land in the store and the build skips them; paths it
doesn't have are a no-op.

This is deliberately additive. Nothing on the target is rewritten and
nothing needs restoring — if the deploy fails, is cancelled, or the
process dies, the only trace is some extra store paths, which is exactly
what a normal fetch would have left. Copies are attempted one path at a
time because `nix copy` fails a whole batch when any single path is
missing from the cache, and missing is the normal case.

The new cache's key is passed as `--option extra-trusted-public-keys`,
which **nix honours only for a trusted user** — hence the `trusted-users`
check. If nothing gets copied, deptui says so and names that as the
likely reason rather than letting the deploy quietly compile anyway. The
per-deploy attempt cap is 200 paths; anything beyond that is reported,
never silently dropped.

## Substituter drift (`Shift+C`)

Adding a binary cache to a NixOS config and deploying it in one step does
not work, and nothing in the toolchain tells you why. `nixos-rebuild` and
deploy-rs **build before they activate**, and `/etc/nix/nix.conf` is a
`restartTrigger` for `nix-daemon.service` — so the new substituter only
goes live once every build has already finished. The first deploy
compiles from source exactly the things the new cache was supposed to
supply.

Concretely: adding `https://cache.nixos-cuda.org` alongside
`services.ollama.package = pkgs.ollama-cuda` should be one 1.2 GiB fetch.
Without the cache in place first, `cuda-merged-12` and `ollama` build
locally.

`Shift+C` evaluates `nixosConfigurations.<node>.config.nix.settings`
(an eval, not a build) and compares its substituters and
trusted-public-keys against the nix installation that will actually do
the building. That last part is the whole point:

| build mode           | who fetches                  | compared against            |
| -------------------- | ---------------------------- | --------------------------- |
| local (default)      | this machine's nix           | local `nix config show`     |
| `--remote-build` (4) | the target's `nix-daemon`    | target's `nix config show`  |

For a **local** build the fix is just extra build args:

```
deploy … -- --option extra-substituters <url> \
            --option extra-trusted-public-keys <key>
```

For a **remote** build that does nothing. `--remote-build` is not
`ssh host nix build`; it is a local nix client driving a remote store
(`nix build … --eval-store auto --store ssh-ng://…`), so `--option`
configures the *local* client while the fetching is done by the target's
daemon out of the target's own `nix.conf`. The setting does not cross the
store boundary. Measured against a live target:

| experiment                                          | derivations built |
| --------------------------------------------------- | ----------------- |
| ssh-ng store, no options                             | 17 (incl. ollama) |
| ssh-ng store + `--option extra-substituters`         | 17 — **inert**    |
| cache in the target's `~/.config/nix/nix.conf`       | 16, ollama fetched|
| run directly on the target + `--option`              | fetched (1.2 GiB) |

So for remote builds the cache has to be configured **on the target**,
either in the system `nix.conf` (the chicken-and-egg case) or in the ssh
user's `~/.config/nix/nix.conf`, which `nix daemon --stdio` reads.

`Shift+C` also checks `trusted-users` on the target. If the deploy's ssh
user isn't trusted, nix **silently ignores** substituter overrides — no
warning, it just builds. That check is why the report names the user.

## The agent (`deptui-agent`)

`deptui-agent` is a companion daemon that watches a git repository and
pushes updates to configured hosts via deploy-rs — GitOps for your
homelab, using the same deploy machinery as the TUI. It polls a branch
head (or a moving tag) on an interval or cron schedule, deploys from
its own private clone (never a working tree), and exposes a control
API on a Unix socket. The TUI connects to it over ssh: press `a`.

### Quick start (NixOS)

```nix
{
  inputs.deptui.url = "github:jdguillot/deptui";

  # in your NixOS configuration:
  imports = [ deptui.nixosModules.deptui-agent ];

  services.deptui-agent = {
    enable = true;
    watches.infra = {
      repo = "git@github.com:me/infra.git";
      branch = "main";
      interval = "15m";
      hosts.web = { };                         # deploy-rs defaults
      hosts.db = { remote_build = true; };     # per-host flag overrides
    };
    # Optional: token-gated TCP listener for CI kicks (see below).
    # listen = { enable = true; port = 7337; tokenFile = "/run/secrets/deptui-token"; };
    # openFirewall = true;
    sshKeyFile = "/run/secrets/deptui-deploy-key";
  };
}
```

Non-NixOS: build with `nix build .#deptui-agent` (or `cargo build -p
deptui-agent`), write `/etc/deptui-agent/config.toml`, and install
`contrib/deptui-agent.service`. The TOML mirrors the module options:

```toml
[[watch]]
name = "infra"
repo = "git@github.com:me/infra.git"
branch = "main"          # or: tag = "prod" (a moving tag)
interval = "15m"         # or: cron = "0 */6 * * *"
offline_recheck = "2m"   # re-probe cadence for offline hosts

[watch.hosts.web]
# deploy-rs flags, only-emit-if-changed like the TUI:
# skip_checks / magic_rollback / auto_rollback / remote_build = true|false
# catch_up = false      # disable deploy-on-return for this host
# mode = "boot"         # default "switch"
# profile = "system"    # default "all"

[notify]
on_failure = "ntfy send homelab 'deploy failed: {host} at {rev}'"
url = "https://ntfy.sh/my-topic"   # built-in webhook (kind = "ntfy" | "json")
kind = "ntfy"
```

Rules the agent lives by:

- **Headless means non-interactive.** Targets must accept
  passwordless activation (root deploy user or NOPASSWD sudo);
  `interactive_sudo` is rejected in agent config, and the agent
  validates reachability at startup (`deptui-agent validate` does it
  standalone for CI).
- **No same-commit retry.** A failed host is parked until a new
  revision, a kick, or a force-deploy. deploy-rs's magic rollback has
  already made the target safe.
- **Offline hosts catch up.** A host that is down when an update
  arrives is *pending*, not failed: the agent re-probes it
  (`offline_recheck`) and deploys the moment it answers. Per-host
  `catch_up = false` opts out.
- Sequential deploys, coalesced to the newest revision; state (and
  the last 50 runs per watch) in `/var/lib/deptui-agent`.
- **Self-managing agents are safe by default.** An agent may watch
  and deploy its *own* host; the module sets `restartIfChanged =
  false` on the service so the activation can't kill the agent
  mid-deploy (the trap: unit changed → activation stops the agent →
  the run, the start-phase, and deploy-rs's confirmation all die with
  it, leaving the service stopped). The running agent keeps its old
  version until `systemctl restart deptui-agent` or a reboot; set
  `restartOnUpdate = true` only if the agent never deploys its own
  host.

### CLI

The binary is both daemon and client — `ssh host deptui-agent …` is
the remote-control interface the TUI itself uses:

```
deptui-agent run                     # the daemon
deptui-agent check [--watch W]      # oneshot poll+deploy (cron escape hatch)
deptui-agent validate               # non-interactive reachability, exit != 0 on failure
deptui-agent status [--json]        # agent + per-host state
deptui-agent history [--json]       # recent runs
deptui-agent log WATCH [--run N]    # captured log of a run
deptui-agent kick [--watch W]      # poll now
deptui-agent pause|resume [--watch W | --host H]   # future polls only
deptui-agent cancel                 # stop the run in flight (kills the deploy)
deptui-agent deploy HOST [--watch W]  # force, bypasses pause + parking
deptui-agent tail                   # live run log
```

### Connecting the TUI

Zero config: press `a`. The TUI scans your `deploy.nodes` for hosts
that answer `deptui-agent status` (over ssh, non-interactively) and
connects to what it finds — the client machine needs no agent setup
at all; all the real configuration (watches, cadence, hosts, flags)
lives with the agent itself. The NixOS module makes the agent host
discoverable (it installs the CLI system-wide); grant socket access
to your ssh user with `services.deptui-agent.users = [ "you" ]` —
the deploy user `root` needs no grant. When a scan finds nothing,
the view lists what each node said so the fix is never a guess.

`~/.config/deptui/config.toml` is optional, for pinning an agent that
is not a deploy node (or ordering several):

```toml
default_agent = "homelab"

[agents.homelab]
ssh = "me@deploy-box"    # any ssh destination; socket access = group membership
```

`a` opens the agent view (status, pause/resume, kick, cancel,
force-deploy, live log). Pause gates *future* polls; `x`/`cancel`
stops a deploy already running — its hosts stay parked at that
revision until a new commit, a kick after one, or a force-deploy. Agent-managed hosts show an `[agent]` badge in the host
list — `[agent!]` when the last agent deploy failed, `[agent~]` when
an update is pending on an offline host.

### Kicking from CI

The kick endpoint means "check now" — it names no refs and deploys
nothing that polling wouldn't. Two ways to trigger it from a GitHub
Action after push:

```yaml
# Over ssh (no exposed port; the runner needs an ssh key):
- name: Kick deptui-agent
  run: ssh -o BatchMode=yes agent@deploy-box deptui-agent kick --watch infra

# Or over the token-gated TCP listener (listen.enable + openFirewall):
- name: Kick deptui-agent
  run: |
    curl -fsS -X POST \
      -H "Authorization: Bearer ${{ secrets.DEPTUI_KICK_TOKEN }}" \
      "https://deploy.example.com:7337/kick?watch=infra"
```

The TCP listener serves *only* `POST /kick` and `GET /status`; the
full control surface never leaves the Unix socket.

## Requirements

- A flake that defines `deploy.nodes` in the style described in the
  [deploy-rs README](https://github.com/serokell/deploy-rs#overall-usage).
- `nix`, `deploy` (from deploy-rs), and `ssh` on `PATH` — the dev shell
  in this repo provides them.
- SSH access to your hosts. Key auth is the smooth path; password and
  passphrase prompts are supported too — they are routed through
  `SSH_ASKPASS` into a masked popup rather than to the terminal (which
  would corrupt the TUI).
- A terminal at least **80x24**. Below that the UI shows a resize message
  instead of a layout whose panes no longer fit their contents.

### Colour

Colours are the 16 ANSI names, not fixed RGB, so your own terminal theme
decides what they look like and the UI reads correctly on light and dark
backgrounds. Setting [`NO_COLOR`](https://no-color.org) (to any non-empty
value), or running under `TERM=dumb`, drops colour entirely — chips and
selections fall back to reverse video, and every state that mattered
(reachability, `sys:`/`home:` badges) is carried by its glyph rather than
its colour.

## Development loop

Three tiers, fastest first:

**Local edit loop — plain cargo, no ssh, no nix build.** The dev
shell puts the runtime tools (`deploy`, `nix`, `ssh`, `git`) on PATH,
so cargo's incremental dev profile is all you need:

```bash
nix develop                      # toolchain + runtime tools on PATH
cargo run -- /path/to/flake      # the TUI
cargo run -p deptui-agent -- --config /tmp/agent-test/config.toml check
cargo test -p deptui-agent       # the agent's e2e suite runs against shims
```

**Dev-profile nix packages — the cargo dev profile, surfaced to nix.**
`.#deptui-dev` and `.#deptui-agent-dev` build the debug profile (no
LTO, no tests) but are wrapped like the real packages, so they run
anywhere nix runs — and `nix copy` ships them to another machine
closure-complete:

```bash
nix run .#deptui-dev -- /path/to/flake
nix build .#deptui-agent-dev && nix copy --to ssh-ng://deploy-box ./result
```

**Push scripts — raw speed for remote testing.** Skip nix entirely
and push the incremental debug binary (caveat: it links this
machine's glibc, so this is a NixOS-to-similar-NixOS shortcut; the
-dev packages above are the robust route):

```bash
scripts/dev-agent me@deploy-box   # → /tmp/deptui-agent-dev
scripts/dev-tui   me@deploy-box   # → /tmp/deptui-dev
```

Plain `nix build` stays what it is: the release profile, for
artifacts you deploy for real.

## Building

This project lives in a Nix flake. The dev shell installs the Rust
toolchain plus everything the TUI shells out to:

```sh
nix develop
cargo build --release
```

Or build directly via Nix:

```sh
nix build
./result/bin/deptui /path/to/your/flake
```

## Running

```sh
# defaults to the current directory
deptui

# or point at any flake reference nix understands
deptui /home/me/.dotfiles
deptui github:me/dotfiles
```

Optional flags:

| flag         | purpose                                          |
| ------------ | ------------------------------------------------ |
| `--log-file` | write tracing logs to a file (TUI stays clean)   |
| `--build-arg` | extra `nix build` arg, forwarded after deploy-rs's `--` (repeatable) |

## Mouse

The panes are mouse-aware: the wheel scrolls the job log (and the help
popup) and moves the host selection; left click focuses panes, selects
host rows, flips the numbered toggles, and presses command buttons —
everything routes through the same handlers the keyboard uses.

**Drag to copy**: dragging across the job log — or, in the agent view,
the watches pane and the live log — highlights the rendered cells and
copies them to the clipboard on release, exactly as shown (wrap,
badges and all). Plain clicks keep their focus/select meaning.

Mouse capture makes your terminal's native text selection require
holding Shift; start with `--no-mouse` to opt out entirely.

## Key bindings

| key            | action                                                       |
| -------------- | ------------------------------------------------------------ |
| `?`            | open the in-app help popup (full reference)                  |
| `q` / `Ctrl-C` | quit (shows confirmation; warns if deploy is running)        |
| `Esc`          | cancel visual selection, then clear search, or close modal   |
| `j` / `k`      | move selection / scroll log                                  |
| `g` / `G`      | jump to top / snap to tail                                   |
| `Space`        | mark/unmark host for batch operations                        |
| `Shift+A` / `Shift+X` | mark all hosts / clear all marks                      |
| `Tab` / `Shift+Tab` | cycle focus forward / backward                          |
| `f`/`p`/`t`/`c` | jump to hosts / job log / toggles / commands                |
| `r`            | refresh online/offline for every host                        |
| `u`            | cheap-tier update check (paths + activation time)            |
| `Shift+U`      | full update check: closure size delta + package diff         |
| `Shift+P`      | build plan preflight: what gets compiled + download size     |
| `Shift+C`      | substituter drift: caches this deploy adds but can't use     |
| `s` / `h`       | toggle the system / home profile in the deploy target — the buttons show the selected set as dots (`s:● sys h:● home`); the last one can't be turned off |
| `Shift+S` / `Shift+B` / `Shift+D` | deploy: switch now / boot entry / dry run  |
| `a`            | open the agent view (auto-deploy daemon status & control)    |
| `x`            | cancel the running deploy (kills its whole process group)    |
| `v` / `V`      | select job-log text by char / by line (works from any pane)  |
| `y`            | yank the visual selection to the clipboard                   |
| `/`            | search the job log (works from any pane)                     |
| `n` / `N`      | next / previous search match (works from any pane)           |
| `1`–`5`        | toggle deploy-rs flags (see below)                           |
| `o`            | open the SSH overrides menu for the selected host            |

### Toggles (`1`–`5`)

| key | flag                       | default | notes                                                   |
| --- | -------------------------- | ------- | ------------------------------------------------------- |
| `1` | `--skip-checks`            | off     | skip the pre-deploy `nix flake check`                   |
| `2` | `--magic-rollback false`   | on      | wait for confirmation, auto-roll-back on timeout        |
| `3` | `--auto-rollback false`    | on      | roll back if activation itself fails                    |
| `4` | `--remote-build`           | off     | build on the target host instead of locally             |
| `5` | `--interactive-sudo true`  | off     | **will hang the TUI** — child reads password from stdin |

The toggles strip at the top of the screen always shows the current
state with a green `●` for on or grey `○` for off.

### SSH overrides (`o` then sub-key)

For hosts that aren't in `~/.ssh/config`, press `o` to open the
overrides menu, then:

| sub-key | action                                                                   |
| ------- | ------------------------------------------------------------------------ |
| `h`     | set hostname / IP override                                               |
| `u`     | set ssh user                                                             |
| `k`     | set identity file path (passed as `ssh -i`)                              |
| `o`     | set extra ssh `-o` options (whitespace-separated, e.g. `Port=2222`)      |
| `c`     | clear all overrides for this host                                        |
| `Esc`   | leave the menu                                                           |

When editing a field, type into the prompt strip at the bottom of the
screen and press `Enter` to save (or `Esc` to cancel). An empty value
clears that field. Hosts with any active override show a magenta
`[ssh]` tag in the host list and a summary line in the details pane.

These overrides are session-only — they're not persisted to disk and
don't modify your flake. They feed both the status checks and the
actual `deploy` invocation, so what you see in the badges matches what
gets pushed.

## Update-check details

### Cheap tier (`u`)

Runs `nix eval --raw <flake>#deploy.nodes.<name>.profiles.<p>.path` to
get the deploy-rs activation wrapper, resolves it to the actual system
toplevel via `nix-store --query --references`, then compares that
against `readlink -f /run/current-system` (for `system`) or the
home-manager profile symlink (for `home`). Falls back to a parsed
name+version comparison when the wrapper isn't in the local store yet.
On-demand because the eval can be slow on large flakes.

### Full tier (`Shift+U`)

After a successful `u`, this measures `nix path-info --closure-size`
on both sides, then runs a metadata-only package diff by listing
`nix-store --query --requisites` locally and remotely. Version
changes, additions, and removals are surfaced per-package. When every
package name+version matches but store paths still differ (e.g. a
config file rebuild), the TUI shows a distinct "content differs"
indicator and lists the divergent paths so the user can identify what
changed.

Stale size and package data are automatically cleared when `u` is
re-run or after a successful deploy.

### Badges

Each host row opens with a reachability dot. The shape carries the state
on its own, so the list stays readable in monochrome and for red/green
colour blindness:

| dot | meaning                                        |
| --- | ---------------------------------------------- |
| `●` | online (TCP connect to the resolved SSH port)  |
| `○` | offline                                        |
| `·` | not probed yet                                 |
| `⠋` | probe in flight (animated braille spinner)     |

Then one badge per profile:

| badge       | meaning                                              |
| ----------- | ---------------------------------------------------- |
| `sys:?`     | not yet checked                                      |
| `sys:✓`     | host already runs the latest build                   |
| `sys:↑`     | host is behind — deploy would change something       |
| `sys:—`     | profile has never been deployed on this host         |
| `sys:!`     | check failed (host unreachable, eval error, …)       |
| `sys:-`     | this profile is not defined for this host            |
| `sys:⠋`     | check in flight (animated braille spinner)           |
| `sys:✓⠋`    | check in flight, prior result was up-to-date         |

## Limitations

- Online check resolves the effective host and port via `ssh -G`
  (respecting `~/.ssh/config` and any per-host SSH overrides set in
  the TUI). It falls back to port 22 only when `ssh -G` fails. Hosts
  whose resolved SSH port is blocked from your machine will still show
  as offline even if they are otherwise up.
- The home-update probe assumes `~/.local/state/nix/profiles/home-manager`
  or `~/.nix-profile`. Custom profile locations aren't auto-detected.
- `--interactive-sudo` (toggle `5`) is supported. deploy-rs reads the
  sudo password from `/dev/tty`, so the TUI asks for it up front and
  pre-writes it into a PTY it allocates for the child. A masked popup
  (`•` characters) collects it; Enter sends, Esc cancels the deploy
  before it starts. The password is never written to the log.
- Host-key confirmations on a brand-new host are not interactive. The
  status probes pass `StrictHostKeyChecking=accept-new`, so an unknown
  host is trusted on first contact — see **Security notes** below.
- SSH overrides are session-only. They feed `deploy` and the status
  checks but are not persisted between runs. If you want them to stick,
  add them to your `~/.ssh/config` or to `deploy.nodes.<name>` in the
  flake.

## Security notes

The parts of this worth knowing about, since the tool holds passwords
and talks to machines you care about.

**Passwords.** Typed into a masked popup, never written to the log, and
never persisted. In memory they live in `SecretBuf` / `Zeroizing`
buffers that are wiped on drop; the buffer reserves capacity up front so
growing it can't strand un-wiped copies on the heap, and it redacts its
own `Debug` so a stray trace call can't leak it into `--log-file`. The
cached password is `mlock`ed against swap, and core dumps are disabled
at startup (`RLIMIT_CORE = 0`).

**The askpass socket.** SSH password and passphrase prompts are relayed
over a Unix socket to the TUI. The socket lives in a `0700` temp dir and
is itself `0600`, so only your own uid can reach it — but there is no
authentication beyond those file permissions. Anything running as you
can ask the socket to put a password dialog in front of you. Prompts are
length-capped and stripped of control bytes before being rendered.

**Host keys.** The status probes (`u`, `Shift+U`, `Shift+C`, `Shift+P`)
pass `StrictHostKeyChecking=accept-new`, which trusts an unknown host on
first contact. Note this **overrides** a stricter setting in your
`~/.ssh/config` for those probes. The deploy itself does not go through
this path — it runs `deploy`, which uses your ssh config unmodified. If
you rely on strict host-key checking, be aware the probes are more
permissive than your config, and verify new hosts before probing them.

**Remote writes.** deptui writes to a target's store in exactly two
places, both additive and both `nix`-mediated: `nix copy --derivation`
during the remote build-plan preflight, and the automatic cache seeding
(`nix copy --from <cache>`). Neither modifies configuration or any file
outside the nix store, so nothing needs restoring if a deploy fails or
is cancelled. Every value interpolated into a remote shell command is
single-quoted, with the quoting verified against a real `sh` in the test
suite.

**Dependencies.** `cargo audit` is clean. `cargo clippy --all-targets --
-D warnings` passes.
