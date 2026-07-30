# Home-manager module. Import via the flake's `homeManagerModules.default`.
#
# A plain module taking no flake argument: the package is built from the importer's own
# `pkgs`, so using this module never pulls our nixpkgs into your closure.
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
      # Built from YOUR nixpkgs — see the note in nixos-module.nix.
      default = pkgs.callPackage ./package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { }";
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
      # Enforce the daemon's own minimum here so an invalid value fails at evaluation
      # rather than at runtime.
      type = lib.types.addCheck lib.types.int (x: x >= 60) // {
        description = "integer of at least 60 (seconds)";
      };
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

    # NOTE: there is deliberately no `rebuildExtraArgs`. Free-form arguments forwarded into
    # root's `nixos-rebuild` (--override-input, -I, --substituters, …) would let anything
    # that can write the config change what root evaluates and builds, which the polkit
    # prompt does not show. Add a typed, vetted option if a specific flag is ever needed.

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
