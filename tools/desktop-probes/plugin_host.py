"""Spike 6 host side: install a plugin ZIP into the user plugin folder, start the
plugin process and run one transcription over the framed protocol.

  plugin_host.py <plugin.zip> <model-dir> <wav-16k-mono>"""
import json
import os
import shutil
import struct
import subprocess
import sys
import time
import wave
import zipfile

zip_path, model_dir, wav_path = sys.argv[1:4]
root = os.path.expanduser("~/.local/share/typewhisper/plugins")

with zipfile.ZipFile(zip_path) as zf:
    manifest = json.loads(zf.read("manifest.json"))
    for name in zf.namelist():
        if name.startswith("/") or ".." in name.split("/"):
            sys.exit(f"unsafe path in package: {name}")
    target = os.path.join(root, manifest["id"])
    shutil.rmtree(target, ignore_errors=True)
    zf.extractall(target)
exe = os.path.join(target, manifest["executable"])
os.chmod(exe, 0o755)
print("installed", manifest["id"], manifest["version"], "->", target)

proc = subprocess.Popen([exe], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
next_id = 0


def send(kind, payload):
    proc.stdin.write(struct.pack(">I", len(payload) + 1) + bytes([kind]) + payload)
    proc.stdin.flush()


def call(method, params=None):
    global next_id
    next_id += 1
    send(1, json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}).encode())
    header = proc.stdout.read(4)
    (length,) = struct.unpack(">I", header)
    body = proc.stdout.read(length)
    message = json.loads(body[1:])
    if "error" in message:
        raise RuntimeError(f"{method}: {message['error']}")
    return message["result"]


print("initialize", call("initialize", {"protocolVersion": 1, "hostVersion": "0.1.0"}))
print("loadModel", call("transcription.loadModel", {"path": model_dir, "threads": 4}))

with wave.open(wav_path) as w:
    assert w.getframerate() == 16000 and w.getnchannels() == 1 and w.getsampwidth() == 2
    pcm16 = w.readframes(w.getnframes())
samples = struct.unpack(f"<{len(pcm16) // 2}h", pcm16)
floats = struct.pack(f"<{len(samples)}f", *(s / 32768.0 for s in samples))

started = time.monotonic()
call("transcription.begin", {"requestId": "r1", "language": "de"})
chunk = 1600 * 4  # 100 ms
for i in range(0, len(floats), chunk):
    send(2, floats[i:i + chunk])
result = call("transcription.end", {"requestId": "r1"})
print("result", json.dumps(result, ensure_ascii=False), f"round trip {1000 * (time.monotonic() - started):.0f} ms")
call("shutdown")
proc.wait(timeout=10)
print("plugin exited", proc.returncode)
