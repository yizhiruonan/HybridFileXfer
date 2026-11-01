//! Persistent configuration stored next to the executable (`config.ini`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::i18n::Lang;

pub const DEFAULT_PORT: u16 = 5740;
pub const DEFAULT_COMPONENT: &str = "top.weixiansen574.hybridfilexfer/.ServerService";
const FILE_NAME: &str = "config.ini";

#[derive(Clone, Debug)]
pub struct Config {
    /// Folder that received files are written to (also reported to the phone as
    /// the remote "home directory").
    pub receive_dir: PathBuf,
    pub port: u16,
    /// Path of adb.exe, `None` means auto detect.
    pub adb_path: Option<PathBuf>,
    /// Only connect to this device serial, `None` means "the only / first one".
    pub device: Option<String>,
    /// Component started on the phone so that it starts listening for us.
    pub service_component: String,
    pub lang: Option<Lang>,
    /// 关闭窗口时是否最小化到系统托盘（后台继续等待连接）
    pub minimize_to_tray: bool,
    /// 同时连接的手机数量上限（多设备）
    pub max_devices: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            receive_dir: default_receive_dir(),
            port: DEFAULT_PORT,
            adb_path: None,
            device: None,
            service_component: DEFAULT_COMPONENT.to_string(),
            lang: None,
            minimize_to_tray: true,
            max_devices: 3,
        }
    }
}

pub fn default_receive_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    match home {
        Some(home) => home.join("Downloads").join("HybridFileXfer"),
        None => PathBuf::from("HybridFileXfer"),
    }
}

pub fn config_path(exe_dir: &Path) -> PathBuf {
    exe_dir.join(FILE_NAME)
}

impl Config {
    pub fn load(path: &Path) -> Config {
        let mut cfg = Config::default();
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => return cfg,
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "receive_dir" | "receive-dir" | "home_dir" => {
                    if !value.is_empty() {
                        cfg.receive_dir = PathBuf::from(value);
                    }
                }
                "port" => {
                    if let Ok(port) = value.parse::<u16>() {
                        cfg.port = port;
                    }
                }
                "adb" | "adb_path" => {
                    cfg.adb_path = if value.is_empty() {
                        None
                    } else {
                        Some(PathBuf::from(value))
                    }
                }
                "device" | "serial" => {
                    cfg.device = if value.is_empty() {
                        None
                    } else {
                        Some(value.to_string())
                    }
                }
                "service_component" | "component" => {
                    if !value.is_empty() {
                        cfg.service_component = value.to_string();
                    }
                }
                "lang" | "language" => {
                    cfg.lang = match value.to_ascii_lowercase().as_str() {
                        "zh" | "zh-cn" | "cn" | "chinese" => Some(Lang::Zh),
                        "en" | "english" => Some(Lang::En),
                        _ => None,
                    };
                }
                "max_devices" | "devices" => {
                    if let Ok(count) = value.parse::<usize>() {
                        cfg.max_devices = count.clamp(1, 8);
                    }
                }
                "minimize_to_tray" | "tray" => {
                    cfg.minimize_to_tray = !(value == "0" || value.eq_ignore_ascii_case("false"));
                }
                _ => {}
            }
        }
        cfg
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let text = format!(
            "# HybridFileXfer 接收器配置 / receiver settings\n\
             # 接收文件保存的文件夹（也是手机上显示的电脑主目录）\n\
             receive_dir = {}\n\
             # 控制通道端口，需与手机端一致\n\
             port = {}\n\
             # adb 路径，留空自动查找（程序目录、HybridFileXfer-PC 目录、PATH）\n\
             adb = {}\n\
             # 只连接指定设备序列号，留空表示任意一台\n\
             device = {}\n\
             # 在手机上启动的组件（接收服务）\n\
             service_component = {}\n\
             # zh / en，留空跟随系统\n\
             lang = {}\n\
             # 关闭窗口时最小化到托盘（1 开启 / 0 关闭）\n\
             minimize_to_tray = {}\n\
             # 同时连接的手机数量上限（1-8）\n\
             max_devices = {}\n",
            self.receive_dir.display(),
            self.port,
            self.adb_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            self.device.clone().unwrap_or_default(),
            self.service_component,
            match self.lang {
                Some(Lang::Zh) => "zh",
                Some(Lang::En) => "en",
                None => "",
            },
            if self.minimize_to_tray { "1" } else { "0" },
            self.max_devices
        );
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(path, text)
    }

    pub fn describe(&self) -> String {
        format!(
            "receive_dir = {}\nport = {}\nadb = {}\ndevice = {}\nservice_component = {}\n",
            self.receive_dir.display(),
            self.port,
            self.adb_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(auto)".to_string()),
            self.device.clone().unwrap_or_else(|| "(any)".to_string()),
            self.service_component
        )
    }
}
