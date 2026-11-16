//! 图形界面版接收器（Slint + fluent 样式，Windows 11 观感）。

// 一律使用 Windows 子系统：GUI 版不要控制台窗口（日志写 receiver.log）
#![windows_subsystem = "windows"]

slint::include_modules!();

use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use hfx_receiver::config::{self, Config};
use hfx_receiver::engine::{self, ConnectMode, EngineOptions, EngineState};
use hfx_receiver::i18n::{self, Lang};
use hfx_receiver::log;
use slint::ComponentHandle;

#[cfg(windows)]
mod tray_win;

/// 窗口标题（托盘线程靠它找到主窗口）
const WINDOW_TITLE: &str = "多轨快传";

struct EngineHandle {
    stop: Arc<AtomicBool>,
    _join: Option<JoinHandle<()>>,
}

struct State {
    config: Config,
    config_path: PathBuf,
    exe_dir: PathBuf,
    engine: Option<EngineHandle>,
    state: EngineState,
    state_rx: Receiver<(String, EngineState)>,
    state_tx: Sender<(String, EngineState)>,
    devices: std::collections::HashMap<String, EngineState>,
    device_order: Vec<String>,
    expanded: std::collections::HashSet<String>,
    expanded_dirty: bool,
    asked_target: bool,
    /// 取消接收后要贴到状态卡下面的那句话，等这次接收真正收完尾（RECEIVING 落下去）
    /// 再显示出来 —— 「已删除」不能比实际删文件早一句话。
    cancel_note: Option<String>,
}

impl State {
    fn running(&self) -> bool {
        self.engine.is_some()
    }

    fn stop_engine(&mut self) {
        if let Some(handle) = self.engine.take() {
            handle.stop.store(true, Ordering::Relaxed);
            log::info(i18n::tr("正在停止接收…", "stopping…"));
        }
        // 停就是停：设备表、排队中的状态一起清掉。
        // 以前只把 engine 摘掉，设备表里还留着「已连接」那条，而界面上的「是否在接收」
        // 又是按设备数算的 —— 点一次停止，界面自己又跳回「已连接/停止接收」，
        // 用户得再点一次才真的停（线程这时才收完尾）。
        self.devices.clear();
        self.device_order.clear();
        self.asked_target = false;
        self.state = EngineState::Stopped;
        self.expanded_dirty = true;
        while self.state_rx.try_recv().is_ok() {}
    }

    fn start_engine(&mut self, port: u16, mode: ConnectMode, dir: PathBuf) {
        if self.engine.is_some() {
            return;
        }
        self.config.receive_dir = dir.clone();
        self.config.port = port;
        let _ = self.config.save(&self.config_path);

        let options = EngineOptions {
            mode,
            port,
            receive_dir: dir,
            device: self.config.device.clone(),
            component: self.config.service_component.clone(),
            adb_path: self.config.adb_path.clone(),
            adb_search_dir: self.exe_dir.clone(),
            local_port: port,
            pinned: false,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let tx = Arc::new(Mutex::new(self.state_tx.clone()));
        let sink: engine::MultiStateSink = Arc::new(move |device: &str, state| {
            if let Ok(sender) = tx.lock() {
                let _ = sender.send((device.to_string(), state));
            }
        });
        let max_devices = self.config.max_devices;
        // engine::run_multi 是阻塞的，必须自己开线程
        let join = std::thread::spawn(move || {
            engine::run_multi(options, stop_for_thread, max_devices, sink);
        });
        self.engine = Some(EngineHandle {
            stop,
            _join: Some(join),
        });
        self.state = EngineState::Waiting("正在启动…".to_string());
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let config_path = config::config_path(&exe_dir);
    let config = Config::load(&config_path);
    let lang = config.lang.unwrap_or_else(detect_lang);
    i18n::set_lang(lang);

    // -v / --verbose：日志里多出调试行（比如每条通道"绑定本机哪个地址"），
    // 排查"梯子/虚拟网卡抢路由"这类问题时用得上
    let verbose = std::env::args().any(|a| a == "-v" || a == "--verbose");
    log::init(Some(exe_dir.join("receiver.log")), verbose);
    log::set_console(false);
    log::info(&format!(
        "GUI build={} pid={} exe={}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    ));

    let (state_tx, state_rx) = mpsc::channel();

    // 清理上一次自我更新留下的旧文件
    let _ = std::fs::remove_file(exe_dir.join("HybridFileXfer-Receiver.exe.old"));

    remove_legacy_autostart_script();
    // Explorer「发送到 → 手机」：把选中文件投递给已运行的实例，投递成功就直接退出
    // 只要命令行里带了已存在的文件路径，就当作「发送到手机」。
    // 这样用户自己把快捷方式拖进 SendTo 即可，程序不需要拉 PowerShell 去造 .lnk
    //（无签名程序 + 拉起脚本 + 写快捷方式，很容易被杀软判成木马）。
    let mut send_files: Vec<PathBuf> = Vec::new();
    let mut send_target: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--send" || arg == "--autostart" || arg == "-v" || arg == "--verbose" {
            continue;
        }
        // --to <串号>：只把文件推给指定设备（多设备时用）
        if arg == "--to" {
            send_target = args.next();
            continue;
        }
        let path = PathBuf::from(&arg);
        if path.exists() {
            send_files.push(path);
        }
    }
    let send_mode = !send_files.is_empty();
    if send_mode {
        #[cfg(windows)]
        if tray_win::deliver_paths_with_target(&send_files, send_target.as_deref()) {
            return Ok(());
        }
        // 没有运行中的实例：这些文件先入队，本进程接着正常启动
        hfx_receiver::pending::push_with_target(send_files.clone(), send_target.clone());
    }

    // 单实例：已经有接收器在跑时，不再开第二个窗口（除非是"发送到手机"的投递进程）
    #[cfg(windows)]
    if tray_win::tray_window_handle().is_some() {
        if send_mode {
            // 走到这里说明投递失败（窗口还没建好），直接退出，别再开一个实例
            log::warn("已有实例在运行，投递未完成");
        } else {
            // 把已运行的窗口叫到前面，然后退出
            tray_win::show_main_window(WINDOW_TITLE);
        }
        return Ok(());
    }

    let autostart = autostart_enabled();
    let ip_text = std::env::var("HFX_LAST_IP").unwrap_or_else(|_| "192.168.1.100".to_string());

    let state = Arc::new(Mutex::new(State {
        config,
        config_path: config_path.clone(),
        exe_dir: exe_dir.clone(),
        engine: None,
        state: EngineState::Stopped,
        devices: std::collections::HashMap::new(),
        device_order: Vec::new(),
        expanded: std::collections::HashSet::new(),
        expanded_dirty: false,
        asked_target: false,
        cancel_note: None,
        state_rx,
        state_tx,
    }));

    let ui = AppWindow::new()?;
    // 独立传输窗口：只读同一份 PROGRESS，不参与主界面布局。
    // 传输一开始自己弹出来，结束自己收起 —— 主窗口最小化到托盘时也能看到进度。
    let xfer_win = TransferWindow::new()?;
    // 用户在传输窗口上点了「收起」：这一轮就别再弹了（下一轮传输重新允许）
    let xfer_dismissed = Arc::new(AtomicBool::new(false));
    ui.set_dir_text(state.lock().unwrap().config.receive_dir.display().to_string().into());
    ui.set_port_text(state.lock().unwrap().config.port.to_string().into());
    ui.set_ip_text(ip_text.into());
    ui.set_autostart(autostart);
    ui.set_tray(state.lock().unwrap().config.minimize_to_tray);
    ui.set_sendto(state.lock().unwrap().config.sendto);
    ui.set_status_kind(0);
    // 之前勾选过就确保快捷方式还在（换电脑/程序挪位置后路径会变）
    if state.lock().unwrap().config.sendto {
        std::thread::spawn(|| {
            if let Err(e) = hfx_receiver::sendto::ensure() {
                log::warn(&format!("创建「发送到手机」快捷方式失败: {e}"));
            }
        });
    }

    // ---------- 回调 ----------
    {
        let ui_weak = ui.as_weak();
        ui.on_browse_clicked(move || {
            if let Some(dir) = pick_folder() {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_dir_text(dir.to_string_lossy().to_string().into());
                }
            }
        });
    }
    {
        let st = Arc::clone(&state);
        let ui_weak = ui.as_weak();
        ui.on_start_clicked(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut s = st.lock().unwrap();
            if s.running() {
                return;
            }
            let port: u16 = match ui.get_port_text().trim().parse() {
                Ok(p) => p,
                Err(_) => {
                    ui.set_message("端口号不正确".into());
                    return;
                }
            };
            let mode = if ui.get_mode_index() == 0 {
                ConnectMode::Auto
            } else {
                match ui.get_ip_text().trim().parse::<IpAddr>() {
                    Ok(ip) => ConnectMode::Ip(ip),
                    Err(_) => {
                        ui.set_message("IP 地址不正确".into());
                        return;
                    }
                }
            };
            let dir = PathBuf::from(ui.get_dir_text().trim());
            if dir.as_os_str().is_empty() {
                ui.set_message("请选择接收文件夹".into());
                return;
            }
            if let Err(e) = std::fs::create_dir_all(&dir) {
                ui.set_message(format!("无法创建接收文件夹：{}", e).into());
                return;
            }
            ui.set_message("".into());
            // 上一次取消留下的提示 / 没关掉的确认框，别带进这一次
            ui.set_notice_text("".into());
            ui.set_cancel_ask(false);
            hfx_receiver::control::pause(false);
            hfx_receiver::receiver::PROGRESS.reset();
            hfx_receiver::session::reset_send_segments();
            log::info("按钮：开始接收");
            s.start_engine(port, mode, dir);
            ui.set_running(true);
        });
    }
    {
        let st = Arc::clone(&state);
        let ui_weak = ui.as_weak();
        ui.on_stop_clicked(move || {
            let mut s = st.lock().unwrap();
            log::info("按钮：停止接收");
            s.stop_engine();
            s.cancel_note = None;
            // 停就是停：弹了一半的取消确认框、为了弹框加的暂停，一起收掉
            hfx_receiver::control::pause(false);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_running(false);
                ui.set_cancel_ask(false);
                ui.set_notice_text("".into());
            }
        });
    }
    {
        let st = Arc::clone(&state);
        ui.on_autostart_toggled(move |enabled| {
            let s = st.lock().unwrap();
            if let Err(e) = set_autostart(enabled, &s.exe_dir) {
                eprintln!("设置开机自启失败: {e}");
            }
        });
    }
    {
        let st = Arc::clone(&state);
        ui.on_device_clicked(move |index| {
            let mut s = st.lock().unwrap();
            let Some(serial) = s.device_order.get(index as usize).cloned() else {
                return;
            };
            if !s.expanded.remove(&serial) {
                s.expanded.insert(serial);
            }
            s.expanded_dirty = true;
        });
    }
    {
        let st = Arc::clone(&state);
        ui.on_device_chosen(move |index| {
            let s = st.lock().unwrap();
            if let Some(serial) = s.device_order.get(index as usize) {
                hfx_receiver::pending::assign_all(serial);
                log::info(&format!("推送目标已选定：{}", serial));
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_toggle_pause_send(move || {
            let paused = !hfx_receiver::control::paused();
            hfx_receiver::control::pause(paused);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_send_paused(paused);
            }
        });
    }
    {
        ui.on_cancel_send(move || {
            hfx_receiver::control::cancel();
        });
    }
    // ---- 独立传输窗口上的两颗按钮（和主窗口同一套控制函数）----
    {
        let weak = xfer_win.as_weak();
        xfer_win.on_toggle_pause(move || {
            let paused = !hfx_receiver::control::paused();
            hfx_receiver::control::pause(paused);
            if let Some(w) = weak.upgrade() {
                w.set_paused(paused);
            }
        });
    }
    // ---- 流光的步进：16ms 一帧 ----
    // 亮带从左边扫到右边大约 1.6 秒一趟（每帧 +0.01）。窗口没显示、暂停、或者已经传完
    // 就直接跳过：进度不动了还在闪，看着像卡住了但其实啥也没发生。
    let flow_timer = slint::Timer::default();
    {
        let weak = xfer_win.as_weak();
        flow_timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), move || {
            let Some(tw) = weak.upgrade() else { return };
            if !tw.window().is_visible() || tw.get_done() || tw.get_paused() {
                return;
            }
            let mut f = tw.get_flow() + 0.010;
            if f >= 1.0 {
                f -= 1.0;
            }
            tw.set_flow(f);
        });
    }
    {
        let weak = xfer_win.as_weak();
        let dismissed = Arc::clone(&xfer_dismissed);
        xfer_win.on_close_window(move || {
            // 收起 = 藏起来，传输继续（要真停下得去主窗口点「取消接收」）
            dismissed.store(true, Ordering::Relaxed);
            if let Some(w) = weak.upgrade() {
                let _ = w.hide();
            }
        });
    }
    // ---- 接收方向的「取消」（不只是暂停 / 继续）----
    // 点「取消」先暂停：手机端在暂停同步里看到电脑暂停就会停发，
    // 别让它在用户看确认框的这几秒里继续往盘里灌数据。
    // 返回 bool：false = 这次点击被忽略（没在接收 / 已经在取消中），
    // Slint 那边据此不弹确认框 —— 同一轮接收只允许取消一次。
    ui.on_cancel_recv(move || -> bool {
        if !hfx_receiver::control::receiving() {
            log::warn(i18n::tr(
                "按钮：取消接收被忽略（当前没有接收在进行）",
                "cancel receiving ignored: nothing is being received",
            ));
            return false;
        }
        if hfx_receiver::control::cancel_requested() {
            // 第一次取消已经在收尾 / 重建里了：这次点击直接复用上一次的结果，
            // 绝不再往控制通道插第二帧（那会和手机端的重建握手抢帧序）。
            log::warn(i18n::tr(
                "按钮：取消接收被忽略（这次取消已经受理过）",
                "cancel receiving ignored: this cancel was already accepted",
            ));
            return false;
        }
        hfx_receiver::control::pause(true);
        log::warn(i18n::tr(
            "按钮：取消接收（已暂停，等用户选择是否保留进度）",
            "button: cancel receiving (paused, waiting for the user's choice)",
        ));
        true
    });
    {
        let st = Arc::clone(&state);
        ui.on_cancel_keep(move || {
            // 保留：和手机端「保留进度」同一个语义 —— 收尾时留着半截文件和 .hfxpart
            if !hfx_receiver::control::cancel_receive(
                hfx_receiver::control::CancelChoice::Keep,
            ) {
                // 幂等：这次取消已经受理过（收尾 / 重建还没做完）时不再做任何事，
                // 上一次的结果原样复用，界面不重复提示、控制通道不插第二帧。
                log::warn(i18n::tr(
                    "按钮：取消接收 · 保留进度被忽略（这次取消已经受理过）",
                    "cancel · keep ignored: this cancel was already accepted",
                ));
                return;
            }
            log::warn(i18n::tr(
                "按钮：取消接收 · 保留进度（半截文件和 .hfxpart 留着，可续传）",
                "button: cancel receiving · keep the partial file (resumable)",
            ));
            if let Ok(mut s) = st.lock() {
                s.cancel_note = Some(
                    i18n::tr(
                        "已取消：保留已收到的部分，下次同一个文件可续传",
                        "cancelled: what already arrived is kept and can be resumed",
                    )
                    .to_string(),
                );
            }
        });
    }
    {
        let st = Arc::clone(&state);
        ui.on_cancel_discard(move || {
            // 不保留：收尾（Receiver::finish_with(false)）会把没传完的文件和续传记录删掉
            if !hfx_receiver::control::cancel_receive(
                hfx_receiver::control::CancelChoice::Discard,
            ) {
                log::warn(i18n::tr(
                    "按钮：取消接收 · 不保留被忽略（这次取消已经受理过）",
                    "cancel · discard ignored: this cancel was already accepted",
                ));
                return;
            }
            log::warn(i18n::tr(
                "按钮：取消接收 · 不保留（收尾删掉没传完的文件和续传记录）",
                "button: cancel receiving · discard the incomplete files",
            ));
            if let Ok(mut s) = st.lock() {
                s.cancel_note = Some(
                    i18n::tr(
                        "已取消：已删除未传完的文件和续传记录",
                        "cancelled: the incomplete files and their resume journals were removed",
                    )
                    .to_string(),
                );
            }
        });
    }
    ui.on_cancel_dismiss(move || {
        // 确认框里选了「继续接收」：把刚才加的暂停放掉，传输照常
        hfx_receiver::control::pause(false);
        log::info(i18n::tr(
            "取消接收确认框：选了「继续接收」",
            "cancel dialog: keep receiving",
        ));
    });
    {
        let st = Arc::clone(&state);
        ui.on_tray_toggled(move |enabled| {
            let mut s = st.lock().unwrap();
            s.config.minimize_to_tray = enabled;
            let path = s.config_path.clone();
            let config = s.config.clone();
            if let Err(e) = config.save(&path) {
                eprintln!("保存托盘设置失败: {e}");
            }
        });
    }
    {
        let st = Arc::clone(&state);
        ui.on_sendto_toggled(move |enabled| {
            let mut s = st.lock().unwrap();
            s.config.sendto = enabled;
            let path = s.config_path.clone();
            let config = s.config.clone();
            drop(s);
            if let Err(e) = config.save(&path) {
                eprintln!("保存「发送到」设置失败: {e}");
            }
            if enabled {
                std::thread::spawn(|| {
                    if let Err(e) = hfx_receiver::sendto::ensure() {
                        log::warn(&format!("创建「发送到手机」快捷方式失败: {e}"));
                    }
                });
            } else {
                hfx_receiver::sendto::remove();
            }
        });
    }
    {
        let st = Arc::clone(&state);
        ui.on_open_dir(move || {
            let s = st.lock().unwrap();
            open_in_explorer(&s.config.receive_dir.display().to_string());
        });
    }

    let launch_at_boot = std::env::args().any(|a| a == "--autostart");

    // ---------- 定时把线程里的状态/日志刷到界面 ----------
    let timer = slint::Timer::default();
    {
        let st = Arc::clone(&state);
        let ui_weak = ui.as_weak();
        // 独立传输窗口：这一轮有没有弹过 / 用户是不是已经把它收起来了
        let xfer_weak = xfer_win.as_weak();
        let xfer_dismissed = Arc::clone(&xfer_dismissed);
        let mut tw_shown = false;
        // 这一轮传完的时刻（用来数「已完成」亮够 3 秒没有）
        let mut tw_finish_at: Option<std::time::Instant> = None;
        let mut last_bytes = 0u64;
        let mut last_tick = std::time::Instant::now();
        let mut speed = 0f64;
        let mut last_xfer_visible = false;
        let mut last_want_h = 0f32;
        let mut boot_hidden = false;

        timer.start(slint::TimerMode::Repeated, Duration::from_millis(150), move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut s = st.lock().unwrap();

            // 「在不在接收」只看引擎：设备数不行 —— 停止过程中设备表里那条还在，
            // 界面就会自己跳回「已连接」，得按两次停止（用户报的那个 bug）
            let engine_running = s.engine.is_some();
            let mut changed = false;
            while let Ok((device, next)) = s.state_rx.try_recv() {
                changed = true;
                // 已经点了停止：引擎线程收尾时还会推几条消息过来，别再让界面跳回「已连接」
                if !engine_running || matches!(next, EngineState::Stopped) {
                    s.devices.remove(&device);
                } else {
                    s.devices.insert(device, next);
                }
            }
            if changed || s.expanded_dirty {
                s.expanded_dirty = false;
                let mut connected: Vec<String> = Vec::new();
                let mut waiting: Option<String> = None;
                let mut retrying: Option<String> = None;
                for (device, state) in &s.devices {
                    match state {
                        EngineState::Connected(names) => connected.extend(names.iter().cloned()),
                        EngineState::Waiting(reason) => {
                            if waiting.is_none() {
                                waiting = Some(format!("{device}：{reason}"));
                            }
                        }
                        EngineState::Retrying(reason) => {
                            if retrying.is_none() {
                                retrying = Some(format!("{device}：{reason}"));
                            }
                        }
                        EngineState::Stopped => {}
                    }
                }
                let count = s.devices.len();
                // 只有真正连上的才算「已连接」：之前把「等待/重试」的设备也算进去，
                // 会出现电脑端显示「已连接 2 台」而手机上还在等连接的情况
                let connected_devices = s
                    .devices
                    .values()
                    .filter(|st| matches!(st, EngineState::Connected(_)))
                    .count();
                let waiting_devices = count.saturating_sub(connected_devices);
                let (kind, title, sub) = if !engine_running {
                    // 没在接收就是没在接收，别让残留的设备状态把界面顶回「已连接」
                    (
                        0,
                        i18n::tr("未开始接收", "not receiving").to_string(),
                        i18n::tr("点「开始接收」等待手机连接", "click Start to wait for a phone")
                            .to_string(),
                    )
                } else if !connected.is_empty() {
                    (
                        2,
                        if connected_devices > 1 {
                            format!("已连接 {connected_devices} 台设备")
                        } else {
                            // 这一侧是电脑，连上来的是手机/平板：「已连接电脑」读着像在说自己，
                            // 统一叫「设备」（和「已连接 N 台设备」保持一套词）
                            "已连接设备".to_string()
                        },
                        // 收起时只给概况，完整通道列表在展开后显示
                        if waiting_devices > 0 {
                            format!(
                                "{connected_devices} 台已连接 · {waiting_devices} 台等待 · {} 条通道",
                                connected.len()
                            )
                        } else {
                            format!("{connected_devices} 台设备 · {} 条通道", connected.len())
                        },
                    )
                } else if let Some(reason) = retrying {
                    (3, "连接有问题，正在重试".to_string(), reason)
                } else if let Some(reason) = waiting {
                    (1, "等待手机连接".to_string(), reason)
                } else {
                    (
                        0,
                        "未开始接收".to_string(),
                        "点「开始接收」等待手机连接".to_string(),
                    )
                };
                let full_channels = format!("通道：{}", connected.join(", "));
                s.state = if count > 0 {
                    EngineState::Connected(connected)
                } else {
                    EngineState::Stopped
                };
                ui.set_status_kind(kind);
                ui.set_status_title(title.into());
                ui.set_status_sub(sub.into());
                ui.set_status_sub_full(full_channels.clone().into());
                ui.set_running(engine_running);

                let mut serials: Vec<String> = s.devices.keys().cloned().collect();
                serials.sort();
                s.device_order = serials.clone();
                let mut rows: Vec<DeviceRow> = Vec::with_capacity(count);
                for serial in &s.device_order {
                    let state = &s.devices[serial];
                    let (label, detail) = match state {
                        EngineState::Connected(names) => (
                            "已连接",
                            format!("通道：{}", names.join(", ")),
                        ),
                        EngineState::Waiting(reason) => ("等待连接", reason.clone()),
                        EngineState::Retrying(reason) => ("重试中", reason.clone()),
                        EngineState::Stopped => ("已停止", String::new()),
                    };
                    let progress = hfx_receiver::receiver::progress_for(serial);
                    let (path, done, total, session_bytes, files) = progress.snapshot();
                    let detail = if detail.is_empty() {
                        format!("已接收 {} · {} 个文件", human(session_bytes), files)
                    } else {
                        format!("{}\n已接收 {} · {} 个文件", detail, human(session_bytes), files)
                    };
                    let _ = (path, done, total);
                    rows.push(DeviceRow {
                        name: serial.chars().take(8).collect::<String>().into(),
                        state: label.into(),
                        rate: human(session_bytes).into(),
                        detail: detail.into(),
                        expanded: s.expanded.contains(serial),
                    });
                }
                // 平板模型给"选择接收设备"卡片用；配对模型给一行两个的设备卡片用
                ui.set_devices(slint::ModelRc::new(slint::VecModel::from(rows.clone())));

                // 两两配对，界面一行画两个
                let mut pairs: Vec<DevicePair> = Vec::with_capacity((rows.len() + 1) / 2);
                let mut iter = rows.into_iter();
                while let Some(left) = iter.next() {
                    let right = iter.next();
                    pairs.push(DevicePair {
                        has_right: right.is_some(),
                        right: right.unwrap_or(DeviceRow {
                            name: "".into(),
                            state: "".into(),
                            rate: "".into(),
                            detail: "".into(),
                            expanded: false,
                        }),
                        left,
                    });
                }
                ui.set_device_rows(slint::ModelRc::new(slint::VecModel::from(pairs)));
            }

            // 多台设备在线时暂扣待发送文件，避免某台抢先取走
            hfx_receiver::pending::set_hold(s.devices.len() >= 2);
            // 多台设备 + 队列里有没有指定目标的文件 → 弹出选择
            let ask = s.devices.len() >= 2 && hfx_receiver::pending::needs_target();
            if ask != ui.get_ask_target() {
                ui.set_ask_target(ask);
            }
            if ask && !s.asked_target {
                s.asked_target = true;
                let _ = ui.show();
                #[cfg(windows)]
                tray_win::show_main_window(WINDOW_TITLE);
            } else if !ask {
                s.asked_target = false;
            }

            // 开机自启：等窗口真正创建后隐藏到托盘（只做一次）
            #[cfg(windows)]
            if launch_at_boot && !boot_hidden && ui.get_tray() {
                boot_hidden = true;
                tray_win::hide_main_window(WINDOW_TITLE);
            }

            // ---- 传输进度 ----
            let (path, done, total, session_bytes, files) =
                hfx_receiver::receiver::PROGRESS.snapshot();
            // 下面 segs 那一段会把 total 这个名字借去当 f32 用（段宽合计），
            // 独立窗口那边要的还是字节总数，先留一份
            let total_bytes = total;

            // 速率只按「这次真收到的字节」算：续传基线（磁盘上本来就有的一块）不算进去，
            // 否则续传一开始会跳出一个几 GB/s 的假峰值。
            let received = hfx_receiver::receiver::PROGRESS.received_bytes();
            let now = std::time::Instant::now();
            let elapsed = now.duration_since(last_tick).as_secs_f64();
            if elapsed >= 0.4 {
                let delta = received.saturating_sub(last_bytes);
                let instant = delta as f64 / elapsed;
                // 轻微平滑，避免数字乱跳
                speed = if speed <= 0.0 { instant } else { speed * 0.6 + instant * 0.4 };
                last_bytes = received;
                last_tick = now;
            }

            // 没有设备真正连着时（比如手机端服务被强停）不再显示残留的传输卡片
            let any_connected = s
                .devices
                .values()
                .any(|st| matches!(st, EngineState::Connected(_)));
            let active = s.running() && total > 0 && any_connected;
            ui.set_xfer_visible(active);
            // 发送中 / 接收中都显示「暂停 / 继续」，发送时额外给「中断发送」
            ui.set_send_active(hfx_receiver::control::sending());
            ui.set_recv_active(hfx_receiver::control::receiving());
            // 这一轮接收的「取消」是否已经受理（收尾 / 重建中）：是就把取消按钮收起来，
            // 取消是幂等的 —— 同一轮接收只允许取消一次。
            ui.set_recv_cancelling(
                hfx_receiver::control::receiving() && hfx_receiver::control::cancel_requested(),
            );
            ui.set_send_paused(hfx_receiver::control::paused());

            // ---- 取消接收：确认框与结果提示 ----
            // 什么时候算「收完尾」：进度卡没了（handle_receive 在 finish_with 之后才 reset 进度），
            // 或者 RECEIVING 已经落下去。两个条件谁先满足都行 —— 「已删除」不能比实际删文件
            // 早一句话，而取消之后的重建握手（最长几十秒）不该把这句话一起拖着。
            let receiving_now = hfx_receiver::control::receiving();
            let idle_now = !receiving_now || !active;
            if let Some(note) = s.cancel_note.clone() {
                if idle_now {
                    s.cancel_note = None;
                    ui.set_notice_text(note.into());
                }
            }
            // 传输自己结束了 / 引擎被停了：确认框没意义了，顺手把「为了弹框」加的暂停放掉，
            // 否则下一次接收会从「已暂停」开始
            // 已经在取消中（或者接收已经结束）就把确认框收掉：用户第二次点「取消」
            // 时不该再看到它，更不该让那里再选一次「保留 / 不保留」。
            if ui.get_cancel_ask()
                && (!receiving_now || hfx_receiver::control::cancel_requested())
            {
                ui.set_cancel_ask(false);
                if !hfx_receiver::control::cancel_requested() {
                    hfx_receiver::control::pause(false);
                }
            }
            // 新的一次传输开始了：上一次的取消提示让位（进度卡回来了，提示就不该再挂着）
            if !ui.get_notice_text().is_empty() && active {
                ui.set_notice_text("".into());
            }

            if active {
                let short = path
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(&path)
                    .to_string();
                ui.set_file_name(short.clone().into());
                ui.set_file_progress(if total > 0 { done as f32 / total as f32 } else { 0.0 });
                ui.set_file_text(format!("{} / {}", human(done), human(total)).into());
                // 手机来取文件时是"已发送"，手机推过来时才是"已接收"
                let verb = if ui.get_send_active() { "已发送" } else { "已接收" };
                // 暂停时速率位置显示「已暂停」（和手机端弹窗一致）
                let speed_text = if ui.get_send_paused() {
                    "已暂停".to_string()
                } else {
                    human_speed(speed)
                };
                ui.set_session_text(format!(
                    "{} {} · {} · {} 个文件",
                    verb,
                    human(session_bytes),
                    speed_text,
                    files
                ).into());

                // IDM 式分块：和手机端一样按"哪条通道负责哪一段"画。
                // 两个方向都统一用块位图（mark_sent / mark_received 都按块记下真实通道号），
                // 段边界 = 相邻块通道归属的变化处：一个块传完就重算一次分段，
                // 通道抢走别人的段后，旧的边界会一直留在位图里（切分痕迹）。
                // 之前发送方向用 SendPlan 的任务区间：边界只在"领任务"那一刻重算，
                // 和实际发完的块对不上，看起来就是整段一起跳、没有切分痕迹。
                let raw: Vec<hfx_receiver::receiver::XferSeg> = hfx_receiver::receiver::PROGRESS.segments();
                let total: f32 = raw.iter().map(|s| s.span).sum::<f32>().max(1.0);
                let mut start = 0.0f32;
                let mut segs: Vec<XferSeg> = Vec::with_capacity(raw.len());
                for s in &raw {
                    let span = s.span / total;
                    segs.push(XferSeg {
                        start: start.clamp(0.0, 1.0),
                        span: span.clamp(0.0, 1.0),
                        filled: s.filled.clamp(0.0, 1.0),
                    });
                    start += span;
                }
                // 已填充比例（按块算）：流光只在这一段里扫
                let fill_frac: f32 = segs.iter().map(|s| s.span * s.filled).sum::<f32>().clamp(0.0, 1.0);
                let seg_model = slint::ModelRc::new(slint::VecModel::from(segs));
                ui.set_xfer_segments(seg_model.clone());

                // ---- 独立传输窗口：同一份数据再喂一遍 ----
                // 传完了没有：真机第一次跑出来的是「已完成 100%」+ 0 B/s 一直挂着不消失 ——
                // 因为 active 只看「有设备连着 + 有进度」，传完之后 PROGRESS 不清零，
                // 这两个条件依然成立。所以「结束」得单独认：字节满了就算完，
                // 亮 3 秒「已完成」再自己收起来。
                // 真机第二次跑出来：传完了 done 停在 599.0/600.0 MB（比 total 少 1MB，
                // 块计数的尾巴对不上），只按 done>=total 判永远判不到「完成」，
                // 窗口就挂着「已完成 99%」不消失。所以再认一个信号：
                // control 那边 receiving()/sending() 都落下去，就是这一轮结束了。
                let in_flight = hfx_receiver::control::receiving() || hfx_receiver::control::sending();
                let finished = (total_bytes > 0 && done >= total_bytes) || !in_flight;
                if let Some(tw) = xfer_weak.upgrade() {
                    tw.set_action_text(
                        if finished {
                            if ui.get_send_active() {
                                "文件已发送完成"
                            } else {
                                "文件已接收完成"
                            }
                        } else if ui.get_send_active() {
                            "正在发送文件"
                        } else {
                            "正在接收文件"
                        }
                        .into(),
                    );
                    // 百分比往下取整：向上四舍五入会在「1.99 / 2.00 GB」就跳成 100%，
                    // 而这时其实还在传（第一次真机就是 100% 挂着 3.5 MB/s，看着像卡死）。
                    // 没传完最多显示 99%，满字节了才给 100%。
                    let pct = if total_bytes > 0 {
                        ((done.min(total_bytes) as f64 / total_bytes as f64) * 100.0).floor() as i32
                    } else {
                        0
                    };
                    tw.set_percent(if finished { 100 } else { pct.min(99) });
                    tw.set_file_name(short.clone().into());
                    tw.set_detail_text(
                        format!("{} / {} · {}", human(done), human(total_bytes), speed_text)
                    .into());
                    tw.set_paused(hfx_receiver::control::paused());
                    tw.set_done(finished);
                    tw.set_fill(fill_frac);
                    tw.set_segs(seg_model);
                    if finished {
                        // 第一眼看到「满了」的时候记时间，3 秒后收窗口
                        if tw_finish_at.is_none() {
                            tw_finish_at = Some(std::time::Instant::now());
                        }
                        if tw_shown
                            && tw_finish_at
                                .map(|t| t.elapsed() >= Duration::from_secs(3))
                                .unwrap_or(false)
                        {
                            tw_shown = false;
                            xfer_dismissed.store(false, Ordering::Relaxed);
                            let _ = tw.hide();
                        }
                    } else {
                        // 又一轮新传输（文件换了 / 续传接上了）：允许重新弹
                        tw_finish_at = None;
                        // 新一轮传输才重新弹（用户点过「收起」就别再烦他）
                        if !tw_shown && !xfer_dismissed.load(Ordering::Relaxed) {
                            tw_shown = true;
                            let _ = tw.show();
                        }
                    }
                }
            } else if tw_shown {
                // 传输结束：窗口自己收起来，下一轮重新允许弹出
                tw_shown = false;
                xfer_dismissed.store(false, Ordering::Relaxed);
                if let Some(tw) = xfer_weak.upgrade() {
                    tw.set_done(true);
                    let _ = tw.hide();
                }
            }

            // 进度卡 / 更新提示出现时调整窗口高度（否则底部按钮会被裁掉）
            let xfer_now = ui.get_xfer_visible();
            // 展开设备列表后内容会变高，窗口高度要跟着加，否则底部按钮被裁掉
            // 展开后是一行两个的小卡片：每行 46 高 + 行距 6，再留出通道换行与内边距
            let devices_h = if ui.get_devices_expanded() {
                let rows = ((s.devices.len() + 1) / 2).max(1) as f32;
                rows * 52.0 + 44.0
            } else {
                0.0
            };
            let want_h = 400.0
                + if ui.get_ask_target() { 60.0 } else { 0.0 }
                // 状态卡下面那行「已取消…」的提示
                + if ui.get_notice_text().is_empty() { 0.0 } else { 20.0 }
                + devices_h
                + if xfer_now { 190.0 } else { 0.0 }
                + if xfer_now && ui.get_send_active() { 50.0 } else { 0.0 }
                + if ui.get_update_state() == 2 || ui.get_update_state() == 5 {
                    34.0
                } else {
                    0.0
                };
            if (want_h - last_want_h).abs() > 0.5 {
                last_want_h = want_h;
                last_xfer_visible = xfer_now;
                let scale = ui.window().scale_factor();
                let current = ui.window().size();
                let width = current.width as f32 / scale;
                ui.window()
                    .set_size(slint::LogicalSize::new(width, want_h));
            }

        });
    }

    // ---------- 更新检查 ----------
    start_update_check(ui.as_weak());

    {
        let ui_weak = ui.as_weak();
        ui.on_update_clicked(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            ui.set_update_state(3);
            ui.set_update_text("正在下载更新，完成后会自动重启…".into());
            let ui_weak2 = ui_weak.clone();
            std::thread::spawn(move || {
                match open_release_page() {
                    Ok(()) => {
                        post_ui(ui_weak2.clone(), move |ui| {
                            ui.set_update_state(5);
                            ui.set_update_text("已打开下载页面".into());
                        });
                    }
                    Err(e) => {
                        let text = format!("更新失败：{}", e);
                        post_ui(ui_weak2.clone(), move |ui| {
                            ui.set_update_state(4);
                            ui.set_update_text(text.into());
                        });
                    }
                }
            });
        });
    }

    // ---------- 系统托盘 / 关闭行为 ----------
    #[cfg(windows)]
    {
        let ui_weak = ui.as_weak();
        ui.window().on_close_requested(move || {
            let tray_enabled = ui_weak.upgrade().map(|ui| ui.get_tray()).unwrap_or(false);
            if tray_enabled {
                tray_win::hide_main_window(WINDOW_TITLE);
                // 不真正关窗，只把原生窗口隐藏，事件循环继续跑
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                slint::CloseRequestResponse::HideWindow
            }
        });

        let ui_weak = ui.as_weak();
        tray_win::init(
            move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let _ = ui.show();
                }
                tray_win::show_main_window(WINDOW_TITLE);
            },
            || {
                let _ = slint::quit_event_loop();
            },
        );
    }

    // --autostart：开机自启时直接开始接收（隐藏交给轮询定时器，等窗口真正创建好）
    if launch_at_boot {
        ui.invoke_start_clicked();
    }

    ui.run()
}

// ---------------- 更新：检查 / 下载 / 自我替换 ----------------

/// Slint 的属性只能在 UI 线程改，后台线程要用这个把更新送回事件循环
fn post_ui(
    ui_weak: slint::Weak<AppWindow>,
    action: impl FnOnce(&AppWindow) + Send + 'static,
) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            action(&ui);
        }
    });
}

const REPO: &str = "yizhiruonan/HybridFileXfer";
/// Release 里单独放一个接收器版本号文件，避免和整个项目的 tag（v3.1.0）混在一起
const VERSION_ASSET: &str = "receiver.version";
const EXE_ASSET: &str = "HybridFileXfer-Receiver.exe";

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let t = text.trim();
    let t = t.strip_prefix('v').unwrap_or(t);
    let mut parts = t.split('.');
    let a = parts.next()?.trim().parse().ok()?;
    let b = parts.next()?.trim().parse().ok()?;
    let c = parts
        .next()
        .and_then(|x| x.trim().parse().ok())
        .unwrap_or(0);
    Some((a, b, c))
}

static UPDATE_EXE_URL: std::sync::LazyLock<Mutex<String>> =
    std::sync::LazyLock::new(|| Mutex::new(String::new()));
static UPDATE_VERSION: std::sync::LazyLock<Mutex<String>> =
    std::sync::LazyLock::new(|| Mutex::new(String::new()));

fn start_update_check(ui_weak: slint::Weak<AppWindow>) {
    std::thread::spawn(move || {
        let fail = |msg: &str| {
            let text = msg.to_string();
            post_ui(ui_weak.clone(), move |ui| {
                ui.set_update_state(4);
                ui.set_update_text(text.into());
            });
        };

        let api = format!("https://api.github.com/repos/{REPO}/releases/latest");
        let release: serde_json::Value = match ureq::get(&api)
            .set("User-Agent", "HybridFileXfer-Receiver")
            .set("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(15))
            .call()
            .and_then(|r| r.into_json::<serde_json::Value>().map_err(Into::into))
        {
            Ok(v) => v,
            Err(_) => return fail("检查更新失败（网络不可用）"),
        };

        let mut version_url = String::new();
        let mut exe_url = String::new();
        if let Some(assets) = release["assets"].as_array() {
            for asset in assets {
                let name = asset["name"].as_str().unwrap_or_default();
                let url = asset["browser_download_url"].as_str().unwrap_or_default();
                if name == VERSION_ASSET {
                    version_url = url.to_string();
                } else if name == EXE_ASSET {
                    exe_url = url.to_string();
                }
            }
        }
        if version_url.is_empty() || exe_url.is_empty() {
            return fail("更新信息不完整（缺少接收器附件）");
        }

        let latest_text = match ureq::get(&version_url)
            .timeout(Duration::from_secs(15))
            .call()
            .and_then(|r| r.into_string().map_err(Into::into))
        {
            Ok(t) => t,
            Err(_) => return fail("检查更新失败（无法读取版本号）"),
        };

        let latest = match parse_version(&latest_text) {
            Some(v) => v,
            None => return fail("更新信息格式不正确"),
        };
        let current = parse_version(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0));

        if latest > current {
            if let Ok(mut guard) = UPDATE_EXE_URL.lock() {
                *guard = exe_url;
            }
            if let Ok(mut guard) = UPDATE_VERSION.lock() {
                *guard = format!("{}.{}.{}", latest.0, latest.1, latest.2);
            }
            let text = format!(
                "发现新版本 v{}.{}.{}（当前 v{}.{}.{}）",
                latest.0, latest.1, latest.2, current.0, current.1, current.2
            );
            post_ui(ui_weak.clone(), move |ui| {
                ui.set_update_state(2);
                ui.set_update_text(text.into());
            });
        } else {
            post_ui(ui_weak.clone(), move |ui| ui.set_update_state(1));
        }
    });
}

/// 不下载可执行文件（会被杀软当成 dropper），只在浏览器里打开 Release 页面
fn open_release_page() -> Result<(), String> {
    let url = "https://github.com/yizhiruonan/HybridFileXfer/releases/latest";
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .map_err(|e| format!("打开浏览器失败：{e}"))?;
    Ok(())
}

// ---------------- 工具函数 ----------------

/// 字节数 -> 人类可读
fn human(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KB * KB * KB {
        format!("{:.2} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// 速度（字节/秒）-> 人类可读
fn human_speed(bps: f64) -> String {
    // 单位要跟着量级走：蓝牙那种 0.2~0.3 MB/s 的通道，按 MB 只有一位小数会显示成 0，
    // 看着像断了；慢速一律降到 KB/s，再慢给 B/s，静态时为 0 KB/s
    let mb = bps / (1024.0 * 1024.0);
    if bps <= 0.0 {
        "0 KB/s".to_string()
    } else if mb >= 1.0 {
        format!("{:.1} MB/s", mb)
    } else if bps >= 1024.0 {
        format!("{:.0} KB/s", bps / 1024.0)
    } else {
        format!("{:.0} B/s", bps)
    }
}

fn detect_lang() -> Lang {
    #[cfg(windows)]
    {
        extern "system" {
            fn GetUserDefaultUILanguage() -> u16;
        }
        let id = unsafe { GetUserDefaultUILanguage() };
        if (id & 0x3FF) == 0x04 {
            return Lang::Zh;
        }
    }
    if let Ok(lang) = std::env::var("LANG") {
        if lang.to_ascii_lowercase().starts_with("zh") {
            return Lang::Zh;
        }
    }
    Lang::En
}

fn open_in_explorer(path: &str) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

/// 开机自启用注册表 Run 项（HKCU），不要写启动目录的 .vbs 脚本——
/// 那种"释放脚本到启动目录"的行为会被杀软判为 dropper。
fn autostart_value_name() -> Vec<u16> {
    "HybridFileXfer-Receiver\0".encode_utf16().collect()
}

fn run_key_path() -> Vec<u16> {
    "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0"
        .encode_utf16()
        .collect()
}

const AUTOSTART_TASK: &str = "HybridFileXfer-Receiver";

fn hidden_command(program: &str) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
}

#[cfg(windows)]
fn logon_task_exists() -> bool {
    hidden_command("schtasks")
        .args(["/Query", "/TN", AUTOSTART_TASK])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn create_logon_task(exe: &Path) -> bool {
    let tr = format!("\"{}\" --autostart", exe.display());
    hidden_command("schtasks")
        .args([
            "/Create", "/TN", AUTOSTART_TASK, "/TR", &tr, "/SC", "ONLOGON", "/RL", "LIMITED", "/F",
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn delete_logon_task() {
    let _ = hidden_command("schtasks")
        .args(["/Delete", "/TN", AUTOSTART_TASK, "/F"])
        .status();
}

/// 注册表 Run 项里有没有我们的条目
#[cfg(windows)]
fn run_value_present() -> bool {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    };
    let mut key: HKEY = std::ptr::null_mut();
    let sub = run_key_path();
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_READ, &mut key) };
    if status != 0 {
        return false;
    }
    let name = autostart_value_name();
    let mut size: u32 = 0;
    let query = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    unsafe { RegCloseKey(key) };
    query == 0 && size > 0
}

fn startup_shortcut_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
            .join("HybridFileXfer-Receiver.lnk"),
    )
}

fn startup_shortcut_present() -> bool {
    startup_shortcut_path().map(|p| p.exists()).unwrap_or(false)
}

fn write_startup_shortcut(exe: &Path) -> bool {
    let Some(lnk) = startup_shortcut_path() else {
        return false;
    };
    let Some(dir) = exe.parent() else {
        return false;
    };
    if let Some(parent) = lnk.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let ps = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}'); $s.TargetPath='{}'; $s.Arguments='--autostart'; $s.WorkingDirectory='{}'; $s.IconLocation='{},0'; $s.Save()",
        lnk.display(),
        exe.display(),
        dir.display(),
        exe.display()
    );
    let encoded = hfx_receiver::sendto::encode_utf16le_base64(&ps);
    let ok = hidden_command("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-EncodedCommand",
            &encoded,
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    ok && lnk.exists()
}

fn remove_startup_shortcut() {
    if let Some(lnk) = startup_shortcut_path() {
        let _ = std::fs::remove_file(lnk);
    }
}

#[cfg(windows)]
fn autostart_enabled() -> bool {
    // 计划任务 / 启动目录快捷方式 / 注册表 Run 任一存在都算勾选
    logon_task_exists() || startup_shortcut_present() || run_value_present()
}

#[cfg(not(windows))]
fn autostart_enabled() -> bool {
    false
}

#[cfg(windows)]
fn write_run_value(exe: &Path) -> io::Result<()> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ,
    };
    let mut key: HKEY = std::ptr::null_mut();
    let sub = run_key_path();
    let status =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_SET_VALUE, &mut key) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let name = autostart_value_name();
    let value = format!("\"{}\" --autostart", exe.display());
    let data: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            REG_SZ,
            data.as_ptr() as *const u8,
            (data.len() * 2) as u32,
        )
    };
    unsafe { RegCloseKey(key) };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    Ok(())
}

#[cfg(windows)]
fn delete_run_value() -> io::Result<()> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
    };
    let mut key: HKEY = std::ptr::null_mut();
    let sub = run_key_path();
    let status =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_SET_VALUE, &mut key) };
    if status != 0 {
        return Ok(());
    }
    let name = autostart_value_name();
    unsafe { RegDeleteValueW(key, name.as_ptr()) };
    unsafe { RegCloseKey(key) };
    Ok(())
}

/// 优先用「登录时触发的计划任务」：Windows 11 的「启动应用」列表不会拦计划任务，
/// 比 HKCU\Run 可靠；建任务失败再退回注册表。
#[cfg(windows)]
fn set_autostart(enabled: bool, exe_dir: &Path) -> io::Result<()> {
    let exe = exe_dir.join("HybridFileXfer-Receiver.exe");
    if enabled {
        // 计划任务最稳，但非管理员建不了；退而求其次用启动目录快捷方式；
        // 都不行再退回注册表 Run。每次只保留一种，避免登录时起两次。
        if create_logon_task(&exe) {
            let _ = delete_run_value();
            remove_startup_shortcut();
            Ok(())
        } else if write_startup_shortcut(&exe) {
            let _ = delete_run_value();
            delete_logon_task();
            Ok(())
        } else {
            write_run_value(&exe)
        }
    } else {
        delete_logon_task();
        remove_startup_shortcut();
        let _ = delete_run_value();
        Ok(())
    }
}

#[cfg(not(windows))]
fn set_autostart(_enabled: bool, _exe_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// 旧版本用启动目录 .vbs 实现自启，启动时顺手清掉
fn remove_legacy_autostart_script() {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let legacy = PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
            .join("HybridFileXfer-Receiver.vbs");
        if legacy.exists() {
            let _ = std::fs::remove_file(legacy);
        }
    }
}

#[cfg(windows)]
fn pick_folder() -> Option<PathBuf> {
    use std::ffi::c_void;

    #[repr(C)]
    struct BrowseInfoW {
        hwnd_owner: isize,
        pidl_root: *mut c_void,
        psz_display_name: *mut u16,
        lpsz_title: *mut u16,
        ul_flags: u32,
        lpfn: isize,
        l_param: isize,
        i_image: i32,
    }

    #[link(name = "shell32")]
    extern "system" {
        fn SHBrowseForFolderW(info: *mut BrowseInfoW) -> *mut c_void;
        fn SHGetPathFromIDListW(pidl: *const c_void, path: *mut u16) -> i32;
    }

    #[link(name = "ole32")]
    extern "system" {
        fn CoTaskMemFree(pv: *mut c_void);
    }

    let title: Vec<u16> = "选择接收文件夹\0".encode_utf16().collect();
    let mut info = BrowseInfoW {
        hwnd_owner: 0,
        pidl_root: std::ptr::null_mut(),
        psz_display_name: std::ptr::null_mut(),
        lpsz_title: title.as_ptr() as *mut u16,
        ul_flags: 0x0001 | 0x0040 | 0x0010,
        lpfn: 0,
        l_param: 0,
        i_image: 0,
    };
    unsafe {
        let pidl = SHBrowseForFolderW(&mut info);
        if pidl.is_null() {
            return None;
        }
        let mut buffer = [0u16; 1024];
        let ok = SHGetPathFromIDListW(pidl, buffer.as_mut_ptr()) != 0;
        CoTaskMemFree(pidl);
        if !ok {
            return None;
        }
        let len = buffer.iter().position(|&c| c == 0).unwrap_or(0);
        Some(PathBuf::from(String::from_utf16_lossy(&buffer[..len])))
    }
}

#[cfg(not(windows))]
fn pick_folder() -> Option<PathBuf> {
    None
}