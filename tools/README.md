# Tools

`vm/` starts complete desktop sessions without a display inside a VM, so portals,
settings daemons and dialogs behave as on a real desktop:

- `start-gnome-session-headless.sh`: GNOME Shell on a virtual monitor through `gnome-session`.
- `start-plasma-headless.sh`: KWin on a virtual output through Plasma's systemd units.

`desktop-probes/` are Python (PyGObject) scripts that exercise the portals TypeWhisper
uses and drive the session for tests:

| Script | Purpose |
| --- | --- |
| `global_shortcuts.py` | Bind a shortcut, log `Activated` and `Deactivated` |
| `insert_text.py` | Paste or type text through the RemoteDesktop and Clipboard portals |
| `target_app.py` | GTK 4 text view that logs what arrives |
| `layer_overlay.py` | Indicator as a layer-shell surface (KDE, wlroots) |
| `fake_app_shell_bridge.py` | App side of the GNOME Shell extension protocol |
| `plugin_host.py` | Install a plugin ZIP and run one transcription |
| `atspi_tool.py`, `atspi_press.py` | Read the accessibility tree, press dialog buttons |
| `mutter_keys.py`, `portal_keys.py` | Simulate keys on GNOME and through the portal |
| `screenshot.py` | Screenshot through the Screenshot portal |

Results of the first run are in [docs/spike-2026-11.md](../docs/spike-2026-11.md).
