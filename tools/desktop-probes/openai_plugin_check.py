#!/usr/bin/env python3
"""Drives the OpenAI-compatible plugin over the plugin protocol, as the app
does, and prints each answer as JSON.

  openai_plugin_check.py <plugin-executable> <server-url> test
  openai_plugin_check.py <plugin-executable> <server-url> models
  openai_plugin_check.py <plugin-executable> <server-url> transcribe <model> <wav> [--transport auto|batch|realtime] [--language de]
  openai_plugin_check.py <plugin-executable> <server-url> llm <model> [--api chat-completions|responses] [--effort low] [--thinking]

The API key comes from the environment variable TW_TEST_API_KEY, so it shows in
no process list. Any WAV works; it is converted to 16 kHz mono first.
"""
import argparse
import json
import os
import struct
import subprocess
import sys
import tempfile
import time
import wave


class Plugin:
    def __init__(self, executable):
        self.proc = subprocess.Popen([executable], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        self.next_id = 0

    def send(self, kind, payload):
        self.proc.stdin.write(struct.pack(">I", len(payload) + 1) + bytes([kind]) + payload)
        self.proc.stdin.flush()

    def call(self, method, params):
        self.next_id += 1
        self.send(1, json.dumps({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}).encode())
        while True:
            header = self.proc.stdout.read(4)
            if len(header) < 4:
                raise SystemExit(f"{method}: the plugin exited")
            (length,) = struct.unpack(">I", header)
            message = json.loads(self.proc.stdout.read(length)[1:])
            if message.get("id") == self.next_id:
                return message
            print(json.dumps({"notification": message.get("method"), "params": message.get("params")}), flush=True)


def samples_16k(path):
    with wave.open(path) as w:
        rate, channels, width = w.getframerate(), w.getnchannels(), w.getsampwidth()
        frames = w.readframes(w.getnframes())
    assert width == 2, "16-bit WAV only"
    pcm = struct.unpack(f"<{len(frames) // 2}h", frames)
    mono = [sum(pcm[i : i + channels]) / channels / 32768.0 for i in range(0, len(pcm), channels)]
    count = int(len(mono) * 16000 / rate)
    out = []
    for i in range(count):
        position = i * rate / 16000
        lower = min(int(position), len(mono) - 1)
        upper = min(lower + 1, len(mono) - 1)
        fraction = position - lower
        out.append(mono[lower] + (mono[upper] - mono[lower]) * fraction)
    return out


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("plugin")
    parser.add_argument("url")
    parser.add_argument("action", choices=["test", "models", "transcribe", "llm"])
    parser.add_argument("model", nargs="?", default="")
    parser.add_argument("wav", nargs="?")
    parser.add_argument("--transport", default="auto")
    parser.add_argument("--language")
    parser.add_argument("--api", default="chat-completions")
    parser.add_argument("--effort", default="")
    parser.add_argument("--thinking", action="store_true")
    parser.add_argument("--api-version", default="")
    args = parser.parse_args()

    profile = {
        "id": "openai-compatible", "name": "Check", "baseURL": args.url, "apiVersion": args.api_version,
        "selectedModelId": args.model if args.action == "transcribe" else "",
        "selectedLLMModelId": args.model if args.action == "llm" else "",
        "transcriptionTransportRaw": args.transport, "llmAPIModeRaw": args.api,
        "reasoningEffortRaw": args.effort, "thinkingEnabled": args.thinking,
    }
    secrets = {}
    if os.environ.get("TW_TEST_API_KEY"):
        secrets["api-key"] = os.environ["TW_TEST_API_KEY"]
    plugin = Plugin(args.plugin)
    data_dir = tempfile.mkdtemp(prefix="openai-check-")
    init = plugin.call("initialize", {"protocolVersion": 1, "hostVersion": "0.1.0", "dataDir": data_dir})
    print(json.dumps({"initialize": init.get("result")}))
    configured = plugin.call("settings.update", {"values": {"profiles": [profile]}, "secrets": secrets})
    print(json.dumps({"settings.update": configured.get("result", configured.get("error"))}))

    if args.action == "test":
        reply = plugin.call("connection.test", {})
    elif args.action == "models":
        reply = plugin.call("models.fetch", {})
    elif args.action == "llm":
        reply = plugin.call("llm.process", {"systemPrompt": "Answer in one short sentence.", "userText": "Say hello."})
    else:
        samples = samples_16k(args.wav)
        started = time.monotonic()
        begin = plugin.call("transcription.begin", {"requestId": "check", "language": args.language})
        if "error" in begin:
            print(json.dumps({"transcription.begin": begin["error"]}))
            return 1
        for start in range(0, len(samples), 1600):
            chunk = samples[start : start + 1600]
            plugin.send(2, struct.pack(f"<{len(chunk)}f", *chunk))
        reply = plugin.call("transcription.end", {"requestId": "check"})
        print(json.dumps({"seconds": round(time.monotonic() - started, 2)}))
    print(json.dumps({args.action: reply.get("result", reply.get("error"))}, ensure_ascii=False))
    plugin.call("shutdown", {})
    return 1 if "error" in reply else 0


if __name__ == "__main__":
    sys.exit(main())
