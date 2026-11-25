"""Press named controls inside one window of one app via AT-SPI.

  atspi_press.py <app-name> <window-title-substring> <control-name>...
Check boxes are switched on, buttons are pressed, in the given order."""
import sys
import time

import pyatspi

app_name, window_part, controls = sys.argv[1], sys.argv[2], sys.argv[3:]


def walk(node):
    yield node
    for child in node:
        if child is not None:
            yield from walk(child)


def find_window():
    for app in pyatspi.Registry.getDesktop(0):
        if app is not None and app.name == app_name:
            for win in app:
                if win is not None and window_part.lower() in (win.name or "").lower():
                    return win
    return None


win = None
for _ in range(40):
    win = find_window()
    if win:
        break
    time.sleep(0.5)
if not win:
    sys.exit(f"window {window_part!r} of {app_name!r} not found")

for name in controls:
    target = next((n for n in walk(win) if n.name == name and n.getRoleName() in
                   ("button", "push button", "check box", "toggle button")), None)
    if target is None:
        sys.exit(f"control {name!r} not found")
    if target.getRoleName() == "check box" and target.getState().contains(pyatspi.STATE_CHECKED):
        print(f"{name!r} already on")
        continue
    action = target.queryAction()
    names = [action.getName(i) for i in range(action.nActions)]
    index = next((i for i, n in enumerate(names) if n in ("press", "click", "toggle", "activate", "Press", "Toggle")), 0)
    action.doAction(index)
    print(f"{name!r}: {names[index]}")
    time.sleep(0.5)
