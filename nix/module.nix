# NixOS module for deptui-agent — the auto-deploy daemon.
#
# RFC-42 shape: `settings` is the freeform mirror of the agent's TOML
# config (every key the agent grows is automatically reachable from
# Nix), with typed conveniences layered on top for the pieces that
# deserve first-class options. Secrets only ever enter by file path
# (tokenFile), never through the store.
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.deptui-agent;
  settingsFormat = pkgs.formats.toml { };

  # Typed options fold into the freeform settings; explicit `settings`
  # keys win so nothing the module generates is un-overridable.
  generatedSettings =
    {
      # `[[watch]]` tables from the `watches.<name>` attrset, the name
      # folded in and unset (null) typed options scrubbed.
      watch = lib.mapAttrsToList (name: w: scrub (w // { inherit name; })) cfg.watches;
    }
    // lib.optionalAttrs cfg.listen.enable {
      listen = {
        addr = "${cfg.listen.address}:${toString cfg.listen.port}";
        token_file = cfg.listen.tokenFile;
      };
    };

  # Typed options default to null when unset; TOML has no null, so
  # unset fields are scrubbed before generation (empty attrsets stay —
  # `hosts.web = { }` is meaningful).
  scrub =
    v: if lib.isAttrs v then lib.mapAttrs (_: scrub) (lib.filterAttrs (_: x: x != null) v) else v;

  mergedSettings = lib.recursiveUpdate generatedSettings (scrub cfg.settings);
  configFile = settingsFormat.generate "deptui-agent-config.toml" mergedSettings;

  defaultUser = "deptui-agent";

  # Typed per-host options with a freeform escape hatch: every knob
  # the agent's TOML schema knows is a real, documented, mergeable
  # NixOS option, and anything the schema grows later still passes
  # through untyped.
  hostModule = lib.types.submodule {
    freeformType = settingsFormat.type;
    options = {
      profile = lib.mkOption {
        type = lib.types.nullOr (
          lib.types.enum [
            "all"
            "system"
            "home"
          ]
        );
        default = null;
        description = "Which deploy-rs profiles to push (agent default: all).";
      };
      mode = lib.mkOption {
        type = lib.types.nullOr (
          lib.types.enum [
            "switch"
            "boot"
          ]
        );
        default = null;
        description = "Activation mode (agent default: switch).";
      };
      skip_checks = lib.mkOption {
        type = lib.types.nullOr lib.types.bool;
        default = null;
        description = "deploy-rs -s / --skip-checks (unset: deploy-rs default).";
      };
      magic_rollback = lib.mkOption {
        type = lib.types.nullOr lib.types.bool;
        default = null;
        description = "deploy-rs --magic-rollback (unset: deploy-rs default, on).";
      };
      auto_rollback = lib.mkOption {
        type = lib.types.nullOr lib.types.bool;
        default = null;
        description = "deploy-rs --auto-rollback (unset: deploy-rs default, on).";
      };
      remote_build = lib.mkOption {
        type = lib.types.nullOr lib.types.bool;
        default = null;
        description = "Build on the target instead of the agent host.";
      };
      catch_up = lib.mkOption {
        type = lib.types.nullOr lib.types.bool;
        default = null;
        description = "Deploy a pending update when an offline host returns (agent default: true).";
      };
      bootstrap = lib.mkOption {
        type = lib.types.nullOr (
          lib.types.enum [
            "hold"
            "deploy"
          ]
        );
        default = null;
        description = "First-encounter policy: probe-and-hold (default) or pure-GitOps deploy.";
      };
      extra_build_args = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = "Extra arguments forwarded to nix build via deploy-rs's -- tail.";
      };
    };
  };

  watchModule = lib.types.submodule {
    freeformType = settingsFormat.type;
    options = {
      repo = lib.mkOption {
        type = lib.types.str;
        description = "Git URL or local path to watch — anything `git clone` accepts.";
      };
      branch = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Branch whose head to follow. Exactly one of branch/tag.";
      };
      tag = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Moving tag to follow. Exactly one of branch/tag.";
      };
      interval = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "15m";
        description = "Poll cadence as a duration. Exactly one of interval/cron.";
      };
      cron = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "0 */6 * * *";
        description = "Poll cadence as a cron expression. Exactly one of interval/cron.";
      };
      offline_recheck = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "2m";
        description = "Re-probe cadence for offline hosts with a pending update (agent default: 2m).";
      };
      hosts = lib.mkOption {
        type = lib.types.attrsOf hostModule;
        default = { };
        description = "Hosts to deploy, keyed by node name in deploy.nodes.";
      };
    };
  };
in
{
  options.services.deptui-agent = {
    enable = lib.mkEnableOption "deptui-agent, the deploy-rs auto-deploy daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.deptui-agent;
      defaultText = lib.literalExpression "deptui.packages.\${system}.deptui-agent";
      description = "The deptui-agent package to run.";
    };

    settings = lib.mkOption {
      type = settingsFormat.type;
      default = { };
      description = ''
        Freeform agent configuration, merged over what the typed
        options generate and written to the TOML config file. See
        docs/agent-design.md in the deptui repository for the schema.
      '';
    };

    watches = lib.mkOption {
      type = lib.types.attrsOf watchModule;
      default = { };
      example = lib.literalExpression ''
        {
          infra = {
            repo = "git@github.com:me/infra.git";
            branch = "main";
            interval = "15m";
            hosts.web = { };
            hosts.db.remote_build = true;
          };
        }
      '';
      description = ''
        Watched repositories, keyed by watch name. Each value is the
        body of one `[[watch]]` table (repo, branch or tag, interval or
        cron, hosts.<node> flag sets, …).
      '';
    };

    listen = {
      enable = lib.mkEnableOption "the TCP kick/status listener for CI";
      address = lib.mkOption {
        type = lib.types.str;
        default = "0.0.0.0";
        description = "Address the kick/status listener binds.";
      };
      port = lib.mkOption {
        type = lib.types.port;
        default = 7337;
        description = "Port of the kick/status listener.";
      };
      tokenFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = ''
          File containing the bearer token the listener requires.
          Provision it with agenix/sops-nix or similar; it is read at
          service start and never enters the Nix store.
        '';
      };
    };

    restartOnUpdate = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Whether NixOS activation may stop/restart the agent when its
        unit changes. Off by default because of the self-deploy trap:
        an agent that deploys its *own* host gets killed by its own
        activation mid-deploy — the run dies, the start-phase and
        deploy-rs's confirmation die with it, and the service is left
        stopped. With this off, a changed agent keeps running the old
        version until `systemctl restart deptui-agent` (or a reboot);
        turn it on only if this agent never deploys the host it runs
        on.
      '';
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open the kick/status listener's port.";
    };

    generateSshKey = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        When no `sshKeyFile` is set (and the dedicated system user is
        in use), generate an ed25519 keypair in the agent's state
        directory on first start — like SSH host keys, the private
        half never leaves the machine, so no secret management is
        needed at all. Read the public half with `deptui-agent pubkey`
        (or from the service log) and authorize it on the targets.
      '';
    };

    hostKeyChecking = lib.mkOption {
      type = lib.types.enum [
        "accept-new"
        "strict"
      ];
      default = "accept-new";
      description = ''
        Host-key policy for the agent's ssh (dedicated user only).
        `accept-new` trusts a host on first contact and pins it from
        then on — no manual known_hosts step; a *changed* key is still
        rejected. `strict` requires every host key pre-pinned (e.g.
        via programs.ssh.knownHosts).
      '';
    };

    sshKeyFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        Private key the agent uses to reach its targets (and private
        repositories). Written as an IdentityFile rule into the service
        user's ssh config. Targets must accept non-interactive
        activation — a headless daemon cannot answer prompts.
      '';
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = defaultUser;
      description = ''
        User the daemon runs as. The default dedicated system user is
        created automatically; set an existing user instead to reuse
        its ssh identity and known_hosts.
      '';
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = defaultUser;
      description = ''
        Group owning the control socket (mode 0660) — membership is
        what grants deptui users control over the agent.
      '';
    };

    users = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "alice" ];
      description = ''
        Users who may control the agent: each is added to the socket
        group. The deploy ssh user needs this (unless it is root) for
        `ssh host deptui-agent …` — the TUI's transport and what its
        agent discovery probes.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = !cfg.listen.enable || cfg.listen.tokenFile != null;
        message = "services.deptui-agent.listen.enable requires listen.tokenFile";
      }
      {
        assertion = !cfg.openFirewall || cfg.listen.enable;
        message = "services.deptui-agent.openFirewall without listen.enable opens nothing";
      }
    ];

    # The CLI's default --config path, so `deptui-agent validate` /
    # `check` work over ssh without hunting down the store path the
    # service was started with.
    environment.etc."deptui-agent/config.toml".source = configFile;

    # The CLI must be reachable over `ssh host deptui-agent …` — that
    # is the TUI's remote-control transport and what its agent
    # discovery probes. The service alone runs fine from the store
    # path, but without this the agent is invisible to clients.
    environment.systemPackages = [ cfg.package ];

    users.users = lib.mkMerge [
      (lib.mkIf (cfg.user == defaultUser) {
        ${defaultUser} = {
          isSystemUser = true;
          group = cfg.group;
          home = "/var/lib/deptui-agent";
          description = "deptui auto-deploy agent";
        };
      })
      (lib.genAttrs cfg.users (_: {
        extraGroups = [ cfg.group ];
      }))
    ];
    users.groups = lib.mkIf (cfg.group == defaultUser) { ${defaultUser} = { }; };

    # $HOME/.ssh/config for the dedicated service user: host-key policy
    # plus the identity, so git and the deploys pick both up without
    # per-host repetition. A custom `user` brings their own ~/.ssh —
    # none of this applies there.
    systemd.tmpfiles.rules = lib.mkIf (cfg.user == defaultUser) [
      "d /var/lib/deptui-agent/.ssh 0700 ${cfg.user} ${cfg.group} -"
      "L+ /var/lib/deptui-agent/.ssh/config - - - - ${pkgs.writeText "deptui-agent-ssh-config" (
        ''
          Host *
            StrictHostKeyChecking ${if cfg.hostKeyChecking == "strict" then "yes" else "accept-new"}
        ''
        + lib.optionalString (cfg.sshKeyFile != null) ''
          IdentityFile ${cfg.sshKeyFile}
        ''
      )}"
    ];

    systemd.services.deptui-agent = {
      description = "deptui auto-deploy agent";
      restartIfChanged = cfg.restartOnUpdate;
      # First-start key generation (dedicated user, no sshKeyFile): the
      # private half never leaves the machine, so there is no secret to
      # manage. And when a key IS provided: a passphrase-protected one
      # makes a headless agent silently useless (SSH_ASKPASS=/bin/false
      # skips the prompt; every auth fails as bare "Permission
      # denied") — warn loudly without blocking the service.
      preStart =
        lib.optionalString (cfg.sshKeyFile == null && cfg.generateSshKey && cfg.user == defaultUser)
          ''
            if [ ! -f "$STATE_DIRECTORY/.ssh/id_ed25519" ]; then
              mkdir -p "$STATE_DIRECTORY/.ssh"
              chmod 700 "$STATE_DIRECTORY/.ssh"
              ${pkgs.openssh}/bin/ssh-keygen -t ed25519 -N "" \
                -C "deptui-agent@$(${pkgs.nettools}/bin/hostname)" \
                -f "$STATE_DIRECTORY/.ssh/id_ed25519"
              echo "generated the agent's ssh identity; authorize this public key on the targets:" >&2
              cat "$STATE_DIRECTORY/.ssh/id_ed25519.pub" >&2
            fi
          ''
        + lib.optionalString (cfg.sshKeyFile != null) ''
          if ! ${pkgs.openssh}/bin/ssh-keygen -y -P "" -f ${lib.escapeShellArg cfg.sshKeyFile} >/dev/null 2>&1; then
            echo "WARNING: ${cfg.sshKeyFile} is passphrase-protected or unreadable —" >&2
            echo "         a headless agent cannot use it; every ssh will fail with" >&2
            echo "         'Permission denied'. Strip it: ssh-keygen -p -N \"\" -f <key>" >&2
          fi
        '';
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      environment.HOME = "/var/lib/deptui-agent";
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/deptui-agent --config ${configFile} run";
        User = cfg.user;
        Group = cfg.group;
        StateDirectory = "deptui-agent";
        RuntimeDirectory = "deptui-agent";
        Restart = "on-failure";
        RestartSec = 5;
      };
    };

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ cfg.listen.port ];
  };
}
