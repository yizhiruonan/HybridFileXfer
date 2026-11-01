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

    // 清理上一次自我更新留下的旧文件
    let _ = std::fs::remove_file(exe_dir.join("HybridFileXfer-Receiver.exe.old"));

    remove_legacy_autostart_script();
    let autostart = autostart_enabled();
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
    ui.set_dir_text(state.lock().unwrap().config.receive_dir.display().to_string().into());
    ui.set_port_text(state.lock().unwrap().config.port.to_string().into());
    ui.set_ip_text(ip_text.into());
    ui.set_autostart(autostart);
    ui.set_tray(state.lock().unwrap().config.minimize_to_tray);
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
        let mut last_bytes = 0u64;
        let mut last_tick = std::time::Instant::now();
        let mut speed = 0f64;
        let mut last_xfer_visible = false;
        let mut last_want_h = 0f32;
        let mut boot_hidden = false;

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

            // 开机自启：等窗口真正创建后隐藏到托盘（只做一次）
            #[cfg(windows)]
            if launch_at_boot && !boot_hidden && ui.get_tray() {
                boot_hidden = true;
                tray_win::hide_main_window(WINDOW_TITLE);
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

            // 进度卡 / 更新提示出现时调整窗口高度（否则底部按钮会被裁掉）
            let xfer_now = ui.get_xfer_visible();
            let want_h = 400.0
                + if xfer_now { 130.0 } else { 0.0 }
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
                match download_update() {
                    Ok(path) => {
                        let text = format!("已下载到下载目录，关闭本程序后替换即可：{}", path.display());
                        post_ui(ui_weak2.clone(), move |ui| {
                            ui.set_update_state(5);
                            ui.set_update_text(text.into());
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

/// 只负责把新版本下载到下载目录，然后定位到该文件；
/// 不做“替换自身 + 重新启动”，那是杀软眼里的 dropper 行为（会被误报为木马释放器）。
fn download_update() -> Result<PathBuf, String> {
    let url = UPDATE_EXE_URL
        .lock()
        .map(|g| g.clone())
        .map_err(|_| "内部状态错误".to_string())?;
    if url.is_empty() {
        return Err("没有可用的下载地址".into());
    }
    let version = UPDATE_VERSION
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();

    let downloads = std::env::var_os("USERPROFILE")
        .map(|p| PathBuf::from(p).join("Downloads"))
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let target = downloads.join(format!(
        "HybridFileXfer-Receiver{}.exe",
        if version.is_empty() {
            String::new()
        } else {
            format!("-v{version}")
        }
    ));

    let response = ureq::get(&url)
        .timeout(Duration::from_secs(600))
        .call()
        .map_err(|e| format!("下载失败：{e}"))?;
    let mut reader = response.into_reader();
    let mut file = std::fs::File::create(&target).map_err(|e| format!("创建文件失败：{e}"))?;
    std::io::copy(&mut reader, &mut file).map_err(|e| format!("写入失败：{e}"))?;
    drop(file);

    // 在资源管理器里定位到刚下载的文件
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{}", target.display()))
        .spawn();

    Ok(target)
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

#[cfg(windows)]
fn autostart_enabled() -> bool {
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

#[cfg(not(windows))]
fn autostart_enabled() -> bool {
    false
}

#[cfg(windows)]
fn set_autostart(enabled: bool, exe_dir: &Path) -> io::Result<()> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_SZ,
    };
    let mut key: HKEY = std::ptr::null_mut();
    let sub = run_key_path();
    let status =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_SET_VALUE, &mut key) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let name = autostart_value_name();
    let result = if enabled {
        let exe = exe_dir.join("HybridFileXfer-Receiver.exe");
        let value = format!("\"{}\" --autostart", exe.display());
        let data: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            )
        }
    } else {
        unsafe { RegDeleteValueW(key, name.as_ptr()) }
    };
    unsafe { RegCloseKey(key) };
    if result != 0 && !(result == 2 && !enabled) {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    Ok(())
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