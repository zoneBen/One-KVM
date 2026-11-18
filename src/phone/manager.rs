//! Phone session lifecycle and target switching.
//!
//! Only one scrcpy session exists at a time. When the last WebRTC viewer goes
//! away the session is kept for [`PhoneConfig::grace`] (5 minutes by default),
//! then suspended; the next viewer restarts it on demand.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

use crate::error::{AppError, Result};
use crate::hid::HidController;
use crate::webrtc::WebRtcStreamer;

use super::adb::{adb_version, default_adb_path, list_devices, AdbClient};
use super::input::DisabledPhoneInput;
use super::session::{PhoneSession, PhoneSessionConfig, PhoneSessionState, PhoneSessionStatus};
use super::types::{PhoneAction, PhoneState, PhoneStatus};

const DEFAULT_SERVER_VERSION: &str = "3.3";
const DEFAULT_GRACE_SECS: u64 = 300;

#[derive(Debug, Clone)]
pub struct PhoneConfig {
    pub adb_path: String,
    pub server_jar: PathBuf,
    pub server_version: String,
    pub max_size: u16,
    pub max_fps: u32,
    pub video_bit_rate: u32,
    pub audio_bit_rate: u32,
    pub audio: bool,
    pub grace: Duration,
}

impl Default for PhoneConfig {
    fn default() -> Self {
        Self {
            adb_path: default_adb_path(),
            server_jar: resolve_server_jar(),
            server_version: std::env::var("ONE_KVM_SCRCPY_SERVER_VERSION")
                .unwrap_or_else(|_| DEFAULT_SERVER_VERSION.to_string()),
            max_size: 1600,
            max_fps: 60,
            video_bit_rate: 8_000_000,
            audio_bit_rate: 128_000,
            audio: true,
            grace: Duration::from_secs(DEFAULT_GRACE_SECS),
        }
    }
}

fn resolve_server_jar() -> PathBuf {
    let candidates = [
        std::env::var("ONE_KVM_SCRCPY_SERVER").ok(),
        Some("/usr/local/share/scrcpy/scrcpy-server".to_string()),
        Some("/usr/share/scrcpy/scrcpy-server".to_string()),
        Some("/usr/share/one-kvm/scrcpy-server".to_string()),
        Some("/opt/one-kvm/scrcpy-server".to_string()),
    ];
    candidates
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("/usr/local/share/scrcpy/scrcpy-server"))
}

pub struct PhoneManager {
    webrtc: Arc<WebRtcStreamer>,
    hid: Arc<HidController>,
    config: PhoneConfig,
    session: Mutex<Option<Arc<PhoneSession>>>,
    status: RwLock<PhoneStatus>,
    generation: AtomicU64,
    activate_lock: Mutex<()>,
    self_weak: parking_lot::RwLock<Option<Weak<Self>>>,
}

impl PhoneManager {
    pub fn new(
        webrtc: Arc<WebRtcStreamer>,
        hid: Arc<HidController>,
        config: PhoneConfig,
    ) -> Arc<Self> {
        let manager = Arc::new(Self {
            webrtc,
            hid,
            config,
            session: Mutex::new(None),
            status: RwLock::new(PhoneStatus::default()),
            generation: AtomicU64::new(0),
            activate_lock: Mutex::new(()),
            self_weak: parking_lot::RwLock::new(None),
        });
        *manager.self_weak.write() = Some(Arc::downgrade(&manager));
        manager
    }

    pub fn config(&self) -> &PhoneConfig {
        &self.config
    }

    /// Re-detect the environment/device without starting a session.
    pub async fn probe(&self) -> PhoneStatus {
        let session = { self.session.lock().await.clone() };
        if let Some(session) = session {
            let status = merge_session(&session, self.config.max_fps).await;
            *self.status.write().await = status.clone();
            return status;
        }

        let available = if !self.config.server_jar.is_file() {
            None
        } else if adb_version(&self.config.adb_path).await.is_err() {
            None
        } else {
            Some(())
        };
        if available.is_none() {
            let status = PhoneStatus::unavailable(format!(
                "缺少 adb 或 scrcpy-server（jar: {}）",
                self.config.server_jar.display()
            ));
            *self.status.write().await = status.clone();
            return status;
        }

        let devices = match list_devices(&self.config.adb_path).await {
            Ok(devices) => devices,
            Err(error) => {
                let status = PhoneStatus {
                    available: true,
                    state: PhoneState::Disconnected,
                    error: Some(error.to_string()),
                    ..Default::default()
                };
                *self.status.write().await = status.clone();
                return status;
            }
        };

        let status = if let Some(device) = devices
            .iter()
            .find(|device| device.is_usb() && device.is_authorized())
        {
            PhoneStatus {
                available: true,
                state: PhoneState::Ready,
                serial: Some(device.serial.clone()),
                model: device.model.clone(),
                fps: self.config.max_fps,
                audio: self.config.audio,
                ..Default::default()
            }
        } else if let Some(device) = devices
            .iter()
            .find(|device| device.is_usb() && device.is_unauthorized())
        {
            PhoneStatus {
                available: true,
                state: PhoneState::Unauthorized,
                serial: Some(device.serial.clone()),
                model: device.model.clone(),
                fps: self.config.max_fps,
                audio: self.config.audio,
                error: Some("请在手机上允许 USB 调试授权".to_string()),
                ..Default::default()
            }
        } else {
            PhoneStatus {
                available: true,
                state: PhoneState::Disconnected,
                fps: self.config.max_fps,
                audio: self.config.audio,
                error: Some("未检测到通过 USB 连接的 Android 手机".to_string()),
                ..Default::default()
            }
        };
        *self.status.write().await = status.clone();
        status
    }

    pub async fn cached_status(&self) -> PhoneStatus {
        let session = { self.session.lock().await.clone() };
        if let Some(session) = session {
            return merge_session(&session, self.config.max_fps).await;
        }
        self.status.read().await.clone()
    }

    /// Start the scrcpy session and install it into WebRTC/HID.
    pub async fn activate(&self) -> Result<()> {
        let _guard = self.activate_lock.lock().await;

        let existing = { self.session.lock().await.clone() };
        if let Some(session) = existing {
            if session.state() == PhoneSessionState::Running {
                self.install(&session).await;
                return Ok(());
            }
        }

        // Drop a stale session before starting a fresh one.
        let stale = { self.session.lock().await.take() };
        if let Some(old) = stale {
            old.stop().await;
        }
        // Keep HID routed away from the PC backend while the phone session is
        // being (re)started. It is replaced by the real phone backend on
        // success and cleared by set_target if the switch is aborted.
        self.clear_media_sources().await;
        self.hid
            .set_override_backend(Some(DisabledPhoneInput::new()))
            .await;

        let detected = self.probe().await;
        match detected.state {
            PhoneState::Unauthorized => {
                let message = "手机尚未授权 USB 调试，请在手机上点“允许”".to_string();
                self.store_error(message.clone()).await;
                return Err(AppError::ServiceUnavailable(message));
            }
            PhoneState::Ready => {}
            _ => {
                let message = detected
                    .error
                    .clone()
                    .unwrap_or_else(|| "未检测到可用的 USB 手机".to_string());
                self.store_error(message.clone()).await;
                return Err(AppError::ServiceUnavailable(message));
            }
        }

        let serial = detected
            .serial
            .clone()
            .ok_or_else(|| AppError::ServiceUnavailable("手机序列号不可用".to_string()))?;
        let adb = AdbClient::new(self.config.adb_path.clone(), serial.clone());
        let model = adb
            .shell("getprop ro.product.model")
            .await
            .unwrap_or_default()
            .trim()
            .to_string();
        let android = adb
            .shell("getprop ro.build.version.release")
            .await
            .unwrap_or_default()
            .trim()
            .to_string();

        let config = PhoneSessionConfig {
            adb_path: self.config.adb_path.clone(),
            serial,
            server_jar: self.config.server_jar.clone(),
            server_version: self.config.server_version.clone(),
            max_size: self.config.max_size,
            max_fps: self.config.max_fps,
            video_bit_rate: self.config.video_bit_rate,
            audio_bit_rate: self.config.audio_bit_rate,
            audio: self.config.audio,
            model,
            android,
        };

        let session = match PhoneSession::start(config).await {
            Ok(session) => session,
            Err(error) => {
                self.store_error(error.to_string()).await;
                return Err(error);
            }
        };

        self.install(&session).await;
        *self.session.lock().await = Some(session.clone());
        let status = merge_session(&session, self.config.max_fps).await;
        *self.status.write().await = status;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.spawn_grace_monitor(generation);
        info!("Phone target activated");
        Ok(())
    }

    pub async fn ensure_active(&self) -> Result<()> {
        let existing = { self.session.lock().await.clone() };
        if let Some(session) = existing {
            if session.state() == PhoneSessionState::Running {
                self.install(&session).await;
                return Ok(());
            }
        }
        self.activate().await
    }

    /// Stop the session immediately (explicit target switch).
    pub async fn deactivate(&self) -> Result<()> {
        let _guard = self.activate_lock.lock().await;
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.stop_session().await;
        let status = self.probe().await;
        *self.status.write().await = status;
        info!("Phone target deactivated");
        Ok(())
    }

    /// Stop the session but keep the target/status as suspended.
    ///
    /// Returns `false` when a viewer connected between the grace monitor's
    /// check and this call, in which case the session is left untouched.
    pub async fn suspend(&self) -> Result<bool> {
        let _guard = self.activate_lock.lock().await;
        // A viewer may have connected after the grace monitor's last check.
        if self.webrtc.session_count().await > 0 {
            return Ok(false);
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        let last = { self.session.lock().await.clone() };
        let mut status = if let Some(session) = last.as_ref() {
            merge_session(session, self.config.max_fps).await
        } else {
            self.status.read().await.clone()
        };
        self.stop_session().await;
        // Keep the phone target owning input even though the session is gone.
        self.hid
            .set_override_backend(Some(DisabledPhoneInput::new()))
            .await;
        status.state = PhoneState::Suspended;
        status.error = None;
        *self.status.write().await = status;
        info!("Phone session suspended after viewer grace period");
        Ok(true)
    }

    /// Clear the HID override (used when a phone switch is aborted and the PC
    /// target remains active).
    pub async fn release_input(&self) {
        self.hid.set_override_backend(None).await;
    }

    async fn stop_session(&self) {
        let session = { self.session.lock().await.take() };
        self.clear_sources().await;
        if let Some(session) = session {
            session.stop().await;
        }
    }

    async fn install(&self, session: &Arc<PhoneSession>) {
        self.webrtc
            .set_phone_video_source(Some(session.video_source()))
            .await;
        let audio_source = if session.audio_enabled() {
            Some(session.audio_source())
        } else {
            None
        };
        self.webrtc.set_phone_audio_source(audio_source).await;
        self.hid
            .set_override_backend(Some(session.control_backend()))
            .await;
    }

    async fn clear_media_sources(&self) {
        self.webrtc.set_phone_video_source(None).await;
        self.webrtc.set_phone_audio_source(None).await;
    }

    async fn clear_sources(&self) {
        self.clear_media_sources().await;
        self.hid.set_override_backend(None).await;
    }

    async fn store_error(&self, error: String) {
        let mut status = self.status.write().await;
        status.available = self.config.server_jar.is_file();
        status.state = PhoneState::Error;
        status.error = Some(error);
        status.fps = self.config.max_fps;
        status.audio = self.config.audio;
    }

    pub async fn send_action(&self, action: PhoneAction) -> Result<()> {
        let session = { self.session.lock().await.clone() }
            .ok_or_else(|| AppError::ServiceUnavailable("手机会话未运行".to_string()))?;
        if session.state() != PhoneSessionState::Running {
            return Err(AppError::ServiceUnavailable("手机会话未就绪".to_string()));
        }
        session.send_action(action).await
    }

    pub async fn set_video_size(&self, width: u32, height: u32) -> Result<()> {
        let session = { self.session.lock().await.clone() }
            .ok_or_else(|| AppError::ServiceUnavailable("手机会话未运行".to_string()))?;
        session.set_video_size(width, height).await
    }

    fn spawn_grace_monitor(&self, generation: u64) {
        let weak = match self.self_weak.read().clone() {
            Some(weak) => weak,
            None => return,
        };
        tokio::spawn(async move {
            let mut idle_since: Option<Instant> = None;
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                let Some(manager) = weak.upgrade() else {
                    break;
                };
                if manager.generation.load(Ordering::SeqCst) != generation {
                    break;
                }
                let active = {
                    let guard = manager.session.lock().await;
                    guard
                        .as_ref()
                        .map(|session| session.state() == PhoneSessionState::Running)
                        .unwrap_or(false)
                };
                if !active {
                    break;
                }
                manager.webrtc.cleanup().await;
                if manager.webrtc.session_count().await > 0 {
                    idle_since = None;
                    continue;
                }
                let since = *idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= manager.config.grace {
                    match manager.suspend().await {
                        Ok(true) => break,
                        Ok(false) => {
                            // A viewer connected in the meantime; keep watching.
                            idle_since = None;
                        }
                        Err(error) => {
                            warn!("Failed to suspend phone session: {}", error);
                            break;
                        }
                    }
                }
            }
        });
    }
}

async fn merge_session(session: &Arc<PhoneSession>, fps: u32) -> PhoneStatus {
    let status: PhoneSessionStatus = session.status().await;
    let state = match status.state {
        PhoneSessionState::Starting => PhoneState::Starting,
        PhoneSessionState::Running => PhoneState::Running,
        PhoneSessionState::Stopped => PhoneState::Suspended,
        PhoneSessionState::Error => PhoneState::Error,
    };
    PhoneStatus {
        available: true,
        state,
        serial: (!status.serial.is_empty()).then_some(status.serial),
        model: (!status.model.is_empty()).then_some(status.model),
        android: (!status.android.is_empty()).then_some(status.android),
        resolution: Some((status.width, status.height)),
        fps,
        audio: status.audio,
        error: status.error,
    }
}
