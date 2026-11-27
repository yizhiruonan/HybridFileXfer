//! 局域网文件夹浏览接口，供手机端选择电脑接收文件夹。
//!
//! 手机端通过 HTTP 访问：
//! - `GET /list?path=<目录>` 列出该目录下的子文件夹（path 为空时用当前接收目录）
//! - `GET /set?path=<目录>`  把接收目录切换到指定目录（不存在会自动创建）
//!
//! 只监听局域网，**没有鉴权**；请只在可信网络里使用。
//!
//! TODO(配对)：这里没有任何身份验证，靠「来源 IP 白名单」兜着（见 allow_peer）。
//! 这是 3.7.1 的低风险缓解，不是最终方案 —— 等配对功能落地后改成 token 鉴权，
//! 别在这套 IP 白名单上继续加东西。
//!
//! 为什么 3.7.1 要先加白名单：这两个接口是**零认证**的，同一个 Wi-Fi 下任何人都能
//! 调 `/set` 把接收目录指到 `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup`，
//! 再走 5740（同样零认证）推一个文件进去，用户重启就执行了。而 5740 和 5742 的后果
//! 是同一个入口的两个面，所以两边都要收口。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread;

use crate::log;
use crate::receiver::SharedDir;

/// 手机端访问的端口。
pub const WEB_PORT: u16 = 5742;

static DIR: OnceLock<SharedDir> = OnceLock::new();

/// 允许访问 5742 的手机来源 IP。
///
/// 只放行「刚才跟我们建立过控制连接（5740）的那台手机」—— 手机端浏览电脑文件夹
/// 用的就是同一张网卡上的 IP（见手机端 Dialogs.kt 的 peerIp，它来自控制连接的对端），
/// 所以正常功能不受影响；同一 Wi-Fi 下的其他人没有控制连接，一律 403。
///
/// 这是 IP 白名单式的缓解（局域网里 IP 可以被冒充，挡不住有意为之的攻击者），
/// 最终方案是配对 + token，见文件头的 TODO。
static ALLOWED: OnceLock<Mutex<HashMap<IpAddr, usize>>> = OnceLock::new();

fn allowed() -> &'static Mutex<HashMap<IpAddr, usize>> {
    ALLOWED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 控制连接建立时登记对端 IP（由 session 在握手成功后调用）。
///
/// 记的是**计数**而不是布尔：同一个 IP 上可能同时有两条会话（手机端重连时新的
/// 先建好、旧的才销毁），用布尔的话后销毁的那条会把刚登记的那条一起删掉，
/// 于是手机浏览文件夹突然 403 —— 一个只在重连瞬间出现、极难复现的坏 bug。
pub fn allow_peer(ip: IpAddr) {
    if ip.is_loopback() {
        // 回环（USB_ADB 转发 / 本机调试）本来就在本机，不用登记
        return;
    }
    if let Ok(mut map) = allowed().lock() {
        let count = map.entry(ip).or_insert(0);
        *count += 1;
        if *count == 1 {
            log::info(&format!("文件夹浏览接口：允许 {} 访问", ip));
        }
    }
}

/// 会话结束时撤销登记：手机断开之后不该再留着这个口子。
pub fn forget_peer(ip: IpAddr) {
    if ip.is_loopback() {
        return;
    }
    if let Ok(mut map) = allowed().lock() {
        if let Some(count) = map.get_mut(&ip) {
            *count -= 1;
            if *count == 0 {
                map.remove(&ip);
            }
        }
    }
}

fn is_allowed(ip: IpAddr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    match allowed().lock() {
        Ok(map) => map.get(&ip).is_some_and(|count| *count > 0),
        // 锁中毒：宁可不放行
        Err(_) => false,
    }
}

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
        // 这条要显眼：接口以前是悄悄开着的，用户根本不知道自己的电脑在局域网上
        // 开了一个没有鉴权的 HTTP 服务（能列目录、能改接收目录）。
        log::warn(&format!(
            "文件夹浏览接口 5742 已对局域网开放（无鉴权，仅接受已连上的手机），仅限可信网络：http://<电脑IP>:{}",
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
    // 来源检查放在最前面：连请求行都不读，避免被当成可探测的接口。
    //
    // 为什么要有这一步：5742 绑在 0.0.0.0 上、没有鉴权，/set 还能改接收目录 ——
    // 同一 Wi-Fi 下任何人都能把接收目录指到自启动目录，再走 5740 推文件进去。
    // 只放行「刚才真的跟我们建立过控制连接的那台手机」。
    let peer = stream.peer_addr().map(|addr| addr.ip()).ok();
    match peer {
        Some(ip) if is_allowed(ip) => {}
        other => {
            log::warn(&format!(
                "文件夹浏览接口拒绝了来自 {} 的请求（没有与之对应的手机会话）",
                other
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|| "未知地址".to_string())
            ));
            let body = "E\tforbidden";
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.as_bytes().len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            return Ok(());
        }
    }
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
            Some(value) if !value.trim().is_empty() => match sanitize_dir(&value) {
                Ok(new_dir) => match std::fs::create_dir_all(&new_dir) {
                    Ok(_) => {
                        dir.set(new_dir.clone());
                        log::info(&format!("手机端切换接收目录：{}", new_dir.display()));
                        ("200 OK", format!("OK\t{}", new_dir.display()))
                    }
                    Err(e) => ("500 Internal Server Error", format!("E\t{}", e)),
                },
                Err(reason) => {
                    log::warn(&format!(
                        "文件夹浏览接口拒绝了接收目录 {}：{}",
                        value, reason
                    ));
                    ("400 Bad Request", format!("E\t{}", reason))
                }
            },
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

/// 校验 /set 传来的接收目录。**这是安全边界，不是格式校验，别放宽。**
///
/// `/set` 能把接收目录改到任意位置，而接收目录会被用来落盘手机推来的任意文件，
/// 所以「能把目录设到哪」等于「能往哪写」。三类明确拒绝：
///
///  1. 含 `..` 或相对路径 —— 那条路能绕开下面所有按前缀做的判断；
///  2. 盘符根（`C:\`）、系统目录（`%WINDIR%` 及其子目录）、Program Files ——
///     落到那里就不是「用户的接收目录」了；
///  3. 自启动目录（`...\Start Menu\Programs\Startup`）—— 写进去等于让文件开机执行，
///     这是这条接口最直接的危害，单独点名。
///
/// 说明：这里只做词法判断（不 canonicalize）。/set 是「选一个还不存在的目录」的正常
/// 用法，canonicalize 会失败；而一旦目录被设到奇怪的地方，落盘那一刻还有
/// `Receiver::ensure_inside` 兜底。两道一起用才够。
fn sanitize_dir(value: &str) -> Result<PathBuf, String> {
    let raw = Path::new(value.trim());
    if !raw.is_absolute() {
        return Err("需要绝对路径".to_string());
    }
    let mut cleaned = PathBuf::new();
    for component in raw.components() {
        match component {
            // 盘符 / 根：绝对路径本来就该有，照抄
            Component::Prefix(prefix) => cleaned.push(prefix.as_os_str()),
            Component::RootDir => cleaned.push(Component::RootDir.as_os_str()),
            Component::Normal(name) => cleaned.push(name),
            // 剩下的 CurDir / ParentDir 一律拒绝：`..` 能一层层爬到任何地方，
            // 后面那些「在不在系统目录里」的前缀判断就全都失效了
            Component::CurDir | Component::ParentDir => {
                return Err("路径里不能有 . 或 ..".to_string())
            }
        }
    }
    if cleaned.components().next_back().is_none() {
        return Err("无效路径".to_string());
    }
    let text = cleaned.to_string_lossy().to_lowercase();
    // 盘符根（C: 或 C:）只有一个 Prefix+RootDir，长度判断最直接
    let depth = cleaned
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    if depth == 0 {
        return Err("不能把接收目录设成盘符根目录".to_string());
    }
    let mut forbidden: Vec<String> = Vec::new();
    for key in ["SystemRoot", "windir"] {
        if let Ok(value) = std::env::var(key) {
            forbidden.push(value.to_lowercase());
        }
    }
    for key in ["ProgramFiles", "ProgramFiles(x86)", "ProgramData"] {
        if let Ok(value) = std::env::var(key) {
            forbidden.push(value.to_lowercase());
        }
    }
    for dir in forbidden {
        let dir = dir.trim_end_matches(['\\', '/']).to_string();
        if !dir.is_empty() && (text == dir || text.starts_with(&format!("{dir}\\")) || text.starts_with(&format!("{dir}/"))) {
            return Err("不能把接收目录设到系统目录".to_string());
        }
    }
    // 自启动目录：写进去等于开机执行，单独判一次（用户自己建的 Startup 目录也拦，
    // 因为名字一样、行为一样，没法区分用户意图）
    let lower = cleaned.to_string_lossy().to_lowercase();
    if lower.contains("start menu\\programs\\startup") || lower.contains("start menu/programs/startup") {
        return Err("不能把接收目录设到自启动目录".to_string());
    }
    Ok(cleaned)
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
