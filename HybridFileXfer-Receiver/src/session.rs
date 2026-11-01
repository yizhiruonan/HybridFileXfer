//! One connection to the phone: handshake, control channel and file receiving.

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::log;
use crate::proto::{self, controller, current_file_system, transfer, BLOCK_SIZE};
use crate::receiver::Receiver;
use crate::util;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(20);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct PeerInterface {
    pub name: String,
    pub address: IpAddr,
    pub bind: Option<IpAddr>,
}

pub struct Session {
    /// 本会话对应的手机串号（多设备时用于对待发送队列分流）
    pub device: String,
    reader: TcpStream,
    writer: TcpStream,
    transfers: Vec<(String, TcpStream)>,
    pub interfaces: Vec<PeerInterface>,
}

pub enum SessionEnd {
    /// The phone asked us to stop.
    Shutdown,
    /// Connection was closed or broke.
    Disconnected(String),
    /// 用户在本机点了"停止"
    Stopped,
}

impl Session {
    pub fn connect(
        address: IpAddr,
        port: u16,
        home_dir: &str,
        expected_interfaces: usize,
    ) -> io::Result<Session> {
        Session::connect_with_local_port(address, port, port, home_dir, expected_interfaces)
    }

    /// 多设备时每台手机有独立的 adb 转发端口：
    /// 回环地址（USB_ADB）用 local_port，其它网卡仍用手机监听的 port。
    pub fn connect_with_local_port(
        address: IpAddr,
        port: u16,
        local_port: u16,
        home_dir: &str,
        expected_interfaces: usize,
    ) -> io::Result<Session> {
        let control_port = if address.is_loopback() { local_port } else { port };
        let control = TcpStream::connect_timeout(
            &SocketAddr::new(address, control_port),
            CONNECT_TIMEOUT,
        )?;
        control.set_nodelay(true)?;
        control.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        control.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let writer = control.try_clone()?;
        let mut reader = control;

        let mut w = writer.try_clone()?;
        w.write_all(proto::CLIENT_HEADER)?;
        proto::write_i32(&mut w, proto::VERSION_CODE)?;
        if !proto::read_bool(&mut reader)? {
            let remote = proto::read_i32(&mut reader).unwrap_or(0);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "protocol version mismatch: local {} remote {}",
                    proto::VERSION_CODE, remote
                ),
            ));
        }

        let count = proto::read_i32(&mut reader)?;
        if count <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the phone is not offering any network interface",
            ));
        }
        let mut interfaces = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let name = proto::read_utf(&mut reader)?;
            let address_length = proto::read_u8(&mut reader)? as usize;
            let mut address_bytes = vec![0u8; address_length];
            reader.read_exact(&mut address_bytes)?;
            let address = ip_from_bytes(&address_bytes)?;
            let bind_length = proto::read_u8(&mut reader)? as usize;
            let bind = if bind_length == 0 {
                None
            } else {
                let mut bind_bytes = vec![0u8; bind_length];
                reader.read_exact(&mut bind_bytes)?;
                Some(ip_from_bytes(&bind_bytes)?)
            };
            interfaces.push(PeerInterface {
                name,
                address,
                bind,
            });
        }
        if expected_interfaces > 0 && expected_interfaces != interfaces.len() {
            log::debug(&format!(
                "interface count changed: expected {} got {}",
                expected_interfaces,
                interfaces.len()
            ));
        }

        // One transfer channel per network interface, connected in the order the
        // phone advertised them (the phone accepts them one by one).
        let mut transfers: Vec<(String, TcpStream)> = Vec::with_capacity(interfaces.len());
        // A writer/reader dedicated to the control channel: the "channel connected"
        // flags are exchanged there, not on the transfer channels.
        let mut cw = writer.try_clone()?;
        for interface in &interfaces {
            let interface_port = if interface.address.is_loopback() {
                local_port
            } else {
                port
            };
            let target = SocketAddr::new(interface.address, interface_port);
            log::debug(&format!(
                "connecting transfer channel {} -> {} (bind: {})",
                interface.name,
                target,
                interface
                    .bind
                    .map(|b| b.to_string())
                    .unwrap_or_else(|| "system default".to_string())
            ));
            match TcpStream::connect_timeout(&target, CONNECT_TIMEOUT) {
                Ok(stream) => {
                    stream.set_nodelay(true)?;
                    stream.set_read_timeout(Some(TRANSFER_TIMEOUT))?;
                    stream.set_write_timeout(Some(TRANSFER_TIMEOUT))?;
                    proto::write_bool(&mut cw, true)?;
                    proto::write_utf(&mut cw, &interface.name)?;
                    let accepted = proto::read_bool(&mut reader)?;
                    if accepted {
                        transfers.push((interface.name.clone(), stream));
                    } else {
                        log::warn(&format!(
                            "the phone refused the transfer channel {} ({})",
                            interface.name, interface.address
                        ));
                    }
                }
                Err(e) => {
                    // 这张网卡连不上（比如电脑和手机的WIFI不在同一网段），
                    // 告诉手机跳过它，其它通道继续用 —— 自适应多通道。
                    let _ = proto::write_bool(&mut cw, false);
                    let _ = proto::write_utf(&mut cw, &interface.name);
                    let _ = proto::read_bool(&mut reader);
                    log::warn(&format!(
                        "cannot reach transfer channel {} ({}) : {} — skipped",
                        interface.name, interface.address, e
                    ));
                }
            }
        }

        // Buffer blocks: the phone tells us how many it wants us to allocate.
        // This receiver writes straight to disk, so acknowledge without
        // allocating anything.
        let buffer_count = proto::read_i32(&mut reader)?;
        log::debug(&format!(
            "phone requested {} buffer blocks (not needed by this receiver)",
            buffer_count
        ));
        proto::write_bool(&mut cw, true)?;
        if !proto::read_bool(&mut reader)? {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "the phone could not allocate its buffer blocks (out of memory)",
            ));
        }
        proto::write_i32(&mut cw, current_file_system())?;
        proto::write_utf(&mut cw, home_dir)?;

        reader.set_read_timeout(Some(IDLE_TIMEOUT))?;
        reader.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

        Ok(Session {
            device: String::new(),
            reader,
            writer,
            transfers,
            interfaces,
        })
    }

    pub fn interface_names(&self) -> String {
        self.interfaces
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Serve the control channel until the phone hangs up, asks to stop, or the
    /// local stop flag is raised.
    pub fn run(&mut self, receiver: &Arc<Receiver>, stop: &Arc<AtomicBool>) -> SessionEnd {
        let mut last_activity = Instant::now();
        // 用较短的读超时轮询 stop 标志；手机端每隔几秒会发心跳
        let _ = self
            .reader
            .set_read_timeout(Some(Duration::from_millis(500)));
        loop {
            if stop.load(Ordering::Relaxed) {
                return SessionEnd::Stopped;
            }
            let id = match proto::read_i16(&mut self.reader) {
                Ok(id) => {
                    last_activity = Instant::now();
                    id
                }
                Err(e) if is_timeout(&e) => {
                    if last_activity.elapsed() > IDLE_TIMEOUT * 5 {
                        return SessionEnd::Disconnected(describe_read_error(&e));
                    }
                    continue;
                }
                Err(e) => {
                    return SessionEnd::Disconnected(describe_read_error(&e));
                }
            };
            // 处理指令期间用长一点的超时（手机端处理大文件需要时间）
            let _ = self
                .reader
                .set_read_timeout(Some(Duration::from_secs(120)));
            let result = match id {
                controller::SHUTDOWN => return SessionEnd::Shutdown,
                controller::PING => Ok(()),
                controller::CHECK_PENDING => self.handle_check_pending(),
                controller::LIST_FILES => self.handle_list_files(),
                controller::DELETE_FILE => self.handle_delete(),
                controller::MKDIR => self.handle_mkdir(),
                controller::REQUEST_RECEIVE => self.handle_receive(receiver),
                controller::REQUEST_SEND => self.handle_send(),
                other => {
                    // 不认识的控制指令就忽略（协议要求老客户端也能兼容），别断开连接，
                    // 否则新手机端 + 旧电脑端会陷入"连上-被踢-重连"的死循环
                    let _ = self.reader.set_read_timeout(Some(Duration::from_millis(500)));
                    log::warn(&format!(
                        "{} {}（已忽略）",
                        crate::i18n::tr("收到不认识的控制指令", "unknown controller command"),
                        other
                    ));
                    Ok(())
                }
            };
            let _ = self
                .reader
                .set_read_timeout(Some(Duration::from_millis(500)));
            if let Err(e) = result {
                return SessionEnd::Disconnected(format!("{}", e));
            }
        }
    }

    fn handle_list_files(&mut self) -> io::Result<()> {
        let path = proto::read_utf(&mut self.reader)?;
        let entries = list_directory(&path);
        match entries {
            Some(entries) => {
                proto::write_i32(&mut self.writer, entries.len() as i32)?;
                for entry in entries {
                    proto::write_utf(&mut self.writer, &entry.name)?;
                    proto::write_utf(&mut self.writer, &entry.path)?;
                    proto::write_i64(&mut self.writer, entry.modified_ms)?;
                    proto::write_i64(&mut self.writer, entry.size)?;
                    proto::write_bool(&mut self.writer, entry.is_dir)?;
                }
            }
            None => {
                proto::write_i32(&mut self.writer, -1)?;
            }
        }
        Ok(())
    }

    fn handle_delete(&mut self) -> io::Result<()> {
        let path = proto::read_utf(&mut self.reader)?;
        let ok = remove_recursive(std::path::Path::new(&path));
        proto::write_bool(&mut self.writer, ok)
    }

    fn handle_mkdir(&mut self) -> io::Result<()> {
        let parent = proto::read_utf(&mut self.reader)?;
        let child = proto::read_utf(&mut self.reader)?;
        let mut path = std::path::PathBuf::from(parent);
        path.push(child);
        let ok = std::fs::create_dir_all(&path).is_ok();
        log::info(&format!(
            "{} {}",
            crate::i18n::tr("创建文件夹", "create folder"),
            path.display()
        ));
        proto::write_bool(&mut self.writer, ok)
    }

    /// 手机请求从电脑取文件（REQUEST_SEND）：把选中的文件/文件夹切块，
    /// 通过所有传输通道发过去。格式与手机端 ReceiveFileCall 对应。
    /// 手机问"有没有要推给我的文件"：回列表（路径 + 大小）
    fn handle_check_pending(&mut self) -> io::Result<()> {
        let items = crate::pending::snapshot_for(&self.device);
        if !items.is_empty() {
            log::info(&format!("CHECK_PENDING -> {} item(s)", items.len()));
        }
        proto::write_i32(&mut self.writer, items.len() as i32)?;
        for (path, size) in &items {
            proto::write_utf(&mut self.writer, &path.to_string_lossy())?;
            proto::write_i64(&mut self.writer, *size as i64)?;
        }
        Ok(())
    }

    fn handle_send(&mut self) -> io::Result<()> {
        // 手机来拉了，队列清空，避免重复推送
        let _ = crate::pending::take_for(&self.device);
        let count = proto::read_i32(&mut self.reader)?.max(0);
        let mut sources = Vec::with_capacity(count as usize);
        for _ in 0..count {
            sources.push(proto::read_utf(&mut self.reader)?);
        }
        let phone_dir = proto::read_utf(&mut self.reader)?;
        let _phone_fs = proto::read_i32(&mut self.reader)?;
        let _pc_dir = proto::read_utf(&mut self.reader)?;

        if self.transfers.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "no transfer channel available",
            ));
        }

        log::info(crate::i18n::tr(
            "手机开始从电脑取文件",
            "the phone is downloading files from this PC",
        ));
        // 这次发送期间允许界面暂停 / 中断；离开作用域自动清标志
        let _sending = crate::control::SendingGuard::begin();

        let base = PathBuf::from(&phone_dir);
        let mut tasks: Vec<SendTask> = Vec::new();
        let mut next_index = 0i32;
        for source in &sources {
            collect_send_tasks(Path::new(source), &base, Path::new(source), &mut tasks, &mut next_index);
        }
        if tasks.is_empty() {
            log::warn(crate::i18n::tr("没有可发送的文件", "nothing to send"));
        }

        // IDM 式动态分段：一开始按通道数把任务表切成连续区间（一条通道一段），
        // 谁先传完自己的段，就把剩余最多的一段再切一半过来。手机端的进度条
        // 按"哪个通道送了第几块"还原这些段，段数会随传输过程变化。
        let tasks = Arc::new(tasks);
        let plan = Arc::new(Mutex::new(SendPlan::new(tasks.len(), self.transfers.len())));
        let sent = Arc::new(AtomicU64::new(0));
        let started = Instant::now();

        let mut handles = Vec::with_capacity(self.transfers.len());
        for (index, (name, stream)) in self.transfers.iter().enumerate() {
            let stream = stream.try_clone()?;
            let tasks = Arc::clone(&tasks);
            let plan = Arc::clone(&plan);
            let sent = Arc::clone(&sent);
            let name = name.clone();
            handles.push((
                name,
                thread::spawn(move || send_channel(stream, index, tasks, plan, sent)),
            ));
        }

        let mut hard_error: Option<String> = None;
        for (name, handle) in handles {
            match handle.join() {
                Ok(Ok(bytes)) => {
                    log::info(&format!("{}: {} {}", name, crate::i18n::tr("已发送", "sent"), crate::receiver::human_bytes(bytes)));
                }
                Ok(Err(e)) => {
                    log::error(&format!("{}: {}", name, e));
                    if hard_error.is_none() {
                        hard_error = Some(format!("{}: {}", name, e));
                    }
                }
                Err(_) => {
                    if hard_error.is_none() {
                        hard_error = Some(format!("{}: panic", name));
                    }
                }
            }
        }

        let total = sent.load(Ordering::Relaxed);
        let seconds = started.elapsed().as_secs_f64().max(0.001);
        log::info(&format!(
            "{} {} · {} · {:.1}s",
            crate::i18n::tr("发送完成，共", "sent"),
            crate::receiver::human_bytes(total),
            crate::i18n::tr("结束", "done"),
            seconds
        ));

        // 与 HFXService.receiveFiles() 的收尾对齐：
        //   手机：writeBoolean(true) → readBoolean()
        //   电脑：先读手机的 bool，再回一个 bool（失败时再跟一段 UTF 错误信息）
        let _ = proto::read_bool(&mut self.reader);
        match &hard_error {
            None => {
                proto::write_bool(&mut self.writer, true)?;
            }
            Some(message) => {
                log::warn(&format!("send finished with error: {message}"));
                proto::write_bool(&mut self.writer, false)?;
                proto::write_utf(&mut self.writer, message)?;
            }
        }
        Ok(())
    }

    fn handle_receive(&mut self, receiver: &Arc<Receiver>) -> io::Result<()> {
        if self.transfers.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "no transfer channel available",
            ));
        }
        log::info(crate::i18n::tr("手机开始发送文件", "the phone is sending files"));
        let started = Instant::now();
        let mut handles = Vec::with_capacity(self.transfers.len());
        for (name, stream) in &self.transfers {
            let stream = stream.try_clone()?;
            let receiver = Arc::clone(receiver);
            let name = name.clone();
            handles.push((
                name,
                thread::spawn(move || read_transfer_channel(stream, receiver)),
            ));
        }
        let monitor_stop = Arc::new(AtomicBool::new(false));
        let monitor = {
            let stop = Arc::clone(&monitor_stop);
            let receiver = Arc::clone(receiver);
            thread::spawn(move || monitor_speed(receiver, stop))
        };

        let mut hard_error: Option<String> = None;
        let mut aborts: Vec<String> = Vec::new();
        for (name, handle) in handles {
            match handle.join() {
                Ok(Ok(ChannelEnd::Eof)) => {}
                Ok(Ok(ChannelEnd::RemoteAbort(reason))) => {
                    log::warn(&format!("{}: {}", name, reason));
                    aborts.push(format!("{}: {}", name, reason));
                }
                Ok(Err(e)) => {
                    log::error(&format!("{}: {}", name, e));
                    if hard_error.is_none() {
                        hard_error = Some(format!("{}: {}", name, e));
                    }
                }
                Err(_) => {
                    log::error(&format!("{}: receive thread panicked", name));
                    if hard_error.is_none() {
                        hard_error = Some(format!("{}: receive thread panicked", name));
                    }
                }
            }
        }
        monitor_stop.store(true, Ordering::Relaxed);
        let _ = monitor.join();

        let bytes = receiver.bytes();
        let files = receiver.file_count();
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        if let Err(e) = receiver.finish() {
            log::warn(&format!("finish(): {}", e));
        }

        if let Some(error) = hard_error {
            log::error(&format!(
                "{}: {}",
                crate::i18n::tr("接收失败", "receiving failed"),
                error
            ));
            proto::write_bool(&mut self.writer, false)?;
            proto::write_utf(&mut self.writer, &error)?;
            return Ok(());
        }

        // Mirror the reference client: report "written" and read the phone's
        // verdict (it fails when reading a file on its own side).
        proto::write_bool(&mut self.writer, true)?;
        if !proto::read_bool(&mut self.reader)? {
            let message = proto::read_utf(&mut self.reader).unwrap_or_default();
            log::error(&format!(
                "{}: {}",
                crate::i18n::tr("手机读取文件时出错", "the phone failed to read a file"),
                message
            ));
        } else if aborts.is_empty() {
            log::info(&format!(
                "{} {} {} {} {}",
                crate::i18n::tr("接收完成：", "received"),
                files,
                crate::i18n::tr("个文件", "file(s),"),
                util::format_bytes(bytes),
                format!(
                    "{} {:.1}s ({})",
                    crate::i18n::tr("用时", "in"),
                    elapsed,
                    util::format_speed(bytes as f64 / elapsed)
                )
            ));
        } else {
            log::warn(crate::i18n::tr(
                "接收已中断",
                "the transfer was interrupted",
            ));
        }
        Ok(())
    }
}

enum ChannelEnd {
    Eof,
    RemoteAbort(String),
}

fn read_transfer_channel(
    mut stream: TcpStream,
    receiver: Arc<Receiver>,
) -> io::Result<ChannelEnd> {
    let mut buffer = vec![0u8; BLOCK_SIZE as usize];
    loop {
        let header = proto::read_i16(&mut stream)?;
        match header {
            transfer::FILE => {
                let _file_index = proto::read_i32(&mut stream)?;
                let path = proto::read_utf(&mut stream)?;
                let modified_ms = proto::read_i64(&mut stream)?;
                let total_size = proto::read_i64(&mut stream)?;
                let index = proto::read_i32(&mut stream)?;
                let length = proto::read_i32(&mut stream)?;
                if index < 0 || length < 0 || length as usize > buffer.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid block: index={} length={}", index, length),
                    ));
                }
                if total_size < 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid file size",
                    ));
                }
                let length = length as usize;
                if length > 0 {
                    stream.read_exact(&mut buffer[..length])?;
                }
                receiver.write_block(
                    &path,
                    total_size as u64,
                    index as u32,
                    modified_ms,
                    &buffer[..length],
                )?;
            }
            transfer::FOLDER => {
                let _file_index = proto::read_i32(&mut stream)?;
                let path = proto::read_utf(&mut stream)?;
                let _modified_ms = proto::read_i64(&mut stream)?;
                receiver.make_dir(&path)?;
            }
            transfer::EOF => return Ok(ChannelEnd::Eof),
            transfer::END_OF_INTERRUPTED => {
                return Ok(ChannelEnd::RemoteAbort(crate::i18n::tr(
                    "传输被其他通道中断",
                    "interrupted because another channel failed",
                )
                .to_string()))
            }
            transfer::END_OF_READ_ERROR => {
                return Ok(ChannelEnd::RemoteAbort(crate::i18n::tr(
                    "手机读取文件时出错",
                    "the phone failed to read a file",
                )
                .to_string()))
            }
            transfer::END_OF_WRITE_ERROR => {
                return Ok(ChannelEnd::RemoteAbort(crate::i18n::tr(
                    "写入文件时出错",
                    "writing the file failed",
                )
                .to_string()))
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected transfer frame {}", other),
                ))
            }
        }
    }
}

fn monitor_speed(receiver: Arc<Receiver>, stop: Arc<AtomicBool>) {
    let mut last_bytes = receiver.bytes();
    let mut last_tick = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(200));
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();
        if now.duration_since(last_tick) < Duration::from_millis(900) {
            continue;
        }
        let bytes = receiver.bytes();
        let seconds = now.duration_since(last_tick).as_secs_f64().max(0.001);
        let speed = (bytes.saturating_sub(last_bytes)) as f64 / seconds;
        log::progress(&format!(
            "{} {}/s  {} {}",
            crate::i18n::tr("接收中", "receiving"),
            util::format_bytes(speed as u64),
            util::format_bytes(bytes),
            crate::i18n::tr("已接收", "total")
        ));
        last_bytes = bytes;
        last_tick = now;
    }
}

fn ip_from_bytes(bytes: &[u8]) -> io::Result<IpAddr> {
    match bytes.len() {
        4 => Ok(IpAddr::from([bytes[0], bytes[1], bytes[2], bytes[3]])),
        16 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            Ok(IpAddr::from(octets))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid IP address length {}", bytes.len()),
        )),
    }
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn describe_read_error(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => crate::i18n::tr(
            "与手机的连接超时（没有收到心跳）",
            "connection to the phone timed out (no heartbeat)",
        )
        .to_string(),
        io::ErrorKind::UnexpectedEof => {
            crate::i18n::tr("手机断开了连接", "the phone closed the connection").to_string()
        }
        _ => format!("{}", error),
    }
}

struct ListedEntry {
    name: String,
    path: String,
    modified_ms: i64,
    size: i64,
    is_dir: bool,
}

fn list_directory(path: &str) -> Option<Vec<ListedEntry>> {
    let trimmed = path.trim();
    // Root: on Windows this is the list of drives, like the Java client.
    #[cfg(windows)]
    {
        if trimmed.is_empty() || trimmed == "/" {
            let mut drives = Vec::new();
            for letter in b'A'..=b'Z' {
                let root = format!("{}:\\", letter as char);
                if std::path::Path::new(&root).exists() {
                    drives.push(ListedEntry {
                        name: root.clone(),
                        path: root,
                        modified_ms: 0,
                        size: 0,
                        is_dir: true,
                    });
                }
            }
            return Some(drives);
        }
    }
    let lookup = if trimmed.is_empty() { "/" } else { trimmed };
    let dir = std::fs::read_dir(lookup).ok()?;
    let mut entries = Vec::new();
    for entry in dir.flatten() {
        let file_name = entry.file_name().to_string_lossy().to_string();
        let full_path = entry.path().to_string_lossy().to_string();
        let metadata = entry.metadata().ok();
        let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = metadata
            .as_ref()
            .map(|m| if is_dir { 0 } else { m.len() as i64 })
            .unwrap_or(0);
        let modified_ms = metadata
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        entries.push(ListedEntry {
            name: file_name,
            path: full_path,
            modified_ms,
            size,
            is_dir,
        });
    }
    Some(entries)
}

fn remove_recursive(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.is_dir() {
        std::fs::remove_dir_all(path).is_ok()
    } else {
        std::fs::remove_file(path).is_ok()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::transfer;
    use std::net::TcpListener;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("hfx-session-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct Block {
        path: String,
        modified_ms: i64,
        total_size: u64,
        index: u32,
        data: Vec<u8>,
    }

    /// A minimal implementation of the phone side of the protocol, following
    /// HybridFileXfer-Android/.../droidcore/HFXServer.java and SendFileCall.java.
    fn spawn_mock_phone(
        listener: TcpListener,
        folder: String,
        blocks: Vec<Block>,
        home_dir: String,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let (control, _) = listener.accept().unwrap();
            control
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            control
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut cr: &TcpStream = &control;
            let mut cw: &TcpStream = &control;
            let mut header = [0u8; 4];
            cr.read_exact(&mut header).unwrap();
            assert_eq!(&header, proto::CLIENT_HEADER);
            assert_eq!(proto::read_i32(&mut cr).unwrap(), proto::VERSION_CODE);
            proto::write_bool(&mut cw, true).unwrap();
            // Two interfaces on localhost -> two transfer channels.
            proto::write_i32(&mut cw, 2).unwrap();
            for name in ["USB_ADB", "wlan0"] {
                proto::write_utf(&mut cw, name).unwrap();
                proto::write_u8(&mut cw, 4).unwrap();
                cw.write_all(&[127, 0, 0, 1]).unwrap();
                proto::write_u8(&mut cw, 0).unwrap();
            }
            let mut transfers = Vec::new();
            for _ in 0..2 {
                let (transfer, _) = listener.accept().unwrap();
                // The "channel connected" flag and name travel on the control channel.
                assert!(proto::read_bool(&mut cr).unwrap());
                assert!(!proto::read_utf(&mut cr).unwrap().is_empty());
                proto::write_bool(&mut cw, true).unwrap();
                transfers.push(transfer);
            }
            // Buffer handshake: the phone asks the client to allocate 512 blocks.
            proto::write_i32(&mut cw, 512).unwrap();
            assert!(proto::read_bool(&mut cr).unwrap());
            proto::write_bool(&mut cw, true).unwrap();
            assert_eq!(
                proto::read_i32(&mut cr).unwrap(),
                proto::current_file_system()
            );
            assert_eq!(proto::read_utf(&mut cr).unwrap(), home_dir);

            // Heartbeat frames must be ignored by the client.
            proto::write_i16(&mut cw, controller::PING).unwrap();
            proto::write_i16(&mut cw, controller::REQUEST_RECEIVE).unwrap();

            let first = transfers[0].try_clone().unwrap();
            let second = transfers[1].try_clone().unwrap();
            let folder_a = folder.clone();
            let blocks_a = blocks
                .iter()
                .filter(|b| b.index % 2 == 0)
                .map(|b| Block {
                    path: b.path.clone(),
                    modified_ms: b.modified_ms,
                    total_size: b.total_size,
                    index: b.index,
                    data: b.data.clone(),
                })
                .collect::<Vec<_>>();
            let blocks_b = blocks
                .iter()
                .filter(|b| b.index % 2 == 1)
                .map(|b| Block {
                    path: b.path.clone(),
                    modified_ms: b.modified_ms,
                    total_size: b.total_size,
                    index: b.index,
                    data: b.data.clone(),
                })
                .collect::<Vec<_>>();
            let writer_a = thread::spawn(move || {
                let mut w: &TcpStream = &first;
                proto::write_i16(&mut w, transfer::FOLDER).unwrap();
                proto::write_i32(&mut w, 0).unwrap();
                proto::write_utf(&mut w, &folder_a).unwrap();
                proto::write_i64(&mut w, 1_700_000_000_000).unwrap();
                let mut file_index = 1;
                let mut last_path = String::new();
                for block in &blocks_a {
                    if block.path != last_path {
                        file_index += 1;
                        last_path = block.path.clone();
                    }
                    proto::write_i16(&mut w, transfer::FILE).unwrap();
                    proto::write_i32(&mut w, file_index).unwrap();
                    proto::write_utf(&mut w, &block.path).unwrap();
                    proto::write_i64(&mut w, block.modified_ms).unwrap();
                    proto::write_i64(&mut w, block.total_size as i64).unwrap();
                    proto::write_i32(&mut w, block.index as i32).unwrap();
                    proto::write_i32(&mut w, block.data.len() as i32).unwrap();
                    w.write_all(&block.data).unwrap();
                }
                proto::write_i16(&mut w, transfer::EOF).unwrap();
            });
            let writer_b = thread::spawn(move || {
                let mut w: &TcpStream = &second;
                let mut file_index = 1;
                let mut last_path = String::new();
                for block in &blocks_b {
                    if block.path != last_path {
                        file_index += 1;
                        last_path = block.path.clone();
                    }
                    proto::write_i16(&mut w, transfer::FILE).unwrap();
                    proto::write_i32(&mut w, file_index).unwrap();
                    proto::write_utf(&mut w, &block.path).unwrap();
                    proto::write_i64(&mut w, block.modified_ms).unwrap();
                    proto::write_i64(&mut w, block.total_size as i64).unwrap();
                    proto::write_i32(&mut w, block.index as i32).unwrap();
                    proto::write_i32(&mut w, block.data.len() as i32).unwrap();
                    w.write_all(&block.data).unwrap();
                }
                proto::write_i16(&mut w, transfer::EOF).unwrap();
            });
            writer_a.join().unwrap();
            writer_b.join().unwrap();

            // The client confirms the write, the phone confirms its own read.
            assert!(proto::read_bool(&mut cr).unwrap());
            proto::write_bool(&mut cw, true).unwrap();
            proto::write_i16(&mut cw, controller::SHUTDOWN).unwrap();
        })
    }

    #[test]
    fn receives_files_from_a_mock_phone() {
        let dir = temp_dir("receive");
        let home = util::normalize_dir(&dir).to_string_lossy().to_string();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let big: Vec<u8> = (0..(BLOCK_SIZE as usize * 2 + 100))
            .map(|i| (i % 251) as u8)
            .collect();
        let small = b"hello hybrid file xfer".to_vec();
        let big_path = dir.join("big.bin").to_string_lossy().to_string();
        let small_path = dir.join("small.txt").to_string_lossy().to_string();
        let folder_path = dir.join("sub dir").to_string_lossy().to_string();

        let mut blocks = Vec::new();
        for (path, data) in [(&big_path, &big), (&small_path, &small)] {
            if data.is_empty() {
                blocks.push(Block {
                    path: path.clone(),
                    modified_ms: 1_700_000_000_000,
                    total_size: 0,
                    index: 0,
                    data: Vec::new(),
                });
                continue;
            }
            let mut offset = 0usize;
            let mut index = 0u32;
            while offset < data.len() {
                let end = (offset + BLOCK_SIZE as usize).min(data.len());
                blocks.push(Block {
                    path: path.clone(),
                    modified_ms: 1_700_000_000_000,
                    total_size: data.len() as u64,
                    index,
                    data: data[offset..end].to_vec(),
                });
                offset = end;
                index += 1;
            }
        }

        let server = spawn_mock_phone(listener, folder_path.clone(), blocks, home.clone());
        let mut session = Session::connect(IpAddr::from([127, 0, 0, 1]), port, &home, 0).unwrap();
        assert_eq!(session.interface_names(), "USB_ADB, wlan0");
        let receiver = Arc::new(Receiver::new(util::normalize_dir(&dir)));
        let stop = Arc::new(AtomicBool::new(false));
        match session.run(&receiver, &stop) {
            SessionEnd::Shutdown => {}
            SessionEnd::Disconnected(reason) => panic!("unexpected disconnect: {}", reason),
            SessionEnd::Stopped => panic!("unexpected stop"),
        }
        server.join().unwrap();

        assert_eq!(std::fs::read(&big_path).unwrap(), big);
        assert_eq!(std::fs::read(&small_path).unwrap(), small);
        assert!(std::path::Path::new(&folder_path).is_dir());
    }
}
// ---------------- 发送方向（电脑 -> 手机） ----------------

/// 发往手机的一条记录：文件夹，或者某个文件的第 index 块
enum SendTask {
    Folder {
        file_index: i32,
        dest: String,
        modified_ms: i64,
    },
    Block {
        file_index: i32,
        dest: String,
        modified_ms: i64,
        total_size: u64,
        index: u32,
        length: u32,
        host_path: PathBuf,
    },
}

/// 把源路径展开成发送任务；目录按协议逐条发 FOLDER 记录，文件按 1MiB 切块
fn collect_send_tasks(
    host: &Path,
    dest_root: &Path,
    base: &Path,
    tasks: &mut Vec<SendTask>,
    next_index: &mut i32,
) {
    let meta = match std::fs::metadata(host) {
        Ok(meta) => meta,
        Err(e) => {
            log::warn(&format!("{}: {}", host.display(), e));
            return;
        }
    };
    // 顶层文件的 strip_prefix 结果是空路径，这时要用文件名，否则目标会变成目录本身
    let relative: PathBuf = host
        .strip_prefix(base)
        .ok()
        .filter(|rest| !rest.as_os_str().is_empty())
        .map(|rest| rest.to_path_buf())
        .unwrap_or_else(|| host.file_name().map(PathBuf::from).unwrap_or_default());
    // 目标路径是 Android 上的路径，统一用正斜杠
    let dest = dest_root
        .join(relative.to_string_lossy().replace('\\', "/"))
        .to_string_lossy()
        .to_string();

    let file_index = *next_index;
    *next_index += 1;
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    if meta.is_dir() {
        tasks.push(SendTask::Folder {
            file_index,
            dest: dest.clone(),
            modified_ms,
        });
        if let Ok(reader) = std::fs::read_dir(host) {
            let mut children: Vec<_> = reader.flatten().map(|e| e.path()).collect();
            children.sort();
            for child in children {
                collect_send_tasks(&child, dest_root, base, tasks, next_index);
            }
        }
        return;
    }

    let total_size = meta.len();
    let blocks = ((total_size + BLOCK_SIZE - 1) / BLOCK_SIZE).max(1) as u32;
    for index in 0..blocks {
        let offset = index as u64 * BLOCK_SIZE;
        let length = (total_size.saturating_sub(offset)).min(BLOCK_SIZE) as u32;
        tasks.push(SendTask::Block {
            file_index,
            dest: dest.clone(),
            modified_ms,
            total_size,
            index,
            length,
            host_path: host.to_path_buf(),
        });
    }
}

/// 一个通道的发送循环：按分段计划不断领任务，全部发完就写 EOF
fn send_channel(
    mut stream: std::net::TcpStream,
    me: usize,
    tasks: Arc<Vec<SendTask>>,
    plan: Arc<Mutex<SendPlan>>,
    sent: Arc<AtomicU64>,
) -> io::Result<u64> {
    let mut written = 0u64;
    loop {
        // 暂停：原地等（手上的 1MB 写完后停）；中断：直接退出，对端会看到断线
        while crate::control::paused() && !crate::control::cancelled() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if crate::control::cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled by user"));
        }
        let next = {
            let mut guard = plan.lock().unwrap();
            guard.take(me)
        };
        let Some(index) = next else { break };
        written += send_task(&mut stream, &tasks[index], &sent)?;
    }
    use std::io::Write as _;
    proto::write_i16(&mut stream, transfer::EOF)?;
    stream.flush()?;
    Ok(written)
}

/// 发一条任务（文件夹 / 一个文件块），返回这一块写了多少字节
fn send_task(stream: &mut std::net::TcpStream, task: &SendTask, sent: &AtomicU64) -> io::Result<u64> {
    match task {
        SendTask::Folder {
            file_index,
            dest,
            modified_ms,
        } => {
            proto::write_i16(stream, transfer::FOLDER)?;
            proto::write_i32(stream, *file_index)?;
            proto::write_utf(stream, dest)?;
            proto::write_i64(stream, *modified_ms)?;
            Ok(0)
        }
        SendTask::Block {
            file_index,
            dest,
            modified_ms,
            total_size,
            index,
            length,
            host_path,
        } => {
            let mut buffer = vec![0u8; *length as usize];
            let mut file = match std::fs::File::open(host_path) {
                Ok(file) => file,
                Err(e) => {
                    log::error(&format!("{}: {}", host_path.display(), e));
                    let _ = proto::write_i16(stream, transfer::END_OF_READ_ERROR);
                    return Err(e);
                }
            };
            use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
            if let Err(e) = file.seek(SeekFrom::Start(*index as u64 * BLOCK_SIZE)) {
                let _ = proto::write_i16(stream, transfer::END_OF_READ_ERROR);
                return Err(e);
            }
            if let Err(e) = file.read_exact(&mut buffer) {
                let _ = proto::write_i16(stream, transfer::END_OF_READ_ERROR);
                return Err(e);
            }

            proto::write_i16(stream, transfer::FILE)?;
            proto::write_i32(stream, *file_index)?;
            proto::write_utf(stream, dest)?;
            proto::write_i64(stream, *modified_ms)?;
            proto::write_i64(stream, *total_size as i64)?;
            proto::write_i32(stream, *index as i32)?;
            proto::write_i32(stream, *length as i32)?;
            stream.write_all(&buffer)?;
            stream.flush()?;

            sent.fetch_add(*length as u64, Ordering::Relaxed);
            crate::receiver::PROGRESS.set_sending(dest, *index as u64 * BLOCK_SIZE + *length as u64, *total_size, *length as u64);
            Ok(*length as u64)
        }
    }
}

/// 发送任务的动态分段计划（IDM 式：一段 = 一条通道负责的连续任务区间）
struct Segment {
    #[allow(dead_code)]
    start: usize,
    end: usize,
    cursor: usize,
    owner: Option<usize>,
}

struct SendPlan {
    segments: Vec<Segment>,
}

impl SendPlan {
    /// 初始：把任务表按通道数切成连续区间
    fn new(total: usize, channels: usize) -> Self {
        let mut segments = Vec::new();
        if total > 0 && channels > 0 {
            let per = (total + channels - 1) / channels;
            let mut start = 0;
            let mut owner = 0usize;
            while start < total {
                let end = (start + per).min(total);
                // 初始一段对应一条通道：一开始 N 条连接就是 N 段
                segments.push(Segment {
                    start,
                    end,
                    cursor: start,
                    owner: Some(owner),
                });
                owner += 1;
                start = end;
            }
        }
        SendPlan { segments }
    }

    /// 领一个任务：先自己的段，再没人认领的段，最后把剩余最多的一段切一半抢过来
    fn take(&mut self, me: usize) -> Option<usize> {
        if let Some(seg) = self
            .segments
            .iter_mut()
            .find(|s| s.owner == Some(me) && s.cursor < s.end)
        {
            let index = seg.cursor;
            seg.cursor += 1;
            return Some(index);
        }
        if let Some(seg) = self
            .segments
            .iter_mut()
            .find(|s| s.owner.is_none() && s.cursor < s.end)
        {
            seg.owner = Some(me);
            let index = seg.cursor;
            seg.cursor += 1;
            return Some(index);
        }
        let victim = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| s.end.saturating_sub(s.cursor) > 1)
            .max_by_key(|(_, s)| s.end - s.cursor)
            .map(|(i, _)| i)?;
        let (mid, old_end) = {
            let seg = &mut self.segments[victim];
            let mid = seg.cursor + (seg.end - seg.cursor) / 2;
            let old_end = seg.end;
            seg.end = mid;
            (mid, old_end)
        };
        self.segments.push(Segment {
            start: mid,
            end: old_end,
            cursor: mid + 1,
            owner: Some(me),
        });
        Some(mid)
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    #[test]
    fn plan_covers_every_task_once_and_splits() {
        let mut plan = SendPlan::new(100, 2);
        let mut seen = vec![false; 100];

        // 通道 0 抢跑，把剩下的段切走一半
        let mut taken = 0;
        for _ in 0..60 {
            if let Some(i) = plan.take(0) {
                assert!(!seen[i]);
                seen[i] = true;
                taken += 1;
            }
        }
        assert_eq!(taken, 60);
        assert!(
            plan.segments.len() > 2,
            "通道 0 抢了别人的段，应该产生新的段"
        );

        // 两条通道交替把剩下的任务领完
        for _ in 0..300 {
            for me in [0usize, 1] {
                if let Some(i) = plan.take(me) {
                    assert!(!seen[i], "同一个任务不能发两次");
                    seen[i] = true;
                }
            }
        }
        assert!(seen.iter().all(|&x| x), "每个任务都必须被发到");
    }
}
