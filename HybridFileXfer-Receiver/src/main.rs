//! 命令行版接收器（无界面），适合开机脚本或自动化。
//!
//! GUI 版见 `HybridFileXfer-Receiver.exe`。

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use hfx_receiver::config::{self, Config};
use hfx_receiver::engine::{self, ConnectMode, EngineState};
use hfx_receiver::i18n::{self, Lang};
use hfx_receiver::log;

#[derive(Default)]
struct Args {
    help: bool,
    verbose: bool,
    print_config: bool,
    receive_dir: Option<PathBuf>,
    port: Option<u16>,
    device: Option<String>,
    adb: Option<PathBuf>,
    component: Option<String>,
    ip: Option<IpAddr>,
    discover: bool,
    lang: Option<Lang>,
}

fn main() {
    enable_utf8_console();
    std::process::exit(run());
}

fn run() -> i32 {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{}", message);
            print_usage();
            return 2;
        }
    };
    if args.help {
        print_usage();
        return 0;
    }

    let config_path = config::config_path(&exe_dir);
    let mut cfg = Config::load(&config_path);
    let lang = args.lang.or(cfg.lang).unwrap_or_else(detect_lang);
    i18n::set_lang(lang);

    let mut must_save = false;
    if let Some(dir) = args.receive_dir.clone() {
        cfg.receive_dir = dir;
        must_save = true;
    }
    if let Some(port) = args.port {
        cfg.port = port;
        must_save = true;
    }
    if let Some(device) = args.device.clone() {
        cfg.device = Some(device);
        must_save = true;
    }
    if let Some(adb) = args.adb.clone() {
        cfg.adb_path = Some(adb);
        must_save = true;
    }
    if let Some(component) = args.component.clone() {
        cfg.service_component = component;
        must_save = true;
    }
    if let Some(lang) = args.lang {
        cfg.lang = Some(lang);
        must_save = true;
    }

    cfg.receive_dir = absolute(&cfg.receive_dir);
    if let Err(e) = std::fs::create_dir_all(&cfg.receive_dir) {
        eprintln!(
            "{} {}: {}",
            i18n::tr("无法创建接收文件夹", "cannot create receive folder"),
            cfg.receive_dir.display(),
            e
        );
        return 1;
    }

    if args.print_config {
        println!("{}", config_path.display());
        print!("{}", cfg.describe());
        return 0;
    }
    if must_save {
        if let Err(e) = cfg.save(&config_path) {
            eprintln!(
                "{} {}: {}",
                i18n::tr("无法保存配置文件", "cannot save config file"),
                config_path.display(),
                e
            );
        }
    }

    log::init(Some(exe_dir.join("receiver.log")), args.verbose);
    print_banner(&cfg, &config_path, &exe_dir, args.ip);

    let mode = if args.discover {
        ConnectMode::Discovery
    } else {
        match args.ip {
            Some(ip) => ConnectMode::Ip(ip),
            None => ConnectMode::Auto,
        }
    };
    let options = engine::options_from_config(&cfg, mode, &exe_dir);
    let stop = Arc::new(AtomicBool::new(false));
    engine::run(options, stop, Arc::new(|state: EngineState| {
        if let EngineState::Retrying(message) = state {
            let _ = message;
        }
    }));
    0
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn print_banner(cfg: &Config, config_path: &Path, exe_dir: &Path, ip: Option<IpAddr>) {
    let separator = "=".repeat(64);
    println!("{}", separator);
    println!(
        " {}{}",
        i18n::tr("多轨快传 · 电脑接收器 v", "HybridFileXfer · PC receiver v"),
        env!("CARGO_PKG_VERSION")
    );
    println!(
        " {}{}",
        i18n::tr("接收文件夹：", "receive folder: "),
        cfg.receive_dir.display()
    );
    println!(
        " {}{}",
        i18n::tr("配置文件：  ", "config file:    "),
        config_path.display()
    );
    println!(
        " {}{}",
        i18n::tr("日志文件：  ", "log file:       "),
        exe_dir.join("receiver.log").display()
    );
    match ip {
        Some(ip) => println!(
            " {}{}:{}",
            i18n::tr("模式：      直接连接 ", "mode:          connect to "),
            ip,
            cfg.port
        ),
        None => println!(
            " {}",
            i18n::tr(
                "模式：      自动检测 USB 设备（adb）",
                "mode:          auto detect USB device (adb)"
            )
        ),
    }
    println!("{}", separator);
    println!();
}

fn parse_args<I: Iterator<Item = String>>(mut it: I) -> Result<Args, String> {
    let mut args = Args::default();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => args.help = true,
            "-v" | "--verbose" => args.verbose = true,
            "--print-config" => args.print_config = true,
            "-d" | "--receive-dir" | "--dir" => {
                args.receive_dir = Some(PathBuf::from(next_value(&mut it, &arg)?));
            }
            "-p" | "--port" => {
                let value = next_value(&mut it, &arg)?;
                args.port = Some(
                    value
                        .parse::<u16>()
                        .map_err(|_| format!("invalid port: {}", value))?,
                );
            }
            "-s" | "--device" | "--serial" => {
                args.device = Some(next_value(&mut it, &arg)?);
            }
            "--adb" => {
                args.adb = Some(PathBuf::from(next_value(&mut it, &arg)?));
            }
            "--component" => {
                args.component = Some(next_value(&mut it, &arg)?);
            }
            "-c" | "--ip" | "--connect" => {
                let value = next_value(&mut it, &arg)?;
                if value.eq_ignore_ascii_case("adb") {
                    args.ip = None;
                } else if value.eq_ignore_ascii_case("discover") || value.eq_ignore_ascii_case("lan") {
                    args.discover = true;
                } else {
                    args.ip = Some(
                        value
                            .parse::<IpAddr>()
                            .map_err(|_| format!("invalid IP address: {}", value))?,
                    );
                }
            }
            "--lang" => {
                let value = next_value(&mut it, &arg)?;
                args.lang = Some(match value.to_ascii_lowercase().as_str() {
                    "zh" | "cn" | "chinese" => Lang::Zh,
                    "en" | "english" => Lang::En,
                    _ => return Err(format!("invalid language: {}", value)),
                });
            }
            other => return Err(format!("unknown argument: {}", other)),
        }
    }
    Ok(args)
}

fn next_value<I: Iterator<Item = String>>(it: &mut I, name: &str) -> Result<String, String> {
    it.next()
        .ok_or_else(|| format!("{} needs a value", name))
}

fn print_usage() {
    println!(
        "{}",
        concat!(
            "HybridFileXfer receiver (command line) / 多轨快传 电脑接收器（命令行版）\n",
            "\n",
            "用法 / usage:\n",
            "  HybridFileXfer-Receiver-cli.exe                自动检测 USB 手机 (adb)\n",
            "  HybridFileXfer-Receiver-cli.exe -c 192.168.1.5  直接连接指定 IP\n",
            "  HybridFileXfer-Receiver-cli.exe -d D:\\接收     设置并保存接收文件夹\n",
            "\n",
            "参数 / options:\n",
            "  -d, --receive-dir <dir>   接收文件夹（保存到 config.ini）\n",
            "  -p, --port <port>         控制通道端口，默认 5740\n",
            "  -s, --device <serial>     只连接指定设备\n",
            "      --adb <path>          adb.exe 路径\n",
            "  -c, --ip <ip|adb|discover>  直接连接 IP / adb（默认自动）/ 只用局域网发现\n",
            "      --component <name>    手机上要启动的组件\n",
            "      --lang <zh|en>        日志语言\n",
            "      --print-config        打印配置后退出\n",
            "  -v, --verbose             输出调试信息\n",
            "  -h, --help                显示帮助\n",
        )
    );
}

fn detect_lang() -> Lang {
    if let Ok(lang) = std::env::var("HFX_LANG") {
        let lang = lang.to_ascii_lowercase();
        if lang.starts_with("zh") {
            return Lang::Zh;
        }
        if lang.starts_with("en") {
            return Lang::En;
        }
    }
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

#[cfg(windows)]
fn enable_utf8_console() {
    extern "system" {
        fn SetConsoleOutputCP(cp: u32) -> i32;
        fn SetConsoleCP(cp: u32) -> i32;
    }
    unsafe {
        SetConsoleOutputCP(65001);
        SetConsoleCP(65001);
    }
}

#[cfg(not(windows))]
fn enable_utf8_console() {}