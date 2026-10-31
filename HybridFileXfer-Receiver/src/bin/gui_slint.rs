//! 图形界面版接收器（Slint + fluent 样式，Windows 11 观感）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
    state_rx: Receiver<EngineState>,
    state_tx: Sender<EngineState>,
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
        self.state = EngineState::Stopped;
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
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let tx = Arc::new(Mutex::new(self.state_tx.clone()));
        let sink: engine::StateSink = Arc::new(move |state| {
            if let Ok(sender) = tx.lock() {
                let _ = sender.send(state);
            }
        });
        // engine::run 是阻塞的，必须自己开线程
        let join = std::thread::spawn(move || {
            engine::run(options, stop_for_thread, sink);
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

    log::init(Some(exe_dir.join("receiver.log")), false);
    log::set_console(false);

    let (state_tx, state_rx) = mpsc::channel();

    let autostart = autostart_path().map(|p| p.exists()).unwrap_or(false);
    let ip_text = std::env::var("HFX_LAST_IP").unwrap_or_else(|_| "192.168.1.100".to_string());

    let state = Arc::new(Mutex::new(State {
        config,
        config_path: config_path.clone(),
        exe_dir: exe_dir.clone(),
        engine: None,
        state: EngineState::Stopped,
        state_rx,
        state_tx,
    }));

    let ui = AppWindow::new()?;
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    ui.set_dir_text(state.lock().unwrap().config.receive_dir.display().to_string().into());
    ui.set_port_text(state.lock().unwrap().config.port.to_string().into());
    ui.set_ip_text(ip_text.into());
    ui.set_autostart(autostart);
    ui.set_status_kind(0);

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
            hfx_receiver::receiver::PROGRESS.reset();
            s.start_engine(port, mode, dir);
            ui.set_running(true);
        });
    }
    {
        let st = Arc::clone(&state);
        let ui_weak = ui.as_weak();
        ui.on_stop_clicked(move || {
            let mut s = st.lock().unwrap();
            s.stop_engine();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_running(false);
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
        ui.on_open_dir(move || {
            let s = st.lock().unwrap();
            open_in_explorer(&s.config.receive_dir.display().to_string());
        });
    }

    // ---------- 定时把线程里的状态/日志刷到界面 ----------
    let timer = slint::Timer::default();
    {
        let st = Arc::clone(&state);
        let ui_weak = ui.as_weak();
        let mut last_bytes = 0u64;
        let mut last_tick = std::time::Instant::now();
        let mut speed = 0f64;
        let mut last_xfer_visible = false;

        timer.start(slint::TimerMode::Repeated, Duration::from_millis(150), move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut s = st.lock().unwrap();

            while let Ok(next) = s.state_rx.try_recv() {
                s.state = next;
                let (kind, title, sub) = match &s.state {
                    EngineState::Connected(names) => (
                        2,
                        "已连接电脑".to_string(),
                        format!("通道：{}", names.join(", ")),
                    ),
                    EngineState::Waiting(reason) => (1, "等待手机连接".to_string(), reason.clone()),
                    EngineState::Retrying(reason) => {
                        (3, "连接有问题，正在重试".to_string(), reason.clone())
                    }
                    EngineState::Stopped => (
                        0,
                        "未开始接收".to_string(),
                        "点「开始接收」等待手机连接".to_string(),
                    ),
                };
                ui.set_status_kind(kind);
                ui.set_status_title(title.into());
                ui.set_status_sub(sub.into());
                ui.set_running(!matches!(s.state, EngineState::Stopped));
            }

            // ---- 传输进度 ----
            let (path, done, total, session_bytes, files) =
                hfx_receiver::receiver::PROGRESS.snapshot();

            let now = std::time::Instant::now();
            let elapsed = now.duration_since(last_tick).as_secs_f64();
            if elapsed >= 0.4 {
                let delta = session_bytes.saturating_sub(last_bytes);
                let instant = delta as f64 / elapsed;
                // 轻微平滑，避免数字乱跳
                speed = if speed <= 0.0 { instant } else { speed * 0.6 + instant * 0.4 };
                last_bytes = session_bytes;
                last_tick = now;
            }

            let active = s.running() && total > 0;
            ui.set_xfer_visible(active);
            if active {
                let short = path
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(&path)
                    .to_string();
                ui.set_file_name(short.into());
                ui.set_file_progress(if total > 0 { done as f32 / total as f32 } else { 0.0 });
                ui.set_file_text(format!("{} / {}", human(done), human(total)).into());
                ui.set_session_text(format!(
                    "已接收 {} · {} · {} 个文件",
                    human(session_bytes),
                    human_speed(speed),
                    files
                ).into());

                // IDM 式分块：切 96 段，每段填多少由已收到的块决定
                let segs = hfx_receiver::receiver::PROGRESS.segments(96);
                ui.set_blocks(slint::ModelRc::new(slint::VecModel::from(segs)));
            }

            // 进度卡出现/消失时调整窗口高度（否则底部按钮会被裁掉）
            let visible_now = ui.get_xfer_visible();
            if visible_now != last_xfer_visible {
                last_xfer_visible = visible_now;
                let scale = ui.window().scale_factor();
                let current = ui.window().size();
                let width = current.width as f32 / scale;
                let height = if visible_now { 530.0 } else { 400.0 };
                ui.window()
                    .set_size(slint::LogicalSize::new(width, height));
            }

        });
    }

    ui.run()
}

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
    if bps <= 0.0 {
        return "0 MB/s".to_string();
    }
    let mb = bps / (1024.0 * 1024.0);
    if mb >= 1.0 {
        format!("{:.1} MB/s", mb)
    } else {
        format!("{:.0} KB/s", bps / 1024.0)
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

fn autostart_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
            .join("HybridFileXfer-Receiver.vbs"),
    )
}

fn set_autostart(enabled: bool, exe_dir: &Path) -> io::Result<()> {
    let path = autostart_path().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "APPDATA environment variable is missing")
    })?;
    if enabled {
        let exe = exe_dir.join("HybridFileXfer-Receiver.exe");
        let content = format!(
            "' HybridFileXfer 接收器开机自启\r\nCreateObject(\"WScript.Shell\").Run \"\"\"\" & \"{}\" & \"\"\" --autostart\", 7, False\r\n",
            exe.display()
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
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