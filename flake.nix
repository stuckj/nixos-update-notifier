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

          # Handy for validating the no-download closure diff by hand.
          packages = [
            pkgs.nvd
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

      # Import as inputs.nixos-update-notifier.homeManagerModules.default
      homeManagerModules.default = import ./nix/hm-module.nix self;
      # Import as inputs.nixos-update-notifier.nixosModules.default
      nixosModules.default = import ./nix/nixos-module.nix self;

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
