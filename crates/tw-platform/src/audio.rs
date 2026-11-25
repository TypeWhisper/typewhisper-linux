//! Microphone capture through the PulseAudio API, which PipeWire serves on every
//! current desktop and which Flatpak allows with `--socket=pulseaudio`.

use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;

/// Sample rate of every buffer TypeWhisper hands to a transcription engine.
pub const SAMPLE_RATE: u32 = 16_000;

/// Records `seconds` from the default source as 16 kHz mono f32.
/// PulseAudio resamples and mixes down, so no sample conversion happens here.
pub fn record(seconds: f32) -> Result<Vec<f32>, String> {
    let spec = Spec {
        format: Format::F32le,
        channels: 1,
        rate: SAMPLE_RATE,
    };
    let simple = Simple::new(
        None,
        "TypeWhisper",
        Direction::Record,
        None,
        "Dictation",
        &spec,
        None,
        None,
    )
    .map_err(|e| format!("connect: {e}"))?;
    let total = (seconds * SAMPLE_RATE as f32) as usize;
    let mut samples = Vec::with_capacity(total);
    let mut bytes = vec![0u8; (SAMPLE_RATE as usize / 10) * 4]; // 100 ms
    while samples.len() < total {
        simple.read(&mut bytes).map_err(|e| format!("read: {e}"))?;
        let remaining = total - samples.len();
        let (chunks, _) = bytes.as_chunks::<4>();
        samples.extend(
            chunks
                .iter()
                .take(remaining)
                .map(|b| f32::from_le_bytes(*b)),
        );
    }
    Ok(samples)
}
