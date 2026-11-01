//! 连接与接收的主循环，供 GUI / 命令行共用。
//!
//! 自适应连接顺序：
//! 1. 有 adb 设备（数据线）→ adb 端口转发 + 拉起手机服务，控制通道走 127.0.0.1；
//!    手机端会把所有网卡都报上来，电脑能连上的通道（USB + WIFI）会同时使用；
//! 2. 没有 adb 设备 → 用局域网广播发现手机，直接连它的 IP（只要同一 WIFI）；
//! 3. 也可以手动指定 IP。

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::adb::{self, Adb};
use crate::discovery::{Discovery, DISCOVERY_PORT};
use crate::i18n::tr;
use crate::log;
use crate::receiver::Receiver;
use crate::session::{Session, SessionEnd};
use crate::util;

#[derive(Clone, Debug, PartialEq)]
pub enum EngineState {
    Stopped,
    /// 正在扫描设备 / 等待连接，附带一句说明
    Waiting(String),
    /// 已连上，附带通道名
    Connected(Vec<String>),
    /// 出错了，仍在重试
    Retrying(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConnectMode {
    /// 自动：adb 优先，其次局域网发现
    Auto,
    /// 只走局域网发现（测试 / 不想开 USB 调试时）
    Discovery,
    /// 直接连指定 IP
    Ip(IpAddr),
}

#[derive(Clone, Debug)]
pub struct EngineOptions {
    pub mode: ConnectMode,
    pub port: u16,
    pub receive_dir: PathBuf,
    pub device: Option<String>,
    pub component: String,
    pub adb_path: Option<PathBuf>,
    /// 用来查找旁边目录里的 adb.exe
    pub adb_search_dir: PathBuf,
}

pub type StateSink = Arc<dyn Fn(EngineState) + Send + Sync>;
/// 多设备：状态回调带上设备串号
pub type MultiStateSink = Arc<dyn Fn(&str, EngineState) + Send + Sync>;

/// 多设备监督者：为每台就绪设备各起一条独立会话线程（最多 max_devices 台）。
/// 单台设备的逻辑完全复用 run_auto，避免重写已验证的路径。
pub fn run_multi(
    options: EngineOptions,
    stop: Arc<AtomicBool>,
    max_devices: usize,
    on_state: MultiStateSink,
) {
    crate::webserve::init(&util::normalize_dir(&options.receive_dir));
    let limit = max_devices.clamp(1, 8);
    let mut running: std::collections::HashMap<String, thread::JoinHandle<()>> =
        std::collections::HashMap::new();

    while !stop.load(Ordering::Relaxed) {
        // 清掉已经结束的会话
        running.retain(|_, handle| !handle.is_finished());

        if let Some(adb) = Adb::detect(&options.adb_search_dir, options.adb_path.as_deref()) {
            if let Ok(devices) = adb.list_devices() {
                let ready = adb::pick_devices(&devices, options.device.as_deref(), limit);
                for device in ready {
                    let serial = device.serial.clone();
                    if running.contains_key(&serial) {
                        continue;
                    }
                    let mut per_device = options.clone();
                    per_device.device = Some(serial.clone());
                    let stop_for_device = Arc::clone(&stop);
                    let sink = {
                        let on_state = Arc::clone(&on_state);
                        let serial = serial.clone();
                        Arc::new(move |state: EngineState| on_state(&serial, state))
                            as StateSink
                    };
                    log::info(&format!("{} {}", tr("开始连接设备", "connecting device"), serial));
                    running.insert(
                        serial,
                        thread::spawn(move || {
                            let stop = stop_for_device;
                            let on_state = sink;
                            run_auto(&per_device, &stop, &on_state);
                        }),
                    );
                }
            }
        }

        if !sleep_unless_stopped(&stop, Duration::from_secs(2)) {
            break;
        }
    }

    for (_, handle) in running {
        let _ = handle.join();
    }
    on_state("", EngineState::Stopped);
}

/// 阻塞运行接收循环，直到 stop 被置为 true。
pub fn run(options: EngineOptions, stop: Arc<AtomicBool>, on_state: StateSink) {
    crate::webserve::init(&util::normalize_dir(&options.receive_dir));
    match options.mode {
        ConnectMode::Auto => run_auto(&options, &stop, &on_state),
        ConnectMode::Discovery => run_discovery(&options, &stop, &on_state),
        ConnectMode::Ip(ip) => run_ip(ip, &options, &stop, &on_state),
    }
    on_state(EngineState::Stopped);
}

fn sleep_unless_stopped(stop: &AtomicBool, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
    !stop.load(Ordering::Relaxed)
}

enum Attempt {
    /// 没有可用的 adb 设备（没插线 / 没授权）
    NoDevice,
    /// 跑完了一次会话
    Ran,
    /// 尝试失败，稍后重试
    Retry,
}

struct AdbCycle {
    fresh_sent_for: Option<String>,
    last_ensure: Instant,
    last_notice: Option<String>,
}

impl AdbCycle {
    fn new() -> Self {
        AdbCycle {
            fresh_sent_for: None,
            last_ensure: Instant::now(),
            last_notice: None,
        }
    }

    fn attempt(
        &mut self,
        options: &EngineOptions,
        adb: &Adb,
        stop: &Arc<AtomicBool>,
        on_state: &StateSink,
    ) -> Attempt {
        let devices = match adb.list_devices() {
            Ok(devices) => devices,
            Err(e) => {
                log::warn(&format!("adb devices: {}", e));
                return Attempt::Retry;
            }
        };
        let device = match adb::pick_device(&devices, options.device.as_deref()) {
            Ok(device) => device.clone(),
            Err(reason) => {
                self.fresh_sent_for = None;
                if self.last_notice.as_deref() != Some(reason.as_str()) {
                    let message = format!("{}（{}）", tr("等待手机连接", "waiting for a phone"), reason);
                    log::info(&message);
                    on_state(EngineState::Waiting(message));
                    self.last_notice = Some(reason);
                }
                return Attempt::NoDevice;
            }
        };
        self.last_notice = None;

        if !adb.forward(&device.serial, options.port).unwrap_or(false) {
            let message = format!(
                "{} {}",
                tr("adb 端口转发失败", "adb port forward failed:"),
                device.serial
            );
            log::warn(&message);
            on_state(EngineState::Retrying(message));
            return Attempt::Retry;
        }

        let new_device = self.fresh_sent_for.as_deref() != Some(device.serial.as_str());
        if new_device {
            log::info(&format!("{} {}", tr("检测到手机", "detected phone"), device.serial));
            on_state(EngineState::Waiting(format!(
                "{} {}",
                tr("检测到手机", "detected phone"),
                device.serial
            )));
            match adb.start_service(&device.serial, &options.component, true) {
                Ok((true, _)) => {}
                Ok((false, text)) => log::warn(&format!(
                    "{} {} {}",
                    tr("无法启动手机端接收服务：", "cannot start the phone service:"),
                    options.component,
                    text.lines().next().unwrap_or("")
                )),
                Err(e) => log::warn(&format!("am start-foreground-service: {}", e)),
            }
            self.fresh_sent_for = Some(device.serial.clone());
            self.last_ensure = Instant::now();
            if !sleep_unless_stopped(stop, Duration::from_millis(500)) {
                return Attempt::Retry;
            }
        } else if self.last_ensure.elapsed() >= Duration::from_secs(10) {
            let _ = adb.start_service(&device.serial, &options.component, false);
            self.last_ensure = Instant::now();
        }

        match Session::connect(IpAddr::from([127, 0, 0, 1]), options.port, &home_dir(options), 0) {
            Ok(mut session) => {
                report_connected(&session, on_state);
                run_session(&mut session, options, stop);
                return Attempt::Ran;
            }
            Err(e) => {
                log::debug(&format!("connect via adb: {}", e));
                return Attempt::Retry;
            }
        }
    }
}

fn run_auto(options: &EngineOptions, stop: &Arc<AtomicBool>, on_state: &StateSink) {
    let discovery = Discovery::start(DISCOVERY_PORT);
    let adb = Adb::detect(&options.adb_search_dir, options.adb_path.as_deref());
    if let Some(adb) = &adb {
        log::debug(&format!("adb: {}", adb.path.display()));
        if let Err(e) = adb.start_server() {
            log::warn(&format!("adb start-server: {}", e));
        }
    } else {
        log::info(tr(
            "没有找到 adb，改用局域网发现",
            "adb not found, using LAN discovery",
        ));
    }
    let mut cycle = AdbCycle::new();

    while !stop.load(Ordering::Relaxed) {
        if let Some(adb) = &adb {
            match cycle.attempt(options, adb, stop, on_state) {
                Attempt::Ran => {
                    if !sleep_unless_stopped(stop, Duration::from_millis(500)) {
                        break;
                    }
                    continue;
                }
                Attempt::Retry => {
                    if !sleep_unless_stopped(stop, Duration::from_millis(1000)) {
                        break;
                    }
                    // adb 不通时也给局域网发现一次机会
                }
                Attempt::NoDevice => {}
            }
        }

        if try_discovery(&discovery, options, stop, on_state) {
            continue;
        }
        if !sleep_unless_stopped(stop, Duration::from_millis(800)) {
            break;
        }
    }
}

fn run_discovery(options: &EngineOptions, stop: &Arc<AtomicBool>, on_state: &StateSink) {
    log::info(tr(
        "只使用局域网发现（UDP 广播）",
        "using LAN discovery only (UDP broadcast)",
    ));
    let discovery = Discovery::start(DISCOVERY_PORT);
    while !stop.load(Ordering::Relaxed) {
        if try_discovery(&discovery, options, stop, on_state) {
            continue;
        }
        if !sleep_unless_stopped(stop, Duration::from_millis(800)) {
            break;
        }
    }
}

/// 用发现的手机连一次，返回 true 表示跑过一轮会话。
fn try_discovery(
    discovery: &Discovery,
    options: &EngineOptions,
    stop: &Arc<AtomicBool>,
    on_state: &StateSink,
) -> bool {
    let Some(peer) = discovery.best() else {
        return false;
    };
    let message = format!(
        "{} {}:{}（{}）",
        tr("局域网发现手机", "found phone on LAN"),
        peer.ip,
        peer.port,
        peer.name
    );
    log::debug(&message);
    on_state(EngineState::Waiting(message));
    match Session::connect(peer.ip, peer.port, &home_dir(options), 0) {
        Ok(mut session) => {
            report_connected(&session, on_state);
            run_session(&mut session, options, stop);
            true
        }
        Err(e) => {
            log::debug(&format!("connect to discovered peer: {}", e));
            sleep_unless_stopped(stop, Duration::from_millis(1000));
            false
        }
    }
}

fn run_ip(ip: IpAddr, options: &EngineOptions, stop: &Arc<AtomicBool>, on_state: &StateSink) {
    log::info(&format!(
        "{} {}:{}",
        tr("直接连接", "connecting to"),
        ip,
        options.port
    ));
    on_state(EngineState::Waiting(format!("{} {}:{}", tr("连接", "connecting"), ip, options.port)));
    let mut last_warn = Instant::now() - Duration::from_secs(60);
    while !stop.load(Ordering::Relaxed) {
        match Session::connect(ip, options.port, &home_dir(options), 0) {
            Ok(mut session) => {
                report_connected(&session, on_state);
                if matches!(run_session(&mut session, options, stop), SessionEnd::Stopped) {
                    break;
                }
                if !sleep_unless_stopped(stop, Duration::from_millis(500)) {
                    break;
                }
            }
            Err(e) => {
                if last_warn.elapsed() >= Duration::from_secs(10) {
                    let message = format!(
                        "{} {}: {}",
                        tr("连接失败，稍后重试", "connection failed, retrying"),
                        ip,
                        e
                    );
                    log::warn(&message);
                    on_state(EngineState::Retrying(message));
                    last_warn = Instant::now();
                }
                if !sleep_unless_stopped(stop, Duration::from_secs(3)) {
                    break;
                }
            }
        }
    }
}

fn report_connected(session: &Session, on_state: &StateSink) {
    let names: Vec<String> = session.interfaces.iter().map(|i| i.name.clone()).collect();
    log::info(&format!(
        "{} {}",
        tr("已连接，通道：", "connected, channels:"),
        names.join(", ")
    ));
    on_state(EngineState::Connected(names));
}

fn run_session(session: &mut Session, options: &EngineOptions, stop: &Arc<AtomicBool>) -> SessionEnd {
    let receiver = Arc::new(Receiver::with_shared(crate::webserve::current_dir()));
    match session.run(&receiver, stop) {
        SessionEnd::Shutdown => {
            log::info(tr(
                "手机端已停止服务，等待重新连接…",
                "the phone stopped the server, waiting for a new connection…",
            ));
            SessionEnd::Shutdown
        }
        SessionEnd::Disconnected(reason) => {
            log::warn(&format!("{} {}", tr("连接已断开：", "disconnected:"), reason));
            SessionEnd::Disconnected(reason)
        }
        SessionEnd::Stopped => SessionEnd::Stopped,
    }
}

pub fn home_dir(options: &EngineOptions) -> String {
    util::normalize_dir(&options.receive_dir)
        .to_string_lossy()
        .to_string()
}

/// 从配置构造运行参数。
pub fn options_from_config(
    config: &crate::config::Config,
    mode: ConnectMode,
    adb_search_dir: &Path,
) -> EngineOptions {
    EngineOptions {
        mode,
        port: config.port,
        receive_dir: config.receive_dir.clone(),
        device: config.device.clone(),
        component: config.service_component.clone(),
        adb_path: config.adb_path.clone(),
        adb_search_dir: adb_search_dir.to_path_buf(),
    }
}