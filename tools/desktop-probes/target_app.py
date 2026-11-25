"""Test target: a GTK4 window with a focused text view that appends every change
of its text to a log file, so probes can check what arrived."""
import sys

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gtk  # noqa: E402

LOG = sys.argv[1]


def on_activate(app):
    win = Gtk.ApplicationWindow(application=app, title="TW Target")
    win.set_default_size(600, 300)
    view = Gtk.TextView()
    buf = view.get_buffer()

    def changed(_buf):
        start, end = buf.get_bounds()
        with open(LOG, "a", encoding="utf-8") as fh:
            fh.write(repr(buf.get_text(start, end, False)) + "\n")

    buf.connect("changed", changed)
    win.set_child(view)
    win.present()
    view.grab_focus()


app = Gtk.Application(application_id="com.typewhisper.Spike.Target")
app.connect("activate", on_activate)
app.run([])
