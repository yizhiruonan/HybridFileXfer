//! One connection to the phone: handshake, control channel and file receiving.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::log;
use crate::proto::{self, controller, current_file_system, transfer, BLOCK_SIZE};
use crate::receiver::Receiver;
use crate::util;

/// 单条通道的连接超时。并行连之后总耗时就等于它，所以压到 1.5 秒：
/// 同一个局域网里 1.5 秒还连不上的通道，多半本来就不通
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(20);
/// 多久没收到手机的心跳就当它已经走了。
///
/// 手机端每 3 秒发一次 PING（ServerService.PING_INTERVAL_MS），所以 12 秒＝连丢 4 次，
/// 真断了不会误判。以前这里是 IDLE_TIMEOUT * 5 = 100 秒 —— 用户在手机上点「停止服务」
/// 再点「启动服务」时，手机那边根本不发 FIN（服务停了、进程还在，套接字还开着），
/// 电脑只能干等超时：界面上一直显示「已连接」，手机一直显示「等待电脑连接」，
/// 用户看到的就是「要等一分钟才连上」。
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(12);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
/// 连不上的通道地址缓存两分钟：重连/拔插时直接跳过，不用每条再等 2 秒超时
const UNREACHABLE_TTL: Duration = Duration::from_secs(120);

fn unreachable_channels() -> &'static Mutex<HashMap<IpAddr, Instant>> {
    static CACHE: OnceLock<Mutex<HashMap<IpAddr, Instant>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

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
    /// 传输期间"偷看"控制通道时读到的、暂不处理的指令：
    /// 传输结束后由主循环按原顺序继续处理
    pending_opcode: Option<i16>,
    pub interfaces: Vec<PeerInterface>,
    /// 手机监听的端口（非回环网卡上的传输通道重连要用）
    port: u16,
    /// adb 转发给这台手机的本地端口（回环通道重连要用）
    local_port: u16,
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
        // 控制通道也一样走"绑定物理网卡"的连接：梯子/虚拟网卡抢走默认路由时
        // 连手机的第一步就会失败或绕远路
        let control = crate::netif::connect(
            SocketAddr::new(address, control_port),
            CONNECT_TIMEOUT,
            None,
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

        // 通道连接必须快：插上线就该连上。以前是一条条串行连、每条最多等 CONNECT_TIMEOUT，
        // 手机那边 4 条 wlan2（192.168.173.x，电脑压根没有这个网段的网卡）就能把「已连接」
        // 拖到几十秒 —— 用户看到的就是「插上一分钟了还连不上」。
        // 现在：本机没有同网段网卡的地址直接跳过（照规矩给手机回 false），其余并行连接、顺序握手。
        struct ChannelAttempt {
            name: String,
            address: IpAddr,
            bind: Option<IpAddr>,
            target: SocketAddr,
            /// 直接跳过、不再尝试的原因
            skip: Option<&'static str>,
        }
        let local_v4 = crate::netif::local_ipv4();
        let mut plan: Vec<ChannelAttempt> = Vec::with_capacity(interfaces.len());
        {
            let cache = unreachable_channels().lock().unwrap();
            for interface in &interfaces {
                let interface_port = if interface.address.is_loopback() {
                    local_port
                } else {
                    port
                };
                let mut skip = None;
                if let IpAddr::V4(v4) = interface.address {
                    if !v4.is_loopback() {
                        let o = v4.octets();
                        // 只比前三段（/24）：手机和电脑在同一个局域网时前缀一定相同
                        let same_subnet = local_v4
                            .iter()
                            .any(|local| !local.is_loopback() && local.octets()[..3] == o[..3]);
                        if !same_subnet {
                            skip = Some("本机没有同网段网卡");
                        }
                    }
                }
                if skip.is_none() {
                    if let Some(at) = cache.get(&interface.address) {
                        if at.elapsed() < UNREACHABLE_TTL {
                            skip = Some("上次连接超时");
                        }
                    }
                }
                plan.push(ChannelAttempt {
                    name: interface.name.clone(),
                    address: interface.address,
                    bind: interface.bind,
                    target: SocketAddr::new(interface.address, interface_port),
                    skip,
                });
            }
        }

        // One transfer channel per network interface. 先并行把 TCP 连上（各自带超时，
        // 总耗时 = 最慢的那条，而不是所有失败通道累加），再按手机广播的顺序握手。
        let mut results: Vec<Option<io::Result<TcpStream>>> = thread::scope(|scope| {
            let mut slots: Vec<Option<io::Result<TcpStream>>> = Vec::with_capacity(plan.len());
            let handles: Vec<_> = plan
                .iter()
                .map(|attempt| {
                    if attempt.skip.is_some() {
                        return None;
                    }
                    log::debug(&format!(
                        "connecting transfer channel {} -> {} (bind: {})",
                        attempt.name,
                        attempt.target,
                        attempt
                            .bind
                            .map(|b| b.to_string())
                            .unwrap_or_else(|| "system default".to_string())
                    ));
                    let target = attempt.target;
                    let bind = attempt.bind;
                    Some(scope.spawn(move || crate::netif::connect(target, CONNECT_TIMEOUT, bind)))
                })
                .collect();
            for handle in handles {
                slots.push(match handle {
                    None => None,
                    Some(handle) => Some(handle.join().unwrap_or_else(|_| {
                        Err(io::Error::new(io::ErrorKind::Other, "connect thread panicked"))
                    })),
                });
            }
            slots
        });

        // A writer/reader dedicated to the control channel: the "channel connected"
        // flags are exchanged there, not on the transfer channels.
        let mut transfers: Vec<(String, TcpStream)> = Vec::with_capacity(interfaces.len());
        let mut cw = writer.try_clone()?;
        for (index, attempt) in plan.iter().enumerate() {
            let outcome = results[index].take();
            let refusal = match (&attempt.skip, &outcome) {
                (Some(reason), _) => Some((*reason).to_string()),
                (None, Some(Err(e))) => Some(e.to_string()),
                (None, Some(Ok(_))) => None,
                (None, None) => Some("连接未完成".to_string()),
            };
            let stream = match (refusal, outcome) {
                (None, Some(Ok(stream))) => stream,
                (reason, _) => {
                    // 这张网卡连不上（比如电脑和手机的 WIFI 不在同一网段）：告诉手机跳过它，
                    // 其它通道继续用 —— 自适应多通道。
                    let _ = proto::write_bool(&mut cw, false);
                    let _ = proto::write_utf(&mut cw, &attempt.name);
                    let _ = proto::read_bool(&mut reader);
                    let reason = reason.unwrap_or_else(|| "连接失败".to_string());
                    if attempt.skip.is_some() {
                        log::debug(&format!(
                            "skip transfer channel {} ({}) : {}",
                            attempt.name, attempt.address, reason
                        ));
                    } else {
                        // 超时的记两分钟，重连时不再白等
                        unreachable_channels()
                            .lock()
                            .unwrap()
                            .insert(attempt.address, Instant::now());
                        log::warn(&format!(
                            "cannot reach transfer channel {} ({}) : {} — skipped",
                            attempt.name, attempt.address, reason
                        ));
                    }
                    continue;
                }
            };
            unreachable_channels().lock().unwrap().remove(&attempt.address);
            stream.set_nodelay(true)?;
            stream.set_read_timeout(Some(TRANSFER_TIMEOUT))?;
            stream.set_write_timeout(Some(TRANSFER_TIMEOUT))?;
            proto::write_bool(&mut cw, true)?;
            proto::write_utf(&mut cw, &attempt.name)?;
            let accepted = proto::read_bool(&mut reader)?;
            if accepted {
                transfers.push((attempt.name.clone(), stream));
            } else {
                log::warn(&format!(
                    "the phone refused the transfer channel {} ({})",
                    attempt.name, attempt.address
                ));
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
            pending_opcode: None,
            interfaces,
            port,
            local_port,
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
            // 传输期间偷看控制通道时会先把指令头取走，这里优先处理暂存的那条
            let id = match self.pending_opcode.take() {
                Some(id) => {
                    last_activity = Instant::now();
                    id
                }
                None => match proto::read_i16(&mut self.reader) {
                    Ok(id) => {
                        last_activity = Instant::now();
                        id
                    }
                    Err(e) if is_timeout(&e) => {
                        // 手机每 3 秒一次心跳：连着 12 秒一个字都没收到就是断了
                        if last_activity.elapsed() > HEARTBEAT_TIMEOUT {
                            return SessionEnd::Disconnected(describe_read_error(&e));
                        }
                        continue;
                    }
                    Err(e) => {
                        return SessionEnd::Disconnected(describe_read_error(&e));
                    }
                },
            };
            // 处理指令期间用长一点的超时（手机端处理大文件需要时间）
            let _ = self
                .reader
                .set_read_timeout(Some(Duration::from_secs(120)));
            let result = match id {
                controller::SHUTDOWN => return SessionEnd::Shutdown,
                controller::PING => Ok(()),
                controller::CHECK_PENDING => self.handle_check_pending(),
                controller::PAUSE_STATE => self.handle_pause_state(),
                controller::LIST_FILES => self.handle_list_files(),
                controller::DELETE_FILE => self.handle_delete(),
                controller::MKDIR => self.handle_mkdir(),
                controller::REQUEST_RECEIVE => self.handle_receive(receiver),
                controller::REQUEST_SEND => self.handle_send(),
                // 302：结论帧正常只在传输收尾读，漏到这里说明时序错位，读干净别留半帧
                controller::RESULT => self.discard_result(),
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

    /// 304：取消之后把传输通道重新连回来（会话不断）。
    ///
    /// 手机端发完 CANCEL 只关传输通道、控制通道留着，就在这儿等我们连回去。
    /// 每条通道：connect → 发 REBUILD_REQ{名字} → 等 REBUILD_ACK{bool}；
    /// 等 ACK 期间手机端的暂停同步可能还在飞，就地处理掉，别让帧序错位。
    ///
    /// 一条都没恢复就返回错误：留着一条都传不了的会话，界面会一直显示"已连接"、
    /// 一传就失败，还不如老老实实断线重连。
    fn rebuild_transfers(&mut self) -> io::Result<usize> {
        // 先告诉手机"这是取消，不是断线" —— 本机点「中断发送」时它还不知道；
        // 手机自己点的取消，这一帧重复一次也无害（它是幂等的）。
        log::warn(crate::i18n::tr(
            "开始重建传输通道（已通知手机端取消）",
            "rebuilding the transfer channels",
        ));
        // 顺序：先关掉旧的传输通道，再告诉手机"这是取消"。
        //
        // 手机端是发送方，它在写满缓冲时会阻塞在 write 里、看不到控制通道；
        // 关通道（shutdown 会回 RST）是让它立刻停下的唯一手段。先关再发帧，
        // 手机端就不用「先读到 CANCEL 还得等写失败」。
        // （read_transfer_channel / handle_receive 的等待循环里已经关过一遍，
        //   这里再关一次是幂等的，覆盖"线程已经正常 EOF 退出"的情形。）
        for (_, stream) in self.transfers.iter() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        // 整个取消过程只写**一帧** CANCEL：它是「重建握手」的开场白，不是可重复的
        // 指令。第二次点「取消 → 保留」会被 control::cancel_receive 的幂等闸门挡掉；
        // 等待循环里通常已经抢先把这一帧写出去了（claim_cancel_frame），这里只是兜底
        // （比如线程已经全部正常退出、等待循环没机会发现取消）。
        if crate::control::claim_cancel_frame() {
            let _ = proto::write_i16(&mut self.writer, controller::CANCEL);
            let _ = self.writer.flush();
        }
        let names: Vec<String> = self.transfers.iter().map(|(name, _)| name.clone()).collect();
        if names.is_empty() {
            return Ok(0);
        }
        // 等手机端 ACK 的总窗口。手机那边要"等传输线程退出（最长 15s）+ 每条新通道
        // 400ms 分类 + takeTransferChannel"，6 条要 2~12 秒；以前只给 20 秒，
        // 实测（PC 端「中断发送」）会超时把整条会话拆掉。给到 45 秒，超时仍然按失败处理。
        let _ = self
            .reader
            .set_read_timeout(Some(Duration::from_secs(45)));
        let mut rebuilt: Vec<(String, TcpStream)> = Vec::with_capacity(names.len());
        for name in names {
            let interface = match self.interfaces.iter().find(|iface| iface.name == name) {
                Some(interface) => interface.clone(),
                None => {
                    log::warn(&format!("rebuild: unknown channel {name}"));
                    continue;
                }
            };
            // 回环通道走 adb 转发的本地端口，其它网卡走手机监听的端口
            let port = if interface.address.is_loopback() {
                self.local_port
            } else {
                self.port
            };
            let target = SocketAddr::new(interface.address, port);
            let stream = match crate::netif::connect(target, CONNECT_TIMEOUT, interface.bind) {
                Ok(stream) => stream,
                Err(e) => {
                    log::warn(&format!(
                        "{} {}: {} — {}",
                        crate::i18n::tr("重建通道失败", "cannot rebuild channel"),
                        name,
                        target,
                        e
                    ));
                    continue;
                }
            };
            let _ = stream.set_nodelay(true);
            let _ = stream.set_read_timeout(Some(TRANSFER_TIMEOUT));
            let _ = stream.set_write_timeout(Some(TRANSFER_TIMEOUT));
            proto::write_i16(&mut self.writer, controller::REBUILD_REQ)?;
            proto::write_utf(&mut self.writer, &name)?;
            self.writer.flush()?;
            loop {
                let id = match self.pending_opcode.take() {
                    Some(id) => id,
                    None => proto::read_i16(&mut self.reader)?,
                };
                match id {
                    controller::REBUILD_ACK => {
                        if proto::read_bool(&mut self.reader)? {
                            log::debug(&format!("rebuild: channel {} is back", name));
                            rebuilt.push((name.clone(), stream));
                        } else {
                            log::warn(&format!(
                                "{} {}",
                                crate::i18n::tr("手机端没有收下通道", "the phone refused the channel"),
                                name
                            ));
                        }
                        break;
                    }
                    controller::RESULT => self.discard_result()?,
                    // 重建期间手机端照旧会发暂停同步、翻目录、取消：必须当场应答，
                    // 否则它那边会卡在 controlLock 上等应答（界面的目录请求），
                    // 重建循环也跟着停摆。以前这里一律 break + 不读载荷，帧流直接错位。
                    controller::PING
                    | controller::CANCEL
                    | controller::CANCEL_DISCARD
                    | controller::PAUSE_STATE
                    | controller::LIST_FILES
                    | controller::DELETE_FILE
                    | controller::MKDIR
                    | controller::CHECK_PENDING => self.handle_control_frame(id)?,
                    other => {
                        log::warn(&format!(
                            "{} {}",
                            crate::i18n::tr(
                                "重建通道时收到意外的控制帧（已放弃重建）",
                                "unexpected frame while waiting for REBUILD_ACK",
                            ),
                            other
                        ));
                        break;
                    }
                }
            }
        }
        if rebuilt.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "no transfer channel could be rebuilt",
            ));
        }
        let count = rebuilt.len();
        self.transfers = rebuilt;
        // 回到主循环前把轮询用的超时恢复过来
        let _ = self
            .reader
            .set_read_timeout(Some(Duration::from_millis(500)));
        Ok(count)
    }

    /// 304：传输线程都退出了，但手机的 CANCEL 可能还在路上
    /// （它先写 CANCEL 再关传输通道，两个 socket 之间没有先后保证）。
    /// 收尾前再轮询一小会儿，别把"取消"误判成"断线"。
    fn grace_poll_cancel(&mut self, budget_ms: u64) {
        if crate::control::cancelled() {
            return;
        }
        let deadline = Instant::now() + Duration::from_millis(budget_ms);
        while Instant::now() < deadline {
            if self.poll_control_during_transfer().is_err() {
                return;
            }
            if crate::control::cancelled() {
                return;
            }
            thread::sleep(Duration::from_millis(30));
        }
    }

    /// 取消收尾：把传输通道重建回来（会话不断）；重建失败才真的断线。
    fn finish_cancelled(&mut self) -> io::Result<()> {
        match self.rebuild_transfers() {
            Ok(count) => {
                log::info(&format!(
                    "{} {} {} {}",
                    crate::i18n::tr("取消后已重建通道", "rebuilt after cancel:"),
                    count,
                    crate::i18n::tr("条，会话保持连接", "channel(s), the session stays up"),
                    "",
                ));
                Ok(())
            }
            Err(e) => {
                log::warn(&format!(
                    "{}: {}",
                    crate::i18n::tr("重建传输通道失败", "rebuilding the transfer channels failed"),
                    e
                ));
                Err(e)
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

    /// 暂停状态同步：手机发它当前是否暂停，电脑回自己是否暂停。
    /// 只有手机这一侧状态**变了**才应用到本地，避免电脑自己点的暂停被手机上报的旧状态冲掉。
    fn handle_pause_state(&mut self) -> io::Result<()> {
        let phone = proto::read_bool(&mut self.reader)?;
        self.apply_pause_state(phone)
    }

    /// 应用手机端的暂停状态并回报电脑自己的状态
    fn apply_pause_state(&mut self, phone: bool) -> io::Result<()> {
        if phone != crate::control::phone_paused() {
            crate::control::set_phone_paused(phone);
            crate::control::pause(phone);
            log::info(crate::i18n::tr(
                if phone { "手机端暂停了传输" } else { "手机端继续传输" },
                if phone { "the phone paused the transfer" } else { "the phone resumed the transfer" },
            ));
        }
        // 302：回报带 opcode。手机端传输期间只有一个读循环（发送线程），
        // 裸 bool 会被它当成别的帧读串。
        proto::write_i16(&mut self.writer, controller::PAUSE_REPLY)?;
        proto::write_bool(&mut self.writer, crate::control::paused())
    }

    /// 303：写一帧结论 `[i16 RESULT][bool ok][i32 n][(i64 size, 32B sha256)*n][utf 错误信息?]`.
    /// 两个方向共用：接收数据的一方报"落盘结论 + 本端落盘文件的指纹"，
    /// 发送数据的一方报"读文件结论"（指纹表为空）。
    fn write_result(
        &mut self,
        ok: bool,
        error: Option<&str>,
        digests: &[FileDigest],
    ) -> io::Result<()> {
        proto::write_i16(&mut self.writer, controller::RESULT)?;
        proto::write_bool(&mut self.writer, ok)?;
        proto::write_i32(&mut self.writer, digests.len() as i32)?;
        for digest in digests {
            proto::write_i64(&mut self.writer, digest.size as i64)?;
            self.writer.write_all(&digest.sha256)?;
        }
        self.writer.flush()?;
        if let Some(message) = error {
            proto::write_utf(&mut self.writer, message)?;
            self.writer.flush()?;
        }
        Ok(())
    }

    /// 303：读一帧结论（含对端产出的文件指纹）。期间遇到手机发来的 PAUSE_STATE /
    /// PING 就地处理 —— 收尾时手机端的暂停同步还在飞，不处理就会把结论读串。
    fn read_result(&mut self) -> io::Result<PeerResult> {
        loop {
            let id = match self.pending_opcode.take() {
                Some(id) => id,
                None => proto::read_i16(&mut self.reader)?,
            };
            log::debug(&format!("waiting phone RESULT: frame {}", id));
            match id {
                controller::RESULT => {
                    let ok = proto::read_bool(&mut self.reader)?;
                    let count = proto::read_i32(&mut self.reader)?.max(0) as usize;
                    let mut digests = Vec::with_capacity(count.min(4096));
                    for _ in 0..count {
                        let size = proto::read_i64(&mut self.reader)?;
                        let mut sha256 = [0u8; 32];
                        self.reader.read_exact(&mut sha256)?;
                        digests.push(FileDigest {
                            size: size.max(0) as u64,
                            sha256,
                        });
                    }
                    let message = if ok {
                        None
                    } else {
                        Some(proto::read_utf(&mut self.reader).unwrap_or_default())
                    };
                    return Ok(PeerResult {
                        ok,
                        message,
                        digests,
                        cancelled: false,
                    });
                }
                controller::PAUSE_STATE => {
                    let phone = proto::read_bool(&mut self.reader)?;
                    self.apply_pause_state(phone)?;
                }
                controller::PING => {}
                // 304：手机在这个窗口里点取消是正常的 —— 数据可能已经收全、本端正在
                // 算 SHA-256 / 等它的结论帧，用户完全可能这时候按取消。以前这里把它当
                // 「帧序错位」直接断开整条会话（实测日志：unexpected control frame 25
                // while waiting for RESULT），取消又变成断线。现在置取消标志，
                // 由调用方走「取消后重建通道」的收尾。
                controller::CANCEL => {
                    log::warn(crate::i18n::tr(
                        "手机端取消了传输",
                        "the phone cancelled the transfer",
                    ));
                    crate::control::cancel();
                    return Ok(PeerResult::cancelled());
                }
                // 同上，只是手机端还选了「不保留进度」：先记下来，收尾时把这次
                // 写到一半的文件和 .hfxpart 一起删掉（见 handle_receive 的 finish_with）
                controller::CANCEL_DISCARD => {
                    log::warn(crate::i18n::tr(
                        "手机端取消了传输（不保留已传的部分）",
                        "the phone cancelled the transfer and asked to discard it",
                    ));
                    crate::control::set_discard_partial(true);
                    crate::control::cancel();
                    return Ok(PeerResult::cancelled());
                }
                // 收尾 / 算校验这两分钟里手机界面照样会翻目录、问待推送文件，而且它
                // 是在自己的锁里等应答：必须当场回，否则它卡住、重建也跟着停摆。
                controller::LIST_FILES => self.handle_list_files()?,
                controller::DELETE_FILE => self.handle_delete()?,
                controller::MKDIR => self.handle_mkdir()?,
                controller::CHECK_PENDING => self.handle_check_pending()?,
                controller::SHUTDOWN => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "phone shut the session down",
                    ))
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected control frame {} while waiting for RESULT", other),
                    ));
                }
            }
        }
    }

    /// 传输进行中也要服务控制通道。
    ///
    /// 手机端每 250ms 会发一条 PAUSE_STATE（带着它自己的暂停状态、并读回电脑的状态）。
    /// 如果电脑在 handle_send / handle_receive 里一直等到传输结束才读控制通道，
    /// 暂停状态就要等整个传输传完才能同步 —— 表现就是"点了暂停另一边没反应"。
    ///
    /// 这里用 peek 偷看：只有拿到完整的 2 字节指令头才去读，避免把半个指令头吃掉。
    /// PING / PAUSE_STATE / CHECK_PENDING 直接处理，其它指令暂存给主循环，
    /// 传输结束后按原顺序继续。
    /// 控制通道已经没了（手机被杀 / 拔线 / 服务被停止）：把传输 socket 也一起 shutdown。
    ///
    /// 不这么做，卡在 read/write 里的传输线程会一直等到 socket 的 120 秒超时才醒，
    /// 期间界面和日志整段冻住 —— 实测 am force-stop 之后 PC 静默 120 秒才去删半截文件。
    /// shutdown 之后阻塞中的读写立刻返回，收尾（删半截文件、写结论帧）马上做。
    fn shutdown_transfers(&self) {
        for (_, stream) in self.transfers.iter() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    fn poll_control_during_transfer(&mut self) -> io::Result<()> {
        if self.pending_opcode.is_some() {
            return Ok(());
        }
        let _ = self
            .reader
            .set_read_timeout(Some(Duration::from_millis(2)));
        // 先 peek 出足够判断"整帧到齐没有"的字节再动手。
        // 只在 peek 到 2 字节就去读 opcode 是有坑的：PAUSE_STATE 是 opcode + 1 字节参数，
        // 参数可能还在下一个 TCP 片段里 —— 那时读出 opcode、等 300ms 超时、把 opcode
        // 记成 pending，控制通道上就留下了一个孤零零的 bool（0x00）。之后下一次读
        // 正好是"残留的 00 + 下一帧的高位 00"，于是读出 opcode 0（SHUTDOWN），
        // 整条控制通道彻底错位。所以这里按帧长判断：不够 3 字节就一个字节都不读。
        let mut window = [0u8; 3];
        // None = 这一轮没数据（超时）；0 = 对端已关闭（EOF）。两者必须分开：
        // 把 EOF 也当成"没数据"，等待循环就永远转下去 —— 手机取消/断线后
        // handle_receive 收不了尾，界面和日志一起定格在「已暂停」。
        let available = match self.reader.peek(&mut window) {
            Ok(n) => Some(n),
            Err(ref e) if is_timeout(e) => None,
            Err(e) => return Err(e),
        };
        let Some(available) = available else {
            let _ = self
                .reader
                .set_read_timeout(Some(Duration::from_secs(120)));
            return Ok(());
        };
        if available == 0 {
            // 对端把控制通道关了：叫醒还在暂停里等的传输线程，再报给上层
            crate::control::cancel();
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "control channel closed by peer",
            ));
        }
        if available < 2 {
            // 没数据或半个指令头，下一轮再看
            let _ = self
                .reader
                .set_read_timeout(Some(Duration::from_secs(120)));
            return Ok(());
        }
        let id = i16::from_be_bytes([window[0], window[1]]);
        let payload_len = match id {
            controller::PAUSE_STATE => 1,
            _ => 0,
        };
        if available < 2 + payload_len {
            // 帧还没到齐，等它齐了再读，绝不把半个帧吃掉
            let _ = self
                .reader
                .set_read_timeout(Some(Duration::from_secs(120)));
            return Ok(());
        }
        let id = proto::read_i16(&mut self.reader)?;
        let result = self.handle_control_frame(id);
        let _ = self
            .reader
            .set_read_timeout(Some(Duration::from_secs(120)));
        result?;
        Ok(())
    }

    /// 传输 / 重建期间收到一帧控制指令。
    ///
    /// 能当场处理的必须当场处理：手机界面在传输和重建期间照样会发
    /// LIST_FILES / CHECK_PENDING，并且在它自己的锁里等应答 —— 推迟处理就会把
    /// 手机端那个调用堵死，连带把暂停同步和写侧一起堵住（实测卡 100 秒）。
    fn handle_control_frame(&mut self, id: i16) -> io::Result<()> {
        match id {
            controller::PING => {}
            controller::CHECK_PENDING => self.handle_check_pending()?,
            controller::PAUSE_STATE => {
                let phone = proto::read_bool(&mut self.reader)?;
                log::debug(&format!("control: PAUSE_STATE {} during transfer", phone));
                self.apply_pause_state(phone)?;
            }
            // 303：手机端点了「取消」。以前取消只有硬断 socket，电脑端在线程里
            // 既等不到数据也等不到错误（暂停时连读都不读），整条会话就卡死在这里。
            // 现在就地置取消标志：所有传输线程下一轮检查时干净退出。
            controller::CANCEL => {
                log::warn(crate::i18n::tr(
                    "手机端取消了传输",
                    "the phone cancelled the transfer",
                ));
                crate::control::cancel();
            }
            // 304 + 取消确认框：手机端取消时选了「不保留进度」——
            // 除了当取消处理，还要让收尾把半截文件和续传日志删掉
            controller::CANCEL_DISCARD => {
                log::warn(crate::i18n::tr(
                    "手机端取消了传输（不保留已传的部分）",
                    "the phone cancelled the transfer and asked to discard it",
                ));
                crate::control::set_discard_partial(true);
                crate::control::cancel();
            }
            controller::RESULT => {
                // 手机端的落盘结论可能在本机发送线程收尾那一瞬间到：先记下指令头，
                // 载荷由收尾时的 read_result() 接着读，不能当成不相干的指令丢掉
                self.pending_opcode = Some(controller::RESULT);
            }
            // 手机界面在传输期间也会翻目录/改文件，这些同步命令必须当场应答
            controller::LIST_FILES => self.handle_list_files()?,
            controller::DELETE_FILE => self.handle_delete()?,
            controller::MKDIR => self.handle_mkdir()?,
            // 手机端正常断开前会先发一帧 SHUTDOWN：留成 pending 会让轮询永久
            // 「有货待处理」而不再读通道，连 EOF 都发现不了，等待循环就永远转下去
            // （实测卡死 3 分钟以上，传输线程也醒不过来）。直接当断线报上去。
            controller::SHUTDOWN => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "phone shut the session down",
                ))
            }
            other => {
                // 传输期间正常不会出现别的指令；先记下来，传输结束后处理
                log::warn(&format!(
                    "{} {}（传输结束后处理）",
                    crate::i18n::tr("传输中收到控制指令", "control command during transfer"),
                    other
                ));
                self.pending_opcode = Some(other);
            }
        }
        Ok(())
    }

    /// 读掉一帧 RESULT 并记一笔（时序错位时的兜底，避免把载荷留成半个帧）
    fn discard_result(&mut self) -> io::Result<()> {
        let ok = proto::read_bool(&mut self.reader)?;
        let count = proto::read_i32(&mut self.reader)?.max(0);
        for _ in 0..count {
            let _size = proto::read_i64(&mut self.reader)?;
            let mut sha256 = [0u8; 32];
            self.reader.read_exact(&mut sha256)?;
        }
        let message = if ok {
            String::new()
        } else {
            proto::read_utf(&mut self.reader).unwrap_or_default()
        };
        log::warn(&format!(
            "{} ok={} {}",
            crate::i18n::tr("空闲时收到传输结论帧（已丢弃）", "unexpected RESULT frame while idle, dropped"),
            ok,
            message
        ));
        Ok(())
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

    /// 306：读手机作为接收端上报的"已经收到哪些块"（电脑→手机方向的续传）
    fn read_resume_state(&mut self) -> io::Result<HashMap<String, Vec<u8>>> {
        let id = proto::read_i16(&mut self.reader)?;
        if id != controller::RESUME_STATE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected RESUME_STATE, got {}", id),
            ));
        }
        let count = proto::read_i32(&mut self.reader)?.max(0) as usize;
        let mut map: HashMap<String, Vec<u8>> = HashMap::new();
        for _ in 0..count {
            let dest = proto::read_utf(&mut self.reader)?;
            let _size = proto::read_i64(&mut self.reader)?;
            let _mtime = proto::read_i64(&mut self.reader)?;
            let len = proto::read_i32(&mut self.reader)?.max(0) as usize;
            let len = len.min(4 * 1024 * 1024);
            let mut bits = vec![0u8; len];
            if len > 0 {
                self.reader.read_exact(&mut bits)?;
            }
            if !bits.is_empty() {
                map.insert(dest, bits);
            }
        }
        Ok(map)
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
        // 306：手机紧接着回一帧"我已经有哪些块"（电脑→手机方向的断点续传）
        let resume = self.read_resume_state()?;

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
        // 每次"手机来取文件"都重置会话计数，避免"已接收 10.41 GB"把上几次的也算进来
        crate::receiver::PROGRESS.reset();

        let base = PathBuf::from(&phone_dir);
        let mut tasks: Vec<SendTask> = Vec::new();
        let mut next_index = 0i32;
        for source in &sources {
            collect_send_tasks(Path::new(source), &base, Path::new(source), &mut tasks, &mut next_index);
        }
        // 收尾算 SHA-256 用：按"这次涉及的全部文件"算，不能按过滤后的任务算 ——
        // 已经全在手机上的文件不会产生任何块任务，但它照样要参与两端比对。
        let all_files = source_files_of(&tasks);
        if resume.is_empty() {
            if tasks.is_empty() {
                log::warn(crate::i18n::tr("没有可发送的文件", "nothing to send"));
            }
        } else {
            let mut skipped = 0usize;
            tasks.retain(|task| match task {
                SendTask::Block { dest, index, .. } => match resume.get(dest) {
                    Some(bits) if resume_has_block(bits, *index) => {
                        skipped += 1;
                        false
                    }
                    _ => true,
                },
                _ => true,
            });
            log::info(&format!(
                "{} {} {} {}",
                crate::i18n::tr("续传：跳过已有块", "resume: skipping blocks already on the phone"),
                skipped,
                crate::i18n::tr("块，剩余待发", "block(s), tasks left:"),
                tasks.len()
            ));
        }

        // IDM 式动态分段：一开始按通道数把任务表切成连续区间（一条通道一段），
        // 谁先传完自己的段，就把剩余最多的一段再切一半过来。手机端的进度条
        // 按"哪个通道送了第几块"还原这些段，段数会随传输过程变化。
        let tasks = Arc::new(tasks);
        let plan = Arc::new(Mutex::new(SendPlan::new(tasks.len(), self.transfers.len())));
        // 一开始就是"通道数"段，之后谁抢了别人的段，新的边界会一直留着
        reset_send_segments();
        if let Ok(guard) = plan.lock() {
            publish_segments(&guard);
        }
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
        // 等各通道结束的同时继续服务控制通道：暂停同步不能等整个传输结束
        let channel_count = handles.len();
        let mut peer_closed = 0usize;
        let mut finished = vec![false; handles.len()];
        let mut remaining = handles.len();
        while remaining > 0 {
            if let Err(e) = self.poll_control_during_transfer() {
                log::warn(&format!(
                    "{}: {}",
                    crate::i18n::tr("控制通道断开", "control channel closed"),
                    e
                ));
                // 手机被杀 / 拔线：别让发送线程干等 120s 超时
                self.shutdown_transfers();
                break;
            }
            for (i, (_, handle)) in handles.iter().enumerate() {
                if !finished[i] && handle.is_finished() {
                    finished[i] = true;
                    remaining -= 1;
                }
            }
            if remaining > 0 {
                thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = self
            .reader
            .set_read_timeout(Some(Duration::from_secs(120)));
        for (name, handle) in handles {
            match handle.join() {
                Ok(Ok(bytes)) => {
                    log::info(&format!("{}: {} {}", name, crate::i18n::tr("已发送", "sent"), crate::receiver::human_bytes(bytes)));
                }
                Ok(Err(e)) => {
                    // 手机端点取消时本端线程以 Interrupted 退出，这不是发送失败
                    if crate::control::cancelled()
                        || e.kind() == io::ErrorKind::Interrupted
                        || is_disconnect(&e)
                    {
                        peer_closed += 1;
                        log::debug(&format!("{}: cancelled ({})", name, e));
                    } else {
                        log::error(&format!("{}: {}", name, e));
                        if hard_error.is_none() {
                            hard_error = Some(format!("{}: {}", name, e));
                        }
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

        // 302 收尾（与手机端 HFXService.receiveFiles() 对齐）：
        //   手机：RESULT{落盘结论} → 读电脑的 RESULT{读文件结论}
        //   电脑：先读手机的 RESULT，再回自己的 RESULT
        // 发一半断了：把界面上的进度清掉，别留个停住不动的进度条
        if hard_error.is_some() {
            crate::receiver::PROGRESS.reset();
            reset_send_segments();
        }

        // 304：取消标记可能还差几毫秒才到（手机先写 CANCEL 再关传输通道，
        // 两个 socket 之间没有先后保证），收尾前再等一会儿，别把取消当断线。
        self.grace_poll_cancel(600);
        // 手机端取消只关传输通道：所有通道都是被对端关掉 / 中断退出，也算取消，
        // 哪怕 CANCEL 帧丢了（它先写 CANCEL 再关通道，两个 socket 没有先后保证）。
        let peer_went_away = hard_error.is_none() && peer_closed > 0 && peer_closed >= channel_count;
        if crate::control::cancelled() || peer_went_away {
            // 别再去读结论帧：手机端这次传输也作废了，不会写。
            // 304：它只关了传输通道、控制通道留着，正等着我们把通道连回去。
            log::warn(crate::i18n::tr("发送已取消", "the transfer was cancelled"));
            crate::receiver::PROGRESS.reset();
            reset_send_segments();
            return self.finish_cancelled();
        }

        // 303：本端是发送方，先把自己发出去的文件算一遍指纹（和手机端算它落盘
        // 文件的指纹同时进行），再读手机的结论帧比对。
        let mine = if hard_error.is_none() {
            digest_files(&all_files)
        } else {
            Vec::new()
        };
        let mut verify_error: Option<String> = None;
        match self.read_result() {
            // 304：手机在等结论的窗口里取消（数据已经发完）—— 重建通道，会话不断
            Ok(peer) if peer.cancelled => {
                log::warn(crate::i18n::tr(
                    "发送已取消",
                    "the transfer was cancelled",
                ));
                crate::receiver::PROGRESS.reset();
                reset_send_segments();
                return self.finish_cancelled();
            }
            Ok(peer) => {
                if let Some(message) = peer.message.clone().filter(|_| !peer.ok) {
                    log::error(&format!(
                        "{}: {}",
                        crate::i18n::tr("手机写入文件时出错", "the phone failed to write a file"),
                        message
                    ));
                }
                if hard_error.is_none() && !peer.digests.is_empty() {
                    match compare_digests(&mine, &peer.digests) {
                        None => {
                            log::info(&format!(
                                "{} {} {}",
                                crate::i18n::tr("校验通过：", "verified:"),
                                mine.len(),
                                crate::i18n::tr(
                                    "个文件 SHA-256 与手机端一致",
                                    "file(s) match the phone byte for byte"
                                )
                            ));
                        }
                        Some(reason) => {
                            log::error(&format!(
                                "{} {}",
                                crate::i18n::tr("文件校验失败：", "file verification failed:"),
                                reason
                            ));
                            verify_error = Some(format!(
                                "{} {}",
                                crate::i18n::tr("文件校验失败：", "file verification failed:"),
                                reason
                            ));
                        }
                    }
                }
            }
            // 对端已经没了：下面写结论会再报一次错，这里不额外放大
            Err(e) => log::warn(&format!(
                "{}: {}",
                crate::i18n::tr("读取手机端的结论失败", "reading the phone's verdict failed"),
                e
            )),
        }
        match hard_error.clone().or(verify_error) {
            None => self.write_result(true, None, &[])?,
            Some(message) => {
                log::warn(&format!("send finished with error: {message}"));
                self.write_result(false, Some(&message), &[])?;
            }
        }
        Ok(())
    }

    /// 305：读手机的续传查询，回每个文件"已经收到哪些块"的位图。
    ///
    /// 帧格式见 {@link proto::controller::RESUME_QUERY} / {@link proto::controller::RESUME_REPLY}。
    /// 手机端在 `REQUEST_RECEIVE` 之后立刻发这一帧，所以这里可以同步读。
    fn handle_resume_query(&mut self, receiver: &Arc<Receiver>) -> io::Result<()> {
        let id = proto::read_i16(&mut self.reader)?;
        if id != controller::RESUME_QUERY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected RESUME_QUERY, got {}", id),
            ));
        }
        let count = proto::read_i32(&mut self.reader)?.max(0) as usize;
        let mut entries: Vec<(String, u64, i64)> = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            let path = proto::read_utf(&mut self.reader)?;
            let size = proto::read_i64(&mut self.reader)?.max(0) as u64;
            let mtime = proto::read_i64(&mut self.reader)?;
            entries.push((path, size, mtime));
        }
        proto::write_i16(&mut self.writer, controller::RESUME_REPLY)?;
        proto::write_i32(&mut self.writer, entries.len() as i32)?;
        let mut resumed = 0usize;
        // 命中续传的文件：位图留着，稍后把「磁盘上已有的块」直接标成本次进度的已完成
        let mut resumed_files: Vec<(String, u64, Vec<u8>)> = Vec::new();
        for (path, size, mtime) in &entries {
            match receiver.resume_bitmap(path, *size, *mtime) {
                Some(bitmap) => {
                    resumed += 1;
                    let done = bitmap.iter().map(|b| b.count_ones() as u64).sum::<u64>();
                    log::info(&format!(
                        "{} {} ({} {})",
                        crate::i18n::tr("续传：已有块", "resume: blocks already on disk"),
                        done,
                        crate::receiver::human_bytes(*size),
                        path
                    ));
                    resumed_files.push((path.clone(), *size, bitmap.clone()));
                    proto::write_bool(&mut self.writer, true)?;
                    proto::write_i32(&mut self.writer, bitmap.len() as i32)?;
                    self.writer.write_all(&bitmap)?;
                }
                None => {
                    proto::write_bool(&mut self.writer, false)?;
                    proto::write_i32(&mut self.writer, 0)?;
                }
            }
        }
        self.writer.flush()?;
        log::info(&format!(
            "{} {}/{}",
            crate::i18n::tr("续传：可续传的文件", "resumable files"),
            resumed,
            entries.len()
        ));
        // 305：口径统一 —— 续传时「磁盘上已有的块」= 已完成（手机端也是这么算的）。
        // 以前只有本轮真收到的块才会计账，跳过的区间既没有归属也不算完成，
        // 进度条就成了「最左边一小段 + 中间一大片空白 + 右边一串碎段」，
        // 而「已接收」的数字却是含这些块的，两个数字对不上。
        for (path, size, bits) in &resumed_files {
            let blocks = crate::receiver::PROGRESS.mark_resumed(path, *size, bits);
            receiver.progress.mark_resumed(path, *size, bits);
            log::info(&format!(
                "{} {} / {} ({})",
                crate::i18n::tr("续传：本次按已完成计的块数", "resume: blocks counted as done"),
                blocks,
                crate::i18n::tr("共", "of"),
                crate::receiver::human_bytes(*size)
            ));
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
        // 本次接收从头计数：上一次传过的字节/文件不能算进来（否则收尾日志会写「4 个文件 5.48 GB」）。
        // 必须排在续传查询之前 —— 查询会把"已经全在"的文件登记进本次会话（收尾要复算它的 SHA-256），
        // 排在后面就会把这些登记一起清掉，日志里出现「0 个文件」。
        receiver.reset_session_counters();
        // 每次接收都要重置进度：不清的话上一条会话里传过的文件还留在 files 表里，
        // 界面会显示「已接收 9.75 GB · 3 个文件」（其实是同一个文件传了三次）。
        //
        // 必须排在续传查询**之前**：查询会把「磁盘上已有的块」标成本次进度的已完成，
        // 排在后面就把这些标记一起清掉了，进度条又变回「大片空白 + 一串碎段」。
        crate::receiver::PROGRESS.reset();
        receiver.progress.reset();
        // 305：手机发完 REQUEST_RECEIVE 会立刻来一帧 RESUME_QUERY（这次要传的文件清单），
        // 这里读它、回每个文件"已经收到哪些块"的位图，发送端据此跳过已收到的块；
        // 同时把这些块直接标进进度（手机端也是把它们当完成的）。
        self.handle_resume_query(receiver)?;
        // 接收方向没有发送计划：清掉上一次的发送分段，界面改按"谁写了第几块"还原
        reset_send_segments();
        // 接收期间界面显示「暂停 / 已暂停」，暂停时通知手机端停发
        let _receiving = crate::control::ReceivingGuard::begin();
        let started = Instant::now();
        let mut handles = Vec::with_capacity(self.transfers.len());
        for (channel, (name, stream)) in self.transfers.iter().enumerate() {
            let stream = stream.try_clone()?;
            let receiver = Arc::clone(receiver);
            let name = name.clone();
            let channel = channel as u8;
            handles.push((
                name,
                thread::spawn(move || read_transfer_channel(stream, receiver, channel)),
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
        // 等各通道结束的同时继续服务控制通道：手机端的暂停同步不能等整个传输结束
        let channel_count = handles.len();
        let mut finished = vec![false; handles.len()];
        let mut remaining = handles.len();
        // 取消时"关掉所有传输通道"只做一次（关 socket 是幂等的，但没必要每 10ms 打一遍）
        let mut abort_shutdown = false;
        while remaining > 0 {
            if let Err(e) = self.poll_control_during_transfer() {
                log::warn(&format!(
                    "{}: {}",
                    crate::i18n::tr("控制通道断开", "control channel closed"),
                    e
                ));
                // 对端没打招呼就走了：别让传输线程干等 120s 超时
                self.shutdown_transfers();
                break;
            }
            // 3.0.0：本地点了「取消」（control::cancel_receive）就立刻把传输通道全关掉。
            // 手机端是发送方，它写满缓冲就阻塞在 write 里，看不到控制通道；
            // 把 socket 关掉它的写才会失败、才会自己停下。放在这里而不是只放在
            // rebuild_transfers：那一步要等所有线程 join + finish_with 写完续传日志，
            // 中间那几百毫秒到几秒手机一直在发（用户报的"取消后手机还在继续发"）。
            if !abort_shutdown && crate::control::cancelled() {
                abort_shutdown = true;
                log::warn(crate::i18n::tr(
                    "取消接收：先关传输通道，再通知手机端（它才能立刻停下）",
                    "cancel: closing the transfer channels first, then telling the phone",
                ));
                // 顺序很重要：先关通道，手机端那次阻塞的 write 才会立刻失败；
                // 它这一下自己就会停，不用等我们后面那帧 CANCEL 走到重建阶段。
                self.shutdown_transfers();
                // 立刻把 CANCEL 写出去（别等 join + finish_with + rebuild_transfers）。
                // 手机端的控制读取线程和发块线程是两个线程，它随时能读到这一帧。
                // claim_cancel_frame() 保证重建阶段不会再写第二帧。
                if crate::control::claim_cancel_frame() {
                    let _ = proto::write_i16(&mut self.writer, controller::CANCEL);
                    let _ = self.writer.flush();
                    log::warn(crate::i18n::tr(
                        "已通知手机端取消（CANCEL 帧已发出）",
                        "the phone has been told about the cancel (CANCEL sent)",
                    ));
                }
            }
            for (i, (_, handle)) in handles.iter().enumerate() {
                if !finished[i] && handle.is_finished() {
                    finished[i] = true;
                    remaining -= 1;
                }
            }
            if remaining > 0 {
                thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = self
            .reader
            .set_read_timeout(Some(Duration::from_secs(120)));
        for (name, handle) in handles {
            match handle.join() {
                Ok(Ok(ChannelEnd::Eof)) => {}
                Ok(Ok(ChannelEnd::RemoteAbort(reason))) => {
                    log::warn(&format!("{}: {}", name, reason));
                    aborts.push(format!("{}: {}", name, reason));
                }
                Ok(Err(e)) => {
                    // 用户取消时线程是以 Interrupted/连接被 reset 退出的，
                    // 那不是"接收失败"，别当成错误弹窗，也别写进错误结论帧。
                    if crate::control::cancelled()
                        || e.kind() == io::ErrorKind::Interrupted
                        || is_disconnect(&e)
                    {
                        log::debug(&format!("{}: cancelled ({})", name, e));
                        aborts.push(format!(
                            "{}: {}",
                            name,
                            crate::i18n::tr("已取消", "cancelled")
                        ));
                    } else {
                        log::error(&format!("{}: {}", name, e));
                        if hard_error.is_none() {
                            hard_error = Some(format!("{}: {}", name, e));
                        }
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
        // 304：传输线程退出了，但手机的 CANCEL 可能还在路上，先等一小会儿再判"是不是取消"
        if !aborts.is_empty() && !crate::control::cancelled() {
            self.grace_poll_cancel(600);
        }
        // 手机端"取消"只会关传输通道：所有通道都是被对端关掉退出（不是本地磁盘错），
        // 那就一定是取消 —— 哪怕 CANCEL 帧丢了（手机暂停时我们读不到控制通道，
        // 它发完 CANCEL 就关通道也是可能的）。真断线的话下面重建会失败，照样重连。
        let peer_went_away = !aborts.is_empty() && aborts.len() >= channel_count;

        let bytes = receiver.bytes();
        let files = receiver.file_count();
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        // 这次到底收全了没有。没传完要不要留在电脑上由下面的 keep_incomplete 决定：
        // 默认留着（续传），任意一端在取消时选了「不保留进度」才删。
        let complete = hard_error.is_none() && aborts.is_empty();
        // 303：先把落盘文件列表记下来，finish_with() 会把条目清空
        let received = receiver.received_files();
        // 2026-11-17：块账本核对 —— 传输线程全都正常退出了（手机端认为发完了），
        // 文件大小也一定和声明的一模一样（set_len 提前占满了整份），
        // 但账本里还缺块 / 缺字节，那就说明盘上的内容有洞。这正是
        // 「两边大小完全一致、SHA-256 不一致」的现场（NEXT-TODO 第 11 条）。
        //
        // 以前这一支当作"数据收齐"往下走：照算 SHA-256、照回 ok 的结论帧，
        // 只能等手机端报「SHA-256 不一致」，电脑端日志里一句解释都没有。
        // 现在电脑端自己就能指名道姓：哪个文件缺哪几块、哪几块只写了一半；
        // 并且按失败收尾 —— 续传位图里那几块是"未完成"，手机端重发时会被补上。
        if hard_error.is_none() && aborts.is_empty() {
            let gaps = receiver.missing_blocks();
            if !gaps.is_empty() {
                let mut parts: Vec<String> = Vec::new();
                for (path, indexes, count, short, short_count) in gaps.iter() {
                    let mut gaps_of_file: Vec<String> = Vec::new();
                    if *count > 0 {
                        let list: Vec<String> = indexes.iter().map(|i| format!("#{}", i)).collect();
                        gaps_of_file.push(format!(
                            "{} {} {}",
                            crate::i18n::tr("缺", "missing"),
                            count,
                            list.join(" ")
                        ));
                    }
                    if *short_count > 0 {
                        let list: Vec<String> = short
                            .iter()
                            .map(|(i, got, want)| format!("#{}({}/{})", i, got, want))
                            .collect();
                        gaps_of_file.push(format!(
                            "{} {} {}",
                            crate::i18n::tr("长度不足", "short"),
                            short_count,
                            list.join(" ")
                        ));
                    }
                    parts.push(format!("{} [{}]", path.display(), gaps_of_file.join(", ")));
                }
                let detail = parts.join("; ");
                log::error(&format!(
                    "{}: {}",
                    crate::i18n::tr(
                        "块账本与写盘不一致（文件大小会是对的，内容是坏的）",
                        "the block ledger disagrees with what is on disk (size right, content wrong)"
                    ),
                    detail
                ));
                if hard_error.is_none() {
                    hard_error = Some(format!(
                        "{}: {}",
                        crate::i18n::tr(
                            "块账本与写盘不一致（缺块/短块）",
                            "block ledger mismatch (missing/short blocks)"
                        ),
                        detail
                    ));
                }
            }
        }
        // 收尾阶段的可观测性：收尾时两边都在等对方，卡住时日志必须能指出卡在哪一步
        // （实测出现过「PC 界面已显示 800.0 MB 收满、手机停在等待校验」的僵持，日志里却什么都没有）
        log::info(&format!(
            "{} ({} {} / {} {})",
            crate::i18n::tr("收尾：数据收齐，开始落盘收尾", "finalising transfer"),
            files,
            crate::i18n::tr("个文件", "file(s)"),
            crate::receiver::human_bytes(bytes),
            ""
        ));
        // 取消确认框：不管选择是手机端给的（CANCEL_DISCARD）还是电脑端给的
        // （「当前文件」卡片上的「取消」→ 保留 / 不保留，见 control::cancel_receive），
        // 都汇总到 DISCARD_PARTIAL 这一个开关上：选了「不保留进度」，收尾就把这次
        // 写到一半的文件和 `.hfxpart` 续传记录一起删掉（下次同一个文件从头传）。
        // 必须在 `_receiving` 这个 guard 结束之前读 —— 它 drop 时会把 DISCARD_PARTIAL
        // 一起 reset 掉。
        let keep_incomplete = !crate::control::discard_partial();
        let cancelling = crate::control::cancelled() || peer_went_away;
        if !keep_incomplete {
            log::warn(crate::i18n::tr(
                "选择不保留进度：这次没传完的文件会被删掉",
                "discard chosen: incomplete files will be removed",
            ));
        } else if cancelling {
            log::warn(crate::i18n::tr(
                "取消接收：保留已收到的部分（可续传）",
                "cancel: keeping what already arrived (resumable)",
            ));
        }
        match receiver.finish_with(keep_incomplete) {
            Ok((kept, kept_bytes)) if kept > 0 => log::warn(&format!(
                "{} {} · {}",
                crate::i18n::tr(
                    "保留未接收完的文件（可续传）",
                    "kept incomplete files (resumable)"
                ),
                kept,
                crate::receiver::human_bytes(kept_bytes)
            )),
            Ok(_) => {}
            Err(e) => log::warn(&format!("finish(): {}", e)),
        }

        // 电脑端在「当前文件」上点了「取消」，而此时数据其实已经收齐（传输线程全部正常退出，
        // 现在正卡在算校验/等结论这两步里）：文件是完整的，不动它 —— 删除只针对没传完的文件；
        // 但这次传输按取消收尾 —— 不写结论帧、不比对指纹、把传输通道重建回来，
        // 手机端收到那帧 CANCEL 也会停在那里显示"已中断"，不会干等到超时。
        if hard_error.is_none() && aborts.is_empty() && crate::control::cancelled() {
            log::warn(crate::i18n::tr(
                "接收已取消（数据已收齐，文件保留）",
                "receiving cancelled after all data arrived; the files stay",
            ));
            crate::receiver::PROGRESS.reset();
            reset_send_segments();
            return self.finish_cancelled();
        }

        // 303：整文件 SHA-256 复算（本端是接收方，算自己落盘的文件）。
        // 和手机端算它发出去的文件同时进行，收尾只多这一遍读盘。
        log::info(&format!(
            "{} ({} {})",
            crate::i18n::tr("收尾：开始计算 SHA-256", "hashing received files"),
            received.len(),
            crate::i18n::tr("个文件", "file(s)")
        ));
        let digests = if complete && hard_error.is_none() {
            digest_files(&received)
        } else {
            Vec::new()
        };
        log::info(crate::i18n::tr("收尾：SHA-256 计算完成", "hashing done"));

        if hard_error.is_none() && !aborts.is_empty() && (crate::control::cancelled() || peer_went_away)
        {
            // 取消：没有错，只是这次传输作废（半截文件上一步已经删掉）。
            // 不写结论帧（手机端不会读），304：把传输通道重建回来，会话不断。
            log::warn(crate::i18n::tr(
                "接收已取消",
                "the transfer was cancelled",
            ));
            crate::receiver::PROGRESS.reset();
            reset_send_segments();
            return self.finish_cancelled();
        }

        if let Some(error) = hard_error {
            log::error(&format!(
                "{}: {}",
                crate::i18n::tr("接收失败", "receiving failed"),
                error
            ));
            // 结论帧带 opcode，手机端 sendFiles 正循环读帧等它
            let _ = self.write_result(false, Some(&error), &[]);
            return Ok(());
        }

        // 与手机端 sendFiles 对齐：电脑先报"落盘结论 + 文件指纹"，再读手机"读文件结论"。
        // 传输期间手机端的 PAUSE_STATE 还在飞，read_result 会就地处理掉。
        log::info(crate::i18n::tr(
            "收尾：回结论帧并等手机端结论",
            "sending RESULT and waiting for the phone's RESULT",
        ));
        self.write_result(true, None, &digests)?;
        log::info(crate::i18n::tr(
            "收尾：结论帧已写出，正在等手机端",
            "RESULT sent, waiting for the phone",
        ));
        let peer = self.read_result()?;
        log::info(crate::i18n::tr(
            "收尾：收到手机端结论",
            "got the phone's RESULT",
        ));
        if peer.cancelled {
            // 304：数据已经收全（上面那帧结论也写出去了），手机却在这一步点了取消。
            // 按「取消后重建通道」收尾，会话不断；那帧结论会被手机的重建握手丢弃。
            log::warn(crate::i18n::tr(
                "接收已取消",
                "the transfer was cancelled",
            ));
            crate::receiver::PROGRESS.reset();
            reset_send_segments();
            return self.finish_cancelled();
        }
        if let Some(message) = peer.message.clone().filter(|_| !peer.ok) {
            log::error(&format!(
                "{}: {}",
                crate::i18n::tr("手机端校验/读文件出错", "the phone reported a problem"),
                message
            ));
        }
        match peer.ok {
            true => {
                if aborts.is_empty() {
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
                    // 303：接收方向的 SHA-256 由手机端比对（它复算自己发出去的文件，
                    // 和本端回给它的落盘指纹逐字节比），这里只转述结论，
                    // 让两个方向都打同样一句确认，用户不用猜这次到底校验了没有。
                    if !digests.is_empty() {
                        log::info(&format!(
                            "{} {} {}",
                            crate::i18n::tr("校验通过：", "verified:"),
                            digests.len(),
                            crate::i18n::tr(
                                "个文件 SHA-256 与手机端一致",
                                "file(s) match the phone byte for byte"
                            )
                        ));
                        // 手机端分享文件夹时勾了「打包成归档发送」：文件名是
                        // <文件夹名>.hfx.zip。校验通过了才解档（要的就是「送达再解档」），
                        // 解完把归档本身删掉。
                        // 文件已经完整（手机端复算的 SHA-256 和本端一致）：续传记录留着不但没用，
                        // 还会让下次传同一个文件按旧位图"续传"（见 clear_resume_journals 的注释）。
                        // 放在解档之前：归档本身也要执行这一步。
                        clear_resume_journals(&received);
                        extract_archives(&received);
                    }
                } else {
                    log::warn(crate::i18n::tr(
                        "接收已中断",
                        "the transfer was interrupted",
                    ));
                }
            }
            false => {
                log::warn(crate::i18n::tr(
                    "手机端报告传输/校验失败",
                    "the phone reported a failed transfer or check",
                ));
                // 校验不一致：这次收下的文件是坏的，必须连同续传记录一起扔掉。
                // 留着它的后果是**修不好**：下一次传同一个文件时，接收端按这张位图回
                // 「这些块我都有了」，手机端就跳过它们 —— 坏块永远补不上，
                // 只有重启接收器（内存里的位图清空）才能传成功。
                // 那 4 次「大小一致但 SHA-256 不一致、重启接收器后重传正常」就是这么来的。
                let mismatch = peer
                    .message
                    .as_deref()
                    .map(|m| m.contains("SHA-256"))
                    .unwrap_or(false);
                if mismatch {
                    discard_received(&received);
                }
            }
        }
        Ok(())
    }
}

/// 解归档：把收到的 "*.hfx.zip" 解开到同名文件夹，然后删掉归档文件本身。
///
/// zip 里的条目是 "<文件夹名>/<相对路径>"（手机端就是这么打的），所以直接解到接收
/// 目录下就会得到一个同名文件夹。用系统自带的 tar 解（Windows 10+ 的 bsdtar 认得
/// zip），不引新依赖；解档失败就把归档留着，别让用户两头空。
/// 校验不一致时把收下的文件和它的续传记录一起删掉（见调用处的注释）。
fn discard_received(files: &[PathBuf]) {
    for path in files {
        crate::journal::ReceiveJournal::remove(path);
        match std::fs::remove_file(path) {
            Ok(()) => log::warn(&format!(
                "{} {} {}",
                crate::i18n::tr("校验不一致，已删除：", "hash mismatch, removed:"),
                path.display(),
                crate::i18n::tr("（下次从头重传）", "(next attempt starts over)"),
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn(&format!(
                "{} {}: {}",
                crate::i18n::tr("删除校验不一致的文件失败", "cannot remove the mismatched file"),
                path.display(),
                e
            )),
        }
    }
}

/// 校验通过之后，把这几份文件的断点续传记录（`<文件名>.hfxpart`）删掉。
///
/// 为什么非删不可：`Receiver::finish_with` 只在"位图认为已经传完"时才顺手删掉它，
/// 而实测会出现**位图没置全、文件却已经校验通过**的情况 ——
/// 900 MB 的文件传完、接收完成、校验通过之后，旁边还留着一个 145 字节的
/// `xxx.hfxpart`（145 = 32 字节头 + 113 字节位图，正是 900 块的那份旧记录）。
/// 留着它，下次传同一个文件就会按这张旧位图"续传"，从错误的偏移接着写 ——
/// 这正是「大小一致但 SHA-256 对不上」那条待办（NEXT-TODO 第 11 条）最可疑的成因。
fn clear_resume_journals(files: &[PathBuf]) {
    let mut removed = 0usize;
    for path in files {
        let sidecar = crate::journal::ReceiveJournal::sidecar_path(path);
        if sidecar.exists() {
            crate::journal::ReceiveJournal::remove(path);
            if !sidecar.exists() {
                removed += 1;
            }
        }
    }
    if removed > 0 {
        log::info(&format!(
            "{} {} {}",
            crate::i18n::tr("清掉续传记录：", "dropped resume journals:"),
            removed,
            crate::i18n::tr("个（文件已经完整，留着会让下次误判成续传）", ""),
        ));
    }
}

fn extract_archives(files: &[PathBuf]) {
    for path in files {
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => continue,
        };
        if !name.ends_with(".hfx.zip") {
            continue;
        }
        let parent = match path.parent() {
            Some(dir) => dir.to_path_buf(),
            None => continue,
        };
        log::info(&format!("{} {}", crate::i18n::tr("解档中：", "extracting:"), name));
        let status = std::process::Command::new("tar")
            .arg("-xf")
            .arg(path)
            .arg("-C")
            .arg(&parent)
            .status();
        match status {
            Ok(code) if code.success() => match std::fs::remove_file(path) {
                Ok(_) => log::info(&format!(
                    "{} {}",
                    crate::i18n::tr("解档完成（归档已删除）：", "extracted (archive removed):"),
                    name
                )),
                Err(e) => log::warn(&format!(
                    "{} {}: {}",
                    crate::i18n::tr(
                        "解档完成，但归档删不掉：",
                        "extracted, but could not delete the archive:"
                    ),
                    name,
                    e
                )),
            },
            Ok(code) => log::error(&format!(
                "{} {} (tar exit {:?})",
                crate::i18n::tr("解档失败，归档留着：", "extraction failed, archive kept:"),
                name,
                code.code()
            )),
            Err(e) => log::error(&format!(
                "{} {}: {}",
                crate::i18n::tr(
                    "解档失败（找不到 tar），归档留着：",
                    "extraction failed (no tar), archive kept:"
                ),
                name,
                e
            )),
        }
    }
}

/// 一个文件的指纹：字节数 + SHA-256。两端各自算一遍再比对，
/// 不带路径（两端的绝对路径本来就不一样），所以按"集合"比。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileDigest {
    pub size: u64,
    pub sha256: [u8; 32],
}

/// 对端（手机）的结论帧
pub struct PeerResult {
    /// 对端这一步是否成功（落盘成功 / 读文件成功）
    pub ok: bool,
    /// 失败原因（只有 ok == false 时有）
    pub message: Option<String>,
    /// 对端产出的文件指纹；本端是发送方时对端为空表
    pub digests: Vec<FileDigest>,
    /// 304：还没等到结论帧，手机先取消了这次传输（等结论的窗口里点取消）。
    /// 调用方据此走「取消后重建通道」，而不是把它当成帧序错位去断开整条会话。
    pub cancelled: bool,
}

impl PeerResult {
    /// 304：手机在等结论的窗口里取消了这次传输
    fn cancelled() -> PeerResult {
        PeerResult {
            ok: false,
            message: None,
            digests: Vec::new(),
            cancelled: true,
        }
    }
}

/// 算一个文件的 SHA-256 和大小（流式读，1MiB 一块，边读边算）
fn sha256_file(path: &Path) -> io::Result<FileDigest> {
    use ring::digest;
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut context = digest::Context::new(&digest::SHA256);
    // 4MiB 一块：1MiB 时 1.78GB 要 read 1800 次，收尾那 1 秒里有一截耗在系统调用上
    let mut buffer = vec![0u8; (BLOCK_SIZE as usize).max(4 * 1024 * 1024)];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }
    let finished = context.finish();
    let mut sha256 = [0u8; 32];
    sha256.copy_from_slice(finished.as_ref());
    Ok(FileDigest { size, sha256 })
}

/// 把一组本地文件算成指纹表（排序后便于两端直接比对）
///
/// 多文件时按文件并行：单个文件的 SHA-256 没法拆（后一段的初值依赖前一段），
/// 但"文件夹里几十个文件"这种收尾能快好几倍。单文件时就是一次顺序读。
fn digest_files(paths: &[PathBuf]) -> Vec<FileDigest> {
    if paths.len() <= 1 {
        return paths
            .iter()
            .filter_map(|path| match sha256_file(path) {
                Ok(digest) => Some(digest),
                Err(e) => {
                    log::warn(&format!("{}: {}", path.display(), e));
                    None
                }
            })
            .collect();
    }
    let lanes = paths.len().min(4).max(1);
    let mut buckets: Vec<Vec<&PathBuf>> = (0..lanes).map(|_| Vec::new()).collect();
    for (i, path) in paths.iter().enumerate() {
        buckets[i % lanes].push(path);
    }
    let mut digests: Vec<FileDigest> = std::thread::scope(|scope| {
        let handles: Vec<_> = buckets
            .into_iter()
            .map(|bucket| {
                scope.spawn(move || {
                    let mut local = Vec::with_capacity(bucket.len());
                    for path in bucket {
                        match sha256_file(path) {
                            Ok(digest) => local.push(digest),
                            Err(e) => log::warn(&format!("{}: {}", path.display(), e)),
                        }
                    }
                    local
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    digests.sort();
    digests
}

/// 比对两端指纹表；一致返回 None，不一致返回一句给人看的原因
fn compare_digests(mine: &[FileDigest], theirs: &[FileDigest]) -> Option<String> {
    if mine.len() != theirs.len() {
        return Some(format!(
            "{} {} / {} {}",
            crate::i18n::tr("文件数量不一致：本端", "file count differs: local"),
            mine.len(),
            crate::i18n::tr("对端", "remote"),
            theirs.len()
        ));
    }
    for (index, (a, b)) in mine.iter().zip(theirs.iter()).enumerate() {
        if a != b {
            return Some(format!(
                "{} {}（{} {} {} -> {}）",
                crate::i18n::tr("第", "file"),
                index + 1,
                crate::i18n::tr("SHA-256 不一致", "SHA-256 mismatch"),
                util::format_bytes(a.size),
                crate::i18n::tr("对端", "remote"),
                hex32(&b.sha256)
            ) + &format!(" {}/{}", hex32(&a.sha256), hex32(&b.sha256)));
        }
    }
    None
}

fn hex32(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

enum ChannelEnd {
    Eof,
    RemoteAbort(String),
}

fn read_transfer_channel(
    mut stream: TcpStream,
    receiver: Arc<Receiver>,
    channel: u8,
) -> io::Result<ChannelEnd> {
    let mut buffer = vec![0u8; BLOCK_SIZE as usize];
    loop {
        // 暂停：接收方向也不再读（TCP 缓冲自然挡住手机端的发送）；中断：退出。
        //
        // 但暂停期间必须还能发现"对端已经走了"：手机点取消/拔线时 socket 被关掉，
        // 如果这里只会 sleep，线程就永远等下去 —— handle_receive 的等待循环
        // 不会结束，界面和日志都定格在「已暂停」（实测就是这么卡死的）。
        // peek 不消费数据，暂停期间对端也不发数据，所以用它探活是安全的。
        let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
        let mut probe = [0u8; 1];
        while crate::control::paused() && !crate::control::cancelled() {
            match stream.peek(&mut probe) {
                Ok(0) => {
                    return Ok(ChannelEnd::RemoteAbort(
                        crate::i18n::tr("手机端取消了传输", "the phone cancelled the transfer")
                            .to_string(),
                    ))
                }
                Ok(_) => {}
                Err(ref e) if is_timeout(e) => {}
                Err(e) if is_disconnect(&e) => {
                    return Ok(ChannelEnd::RemoteAbort(
                        crate::i18n::tr(
                            "手机端断开了连接",
                            "the phone closed the connection",
                        )
                        .to_string(),
                    ))
                }
                Err(e) => return Err(e),
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
        if crate::control::cancelled() {
            // 3.0.0：取消的时候**必须**先把这条通道关掉，别只是自己退出。
            //
            // 取消的是「手机 → 电脑」这个方向，手机那边正一门心思往这条通道里写；
            // 写满 TCP 缓冲之后它就阻塞在 write 里，自己不会回头去看控制通道，
            // 也就看不到我们随后写的 CANCEL 帧。只有让它的 write 立刻失败
            // （shutdown 之后对端的写会拿到 RST / EPIPE），它才会走取消分支。
            // 以前这里只置了个本地标志就返回：要等 join、等 finish_with 写完续传日志、
            // 等 rebuild_transfers 里才 shutdown —— 实测那几秒手机还在继续发数据。
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled by user"));
        }
        let header = match proto::read_i16(&mut stream) {
            Ok(h) => h,
            Err(e) => {
                log::debug(&format!("ch{}: read header failed: {}", channel, e));
                return Err(e);
            }
        };
        match header {
            transfer::FILE => {
                let _file_index = proto::read_i32(&mut stream)?;
                let path = proto::read_utf(&mut stream)?;
                let modified_ms = proto::read_i64(&mut stream)?;
                let total_size = proto::read_i64(&mut stream)?;
                let index = proto::read_i32(&mut stream)?;
                let length = proto::read_i32(&mut stream)?;
                // 303：这一块的 CRC32，写盘前必须一致
                let expected_crc = proto::read_i32(&mut stream)? as u32;
                log::debug(&format!(
                    "ch{}: FILE idx={} len={} path={}",
                    channel, index, length, path
                ));
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
                let actual_crc = crc32fast::hash(&buffer[..length]);
                if actual_crc != expected_crc {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        crate::i18n::tr(
                            "数据校验失败：块内容与发送端不一致",
                            "data check failed: block differs from what the phone sent",
                        )
                        .to_string()
                            + &format!(
                                " (ch{} {}#{}: crc {:08x} != {:08x})",
                                channel, path, index, actual_crc, expected_crc
                            ),
                    ));
                }
                receiver.write_block(
                    channel,
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
                // 顺手把后续字节抓出来：能看出对端到底发了什么（错误帧？控制帧？半截块？）
                let mut tail = [0u8; 32];
                let n = stream.peek(&mut tail).unwrap_or(0);
                log::error(&format!(
                    "ch{}: unexpected transfer frame {} tail={:02x?}",
                    channel, other, &tail[..n]
                ));
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

/// 对端消失类的错误（被 reset / 关闭 / 管道断）：和"真的出错"要分开处理
fn is_disconnect(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::NotConnected
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
        spawn_mock_phone_expecting(listener, folder, blocks, home_dir, true)
    }

    /// 同 {@link spawn_mock_phone}，但可以要求电脑最后回一个"失败"的结论帧
    /// （用来断言电脑自己发现了"块账本对不上"，而不是照回 ok 让手机端去喊 SHA 不一致）。
    fn spawn_mock_phone_expecting(
        listener: TcpListener,
        folder: String,
        blocks: Vec<Block>,
        home_dir: String,
        expect_ok: bool,
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
            // 305：REQUEST_RECEIVE 之后紧跟着必须是 RESUME_QUERY（电脑端同步读这一帧），
            // 一个文件都不查也要发，只是 count = 0。
            proto::write_i16(&mut cw, controller::RESUME_QUERY).unwrap();
            proto::write_i32(&mut cw, 0).unwrap();
            assert_eq!(
                proto::read_i16(&mut cr).unwrap(),
                controller::RESUME_REPLY,
                "电脑端应该用 RESUME_REPLY 帧回答续传查询"
            );
            assert_eq!(proto::read_i32(&mut cr).unwrap(), 0);

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
                    // 303：每块后面跟一个 i32 CRC32
                    proto::write_i32(&mut w, crc32fast::hash(&block.data) as i32).unwrap();
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
                    // 303：每块后面跟一个 i32 CRC32
                    proto::write_i32(&mut w, crc32fast::hash(&block.data) as i32).unwrap();
                    w.write_all(&block.data).unwrap();
                }
                proto::write_i16(&mut w, transfer::EOF).unwrap();
            });
            // 302：传输期间的暂停同步 —— 手机发 PAUSE_STATE，电脑回 PAUSE_REPLY 帧。
            // 以前电脑回的是裸 bool，手机端发送线程的读循环会把它当结论读串。
            proto::write_i16(&mut cw, controller::PAUSE_STATE).unwrap();
            proto::write_bool(&mut cw, false).unwrap();
            assert_eq!(
                proto::read_i16(&mut cr).unwrap(),
                controller::PAUSE_REPLY,
                "电脑应该用 PAUSE_REPLY 帧回报暂停状态"
            );
            assert!(!proto::read_bool(&mut cr).unwrap());

            writer_a.join().unwrap();
            writer_b.join().unwrap();

            // 302 收尾：电脑先给落盘结论（RESULT），手机再回读文件结论（RESULT）
            assert_eq!(proto::read_i16(&mut cr).unwrap(), controller::RESULT);
            let ok = proto::read_bool(&mut cr).unwrap();
            assert_eq!(ok, expect_ok, "落盘结论帧的 ok 和预期不符");
            // 303：成功时结论帧后面跟一张指纹表（这里是空的），失败时跟一段错误信息
            let count = proto::read_i32(&mut cr).unwrap();
            for _ in 0..count.max(0) {
                let _size = proto::read_i64(&mut cr).unwrap();
                let mut sha = [0u8; 32];
                cr.read_exact(&mut sha).unwrap();
            }
            if !ok {
                // 失败时电脑端必须说清楚是哪几块对不上账（不能只写"接收失败"）
                let message = proto::read_utf(&mut cr).unwrap();
                assert!(
                    message.contains("块") || message.contains("block"),
                    "失败结论里要指出缺块，实际是：{}",
                    message
                );
            }
            if expect_ok {
                // 电脑端会读手机端的"读文件结论"，成功后这条通道才收得干净
                proto::write_i16(&mut cw, controller::RESULT).unwrap();
                proto::write_bool(&mut cw, true).unwrap();
                proto::write_i32(&mut cw, 0).unwrap();
            }
            proto::write_i16(&mut cw, controller::SHUTDOWN).unwrap();
            // 别立刻关 socket：电脑端还要读走 SHUTDOWN。马上 drop 这一端会被当成
            // "连接被重置"（10054），测试就会偶发地变红。
            thread::sleep(Duration::from_millis(150));
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

    /// 2026-11-17：手机端把同一块发了两遍、漏了另一块 ——
    /// 文件大小照样等于声明大小（set_len 预先占满），收到的字节数也"刚好"，
    /// 但内容是坏的。这就是 NEXT-TODO 第 11 条「大小一致、SHA-256 不一致」。
    ///
    /// 以前电脑端会照回 ok 的结论帧、照算 SHA-256，然后在手机端喊不一致之后
    /// 一句解释都留不下来。现在电脑端自己按块账本发现缺块，回失败的结论帧，
    /// 结论里点名缺哪一块。
    #[test]
    fn a_repeated_block_with_a_hole_fails_the_transfer_locally() {
        let dir = temp_dir("receive-hole");
        let home = util::normalize_dir(&dir).to_string_lossy().to_string();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let target = dir.join("hole.bin").to_string_lossy().to_string();
        let total = 2 * BLOCK_SIZE;
        let blocks = vec![
            Block {
                path: target.clone(),
                modified_ms: 1_700_000_000_000,
                total_size: total,
                index: 0,
                data: vec![1u8; BLOCK_SIZE as usize],
            },
            // 第 0 块又来一遍（内容不同），第 1 块一次都没发
            Block {
                path: target.clone(),
                modified_ms: 1_700_000_000_000,
                total_size: total,
                index: 0,
                data: vec![9u8; BLOCK_SIZE as usize],
            },
        ];
        let server = spawn_mock_phone_expecting(listener, home.clone(), blocks, home.clone(), false);

        let mut session = Session::connect(IpAddr::from([127, 0, 0, 1]), port, &home, 0).unwrap();
        let receiver = Arc::new(Receiver::new(util::normalize_dir(&dir)));
        let stop = Arc::new(AtomicBool::new(false));
        match session.run(&receiver, &stop) {
            SessionEnd::Shutdown => {}
            SessionEnd::Disconnected(reason) => panic!("unexpected disconnect: {}", reason),
            SessionEnd::Stopped => panic!("unexpected stop"),
        }
        server.join().unwrap();

        // 大小是对的（预分配），内容有洞 —— 这正是"看起来一切正常"的那种坏文件
        let data = std::fs::read(&target).unwrap();
        assert_eq!(data.len() as u64, total, "文件大小仍然是声明的大小");
        assert!(data[..BLOCK_SIZE as usize].iter().all(|b| *b == 9));
        assert!(
            data[BLOCK_SIZE as usize..].iter().all(|b| *b == 0),
            "第 1 块从没来过，那一段留在盘上是 0"
        );
        // 收尾时必须留下"缺第 1 块"的续传记录：下次同一个文件重传会补上这一段
        let sidecar = crate::journal::ReceiveJournal::sidecar_path(std::path::Path::new(&target));
        assert!(sidecar.exists(), "没传完（有洞）的文件要留续传记录");
        let raw = std::fs::read(&sidecar).unwrap();
        let bitmap = &raw[32..];
        assert_eq!(bitmap[0] & 0b0000_0001, 1, "第 0 块是收到的");
        assert_eq!(bitmap[0] & 0b0000_0010, 0, "第 1 块必须留着让重传补");
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

/// 303：发送任务里出现过的文件（按 file_index 去重），收尾算 SHA-256 用
fn source_files_of(tasks: &[SendTask]) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut files = Vec::new();
    for task in tasks {
        if let SendTask::Block {
            file_index,
            host_path,
            ..
        } = task
        {
            if seen.insert(*file_index) {
                files.push(host_path.clone());
            }
        }
    }
    files
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
            let taken = guard.take(me);
            // 领完任务就把"谁负责哪一段"发给界面：被切走的段会留下自己的边界
            publish_segments(&guard);
            taken
        };
        let Some(index) = next else { break };
        written += send_task(&mut stream, &tasks[index], &sent, me as u8)?;
    }
    use std::io::Write as _;
    proto::write_i16(&mut stream, transfer::EOF)?;
    stream.flush()?;
    Ok(written)
}

/// 306：位图第 index 位是否为 1（那块已经在接收端了）
fn resume_has_block(bits: &[u8], index: u32) -> bool {
    let byte = index as usize / 8;
    byte < bits.len() && (bits[byte] & (1u8 << (index % 8))) != 0
}

/// 发一条任务（文件夹 / 一个文件块），返回这一块写了多少字节
fn send_task(
    stream: &mut std::net::TcpStream,
    task: &SendTask,
    sent: &AtomicU64,
    channel: u8,
) -> io::Result<u64> {
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
            // 303：块校验和（CRC32），手机端写盘前校验这一块的数据
            proto::write_i32(stream, crc32fast::hash(&buffer) as i32)?;
            stream.write_all(&buffer)?;
            stream.flush()?;

            sent.fetch_add(*length as u64, Ordering::Relaxed);
            crate::receiver::PROGRESS.mark_sent(
                channel,
                dest,
                *index,
                *total_size,
                *length as u64,
            );
            Ok(*length as u64)
        }
    }
}

/// 进度条上的一段（GUI 用）：一段 = 计划里一条通道负责的连续任务区间
#[derive(Clone, Copy, Default, Debug)]
pub struct SendSegView {
    /// 这一段的任务数（决定宽度）
    pub span: u32,
    /// 这一段已完成比例 0..1
    pub filled: f32,
}

static SEND_SEGS: OnceLock<Mutex<Vec<SendSegView>>> = OnceLock::new();

fn send_seg_cache() -> &'static Mutex<Vec<SendSegView>> {
    SEND_SEGS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 当前这次发送的分段结构（一次「手机来取文件」开始时重置）。
/// 和手机端一样按"哪条通道负责哪一段"画：被切走的段会作为新的段留下，
/// 不会因为数据传完就重新合并回通道数。
pub fn send_segments() -> Vec<SendSegView> {
    send_seg_cache()
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default()
}

pub fn reset_send_segments() {
    if let Ok(mut g) = send_seg_cache().lock() {
        g.clear();
    }
}

/// 把计划里的分段发布给界面；段数过多时退化成一条整进度
fn publish_segments(plan: &SendPlan) {
    let mut view = match send_seg_cache().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    view.clear();
    let mut segs: Vec<(usize, usize, usize)> = plan
        .segments
        .iter()
        .map(|s| (s.start, s.end, s.cursor))
        .collect();
    if segs.is_empty() {
        return;
    }
    segs.sort_by_key(|s| s.0);
    const MAX_SEGMENTS: usize = 32;
    if segs.len() > MAX_SEGMENTS {
        let total: usize = segs
            .iter()
            .map(|(s, e, _)| e.saturating_sub(*s))
            .sum();
        let done: usize = segs
            .iter()
            .map(|(s, e, c)| c.saturating_sub(*s).min(e.saturating_sub(*s)))
            .sum();
        let total = total.max(1);
        view.push(SendSegView {
            span: total as u32,
            filled: (done as f32 / total as f32).clamp(0.0, 1.0),
        });
        return;
    }
    for (start, end, cursor) in segs {
        let span = end.saturating_sub(start).max(1);
        let done = cursor.saturating_sub(start).min(span);
        view.push(SendSegView {
            span: span as u32,
            filled: (done as f32 / span as f32).clamp(0.0, 1.0),
        });
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

#[cfg(test)]
mod bugfix_tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hfx-bugfix-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// BUG-2：传完之后旁边的 `.hfxpart` 必须消失，文件本身不动
    #[test]
    fn clear_resume_journals_removes_sidecar_only() {
        let dir = tmp("clear");
        let file = dir.join("a.bin");
        fs::write(&file, b"hello").unwrap();
        let sidecar = crate::journal::ReceiveJournal::sidecar_path(&file);
        fs::write(&sidecar, b"dummy").unwrap();
        assert!(sidecar.exists());

        clear_resume_journals(&[file.clone()]);

        assert!(!sidecar.exists(), "sidecar must be gone");
        assert!(file.exists(), "the file itself must stay");
    }

    /// 校验不一致：文件和续传记录一起删掉（否则下次传同一个文件还是旧的坏数据）
    #[test]
    fn discard_received_removes_file_and_sidecar() {
        let dir = tmp("discard");
        let file = dir.join("b.bin");
        fs::write(&file, b"broken").unwrap();
        let sidecar = crate::journal::ReceiveJournal::sidecar_path(&file);
        fs::write(&sidecar, b"dummy").unwrap();

        discard_received(&[file.clone()]);

        assert!(!file.exists(), "the mismatched file must be removed");
        assert!(!sidecar.exists(), "its sidecar must be removed too");
    }

    /// 删一个本来就不存在的文件不许 panic（收尾路径上会碰到）
    #[test]
    fn discard_received_tolerates_missing_file() {
        let dir = tmp("missing");
        let file = dir.join("c.bin");
        discard_received(&[file.clone()]);
        assert!(!file.exists());
    }

    /// 没有续传记录的普通文件：两个函数都得当作没事发生
    #[test]
    fn both_helpers_are_noops_without_sidecar() {
        let dir = tmp("noop");
        let file = dir.join("d.bin");
        fs::write(&file, b"x").unwrap();
        clear_resume_journals(&[file.clone()]);
        assert!(file.exists());
        discard_received(&[file.clone()]);
        assert!(!file.exists());
    }
}