# scripts/plasma-smoke-test.sh, run unattended in a NixOS VM with a real Plasma 6 Wayland
# session: the tray watcher, the notification server and the compositor are Plasma's own.
#
# Needs KVM, so it is not part of `checks` (`nix flake check` must work without it).
#   nix build -L .#vmTests.plasma-smoke                # pass/fail
#   nix build .#vmTests.plasma-smoke.driver && ./result/bin/nixos-test-driver --output_directory out
#                                                      # same, keeping screenshots on failure
#
# The daemon checks a copy of the network-free hostflake fixture. It has no
# nixosConfigurations, so the check ends in an error state; the D-Bus service, tray item
# and GTK client still have to work, which is what this test is about.
{
  pkgs,
  package,
}:
pkgs.testers.runNixOSTest {
  name = "plasma-smoke";
  enableOCR = true;

  nodes.machine = {
    users.users.alice = {
      isNormalUser = true;
      uid = 1000;
      password = "foobar";
    };

    services.displayManager.plasma-login-manager.enable = true;
    services.displayManager.defaultSession = "plasma";
    services.displayManager.autoLogin = {
      enable = true;
      user = "alice";
    };
    services.desktopManager.plasma6.enable = true;

    environment.systemPackages = [
      package
      pkgs.libnotify
    ];
    nix.settings.experimental-features = [
      "nix-command"
      "flakes"
    ];

    virtualisation.memorySize = 4096;
    virtualisation.cores = 2;
  };

  testScript =
    let
      fixture = ../../crates/core/tests/fixtures/hostflake;
      script = ../../scripts/plasma-smoke-test.sh;
      config = pkgs.writeText "config.toml" ''
        flake_path = "/home/alice/hostflake"
        host_attr = "smoke"
        interval = 86400
        notify = true
        check_releases = false
      '';
      sessionEnv = builtins.concatStringsSep " " [
        "XDG_RUNTIME_DIR=/run/user/1000"
        "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus"
        "WAYLAND_DISPLAY=wayland-0"
        "XDG_SESSION_TYPE=wayland"
        "XDG_CURRENT_DESKTOP=KDE"
      ];
    in
    ''
      def as_alice(cmd):
          return f"su - alice -c '${sessionEnv} {cmd}'"

      start_all()

      with subtest("Plasma session is up"):
          machine.wait_until_succeeds("pgrep -u alice plasmashell", timeout=600)
          machine.wait_until_succeeds(
              as_alice("busctl --user status org.kde.StatusNotifierWatcher"), timeout=300
          )
          machine.wait_until_succeeds(
              as_alice("busctl --user status org.freedesktop.Notifications"), timeout=300
          )

      with subtest("Fixture flake and config"):
          machine.succeed(
              "cp -r ${fixture} /home/alice/hostflake",
              "cp ${config} /home/alice/config.toml",
              "chmod -R u+w /home/alice/hostflake /home/alice/config.toml",
              "chown -R alice:users /home/alice/hostflake /home/alice/config.toml",
          )

      with subtest("Smoke test script"):
          status, out = machine.execute(
              as_alice(
                  "bash ${script} --non-interactive --keep-running"
                  " --bin-dir /run/current-system/sw/bin --config /home/alice/config.toml"
              ),
              timeout=600,
          )
          print(out)
          assert status == 0, f"plasma-smoke-test.sh exited {status}"

      with subtest("GTK window is on screen and connected"):
          machine.wait_for_text("Pending updates", timeout=120)
          machine.screenshot("plasma-smoke")
          assert "Daemon not running" not in machine.get_screen_text(), \
              "the GTK window says the daemon is not running"
    '';
}
