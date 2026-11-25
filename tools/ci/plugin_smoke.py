#!/usr/bin/env python3
"""Start an installed plugin and check that it answers `initialize` and shuts down.

    plugin_smoke.py <plugin-folder>

The folder holds the plugin's manifest.json. CI runs this inside the Flatpak sandbox: it
passes only if the add-on is mounted where the app looks for it and the executable finds
its libraries there.
"""
import json
import os
import struct
import subprocess
import sys
import threading

KIND_JSON = 1
# A plugin that starts but never answers must not hold the job until its timeout.
DEADLINE_SECONDS = 60


def call(process, request_id, method, params):
    payload = json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}).encode()
    process.stdin.write(struct.pack(">I", len(payload) + 1) + bytes([KIND_JSON]) + payload)
    process.stdin.flush()
    header = process.stdout.read(4)
    if len(header) < 4:
        sys.exit(f"{method}: no reply; the plugin closed its output or did not answer within "
                 f"{DEADLINE_SECONDS} seconds")
    body = process.stdout.read(struct.unpack(">I", header)[0])
    message = json.loads(body[1:])
    if message.get("id") != request_id or "error" in message:
        sys.exit(f"{method}: unexpected reply {message}")
    return message["result"]


def main():
    folder = sys.argv[1]
    with open(os.path.join(folder, "manifest.json"), encoding="utf-8") as file:
        manifest = json.load(file)
    process = subprocess.Popen([os.path.join(folder, manifest["executable"])],
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    # Killing the plugin ends a read that waits for its reply.
    watchdog = threading.Timer(DEADLINE_SECONDS, process.kill)
    watchdog.start()
    try:
        result = call(process, 1, "initialize",
                      {"protocolVersion": manifest["protocolVersion"], "hostVersion": manifest["minHostVersion"]})
        if result.get("pluginId") != manifest["id"]:
            sys.exit(f"initialize: expected plugin {manifest['id']}, got {result}")
        call(process, 2, "shutdown", {})
        code = process.wait()
    finally:
        watchdog.cancel()
        process.kill()
        process.wait()
    if code != 0:
        sys.exit(f"the plugin exited with code {code}")
    print(f"{manifest['id']} {manifest['version']} answers initialize and shuts down")


if __name__ == "__main__":
    main()
