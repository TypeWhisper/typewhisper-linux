//! Wire protocol between TypeWhisper and its plugins.
//!
//! A plugin is its own executable. The app starts it and talks to it over the
//! plugin's stdin and stdout, one frame per message:
//!
//! ```text
//! u32 big-endian length of (kind + payload) | u8 kind | payload
//! ```
//!
//! Kind 1 carries a JSON-RPC 2.0 message in UTF-8. Kind 2 carries PCM audio,
//! 16 kHz mono f32 little-endian, for the transcription request opened last.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Version of this protocol. Host and plugin compare it in `initialize`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest frame either side accepts. 16 MiB holds about 4 minutes of audio,
/// far more than one 100 ms chunk, and stops a broken peer from exhausting memory.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

const KIND_JSON: u8 = 1;
const KIND_PCM: u8 = 2;

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Json(Value),
    Pcm(Vec<f32>),
}

/// Reads one frame. Returns `Ok(None)` when the peer closed the stream between frames.
pub fn read_frame(input: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} out of range"),
        ));
    }
    let mut body = vec![0u8; len];
    input.read_exact(&mut body)?;
    let payload = &body[1..];
    match body[0] {
        KIND_JSON => serde_json::from_slice(payload)
            .map(|v| Some(Frame::Json(v)))
            .map_err(io::Error::from),
        KIND_PCM if payload.len().is_multiple_of(4) => {
            let (samples, _) = payload.as_chunks::<4>();
            Ok(Some(Frame::Pcm(
                samples.iter().map(|b| f32::from_le_bytes(*b)).collect(),
            )))
        }
        KIND_PCM => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PCM payload is not whole f32 samples",
        )),
        kind => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown frame kind {kind}"),
        )),
    }
}

/// Writes one frame and flushes it.
pub fn write_frame(output: &mut impl Write, frame: &Frame) -> io::Result<()> {
    let (kind, payload) = match frame {
        Frame::Json(value) => (KIND_JSON, serde_json::to_vec(value)?),
        Frame::Pcm(samples) => (
            KIND_PCM,
            samples.iter().flat_map(|s| s.to_le_bytes()).collect(),
        ),
    };
    if payload.len() + 1 > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    output.write_all(&((payload.len() + 1) as u32).to_be_bytes())?;
    output.write_all(&[kind])?;
    output.write_all(&payload)?;
    output.flush()
}

/// `manifest.json` at the root of every plugin package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// Same ID as on macOS and Windows, for example `com.typewhisper.parakeet`.
    pub id: String,
    pub name: String,
    pub version: String,
    pub protocol_version: u32,
    pub min_host_version: String,
    /// Path of the executable inside the package, for example `bin/tw-plugin-parakeet`.
    pub executable: String,
    pub categories: Vec<String>,
    /// `local` or `cloud`.
    pub locality: String,
    /// `x64` and `arm64`, as in the Windows plugin catalog.
    pub architectures: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_round_trip() {
        let frames = [
            Frame::Json(
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}),
            ),
            Frame::Pcm(vec![0.0, 0.5, -0.25, 1.0]),
        ];
        let mut buffer = Vec::new();
        for frame in &frames {
            write_frame(&mut buffer, frame).unwrap();
        }
        let mut reader = buffer.as_slice();
        for frame in &frames {
            assert_eq!(read_frame(&mut reader).unwrap().as_ref(), Some(frame));
        }
        assert_eq!(read_frame(&mut reader).unwrap(), None);
    }

    #[test]
    fn rejects_oversized_and_unknown_frames() {
        let mut huge = Vec::from(((MAX_FRAME_LEN + 1) as u32).to_be_bytes());
        huge.push(KIND_JSON);
        assert!(read_frame(&mut huge.as_slice()).is_err());
        let unknown = [0, 0, 0, 2, 9, 0];
        assert!(read_frame(&mut unknown.as_slice()).is_err());
        let torn_pcm = [0, 0, 0, 4, KIND_PCM, 1, 2, 3];
        assert!(read_frame(&mut torn_pcm.as_slice()).is_err());
    }

    #[test]
    fn parses_manifest() {
        let manifest: Manifest = serde_json::from_value(json!({
            "id": "com.typewhisper.parakeet", "name": "NVIDIA Parakeet", "version": "0.1.0",
            "protocolVersion": 1, "minHostVersion": "0.1.0", "executable": "bin/tw-plugin-parakeet",
            "categories": ["transcription"], "locality": "local", "architectures": ["x64", "arm64"]
        }))
        .unwrap();
        assert_eq!(manifest.protocol_version, PROTOCOL_VERSION);
    }
}
