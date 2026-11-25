"""Inspect and drive windows in the headless session through AT-SPI.

  atspi_tool.py tree [app-substring]        print the accessible tree
  atspi_tool.py click <button-name> [app]   press the first button with that name
  atspi_tool.py text <app-substring>        print the text of every text widget
"""
import sys
import time

import pyatspi


def apps(substring=None):
    desktop = pyatspi.Registry.getDesktop(0)
    for app in desktop:
        if app is None:
            continue
        if substring is None or substring.lower() in (app.name or "").lower():
            yield app


def walk(node, depth=0):
    yield node, depth
    for child in node:
        if child is not None:
            yield from walk(child, depth + 1)


def tree(substring=None):
    for app in apps(substring):
        for node, depth in walk(app):
            print("  " * depth + f"{node.getRoleName()}: {node.name!r}")


def click(name, substring=None, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        for app in apps(substring):
            for node, _ in walk(app):
                if node.getRoleName() in ("push button", "button", "toggle button") and node.name == name:
                    action = node.queryAction()
                    for i in range(action.nActions):
                        if action.getName(i) in ("click", "press", "activate"):
                            action.doAction(i)
                            print(f"clicked {name!r} in {app.name!r}")
                            return 0
        time.sleep(0.5)
    print(f"button {name!r} not found", file=sys.stderr)
    return 1


def text(substring):
    for app in apps(substring):
        for node, _ in walk(app):
            try:
                t = node.queryText()
            except NotImplementedError:
                continue
            content = t.getText(0, t.characterCount)
            if content:
                print(f"{node.getRoleName()}: {content!r}")


if __name__ == "__main__":
    cmd = sys.argv[1]
    if cmd == "tree":
        tree(sys.argv[2] if len(sys.argv) > 2 else None)
    elif cmd == "click":
        sys.exit(click(sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else None))
    elif cmd == "text":
        text(sys.argv[2])
