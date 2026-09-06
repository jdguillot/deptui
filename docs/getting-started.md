# Getting started

(Anything in `<angle-brackets>` below is yours to replace — e.g.
`<agent-host>` is the hostname/IP of the machine running the agent.)

Two pieces — the TUI stands alone; the agent is optional on top:

- **`deptui`** — a terminal UI for [deploy-rs](https://github.com/serokell/deploy-rs):
  host status, update probes, preflights, and deploys, for any flake
  with `deploy.nodes`.
- **`deptui-agent`** *(optional)* — a daemon that watches your git
  repo and deploys updates on a schedule, with the TUI as its remote
  control. Skip section 2 entirely if you only want the TUI.

## 1. The TUI

No install needed to try it:

```bash
nix run github:jdguillot/deptui -- /path/to/your/flake
```

The flake ref can be anything `nix` accepts — a local checkout,
`github:you/infra`, etc. You'll see every `deploy.nodes` host with
reachability dots and per-profile badges.

To install just the TUI permanently:

```nix
# flake input
inputs.deptui.url = "github:jdguillot/deptui";

# NixOS
environment.systemPackages = [ inputs.deptui.packages.${pkgs.system}.deptui ];
# …or home-manager
home.packages = [ inputs.deptui.packages.${pkgs.system}.deptui ];
```

(or imperatively: `nix profile install github:jdguillot/deptui`). The
package wraps `deploy`, `nix`, and `ssh` onto its PATH, so nothing
else is required.

The keys to know on day one: `r` refresh, `u` update check, `Space`
mark hosts, `Shift+S` deploy (switch), `x` cancel, `?` the full cheat
sheet. Everything is also clickable.

## 2. The agent (optional)

The agent runs on an always-on machine (it can be one of your deploy
nodes — self-deploys are safe) and needs three things: the module
enabled with a watch, its ssh key authorized on the targets, and
passwordless activation on the targets.

### Enable it

```nix
# flake input
inputs.deptui.url = "github:jdguillot/deptui";

# the agent host's NixOS configuration
imports = [ inputs.deptui.nixosModules.deptui-agent ];

services.deptui-agent = {
  enable = true;
  # "infra" is your name for this watch; the keys under `hosts` are
  # NOT arbitrary — each must match a node name in the watched
  # flake's `deploy.nodes` ("web" and "db" here are placeholders for
  # whatever your nodes are called).
  watches.infra = {
    repo = "git@github.com:you/infra.git";  # or https://…
    branch = "main";                        # or tag = "prod" (a moving tag)
    interval = "15m";                       # or cron = "0 */6 * * *"
    hosts.web = { };                        # ← deploy.nodes.web, deploy-rs defaults
    hosts.db = { remote_build = true; };    # ← deploy.nodes.db, per-host overrides
  };
  # who may control the agent over ssh / from the TUI (socket access):
  users = [ "yourname" ];
};
```

Deploy the agent host. On first start the agent **generates its own
ssh identity** — no secrets management; the private key never leaves
the machine.

### Authorize its key

```bash
ssh <agent-host> deptui-agent pubkey   # <agent-host> = where the agent runs
```

Add the printed line to the deploy user's `authorized_keys` on every
target —
declaratively, e.g.:

```nix
users.users.yourname.openssh.authorizedKeys.keys = [
  "ssh-ed25519 AAAA… deptui-agent@<agent-host>"  # paste pubkey's output verbatim
];
```

and deploy the targets once. Host keys need no setup: the agent
trusts a target on first contact and pins it from then on
(`hostKeyChecking = "strict"` if you'd rather pre-pin via
`programs.ssh.knownHosts`).

The targets must accept **non-interactive activation**: a root deploy
user, or NOPASSWD sudo for the deploy user. A headless daemon cannot
answer prompts — anything that would prompt fails fast instead. On
NixOS targets:

```nix
security.sudo.extraRules = [{
  users = [ "yourname" ];
  commands = [ { command = "ALL"; options = [ "NOPASSWD" ]; } ];
}];
# (or simply security.sudo.wheelNeedsPassword = false; if you already
#  treat wheel that way)
```

**Why not scope the rule to the one command deploy-rs runs?**
Because that command is a *store path that changes every generation*
(`sudo /nix/store/<hash>-…/activate-rs …`), a scoped rule must
wildcard it — `NOPASSWD: /nix/store/*/activate-rs` or similar. And on
a Nix machine that wildcard is root: any local user can ask the nix
daemon to materialize a store path with any content under a matching
name (that is what `nix build` *is*), so the deploy user could build
their own "activate-rs" that execs a shell and sudo it. Pinning the
exact hash instead is a chicken-and-egg: the deploy that would
install next generation's rule needs the permission before it runs.
So an agent-managed host has exactly two configurations:

- **NOPASSWD `ALL` for a dedicated deploy user** — root-equivalent,
  but visible, auditable, and independently revocable (its own key,
  its own journal identity). This is what every deploy tool of this
  family (deploy-rs, colmena, morph) assumes.
- **`sshUser = "root"`** (with `PermitRootLogin prohibit-password`) —
  the same trust stated more plainly: one credential that *says* it
  is root, no sudo indirection at all.

If neither is acceptable for some host, the consequence is simply
that **that host cannot be agent-managed** — leave it out of the
watches and deploy it from the TUI instead, where `--interactive-sudo`
(toggle `5`) lets you keep passworded sudo and type it per deploy.
(There is deliberately no "agent reads the sudo password from a
file" mode: a stored, reusable human password is strictly more
sensitive than a dedicated ssh key, so it would weaken your setup
while pretending to harden it — agent config rejects
`interactive_sudo` outright.)

Either way, root-equivalence is inherent to unattended deployment —
the design goal is keeping it visible, not pretending a wildcard
contains it. deptui ships no option to write this rule because it
belongs to the *target's* config (a different machine than the agent
module manages).

### Check the chain

```bash
ssh <agent-host> deptui-agent validate
```

This diagnoses the agent's identity first (missing / passphrase /
ok — printing the public key), then probes every configured host.
Each failure names its own fix.

### First encounters: the agent asks before it takes over

The agent never blind-deploys a host it has never deployed. On first
contact it probes: already running the watched revision → **adopted**
silently; anything else → **held**, and you get notified. Approve the
takeover with `deptui-agent approve HOST` (or `Enter` in the TUI's
agent view — it warns that the next round will move the host off
whatever was deployed outside the repo). Approval is consumed by the
next scheduled poll, kick, or catch-up — the agent never deploys the
moment you approve. Pure-GitOps hosts can opt out with
`bootstrap = "deploy"`.

Also good to know: a host that is *down* when an update arrives is
pending, not failed — the agent re-probes it and deploys when it
answers (`catch_up = false` per host opts out). A *failed* deploy
parks the host until a new revision, a kick, or an approval. `pause`
stops future polls; `cancel` stops a run already in flight.

### Drive it from the TUI

On your workstation, just press `a`. The TUI scans your deploy nodes
for hosts answering `deptui-agent status` and connects — no client
config needed. (An agent that isn't a deploy node can be pinned in
`~/.config/deptui/config.toml`: `[agents.name] ssh = "you@host"`.)
The view shows watches, per-host state, and the agent's full log
(searchable, selectable, yankable — same powers as the main job log),
with `u` kick, `p`/`P` pause, `x` cancel, `Enter` approve.

### Notifications

```nix
services.deptui-agent.settings.notify = {
  url = "https://ntfy.sh/your-topic";   # ntfy or generic JSON POST
  kind = "ntfy";
  on_failure = "some-command {host} {rev}";  # any hook you like
};
```

Failures (and holds) always notify; start/success are opt-in via
`events`.

### Kick from CI (optional)

```nix
services.deptui-agent = {
  listen = { enable = true; port = 7337; tokenFile = "/run/secrets/kick-token"; };
  openFirewall = true;
};
```

```yaml
# GitHub Actions, after push
- run: |
    curl -fsS -X POST \
      -H "Authorization: Bearer ${{ secrets.DEPTUI_KICK_TOKEN }}" \
      "https://<agent-host>:7337/kick?watch=infra"
```

The TCP listener serves *only* kick + status; the full control
surface never leaves the group-gated Unix socket. (`ssh <agent-host> deptui-agent kick` works too, with no open port at all.)

## Troubleshooting one-liners

| symptom | cause → fix |
| --- | --- |
| `a` shows "no agents found" with per-host reasons | read them: `command not found` → rebuild the agent host with the module (it installs the CLI); connection errors → host is down |
| every target: bare `Permission denied` | run `validate` — usually a missing or passphrase-protected key, both named outright |
| socket `Permission denied` on `deptui-agent status` | your ssh user isn't in `services.deptui-agent.users` |
| host shows `HELD` | by design: first encounter differs from the repo — approve it (`Enter` / `approve`) |
| agent deployed its own host and disappeared | you set `restartOnUpdate = true`; the default (`false`) survives self-deploys, new agent takes over on next restart/reboot |
