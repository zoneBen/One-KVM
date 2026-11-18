//! `HidBackend` implementation that injects input through the scrcpy control socket.
//!
//! The browser keeps sending the existing One-KVM HID events; this backend maps
//! them to scrcpy `INJECT_TOUCH_EVENT` / `INJECT_KEYCODE` messages. Only the
//! left mouse button and single-finger touch gestures are supported in v1.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

use crate::error::{AppError, Result};
use crate::hid::{
    ConsumerEvent, HidBackend, HidBackendRuntimeSnapshot, KeyboardEvent, MouseButton, MouseEvent,
    MouseEventType,
};

use super::protocol::{
    android_keycode, android_meta_state, keys, ControlCommand, POINTER_ID_FINGER,
};

const NORMALIZED_MAX: u32 = 32767;

pub struct ScrcpyBackend {
    control: mpsc::Sender<ControlCommand>,
    last_x: AtomicU32,
    last_y: AtomicU32,
    touch_down: AtomicBool,
    screen_width: AtomicU32,
    screen_height: AtomicU32,
    runtime: watch::Sender<()>,
}

/// Sink installed while the phone target holds input but no scrcpy session is
/// running (for example after the no-viewer grace period). It rejects input
/// instead of letting it fall through to the PC backend.
pub struct DisabledPhoneInput {
    runtime: watch::Sender<()>,
}

impl DisabledPhoneInput {
    pub fn new() -> Arc<Self> {
        let (runtime, _) = watch::channel(());
        Arc::new(Self { runtime })
    }

    fn unavailable<T>() -> Result<T> {
        Err(AppError::ServiceUnavailable("手机会话未运行".to_string()))
    }
}

impl ScrcpyBackend {
    pub fn new(
        control: mpsc::Sender<ControlCommand>,
        screen_width: u32,
        screen_height: u32,
    ) -> Arc<Self> {
        let (runtime, _) = watch::channel(());
        Arc::new(Self {
            control,
            last_x: AtomicU32::new(NORMALIZED_MAX / 2),
            last_y: AtomicU32::new(NORMALIZED_MAX / 2),
            touch_down: AtomicBool::new(false),
            screen_width: AtomicU32::new(screen_width.max(1)),
            screen_height: AtomicU32::new(screen_height.max(1)),
            runtime,
        })
    }

    fn screen(&self) -> (u32, u32) {
        (
            self.screen_width.load(Ordering::Relaxed).clamp(1, 65535),
            self.screen_height.load(Ordering::Relaxed).clamp(1, 65535),
        )
    }

    fn position(&self, width: u32, height: u32) -> (u32, u32) {
        let x = self.last_x.load(Ordering::Relaxed).min(NORMALIZED_MAX);
        let y = self.last_y.load(Ordering::Relaxed).min(NORMALIZED_MAX);
        (
            (u64::from(x) * u64::from(width) / u64::from(NORMALIZED_MAX)) as u32,
            (u64::from(y) * u64::from(height) / u64::from(NORMALIZED_MAX)) as u32,
        )
    }

    fn set_position(&self, x: i32, y: i32) {
        self.last_x
            .store(x.clamp(0, NORMALIZED_MAX as i32) as u32, Ordering::Relaxed);
        self.last_y
            .store(y.clamp(0, NORMALIZED_MAX as i32) as u32, Ordering::Relaxed);
    }

    async fn send(&self, command: ControlCommand) -> Result<()> {
        self.control
            .send(command)
            .await
            .map_err(|_| AppError::ServiceUnavailable("手机控制通道已断开".to_string()))
    }

    async fn send_touch(&self, action: u8, pressure: f32) -> Result<()> {
        let (width, height) = self.screen();
        let (x, y) = self.position(width, height);
        self.send(ControlCommand::Touch {
            action,
            pointer_id: POINTER_ID_FINGER,
            x,
            y,
            screen_width: width as u16,
            screen_height: height as u16,
            pressure,
            action_button: 0,
            buttons: 0,
        })
        .await
    }

    async fn send_keycode(&self, keycode: u32, meta_state: u32, down: bool) -> Result<()> {
        self.send(ControlCommand::Keycode {
            action: u8::from(!down),
            keycode,
            repeat: 0,
            meta_state,
        })
        .await
    }
}

#[async_trait]
impl HidBackend for ScrcpyBackend {
    async fn init(&self) -> Result<()> {
        Ok(())
    }

    async fn send_keyboard(&self, event: KeyboardEvent) -> Result<()> {
        let Some(keycode) = android_keycode(event.key) else {
            return Ok(());
        };
        let meta = android_meta_state(&event.modifiers);
        let down = matches!(event.event_type, crate::hid::KeyEventType::Down);
        self.send_keycode(keycode, meta, down).await
    }

    async fn send_mouse(&self, event: MouseEvent) -> Result<()> {
        match event.event_type {
            MouseEventType::MoveAbs => {
                self.set_position(event.x, event.y);
                if self.touch_down.load(Ordering::Relaxed) {
                    self.send_touch(2, 1.0).await?;
                }
                Ok(())
            }
            MouseEventType::Move => {
                let x = self.last_x.load(Ordering::Relaxed) as i32 + event.x;
                let y = self.last_y.load(Ordering::Relaxed) as i32 + event.y;
                self.set_position(x, y);
                if self.touch_down.load(Ordering::Relaxed) {
                    self.send_touch(2, 1.0).await?;
                }
                Ok(())
            }
            MouseEventType::Down => {
                if event.button != Some(MouseButton::Left) {
                    return Ok(());
                }
                self.touch_down.store(true, Ordering::Relaxed);
                self.send_touch(0, 1.0).await
            }
            MouseEventType::Up => {
                if event.button != Some(MouseButton::Left) {
                    return Ok(());
                }
                self.touch_down.store(false, Ordering::Relaxed);
                self.send_touch(1, 0.0).await
            }
            MouseEventType::Scroll => {
                let (width, height) = self.screen();
                let (x, y) = self.position(width, height);
                self.send(ControlCommand::Scroll {
                    x,
                    y,
                    screen_width: width as u16,
                    screen_height: height as u16,
                    hscroll: 0.0,
                    // Frontend: wheel up => +1. scrcpy/SDL uses the same sign.
                    vscroll: event.scroll as f32,
                    buttons: 0,
                })
                .await
            }
        }
    }

    async fn send_consumer(&self, event: ConsumerEvent) -> Result<()> {
        let keycode = match event.usage {
            0x00CD => 85, // play/pause
            0x00B5 => 87, // next
            0x00B6 => 88, // previous
            0x00E9 => keys::VOLUME_UP,
            0x00EA => keys::VOLUME_DOWN,
            0x00E2 => 164, // mute
            _ => return Ok(()),
        };
        self.send_keycode(keycode, 0, true).await?;
        self.send_keycode(keycode, 0, false).await
    }

    async fn reset(&self) -> Result<()> {
        if self.touch_down.swap(false, Ordering::Relaxed) {
            let _ = self.send_touch(1, 0.0).await;
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        let _ = self.reset().await;
        Ok(())
    }

    fn runtime_snapshot(&self) -> HidBackendRuntimeSnapshot {
        let (width, height) = self.screen();
        HidBackendRuntimeSnapshot {
            initialized: true,
            online: true,
            supports_absolute_mouse: true,
            screen_resolution: Some((width, height)),
            ..Default::default()
        }
    }

    fn subscribe_runtime(&self) -> watch::Receiver<()> {
        self.runtime.subscribe()
    }

    fn set_screen_resolution(&self, width: u32, height: u32) {
        if width == 0 || height == 0 || width > 65535 || height > 65535 {
            return;
        }
        self.screen_width.store(width, Ordering::Relaxed);
        self.screen_height.store(height, Ordering::Relaxed);
    }
}

#[async_trait]
impl HidBackend for DisabledPhoneInput {
    async fn init(&self) -> Result<()> {
        Ok(())
    }

    async fn send_keyboard(&self, _event: KeyboardEvent) -> Result<()> {
        Self::unavailable()
    }

    async fn send_mouse(&self, _event: MouseEvent) -> Result<()> {
        Self::unavailable()
    }

    async fn send_consumer(&self, _event: ConsumerEvent) -> Result<()> {
        Self::unavailable()
    }

    async fn reset(&self) -> Result<()> {
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    fn runtime_snapshot(&self) -> HidBackendRuntimeSnapshot {
        HidBackendRuntimeSnapshot {
            initialized: false,
            online: false,
            supports_absolute_mouse: false,
            error: Some("手机控制已暂停".to_string()),
            ..Default::default()
        }
    }

    fn subscribe_runtime(&self) -> watch::Receiver<()> {
        self.runtime.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::{KeyboardModifiers, MouseEvent};

    #[tokio::test]
    async fn maps_mouse_to_finger_touch() {
        let (tx, mut rx) = mpsc::channel(8);
        let backend = ScrcpyBackend::new(tx, 1000, 2000);
        backend
            .send_mouse(MouseEvent::move_abs(16383, 16383))
            .await
            .unwrap();
        backend
            .send_mouse(MouseEvent::button_down(MouseButton::Left))
            .await
            .unwrap();
        backend
            .send_mouse(MouseEvent::button_up(MouseButton::Left))
            .await
            .unwrap();

        match rx.recv().await.unwrap() {
            ControlCommand::Touch { action, .. } => assert_eq!(action, 0),
            other => panic!("expected touch, got {other:?}"),
        }
        match rx.recv().await.unwrap() {
            ControlCommand::Touch { action, .. } => assert_eq!(action, 1),
            other => panic!("expected touch up, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn maps_keyboard_to_android_keycode() {
        let (tx, mut rx) = mpsc::channel(8);
        let backend = ScrcpyBackend::new(tx, 1000, 2000);
        let event = KeyboardEvent::key_down(
            crate::hid::CanonicalKey::KeyA,
            KeyboardModifiers {
                left_ctrl: true,
                ..Default::default()
            },
        );
        backend.send_keyboard(event).await.unwrap();
        match rx.recv().await.unwrap() {
            ControlCommand::Keycode {
                action,
                keycode,
                meta_state,
                ..
            } => {
                assert_eq!(action, 0);
                assert_eq!(keycode, 29);
                assert_eq!(meta_state, 0x1000);
            }
            other => panic!("expected keycode, got {other:?}"),
        }
    }
}
