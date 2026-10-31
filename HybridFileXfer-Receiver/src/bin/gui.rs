//! 图形界面版接收器。
//!
//! 电脑上双击运行，可以设置接收文件夹、开始/停止接收，并实时看到连接状态与日志。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use eframe::egui::{self, Color32, Margin, RichText, Rounding};

use hfx_receiver::config::{self, Config};
use hfx_receiver::engine::{self, ConnectMode, EngineOptions, EngineState};
use hfx_receiver::i18n::{self, Lang};
use hfx_receiver::log;

// ---------- 配色 ----------
const BG: Color32 = Color32::from_rgb(0xF4, 0xF6, 0xF9);
const CARD: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
const TEXT: Color32 = Color32::from_rgb(0x10, 0x14, 0x18);
const SUBTLE: Color32 = Color32::from_rgb(0x6B, 0x72, 0x80);
const GREEN: Color32 = Color32::from_rgb(0x16, 0xA3, 0x4A);
const GREEN_SOFT: Color32 = Color32::from_rgb(0xE8, 0xF7, 0xEE);
const GRAY_SOFT: Color32 = Color32::from_rgb(0xEC, 0xEF, 0xF4);
const WARN: Color32 = Color32::from_rgb(0xD9, 0x77, 0x06);
const RED: Color32 = Color32::from_rgb(0xEF, 0x44, 0x44);
const FIELD: Color32 = Color32::from_rgb(0xEE, 0xF1, 0xF5);

fn main() -> eframe::Result<()> {
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

    let autostart_launch = std::env::args().any(|a| a == "--autostart");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 760.0])
            .with_min_inner_size([680.0, 620.0])
            .with_title("多轨快传 · 电脑接收器"),
        ..Default::default()
    };

    eframe::run_native(
        "HybridFileXfer-Receiver",
        options,
        Box::new(move |cc| {
            Ok(Box::new(ReceiverApp::new(
                cc,
                config,
                config_path,
                exe_dir,
                autostart_launch,
            )))
        }),
    )
}

struct EngineHandle {
    stop: Arc<AtomicBool>,
    _join: Option<JoinHandle<()>>,
}

struct ReceiverApp {
    config: Config,
    config_path: PathBuf,
    exe_dir: PathBuf,
    mode_auto: bool,
    ip_text: String,
    port_text: String,
    dir_text: String,
    autostart: bool,
    state: EngineState,
    log_lines: Vec<String>,
    log_rx: Receiver<String>,
    log_tx: Sender<String>,
    state_tx: Sender<EngineState>,
    state_rx: Receiver<EngineState>,
    engine: Option<EngineHandle>,
    message: String,
}

impl ReceiverApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
        config_path: PathBuf,
        exe_dir: PathBuf,
        autostart_launch: bool,
    ) -> Self {
        setup_fonts(&cc.egui_ctx);
        setup_style(&cc.egui_ctx);

        let (log_tx, log_rx) = mpsc::channel();
        let (state_tx, state_rx) = mpsc::channel();
        log::set_ui_sink(log_tx.clone());

        let port_text = config.port.to_string();
        let dir_text = config.receive_dir.display().to_string();
        let ip_text = std::env::var("HFX_LAST_IP").unwrap_or_else(|_| "192.168.1.100".to_string());
        let autostart = autostart_path().map(|p| p.exists()).unwrap_or(false);

        let mut app = ReceiverApp {
            mode_auto: true,
            ip_text,
            port_text,
            dir_text,
            autostart,
            config,
            config_path,
            exe_dir,
            state: EngineState::Stopped,
            log_lines: Vec::new(),
            log_rx,
            log_tx,
            state_tx,
            state_rx,
            engine: None,
            message: String::new(),
        };
        if autostart_launch {
            app.start_engine();
        }
        app
    }

    fn running(&self) -> bool {
        self.engine.is_some()
    }

    fn start_engine(&mut self) {
        if self.engine.is_some() {
            return;
        }
        let port: u16 = match self.port_text.trim().parse() {
            Ok(port) => port,
            Err(_) => {
                self.message = "端口号不正确".to_string();
                return;
            }
        };
        let mode = if self.mode_auto {
            ConnectMode::Auto
        } else {
            match self.ip_text.trim().parse::<IpAddr>() {
                Ok(ip) => ConnectMode::Ip(ip),
                Err(_) => {
                    self.message = "IP 地址不正确".to_string();
                    return;
                }
            }
        };
        let dir = PathBuf::from(self.dir_text.trim());
        if dir.as_os_str().is_empty() {
            self.message = "请选择接收文件夹".to_string();
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.message = format!("无法创建接收文件夹：{}", e);
            return;
        }
        self.message.clear();

        // 记住这次的设置
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
        log::set_ui_sink(self.log_tx.clone());
        let join = std::thread::spawn(move || {
            engine::run(options, stop_for_thread, sink);
        });
        self.engine = Some(EngineHandle {
            stop,
            _join: Some(join),
        });
        self.state = EngineState::Waiting("正在启动…".to_string());
    }

    fn stop_engine(&mut self) {
        if let Some(handle) = self.engine.take() {
            handle.stop.store(true, Ordering::Relaxed);
            log::info(i18n::tr("正在停止接收…", "stopping…"));
        }
        self.state = EngineState::Stopped;
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("多轨快传").size(26.0).strong().color(TEXT));
            ui.label(RichText::new("电脑接收器").size(13.0).color(SUBTLE));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("打开日志").size(12.0).color(SUBTLE))
                            .fill(Color32::TRANSPARENT)
                            .rounding(Rounding::same(8.0)),
                    )
                    .clicked()
                {
                    let _ = std::process::Command::new("notepad")
                        .arg(self.exe_dir.join("receiver.log"))
                        .spawn();
                }
                if ui
                    .add(
                        egui::Button::new(RichText::new("接收文件夹").size(12.0).color(SUBTLE))
                            .fill(Color32::TRANSPARENT)
                            .rounding(Rounding::same(8.0)),
                    )
                    .clicked()
                {
                    open_in_explorer(&self.dir_text);
                }
            });
        });
        ui.add_space(10.0);
    }

    fn status_card(&mut self, ui: &mut egui::Ui) {
        let (fill, dot, title, subtitle) = match &self.state {
            EngineState::Connected(names) => (
                GREEN_SOFT,
                GREEN,
                i18n::tr("已连接电脑", "connected"),
                format!(
                    "{} {}",
                    i18n::tr("通道：", "channels: "),
                    names.join(", ")
                ),
            ),
            EngineState::Waiting(reason) => (
                GRAY_SOFT,
                SUBTLE,
                i18n::tr("等待手机连接", "waiting for the phone"),
                reason.clone(),
            ),
            EngineState::Retrying(reason) => (
                GRAY_SOFT,
                WARN,
                i18n::tr("连接有问题，正在重试", "retrying"),
                reason.clone(),
            ),
            EngineState::Stopped => (
                GRAY_SOFT,
                SUBTLE,
                i18n::tr("未开始接收", "not running"),
                i18n::tr(
                    "点下面的按钮开始等待手机",
                    "press start to wait for the phone",
                )
                .to_string(),
            ),
        };
        let connected = matches!(self.state, EngineState::Connected(_));
        card(ui, fill, |ui| {
            ui.horizontal(|ui| {
                status_badge(ui, dot, connected);
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(title).size(21.0).strong().color(TEXT));
                    ui.label(RichText::new(subtitle).size(13.0).color(SUBTLE));
                });
            });
        });
    }

    fn settings_card(&mut self, ui: &mut egui::Ui) {
        card(ui, CARD, |ui| {
            ui.label(RichText::new("接收设置").size(16.0).strong().color(TEXT));
            ui.add_space(6.0);
            egui::Grid::new("settings")
                .num_columns(3)
                .spacing([10.0, 10.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("接收文件夹").color(TEXT));
                    ui.add_sized(
                        [360.0, 26.0],
                        egui::TextEdit::singleline(&mut self.dir_text)
                            .hint_text("C:\\Users\\你\\Downloads\\HybridFileXfer"),
                    );
                    if ui.button("浏览…").clicked() {
                        if let Some(dir) = pick_folder() {
                            self.dir_text = dir.to_string_lossy().to_string();
                        }
                    }
                    ui.end_row();

                    ui.label(RichText::new("端口").color(TEXT));
                    ui.add_sized(
                        [100.0, 26.0],
                        egui::TextEdit::singleline(&mut self.port_text),
                    );
                    ui.label(RichText::new("默认 5740，需与手机端一致").size(11.0).color(SUBTLE));
                    ui.end_row();
                });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.mode_auto, true, "自动（USB / 局域网发现）");
                ui.radio_value(&mut self.mode_auto, false, "指定 IP");
                if !self.mode_auto {
                    ui.add_sized(
                        [150.0, 26.0],
                        egui::TextEdit::singleline(&mut self.ip_text).hint_text("192.168.1.5"),
                    );
                }
            });
            ui.add_space(6.0);
            if ui
                .checkbox(&mut self.autostart, "开机自动启动（最小化运行）")
                .changed()
            {
                let result = set_autostart(self.autostart, &self.exe_dir);
                if let Err(e) = result {
                    self.message = format!("设置开机自启失败：{}", e);
                }
            }
            if !self.message.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new(&self.message).size(12.0).color(WARN));
            }

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let running = self.running();
                let start = ui.add_enabled(
                    !running,
                    egui::Button::new(RichText::new("开始接收").size(15.0).color(Color32::WHITE))
                        .fill(GREEN)
                        .rounding(Rounding::same(12.0))
                        .min_size(egui::vec2(150.0, 40.0)),
                );
                if start.clicked() {
                    self.start_engine();
                }
                if running {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("停止").size(15.0).color(RED))
                                .fill(GRAY_SOFT)
                                .rounding(Rounding::same(12.0))
                                .min_size(egui::vec2(110.0, 40.0)),
                        )
                        .clicked()
                    {
                        self.stop_engine();
                    }
                }
            });
            ui.add_space(4.0);
            let mut config_text = self.config_path.display().to_string();
            if config_text.chars().count() > 62 {
                let tail: String = config_text
                    .chars()
                    .skip(config_text.chars().count() - 59)
                    .collect();
                config_text = format!("…{}", tail);
            }
            ui.label(
                RichText::new(format!("配置文件：{}", config_text))
                    .size(11.0)
                    .color(SUBTLE),
            );
        });
    }

    fn log_card(&mut self, ui: &mut egui::Ui) {
        card(ui, CARD, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("运行日志").size(16.0).strong().color(TEXT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{} 行", self.log_lines.len()))
                            .size(11.0)
                            .color(SUBTLE),
                    );
                });
            });
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .max_height(220.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.log_lines.is_empty() {
                        ui.label(
                            RichText::new(i18n::tr(
                                "还没有日志，点开始接收试试",
                                "no logs yet, press start",
                            ))
                            .size(12.0)
                            .color(SUBTLE),
                        );
                    }
                    for line in &self.log_lines {
                        ui.label(
                            RichText::new(line)
                                .monospace()
                                .size(12.0)
                                .color(SUBTLE),
                        );
                    }
                });
        });
    }
}

impl eframe::App for ReceiverApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(state) = self.state_rx.try_recv() {
            self.state = state;
        }
        while let Ok(line) = self.log_rx.try_recv() {
            self.log_lines.push(line);
        }
        if self.log_lines.len() > 500 {
            let overflow = self.log_lines.len() - 500;
            self.log_lines.drain(..overflow);
        }
        ctx.request_repaint_after(Duration::from_millis(250));

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG).inner_margin(Margin::same(18.0)))
            .show(ctx, |ui| {
                self.header(ui);
                self.status_card(ui);
                ui.add_space(10.0);
                self.settings_card(ui);
                ui.add_space(10.0);
                self.log_card(ui);
            });
    }
}

impl Drop for ReceiverApp {
    fn drop(&mut self) {
        if let Some(handle) = self.engine.take() {
            handle.stop.store(true, Ordering::Relaxed);
        }
    }
}

fn card<R>(ui: &mut egui::Ui, fill: Color32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::none()
        .fill(fill)
        .rounding(Rounding::same(16.0))
        .inner_margin(Margin::same(16.0))
        .show(ui, add)
        .inner
}

fn status_badge(ui: &mut egui::Ui, color: Color32, connected: bool) {
    let size = egui::vec2(36.0, 36.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let center = rect.center();
    if connected {
        ui.painter().circle_filled(center, 17.0, color);
        let stroke = egui::Stroke::new(2.6_f32, Color32::WHITE);
        let p1 = center + egui::vec2(-7.5, 0.5);
        let p2 = center + egui::vec2(-2.5, 6.0);
        let p3 = center + egui::vec2(8.0, -5.5);
        ui.painter().line_segment([p1, p2], stroke);
        ui.painter().line_segment([p2, p3], stroke);
    } else {
        ui.painter().circle_stroke(center, 15.0, egui::Stroke::new(2.0_f32, color));
        ui.painter().circle_filled(center, 5.0, color);
    }
}

fn setup_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = BG;
    visuals.window_fill = CARD;
    visuals.override_text_color = Some(TEXT);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, TEXT);
    visuals.widgets.inactive.bg_fill = FIELD;
    visuals.widgets.inactive.weak_bg_fill = FIELD;
    visuals.widgets.inactive.bg_stroke =
        egui::Stroke::new(1.0_f32, Color32::from_rgb(0xDF, 0xE3, 0xEA));
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0xE3, 0xE8, 0xEF);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0xE3, 0xE8, 0xEF);
    visuals.widgets.active.bg_fill = Color32::from_rgb(0xD8, 0xE0, 0xEA);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(0xD8, 0xE0, 0xEA);
    visuals.widgets.inactive.rounding = Rounding::same(10.0);
    visuals.widgets.hovered.rounding = Rounding::same(10.0);
    visuals.widgets.active.rounding = Rounding::same(10.0);
    visuals.window_rounding = Rounding::same(16.0);
    style.visuals = visuals;
    style.spacing.item_spacing = egui::vec2(9.0, 9.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    ctx.set_style(style);
}

fn setup_fonts(ctx: &egui::Context) {
    // Windows 自带中文字体，挑一个能用的
    let candidates = [
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/Deng.ttf",
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simsun.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "cjk".to_owned(),
                egui::FontData::from_owned(bytes),
            );
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                family.insert(0, "cjk".to_owned());
            }
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                family.push("cjk".to_owned());
            }
            ctx.set_fonts(fonts);
            return;
        }
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

// ---------------- 开机自启（启动文件夹里的一个 vbs） ----------------

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
        io::Error::new(
            io::ErrorKind::NotFound,
            "APPDATA environment variable is missing",
        )
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

// ---------------- 选择文件夹（Win32 原生对话框） ----------------

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
        // BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX
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