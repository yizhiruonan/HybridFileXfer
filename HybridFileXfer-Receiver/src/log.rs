//! Console + rolling file log with local timestamps.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);
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