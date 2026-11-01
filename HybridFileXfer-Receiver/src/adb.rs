//! Thin wrapper around the adb command line tool.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug)]
pub struct Device {
    pub serial: String,
    pub state: String,
}

#[derive(Clone)]
pub struct Adb {
    pub path: PathBuf,
}

impl Adb {
    /// Locate adb: explicit config first, then next to the executable, then the
    /// bundled copy of the Java client, then `PATH`.
    pub fn detect(exe_dir: &Path, configured: Option<&Path>) -> Option<Adb> {
        if let Some(path) = configured {
            if path.is_file() {
                return Some(Adb {
                    path: path.to_path_buf(),
                });
            }
        }
        let exe_name = if cfg!(windows) { "adb.exe" } else { "adb" };
        let mut candidates: Vec<PathBuf> = vec![
            exe_dir.join(exe_name),
            exe_dir.join("platform-tools").join(exe_name),
            exe_dir.join("..").join("HybridFileXfer-PC").join(exe_name),
            exe_dir.join("..").join("adb").join(exe_name),
            exe_dir.join("..").join(exe_name),
        ];
        if let Ok(current) = std::env::current_exe() {
            if let Some(dir) = current.parent() {
                candidates.push(dir.join(exe_name));
                candidates.push(dir.join("platform-tools").join(exe_name));
            }
        }
        for candidate in candidates {
            if candidate.is_file() {
                let path = std::fs::canonicalize(&candidate).unwrap_or(candidate);
                let text = path.to_string_lossy().to_string();
                let path = match text.strip_prefix("\\\\?\\") {
                    Some(stripped) => PathBuf::from(stripped),
                    None => PathBuf::from(text),
                };
                return Some(Adb { path });
            }
        }
        if command_exists("adb") {
            return Some(Adb {
                path: PathBuf::from("adb"),
            });
        }
        None
    }

    pub fn version(&self) -> Option<String> {
        match run(&mut self.command(), Duration::from_secs(10)) {
            Ok(out) => {
                if out.success {
                    Some(out.stdout.trim().to_string())
                } else {
                    crate::log::debug(&format!(
                        "adb version failed with status, stderr: {}",
                        out.stderr.trim()
                    ));
                    None
                }
            }
            Err(e) => {
                crate::log::debug(&format!("cannot run adb: {}", e));
                None
            }
        }
    }

    pub fn start_server(&self) -> io::Result<()> {
        let mut cmd = self.command();
        cmd.arg("start-server");
        let out = run(&mut cmd, Duration::from_secs(30))?;
        if out.success {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("adb start-server failed: {}", out.stderr.trim()),
            ))
        }
    }

    pub fn list_devices(&self) -> io::Result<Vec<Device>> {
        let mut cmd = self.command();
        cmd.arg("devices");
        let out = run(&mut cmd, Duration::from_secs(15))?;
        if !out.success {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("adb devices failed: {}", out.stderr.trim()),
            ));
        }
        let mut devices = Vec::new();
        for line in out.stdout.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("List of devices") || line.starts_with('*') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(serial) = parts.next() else { continue };
            let state = parts.next().unwrap_or("unknown");
            devices.push(Device {
                serial: serial.to_string(),
                state: state.to_string(),
            });
        }
        Ok(devices)
    }

    /// 本机 <local_port> → 手机 <remote_port>（多设备时每台用不同的本机端口）
    pub fn forward(&self, serial: &str, local_port: u16, remote_port: u16) -> io::Result<bool> {
        let mut cmd = self.command();
        cmd.arg("-s")
            .arg(serial)
            .arg("forward")
            .arg(format!("tcp:{local_port}"))
            .arg(format!("tcp:{remote_port}"));
        let out = run(&mut cmd, Duration::from_secs(15))?;
        Ok(out.success)
    }

    #[allow(dead_code)]
    pub fn remove_forward(&self, serial: &str, port: u16) {
        let mut cmd = self.command();
        cmd.arg("-s")
            .arg(serial)
            .arg("forward")
            .arg("--remove")
            .arg(format!("tcp:{port}"));
        let _ = run(&mut cmd, Duration::from_secs(10));
    }

    /// Ask the phone's foreground service to start (or restart) listening.
    /// Returns Ok(true) when the component was started by the shell.
    pub fn start_service(
        &self,
        serial: &str,
        component: &str,
        fresh: bool,
    ) -> io::Result<(bool, String)> {
        let mut cmd = self.command();
        cmd.arg("-s")
            .arg(serial)
            .arg("shell")
            .arg("am")
            .arg("start-foreground-service")
            .arg("-n")
            .arg(component);
        if fresh {
            cmd.arg("--ez").arg("fresh").arg("true");
        }
        let out = run(&mut cmd, Duration::from_secs(20))?;
        let text = format!("{}{}", out.stdout.trim(), out.stderr.trim());
        let ok = out.success
            && !text.contains("Error:")
            && !text.contains("Exception")
            && !text.contains("does not exist");
        Ok((ok, text))
    }

    fn command(&self) -> Command {
        Command::new(&self.path)
    }
}

#[cfg(windows)]
fn hide_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_window(_cmd: &mut Command) {}

struct Output {
    success: bool,
    stdout: String,
    stderr: String,
}

fn run(cmd: &mut Command, timeout: Duration) -> io::Result<Output> {
    hide_window(cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => {
                let output = child.wait_with_output()?;
                return Ok(Output {
                    success: status.success(),
                    stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                });
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("command timed out after {:?}", timeout),
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn command_exists(name: &str) -> bool {
    let mut cmd = Command::new(name);
    cmd.arg("version");
    run(&mut cmd, Duration::from_secs(10))
        .map(|out| out.success)
        .unwrap_or(false)
}

/// 多设备：返回所有就绪设备（可按 serial 过滤），最多 `limit` 台
pub fn pick_devices<'a>(
    devices: &'a [Device],
    wanted: Option<&str>,
    limit: usize,
) -> Vec<&'a Device> {
    let mut ready: Vec<&'a Device> = devices.iter().filter(|d| d.state == "device").collect();
    if let Some(wanted) = wanted {
        let wanted = wanted.trim();
        if !wanted.is_empty() {
            ready.retain(|d| d.serial == wanted);
        }
    }
    ready.truncate(limit.max(1));
    ready
}

pub fn pick_device<'a>(devices: &'a [Device], wanted: Option<&str>) -> Result<&'a Device, String> {
    if let Some(wanted) = wanted {
        if let Some(device) = devices.iter().find(|d| d.serial == wanted) {
            if device.state == "device" {
                return Ok(device);
            }
            return Err(format!(
                "device {} is {}, not ready",
                device.serial, device.state
            ));
        }
        return Err(format!("device {} not found", wanted));
    }
    let ready: Vec<&Device> = devices.iter().filter(|d| d.state == "device").collect();
    match ready.len() {
        0 => Err("no authorized device".to_string()),
        1 => Ok(ready[0]),
        _ => Err("more than one device connected, please set \"device\" in config.ini".to_string()),
    }
}