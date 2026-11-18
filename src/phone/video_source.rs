//! Broadcasts scrcpy H.264 frames to WebRTC sessions.
//!
//! This is the phone equivalent of `SharedVideoPipeline::subscribe()`: it
//! exposes an [`EncodedVideoFrameReceiver`] per viewer, keeps a bootstrap
//! config+keyframe pair for fast joins, and forwards keyframe requests to the
//! scrcpy control channel.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::video::codec::h264_bitstream;
use crate::video::format::{PixelFormat, Resolution};
use crate::video::pipeline::{
    encoded_frame_channel, EncodedFrameSender, EncodedVideoFrame, EncodedVideoFrameReceiver,
};
use crate::video::types::VideoEncoderType;

const SUBSCRIBER_CHANNEL_CAPACITY: usize = 4;
const FRAME_DURATION_MS: u64 = 16;

pub struct PhoneVideoSource {
    subscribers: Mutex<Vec<EncodedFrameSender>>,
    bootstrap: RwLock<Vec<Arc<EncodedVideoFrame>>>,
    last_config: RwLock<Option<Arc<EncodedVideoFrame>>>,
    profile_tx: watch::Sender<Option<String>>,
    profile_rx: watch::Receiver<Option<String>>,
    keyframe_tx: mpsc::Sender<()>,
    geometry: RwLock<(Resolution, u32)>,
    sequence: AtomicU64,
}

impl PhoneVideoSource {
    pub fn new(keyframe_tx: mpsc::Sender<()>) -> Arc<Self> {
        let (profile_tx, profile_rx) = watch::channel(None);
        Arc::new(Self {
            subscribers: Mutex::new(Vec::new()),
            bootstrap: RwLock::new(Vec::new()),
            last_config: RwLock::new(None),
            profile_tx,
            profile_rx,
            keyframe_tx,
            geometry: RwLock::new((Resolution::HD720, 60)),
            sequence: AtomicU64::new(0),
        })
    }

    pub fn subscribe(&self) -> EncodedVideoFrameReceiver {
        let (sender, receiver) = encoded_frame_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        for frame in self.bootstrap.read().iter() {
            let _ = sender.try_send(frame.clone());
        }
        self.subscribers.lock().push(sender);
        receiver
    }

    pub fn subscriber_count(&self) -> usize {
        let mut subscribers = self.subscribers.lock();
        subscribers.retain(|sender| !sender.is_closed());
        subscribers.len()
    }

    /// Ask the phone to reset its encoder, which forces a fresh IDR/config.
    pub fn request_keyframe(&self) {
        let _ = self.keyframe_tx.try_send(());
    }

    pub fn set_geometry(&self, width: u32, height: u32, fps: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let mut geometry = self.geometry.write();
        let fps = fps.clamp(1, 120);
        let (current, current_fps) = *geometry;
        if current.width != width || current.height != height || current_fps != fps {
            *geometry = (Resolution::new(width, height), fps);
            debug!("Phone video geometry: {}x{} @ {}fps", width, height, fps);
        }
    }

    pub fn geometry(&self) -> (Resolution, PixelFormat, u32) {
        let geometry = self.geometry.read();
        let (resolution, fps) = *geometry;
        (resolution, PixelFormat::Nv12, fps)
    }

    pub fn fps(&self) -> u32 {
        let geometry = self.geometry.read();
        let (_, fps) = *geometry;
        fps
    }

    pub fn profile_level_id(&self) -> Option<String> {
        self.profile_rx.borrow().clone()
    }

    /// Wait briefly for the first SPS so WebRTC's fmtp line matches the stream.
    pub async fn wait_profile_level_id(&self, timeout: Duration) -> Option<String> {
        if let Some(value) = self.profile_level_id() {
            return Some(value);
        }
        let mut rx = self.profile_rx.clone();
        let wait = async {
            loop {
                if rx.changed().await.is_err() {
                    return None;
                }
                if let Some(value) = rx.borrow().clone() {
                    return Some(value);
                }
            }
        };
        tokio::time::timeout(timeout, wait).await.ok().flatten()
    }

    pub fn publish_config(&self, data: Bytes) {
        self.update_profile(&data);
        let frame = self.make_frame(data, 0, false, true);
        *self.last_config.write() = Some(frame.clone());
        // Do not pair an old keyframe with a new config (rotation/resolution).
        *self.bootstrap.write() = vec![frame.clone()];
        self.broadcast(frame);
    }

    pub fn publish_frame(&self, data: Bytes, pts_ms: i64, keyframe: bool) {
        self.update_profile(&data);
        if keyframe && h264_bitstream::has_sps_pps(&data) {
            // The keyframe carries its own parameter sets; drop a stale config.
            *self.last_config.write() = None;
        }
        let frame = self.make_frame(data, pts_ms, keyframe, false);
        if keyframe {
            let mut bootstrap = Vec::with_capacity(2);
            if let Some(config) = self.last_config.read().clone() {
                bootstrap.push(config);
            }
            bootstrap.push(frame.clone());
            *self.bootstrap.write() = bootstrap;
        }
        self.broadcast(frame);
    }

    fn make_frame(
        &self,
        data: Bytes,
        pts_ms: i64,
        keyframe: bool,
        _config: bool,
    ) -> Arc<EncodedVideoFrame> {
        Arc::new(EncodedVideoFrame {
            data,
            pts_ms,
            is_keyframe: keyframe,
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
            duration: Duration::from_millis(FRAME_DURATION_MS),
            codec: VideoEncoderType::H264,
        })
    }

    fn update_profile(&self, data: &[u8]) {
        if let Some(profile) = h264_bitstream::extract_profile_level_id(data) {
            let current = self.profile_tx.borrow().clone();
            if current.as_deref() != Some(profile.as_str()) {
                let _ = self.profile_tx.send(Some(profile));
            }
        }
    }

    fn broadcast(&self, frame: Arc<EncodedVideoFrame>) {
        let mut subscribers = self.subscribers.lock();
        subscribers.retain(|sender| !sender.is_closed());
        for sender in subscribers.iter() {
            // Drop on backpressure: the WebRTC session detects the sequence gap
            // and asks for a fresh keyframe, which is cheaper than buffering.
            let _ = sender.try_send(frame.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscriber_receives_bootstrap_and_live_frame() {
        let (keyframe_tx, _keyframe_rx) = mpsc::channel(1);
        let source = PhoneVideoSource::new(keyframe_tx);
        source.publish_config(Bytes::from_static(&[0x00, 0x00, 0x00, 0x01, 0x67]));
        source.publish_frame(
            Bytes::from_static(&[0x00, 0x00, 0x00, 0x01, 0x65]),
            10,
            true,
        );

        let mut receiver = source.subscribe();
        let first = receiver.recv().await.expect("bootstrap frame");
        assert!(!first.is_keyframe);
        let second = receiver.recv().await.expect("keyframe");
        assert!(second.is_keyframe);

        source.publish_frame(Bytes::from_static(&[0x00, 0x00, 0x01]), 20, false);
        let third = receiver.recv().await.expect("live frame");
        assert_eq!(third.pts_ms, 20);
    }

    #[test]
    fn geometry_ignores_zero() {
        let (keyframe_tx, _) = mpsc::channel(1);
        let source = PhoneVideoSource::new(keyframe_tx);
        source.set_geometry(1600, 720, 60);
        source.set_geometry(0, 720, 60);
        let (resolution, _, fps) = source.geometry();
        assert_eq!(resolution, Resolution::new(1600, 720));
        assert_eq!(fps, 60);
    }
}
