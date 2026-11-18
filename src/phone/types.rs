//! Public phone/target types shared by the manager, API and frontend.

use serde::{Deserialize, Serialize};

/// Which target the KVM currently controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetMode {
    Pc,
    Phone,
}

impl Default for TargetMode {
    fn default() -> Self {
        Self::Pc
    }
}

/// Lifecycle of the optional phone target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhoneState {
    /// adb or scrcpy-server is missing.
    Unavailable,
    /// Environment is ready but no phone is connected.
    Disconnected,
    /// A phone is connected and waiting for USB debugging authorization.
    Unauthorized,
    /// A phone is connected but the session has not started.
    Ready,
    /// adb/server/sockets are being established.
    Starting,
    /// Video/audio/control are running.
    Running,
    /// Stopped after the no-viewer grace period; restarts on demand.
    Suspended,
    /// Last start failed.
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhoneStatus {
    pub available: bool,
    pub state: PhoneState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub android: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<(u32, u32)>,
    pub fps: u32,
    pub audio: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Default for PhoneStatus {
    fn default() -> Self {
        Self {
            available: false,
            state: PhoneState::Unavailable,
            serial: None,
            model: None,
            android: None,
            resolution: None,
            fps: 0,
            audio: false,
            error: None,
        }
    }
}

impl PhoneStatus {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            state: PhoneState::Unavailable,
            error: Some(reason.into()),
            ..Default::default()
        }
    }

    pub fn is_running(&self) -> bool {
        self.state == PhoneState::Running
    }
}

/// Actions exposed by the console's phone toolbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhoneAction {
    Back,
    Home,
    Recents,
    Power,
    VolumeUp,
    VolumeDown,
    Wake,
    Rotate,
    ScreenOn,
    ScreenOff,
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetStatus {
    pub target: TargetMode,
    pub phone: PhoneStatus,
}
