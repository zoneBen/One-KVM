//! Android phone control over USB via adb + the scrcpy server.
//!
//! The module owns the adb/scrcpy session, exposes H.264 frames through the
//! normal WebRTC pipeline, and injects browser input through the scrcpy control
//! socket. The desktop `scrcpy` client is only used for validation, not at
//! runtime.

pub mod adb;
pub mod audio_source;
pub mod input;
pub mod manager;
pub mod protocol;
pub mod session;
pub mod types;
pub mod video_source;

pub use manager::PhoneManager;
pub use types::{PhoneAction, PhoneState, PhoneStatus, TargetMode, TargetStatus};
