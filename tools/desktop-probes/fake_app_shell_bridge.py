"""Stand-in for the TypeWhisper app side of the Shell extension protocol.

Owns com.typewhisper.TypeWhisper, logs FocusChanged calls from the extension and
emits StateChanged. Commands arrive as lines in a FIFO: "<state> <text>"."""
import os
import sys
import threading
import time

from gi.repository import Gio, GLib

NAME = "com.typewhisper.TypeWhisper"
PATH = "/com/typewhisper/TypeWhisper"
IFACE = "com.typewhisper.TypeWhisper.Shell"
XML = f"""<node><interface name="{IFACE}">
  <method name="FocusChanged"><arg type="u" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/></method>
  <signal name="StateChanged"><arg type="u"/><arg type="s"/><arg type="s"/></signal>
</interface></node>"""
LOG, FIFO = sys.argv[1], sys.argv[2]


def log(*parts):
    with open(LOG, "a", encoding="utf-8") as fh:
        fh.write(f"[{time.monotonic():10.3f}] " + " ".join(str(p) for p in parts) + "\n")


bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
node = Gio.DBusNodeInfo.new_for_xml(XML)


def on_call(_conn, sender, _path, _iface, method, params, invocation):
    if method == "FocusChanged":
        version, app_id, title = params.unpack()
        log("FocusChanged from", sender, "v", version, repr(app_id), repr(title))
    invocation.return_value(None)


bus.register_object(PATH, node.interfaces[0], on_call, None, None)
Gio.bus_own_name_on_connection(bus, NAME, Gio.BusNameOwnerFlags.NONE, lambda *a: log("owns", NAME), None)

if not os.path.exists(FIFO):
    os.mkfifo(FIFO)


def reader():
    while True:
        with open(FIFO, encoding="utf-8") as fh:
            for line in fh:
                state, _, text = line.strip().partition(" ")
                GLib.idle_add(emit, state, text)


def emit(state, text):
    bus.emit_signal(None, PATH, IFACE, "StateChanged", GLib.Variant("(uss)", (1, state, text)))
    log("StateChanged", state, repr(text))
    return False


threading.Thread(target=reader, daemon=True).start()
GLib.MainLoop().run()
