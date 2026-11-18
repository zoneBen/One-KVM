//! Phone audio (scrcpy Opus packets) fan-out to WebRTC sessions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::{broadcast, mpsc};

use crate::audio::OpusFrame;

const BROADCAST_CAPACITY: usize = 32;
const SUBSCRIBER_CHANNEL_CAPACITY: usize = 16;

pub struct PhoneAudioSource {
    sender: broadcast::Sender<Arc<OpusFrame>>,
    sequence: AtomicU64,
}

impl PhoneAudioSource {
    pub fn new() -> Arc<Self> {
        let (sender, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            sender,
            sequence: AtomicU64::new(0),
        })
    }

    /// Publish one encoded Opus packet. `duration_ms` is currently fixed to
    /// 20 ms because `UniversalSession` writes 960 samples per packet.
    pub fn publish(&self, data: Bytes, duration_ms: u32) {
        let frame = Arc::new(OpusFrame {
            data,
            duration_ms,
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
        });
        let _ = self.sender.send(frame);
    }

    pub fn subscribe(&self) -> mpsc::Receiver<Arc<OpusFrame>> {
        let mut receiver = self.sender.subscribe();
        let (sender, output) = mpsc::channel(SUBSCRIBER_CHANNEL_CAPACITY);
        tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(frame) => {
                        if sender.send(frame).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::debug!("Phone audio subscriber lagged by {} packets", skipped);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscribers_receive_published_audio() {
        let source = PhoneAudioSource::new();
        let mut receiver = source.subscribe();
        source.publish(Bytes::from_static(&[1, 2, 3]), 20);
        let frame = receiver.recv().await.expect("opus frame");
        assert_eq!(frame.data.as_ref(), &[1, 2, 3]);
        assert_eq!(frame.duration_ms, 20);
    }
}
