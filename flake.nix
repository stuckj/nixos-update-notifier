{
  description = "System-tray update notifier for flake-based NixOS systems";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.callPackage ./nix/package.nix { };
        nixos-update-notifier = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      });

      # `nix flake check` builds these. The module check exists because the modules are
      # otherwise untested: nothing else evaluates them, so a change to their signature or
      # to the package default would only surface in someone else's configuration.
      checks = forAllSystems (
        pkgs:
        let
          system = pkgs.stdenv.hostPlatform.system;
          # A minimal system that enables the module, so evaluation exercises the option
          # definitions, the polkit rule and the package default. Evaluated, not built —
          # building a whole NixOS toplevel in CI would cost minutes for no extra signal.
          evaluated = (nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              {
                boot.loader.grub.devices = [ "/dev/null" ];
                fileSystems."/" = {
                  device = "/dev/null";
                  fsType = "ext4";
                };
                system.stateVersion = "26.05";
                services.nixos-update-notifier.enable = true;
                services.nixos-update-notifier.polkit.passwordlessUsers = [ "tester" ];
              }
            ];
          }).config;
        in
        {
          # Forces the evaluation above, and records what the package default resolved to.
          nixos-module = pkgs.runCommand "nixos-module-evaluates" { } ''
            echo "package: ${evaluated.services.nixos-update-notifier.package}" > $out
            echo "polkit rules present: ${
              if evaluated.security.polkit.enable then "yes" else "no"
            }" >> $out
          '';
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          name = "nixos-update-notifier-dev";

          # pkg-config resolves the GTK4 deps below for `cargo build`.
          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.rustc
            pkgs.cargo
            pkgs.rustfmt
            pkgs.clippy
            pkgs.rust-analyzer
          ];

          buildInputs = [
            pkgs.glib
            pkgs.gtk4
            pkgs.gsettings-desktop-schemas
          ];

          packages = [
            # Handy for validating the no-download closure diff by hand.
            pkgs.nvd
            # CI lints scripts/*.sh with this; without it in the shell the check can only
            # fail after a push, which is how a formatting-class failure gets noticed late.
            pkgs.shellcheck
          ];

          RUST_LOG = "nixos_update_notifier=debug";
          shellHook = ''
            echo "nixos-update-notifier dev shell"
            echo "  cargo generate-lockfile   # first time, to create Cargo.lock"
            echo "  cargo test                # runs the pure-logic unit tests"
            echo "  cargo run -- check        # one-shot check (downloads nothing)"
          '';
        };
      });

      # Adds `pkgs.nixos-update-notifier`, built from the nixpkgs the overlay is applied to.
      #
      # This flake tracks nixos-unstable for its own devShell and CI, which is no reason to
      # impose that on you: the overlay and both modules build from YOUR nixpkgs, so
      # installing this tool does not add a second nixpkgs to your closure. See README for
      # the `follows` escape hatch if you reference `packages.<system>.default` directly.
      overlays.default = final: _prev: {
        nixos-update-notifier = final.callPackage ./nix/package.nix { };
      };

      # Plain modules — they take no flake argument, so they resolve the package from the
      # importer's `pkgs` rather than from this flake's nixpkgs pin.
      # Import as inputs.nixos-update-notifier.homeManagerModules.default
      homeManagerModules.default = import ./nix/hm-module.nix;
      # Import as inputs.nixos-update-notifier.nixosModules.default
      nixosModules.default = import ./nix/nixos-module.nix;

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
