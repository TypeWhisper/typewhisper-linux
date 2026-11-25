#!/usr/bin/env bash
# Starts a Plasma Wayland session whose KWin renders to a virtual output, through
# Plasma's own systemd units, so portals and kglobalacceld run as in a real session.
set -euo pipefail

export XDG_RUNTIME_DIR="/run/user/$(id -u)"
export DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/bus"
loginctl enable-linger "$USER"

dropin="${HOME}/.config/systemd/user/plasma-kwin_wayland.service.d"
mkdir -p "${dropin}"
cat > "${dropin}/headless.conf" <<'UNIT'
[Service]
ExecStart=
ExecStart=/usr/bin/kwin_wayland_wrapper --xwayland --virtual --width 1280 --height 800 --no-lockscreen
UNIT
systemctl --user daemon-reload

systemctl --user set-environment XDG_CURRENT_DESKTOP=KDE XDG_SESSION_DESKTOP=KDE \
  XDG_SESSION_TYPE=wayland KDE_FULL_SESSION=true KDE_SESSION_VERSION=6 \
  WAYLAND_DISPLAY=wayland-1 QT_QPA_PLATFORM=wayland
systemctl --user start pipewire.socket pipewire-pulse.socket wireplumber.service || true
systemctl --user start plasma-workspace-wayland.target

for _ in $(seq 1 60); do
  if [ -S "${XDG_RUNTIME_DIR}/wayland-1" ] && busctl --user status org.kde.KWin >/dev/null 2>&1; then
    echo "Plasma session ready"
    exit 0
  fi
  sleep 1
done
echo "Plasma session did not become ready" >&2
journalctl --user --since "-2min" --no-pager | tail -30 >&2
exit 1
