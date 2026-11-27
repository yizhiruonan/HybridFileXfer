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
    // 这一轮里已经起过会话线程的设备：再起就只是重连，不再打 info（见下）
    let mut spawned: std::collections::HashSet<String> = std::collections::HashSet::new();

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
                    if spawned.contains(&serial) {
                        // 线程掉了又起来（设备在 adb 列表里闪、上一次 pick 失败等）：
                        // 原来每次都是 info，真机上见过 20 秒里刷 21 行，日志全被它占了。
                        log::debug(&format!(
                            "{} {}",
                            tr("重新连接设备", "reconnecting device"),
                            serial
                        ));
                    } else {
                        spawned.insert(serial.clone());
                        log::info(&format!("{} {}", tr("开始连接设备", "connecting device"), serial));
                    }
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
    /// 手机端接收服务连续启动失败的次数（一旦连上就清零）。
    /// 攒到三次还不行就不干等了 —— 退到「直接把 App 拉起来」，见 ensure_service。
    start_fails: u32,
    /// 上一次在手机上拉起 App 的时间。有冷却，别每轮（2~3 秒）都在用户手机上弹一次窗口。
    last_app_launch: Option<Instant>,
    /// 拉起 App 也失败了几次。到 3 次就不再弹了，改成在界面上告诉用户自己去点一下。
    app_launch_fails: u32,
    /// 「请在手机上打开 App」这句提示已经发给界面了吗（避免每 10 秒重刷一遍）
    app_hint_shown: bool,
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
            start_fails: 0,
            last_app_launch: None,
            app_launch_fails: 0,
            app_hint_shown: false,
        }
    }

    /// 把手机端接收服务拉起来。拉不起来就退一步把 App 自己拉起来。
    ///
    /// 真机上遇到过：`am start-foreground-service` 回
    /// `Error: Not found; no service started.`（App 被系统收掉之后），之后每 2~3 秒
    /// 重试一次全是同一个失败，界面永远停在「等待手机连接」。用户手动点一下图标就正常，
    /// 所以这里替用户点那一下：第二次失败开始尝试 `am start`，15 秒冷却，最多 3 次。
    ///
    /// 返回 true 表示这次 am 明确说服务起来了。
    fn ensure_service(&mut self, adb: &Adb, serial: &str, component: &str, fresh: bool) -> bool {
        let (ok, text) = adb
            .start_service(serial, component, fresh)
            .unwrap_or_else(|e| (false, e.to_string()));
        if ok {
            self.start_fails = 0;
            self.app_launch_fails = 0;
            return true;
        }
        self.start_fails += 1;
        if self.start_fails == 1 {
            log::warn(&format!(
                "{} {} {}",
                tr("无法启动手机端接收服务：", "cannot start the phone service:"),
                component,
                text.lines().next().unwrap_or("")
            ));
        }
        let cooled = self
            .last_app_launch
            .map(|t| t.elapsed() >= Duration::from_secs(15))
            .unwrap_or(true);
        // 连续三次（≈6~9 秒）都拉不起来才动手：这条 am 偶发失败一两次、下一轮自己就好了，
        // 没必要为了一次抖动就在用户手机上弹窗口。
        if self.start_fails >= 3 && cooled && self.app_launch_fails < 3 {
            let package = component.split('/').next().unwrap_or(component);
            match adb.launch_app(serial, package) {
                Ok((true, _)) => {
                    log::info(tr(
                        "手机端接收服务起不来，改为在手机上打开 App",
                        "cannot start the phone service, opening the app on the phone instead",
                    ));
                    self.start_fails = 0;
                }
                other => {
                    let reason = match other {
                        Ok((_, text)) => text.lines().next().unwrap_or("").to_string(),
                        Err(e) => e.to_string(),
                    };
                    self.app_launch_fails += 1;
                    log::warn(&format!(
                        "{} {}",
                        tr("在手机上打开 App 也失败了：", "cannot open the app on the phone:"),
                        reason
                    ));
                }
            }
            self.last_app_launch = Some(Instant::now());
        }
        false
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
            self.app_hint_shown = false;
            log::info(&format!("{} {}", tr("检测到手机", "detected phone"), device.serial));
            on_state(EngineState::Waiting(format!(
                "{} {}",
                tr("检测到手机", "detected phone"),
                device.serial
            )));
            self.last_start_ok = self.ensure_service(adb, &device.serial, &options.component, true);
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
                self.last_start_ok =
                    self.ensure_service(adb, &device.serial, &options.component, false);
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
                    // 通道数必须跟着真连上的那几条走（见 attach_channel_sink）
                    attach_channel_sink(&mut session, on_state);
                    self.connected_once = true;
                    self.stop_notice_shown = false;
                    self.start_fails = 0;
                    self.app_launch_fails = 0;
                    self.app_hint_shown = false;
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
        // 一直连不上、手机上 App 也打不开：把真实原因说到界面上去。
        // 原来这里只有每 10 秒一行的日志，用户看到的是「检测到手机 … 等待中」一动不动，
        // 内容还是「adb 连接没成功」，既看不懂也不知道该做什么。
        if !self.connected_once && self.app_launch_fails >= 1 && !self.app_hint_shown {
            self.app_hint_shown = true;
            // 不写串号：界面上副标题是「手机名：<这句>」，一行放不下就被截成
            // 「请在手机…」，正好把该做的事截掉。手机名本来就已经显示了。
            let message = tr(
                "手机端没在运行，请在手机上打开「多轨快传」",
                "the phone app is not running, open it on the phone",
            )
            .to_string();
            log::info(&message);
            on_state(EngineState::Waiting(message));
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
            attach_channel_sink(&mut session, on_state);
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
                attach_channel_sink(&mut session, on_state);
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

/// 把「通道集合变了」接到界面上。
///
/// 为什么必须有这条回调：界面上那个「N 条通道」是用户判断「多轨有没有生效」的唯一依据。
/// 手机切换网卡（svc wifi disable/enable、开关蓝牙、USB 网络共享）之后，原来那几条
/// TCP 已经死了，只有当场把通道数改小，用户才不会看到「6 条通道、速度只有 2 条的
/// 水平」。
fn attach_channel_sink(session: &mut Session, on_state: &StateSink) {
    let on_state = Arc::clone(on_state);
    session.set_channels_sink(Arc::new(move |names: &[String]| {
        let names: Vec<String> = names.to_vec();
        if names.is_empty() {
            return;
        }
        log::warn(&format!(
            "{} {}",
            tr("通道数已更新：", "channels updated:"),
            names.join(", ")
        ));
        on_state(EngineState::Connected(names));
    }));
}

/// 连上了：报**真连上的通道**，不是手机报的网卡计划。
///
/// session.interfaces 是手机在握手时报的「我有哪些网卡」，电脑连不上的那些（不在同一
/// 网段、被防火墙挡掉、网卡刚刚已经死了）会走 skip 分支，压根没有 socket。以前这里
/// 取的是 interfaces，于是日志和界面都是 6 条，实际只有 2 条在跑 —— 用户报的
/// 「显示 6 条通道、速度只有 2 条的水平」就是这么来的。
fn report_connected(session: &Session, on_state: &StateSink) {
    let names = session.channel_names();
    let planned: Vec<String> = session.interfaces.iter().map(|i| i.name.clone()).collect();
    if planned.len() != names.len() {
        // 差额必须写进日志：真机上「网卡切换完只连上 2 条」全靠这一行才能一眼看出来。
        let missing: Vec<String> = planned
            .iter()
            .filter(|name| !names.contains(name))
            .cloned()
            .collect();
        log::warn(&format!(
            "{} {} {} {} {}{}",
            tr("手机报了", "the phone offered"),
            planned.len(),
            tr("张网卡，只连上", "interfaces but only"),
            names.len(),
            tr("条通道；没连上的：", "channel(s); not connected:"),
            missing.join(", ")
        ));
    }
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
    // 进度按设备分开记：主界面每台设备那一行读的是 progress_for(串号)（gui_slint.rs 设备行），
    // 以前这里一律用 with_shared —— 它的键是固定的 "default"，
    // 于是 progress_for(真串号) 那份从来没人写，设备行永远显示「已接收 0 B · 0 个文件」。
    // 收块时本来就同时写「本设备」和「全局」两份（见 receiver.rs write_block 的进度上报），
    // 所以这里带上串号只是把「本设备」那份接到正确的键上，全局那份和界面大卡片不受影响。
    let receiver = {
        let dir = crate::webserve::current_dir();
        match options.device.as_deref() {
            Some(dev) if !dev.is_empty() => Arc::new(Receiver::with_device(dir, dev)),
            _ => Arc::new(Receiver::with_shared(dir)),
        }
    };
    let end = session.run(&receiver, stop);
    // 309：这台手机的队列快照跟着会话一起作废 —— 断线之后界面不该再挂着一排
    // 「等待中」的块（那些文件是上一次连接时排的，手机那边可能早换了队列）。
    // 键就是会话自己的串号，和 QUEUE_STATE 入库时用的是同一个。
    crate::queue::clear(&session.device);
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