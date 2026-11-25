"""Play a WAV file into a virtual microphone in real time.

The microphone is a pipe source that reads raw s16le 16 kHz mono from a FIFO:

  pactl load-module module-pipe-source source_name=tw_virtual_mic \\
      file=/tmp/tw-mic.fifo format=s16le rate=16000 channels=1
  pactl set-default-source tw_virtual_mic

  feed_mic.py <file.wav> [lead-seconds] [tail-seconds] [fifo]

Silence goes before (default 0.5 s) and after (default 1.0 s) the file. Takes
mono 16-bit WAVs at any rate and resamples them linearly to 16 kHz."""
import array
import sys
import time
import wave

RATE = 16000
CHUNK = RATE // 50  # 20 ms

wav_path = sys.argv[1]
lead = float(sys.argv[2]) if len(sys.argv) > 2 else 0.5
tail = float(sys.argv[3]) if len(sys.argv) > 3 else 1.0
fifo = sys.argv[4] if len(sys.argv) > 4 else "/tmp/tw-mic.fifo"

with wave.open(wav_path) as w:
    if w.getnchannels() != 1 or w.getsampwidth() != 2:
        sys.exit("need a mono 16-bit WAV")
    rate = w.getframerate()
    speech = array.array("h", w.readframes(w.getnframes()))

if rate != RATE:
    step = rate / RATE
    resampled = array.array("h", bytes(2 * int(len(speech) / step)))
    for i in range(len(resampled)):
        pos = i * step
        j = int(pos)
        a, b = speech[j], speech[min(j + 1, len(speech) - 1)]
        resampled[i] = int(a + (b - a) * (pos - j))
    speech = resampled

samples = array.array("h", bytes(2 * int(lead * RATE)))
samples += speech
samples += array.array("h", bytes(2 * int(tail * RATE)))
data = samples.tobytes()

with open(fifo, "wb", buffering=0) as out:
    start = time.monotonic()
    for offset in range(0, len(data), CHUNK * 2):
        out.write(data[offset:offset + CHUNK * 2])
        due = start + (offset // 2 + CHUNK) / RATE
        time.sleep(max(0.0, due - time.monotonic()))
print(f"played {len(samples) / RATE:.2f} s from {wav_path}", flush=True)
