"""Spike 3: insert text into the focused app through the RemoteDesktop portal.

  insert_text.py <mode> <text> [restore-token-file]

mode "paste": put the text on the clipboard through the Clipboard portal, then press Ctrl+V.
mode "type":  send every character as a keysym through NotifyKeyboardKeysym.
The restore token from the first approval is saved to the token file and reused."""
import os
import sys
import time

from gi.repository import Gio, GLib

from portal_common import BUS_NAME, OBJ, Portal, fail, log

RD = "org.freedesktop.portal.RemoteDesktop"
CB = "org.freedesktop.portal.Clipboard"
mode, text = sys.argv[1], sys.argv[2]
token_file = sys.argv[3] if len(sys.argv) > 3 else None

portal = Portal("com.typewhisper.TypeWhisper.Spike")
code, results = portal.request(RD, "CreateSession", "(a{sv})", (),
                               {"session_handle_token": GLib.Variant("s", portal.token())})
if code != 0:
    fail(f"CreateSession {code}")
session = results["session_handle"]
log("session", session)

portal.call(CB, "RequestClipboard", "(oa{sv})", (session, {}))
log("clipboard requested")

select = {"types": GLib.Variant("u", 1), "persist_mode": GLib.Variant("u", 2)}
if token_file and os.path.exists(token_file):
    select["restore_token"] = GLib.Variant("s", open(token_file).read().strip())
    log("using restore token")
code, results = portal.request(RD, "SelectDevices", "(oa{sv})", (session,), select)
if code != 0:
    fail(f"SelectDevices {code}")

t0 = time.monotonic()
code, results = portal.request(RD, "Start", "(osa{sv})", (session, ""), {}, timeout=120)
log("Start response", code, {k: v for k, v in results.items() if k != "restore_token"},
    f"after {time.monotonic() - t0:.1f}s")
if code != 0:
    fail("remote desktop not granted")
if token_file and "restore_token" in results:
    with open(token_file, "w") as fh:
        fh.write(results["restore_token"])
    log("restore token saved")


def key(keysym, pressed):
    portal.call(RD, "NotifyKeyboardKeysym", "(oa{sv}iu)", (session, {}, keysym, 1 if pressed else 0))


def keysym_for(ch):
    cp = ord(ch)
    if 0x20 <= cp <= 0x7E or 0xA0 <= cp <= 0xFF:
        return cp
    return 0x01000000 + cp


if mode == "paste":
    payload = text.encode("utf-8")

    def on_transfer(_c, _s, _p, _i, _sig, params):
        _session, mime, serial = params.unpack()
        log("SelectionTransfer", mime, serial)
        fd_list_reply = portal.bus.call_with_unix_fd_list_sync(
            BUS_NAME, OBJ, CB, "SelectionWrite", GLib.Variant("(ou)", (session, serial)),
            GLib.VariantType("(h)"), Gio.DBusCallFlags.NONE, -1, None, None)
        reply, fds = fd_list_reply
        fd = fds.get(reply.unpack()[0])
        os.write(fd, payload)
        os.close(fd)
        portal.call(CB, "SelectionWriteDone", "(oub)", (session, serial, True))
        log("selection written", len(payload), "bytes")

    portal.bus.signal_subscribe(BUS_NAME, CB, "SelectionTransfer", OBJ, None,
                                Gio.DBusSignalFlags.NONE, on_transfer)
    portal.call(CB, "SetSelection", "(oa{sv})", (session, {
        "mime_types": GLib.Variant("as", ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"])}))
    log("selection offered")
    ctx = GLib.MainContext.default()
    end = time.monotonic() + 0.3
    while time.monotonic() < end:
        ctx.iteration(False)
        time.sleep(0.01)
    use_keycodes = os.environ.get("TW_KEYCODES") == "1"
    for sym, code_, down in ((0xFFE3, 29, True), (0x76, 47, True), (0x76, 47, False), (0xFFE3, 29, False)):
        if use_keycodes:
            portal.call(RD, "NotifyKeyboardKeycode", "(oa{sv}iu)", (session, {}, code_, 1 if down else 0))
        else:
            key(sym, down)
        end = time.monotonic() + 0.05
        while time.monotonic() < end:
            ctx.iteration(False)
            time.sleep(0.005)
    log("pressed Ctrl+V", "(keycodes)" if use_keycodes else "(keysyms)")
    end = time.monotonic() + 3
    while time.monotonic() < end:
        ctx.iteration(False)
        time.sleep(0.01)
elif mode == "type":
    for ch in text:
        ks = keysym_for(ch)
        key(ks, True)
        key(ks, False)
        time.sleep(0.01)
    log("typed", len(text), "characters")
    time.sleep(1)

portal.bus.call_sync(BUS_NAME, session, "org.freedesktop.portal.Session", "Close",
                     None, None, Gio.DBusCallFlags.NONE, -1, None)
log("done")
