"""Spike 5 (KDE): the dictation indicator as a wlr-layer-shell surface.
Run with LD_PRELOAD=/usr/lib64/libgtk4-layer-shell.so.

  layer_overlay.py <seconds> <text>"""
import sys

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gtk4LayerShell", "1.0")
from gi.repository import GLib, Gtk  # noqa: E402
from gi.repository import Gtk4LayerShell as LayerShell  # noqa: E402

seconds, text = int(sys.argv[1]), sys.argv[2]
CSS = b"""
window { background: transparent; }
.pill { background: rgba(28,28,30,0.94); border-radius: 999px; padding: 10px 20px; }
.dot { background: #e01b24; border-radius: 6px; min-width: 12px; min-height: 12px; }
.label { color: white; font-weight: bold; }
"""


def on_activate(app):
    win = Gtk.Window(application=app)
    LayerShell.init_for_window(win)
    LayerShell.set_layer(win, LayerShell.Layer.OVERLAY)
    LayerShell.set_anchor(win, LayerShell.Edge.BOTTOM, True)
    LayerShell.set_margin(win, LayerShell.Edge.BOTTOM, 64)
    LayerShell.set_keyboard_mode(win, LayerShell.KeyboardMode.NONE)
    LayerShell.set_namespace(win, "typewhisper-indicator")
    provider = Gtk.CssProvider()
    provider.load_from_data(CSS)
    Gtk.StyleContext.add_provider_for_display(win.get_display(), provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
    box = Gtk.Box(spacing=12, css_classes=["pill"])
    dot = Gtk.Box(css_classes=["dot"], valign=Gtk.Align.CENTER)
    box.append(dot)
    box.append(Gtk.Label(label=text, css_classes=["label"]))
    win.set_child(box)
    win.present()
    print("layer surface supported:", LayerShell.is_supported(), flush=True)
    GLib.timeout_add_seconds(seconds, app.quit)


app = Gtk.Application(application_id="com.typewhisper.Spike.Overlay")
app.connect("activate", on_activate)
app.run([])
