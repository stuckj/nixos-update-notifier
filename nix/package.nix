{
  lib,
  rustPlatform,
  pkg-config,
  wrapGAppsHook4,
  glib,
  gtk4,
  gsettings-desktop-schemas,
  # Runtime tools we shell out to. `nix` and `nixos-rebuild` are intentionally NOT bundled
  # — they must be the system's so the store/rebuild stay consistent. We only ensure a few
  # generic helpers are reachable via a wrapped PATH suffix.
  coreutils,
  makeBinaryWrapper,
}:
rustPlatform.buildRustPackage {
  pname = "nixos-update-notifier";
  version = "0.1.0";

  # Cargo workspace: builds both the `nixos-update-notifier` (daemon+CLI) and
  # `nixos-update-notifier-gtk` (GUI client) binaries. Only the GTK crate links GTK.
  src = lib.cleanSource ../.;

  # Cargo.lock must exist in the source tree. Generate it once in the devShell with
  # `cargo generate-lockfile` (needs network) and commit it.
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    wrapGAppsHook4
    makeBinaryWrapper
  ];

  buildInputs = [
    glib
    gtk4
    gsettings-desktop-schemas
  ];

  # We wrap the binary ourselves so we can merge the GApps env with a PATH suffix in a
  # single wrapper (avoids the double-wrap you'd get from the automatic hook).
  dontWrapGApps = true;

  # Install the desktop/autostart entry.
  postInstall = ''
    install -Dm644 $src/share/applications/nixos-update-notifier.desktop \
      $out/share/applications/nixos-update-notifier.desktop
  '';

  # Wrap each binary for what it actually needs:
  #  - the GTK client gets the GApps env (schemas, icons, …);
  #  - the daemon gets a PATH suffix guaranteeing `cp` (coreutils). `nix`,
  #    `nixos-rebuild` and `pkexec` come from the running system's PATH (the systemd unit
  #    sets it explicitly); the daemon re-execs the sibling GTK binary from the same bin/.
  postFixup = ''
    wrapProgram $out/bin/nixos-update-notifier-gtk \
      "''${gappsWrapperArgs[@]}"

    wrapProgram $out/bin/nixos-update-notifier \
      --suffix PATH : ${lib.makeBinPath [ coreutils ]}
  '';

  meta = {
    description = "System-tray update notifier for flake-based NixOS systems";
    homepage = "https://github.com/stuckj/nixos-update-notifier";
    license = lib.licenses.mit;
    mainProgram = "nixos-update-notifier";
    platforms = lib.platforms.linux;
  };
}
