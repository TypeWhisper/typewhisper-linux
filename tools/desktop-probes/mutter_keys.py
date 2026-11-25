"""Simulate a physical key chord in the headless GNOME session through Mutter's
own remote desktop D-Bus API (the one gnome-remote-desktop uses). Test helper only.

  mutter_keys.py chord <hold-seconds> <keysym-hex>...   press in order, hold, release in reverse
"""
import sys
import time

from gi.repository import Gio, GLib

bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
NAME = "org.gnome.Mutter.RemoteDesktop"


def call(path, iface, method, fmt=None, args=None, reply=None):
    result = bus.call_sync(NAME, path, iface, method,
                           GLib.Variant(fmt, args) if fmt else None,
                           GLib.VariantType(reply) if reply else None,
                           Gio.DBusCallFlags.NONE, -1, None)
    return result.unpack() if result else None


session = call("/org/gnome/Mutter/RemoteDesktop", NAME, "CreateSession", reply="(o)")[0]
siface = NAME + ".Session"
call(session, siface, "Start")
time.sleep(0.3)

cmd = sys.argv[1]
if cmd == "chord":
    hold = float(sys.argv[2])
    keys = [int(k, 16) for k in sys.argv[3:]]
    for k in keys:
        call(session, siface, "NotifyKeyboardKeysym", "(ub)", (k, True))
        time.sleep(0.05)
    print(f"[{time.monotonic():10.3f}] pressed", flush=True)
    time.sleep(hold)
    for k in reversed(keys):
        call(session, siface, "NotifyKeyboardKeysym", "(ub)", (k, False))
        time.sleep(0.05)
    print(f"[{time.monotonic():10.3f}] released", flush=True)

time.sleep(0.3)
call(session, siface, "Stop")
