"""Spike 2: bind a global shortcut through the GlobalShortcuts portal and log
Activated/Deactivated with timestamps, to see whether hold-to-talk is possible."""
import os
import sys

from gi.repository import Gio, GLib

from portal_common import BUS_NAME, Portal, fail, log

IFACE = "org.freedesktop.portal.GlobalShortcuts"
LISTEN_SECONDS = int(sys.argv[1]) if len(sys.argv) > 1 else 60

portal = Portal("com.typewhisper.TypeWhisper.Spike")
code, results = portal.request(
    IFACE, "CreateSession", "(a{sv})", (),
    {"session_handle_token": GLib.Variant("s", portal.token())})
if code != 0:
    fail(f"CreateSession response {code}")
session = results["session_handle"]
log("session", session)

shortcuts = [("dictate", {
    "description": GLib.Variant("s", "Dictate with TypeWhisper"),
    "preferred_trigger": GLib.Variant("s", os.environ.get("TW_TRIGGER", "<Control><Alt>d")),
})]
code, results = portal.request(
    IFACE, "BindShortcuts", "(oa(sa{sv})sa{sv})", (session, shortcuts, ""), {}, timeout=180)
log("BindShortcuts response", code, results)
if code != 0:
    fail("shortcut not bound")


def on_signal(_conn, _sender, _path, _iface, signal, params):
    values = params.unpack()
    log(signal, values[1], "timestamp_ms", values[2])


portal.bus.signal_subscribe(BUS_NAME, IFACE, None, "/org/freedesktop/portal/desktop",
                            None, Gio.DBusSignalFlags.NONE, on_signal)
log(f"listening {LISTEN_SECONDS}s")
GLib.timeout_add_seconds(LISTEN_SECONDS, portal.loop.quit)
portal.loop.run()
log("done")
