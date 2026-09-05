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
      # folded in.
      watch = lib.mapAttrsToList (name: w: w // { inherit name; }) cfg.watches;
    }
    // lib.optionalAttrs cfg.listen.enable {
      listen = {
        addr = "${cfg.listen.address}:${toString cfg.listen.port}";
        token_file = cfg.listen.tokenFile;
      };
    };

  mergedSettings = lib.recursiveUpdate generatedSettings cfg.settings;
  configFile = settingsFormat.generate "deptui-agent-config.toml" mergedSettings;

  defaultUser = "deptui-agent";
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
      type = lib.types.attrsOf settingsFormat.type;
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

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open the kick/status listener's port.";
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

    # $HOME/.ssh/config for the service user, so both git and the
    # deploys pick the key up without per-host repetition.
    systemd.tmpfiles.rules = lib.mkIf (cfg.sshKeyFile != null) [
      "d /var/lib/deptui-agent/.ssh 0700 ${cfg.user} ${cfg.group} -"
      "L+ /var/lib/deptui-agent/.ssh/config - - - - ${pkgs.writeText "deptui-agent-ssh-config" ''
        IdentityFile ${cfg.sshKeyFile}
      ''}"
    ];

    systemd.services.deptui-agent = {
      description = "deptui auto-deploy agent";
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
