//! 连接与接收的主循环，供 GUI / 命令行共用。
//!
//! 自适应连接顺序：
//! 1. 有 adb 设备（数据线）→ adb 端口转发 + 拉起手机服务，控制通道走 127.0.0.1；
//!    手机端会把所有网卡都报上来，电脑能连上的通道（USB + WIFI）会同时使用；
//! 2. 没有 adb 设备 → 用局域网广播发现手机，直接连它的 IP（只要同一 WIFI）；
//! 3. 也可以手动指定 IP。

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// 本机 adb 转发用的端口（多设备时每台不同，避免互相覆盖）
    pub local_port: u16,
    /// 已绑定具体设备（多设备会话）：不要再走"谁最近就联谁"的局域网发现回退，
    /// 否则两条会话可能抢同一台手机，互相把对方踢掉。
    pub pinned: bool,
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
                    // 每台手机一个独立的本机转发端口（从 5750 起），否则互相覆盖
                    per_device.local_port = 5750 + running.len() as u16;
                    per_device.pinned = true;
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

        // 800ms 扫一次 adb：拔插后能更快发现设备（原来 2 秒，插上要等一下才连）
        if !sleep_unless_stopped(&stop, Duration::from_millis(800)) {
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
    /// 绑定过的设备已经从 adb 列表里消失（拔线/换机）→ 结束这台设备的会话
    Gone,
}

struct AdbCycle {
    fresh_sent_for: Option<String>,
    last_ensure: Instant,
    last_notice: Option<String>,
    /// 这台设备之前成功出现过；之后再从 adb 列表里消失就结束会话
    seen: bool,
    /// 上一次启动手机端服务是否成功：失败了就 2 秒重试一次，别等 10 秒
    last_start_ok: bool,
    /// 上一次打印「adb 连接没成功」的时间，避免每秒刷一行
    last_fail_log: Instant,
    /// 这一轮「开始接收」里已经连上过手机了。连上过之后再断开（用户在手机上点「停止服务」
    /// 是最常见的情况）就不再自动拉起手机端服务 —— 否则手机上刚变「启动服务并等待连接」，
    /// 两三秒后又被电脑拉回来，用户看到的是「点了停止却停不下来」。
    /// 只在「还没连上过」时自动拉起（刚点开始接收 / 刚插上线，方便用户不用去碰手机）。
    connected_once: bool,
    /// 「手机端已停止服务」这条提示已经发给界面了吗（避免每个轮次重复刷）
    stop_notice_shown: bool,
}

impl AdbCycle {
    fn new() -> Self {
        AdbCycle {
            fresh_sent_for: None,
            last_ensure: Instant::now(),
            last_notice: None,
            seen: false,
            last_start_ok: true,
            last_fail_log: Instant::now() - Duration::from_secs(60),
            connected_once: false,
            stop_notice_shown: false,
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
                // 不要清空 fresh_sent_for：设备短暂不可见时若清空，
                // 下次连上又会带 fresh=true 去重启手机端服务，
                // 两台设备同时在线时会互相把对方的服务顶掉，形成连接抖动。
                if self.last_notice.as_deref() != Some(reason.as_str()) {
                    let message = format!("{}（{}）", tr("等待手机连接", "waiting for a phone"), reason);
                    log::info(&message);
                    on_state(EngineState::Waiting(message));
                    self.last_notice = Some(reason);
                }
                // 绑定过的设备彻底从 adb 消失（拔线/换机）：结束这台设备的线程，
                // 让电脑端把它从设备列表里移除，而不是一直挂着等它回来
                if options.pinned && self.seen {
                    log::info(tr(
                        "设备已断开，移除这台设备的会话",
                        "device disappeared, removing its session",
                    ));
                    return Attempt::Gone;
                }
                return Attempt::NoDevice;
            }
        };
        self.last_notice = None;
        self.seen = true;

        if !adb.forward(&device.serial, options.local_port, options.port).unwrap_or(false) {
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
            let started = match adb.start_service(&device.serial, &options.component, true) {
                Ok((true, _)) => true,
                Ok((false, text)) => {
                    log::warn(&format!(
                        "{} {} {}",
                        tr("无法启动手机端接收服务：", "cannot start the phone service:"),
                        options.component,
                        text.lines().next().unwrap_or("")
                    ));
                    false
                }
                Err(e) => {
                    log::warn(&format!("am start-foreground-service: {}", e));
                    false
                }
            };
            self.last_start_ok = started;
            self.fresh_sent_for = Some(device.serial.clone());
            self.last_ensure = Instant::now();
            // 手机端从「服务被拉起」到「开始监听」还有一两百毫秒，刚插上线时更长。
            // 以前先睡 500ms，失败一次就退到局域网发现（那要等广播），用户看到的就是
            // 「插上好几秒才连上」。下面连 3 秒内每 250ms 重试一次 adb 通道。
            if !sleep_unless_stopped(stop, Duration::from_millis(200)) {
                return Attempt::Retry;
            }
        } else if self.connected_once {
            // 连上过又断了：不自动拉起手机端服务（用户点「停止服务」是明确意图），
            // 只被动重连 —— 手机上一点「启动服务」，下面这个循环一秒内就接上。
            if !self.stop_notice_shown {
                self.stop_notice_shown = true;
                on_state(EngineState::Waiting(tr(
                    "手机已断开，在手机上点「启动服务」后会自动连上",
                    "the phone disconnected; tap Start on the phone to reconnect",
                ).to_string()));
            }
        } else {
            // 这一轮还没连上过（刚点「开始接收」/ 刚插上线）：定期把手机端服务拉起来，
            // 用户不用去碰手机。间隔 3 秒（以前成功拉过一次之后就隔 10 秒，插上要等十几秒）。
            // 服务本来就活着时 am start-foreground-service 是个很快的空操作。
            // 注意：连上过、后来断开的情况走上面那一支，不在这里拉。
            let interval = if self.last_start_ok {
                Duration::from_secs(3)
            } else {
                Duration::from_secs(2)
            };
            if self.last_ensure.elapsed() >= interval {
                self.last_start_ok = adb
                    .start_service(&device.serial, &options.component, false)
                    .map(|(ok, _)| ok)
                    .unwrap_or(false);
                self.last_ensure = Instant::now();
            }
        }

        // 插上就连接：3 秒内每 250ms 重试一次，别一次失败就退到局域网发现
        let deadline = Instant::now() + Duration::from_millis(3000);
        let mut last_error: Option<String> = None;
        loop {
            match Session::connect_with_local_port(
                IpAddr::from([127, 0, 0, 1]),
                options.port,
                options.local_port,
                &home_dir(options),
                0,
            ) {
                Ok(mut session) => {
                    session.device = device.serial.clone();
                    self.connected_once = true;
                    self.stop_notice_shown = false;
                    report_connected(&session, on_state);
                    let end = run_session(&mut session, options, stop);
                    // 断开后立刻把界面切成「等手机点启动服务」：不能一直显示「已连接」，
                    // 那正是用户报的「手机点停止、电脑端毫无变化」。
                    let notice = match end {
                        SessionEnd::Shutdown => Some(tr(
                            "手机端已停止服务，在手机上点「启动服务」后会自动连上",
                            "the phone stopped the server; tap Start on the phone to reconnect",
                        )),
                        SessionEnd::Disconnected(_) => Some(tr(
                            "手机已断开，在手机上点「启动服务」后会自动连上",
                            "the phone disconnected; tap Start on the phone to reconnect",
                        )),
                        SessionEnd::Stopped => None,
                    };
                    if let Some(notice) = notice {
                        self.stop_notice_shown = true;
                        on_state(EngineState::Waiting(notice.to_string()));
                    }
                    return Attempt::Ran;
                }
                Err(e) => {
                    last_error = Some(e.to_string());
                    if Instant::now() >= deadline
                        || !sleep_unless_stopped(stop, Duration::from_millis(250))
                    {
                        break;
                    }
                }
            }
        }
        if self.last_fail_log.elapsed() >= Duration::from_secs(10) {
            log::info(&format!(
                "adb 连接没成功（稍后重试）：{}",
                last_error.unwrap_or_else(|| "timeout".to_string())
            ));
            self.last_fail_log = Instant::now();
        }
        Attempt::Retry
    }
}

fn run_auto(options: &EngineOptions, stop: &Arc<AtomicBool>, on_state: &StateSink) {
    let discovery = Discovery::shared(DISCOVERY_PORT);
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
                Attempt::Gone => {
                    on_state(EngineState::Stopped);
                    break;
                }
            }
        }

        if !options.pinned && try_discovery(&discovery, options, stop, on_state) {
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
    let discovery = Discovery::shared(DISCOVERY_PORT);
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
            let end = run_session(&mut session, options, stop);
            if matches!(end, SessionEnd::Shutdown) {
                on_state(EngineState::Waiting(tr(
                    "手机端已停止服务，在手机上点「启动服务」后会自动连上",
                    "the phone stopped the server; tap Start on the phone to reconnect",
                ).to_string()));
            }
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

/// 正在进行中的会话线程数。
///
/// PROGRESS 是全局单例（界面上只有一份进度），所以「会话结束要不要把进度清掉」
/// 不能只看自己这一条：多设备（run_multi）时任何一台断开都会走到 run_session
/// 的收尾，无条件 reset 会把**另一台正在传**的进度抹掉 —— 于是明细表整张退回
/// 「通道 1 / 通道 2 …」，进度条也归零。只在最后一个会话退出时才清。
static ACTIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);

fn run_session(session: &mut Session, options: &EngineOptions, stop: &Arc<AtomicBool>) -> SessionEnd {
    ACTIVE_SESSIONS.fetch_add(1, Ordering::SeqCst);
    let receiver = Arc::new(Receiver::with_shared(crate::webserve::current_dir()));
    let end = session.run(&receiver, stop);
    // 会话结束（断开/被停止/出错）：把还开着的文件句柄放掉、续传日志落盘，
    // 界面进度也复位，否则卡片上会一直挂着一个停住不动的假进度。
    //
    // 这里要传 true（保留）：掉线/被杀不是用户主动取消，半截文件留着，
    // 下次同一个文件还能续传。真正"删掉"只有两条路，都走 handle_receive 里的
    // finish_with(keep_incomplete = false)：手机端取消时选了「不保留进度」
    // （CANCEL_DISCARD），或电脑端在「当前文件」卡片上点「取消」再选「不保留」。
    if let Ok((kept, _lost)) = receiver.finish_with(true) {
        if kept > 0 {
            log::warn(&format!(
                "{} {}",
                tr(
                    "保留未接收完成的文件（可续传）：",
                    "kept incomplete files (resumable):"
                ),
                kept
            ));
        }
    }
    // 最后一个会话退出才清（见 ACTIVE_SESSIONS 的注释）。
    if ACTIVE_SESSIONS.fetch_sub(1, Ordering::SeqCst) == 1 {
        crate::receiver::PROGRESS.reset();
    }
    crate::session::reset_send_segments();
    match end {
        SessionEnd::Shutdown => {
            log::info(tr(
                "手机端已停止服务，等手机上重新点「启动服务」（不再自动拉起）",
                "the phone stopped the server; waiting for the phone to start it again",
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
        local_port: config.port,
        pinned: false,
    }
}