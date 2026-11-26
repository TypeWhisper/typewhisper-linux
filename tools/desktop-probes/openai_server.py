#!/usr/bin/env python3
"""A small OpenAI-compatible server for testing the OpenAI-compatible plugin
without a cloud account. Standard library only.

  openai_server.py [--host 0.0.0.0] [--port 8765] [--key KEY]
                   [--parakeet <tw-plugin-parakeet> --model <parakeet model dir>]

Endpoints: GET /v1/models, POST /v1/audio/transcriptions (multipart WAV),
POST /v1/chat/completions, POST /v1/responses and the /v1/realtime WebSocket
(transcription sessions with 24 kHz PCM16). With --parakeet, audio is
transcribed for real by the Parakeet plugin; otherwise every transcript is
"mock transcript". With --key, a request without "Authorization: Bearer KEY"
gets 401. The model "strict-model" refuses max_tokens like newer OpenAI models.

Each request prints one JSON line: method, path, whether an Authorization or
api-key header came (never the key), and the fields that matter for the test.
"""
import argparse
import base64
import hashlib
import json
import os
import struct
import subprocess
import sys
import tempfile
import threading
import wave
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from io import BytesIO

WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
MODELS = ["whisper-1", "gpt-live-transcribe", "gpt-realtime-whisper", "mock-chat", "strict-model"]


class Parakeet:
    """Runs the Parakeet plugin over the TypeWhisper plugin protocol."""

    def __init__(self, executable, model_dir):
        self.lock = threading.Lock()
        self.next_id = 0
        self.proc = subprocess.Popen([executable], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        data_dir = tempfile.mkdtemp(prefix="openai-server-")
        self.call("initialize", {"protocolVersion": 1, "hostVersion": "0.1.0", "dataDir": data_dir})
        self.call("transcription.loadModel", {"path": model_dir, "threads": 4})

    def send(self, kind, payload):
        self.proc.stdin.write(struct.pack(">I", len(payload) + 1) + bytes([kind]) + payload)
        self.proc.stdin.flush()

    def call(self, method, params):
        self.next_id += 1
        message = {"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}
        self.send(1, json.dumps(message).encode())
        while True:
            (length,) = struct.unpack(">I", self.proc.stdout.read(4))
            body = self.proc.stdout.read(length)
            reply = json.loads(body[1:])
            if reply.get("id") == self.next_id:
                if "error" in reply:
                    raise RuntimeError(reply["error"])
                return reply["result"]

    def transcribe(self, samples):
        """16 kHz samples from -1 to 1."""
        with self.lock:
            self.call("transcription.begin", {"requestId": "server"})
            for start in range(0, len(samples), 1600):
                chunk = samples[start : start + 1600]
                self.send(2, struct.pack(f"<{len(chunk)}f", *chunk))
            return self.call("transcription.end", {"requestId": "server"})["text"]


ENGINE = None


def transcribe(samples):
    return ENGINE.transcribe(samples) if ENGINE else "mock transcript"


def log(**fields):
    print(json.dumps(fields), flush=True)


def multipart(body, content_type):
    boundary = content_type.split("boundary=", 1)[1].strip().strip('"').encode()
    fields = {}
    for part in body.split(b"--" + boundary):
        if b"\r\n\r\n" not in part:
            continue
        head, value = part.split(b"\r\n\r\n", 1)
        if value.endswith(b"\r\n"):
            value = value[:-2]
        for line in head.decode("utf-8", "replace").split("\r\n"):
            if line.lower().startswith("content-disposition:") and 'name="' in line:
                name = line.split('name="', 1)[1].split('"', 1)[0]
                fields[name] = value
    return fields


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    key = None

    def log_message(self, *args):
        pass

    def auth(self):
        header = self.headers.get("Authorization")
        return {"authorization": header is not None, "apiKeyHeader": "api-key" in self.headers}

    def authorized(self):
        if self.key is None:
            return True
        if self.headers.get("Authorization") == f"Bearer {self.key}":
            return True
        self.reply(401, {"error": {"message": "Incorrect API key provided", "type": "invalid_request_error"}})
        return False

    def reply(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def body(self):
        return self.rfile.read(int(self.headers.get("Content-Length", 0)))

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path.endswith("/v1/realtime") and self.headers.get("Upgrade", "").lower() == "websocket":
            log(method="GET", path=self.path, websocket=True, **self.auth())
            if self.authorized():
                self.realtime()
            return
        log(method="GET", path=self.path, **self.auth())
        if not self.authorized():
            return
        if path.endswith("/v1/models"):
            self.reply(200, {"object": "list", "data": [{"id": m, "object": "model", "owned_by": "mock"} for m in MODELS]})
        else:
            self.reply(404, {"error": {"message": f"no route {path}"}})

    def do_POST(self):
        path = self.path.split("?", 1)[0]
        body = self.body()
        if path.endswith("/audio/transcriptions") or path.endswith("/audio/translations"):
            fields = multipart(body, self.headers.get("Content-Type", ""))
            names = sorted(fields)
            text_fields = {k: v.decode() for k, v in fields.items() if k != "file"}
            log(method="POST", path=self.path, fields=names, form=text_fields, **self.auth())
            if not self.authorized():
                return
            with wave.open(BytesIO(fields["file"])) as w:
                frames = w.readframes(w.getnframes())
                rate = w.getframerate()
            pcm = struct.unpack(f"<{len(frames) // 2}h", frames)
            text = transcribe([s / 32768.0 for s in pcm])
            duration = len(pcm) / rate
            answer = {"text": text}
            if text_fields.get("response_format") == "verbose_json":
                # As GPUStack does, with the numbers as strings.
                answer.update(language="english", duration=f"{duration:.2f}",
                              segments=[{"text": text, "start": 0, "end": duration, "no_speech_prob": 0.01}])
            self.reply(200, answer)
            return
        request = json.loads(body or b"{}")
        if path.endswith("/v1/chat/completions"):
            controls = {k: request[k] for k in ("max_tokens", "max_completion_tokens", "thinking", "reasoning_effort", "temperature") if k in request}
            log(method="POST", path=self.path, model=request.get("model"), controls=controls, **self.auth())
            if not self.authorized():
                return
            if request.get("model") == "strict-model" and "max_tokens" in request:
                self.reply(400, {"error": {"message": "Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead."}})
                return
            user = request["messages"][-1]["content"]
            content = f"<think>reasoning</think>\nMock answer to: {user}"
            self.reply(200, {"choices": [{"message": {"role": "assistant", "content": content}, "finish_reason": "stop"}]})
        elif path.endswith("/v1/responses"):
            controls = {k: request[k] for k in ("max_output_tokens", "reasoning", "temperature", "store") if k in request}
            log(method="POST", path=self.path, model=request.get("model"), controls=controls, **self.auth())
            if not self.authorized():
                return
            text = request["input"][0]["content"][0]["text"]
            self.reply(200, {"status": "completed", "output": [{"type": "message", "content": [{"type": "output_text", "text": f"Mock response to: {text}"}]}]})
        else:
            self.reply(404, {"error": {"message": f"no route {path}"}})

    # The WebSocket side of /v1/realtime.

    def realtime(self):
        accept = base64.b64encode(hashlib.sha1((self.headers["Sec-WebSocket-Key"] + WS_GUID).encode()).digest()).decode()
        self.send_response(101)
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept)
        self.end_headers()
        self.wfile.flush()
        audio = bytearray()
        appends = 0
        while True:
            message = self.ws_read()
            if message is None:
                return
            event = json.loads(message)
            kind = event.get("type")
            if kind == "session.update":
                log(event="session.update", session=event["session"])
                self.ws_send({"type": "session.updated", "session": event["session"]})
            elif kind == "input_audio_buffer.append":
                audio += base64.b64decode(event["audio"])
                appends += 1
            elif kind == "input_audio_buffer.commit":
                log(event="commit", appends=appends, samples24k=len(audio) // 2)
                self.ws_send({"type": "input_audio_buffer.committed", "item_id": "item_1"})
                pcm = struct.unpack(f"<{len(audio) // 2}h", bytes(audio))
                # 24 to 16 kHz: every third output sample falls between two inputs.
                samples = [pcm[int(i * 1.5)] / 32768.0 for i in range(len(pcm) * 2 // 3)]
                text = transcribe(samples)
                for word in text.split():
                    self.ws_send({"type": "conversation.item.input_audio_transcription.delta", "item_id": "item_1", "delta": word + " "})
                self.ws_send({"type": "conversation.item.input_audio_transcription.completed", "item_id": "item_1", "transcript": text})

    def ws_read(self):
        """One text message, or None when the client closes."""
        payload = b""
        while True:
            head = self.rfile.read(2)
            if len(head) < 2:
                return None
            fin, opcode = head[0] & 0x80, head[0] & 0x0F
            length = head[1] & 0x7F
            if length == 126:
                (length,) = struct.unpack(">H", self.rfile.read(2))
            elif length == 127:
                (length,) = struct.unpack(">Q", self.rfile.read(8))
            mask = self.rfile.read(4) if head[1] & 0x80 else b"\0\0\0\0"
            data = bytes(b ^ mask[i % 4] for i, b in enumerate(self.rfile.read(length)))
            if opcode == 8:
                return None
            if opcode == 9:
                self.ws_frame(0xA, data)
                continue
            payload += data
            if fin:
                return payload.decode()

    def ws_frame(self, opcode, data):
        head = bytes([0x80 | opcode])
        if len(data) < 126:
            head += bytes([len(data)])
        elif len(data) < 65536:
            head += bytes([126]) + struct.pack(">H", len(data))
        else:
            head += bytes([127]) + struct.pack(">Q", len(data))
        self.wfile.write(head + data)
        self.wfile.flush()

    def ws_send(self, event):
        self.ws_frame(1, json.dumps(event).encode())


def main():
    global ENGINE
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--key")
    parser.add_argument("--parakeet")
    parser.add_argument("--model")
    args = parser.parse_args()
    if args.parakeet:
        ENGINE = Parakeet(args.parakeet, args.model)
    Handler.key = args.key or os.environ.get("OPENAI_SERVER_KEY")
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"listening on {args.host}:{args.port}", file=sys.stderr, flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
