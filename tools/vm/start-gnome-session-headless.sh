#!/usr/bin/env bash
# Starts a complete GNOME session (gnome-session, settings daemons, portals) whose
# shell runs headless on a virtual monitor. Used over SSH, no display needed.
set -euo pipefail

XDG_RUNTIME_DIR="/run/user/$(id -u)"
export XDG_RUNTIME_DIR
export DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/bus"

# Stop the minimal shell from start-gnome-headless.sh, if it runs.
systemctl --user stop tw-session.target tw-gnome-shell tw-gsd-media-keys tw-gsd-keyboard 2>/dev/null || true

dropin="${HOME}/.config/systemd/user/org.gnome.Shell@.service.d"
mkdir -p "${dropin}"
cat > "${dropin}/headless.conf" <<'UNIT'
[Service]
ExecStart=
ExecStart=/usr/bin/gnome-shell --mode=%i --headless --virtual-monitor 1280x800 --wayland-display=wayland-0
UNIT
systemctl --user daemon-reload

systemctl --user set-environment XDG_CURRENT_DESKTOP=GNOME XDG_SESSION_DESKTOP=gnome \
  XDG_SESSION_TYPE=wayland WAYLAND_DISPLAY=wayland-0
systemctl --user start pipewire.socket pipewire-pulse.socket wireplumber.service || true

if ! systemctl --user is-active --quiet tw-gnome-session; then
  systemd-run --user --unit=tw-gnome-session --collect \
    --setenv=XDG_CURRENT_DESKTOP=GNOME --setenv=XDG_SESSION_TYPE=wayland \
    --setenv=XDG_SESSION_DESKTOP=gnome \
    gnome-session --session=gnome
fi

for _ in $(seq 1 60); do
  if systemctl --user is-active --quiet org.gnome.Shell@user.service && [ -S "${XDG_RUNTIME_DIR}/wayland-0" ]; then
    echo "GNOME session ready"
    exit 0
  fi
  sleep 1
done
echo "GNOME session did not become ready" >&2
journalctl --user --since "-2min" --no-pager | tail -40 >&2
exit 1
