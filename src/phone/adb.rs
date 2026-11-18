//! Thin async wrapper around the system `adb` binary.
//!
//! One-KVM intentionally shells out to adb instead of reimplementing the ADB
//! protocol: authorization, USB permissions, reverse/forward tunnels and the
//! adb server lifecycle are already handled by the distribution package.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tracing::debug;

use crate::error::{AppError, Result};

const DEVICES_TIMEOUT: Duration = Duration::from_secs(10);
const SHELL_TIMEOUT: Duration = Duration::from_secs(15);
const PUSH_TIMEOUT: Duration = Duration::from_secs(30);
const TUNNEL_TIMEOUT: Duration = Duration::from_secs(10);
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct AdbDevice {
    pub serial: String,
    pub state: String,
    pub usb_path: Option<String>,
    pub model: Option<String>,
    pub product: Option<String>,
    pub device: Option<String>,
}

impl AdbDevice {
    pub fn is_authorized(&self) -> bool {
        self.state == "device"
    }

    pub fn is_unauthorized(&self) -> bool {
        self.state == "unauthorized"
    }

    pub fn is_usb(&self) -> bool {
        self.usb_path.is_some()
    }
}

pub fn default_adb_path() -> String {
    std::env::var("ONE_KVM_ADB").unwrap_or_else(|_| "adb".to_string())
}

async fn run_plain(adb: &str, args: &[&str], timeout: Duration) -> Result<String> {
    let mut command = Command::new(adb);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = command
        .spawn()
        .map_err(|error| AppError::Internal(format!("failed to run {adb}: {error}")))?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| AppError::ServiceUnavailable(format!("adb {args:?} timed out")))?
        .map_err(AppError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::Internal(format!(
            "adb {args:?} failed: {}",
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub async fn adb_version(adb: &str) -> Result<String> {
    let text = run_plain(adb, &["version"], DEVICES_TIMEOUT).await?;
    Ok(text.lines().next().unwrap_or_default().trim().to_string())
}

pub async fn list_devices(adb: &str) -> Result<Vec<AdbDevice>> {
    let text = run_plain(adb, &["devices", "-l"], DEVICES_TIMEOUT).await?;
    Ok(parse_devices(&text))
}

fn parse_devices(text: &str) -> Vec<AdbDevice> {
    let mut devices = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with("List of devices") || line.starts_with('*') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some(serial) = fields.next() else {
            continue;
        };
        let Some(state) = fields.next() else {
            continue;
        };
        let mut device = AdbDevice {
            serial: serial.to_string(),
            state: state.to_string(),
            usb_path: None,
            model: None,
            product: None,
            device: None,
        };
        for field in fields {
            if let Some(value) = field.strip_prefix("usb:") {
                device.usb_path = Some(value.to_string());
            } else if let Some(value) = field.strip_prefix("model:") {
                device.model = Some(value.to_string());
            } else if let Some(value) = field.strip_prefix("product:") {
                device.product = Some(value.to_string());
            } else if let Some(value) = field.strip_prefix("device:") {
                device.device = Some(value.to_string());
            }
        }
        devices.push(device);
    }
    devices
}

/// Wait for a USB-attached Android device.
///
/// An unauthorized device is returned immediately so the UI can explain how to
/// accept the RSA prompt; a fully authorized device takes priority.
pub async fn wait_for_usb_device(adb: &str, timeout: Duration) -> Result<AdbDevice> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(devices) = list_devices(adb).await {
            if let Some(device) = devices.iter().find(|d| d.is_usb() && d.is_authorized()) {
                return Ok(device.clone());
            }
            if let Some(device) = devices.iter().find(|d| d.is_usb() && d.is_unauthorized()) {
                return Ok(device.clone());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(AppError::ServiceUnavailable(
                "未检测到通过 USB 连接的 Android 手机".to_string(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[derive(Clone)]
pub struct AdbClient {
    adb: String,
    serial: String,
}

impl AdbClient {
    pub fn new(adb: impl Into<String>, serial: impl Into<String>) -> Self {
        Self {
            adb: adb.into(),
            serial: serial.into(),
        }
    }

    pub fn serial(&self) -> &str {
        &self.serial
    }

    async fn run(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let mut command = Command::new(&self.adb);
        command
            .arg("-s")
            .arg(&self.serial)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = command
            .spawn()
            .map_err(|error| AppError::Internal(format!("failed to run {}: {error}", self.adb)))?;
        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| AppError::ServiceUnavailable(format!("adb {args:?} timed out")))?
            .map_err(AppError::Io)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppError::Internal(format!(
                "adb {args:?} failed: {}",
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    pub async fn push(&self, local: &Path, remote: &str) -> Result<()> {
        let local = local.to_string_lossy().to_string();
        self.run(&["push", local.as_str(), remote], PUSH_TIMEOUT)
            .await?;
        debug!("adb push {} -> {}", local, remote);
        Ok(())
    }

    pub async fn shell(&self, command: &str) -> Result<String> {
        self.run(&["shell", command], SHELL_TIMEOUT).await
    }

    pub async fn reverse_add(&self, socket_name: &str, local_port: u16) -> Result<()> {
        let remote = format!("localabstract:{socket_name}");
        let local = format!("tcp:{local_port}");
        self.run(
            &["reverse", remote.as_str(), local.as_str()],
            TUNNEL_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    pub async fn reverse_remove(&self, socket_name: &str) {
        let remote = format!("localabstract:{socket_name}");
        let _ = self
            .run(&["reverse", "--remove", remote.as_str()], TUNNEL_TIMEOUT)
            .await;
    }

    /// Spawn a long-running `adb shell` command, keeping stdout/stderr pipes.
    pub async fn spawn_shell(&self, command: &str) -> Result<Child> {
        let mut child = Command::new(&self.adb);
        child
            .arg("-s")
            .arg(&self.serial)
            .arg("shell")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        child
            .spawn()
            .map_err(|error| AppError::Internal(format!("failed to run adb shell: {error}")))
    }

    pub async fn kill_pid(&self, pid: u32) {
        let command = format!("kill -9 {pid}");
        let _ = self.run(&["shell", command.as_str()], KILL_TIMEOUT).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_adb_devices_output() {
        let text = "List of devices attached\n\
                    3B1621003AW00000       device usb:5-1 product:PLQ110 model:PLQ110 device:OP6113L1 transport_id:1\n\
                    ABC123                 unauthorized usb:1-2 transport_id:2\n";
        let devices = parse_devices(text);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].serial, "3B1621003AW00000");
        assert_eq!(devices[0].state, "device");
        assert_eq!(devices[0].usb_path.as_deref(), Some("5-1"));
        assert_eq!(devices[0].model.as_deref(), Some("PLQ110"));
        assert!(devices[1].is_unauthorized());
    }
}
