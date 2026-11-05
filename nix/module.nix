# services.facade: a facade server (control plane, also running apps on this
# machine) or an extra node's agent, the way deploy/install.sh sets them up on
# Ubuntu: facaded-ingress serves the machine's apps as its own service
# (`ingress = "external"`), so restarting facaded never touches it.
#
# Nix owns the binaries: facade sees it runs from /nix/store and never updates
# itself (`facade server update`, Settings → Updates). To update, bump the facade
# flake input and `nixos-rebuild switch`.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.facade;
  inherit (lib)
    literalExpression
    mkEnableOption
    mkIf
    mkMerge
    mkOption
    optional
    optionalAttrs
    types
    ;
  system = pkgs.stdenv.hostPlatform.system;

  # Agents always use facaded-ingress; a server unless `ingress = "embedded"`.
  ingressService = cfg.agent.enable || cfg.server.settings.ingress == "external";

  toml = pkgs.formats.toml { };
  # TOML has no null: an unset option is left out, so facaded uses its default.
  settingsFile = toml.generate "server.toml" (
    lib.filterAttrsRecursive (_: v: v != null) cfg.server.settings
  );
  configPath = "/etc/facade/server.toml";

  # The agent's data directory, where volumes with a size are kept.
  agentDir =
    if cfg.server.enable then "${cfg.server.settings.data_dir}/agent" else "/var/lib/facade-agent";

  # What facaded runs: git (over SSH for deploy keys), the Docker CLI (builds,
  # disk usage, pruning), the tools for volumes with a size (mkfs.ext4,
  # resize2fs, losetup, mount), systemctl and journalctl, nix-store, and last
  # the system's own (nixos-version, which no option exposes).
  servicePath = [
    pkgs.git
    pkgs.openssh
    pkgs.e2fsprogs
    pkgs.util-linux
    config.systemd.package
    config.virtualisation.docker.package
    config.nix.package
    "/run/current-system/sw"
  ];

  environment = {
    RUST_LOG = "info";
  }
  // optionalAttrs (cfg.nixpkgsBranch != null) { FACADE_NIXPKGS_BRANCH = cfg.nixpkgsBranch; };

  daemon = {
    Restart = "always";
    RestartSec = 3;
    LimitNOFILE = 65536;
  };

  # `facaded agent --join` is only needed once: the token is then kept in the
  # data directory. Until it is, read it from joinTokenFile (an environment
  # variable, not an argument everyone could read in `ps`).
  agentStart = pkgs.writeShellScript "facaded-agent-start" ''
    set -eu
    ${lib.optionalString (cfg.agent.joinTokenFile != null) ''
      if [ ! -s ${agentDir}/join-token ]; then
        if [ ! -r ${lib.escapeShellArg cfg.agent.joinTokenFile} ]; then
          echo "facade: not joined yet and ${cfg.agent.joinTokenFile} is missing" >&2
          exit 1
        fi
        FACADE_JOIN=$(cat ${lib.escapeShellArg cfg.agent.joinTokenFile})
        export FACADE_JOIN
      fi
    ''}
    exec ${lib.getExe' cfg.package "facaded"} agent \
      --data-dir ${agentDir} --ingress-control /run/facade/ingress.sock
  '';
in
{
  options.services.facade = {
    package = mkOption {
      type = types.package;
      default = self.packages.${system}.facaded;
      defaultText = literalExpression "facade.packages.\${system}.facaded";
      description = "The facaded package.";
    };

    cliPackage = mkOption {
      type = types.nullOr types.package;
      default = self.packages.${system}.facade;
      defaultText = literalExpression "facade.packages.\${system}.facade";
      description = "The facade CLI, installed system-wide (null: not installed).";
    };

    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Open TCP 80 and 443 (apps), and on the server 7700 (nodes) and 7701
        (API and web UI with the self-signed certificate).
      '';
    };

    nixpkgsBranch = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "nixos-26.05";
      description = ''
        The nixpkgs branch this machine follows, against which facade reports
        how far behind its system is (FACADE_NIXPKGS_BRANCH).
      '';
    };

    server = {
      enable = mkEnableOption "the facade server (control plane, also running apps on this machine)";

      settings = mkOption {
        description = ''
          /etc/facade/server.toml (every option: deploy/server.example.toml in the
          facade repository). The server never writes this file. `facade server set`
          (or Settings → Server access) keeps the API domain and the ACME e-mail in
          the server's database, which wins over the values here from then on.
        '';
        default = { };
        example = literalExpression ''
          {
            public_host = "203.0.113.7";
            api_domain = "facade.example.com";
            acme.email = "you@example.com";
          }
        '';
        type = types.submodule {
          freeformType = toml.type;
          options = {
            public_host = mkOption {
              type = types.nullOr types.str;
              default = null;
              description = "Address other nodes reach the server at (IP or hostname, no port).";
            };
            api_domain = mkOption {
              type = types.nullOr types.str;
              default = null;
              description = "Domain serving the web UI and API with a Let's Encrypt certificate.";
            };
            ingress = mkOption {
              type = types.enum [
                "external"
                "embedded"
              ];
              default = "external";
              description = ''
                `external`: apps are served by facaded-ingress.service, so restarting
                facaded never drops traffic. `embedded`: by facaded itself.
              '';
            };
            data_dir = mkOption {
              type = types.str;
              default = "/var/lib/facade";
              description = "The server's state (database, key, backups, its agent's data).";
            };
            acme = mkOption {
              default = { };
              type = types.submodule {
                freeformType = toml.type;
                options.email = mkOption {
                  type = types.nullOr types.str;
                  default = null;
                  description = "Contact address for Let's Encrypt (expiry notices).";
                };
              };
            };
          };
        };
      };
    };

    agent = {
      enable = mkEnableOption "a facade agent: an extra node of a facade server";

      joinTokenFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "/run/secrets/facade-join";
        description = ''
          A file holding the join token from `facade node add <name>` (keep it out
          of the Nix store). Read while the node has not joined yet; the agent then
          keeps the token in /var/lib/facade-agent/join-token. To join again with
          another token, delete that file and restart facaded-agent.
        '';
      };
    };
  };

  config = mkIf (cfg.server.enable || cfg.agent.enable) (mkMerge [
    {
      assertions = [
        {
          assertion = !(cfg.server.enable && cfg.agent.enable);
          message = "services.facade: enable server or agent, not both (the server runs its own agent).";
        }
      ];

      virtualisation.docker.enable = true;

      environment.systemPackages = [ cfg.package ] ++ optional (cfg.cliPackage != null) cfg.cliPackage;

      networking.firewall.allowedTCPPorts = mkIf cfg.openFirewall (
        [
          80
          443
        ]
        ++ lib.optionals cfg.server.enable [
          7700
          7701
        ]
      );

      # The node's entry point (HTTP and HTTPS). Reads http_listen, https_listen and
      # ingress_control from the server config when there is one.
      #
      # A rebuild with a new facade reloads it instead of restarting it: on reload
      # (SIGUSR2) it starts the binary at the path it was started from with its
      # listening sockets, and the old process finishes its open connections,
      # as `facade server update` does on Ubuntu. That path is /etc/facade/facaded,
      # which the rebuild points to the new package before it reloads services.
      # For the reload not to turn into a restart, the unit has no store path in
      # it (no default PATH: it runs nothing), so only the package as a reload
      # trigger, and the listen settings as restart triggers, change it.
      environment.etc."facade/facaded".source = lib.getExe' cfg.package "facaded";

      systemd.services.facaded-ingress = mkIf ingressService {
        description = "facade entry point (HTTP and HTTPS)";
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        wantedBy = [ "multi-user.target" ];
        inherit environment;
        enableDefaultPath = false;
        reloadTriggers = [ cfg.package ];
        # Only what it reads from server.toml: other settings leave traffic alone.
        restartTriggers = lib.optional cfg.server.enable (
          builtins.toJSON (
            lib.genAttrs [ "http_listen" "https_listen" "ingress_control" ] (
              k: cfg.server.settings.${k} or null
            )
          )
        );
        serviceConfig = {
          ExecStart = "/etc/facade/facaded ingress --config ${configPath}";
          ExecReload = "/run/current-system/sw/bin/kill -USR2 $MAINPID";
          Type = "notify";
          NotifyAccess = "all";
          Restart = "always";
          RestartSec = 2;
          LimitNOFILE = 65536;
        };
      };

      # Mounts the volumes with a size (loop-mounted image files) before Docker
      # starts the containers using them. facade writes this unit itself on other
      # systems; on NixOS it leaves it to this module.
      systemd.services.facade-volumes = {
        description = "facade volumes with a size";
        after = [ "local-fs.target" ];
        before = [ "docker.service" ];
        wantedBy = [ "multi-user.target" ];
        path = servicePath;
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${lib.getExe' cfg.package "facaded"} mount-volumes ${agentDir}";
        };
      };
    }

    (mkIf cfg.server.enable {
      environment.etc."facade/server.toml".source = settingsFile;

      # Docker is wanted, not required (as in deploy/facaded.service): with
      # Requires=, Docker failing once at boot (it restarts by itself) would leave
      # facaded down for good. Without Docker its agent keeps retrying.
      systemd.services.facaded = {
        description = "facade server";
        after = [
          "network-online.target"
          "docker.service"
        ];
        wants = [
          "network-online.target"
          "docker.service"
        ]
        ++ optional ingressService "facaded-ingress.service";
        wantedBy = [ "multi-user.target" ];
        path = servicePath;
        inherit environment;
        restartTriggers = [ settingsFile ];
        serviceConfig = daemon // {
          ExecStart = "${lib.getExe' cfg.package "facaded"} server --config ${configPath}";
        };
      };
    })

    (mkIf cfg.agent.enable {
      systemd.services.facaded-agent = {
        description = "facade agent";
        after = [
          "network-online.target"
          "docker.service"
        ];
        wants = [
          "network-online.target"
          "docker.service"
          "facaded-ingress.service"
        ];
        wantedBy = [ "multi-user.target" ];
        path = servicePath;
        inherit environment;
        serviceConfig = daemon // {
          ExecStart = agentStart;
        };
      };
    })
  ]);
}
