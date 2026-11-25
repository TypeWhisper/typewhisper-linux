//! Microphone capture through the PulseAudio API, which PipeWire serves on every
//! current desktop and which Flatpak allows with `--socket=pulseaudio`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;

/// Sample rate of every buffer TypeWhisper hands to a transcription engine.
pub const SAMPLE_RATE: u32 = 16_000;
const CHUNK_SAMPLES: usize = SAMPLE_RATE as usize / 10; // 100 ms
/// Chunks read after `Recorder::stop`, for audio that was still on its way.
const TAIL_CHUNKS: usize = 2;

fn connect() -> Result<Simple, String> {
    let spec = Spec {
        format: Format::F32le,
        channels: 1,
        rate: SAMPLE_RATE,
    };
    // Without a fragment size, PipeWire delivers recorded audio in 2 s blocks, and
    // whatever the server still holds when the recording stops is lost.
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: (CHUNK_SAMPLES * 4) as u32,
    };
    Simple::new(
        None,
        "TypeWhisper",
        Direction::Record,
        None,
        "Dictation",
        &spec,
        None,
        Some(&attr),
    )
    .map_err(|e| format!("Could not open the microphone: {e}"))
}

fn read_chunk(simple: &Simple, bytes: &mut [u8], samples: &mut Vec<f32>) -> Result<(), String> {
    simple
        .read(bytes)
        .map_err(|e| format!("Recording failed: {e}"))?;
    let (chunks, _) = bytes.as_chunks::<4>();
    samples.extend(chunks.iter().map(|b| f32::from_le_bytes(*b)));
    Ok(())
}

/// Records `seconds` from the default source as 16 kHz mono f32.
pub fn record(seconds: f32) -> Result<Vec<f32>, String> {
    let simple = connect()?;
    let total = (seconds * SAMPLE_RATE as f32) as usize;
    let mut samples = Vec::with_capacity(total + CHUNK_SAMPLES);
    let mut bytes = vec![0u8; CHUNK_SAMPLES * 4];
    while samples.len() < total {
        read_chunk(&simple, &mut bytes, &mut samples)?;
    }
    samples.truncate(total);
    Ok(samples)
}

/// A running recording from the default source. PulseAudio resamples and mixes
/// down, so the samples are already 16 kHz mono.
pub struct Recorder {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<Vec<f32>, String>>>,
}

impl Recorder {
    /// Starts recording on a background thread. The thread opens the
    /// microphone, so the caller's main loop does not wait for the audio server;
    /// a failure to open it is reported by [`Recorder::stop`].
    pub fn start() -> Result<Self, String> {
        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);
        let thread = std::thread::Builder::new()
            .name("tw-recorder".into())
            .spawn(move || {
                let simple = connect()?;
                let mut samples = Vec::new();
                let mut bytes = vec![0u8; CHUNK_SAMPLES * 4];
                while flag.load(Ordering::Acquire) {
                    read_chunk(&simple, &mut bytes, &mut samples)?;
                }
                for _ in 0..TAIL_CHUNKS {
                    read_chunk(&simple, &mut bytes, &mut samples)?;
                }
                Ok(samples)
            })
            .map_err(|e| format!("Could not start the recorder: {e}"))?;
        Ok(Self {
            running,
            thread: Some(thread),
        })
    }

    /// Stops after the current 100 ms chunk plus a short tail and returns
    /// everything recorded.
    pub fn stop(mut self) -> Result<Vec<f32>, String> {
        self.running.store(false, Ordering::Release);
        self.thread
            .take()
            .expect("recorder thread")
            .join()
            .map_err(|_| "The recorder thread panicked".to_string())?
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
