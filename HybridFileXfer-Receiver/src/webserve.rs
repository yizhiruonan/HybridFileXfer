//! 局域网文件夹浏览接口，供手机端选择电脑接收文件夹。
//!
//! 手机端通过 HTTP 访问：
//! - `GET /list?path=<目录>` 列出该目录下的子文件夹（path 为空时用当前接收目录）
//! - `GET /set?path=<目录>`  把接收目录切换到指定目录（不存在会自动创建）
//!
//! 只监听局域网，没有鉴权；请只在可信网络里使用。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread;

use crate::log;
use crate::receiver::SharedDir;

/// 手机端访问的端口。
pub const WEB_PORT: u16 = 5742;

static DIR: OnceLock<SharedDir> = OnceLock::new();

/// 初始化并启动浏览接口（重复调用只启动一次）。
pub fn init(dir: &PathBuf) {
    if DIR.get().is_some() {
        return;
    }
    let shared = SharedDir::new(dir.clone());
    if DIR.set(shared.clone()).is_err() {
        return;
    }
    thread::spawn(move || {
        let listener = match TcpListener::bind(("0.0.0.0", WEB_PORT)) {
            Ok(listener) => listener,
            Err(e) => {
                log::warn(&format!("文件夹浏览接口启动失败（端口 {}）：{}", WEB_PORT, e));
                return;
            }
        };
        log::info(&format!(
            "文件夹浏览接口已启动：http://<电脑IP>:{}",
            WEB_PORT
        ));
        for stream in listener.incoming() {
            if let Ok(stream) = stream {
                let dir = shared.clone();
                thread::spawn(move || {
                    let _ = handle(stream, &dir);
                });
            }
        }
    });
}

/// 当前的接收目录（手机端 /list 不带 path 时使用）。
pub fn current_dir() -> SharedDir {
    DIR.get_or_init(|| SharedDir::new(PathBuf::from("."))).clone()
}

fn handle(mut stream: TcpStream, dir: &SharedDir) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    if reader.read_line(&mut request)? == 0 {
        return Ok(());
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
    }
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    let param = |key: &str| {
        query
            .split('&')
            .find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| url_decode(v)))
    };
    let (status, body) = match (method, path) {
        ("GET", "/list") => {
            let base = param("path")
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| dir.get().to_string_lossy().to_string());
            match list_dirs(&base) {
                Ok(text) => ("200 OK", format!("P\t{}\n{}", base, text)),
                Err(e) => ("500 Internal Server Error", format!("E\t{}", e)),
            }
        }
        ("GET", "/set") => match param("path") {
            Some(value) if !value.trim().is_empty() => {
                let new_dir = PathBuf::from(&value);
                match std::fs::create_dir_all(&new_dir) {
                    Ok(_) => {
                        dir.set(new_dir.clone());
                        log::info(&format!("手机端切换接收目录：{}", new_dir.display()));
                        ("200 OK", format!("OK\t{}", new_dir.display()))
                    }
                    Err(e) => ("500 Internal Server Error", format!("E\t{}", e)),
                }
            }
            _ => ("400 Bad Request", "E\tmissing path".to_string()),
        },
        _ => ("404 Not Found", "E\tnot found".to_string()),
    };
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.as_bytes().len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn list_dirs(base: &str) -> std::io::Result<String> {
    let mut out = String::new();
    let entries = std::fs::read_dir(base)?;
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.path().is_dir() {
            dirs.push(name);
        } else {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();
    for dir in dirs {
        out.push_str("D\t");
        out.push_str(&dir);
        out.push('\n');
    }
    for file in files {
        out.push_str("F\t");
        out.push_str(&file);
        out.push('\n');
    }
    Ok(out)
}

fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = &input[i + 1..i + 3];
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    out.push(value);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}
