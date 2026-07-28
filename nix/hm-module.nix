# Home-manager module. Import via the flake's `homeManagerModules.default`, which passes
# `self` so the default package resolves to this flake's build.
self:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.nixos-update-notifier;

  # Assemble the TOML the daemon reads, from the module options.
  tomlFormat = pkgs.formats.toml { };
  configFile = tomlFormat.generate "nixos-update-notifier.toml" (
    {
      flake_path = cfg.flakePath;
      host_attr = cfg.hostAttr;
      update_inputs = cfg.updateInputs;
      exclude_inputs = cfg.excludeInputs;
      interval = cfg.interval;
      notify = cfg.notify;
      rebuild_extra_args = cfg.rebuildExtraArgs;
    }
    // lib.optionalAttrs (cfg.nixpkgsRefForChangelogs != null) {
      nixpkgs_ref_for_changelogs = cfg.nixpkgsRefForChangelogs;
    }
    // lib.optionalAttrs (cfg.icons != null) { icons = cfg.icons; }
  );

  # Minimal but sufficient PATH for the daemon: system nix + nixos-rebuild live in
  # /run/current-system/sw/bin, the setuid pkexec wrapper in /run/wrappers/bin, plus the
  # package's own bin (so it can re-exec itself for the GTK subprocesses).
  daemonPath = lib.concatStringsSep ":" [
    "/run/wrappers/bin"
    "/run/current-system/sw/bin"
    "${cfg.package}/bin"
  ];
in
{
  options.services.nixos-update-notifier = {
    enable = lib.mkEnableOption "the NixOS flake update tray notifier";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "nixos-update-notifier.packages.\${system}.default";
      description = "The nixos-update-notifier package to use.";
    };

    flakePath = lib.mkOption {
      type = lib.types.str;
      example = "/home/you/dev/personal/nixos-config";
      description = "Absolute path to the flake repo (directory containing flake.nix).";
    };

    hostAttr = lib.mkOption {
      type = lib.types.str;
      example = "nixos-x1";
      description = "The nixosConfigurations.<name> attribute to evaluate and rebuild.";
    };

    updateInputs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = ''
        Inputs to advance. Empty means "all inputs except excludeInputs". Non-empty means
        "only these inputs (minus excludeInputs)".
      '';
    };

    excludeInputs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "nixpkgs-kernel" ];
      description = "Inputs that must NEVER be advanced (e.g. a rev-pinned kernel input).";
    };

    interval = lib.mkOption {
      type = lib.types.ints.positive;
      default = 21600;
      description = "Background check cadence, in seconds (minimum 60).";
    };

    notify = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Fire desktop notifications when updates appear.";
    };

    nixpkgsRefForChangelogs = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "github:NixOS/nixpkgs/nixos-24.11";
      description = "nixpkgs flake ref to resolve meta.changelog against. Null = registry \"nixpkgs\".";
    };

    rebuildExtraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Extra arguments forwarded to nixos-rebuild on apply.";
    };

    icons = lib.mkOption {
      type = lib.types.nullOr (lib.types.attrsOf lib.types.str);
      default = null;
      example = {
        idle = "nix-snowflake";
        updates_available = "software-update-available";
      };
      description = "Optional freedesktop icon-name overrides per tray state.";
    };

    timer = {
      enable = lib.mkEnableOption "an additional systemd user timer that triggers checks";
      onCalendar = lib.mkOption {
        type = lib.types.str;
        default = "hourly";
        description = "systemd OnCalendar expression for the extra check timer.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."nixos-update-notifier/config.toml".source = configFile;

    systemd.user.services.nixos-update-notifier = {
      Unit = {
        Description = "NixOS flake update tray notifier";
        After = [ "graphical-session.target" ];
        PartOf = [ "graphical-session.target" ];
      };
      Service = {
        ExecStart = "${cfg.package}/bin/nixos-update-notifier run";
        Environment = [ "PATH=${daemonPath}" ];
        Restart = "on-failure";
        RestartSec = 5;
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };

    # Optional extra timer: pokes the running daemon (SIGUSR1) to force a check, on top of
    # the daemon's own internal interval.
    systemd.user.services.nixos-update-notifier-check = lib.mkIf cfg.timer.enable {
      Unit.Description = "Trigger a NixOS update check";
      Service = {
        Type = "oneshot";
        ExecStart = "${pkgs.systemd}/bin/systemctl --user kill -s SIGUSR1 nixos-update-notifier.service";
      };
    };
    systemd.user.timers.nixos-update-notifier-check = lib.mkIf cfg.timer.enable {
      Unit.Description = "Periodic NixOS update check trigger";
      Timer = {
        OnCalendar = cfg.timer.onCalendar;
        Persistent = true;
      };
      Install.WantedBy = [ "timers.target" ];
    };
  };
}
