"""Small helpers for calling XDG desktop portals from probe scripts (GDBus)."""
import secrets
import sys
import time

from gi.repository import Gio, GLib

BUS_NAME = "org.freedesktop.portal.Desktop"
OBJ = "/org/freedesktop/portal/desktop"


def log(*parts):
    print(f"[{time.monotonic():10.3f}]", *parts, flush=True)


class Portal:
    def __init__(self, app_id):
        self.bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        self.loop = GLib.MainLoop()
        self.sender = self.bus.get_unique_name()[1:].replace(".", "_")
        # Host (unsandboxed) apps tell the portal their app ID before any other call.
        try:
            self.bus.call_sync(
                BUS_NAME, OBJ, "org.freedesktop.host.portal.Registry", "Register",
                GLib.Variant("(sa{sv})", (app_id, {})), None, Gio.DBusCallFlags.NONE, -1, None)
            log("registered app id", app_id)
        except GLib.Error as error:
            log("registry not available:", error.message)

    def token(self):
        return "tw" + secrets.token_hex(6)

    def request(self, interface, method, args_fmt, args, options, timeout=120):
        """Calls a portal method that answers through a Request object and waits for Response."""
        handle_token = self.token()
        options = dict(options)
        options["handle_token"] = GLib.Variant("s", handle_token)
        request_path = f"/org/freedesktop/portal/desktop/request/{self.sender}/{handle_token}"
        result = {}

        def on_response(_conn, _sender, _path, _iface, _signal, params):
            code, results = params.unpack()
            result["code"] = code
            result["results"] = results
            self.loop.quit()

        sub = self.bus.signal_subscribe(
            BUS_NAME, "org.freedesktop.portal.Request", "Response", request_path,
            None, Gio.DBusSignalFlags.NO_MATCH_RULE, on_response)
        variant = GLib.Variant(args_fmt, (*args, options))
        self.bus.call_sync(BUS_NAME, OBJ, interface, method, variant, None,
                           Gio.DBusCallFlags.NONE, -1, None)
        GLib.timeout_add_seconds(timeout, self.loop.quit)
        self.loop.run()
        self.bus.signal_unsubscribe(sub)
        if "code" not in result:
            raise TimeoutError(f"{interface}.{method}: no response within {timeout}s")
        return result["code"], result["results"]

    def call(self, interface, method, args_fmt, args, reply_fmt=None):
        reply = self.bus.call_sync(
            BUS_NAME, OBJ, interface, method,
            GLib.Variant(args_fmt, args) if args_fmt else None,
            GLib.VariantType(reply_fmt) if reply_fmt else None,
            Gio.DBusCallFlags.NONE, -1, None)
        return reply.unpack() if reply else None


def fail(message):
    log("FAIL", message)
    sys.exit(1)
