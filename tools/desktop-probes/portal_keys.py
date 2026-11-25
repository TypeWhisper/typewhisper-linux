"""Press a key chord through the RemoteDesktop portal (test driver for desktops
without Mutter's private API, such as KDE).

  portal_keys.py <token-file> <hold-seconds> <keysym-hex>..."""
import os
import sys
import time

from gi.repository import Gio, GLib

from portal_common import BUS_NAME, Portal, fail, log

RD = "org.freedesktop.portal.RemoteDesktop"
token_file, hold = sys.argv[1], float(sys.argv[2])
keys = [int(k, 16) for k in sys.argv[3:]]

portal = Portal("com.typewhisper.TypeWhisper.SpikeDriver")
code, results = portal.request(RD, "CreateSession", "(a{sv})", (),
                               {"session_handle_token": GLib.Variant("s", portal.token())})
session = results["session_handle"]
select = {"types": GLib.Variant("u", 1), "persist_mode": GLib.Variant("u", 2)}
if os.path.exists(token_file):
    select["restore_token"] = GLib.Variant("s", open(token_file).read().strip())
code, _ = portal.request(RD, "SelectDevices", "(oa{sv})", (session,), select)
code, results = portal.request(RD, "Start", "(osa{sv})", (session, ""), {}, timeout=120)
if code != 0:
    fail(f"Start {code}")
if "restore_token" in results:
    open(token_file, "w").write(results["restore_token"])

ctx = GLib.MainContext.default()


def pump(seconds):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        ctx.iteration(False)
        time.sleep(0.005)


def key(sym, down):
    portal.call(RD, "NotifyKeyboardKeysym", "(oa{sv}iu)", (session, {}, sym, 1 if down else 0))
    pump(0.05)


for k in keys:
    key(k, True)
log("pressed")
pump(hold)
for k in reversed(keys):
    key(k, False)
log("released")
pump(0.2)
portal.bus.call_sync(BUS_NAME, session, "org.freedesktop.portal.Session", "Close",
                     None, None, Gio.DBusCallFlags.NONE, -1, None)
