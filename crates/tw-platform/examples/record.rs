//! Records from the default microphone and prints level and dominant frequency.
//! Used to check capture in test VMs that feed a known tone into a virtual source.
//!
//!     cargo run -p tw-platform --example record -- 3

fn main() {
    let seconds: f32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(3.0);
    let samples = tw_platform::audio::record(seconds).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    let crossings = samples
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count();
    println!(
        "{} samples, rms {rms:.4}, about {:.0} Hz",
        samples.len(),
        crossings as f32 / seconds
    );
}
