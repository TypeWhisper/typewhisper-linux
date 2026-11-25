//! Microphone capture through the PulseAudio API, which PipeWire serves on every
//! current desktop and which Flatpak allows with `--socket=pulseaudio`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;

use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;

use crate::i18n::{tr, tr_with};

/// Sample rate of every buffer TypeWhisper hands to a transcription engine.
pub const SAMPLE_RATE: u32 = 16_000;
/// One read, about 33 ms, so that the level updates 30 times a second as on macOS.
const CHUNK_SAMPLES: usize = SAMPLE_RATE as usize / 30;
/// Chunks read after `Recorder::stop`, for audio that was still on its way:
/// about 200 ms.
const TAIL_CHUNKS: usize = 6;
/// The level meter's range, as in the macOS app: quieter is 0, louder is 1.
const SILENT_DB: f32 = -55.0;
const LOUD_DB: f32 = -18.0;

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
    .map_err(|e| {
        tr_with(
            "Could not open the microphone: {error}",
            &[("error", &format!("{e}"))],
        )
    })
}

fn read_chunk(simple: &Simple, bytes: &mut [u8], samples: &mut Vec<f32>) -> Result<(), String> {
    simple
        .read(bytes)
        .map_err(|e| tr_with("Recording failed: {error}", &[("error", &format!("{e}"))]))?;
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

/// The level of the last chunk of `samples`, from 0 to 1.
fn level_of(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    normalized_level(rms)
}

/// Maps an RMS value to 0..1 on a decibel scale, like the macOS app's meter.
fn normalized_level(rms: f32) -> f32 {
    if rms <= 0.0 {
        return 0.0;
    }
    let decibels = 20.0 * rms.log10();
    ((decibels - SILENT_DB) / (LOUD_DB - SILENT_DB)).clamp(0.0, 1.0)
}

/// The input level of a running recording, readable from any thread.
#[derive(Clone, Default)]
pub struct LevelMeter(Arc<AtomicU32>);

impl LevelMeter {
    /// The level of the latest chunk, from 0 (silence) to 1; 0 once the
    /// recording has ended.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    fn set(&self, level: f32) {
        self.0.store(level.to_bits(), Ordering::Relaxed);
    }
}

/// A running recording from the default source. PulseAudio resamples and mixes
/// down, so the samples are already 16 kHz mono.
pub struct Recorder {
    running: Arc<AtomicBool>,
    meter: LevelMeter,
    thread: Option<JoinHandle<Result<Vec<f32>, String>>>,
}

impl Recorder {
    /// Starts recording on a background thread. The thread opens the
    /// microphone, so the caller's main loop does not wait for the audio server;
    /// a failure to open it is reported by [`Recorder::stop`].
    pub fn start() -> Result<Self, String> {
        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);
        let meter = LevelMeter::default();
        let levels = meter.clone();
        let thread = std::thread::Builder::new()
            .name("tw-recorder".into())
            .spawn(move || {
                let recorded = (|| {
                    let simple = connect()?;
                    let mut samples = Vec::new();
                    let mut bytes = vec![0u8; CHUNK_SAMPLES * 4];
                    while flag.load(Ordering::Acquire) {
                        let start = samples.len();
                        read_chunk(&simple, &mut bytes, &mut samples)?;
                        levels.set(level_of(&samples[start..]));
                    }
                    for _ in 0..TAIL_CHUNKS {
                        read_chunk(&simple, &mut bytes, &mut samples)?;
                    }
                    Ok(samples)
                })();
                levels.set(0.0);
                recorded
            })
            .map_err(|e| {
                tr_with(
                    "Could not start the recorder: {error}",
                    &[("error", &format!("{e}"))],
                )
            })?;
        Ok(Self {
            running,
            meter,
            thread: Some(thread),
        })
    }

    /// The input level while the recording runs.
    pub fn level_meter(&self) -> LevelMeter {
        self.meter.clone()
    }

    /// Stops after the current chunk plus a short tail and returns everything
    /// recorded.
    pub fn stop(mut self) -> Result<Vec<f32>, String> {
        self.running.store(false, Ordering::Release);
        self.thread
            .take()
            .expect("recorder thread")
            .join()
            .map_err(|_| tr("The recorder stopped unexpectedly."))?
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_follow_the_macos_meter() {
        assert_eq!(normalized_level(0.0), 0.0);
        // -60 dB is below the range, -18 dB its top, -36.5 dB its middle.
        assert_eq!(normalized_level(0.001), 0.0);
        assert!((normalized_level(10f32.powf(-18.0 / 20.0)) - 1.0).abs() < 1e-4);
        assert!((normalized_level(10f32.powf(-36.5 / 20.0)) - 0.5).abs() < 1e-4);
        assert_eq!(normalized_level(1.0), 1.0);
    }

    #[test]
    fn level_is_the_rms_of_the_chunk() {
        assert_eq!(level_of(&[]), 0.0);
        let loud = vec![0.5f32; CHUNK_SAMPLES];
        assert_eq!(level_of(&loud), 1.0);
        let quiet = vec![0.0f32; CHUNK_SAMPLES];
        assert_eq!(level_of(&quiet), 0.0);
    }
}
