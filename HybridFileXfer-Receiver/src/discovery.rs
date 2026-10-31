//! 局域网发现：手机端会周期广播 `HFX1 <port> <name>`，这里负责接收。
//!
//! 有了它，电脑不需要 adb、也不需要手输 IP，就能找到同一 WIFI 下的手机。

use std::net::{IpAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const DISCOVERY_PORT: u16 = 5741;
const MAGIC: &str = "HFX1";
const FRESH: Duration = Duration::from_secs(12);

#[derive(Clone, Debug)]
pub struct Peer {
    pub ip: IpAddr,
    pub port: u16,
    pub name: String,
    pub last_seen: Instant,
}

#[derive(Clone)]
pub struct Discovery {
    peers: Arc<Mutex<Vec<Peer>>>,
}

impl Discovery {
    /// 后台监听 UDP 广播；端口被占用时只打日志，不影响其它连接方式。
    pub fn start(port: u16) -> Discovery {
        let discovery = Discovery {
            peers: Arc::new(Mutex::new(Vec::new())),
        };
        let peers = Arc::clone(&discovery.peers);
        thread::spawn(move || {
            let socket = match UdpSocket::bind(("0.0.0.0", port)) {
                Ok(socket) => socket,
                Err(e) => {
                    crate::log::warn(&format!(
                        "局域网发现不可用（UDP {} 绑定失败）：{}",
                        port, e
                    ));
                    return;
                }
            };
            let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
            crate::log::debug(&format!("listening for discovery on udp {}", port));
            let mut buffer = [0u8; 512];
            loop {
                match socket.recv_from(&mut buffer) {
                    Ok((length, from)) => {
                        let text = String::from_utf8_lossy(&buffer[..length]);
                        let Some((peer_port, name)) = parse(&text) else {
                            continue;
                        };
                        let ip = from.ip();
                        if ip.is_loopback() {
                            continue;
                        }
                        let mut guard = match peers.lock() {
                            Ok(guard) => guard,
                            Err(_) => continue,
                        };
                        let now = Instant::now();
                        if let Some(existing) = guard.iter_mut().find(|p| p.ip == ip && p.port == peer_port) {
                            existing.last_seen = now;
                            existing.name = name;
                        } else {
                            crate::log::info(&format!(
                                "{} {}:{}（{}）",
                                crate::i18n::tr("局域网发现手机", "found phone on LAN"),
                                ip,
                                peer_port,
                                name
                            ));
                            guard.push(Peer {
                                ip,
                                port: peer_port,
                                name,
                                last_seen: now,
                            });
                        }
                    }
                    Err(_) => continue,
                }
            }
        });
        discovery
    }

    /// 最近 12 秒内还在广播的设备里，取最新的一台。
    pub fn best(&self) -> Option<Peer> {
        let guard = self.peers.lock().ok()?;
        let now = Instant::now();
        guard
            .iter()
            .filter(|peer| now.duration_since(peer.last_seen) < FRESH)
            .cloned()
            .max_by_key(|peer| peer.last_seen)
    }

    pub fn clear(&self) {
        if let Ok(mut guard) = self.peers.lock() {
            guard.clear();
        }
    }
}

fn parse(text: &str) -> Option<(u16, String)> {
    let mut parts = text.trim().splitn(3, ' ');
    if parts.next()? != MAGIC {
        return None;
    }
    let port = parts.next()?.parse::<u16>().ok()?;
    let name = parts.next().unwrap_or("android").to_string();
    Some((port, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_beacon() {
        assert_eq!(parse("HFX1 5740 Pixel_7"), Some((5740, "Pixel_7".to_string())));
        assert_eq!(parse("HFX1 5740"), Some((5740, "android".to_string())));
        assert_eq!(parse("HFX2 5740 x"), None);
        assert_eq!(parse("garbage"), None);
    }
}
