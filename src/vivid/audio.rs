//! Native audio services shared through the SDK.

// Rust guideline compliant 2026-10-07
pub use vivid_sdk::audio::{CaptureResampler, Decoder as AudioDecoder, supports};
pub use vivid_sdk::audio_output::Output as AudioOutput;
