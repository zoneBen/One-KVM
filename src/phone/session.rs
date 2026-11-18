//! One live scrcpy session: server launch, video/audio/control sockets and input.
//!
//! The socket layout mirrors scrcpy 3.x:
//!   video socket  -> 64-byte device name, 12-byte video header, packet stream
//!   audio socket  -> 4-byte codec id (or a 4-byte disable code), packet stream
//!   control socket-> bidirectional control/device messages
//!
//! All frame headers are 8-byte PTS/flags (big-endian) + 4-byte payload size.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex as ParkingMutex;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf,
};
use tokio::net::TcpListener;
use tokio::process::{Child, ChildStdout};
use tokio::sync::{mpsc, watch, RwLock};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::error::{AppError, Result};
use crate::hid::HidBackend;

use super::adb::AdbClient;
use super::audio_source::PhoneAudioSource;
use super::input::ScrcpyBackend;
use super::protocol::{
    self, is_config_packet, is_key_frame, packet_pts_us, parse_frame_header, parse_video_header,
    ControlCommand, AUDIO_CODEC_OPUS, VIDEO_CODEC_H264,
};
use super::types::PhoneAction;
use super::video_source::PhoneVideoSource;

pub const DEVICE_JAR_PATH: &str = "/data/local/tmp/one-kvm-scrcpy-server.jar";

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);
const HEADER_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PACKET_SIZE: usize = 16 * 1024 * 1024;
const AUDIO_PACKET_DURATION_MS: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhoneSessionState {
    Starting,
    Running,
    Stopped,
    Error,
}

#[derive(Debug, Clone)]
pub struct PhoneSessionStatus {
    pub state: PhoneSessionState,
    pub serial: String,
    pub model: String,
    pub android: String,
    pub width: u32,
    pub height: u32,
    pub audio: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PhoneSessionConfig {
    pub adb_path: String,
    pub serial: String,
    pub server_jar: PathBuf,
    pub server_version: String,
    pub max_size: u16,
    pub max_fps: u32,
    pub video_bit_rate: u32,
    pub audio_bit_rate: u32,
    pub audio: bool,
    pub model: String,
    pub android: String,
}

pub struct PhoneSession {
    adb: AdbClient,
    socket_name: String,
    scid: u32,
    server_pid: AtomicU32,
    video: Arc<PhoneVideoSource>,
    audio: Arc<PhoneAudioSource>,
    input: Arc<ScrcpyBackend>,
    control_tx: mpsc::Sender<ControlCommand>,
    audio_enabled: bool,
    size: Arc<RwLock<(u32, u32)>>,
    state_tx: watch::Sender<PhoneSessionState>,
    state_rx: watch::Receiver<PhoneSessionState>,
    status: RwLock<PhoneSessionStatus>,
    shutdown_tx: watch::Sender<bool>,
    tasks: ParkingMutex<Vec<JoinHandle<()>>>,
    /// Kept separate so `stop()` can close the sockets and give the scrcpy
    /// server a moment to run `CleanUp` before the child is aborted.
    monitor_task: ParkingMutex<Option<JoinHandle<()>>>,
}

impl PhoneSession {
    pub async fn start(config: PhoneSessionConfig) -> Result<Arc<Self>> {
        let adb = AdbClient::new(config.adb_path.clone(), config.serial.clone());

        adb.push(&config.server_jar, DEVICE_JAR_PATH).await?;

        let scid = rand::random::<u32>() & 0x7fff_ffff;
        let socket_name = format!("scrcpy_{scid:08x}");

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|error| {
            AppError::Internal(format!("failed to bind scrcpy listener: {error}"))
        })?;
        let port = listener
            .local_addr()
            .map_err(|error| AppError::Internal(format!("failed to read listener port: {error}")))?
            .port();

        adb.reverse_add(&socket_name, port).await?;

        let server_args = format!(
            "scid={scid:08x} log_level=info video=true audio={} control=true \
             max_size={} max_fps={} video_bit_rate={} audio_bit_rate={} \
             video_codec=h264 audio_codec=opus stay_awake=true power_on=true \
             cleanup=true clipboard_autosync=false \
             video_codec_options=max-b-frames=0,i-frame-interval:float=2",
            config.audio,
            config.max_size,
            config.max_fps,
            config.video_bit_rate,
            config.audio_bit_rate,
        );
        let command = format!(
            "echo $$; CLASSPATH={DEVICE_JAR_PATH}; export CLASSPATH; \
             exec app_process / com.genymobile.scrcpy.Server {} {server_args}",
            config.server_version,
        );

        let mut child = adb.spawn_shell(&command).await?;
        let (pid, stdout_reader) = match read_server_pid(&mut child).await {
            Ok(value) => value,
            Err(error) => {
                let _ = child.kill().await;
                adb.reverse_remove(&socket_name).await;
                return Err(error);
            }
        };

        let result = Self::open_session(
            adb.clone(),
            socket_name.clone(),
            scid,
            pid,
            config,
            listener,
            child,
            stdout_reader,
        )
        .await;

        if result.is_err() {
            adb.kill_pid(pid).await;
            adb.reverse_remove(&socket_name).await;
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn open_session(
        adb: AdbClient,
        socket_name: String,
        scid: u32,
        pid: u32,
        config: PhoneSessionConfig,
        listener: TcpListener,
        mut child: Child,
        stdout_reader: BufReader<ChildStdout>,
    ) -> Result<Arc<Self>> {
        let video_sock = accept(&listener, "video").await?;

        let audio_sock = if config.audio {
            Some(accept(&listener, "audio").await?)
        } else {
            None
        };
        let control_sock = accept(&listener, "control").await?;
        drop(listener);
        adb.reverse_remove(&socket_name).await;

        // The first socket always carries the 64-byte device name, then the
        // 12-byte video header (codec id, width, height).
        let mut video_sock = video_sock;
        let mut device_name = [0u8; 64];
        timeout_read_exact(&mut video_sock, &mut device_name, HEADER_TIMEOUT).await?;
        let mut video_header = [0u8; 12];
        timeout_read_exact(&mut video_sock, &mut video_header, HEADER_TIMEOUT).await?;
        let Some((codec, width, height)) = parse_video_header(&video_header) else {
            return Err(AppError::VideoError("invalid scrcpy video header".into()));
        };
        if codec != VIDEO_CODEC_H264 {
            return Err(AppError::VideoError(format!(
                "unsupported scrcpy video codec: 0x{codec:08x}"
            )));
        }
        if width == 0 || height == 0 {
            return Err(AppError::VideoError("scrcpy video size is zero".into()));
        }

        let mut audio_enabled = false;
        let audio_sock = if let Some(mut audio_sock) = audio_sock {
            let mut codec_buf = [0u8; 4];
            timeout_read_exact(&mut audio_sock, &mut codec_buf, HEADER_TIMEOUT).await?;
            let audio_codec = u32::from_be_bytes(codec_buf);
            match audio_codec {
                0 => {
                    debug!("scrcpy server disabled audio (capture unavailable)");
                    None
                }
                1 => {
                    return Err(AppError::AudioError(
                        "scrcpy server reported an audio configuration error".into(),
                    ));
                }
                AUDIO_CODEC_OPUS => {
                    audio_enabled = true;
                    Some(audio_sock)
                }
                other => {
                    warn!("unsupported scrcpy audio codec 0x{other:08x}; audio disabled");
                    None
                }
            }
        } else {
            None
        };

        let _ = control_sock.set_nodelay(true);

        let (control_tx, control_rx) = mpsc::channel(64);
        let (keyframe_tx, keyframe_rx) = mpsc::channel(4);
        let video = PhoneVideoSource::new(keyframe_tx);
        video.set_geometry(width, height, config.max_fps);
        let audio = PhoneAudioSource::new();
        let input = ScrcpyBackend::new(control_tx.clone(), width, height);
        let size = Arc::new(RwLock::new((width, height)));
        let (state_tx, state_rx) = watch::channel(PhoneSessionState::Starting);
        let (shutdown_tx, _) = watch::channel(false);

        let status = PhoneSessionStatus {
            state: PhoneSessionState::Starting,
            serial: config.serial.clone(),
            model: config.model.clone(),
            android: config.android.clone(),
            width,
            height,
            audio: audio_enabled,
            error: None,
        };

        let session = Arc::new(Self {
            adb,
            socket_name,
            scid,
            server_pid: AtomicU32::new(pid),
            video,
            audio,
            input,
            control_tx,
            audio_enabled,
            size,
            state_tx,
            state_rx,
            status: RwLock::new(status),
            shutdown_tx,
            tasks: ParkingMutex::new(Vec::new()),
            monitor_task: ParkingMutex::new(None),
        });

        session.clone().spawn_video(video_sock);
        if let Some(audio_sock) = audio_sock {
            session.clone().spawn_audio(audio_sock);
        }
        let (control_read, control_write) = tokio::io::split(control_sock);
        session
            .clone()
            .spawn_control(control_read, control_write, control_rx, keyframe_rx);
        session.clone().spawn_log_reader(stdout_reader, "scrcpy");
        if let Some(stderr) = child.stderr.take() {
            session.clone().spawn_log_reader(stderr, "scrcpy stderr");
        }
        session.clone().spawn_child_monitor(child);

        session.set_state(PhoneSessionState::Running).await;
        info!(
            "Phone session started: {}x{}, audio={}, pid={}",
            width, height, audio_enabled, pid
        );
        Ok(session)
    }

    fn spawn_video(self: Arc<Self>, stream: tokio::net::TcpStream) {
        let mut shutdown = self.shutdown_tx.subscribe();
        let session = self.clone();
        let handle = tokio::spawn(async move {
            let mut stream = stream;
            let mut header = [0u8; 12];
            loop {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            break;
                        }
                    }
                    result = tokio::time::timeout(READ_TIMEOUT, stream.read_exact(&mut header)) => {
                        if !matches!(result, Ok(Ok(_))) {
                            break;
                        }
                    }
                }
                let Some((pts_and_flags, size)) = parse_frame_header(&header) else {
                    break;
                };
                let size = size as usize;
                if size == 0 || size > MAX_PACKET_SIZE {
                    break;
                }
                let mut payload = vec![0u8; size];
                if !matches!(
                    tokio::time::timeout(READ_TIMEOUT, stream.read_exact(&mut payload)).await,
                    Ok(Ok(_))
                ) {
                    break;
                }
                let data = Bytes::from(payload);
                if is_config_packet(pts_and_flags) {
                    session.video.publish_config(data);
                } else {
                    let pts_ms = packet_pts_us(pts_and_flags) / 1000;
                    session
                        .video
                        .publish_frame(data, pts_ms, is_key_frame(pts_and_flags));
                }
            }
            if !*shutdown.borrow() {
                session.set_error("手机视频流已断开").await;
            }
        });
        self.push_task(handle);
    }

    fn spawn_audio(self: Arc<Self>, stream: tokio::net::TcpStream) {
        let mut shutdown = self.shutdown_tx.subscribe();
        let session = self.clone();
        let handle = tokio::spawn(async move {
            let mut stream = stream;
            let mut header = [0u8; 12];
            loop {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            break;
                        }
                    }
                    result = tokio::time::timeout(READ_TIMEOUT, stream.read_exact(&mut header)) => {
                        if !matches!(result, Ok(Ok(_))) {
                            break;
                        }
                    }
                }
                let Some((pts_and_flags, size)) = parse_frame_header(&header) else {
                    break;
                };
                let size = size as usize;
                if size == 0 || size > MAX_PACKET_SIZE {
                    break;
                }
                let mut payload = vec![0u8; size];
                if !matches!(
                    tokio::time::timeout(READ_TIMEOUT, stream.read_exact(&mut payload)).await,
                    Ok(Ok(_))
                ) {
                    break;
                }
                if !is_config_packet(pts_and_flags) {
                    session
                        .audio
                        .publish(Bytes::from(payload), AUDIO_PACKET_DURATION_MS);
                }
            }
            if !*shutdown.borrow() {
                // Audio is optional: keep video/control alive if the audio
                // socket alone goes away.
                warn!("Phone audio stream disconnected");
            }
        });
        self.push_task(handle);
    }

    fn spawn_control(
        self: Arc<Self>,
        mut read: ReadHalf<tokio::net::TcpStream>,
        mut write: WriteHalf<tokio::net::TcpStream>,
        mut commands: mpsc::Receiver<ControlCommand>,
        mut keyframes: mpsc::Receiver<()>,
    ) {
        let mut shutdown = self.shutdown_tx.subscribe();
        let writer = tokio::spawn(async move {
            let mut keyframes_open = true;
            loop {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            break;
                        }
                    }
                    command = commands.recv() => {
                        let Some(command) = command else { break };
                        if write.write_all(&command.serialize()).await.is_err() {
                            break;
                        }
                    }
                    keyframe = keyframes.recv(), if keyframes_open => {
                        if keyframe.is_none() {
                            keyframes_open = false;
                            continue;
                        }
                        if write
                            .write_all(&ControlCommand::ResetVideo.serialize())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
        self.push_task(writer);

        let mut shutdown = self.shutdown_tx.subscribe();
        let reader = tokio::spawn(async move {
            let mut buffer = [0u8; 1024];
            loop {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            break;
                        }
                    }
                    read = read.read(&mut buffer) => {
                        match read {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                }
            }
        });
        self.push_task(reader);
    }

    fn spawn_child_monitor(self: Arc<Self>, mut child: Child) {
        let session = self.clone();
        let handle = tokio::spawn(async move {
            let status = child.wait().await;
            if !*session.shutdown_tx.borrow() {
                let reason = match status {
                    Ok(status) => format!("scrcpy-server exited: {status}"),
                    Err(error) => format!("scrcpy-server wait failed: {error}"),
                };
                session.set_error(reason).await;
            }
        });
        *self.monitor_task.lock() = Some(handle);
    }

    fn spawn_log_reader<R>(self: Arc<Self>, reader: R, label: &'static str)
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let handle = tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = line.trim();
                        if !line.is_empty() {
                            debug!("{}: {}", label, line);
                        }
                    }
                }
            }
        });
        self.push_task(handle);
    }

    fn push_task(&self, handle: JoinHandle<()>) {
        self.tasks.lock().push(handle);
    }

    pub async fn stop(&self) {
        let _ = self.shutdown_tx.send(true);

        // Drop the scrcpy sockets first: the server then exits normally and
        // its CleanUp thread can restore the phone state before we kill it.
        let handles = {
            let mut tasks = self.tasks.lock();
            std::mem::take(&mut *tasks)
        };
        for handle in handles {
            handle.abort();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;

        let pid = self.server_pid.load(Ordering::Relaxed);
        if pid > 0 {
            self.adb.kill_pid(pid).await;
        }
        self.adb.reverse_remove(&self.socket_name).await;

        if let Some(handle) = self.monitor_task.lock().take() {
            handle.abort();
        }
        self.set_state(PhoneSessionState::Stopped).await;
    }

    pub async fn set_video_size(&self, width: u32, height: u32) -> Result<()> {
        if !(1..=65535).contains(&width) || !(1..=65535).contains(&height) {
            return Err(AppError::BadRequest(
                "手机视频尺寸必须在 1..65535 之间".to_string(),
            ));
        }
        *self.size.write().await = (width, height);
        self.input.set_screen_resolution(width, height);
        self.video.set_geometry(width, height, self.video.fps());
        Ok(())
    }

    pub async fn send_action(&self, action: PhoneAction) -> Result<()> {
        let (keycode, meta_state) = match action {
            PhoneAction::Back => (protocol::keys::BACK, 0u32),
            PhoneAction::Home => (protocol::keys::HOME, 0u32),
            PhoneAction::Recents => (protocol::keys::APP_SWITCH, 0u32),
            PhoneAction::Power => (protocol::keys::POWER, 0u32),
            PhoneAction::VolumeUp => (protocol::keys::VOLUME_UP, 0u32),
            PhoneAction::VolumeDown => (protocol::keys::VOLUME_DOWN, 0u32),
            PhoneAction::Wake => (protocol::keys::WAKEUP, 0u32),
            PhoneAction::Rotate => {
                return self.send_control(ControlCommand::RotateDevice).await;
            }
            PhoneAction::ScreenOn => {
                return self
                    .send_control(ControlCommand::SetDisplayPower(true))
                    .await;
            }
            PhoneAction::ScreenOff => {
                return self
                    .send_control(ControlCommand::SetDisplayPower(false))
                    .await;
            }
        };
        self.send_control(ControlCommand::Keycode {
            action: 0,
            keycode,
            repeat: 0,
            meta_state,
        })
        .await?;
        self.send_control(ControlCommand::Keycode {
            action: 1,
            keycode,
            repeat: 0,
            meta_state,
        })
        .await
    }

    async fn send_control(&self, command: ControlCommand) -> Result<()> {
        self.control_tx
            .send(command)
            .await
            .map_err(|_| AppError::ServiceUnavailable("手机控制通道已断开".to_string()))
    }

    pub fn video_source(&self) -> Arc<PhoneVideoSource> {
        self.video.clone()
    }

    pub fn audio_source(&self) -> Arc<PhoneAudioSource> {
        self.audio.clone()
    }

    pub fn audio_enabled(&self) -> bool {
        self.audio_enabled
    }

    pub fn control_backend(&self) -> Arc<dyn HidBackend> {
        self.input.clone()
    }

    pub fn state(&self) -> PhoneSessionState {
        *self.state_rx.borrow()
    }

    pub fn state_rx(&self) -> watch::Receiver<PhoneSessionState> {
        self.state_rx.clone()
    }

    pub fn scid(&self) -> u32 {
        self.scid
    }

    pub async fn status(&self) -> PhoneSessionStatus {
        let mut status = self.status.read().await.clone();
        status.state = *self.state_rx.borrow();
        let (width, height) = *self.size.read().await;
        status.width = width;
        status.height = height;
        status
    }

    async fn set_state(&self, state: PhoneSessionState) {
        let _ = self.state_tx.send(state);
        let mut status = self.status.write().await;
        status.state = state;
    }

    async fn set_error(&self, error: impl Into<String>) {
        if *self.shutdown_tx.borrow() {
            return;
        }
        let error = error.into();
        warn!("Phone session error: {}", error);
        let _ = self.state_tx.send(PhoneSessionState::Error);
        let mut status = self.status.write().await;
        status.state = PhoneSessionState::Error;
        status.error = Some(error);
    }
}

async fn accept(listener: &TcpListener, name: &str) -> Result<tokio::net::TcpStream> {
    tokio::time::timeout(ACCEPT_TIMEOUT, listener.accept())
        .await
        .map_err(|_| AppError::ServiceUnavailable(format!("等待 scrcpy {name} socket 超时")))?
        .map(|(stream, _)| stream)
        .map_err(|error| AppError::Internal(format!("接受 scrcpy {name} socket 失败: {error}")))
}

async fn timeout_read_exact(
    stream: &mut tokio::net::TcpStream,
    buffer: &mut [u8],
    timeout: Duration,
) -> Result<()> {
    tokio::time::timeout(timeout, stream.read_exact(buffer))
        .await
        .map_err(|_| AppError::ServiceUnavailable("读取 scrcpy 头部超时".to_string()))?
        .map_err(|error| AppError::Internal(format!("读取 scrcpy 头部失败: {error}")))?;
    Ok(())
}

async fn read_server_pid(child: &mut Child) -> Result<(u32, BufReader<ChildStdout>)> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Internal("scrcpy-server stdout unavailable".to_string()))?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let result = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            line.clear();
            let read = reader.read_line(&mut line).await.map_err(|error| {
                AppError::Internal(format!("读取 scrcpy-server PID 失败: {error}"))
            })?;
            if read == 0 {
                return Err(AppError::Internal(
                    "scrcpy-server exited before reporting its pid".to_string(),
                ));
            }
            if let Ok(pid) = line.trim().parse::<u32>() {
                return Ok(pid);
            }
        }
    })
    .await;
    match result {
        Ok(Ok(pid)) => Ok((pid, reader)),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(AppError::ServiceUnavailable(
            "等待 scrcpy-server PID 超时".to_string(),
        )),
    }
}
