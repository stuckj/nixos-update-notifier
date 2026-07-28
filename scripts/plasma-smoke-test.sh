#!/usr/bin/env bash
# Manual smoke test for a real KDE Plasma 6 / Wayland session.
#
# Verifies the parts that can't be exercised headlessly: the SNI tray registration, the
# D-Bus service, desktop notifications, and the GTK client connecting to the daemon. It is
# strictly READ-ONLY — it never applies an update.
#
# Usage:
#   scripts/plasma-smoke-test.sh --config ~/.config/nixos-update-notifier/config.toml
#   scripts/plasma-smoke-test.sh --flake ~/dev/personal/nixos-config --host nixos-x1
#   scripts/plasma-smoke-test.sh --bin-dir ./result/bin   # after `nix build`
#
# If a daemon is already running (e.g. the systemd user service), it is reused and no
# config is needed. Otherwise one is started from --config, or from --flake + --host.
set -uo pipefail

CONFIG="" FLAKE="" HOST="" BIN_DIR=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --config)  CONFIG="$2"; shift 2 ;;
    --flake)   FLAKE="$2"; shift 2 ;;
    --host)    HOST="$2"; shift 2 ;;
    --bin-dir) BIN_DIR="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

PASS=0 FAIL=0
ok()   { echo "  [PASS] $*"; PASS=$((PASS + 1)); }
no()   { echo "  [FAIL] $*"; FAIL=$((FAIL + 1)); }
info() { echo "  [info] $*"; }
ask()  { # ask "<question>" "<label>"
  local a
  read -r -p "  [ ?  ] $1 [y/N] " a
  if [[ "$a" == [yY]* ]]; then ok "$2"; else no "$2"; fi
}

BUS="busctl --user"
SVC="org.nixos.UpdateNotifier"
OBJ="/org/nixos/UpdateNotifier"
IFACE="org.nixos.UpdateNotifier1"

have_bin()   { command -v "$1" >/dev/null 2>&1 || [[ -x "$1" ]]; }
name_owned() { $BUS status "$1" >/dev/null 2>&1; }

command -v busctl >/dev/null || { echo "busctl (systemd) is required" >&2; exit 1; }

echo "== Environment =="
if [[ "${XDG_SESSION_TYPE:-}" == "wayland" ]]; then
  ok "Wayland session"
else
  info "session type is '${XDG_SESSION_TYPE:-unknown}' (the app targets Wayland)"
fi
if [[ "${XDG_CURRENT_DESKTOP:-}" == *KDE* ]]; then
  ok "KDE Plasma desktop"
else
  info "desktop is '${XDG_CURRENT_DESKTOP:-unknown}'"
fi
if name_owned org.kde.StatusNotifierWatcher; then
  ok "StatusNotifierWatcher (SNI host) present"
else
  no "no StatusNotifierWatcher — the tray icon cannot appear"
fi
if name_owned org.freedesktop.Notifications; then
  ok "notification daemon present"
else
  no "no org.freedesktop.Notifications daemon"
fi

echo "== Binaries =="
if [[ -z "$BIN_DIR" ]]; then
  [[ -x ./result/bin/nixos-update-notifier ]] && BIN_DIR=./result/bin
fi
daemon_bin="nixos-update-notifier"
gtk_bin="nixos-update-notifier-gtk"
if [[ -n "$BIN_DIR" ]]; then
  daemon_bin="$BIN_DIR/nixos-update-notifier"
  gtk_bin="$BIN_DIR/nixos-update-notifier-gtk"
fi
if have_bin "$daemon_bin"; then ok "found daemon: $daemon_bin"; else no "daemon binary not found (pass --bin-dir or install it)"; fi
if have_bin "$gtk_bin"; then ok "found GTK client: $gtk_bin"; else no "GTK client binary not found"; fi

echo "== Daemon =="
STARTED=0 DPID="" LOG=""
if name_owned "$SVC"; then
  ok "a daemon is already running — reusing it"
else
  cfg="$CONFIG"
  if [[ -z "$cfg" ]]; then
    if [[ -n "$FLAKE" && -n "$HOST" ]]; then
      cfg="$(mktemp --suffix=.toml)"
      cat >"$cfg" <<EOF
flake_path = "$FLAKE"
host_attr = "$HOST"
interval = 86400
notify = true
EOF
      info "wrote a temporary config: $cfg"
    else
      no "no daemon running and neither --config nor --flake/--host provided"
    fi
  fi
  if [[ -n "$cfg" ]]; then
    LOG="$(mktemp --suffix=.log)"
    info "starting: $daemon_bin --config $cfg run  (log: $LOG)"
    "$daemon_bin" --config "$cfg" run >"$LOG" 2>&1 &
    DPID=$!; STARTED=1
    for _ in $(seq 1 40); do
      name_owned "$SVC" && break
      kill -0 "$DPID" 2>/dev/null || break
      sleep 0.5
    done
    if name_owned "$SVC"; then
      ok "daemon started and owns the D-Bus name"
    else
      no "daemon did not register within 20s (see $LOG)"
    fi
  fi
fi

echo "== D-Bus service =="
if $BUS introspect "$SVC" "$OBJ" 2>/dev/null | grep -q "$IFACE"; then
  ok "interface $IFACE exported at $OBJ"
else
  no "interface $IFACE not found (daemon up?)"
fi
if st=$($BUS call "$SVC" "$OBJ" "$IFACE" GetStatus 2>&1); then
  ok "GetStatus -> $st"
else
  no "GetStatus failed: $st"
fi
if $BUS call "$SVC" "$OBJ" "$IFACE" GetUpdates >/dev/null 2>&1; then
  ok "GetUpdates responded"
else
  no "GetUpdates failed"
fi

# Regression guard. The command loop used to await the check inline, so for the whole
# duration of a check (40s to minutes, and one runs at startup) the daemon accepted no menu
# actions: clicks queued silently and then all fired at once when the check finished. It
# looked exactly like "the tray menu is broken". SIGUSR1 is a command the loop must handle,
# so if the daemon is mid-check and still logs/handles it promptly, the loop is not blocked.
echo "== Responsiveness while checking =="
status_now() { $BUS call "$SVC" "$OBJ" "$IFACE" GetStatus 2>/dev/null; }
if [[ "$(status_now)" == *checking* ]]; then
  if [[ -n "${DPID:-}" ]] && kill -USR1 "$DPID" 2>/dev/null; then
    sleep 2
    if [[ "$(status_now)" == *checking* ]]; then
      ok "daemon still responsive mid-check (accepted SIGUSR1, check undisturbed)"
    else
      info "status changed while probing; inconclusive"
    fi
  else
    info "no daemon PID to signal (reusing an existing daemon); skipping"
  fi
else
  info "daemon is not mid-check right now; skipping this probe"
fi

echo "== Tray (SNI) =="
# An icon name the theme cannot resolve renders as a blank gap and logs nothing anywhere,
# so check the names the item actually publishes against the icon theme. This is how the
# tray silently showed no icon: three of the defaults were plausible freedesktop names that
# Breeze does not ship.
if [[ -n "${DPID:-}" ]]; then
  sni="org.kde.StatusNotifierItem-${DPID}-1"
  for prop in IconName AttentionIconName; do
    name=$($BUS get-property "$sni" /StatusNotifierItem org.kde.StatusNotifierItem "$prop" 2>/dev/null \
             | awk '{print $2}' | tr -d '"')
    if [[ -z "$name" ]]; then
      info "$prop not published (yet)"
      continue
    fi
    found=""
    IFS=':' read -r -a datadirs <<< "${XDG_DATA_DIRS:-/usr/share}"
    for d in "${datadirs[@]}" "$HOME/.local/share" "$HOME/.nix-profile/share" /run/current-system/sw/share; do
      [[ -d "$d/icons" ]] || continue
      if find "$d/icons" -name "${name}.*" -print -quit 2>/dev/null | grep -q .; then found=1; break; fi
    done
    # Plasma resolves against its own Breeze copy, which may not be in our XDG_DATA_DIRS,
    # so a miss here is a warning to check by eye rather than an outright failure.
    if [[ -n "$found" ]]; then ok "$prop '$name' resolves in an icon theme"
    else info "$prop '$name' not found in this process's icon paths — confirm it renders"; fi
  done
fi
items=$($BUS get-property org.kde.StatusNotifierWatcher /StatusNotifierWatcher \
  org.kde.StatusNotifierWatcher RegisteredStatusNotifierItems 2>/dev/null)
if echo "$items" | grep -qi "UpdateNotifier"; then
  ok "our item is registered with the StatusNotifierWatcher"
else
  info "watcher items: ${items:-<none>} (ksni may register under a generated name)"
fi
ask "Is the NixOS update-notifier tray icon visible in your system tray?" "tray icon visible"

echo "== Notifications =="
if command -v notify-send >/dev/null 2>&1; then
  notify-send -a "nixos-update-notifier" "Smoke test" "If you can see this, notifications work."
  ask "Did a test desktop notification appear?" "notifications render"
else
  info "notify-send not installed; skipping (the daemon calls the Notifications D-Bus API directly)"
fi

echo "== GTK client =="
info "launching: $gtk_bin updates"
"$gtk_bin" updates >/dev/null 2>&1 &
GPID=$!
sleep 3
ask "Did the 'NixOS — Pending updates' window open and show a status/list (not 'Daemon not running')?" \
  "GTK client opens and connects over D-Bus"
kill "$GPID" 2>/dev/null

if [[ "$STARTED" -eq 1 && -n "$DPID" ]]; then
  info "stopping the daemon this script started (pid $DPID)"
  kill "$DPID" 2>/dev/null
fi

echo
echo "== Summary: $PASS passed, $FAIL failed =="
[[ "$FAIL" -eq 0 ]]
