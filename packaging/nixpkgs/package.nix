{
  lib,
  rustPlatform,
  fetchFromGitHub,
  pkg-config,
  wrapGAppsHook4,
  glib,
  gtk4,
  gsettings-desktop-schemas,
  coreutils,
  makeBinaryWrapper,
  nix-update-script,
}:
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "nixos-update-notifier";
  version = "0-unstable-2026-07-30";

  src = fetchFromGitHub {
    owner = "stuckj";
    repo = "nixos-update-notifier";
    rev = "b3f411f1665a64ce2cd8b4dbaa1d248349cd4aec";
    hash = "sha256-ZQRnfeKT8cbRJWf2jOk1JjffMiQ/zc6ygaFykAGCK6Q=";
  };

  cargoHash = "sha256-4T0YWIyCzK6rgP4ovUq1CyRNnYInKFZ3JrGRVhZb2qE=";

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

  dontWrapGApps = true;

  postInstall = ''
    install -Dm644 $src/share/applications/nixos-update-notifier.desktop \
      $out/share/applications/nixos-update-notifier.desktop
  '';

  postFixup = ''
    wrapProgram $out/bin/nixos-update-notifier-gtk \
      "''${gappsWrapperArgs[@]}"

    wrapProgram $out/bin/nixos-update-notifier \
      --suffix PATH : ${lib.makeBinPath [ coreutils ]}
  '';

  passthru.updateScript = nix-update-script { };

  meta = {
    description = "System-tray update notifier for flake-based NixOS systems";
    homepage = "https://github.com/stuckj/nixos-update-notifier";
    license = lib.licenses.mit;
    mainProgram = "nixos-update-notifier";
    maintainers = [ ];
    platforms = lib.platforms.linux;
  };
})
