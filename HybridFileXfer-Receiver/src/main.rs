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
    /// 307：本机限速（MB/s），0 = 不限速。和手机端互相协商，取非零里较小的一个。
    speed_limit_mb: Option<u32>,
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
    // 限速的运行期状态**不能**在这里灌：命令行参数在下面才覆盖 cfg，提前灌读到的是
    // 配置文件里的旧值（首次运行时配置文件不存在，load 出来就是默认 0 = 不限速）。
    // 灌值那一步移到了参数处理之后，见下面 set_configured_mb 那一段。
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
    if let Some(mb) = args.speed_limit_mb {
        cfg.speed_limit_mb = mb.min(1000);
        must_save = true;
    }

    // 307：命令行版本同样要参与限速协商 —— 握手时把自己的上限报给手机。不灌的话
    // configured_mb() 永远是 0，CLI 跑起来就变成「本机不限速」，电脑自己那条发送
    // 路径完全不限，和图形版行为不一致。
    //
    // 位置很关键：必须在命令行参数**覆盖完 cfg 之后**才灌。以前这一句紧跟
    // Config::load，而 --speed-limit 是后面才写进 cfg 的，于是首次运行（配置文件
    // 还不存在、load 出来是默认 0 = 不限速）灌进去的就是 0：命令行那个值只进了
    // cfg 和配置文件，运行期一直不限速，要等第二次运行从文件里读到才生效。
    // 用户报的「--speed-limit 首次不生效、第二次才生效」就是这个顺序问题。
    hfx_receiver::ratelimit::set_configured_mb(cfg.speed_limit_mb as i64);
    // 限速「以为设了其实没设」是这个参数最容易踩的坑，来源要跟着值一起打出去
    let limit_from_cli = args.speed_limit_mb.is_some();

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
    // 命令行版虽然能看见 stderr，但开机脚本里那点输出没人看；落盘才查得到
    log::install_panic_hook();
    print_banner(&cfg, &config_path, &exe_dir, args.ip, limit_from_cli);

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

fn print_banner(
    cfg: &Config,
    config_path: &Path,
    exe_dir: &Path,
    ip: Option<IpAddr>,
    limit_from_cli: bool,
) {
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
    // 限速值放在最后一行：它是「设了没生效」最爱出问题的一项，紧挨着分隔线最好找。
    // 顺手写进日志 —— 来源（命令行 / 配置文件）比数值本身更关键：
    // 「--speed-limit 首次运行不生效」那类问题看这一行就能定位。
    let limit_text = if cfg.speed_limit_mb == 0 {
        i18n::tr("不限速", "unlimited").to_string()
    } else {
        format!("{} MB/s", cfg.speed_limit_mb)
    };
    let limit_line = format!(
        "{}{}（{}）",
        i18n::tr("限速：", "speed limit: "),
        limit_text,
        if limit_from_cli {
            i18n::tr("来自命令行", "from command line")
        } else {
            i18n::tr("来自配置文件", "from config file")
        }
    );
    log::info(&limit_line);
    println!(" {}", limit_line);
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
            "--speed-limit" | "--limit" | "--speed" => {
                let value = next_value(&mut it, &arg)?;
                // 0 是合法值（= 不限速），所以不能用 next_value 里那套「空串 = 缺值」的判断。
                args.speed_limit_mb = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid speed limit: {}", value))?
                        .min(1000),
                );
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
            "      --speed-limit <mbps>  限速（MB/s，0 = 不限速），与手机端取较小值\n",
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