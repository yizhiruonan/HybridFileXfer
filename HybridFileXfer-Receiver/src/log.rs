//! Console + rolling file log with local timestamps.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);
/// 日志文件路径。panic 钩子要靠它**另开一个句柄**写日志（见 panic_emit）。
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static VERBOSE: AtomicBool = AtomicBool::new(false);
static UI_SINK: OnceLock<Mutex<Option<Sender<String>>>> = OnceLock::new();

fn ui_sink() -> &'static Mutex<Option<Sender<String>>> {
    UI_SINK.get_or_init(|| Mutex::new(None))
}

/// 让界面也能收到日志行（GUI 用）。
pub fn set_ui_sink(sender: Sender<String>) {
    *ui_sink().lock().unwrap() = Some(sender);
}

/// 要不要同时打印到控制台（GUI 模式下不需要）。
pub fn set_console(enabled: bool) {
    CONSOLE.store(enabled, Ordering::Relaxed);
}

static CONSOLE: AtomicBool = AtomicBool::new(true);

pub fn init(file: Option<PathBuf>, verbose: bool) {
    VERBOSE.store(verbose, Ordering::Relaxed);
    if let Some(path) = file {
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                let _ = LOG_PATH.set(path.clone());
                *LOG_FILE.lock().unwrap() = Some(f);
                info(&format!(
                    "==== HybridFileXfer receiver started, log file: {} ====",
                    path.display()
                ));
            }
            Err(e) => eprintln!("cannot open log file {}: {}", path.display(), e),
        }
    }
}

pub fn info(msg: &str) {
    emit('I', msg, true);
}

pub fn warn(msg: &str) {
    emit('W', msg, true);
}

pub fn error(msg: &str) {
    emit('E', msg, true);
}

/// Console only progress line (not written to the log file).
pub fn progress(msg: &str) {
    let (local_ts, _) = crate::util::timestamps();
    println!("[{}] {}", local_ts, msg);
}

pub fn debug(msg: &str) {
    if VERBOSE.load(Ordering::Relaxed) {
        emit('D', msg, false);
    }
}

/// 装一个 panic 钩子：把 panic 的**位置、消息、线程名和栈**写进日志文件。
///
/// 为什么需要：GUI 是 Windows 子系统程序，标准错误没有控制台可去 —— 一个线程里的
/// panic 默认只往 stderr 打一行，然后静默消失。release 又是 opt-level=3 + lto + strip，
/// 连 Windows 事件日志里都不会留痕迹（那是段错误才有的），用户看到的就是「程序自己
/// 关了、什么都没有」。实测出现过「传输到一半窗口消失、日志停在上一行」正是这样。
///
/// 所以这里把 panic 记成 [E] 级日志（每行都 flush），落盘为证；再调链上原来的钩子，
/// 保持控制台调试时的默认行为不变。
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // 位置单独拼：PanicInfo 的 Display 在不同版本里变过，Location 是稳的
        let place = match info.location() {
            Some(l) => format!("{}:{}:{}", l.file(), l.line(), l.column()),
            None => "?".to_string(),
        };
        // 线程名能区分「UI 线程炸了」和「只是某个传输线程炸了」——前者进程直接没了，
        // 后者进程还在。这是排查时最先要看的一个信息。
        let who = std::thread::current()
            .name()
            .map(|n| n.to_string())
            .unwrap_or_else(|| format!("{:?}", std::thread::current().id()));
        let message = payload_text(info.payload());
        // force_capture 不依赖 RUST_BACKTRACE，用户机器上默认也能拿到
        let backtrace = std::backtrace::Backtrace::force_capture();
        panic_emit(&format!("线程 {} 发生 panic：{}（{}）", who, message, place));
        for line in format!("{backtrace}").lines() {
            panic_emit(&format!("  panic 栈：{line}"));
        }
        previous(info);
    }));
}

/// panic 时专用的日志写入：**不取 LOG_FILE 锁**。
///
/// 为什么要单独一条路：`emit()` 全程持着 `LOG_FILE` 的 `Mutex`。如果 panic 就发生在
/// 持锁的这一刻（写日志途中 panic，或者 emit 里 println 又炸了），钩子再进 `emit()`
/// 就是对同一把**不可重入**的标准库 Mutex 二次上锁 —— 当场死锁，钩子等于没有：
/// 日志里一个字都不留，进程就停在那儿（正是「窗口消失、日志停在上一行」的样子）。
///
/// 所以这里另开一个文件句柄追加（append 模式下多个句柄同时写也不会互相截断），
/// 完全绕开那把锁；stderr 也是直写，不经过任何共享状态。
fn panic_emit(msg: &str) {
    let (local_ts, utc_ts) = crate::util::timestamps();
    let mut err = std::io::stderr();
    let _ = writeln!(err, "[{}] [E] {}", local_ts, msg);
    if let Some(path) = LOG_PATH.get() {
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "[{}] [E] {}", utc_ts, msg);
            let _ = file.flush();
        }
    }
}

/// panic 的 payload 正常是 &str / String；别的一律用类型名顶上，不能在这里再 panic
fn payload_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return s.to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<非字符串 payload>".to_string()
}

fn emit(level: char, msg: &str, console: bool) {
    let (local_ts, utc_ts) = crate::util::timestamps();
    if console && CONSOLE.load(Ordering::Relaxed) {
        match level {
            'E' => eprintln!("[{}] {}", local_ts, msg),
            _ => println!("[{}] {}", local_ts, msg),
        }
    }
    if let Ok(mut guard) = LOG_FILE.lock() {
        if let Some(file) = guard.as_mut() {
            let _ = writeln!(file, "[{}] [{}] {}", utc_ts, level, msg);
            let _ = file.flush();
        }
    }
    if let Ok(guard) = ui_sink().lock() {
        if let Some(sender) = guard.as_ref() {
            let _ = sender.send(format!("[{}] {}", local_ts, msg));
        }
    }
}