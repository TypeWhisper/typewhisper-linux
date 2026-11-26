//! Server profiles for the OpenAI-compatible engine, and the rules every
//! request to such a server follows. The app stores and edits the profiles; the
//! plugin `com.typewhisper.openai-compatible` sends the requests. Both use the
//! same rules, so the editor can say what a request will do.
//!
//! The profiles keep the field names of the macOS app and the behaviour of the
//! Windows app's `TypeWhisper.Plugin.OpenAiCompatible`.

pub mod endpoint;
pub mod profile;
pub mod server_url;

pub use profile::{
    BatchEndpoint, DEFAULT_PROFILE_ID, FetchedModel, Profile, ProfileError, Profiles,
    RealtimeProtocol, ReasoningEffort, TemperatureMode, TextApi, Transport, secret_name,
};

/// The plugin's ID, the same as on macOS and Windows.
pub const PLUGIN_ID: &str = "com.typewhisper.openai-compatible";
