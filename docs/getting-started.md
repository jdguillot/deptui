# Getting started

Two pieces, adopt one or both:

- **`deptui`** — a terminal UI for [deploy-rs](https://github.com/serokell/deploy-rs):
  host status, update probes, preflights, and deploys, for any flake
  with `deploy.nodes`.
- **`deptui-agent`** — a daemon that watches your git repo and deploys
  updates on a schedule, with the TUI as its remote control.

## 1. The TUI

No install needed to try it:

```bash
nix run github:jdguillot/deptui -- /path/to/your/flake
```

(or add `github:jdguillot/deptui` as a flake input and put
`deptui.packages.${system}.deptui` in your packages). The flake ref
can be anything `nix` accepts — a local checkout, `github:you/infra`,
etc. You'll see every `deploy.nodes` host with reachability dots and
per-profile badges.

The keys to know on day one: `r` refresh, `u` update check, `Space`
mark hosts, `Shift+S` deploy (switch), `x` cancel, `?` the full cheat
sheet. Everything is also clickable.

## 2. The agent

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
  watches.infra = {
    repo = "git@github.com:you/infra.git";  # or https://…
    branch = "main";                        # or tag = "prod" (a moving tag)
    interval = "15m";                       # or cron = "0 */6 * * *"
    hosts.web = { };                        # deploy-rs defaults
    hosts.db = { remote_build = true; };    # per-host flag overrides
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
ssh agent-host deptui-agent pubkey
```

Add that line to the deploy user's `authorized_keys` on every target —
declaratively, e.g.:

```nix
users.users.yourname.openssh.authorizedKeys.keys = [
  "ssh-ed25519 AAAA… deptui-agent@agent-host"
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

deptui deliberately ships no option for this, for an honest reason:
it would live on the *target's* config, not the agent's, and scoping
it tighter than `ALL` is security theater on NixOS — activation runs
per-generation `/nix/store/*` paths, and any rule matching those
matches a shell too. A deploy user with NOPASSWD is root-equivalent;
that is inherent to what deploying *is*, so the grant belongs where
you can see it.

### Check the chain

```bash
ssh agent-host deptui-agent validate
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
      "https://agent-host:7337/kick?watch=infra"
```

The TCP listener serves *only* kick + status; the full control
surface never leaves the group-gated Unix socket. (`ssh agent-host
deptui-agent kick` works too, with no open port at all.)

## Troubleshooting one-liners

| symptom | cause → fix |
| --- | --- |
| `a` shows "no agents found" with per-host reasons | read them: `command not found` → rebuild the agent host with the module (it installs the CLI); connection errors → host is down |
| every target: bare `Permission denied` | run `validate` — usually a missing or passphrase-protected key, both named outright |
| socket `Permission denied` on `deptui-agent status` | your ssh user isn't in `services.deptui-agent.users` |
| host shows `HELD` | by design: first encounter differs from the repo — approve it (`Enter` / `approve`) |
| agent deployed its own host and disappeared | you set `restartOnUpdate = true`; the default (`false`) survives self-deploys, new agent takes over on next restart/reboot |
