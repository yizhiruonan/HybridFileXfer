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
use crate::queue;
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
/// 取消路径上写一帧 CANCEL 的时间上限。
///
/// 取消这条路上**任何一步都不许无限期等**：手机处于暂停态时它不读控制通道、
/// 也不往传输通道写，任何一次「等对端」都可能永远等下去（实测：点了取消，
/// 日志停在「先关传输通道，再通知手机端」那一行，后面一行都没有了）。
/// CANCEL 只有 2 字节，只要 socket 发送缓冲还有一点空就瞬间写完 ——
/// 正常路径上这个超时一次也用不上，它只是把「可能永远等」变成「最多等半秒」。
const CANCEL_FRAME_TIMEOUT: Duration = Duration::from_millis(500);
/// 取消 / 停止之后，还留给传输线程多久自己退出（整段合计，不是每条通道各算）。
///
/// 会话线程**不能**无条件 join 它们：暂停态下那些线程正停在「等手机发下一块」
/// 的阻塞读里，等它们退出就是把取消按钮钉死。过了这个宽限期，
/// 还没退出的线程就脱离 join 继续跑（它们自己会随取消标志 / socket shutdown 走掉），
/// 会话线程带着「有一个线程还没退」这件事继续收尾和重建。
const CANCEL_JOIN_GRACE: Duration = Duration::from_millis(1500);
/// 传输通道读循环里等「下一帧帧头」的轮询粒度，也是取消在通道读线程里的响应粒度。
///
/// 以前这里直接拿 120 秒的读超时去读帧头：手机在块与块之间暂停时（它不发数据），
/// 一次读就要挂到超时，取消之后会话线程还得 join 它 —— 取消看起来「卡死」的
/// 一大半时间就是这种没人打断的阻塞读。
const TRANSFER_POLL: Duration = Duration::from_millis(50);
/// 传输通道多久一点数据都没读到就当它已经死了（和原来的 120 秒读超时同一个尺度）。
///
/// 以前这个数是 socket 上一次读的超时，现在是「累计没有任何进展」的上限：
/// 每读一段都回去看一眼取消标志，所以取消不必等这个数。
const TRANSFER_STALL: Duration = Duration::from_secs(120);
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

/// 会话持有的「5742 来源白名单」凭证；Drop 时逐条撤销。
struct WebGrant {
    ips: Vec<IpAddr>,
}

impl WebGrant {
    fn new(mut ips: Vec<IpAddr>) -> WebGrant {
        ips.sort();
        ips.dedup();
        for ip in &ips {
            crate::webserve::allow_peer(*ip);
        }
        WebGrant { ips }
    }
}

impl Drop for WebGrant {
    fn drop(&mut self) {
        for ip in &self.ips {
            crate::webserve::forget_peer(*ip);
        }
    }
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
    /// 重建传输通道期间，手机端要求「开下一单」（REQUEST_RECEIVE）的记号。
    ///
    /// 手机端有了「多选排队」之后，电脑取消当前这一单，手机立刻取下一个开始：
    /// 它往控制通道写一帧 REQUEST_RECEIVE，紧跟一帧 RESUME_QUERY（整份文件清单）。
    /// 这两帧落在「电脑正等 REBUILD_ACK」的窗口里是必然的（真机 06:54:45 那三条
    /// 「意外的控制帧」就是这么来的），而处理它要 receiver 参数（主循环里是
    /// handle_receive(receiver)），rebuild_transfers 拿不到。
    ///
    /// 所以只能连帧带载荷一起读下来存进这里，重建结束回到主循环后立刻补办 ——
    /// 以前是「打个警告就地 break」：帧头读走了、载荷留在流里，下一次读 opcode
    /// 读到的就是文件清单里的字节，整条控制通道当场错位，重建和会话一起报废。
    pending_receive: bool,
    /// 已经替主循环读好的「下一单文件清单」（RESUME_QUERY 的载荷）。
    ///
    /// 主循环补办 handle_receive 时直接用这一份，别再去流里读一遍 ——
    /// 那一帧已经消费掉了，再读会一直等到读超时。
    pending_resume: Option<Vec<(String, u64, i64)>>,
    pub interfaces: Vec<PeerInterface>,
    /// 本会话给 5742 文件夹浏览接口开的来源白名单，会话销毁时自动撤销。
    ///
    /// 为什么是一串而不是一个：电脑是**主动连接**手机的那一方，拨号用的是手机
    /// 报来的某一个网卡地址；手机反过来访问 5742 时，源地址由它自己的路由决定，
    /// 可能是另一张网卡。把手机报来的所有网卡地址都登记上，才不会出现
    /// 「连着能用、换个网卡就 403」。这些地址全都是这台手机的，不扩大权限面。
    ///
    /// 用「带 Drop 的守卫」而不是给 Session 自己写 Drop：Session 有几处是部分移动
    /// （把 transfers / reader 单独取走），一旦类型自己实现了 Drop 就编不过。
    /// 前导下划线是给编译器看的：这个字段本来就只是「活着的时候有效」，
    /// 没有任何代码需要读它。
    _web_grant: WebGrant,
    /// 手机监听的端口（非回环网卡上的传输通道重连要用）
    port: u16,
    /// adb 转发给这台手机的本地端口（回环通道重连要用）
    local_port: u16,
    /// 上一次把限速值推给手机时的代次（见 ratelimit::configured_epoch）。
    /// 轮询里发现代次变了就补发一帧 SPEED_LIMIT。
    limit_epoch: i64,
    /// 用户点的「停止」标志，run() 时存进来一份。
    ///
    /// 传输循环（等通道 / 重建握手 / 等结论）原来都不读它 —— 只有主循环顶部读，
    /// 而主循环在传输期间根本不转。于是「传输中途点停止」要等这一轮传完才有反应，
    /// 期间用户再点一次「开始接收」，就又建了一条会话（手机上凑成两台电脑）。
    stop: Option<Arc<AtomicBool>>,
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
    /// 用户点了「停止」没有。传输循环里每转一圈都问一次。
    fn stop_requested(&self) -> bool {
        self.stop
            .as_ref()
            .map(|s| s.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

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
        // ---- 307：双端限速协商 ----
        //
        // 位置刻意选在「版本匹配」之后：版本不匹配时这一对帧不交换，
        // 老版本对端看到的仍然是原来那句「版本不一致」，不会卡在一个它没见过的字段上。
        //
        // 308 起语义变了：两端不再各报各的取 min，而是**共用一个值**。
        //
        // 定值的规则（两边必须一模一样，见手机端 HFXServer.negotiateLimitMbps）：
        //   电脑这边非 0 就用电脑的；电脑填 0（= 我这端不限速）才用手机的。
        // 定下来之后两端都存这个数、都按这个数跑，界面上显示的也是它 ——
        // 以前是「电脑写 20、手机写 5，实际跑 5，但两边界面各写各的」。
        let local_limit_mb = crate::ratelimit::configured_mb();
        proto::write_i32(&mut w, local_limit_mb)?;
        w.flush()?;
        // 手机读完会把它认下来的共用值写回来（可能等于它自己的值）。
        let shared_mb = proto::read_i32(&mut reader)?;
        // adopt 内部含 set_limit（会 reset_window），这里不用再调一次。
        crate::ratelimit::adopt_configured_mb(shared_mb as i64);
        // limit_text 自己带单位（"5 MB/s" / "不限速"），所以格式串里不要再写 MB/s ——
        // 否则不限速时会拼出「生效 不限速 MB/s」。
        log::info(&format!(
            "限速（两端共用）：本机 {}，手机 {}",
            limit_text(local_limit_mb),
            limit_text(shared_mb)
        ));
        // 这一帧已经把值对齐了。代次不用在这里记：Session 结构体就是在握手之后
        // 构造的，limit_epoch 的初值取的就是此刻的代次，刚连上不会多发一帧。

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

        // 握手成功 = 这台手机确实是刚才连上来的那一台，此时才给它的地址开 5742 的口子。
        // 顺带把控制连接的对端地址也算上：手机报的网卡清单可能漏掉它正在用的那一张。
        let mut web_ips: Vec<IpAddr> = interfaces.iter().map(|i| i.address).collect();
        if let Ok(peer) = reader.peer_addr() {
            web_ips.push(peer.ip());
        }

        Ok(Session {
            device: String::new(),
            reader,
            writer,
            transfers,
            pending_opcode: None,
            pending_receive: false,
            pending_resume: None,
            limit_epoch: crate::ratelimit::configured_epoch(),
            interfaces,
            _web_grant: WebGrant::new(web_ips),
            port,
            local_port,
            stop: None,
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
        // 存一份给传输循环看：那几个循环跑起来之后主循环是不转的，
        // 不存的话「停止」要等传完才生效（手机上会因此凑出两台电脑）。
        self.stop = Some(Arc::clone(stop));
        let mut last_activity = Instant::now();
        // 用较短的读超时轮询 stop 标志；手机端每隔几秒会发心跳
        let _ = self
            .reader
            .set_read_timeout(Some(Duration::from_millis(500)));
        loop {
            if stop.load(Ordering::Relaxed) {
                return SessionEnd::Stopped;
            }
            // 空闲时也要把「本机改了限速」推过去：这时主循环在 500ms 的读超时里转，
            // 不推的话用户改完设置要等下一次握手才同步（手机上会一直显示旧值）。
            if let Err(e) = self.flush_speed_limit() {
                return SessionEnd::Disconnected(describe_read_error(&e));
            }
            // 取消之后重建通道时，手机端可能已经把下一单排上了（REQUEST_RECEIVE，
            // 见 rebuild_transfers）：那一帧和它的文件清单都已经读好存在 pending 里，
            // 处理它要 receiver，只能回到这里办。
            //
            // 必须排在「读下一帧」之前：手机端此刻正卡在等 RESUME_REPLY 上，
            // 我们晚回它就一直停在那儿（队列里剩下的那几个文件也就一直不开始）。
            if self.pending_receive {
                self.pending_receive = false;
                let _ = self
                    .reader
                    .set_read_timeout(Some(Duration::from_secs(120)));
                let result = self.handle_receive(receiver);
                let _ = self
                    .reader
                    .set_read_timeout(Some(Duration::from_millis(500)));
                if let Err(e) = result {
                    return SessionEnd::Disconnected(format!("{}", e));
                }
                last_activity = Instant::now();
                continue;
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
                controller::SPEED_LIMIT => self.handle_speed_limit(),
                // 309：手机的队列快照。正常只在空闲时来（用户又勾了 / 删了排队项），
                // 整表覆盖进登记表，界面下一帧就能看见「等待中」那几块。
                controller::QUEUE_STATE => self.handle_queue_state(),
                controller::LIST_FILES => self.handle_list_files(),
                controller::DELETE_FILE => self.handle_delete(),
                controller::MKDIR => self.handle_mkdir(),
                controller::REQUEST_RECEIVE => self.handle_receive(receiver),
                controller::REQUEST_SEND => self.handle_send(),
                // 305：续传查询本该紧跟 REQUEST_RECEIVE、由 handle_receive 同步读掉。
                // 万一那帧 REQUEST_RECEIVE 落在了重建窗口里（见 rebuild_transfers），
                // 只剩这一帧漏到主循环：先把载荷（整份文件清单）读干净存起来，
                // 下一圈的开头按「下一单」补办 —— 只读帧头就走，文件名就成了下一个帧头。
                controller::RESUME_QUERY => self.stash_resume_query(),
                // 306：这个方向的续传状态帧只该由 handle_send 里的 read_resume_state 读。
                // 出现在别处就把载荷读干净再丢（留着它同样是帧序错位）。
                controller::RESUME_STATE => self.discard_resume_state(),
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
            // 这条指令可能执行了很久（传输 / 取消后的重建，实测重建要 13 秒）。
            // 期间手机端的心跳是**在这条指令内部**被就地处理掉的，没有更新
            // last_activity；回到循环里若正好赶上一次读超时，就会拿十几秒前的
            // 时间戳去比 12 秒的心跳上限 —— 明明刚重建成功 9 条通道，
            // 下一秒却报「连接超时」把会话拆掉（真机 2026-11-21 撞见）。
            // 指令正常返回就说明对端一直活着，直接把计时器归零。
            if result.is_ok() {
                last_activity = Instant::now();
            }
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
    ///
    /// 帧流上的硬规矩（上一轮错位的根因）：**每一条退出路径上都不许留下没读走的变长载荷**。
    /// 也就是说帧头读下来之后，要么当场把载荷读完，要么把「还没读完」这件事记下来
    /// 交给能读完的人 —— 绝不允许"读了 opcode 就 break/return"。
    ///
    /// 「手机端要开下一单」（REQUEST_RECEIVE + RESUME_QUERY）的策略：读下来、存进
    /// pending_receive / pending_resume，等重建彻底结束回主循环再办（那里才有 receiver）。
    /// 为什么这样延迟是安全的 —— 时序的闸门在电脑手里：
    ///   * 手机端发完 RESUME_QUERY 就停在那里等 RESUME_REPLY，位图没拿到之前它不会往
    ///     任何传输通道写一个字节（对端 handle_resume_query 的那段逻辑就是这么写的）；
    ///   * RESUME_REPLY 是主循环重新进入 handle_receive 之后才发的，而那时重建已经结束了
    ///     —— 不存在"重建还没收全通道、手机已经开始发块"的窗口；
    ///   * 重建只成功一部分也没关系：这一单的通道集合就是 self.transfers 里剩下的那些，
    ///     handle_receive 按它们 spawn 读线程，手机端也只往它 ACK 过的那几条上写，
    ///     少几条通道只是慢一点，不会写丢；
    ///   * 一条都没重建回来的话，rebuild_transfers 照旧返回错误（下面那支），会话断掉、
    ///     pending 里的那一单一起作废：没有能收的通道，硬留着会话只会让手机端卡在等
    ///     RESUME_REPLY 上直到超时，还不如老实断线重连。
    fn rebuild_transfers(&mut self) -> io::Result<usize> {
        // 先告诉手机"这是取消，不是断线" —— 本机点「中断发送」时它还不知道；
        // 手机自己点的取消，这一帧重复一次也无害（它是幂等的）。
        log::warn(crate::i18n::tr(
            "开始重建传输通道（已通知手机端取消）",
            "rebuilding the transfer channels",
        ));
        // 先把这一轮用过的通道名字记下来（下面 self.transfers 会被整个取空）。
        let names: Vec<String> = self.transfers.iter().map(|(name, _)| name.clone()).collect();
        // 旧通道必须**扔掉**，不能只 shutdown：shutdown 只发 FIN，手机端那头照样能
        // 继续往这条连接里写（FIN 只表示"我不发了"），它要写到自己的发送缓冲满了才
        // 发现 —— 真机实测这一刻要拖 4~5 秒，手机还在按满速往一条死连接里灌数据，
        // 用户看到的就是「点了取消，进度条挂着不动，二十秒后才开始下一单」。
        // 所以先把 SO_LINGER 设成 0（关闭时回 RST 而不是 FIN），再 drop：手机下一次写
        // 立刻抛 "Connection reset by peer"，发块线程当场退出。
        // （linger=0 只影响 close 的收尾方式，对正常收发没有副作用。）
        //
        // 先关+发帧、还是先发帧+关？这里保持「先关再发帧」：这一段跑在会话线程上，
        // 主循环里那条「先写 CANCEL」的快路径通常已经先把帧发出去了（claim_cancel_frame），
        // 走到这里时多半不用再写。
        // （read_transfer_channel / handle_receive 的等待循环里已经关过一遍，
        //   这里再关一次是幂等的，覆盖"线程已经正常 EOF 退出"的情形。）
        let old_transfers = std::mem::take(&mut self.transfers);
        for (_, stream) in old_transfers.iter() {
            crate::netif::abort(stream);
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        drop(old_transfers);
        // 整个取消过程只写**一帧** CANCEL：它是「重建握手」的开场白，不是可重复的
        // 指令。第二次点「取消 → 保留」会被 control::cancel_receive 的幂等闸门挡掉；
        // 等待循环里通常已经抢先把这一帧写出去了（claim_cancel_frame），这里只是兜底
        // （比如线程已经全部正常退出、等待循环没机会发现取消）。
        if crate::control::claim_cancel_frame() {
            // 发哪一帧取决于"半截文件在哪一头"，也就是当前是哪个方向：
            //   * 发送方向（电脑是发件方，半截文件在手机）：用户选了「不保留」就要
            //     让手机把它写了一半的文件和续传日志删掉 —— CANCEL_DISCARD；
            //   * 接收方向（手机是发件方，半截文件在电脑）：绝不发 CANCEL_DISCARD，
            //     否则手机那头会以为是"让我删东西"，把它自己的源文件删了。
            let discard_remote =
                crate::control::sending() && crate::control::discard_partial();
            let opcode = if discard_remote {
                controller::CANCEL_DISCARD
            } else {
                controller::CANCEL
            };
            if discard_remote {
                log::warn(crate::i18n::tr(
                    "已通知手机：把没传完的文件和续传记录删掉（CANCEL_DISCARD）",
                    "the phone was told to remove the incomplete file (CANCEL_DISCARD)",
                ));
            }
            // 兜底那一帧同样带超时：这里要是顶满 15 秒的写超时，
            // 「取消 → CANCEL 发出」这段时间又回到十几秒（用户看到的就是进度还在涨）。
            let _ = self.write_control_frame_within(opcode, CANCEL_FRAME_TIMEOUT);
        }
        if names.is_empty() {
            return Ok(0);
        }
        // 等手机端 ACK 的总窗口。手机那边要"等传输线程退出（最长 15s）+ 每条新通道
        // 400ms 分类 + takeTransferChannel"，6 条要 2~12 秒；以前只给 20 秒，
        // 实测（PC 端「中断发送」）会超时把整条会话拆掉。给到 45 秒，超时仍然按失败处理。
        //
        // 读超时本身用短的（500ms）轮询，不是一次等 45 秒：45 秒里点了「停止」
        // 会一直卡在 read 上，stop_engine 的 join 就得陪着等 —— 界面按停止要卡 45 秒。
        // 真正的总窗口由下面的 deadline 控制，超时才按失败处理。
        let rebuild_deadline = Instant::now() + Duration::from_secs(45);
        // 轮询用短超时只是为了让 stop 能被及时看到，但**不能直接拿短超时去读 i16**：
        // 帧头两个字节若分在两个 TCP 片段里到达，第一个字节读到、第二个超时，
        // 这条控制通道从此错位一拍。所以照 poll_control_during_transfer 的写法，
        // 先用 peek 确认整帧到了再正式读。
        let _ = self
            .reader
            .set_read_timeout(Some(Duration::from_millis(200)));
        // 先把「名字 → 目标地址」整批算出来：下面要预连接，握手循环里还要用同一份。
        let plan: Vec<(String, SocketAddr, Option<IpAddr>)> = names
            .into_iter()
            .filter_map(|name| {
                let interface = match self.interfaces.iter().find(|iface| iface.name == name) {
                    Some(interface) => interface.clone(),
                    None => {
                        log::warn(&format!("rebuild: unknown channel {name}"));
                        return None;
                    }
                };
                // 回环通道走 adb 转发的本地端口，其它网卡走手机监听的端口
                let port = if interface.address.is_loopback() {
                    self.local_port
                } else {
                    self.port
                };
                Some((name, SocketAddr::new(interface.address, port), interface.bind))
            })
            .collect();
        // 预连接：把这一批通道一次全连上，再逐条做握手。
        //
        // 手机端每收一条连接要付一个 100ms 的「分类窗口」（它在 accept 线程里串行做，
        // 靠「连上之后安静不安静」区分控制通道和传输通道），而 connect 本身只要几毫秒。
        // 以前是「连一条 → 发 REQ → 等 ACK → 连下一条」，那 6 个 100ms 全落在握手窗口里，
        // 光分类就 0.6 秒；而这些连接早一秒躺进它的 accept 队列是没关系的 ——
        // 它那边这会儿正忙着等自己的传输线程退出（真机 1 秒左右），分类正好和那段并行。
        // 握手开始时通道已经在它的待领队列里，一条只要几毫秒。
        let mut pre: Vec<Option<TcpStream>> = Vec::with_capacity(plan.len());
        for (name, target, bind) in &plan {
            let stream = crate::netif::connect(*target, CONNECT_TIMEOUT, *bind).ok();
            match stream.as_ref() {
                Some(stream) => {
                    let _ = stream.set_nodelay(true);
                    let _ = stream.set_read_timeout(Some(TRANSFER_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(TRANSFER_TIMEOUT));
                }
                None => log::debug(&format!(
                    "rebuild: 通道 {} 预连接失败，握手时再试一次",
                    name
                )),
            }
            pre.push(stream);
        }
        let mut rebuilt: Vec<(String, TcpStream)> = Vec::with_capacity(plan.len());
        for (index, (name, target, bind)) in plan.into_iter().enumerate() {
            let mut prepared = pre[index].take();
            // 为什么这里要允许「重连一次」：手机端从收到 CANCEL 到真正进入重建握手，
            // 中间隔着它自己的传输线程退出（真机实测 1~6 秒）。我们这个 socket 要是
            // 比它早到，就会落在它「还没登记要通道」的空窗里 —— 那边 accept 线程把这种
            // 安静连接当成上一轮的残留关掉，于是这一条通道永远等不到，手机端白等一个
            // 读超时（以前 12 秒）才回 REBUILD_ACK false，整条队列就卡在那里不开工。
            // 重连一次能把这条通道捡回来，代价只有一次 connect。
            let mut restored: Option<TcpStream> = None;
            let channel_started = Instant::now();
            for attempt in 0..2 {
                // 第一次用预连接好的那条；它没连上或者被手机端拒了，这里再连一条。
                let stream = match prepared.take() {
                    Some(stream) => stream,
                    None => match crate::netif::connect(target, CONNECT_TIMEOUT, bind) {
                        Ok(stream) => {
                            let _ = stream.set_nodelay(true);
                            let _ = stream.set_read_timeout(Some(TRANSFER_TIMEOUT));
                            let _ = stream.set_write_timeout(Some(TRANSFER_TIMEOUT));
                            stream
                        }
                        Err(e) => {
                            log::warn(&format!(
                                "{} {}: {} — {}",
                                crate::i18n::tr("重建通道失败", "cannot rebuild channel"),
                                name,
                                target,
                                e
                            ));
                            break;
                        }
                    },
                };
                proto::write_i16(&mut self.writer, controller::REBUILD_REQ)?;
                proto::write_utf(&mut self.writer, &name)?;
                self.writer.flush()?;
                loop {
                    // 重建要等 REBUILD_ACK（读超时 45 秒）。这里点了「停止」也得能走开，
                    // 否则 stop_engine 的 join 会把界面卡住这么久。
                    if self.stop_requested() {
                        return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
                    }
                    if Instant::now() > rebuild_deadline {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "rebuild timed out"));
                    }
                    let id = match self.pending_opcode.take() {
                        Some(id) => id,
                        None => {
                            let mut window = [0u8; 2];
                            match self.reader.peek(&mut window) {
                                // 对端把控制通道关了
                                Ok(0) => {
                                    return Err(io::Error::new(
                                        io::ErrorKind::UnexpectedEof,
                                        "control channel closed by peer",
                                    ))
                                }
                                // 半个帧头：下一轮再看（此时一个字节都还没读走）
                                Ok(n) if n < 2 => continue,
                                Ok(_) => {
                                    let _ = self
                                        .reader
                                        .set_read_timeout(Some(Duration::from_secs(45)));
                                    proto::read_i16(&mut self.reader)?
                                }
                                // 200ms 没数据：回去再看一眼 stop / deadline
                                Err(e) if is_timeout(&e) => continue,
                                Err(e) => return Err(e),
                            }
                        }
                    };
                    match id {
                        controller::REBUILD_ACK => {
                            if proto::read_bool(&mut self.reader)? {
                                log::debug(&format!("rebuild: channel {} is back", name));
                                restored = Some(stream);
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
                        | controller::SPEED_LIMIT
                        // 309：队列快照也是「走到哪儿都能吞」的那种 —— 它载荷变长，
                        // 当 pending 留给主循环会把后面的帧读错位
                        | controller::QUEUE_STATE
                        | controller::LIST_FILES
                        | controller::DELETE_FILE
                        | controller::MKDIR
                        | controller::CHECK_PENDING
                        // 手机端正常断开前会发这一帧：当成断线报上去（不是"意外帧"），
                        // 否则要干等到 45 秒的 deadline 才失败，界面卡在那儿不动
                        | controller::SHUTDOWN => self.handle_control_frame(id)?,
                        // 手机端要开「下一单」（多选排队）。REQUEST_RECEIVE 自己不载荷，
                        // 真正带载荷的是紧跟其后的一帧 RESUME_QUERY（整份文件清单）。
                        // 这里只做记号、把帧头吃掉是安全的；清单那一帧由下面那支读干净。
                        controller::REQUEST_RECEIVE => self.note_pending_receive(),
                        // 清单来了：载荷当场读完存进 pending_resume（回主循环再回位图）。
                        // 也可能 REQUEST_RECEIVE 已经落在重建窗口之前，这里补上记号。
                        controller::RESUME_QUERY => self.stash_resume_query()?,
                        // 306：反方向的续传状态帧。重建期间没有 handle_send 在读它，
                        // 也没有能接手它的人 —— 载荷读干净丢掉，绝不能留在流里。
                        controller::RESUME_STATE => self.discard_resume_state()?,
                        // 手机来电脑取文件（发送方向的「下一单」）：它的载荷变长，
                        // 而重建窗口里跑不了 handle_send —— 同样读干净丢掉。
                        controller::REQUEST_SEND => self.discard_send_request()?,
                        other => {
                            // 认不出来的帧：协议里没有"帧长"字段，它有没有载荷、
                            // 载荷多长都无从得知。但**不能**因此放弃重建 —— 那等于因为
                            // 手机上多传了一帧就把整条会话拆掉（就是这一轮报的那个 bug）。
                            // 只吃掉它的 opcode（和主循环遇到不认识指令时一模一样：
                            // 警告 + 忽略），重建继续等 ACK。
                            log::warn(&format!(
                                "{} {}（已忽略，重建继续）",
                                crate::i18n::tr(
                                    "重建通道时收到不认识的控制帧",
                                    "unknown control frame while waiting for REBUILD_ACK",
                                ),
                                other
                            ));
                        }
                    }
                }
                if let Some(stream) = restored {
                    // 每条通道恢复要多久，是「取消之后下一单多久才开跑」里最大的一块。
                    // 打这一行（debug 级）是为了以后不用再靠猜：真机实测 300~400ms/条，
                    // 大头是手机端 accept 线程里那 100ms 分类窗口（一次只处理一条连接）。
                    log::debug(&format!(
                        "rebuild: channel {} restored in {} ms",
                        name,
                        channel_started.elapsed().as_millis()
                    ));
                    rebuilt.push((name.clone(), stream));
                    break;
                }
                if attempt == 0 {
                    log::warn(&format!(
                        "{} {}",
                        crate::i18n::tr(
                            "手机端没能在窗口里收下通道，重连一次",
                            "the phone did not take the channel in time, retrying once",
                        ),
                        name
                    ));
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
            // 这里在等结论帧，但手机可能先插一帧队列快照进来（用户随时能改队列）。
            // 由辅助函数就地吞掉，别的帧照原样返回。
            let id = self.read_control_frame_skipping_queue()?;
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
                // 308：等结论的这段时间也会收到限速同步帧，就地吃掉（不处理的话
                // 它会被当成「帧序错位」，取消或同步一次就把会话打断）。
                controller::SPEED_LIMIT => {
                    let mb = proto::read_i32(&mut self.reader)?;
                    crate::ratelimit::adopt_configured_mb(mb as i64);
                    self.limit_epoch = crate::ratelimit::configured_epoch();
                    log::info(&format!(
                        "限速已同步到本机（手机上改的）：{}",
                        limit_text(mb)
                    ));
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
    ///
    /// **只做 shutdown，绝不 join 线程、也不拿锁**：这条路径是取消 / 停止时从会话线程
    /// 直接调用的，暂停态下传输线程正停在「等手机发下一块」里，等它们退出就是死等。
    /// shutdown 本身是一次内核调用（把本地这一端关掉，让对端那次阻塞的 write 立刻失败），
    /// 数据还堵在不在缓冲区里都一样，几十微秒就返回。
    fn shutdown_transfers(&self) {
        for (_, stream) in self.transfers.iter() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    /// 把一帧控制指令写出去，最多等 timeout 这么久；写成功返回 true。
    ///
    /// 控制通道的写超时是握手时设的 15 秒（HANDSHAKE_TIMEOUT），而手机暂停时它根本不读
    /// 控制通道 —— 一次写就可能顶满整个超时。取消路径上这一等就把会话线程钉住了
    /// （实测日志停在「先关传输通道，再通知手机端」之后，一行都没有）。
    ///
    /// 写失败时**不**把「这一帧的写名额」还给别人：一轮取消在控制通道上只写一帧 CANCEL
    /// 是硬要求（多写一帧会被手机端正在做的重建握手当成未知指令，帧序一乱整条会话就废，
    /// 见 control::claim_cancel_frame）。代价是这一次取消可能没通知到手机，那就靠它自己
    /// 那条被 shutdown 的传输通道发现异常 —— 它会重连，重建握手照样能对上。
    fn write_control_frame_within(&mut self, opcode: i16, timeout: Duration) -> bool {
        let mut written = self.writer.set_write_timeout(Some(timeout)).is_ok();
        if written {
            written =
                proto::write_i16(&mut self.writer, opcode).is_ok() && self.writer.flush().is_ok();
        }
        // 恢复握手时的写超时：下面还要写 REBUILD_REQ、RESULT 这些帧，不能留着这个短超时
        let _ = self.writer.set_write_timeout(Some(HANDSHAKE_TIMEOUT));
        written
    }

    fn poll_control_during_transfer(&mut self) -> io::Result<()> {
        // 本地点了「取消」就先回主循环去办正事（关传输通道 + 发 CANCEL），
        // 别把这一轮控制帧的应答先做完 —— 那些应答是**阻塞写**，手机那边一时
        // 读得慢就会顶住几百毫秒到几秒，用户看到的就是「按下取消，进度还在涨」。
        //
        // 说清楚它的边界：这只是「取消之后不再多干一份活」，打断不了已经在飞的
        // read/write（见 handle_receive 等待循环里那段注释）。
        if crate::control::cancelled() {
            return Ok(());
        }
        // 传输期间照样把「本机改了限速」推过去（这一步每圈都过，代价是一次原子读）。
        self.flush_speed_limit()?;
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
        // 6 = 最大的「定长载荷帧」整帧长度：帧头 2 + 限速帧的 i32 4（308）。
        // 窗口必须装得下最长的那个，否则帧永远判成「还没到齐」，一个字节都不读，
        // 它就永远卡在缓冲区里（实测：只开 3 字节时 SPEED_LIMIT 一直不被采纳）。
        let mut window = [0u8; 6];
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
            // 308：限速帧带一个 i32，不按 6 字节等就会把 4 字节载荷留在通道里，
            // 下一次读的「opcode」正好是那个 i32 的高位 —— 帧流当场错位。
            controller::SPEED_LIMIT => 4,
            // 309 的 QUEUE_STATE 载荷是变长的（条数 + 每条变长文件名），peek 这两个字节
            // 算不出整帧要多少，所以落到下面这个 0：帧头读完由 handle_control_frame
            // 顺着载荷读下去。手机是把整帧一次 write 出来的，中间那几毫秒的等待可以接受。
            _ => 0,
        };
        if available < 2 + payload_len {
            // 帧还没到齐，等它齐了再读，绝不把半个帧吃掉
            let _ = self
                .reader
                .set_read_timeout(Some(Duration::from_secs(120)));
            return Ok(());
        }
        // 读帧体前必须把超时恢复成正常值：QUEUE_STATE 是变长帧，payload_len 落在 0，
        // 也就是「peek 到 2 字节就动手读帧头」。手机虽然整帧一次 writeFully，TCP 仍可能
        // 把几百字节的文件名载荷切成两段 —— 还顶着 2ms 超时去读，读到一半就超时，
        // 帧体读残缺，下一个「opcode」成了文件名的半截字节，整条控制通道当场错位。
        // 定长帧（PAUSE_STATE / SPEED_LIMIT）在上面已经确认到齐，这一步对它们没有代价。
        let _ = self
            .reader
            .set_read_timeout(Some(Duration::from_secs(120)));
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
    /// 本机改了限速就把新值推给手机（308 的「两端共用一个值」）。
    ///
    /// 由界面去写 socket 是行不通的：控制通道有且只有一个写者（会话线程），
    /// 两个线程同时写会把帧切开。所以界面只改配置 + 提升代次，这里在会话
    /// 自己的轮询里发现代次变了再补发 —— 传输中（poll_control_during_transfer）
    /// 和空闲（主循环）都会转到这里，最迟 250ms 内发出去。
    fn flush_speed_limit(&mut self) -> io::Result<()> {
        let epoch = crate::ratelimit::configured_epoch();
        if epoch == self.limit_epoch {
            return Ok(());
        }
        self.limit_epoch = epoch;
        let mb = crate::ratelimit::configured_mb();
        proto::write_i16(&mut self.writer, controller::SPEED_LIMIT)?;
        proto::write_i32(&mut self.writer, mb)?;
        self.writer.flush()?;
        log::info(&format!(
            "限速已同步给手机：{}",
            limit_text(mb)
        ));
        Ok(())
    }

    /// 手机改了限速，推给本机：采纳它的值（不提升代次，否则会互相反弹）。
    fn handle_speed_limit(&mut self) -> io::Result<()> {
        let mb = proto::read_i32(&mut self.reader)?;
        crate::ratelimit::adopt_configured_mb(mb as i64);
        // 采纳之后本机代次没变，但记一下当前代次，免得下次 flush 时
        // 把别人给的值又当成"我改的"推回去。
        self.limit_epoch = crate::ratelimit::configured_epoch();
        log::info(&format!(
            "限速已同步到本机（手机上改的）：{}",
            limit_text(mb)
        ));
        Ok(())
    }

    /// 33（309）：手机推来「排队中」任务的快照。
    ///
    /// 载荷 `{i32 count}{ (UTF 文件名, i64 字节数, u8 状态) * count }`，整表覆盖。
    /// 只发不答：手机不等电脑确认，电脑端把这张表存进登记表就够了 ——
    /// 界面每 16ms 读一次登记表，读不到就当队列是空的，不需要一次控制通道往返。
    fn handle_queue_state(&mut self) -> io::Result<()> {
        let items = queue::read_items(&mut self.reader)?;
        let previous = queue::get_for(&self.device);
        let waiting = items
            .iter()
            .filter(|item| item.state == queue::state::WAITING)
            .count();
        // 手机在传输期间每秒还会补发一帧兜底（那边 ServerService 的 1 秒节拍），
        // 内容通常和上一帧一模一样 —— 每帧都按 info 记，日志就成了每秒一行。
        // 只有「这一帧和登记表里那张不一样」才值得写进日志。
        let line = format!(
            "{} {} {} {}",
            crate::i18n::tr("手机队列更新：共", "queue update from the phone:"),
            items.len(),
            crate::i18n::tr("条，其中等待中", "item(s), waiting:"),
            waiting
        );
        if previous == items {
            log::debug(&line);
        } else {
            log::info(&line);
        }
        // 整表覆盖（含空表）：手机每报一次就以这一次为准，见 queue::set_for
        queue::set_for(&self.device, items);
        Ok(())
    }

    /// 读下一个控制帧的 opcode；路上撞见 QUEUE_STATE 就地吃掉（解析存表）再继续读。
    ///
    /// 控制通道的读点都是「上下文特定」的：这里在等 RESULT（收尾时），那里在等
    /// RESUME_QUERY / RESUME_STATE（开传之前）。手机随时可能推队列快照（用户在手机上
    /// 又勾了几个文件、或者删掉排队里的某一条），它插在哪两次读之间只取决于线程调度。
    ///
    /// 所以不在每个等待点各写一个 switch 分支 —— 漏一处就是帧序错位（那一帧的载荷
    /// 会被当成下一个帧头读走，整条会话当场报废）。统一走这个入口：QUEUE_STATE 是
    /// 唯一「出现在哪儿都能吞掉」的帧，其它帧原样交回给调用方自己判。
    fn read_control_frame_skipping_queue(&mut self) -> io::Result<i16> {
        loop {
            let id = match self.pending_opcode.take() {
                Some(id) => id,
                None => proto::read_i16(&mut self.reader)?,
            };
            if id != controller::QUEUE_STATE {
                return Ok(id);
            }
            self.handle_queue_state()?;
        }
    }

    fn handle_control_frame(&mut self, id: i16) -> io::Result<()> {
        match id {
            controller::PING => {}
            controller::CHECK_PENDING => self.handle_check_pending()?,
            controller::SPEED_LIMIT => self.handle_speed_limit()?,
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
            // 309：队列快照在传输 / 重建期间也可能来（手机那头随时能改队列）。
            // 载荷是变长的，必须就地读完存表 —— 当 pending 留给主循环，
            // 「只读了帧头没读载荷」的通道立刻错位一拍。
            controller::QUEUE_STATE => self.handle_queue_state()?,
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
        // 同上：等 RESUME_STATE 时也要能跳过路上插进来的队列快照
        let id = self.read_control_frame_skipping_queue()?;
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
        // 307：限速计时窗随本次传输重新开始，避免上次设完之后积累的空闲时间
        // 变成等额的免费流量额度（限 10MB/s、隔 90 秒再传，前 900MB 完全不睡）。
        crate::ratelimit::reset_window();
        // 每次"手机来取文件"都重置会话计数，避免"已接收 10.41 GB"把上几次的也算进来
        crate::receiver::PROGRESS.reset();
        // 发送方向同样要登记通道名，否则明细里全是「通道 1 / 通道 2 …」。
        //
        // set_channels 原来**只在接收路径**调用（handle_receive），发送路径漏了。
        // 而明细那张表是 mark_sent / mark_received **共用**的同一套行：owner 存的是
        // 通道号，画的时候用 channel_name(owner) 换成名字。接收方向登记过所以正常，
        // 发送方向 names 一直是空的 → channel_name() 全返回 None → 整张表掉进
        // 「通道 N」的兜底文案（真机截图：推送 mp4 时 10 行全是「通道 1..通道 10」）。
        // 下标必须和下面 spawn 时的 index 一致（同一个 iter().enumerate() 顺序）。
        crate::receiver::PROGRESS.set_channels(
            self.transfers
                .iter()
                .map(|(name, _)| name.clone())
                .collect(),
        );

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
            // 点「停止」也要能打断传输：主循环这时候不转，只有这里看一眼
            if self.stop_requested() {
                log::warn(crate::i18n::tr(
                    "发送中途被停止（关掉传输通道）",
                    "sending stopped midway (closing the transfer channels)",
                ));
                self.shutdown_transfers();
                hard_error = Some(crate::i18n::tr("已停止", "stopped").to_string());
                break;
            }
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
        // 被取消的那一轮不能报「发送完成」：上一句是「用户点了取消」，
        // 下一句却是「完成，共 447.0 MB」，日志读起来像自相矛盾
        // （真机 2026-11-21 取「不保留」时就是这么写的）。
        log::info(&format!(
            "{} {} · {} · {:.1}s",
            if crate::control::cancelled() {
                crate::i18n::tr("已取消发送，已发出共", "cancelled after")
            } else {
                crate::i18n::tr("发送完成，共", "sent")
            },
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
        let entries = self.read_resume_query_entries()?;
        self.reply_resume_entries(receiver, entries)
    }

    /// 读一帧 RESUME_QUERY（帧头 + 载荷）。
    fn read_resume_query_entries(&mut self) -> io::Result<Vec<(String, u64, i64)>> {
        // 手机在 REQUEST_RECEIVE 之后立刻发这一帧，但队列快照有可能正好插在中间
        // （用户在手机上点了「发送」又顺手改了队列）：照常跳过它，别把帧序读歪。
        let id = self.read_control_frame_skipping_queue()?;
        if id != controller::RESUME_QUERY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected RESUME_QUERY, got {}", id),
            ));
        }
        self.read_resume_query_payload()
    }

    /// 读 RESUME_QUERY 的载荷：{i32 count}{ (UTF 路径, i64 大小, i64 修改时间) * count }
    /// —— 帧头已经读掉了。
    ///
    /// 和帧头拆开是给 rebuild_transfers / 主循环用的：那两处的 opcode 是自己按
    /// 帧序读到的（不能重读），而载荷必须当场读干净 —— 留在流里就是帧序错位；
    /// 但"回位图"那一步要 receiver，只能回主循环做。
    fn read_resume_query_payload(&mut self) -> io::Result<Vec<(String, u64, i64)>> {
        let count = proto::read_i32(&mut self.reader)?.max(0) as usize;
        let mut entries: Vec<(String, u64, i64)> = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            let path = proto::read_utf(&mut self.reader)?;
            let size = proto::read_i64(&mut self.reader)?.max(0) as u64;
            let mtime = proto::read_i64(&mut self.reader)?;
            entries.push((path, size, mtime));
        }
        Ok(entries)
    }

    /// 回一帧 RESUME_REPLY：每个文件"已经收到哪些块"的位图，并把这些块标进进度。
    fn reply_resume_entries(
        &mut self,
        receiver: &Arc<Receiver>,
        entries: Vec<(String, u64, i64)>,
    ) -> io::Result<()> {
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

    /// 记下「手机端要开下一单」这件事（REQUEST_RECEIVE）。
    ///
    /// 这一帧自己不带载荷（只有两字节 opcode），真正带载荷的是紧跟其后的
    /// RESUME_QUERY —— 见 stash_resume_query。这里只置记号：
    /// 处理这一单要 receiver，而当前线程（重建 / 传输收尾）拿不到它。
    fn note_pending_receive(&mut self) {
        self.pending_receive = true;
        log::info(crate::i18n::tr(
            "手机端要开始下一单（等通道就绪后接着接收）",
            "the phone queued the next transfer (it starts once the channels are back)",
        ));
    }

    /// 读掉一帧 RESUME_QUERY 的载荷（整份文件清单）并暂存，回主循环再回位图。
    ///
    /// 必须当场读完：这一帧是变长的，只读帧头就走，下一次「读 opcode」读到的是
    /// 清单里的字节 —— 真机日志上那一串 0 / 75 / -20960 / 418 就是这么来的。
    fn stash_resume_query(&mut self) -> io::Result<()> {
        // 走到这儿的调用方都已经把帧头（RESUME_QUERY 那两个字节）读掉了
        let entries = self.read_resume_query_payload()?;
        log::info(&format!(
            "{} {} {}",
            crate::i18n::tr(
                "手机端已经排好下一单（清单已读好，等通道重建完就开传）",
                "the phone queued the next transfer; its file list is buffered",
            ),
            entries.len(),
            crate::i18n::tr("个文件", "file(s)")
        ));
        // 清掉上一份：同一时刻只可能有一单在开头，旧的只可能是没来得及办就作废的那种
        self.pending_resume = Some(entries);
        // 记号在这儿置上就够了（上面那行日志已经说明"要做下一单"）：
        // 万一 10 那帧没走到 note_pending_receive（比如它落在重建窗口之前），
        // 光凭这一帧也得让主循环去把这一单办了。
        self.pending_receive = true;
        Ok(())
    }

    /// 读掉一帧 REQUEST_SEND（手机来电脑取文件）连同它后面的续传状态帧。
    ///
    /// 重建窗口里没有发送计划可跑（传输通道正在重建），但这一帧的载荷是变长的
    /// —— 只吃 opcode 就会把后面每一帧读歪。所以整段读干净再丢，日志里说清楚
    /// 这一次取文件请求没有被满足（用户再点一次就行）。
    fn discard_send_request(&mut self) -> io::Result<()> {
        // 字段顺序和 handle_send 读的那一段完全一致（协议里两者是一份）
        let count = proto::read_i32(&mut self.reader)?.max(0).min(4096);
        for _ in 0..count {
            let _source = proto::read_utf(&mut self.reader)?;
        }
        let _phone_dir = proto::read_utf(&mut self.reader)?;
        let _phone_fs = proto::read_i32(&mut self.reader)?;
        let _pc_dir = proto::read_utf(&mut self.reader)?;
        // 手机在这之后紧接着还会发一帧 RESUME_STATE（它已经有哪些块）：
        // handle_send 里是用 read_resume_state 同步读的，这里照同一个顺序读掉。
        let id = self.read_control_frame_skipping_queue()?;
        if id == controller::RESUME_STATE {
            self.discard_resume_state()?;
        } else {
            // 正常不会有别的帧；真出现了也不能丢 —— opcode 已经读走，放回 pending_opcode，
            // 由调用方的循环按原顺序接手（它会接着把这一帧的载荷读干净）。
            self.pending_opcode = Some(id);
        }
        log::warn(crate::i18n::tr(
            "重建通道时收到手机端取文件请求（已读干净丢弃，请让手机再点一次）",
            "a file pull request arrived while the channels were being rebuilt; it was dropped",
        ));
        Ok(())
    }

    /// 读掉一帧 RESUME_STATE 的载荷并丢掉。
    ///
    /// 306 的这帧只属于「电脑 → 手机」那个方向，正常由 handle_send 里的
    /// read_resume_state 同步读掉。它要是出现在别的地方（手机那边时序被取消 /
    /// 排队打乱），也必须把载荷读干净再丢 —— 留着它，下一条帧的头就是位图里的字节。
    fn discard_resume_state(&mut self) -> io::Result<()> {
        // count 是变长载荷的总开关：真错位时它可能是任意垃圾值，别照着它转几百万圈
        let count = proto::read_i32(&mut self.reader)?.max(0).min(65536) as usize;
        for _ in 0..count {
            let _dest = proto::read_utf(&mut self.reader)?;
            let _size = proto::read_i64(&mut self.reader)?;
            let _mtime = proto::read_i64(&mut self.reader)?;
            let len = proto::read_i32(&mut self.reader)?.max(0) as usize;
            let len = len.min(4 * 1024 * 1024);
            let mut bits = vec![0u8; len];
            if len > 0 {
                self.reader.read_exact(&mut bits)?;
            }
        }
        log::warn(crate::i18n::tr(
            "当前不该收到续传状态帧（已读干净丢弃）",
            "a RESUME_STATE frame arrived out of place; read and dropped",
        ));
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
        // 「下一单」这一单就在这里办了，记号清掉（清单接着从 pending_resume 取）
        self.pending_receive = false;
        // 新一轮开始了：把上一轮的「接收失败」清掉。
        // 不点「开始接收」、直接由手机发起的那一轮（手机主动发文件）也是从这里进的，
        // 不清的话上一轮的失败红字会挂在窗口上，看着像这一轮也失败了（真机 2026-11-17 撞见）。
        crate::receiver::clear_failure();
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
        // 通道名必须**在** handle_resume_query 之前登记。
        //
        // reset() 会连 channels 一起清空（它是「清干净再来一轮」）。
        // 原来 set_channels 排在 handle_resume_query **之后** —— 而续传查询会立刻
        // 把「磁盘上已经有的块」标成完成（mark_received → owner = OWNER_RESUMED）
        // 并建出文件条目。于是那一段时间的明细行算通道名时 channels 还是空的，
        // channel_name() 全返回 None，整张表掉进「通道 1 / 通道 2 …」的兜底文案；
        // 而续传一轮里未分配 / 续传的段特别多，正好把整张表占满 —— 真机上看到的
        // 「通道名称没有显示」就是这个（截图里 12 条通道 + 一堆「续传」行）。
        //
        // 顺序要求：reset → set_channels → handle_resume_query。
        //
        // 两份都要登记：全局那份给主窗口的大卡片，receiver.progress 那份是"本设备"的，
        // 多设备时每台设备一个操作块，各读各的（见 receiver::progress_for）。
        let channel_names: Vec<String> = self
            .transfers
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        receiver.progress.set_channels(channel_names.clone());
        crate::receiver::PROGRESS.set_channels(channel_names);
        // 305：手机发完 REQUEST_RECEIVE 会立刻来一帧 RESUME_QUERY（这次要传的文件清单），
        // 这里读它、回每个文件"已经收到哪些块"的位图，发送端据此跳过已收到的块；
        // 同时把这些块直接标进进度（手机端也是把它们当完成的）。
        //
        // 清单也可能已经替我们读好了（取消后重建通道时收到的那一单，
        // 见 rebuild_transfers / stash_resume_query）：那种情况下去流里再读一次
        // 会一直等到读超时 —— 那一帧早就消费掉了。
        match self.pending_resume.take() {
            Some(entries) => self.reply_resume_entries(receiver, entries)?,
            None => self.handle_resume_query(receiver)?,
        }
        // 接收方向没有发送计划：清掉上一次的发送分段，界面改按"谁写了第几块"还原
        reset_send_segments();
        // 通道名已在上面（reset 之后、续传查询之前）登记 —— 那时下标就和下面
        // spawn 时的 channel 一致，早登记才不会让续传自查那一段掉进兜底文案。
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
        // 关掉通道之后开始计时的宽限期：过了它就不再等线程，带着「还有线程没退」往下走。
        // 取消 / 停止都必须有这一个上限，否则暂停态下这里就是一堵墙（见 CANCEL_JOIN_GRACE）。
        let mut grace_deadline: Option<Instant> = None;
        while remaining > 0 {
            // 点「停止」也要能打断传输（主循环这时候不转，只有这里看一眼）。
            // 和取消同一条路：先关传输通道，手机端那次阻塞的 write 才会立刻失败。
            if self.stop_requested() {
                // 下面紧跟着就 break 出去，abort_shutdown 不用置：它的作用是
                // 「取消那段只关一次通道」，停止之后这一轮已经结束了。
                log::warn(crate::i18n::tr(
                    "接收中途被停止（关掉传输通道）",
                    "receiving stopped midway (closing the transfer channels)",
                ));
                self.shutdown_transfers();
                // 停止同样是「通知手机端」的时刻，这一帧也必须带超时写：
                // 手机暂停时它不读控制通道，裸 write 会顶满握手时设的 15 秒写超时。
                if crate::control::claim_cancel_frame() {
                    let _ = self.write_control_frame_within(controller::CANCEL, CANCEL_FRAME_TIMEOUT);
                }
                break;
            }
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
            // 3.0.0：本地点了「取消」（control::cancel_receive）就立刻通知手机端 + 关通道。
            // 放在这里而不是只放在 rebuild_transfers：那一步要等所有线程 join +
            // finish_with 写完续传日志，中间那几百毫秒到几秒手机一直在发
            // （用户报的"取消后手机还在继续发"）。
            if !abort_shutdown && crate::control::cancelled() {
                abort_shutdown = true;
                grace_deadline = Some(Instant::now() + CANCEL_JOIN_GRACE);
                log::warn(crate::i18n::tr(
                    "取消接收：先通知手机端，再关传输通道",
                    "cancel: telling the phone first, then closing the transfer channels",
                ));
                // 顺序：**先写 CANCEL，再关通道**。
                //
                // 以前是先关再写，理由是「手机端那次阻塞的 write 失败得越早它停得越早」。
                // 真机测下来这个理由站不住：shutdown 这一步本身要花将近 1 秒（6 条通道
                // 上有别的线程正在写，Windows 的 shutdown 得等内核把在途数据收拾掉，
                // 实测 985 ms），而手机端认取消靠的是控制通道上这一帧 —— 它读到就自己
                // 把 6 条通道关掉了，根本不依赖我们把 socket 关掉。
                // 所以先写帧：手机端立刻开始收尾，那 1 秒的 shutdown 就在它收尾的同时跑。
                // 关通道留着，是给「手机暂停了、没在读控制通道」那种情形兜底。
                //
                // 写这一帧**必须有上限**：手机暂停时它不读控制通道，裸 write 会一直顶到
                // 握手时设的 15 秒写超时才回来 —— 用户在界面上看到的就是「点了取消没反应」。
                // 写不出去只记一行日志，取消流程照旧往下走（关通道 + 重建）。
                // claim_cancel_frame() 保证重建阶段不会再写第二帧。
                if crate::control::claim_cancel_frame() {
                    let started = Instant::now();
                    if self.write_control_frame_within(controller::CANCEL, CANCEL_FRAME_TIMEOUT) {
                        log::warn(&format!(
                            "{}（写信令 {} ms）",
                            crate::i18n::tr(
                                "已通知手机端取消（CANCEL 帧已发出）",
                                "the phone has been told about the cancel (CANCEL sent)",
                            ),
                            started.elapsed().as_millis()
                        ));
                    } else {
                        log::warn(crate::i18n::tr(
                            "CANCEL 帧没写出去（手机端可能没在读控制通道），继续走重建",
                            "the CANCEL frame could not be written (the phone may not be reading the control channel); going on with the rebuild",
                        ));
                    }
                }
                // 最后才关通道（给「手机没读控制通道」兜底）。这一步在真机上要花将近
                // 1 秒，放在写帧之后，它就和手机端的收尾并行跑了。
                // 它只做 shutdown：不 join、不拿锁；「等线程退出」挪到循环外面的 join 段，
                // 那里有 grace_deadline 兜着。
                let shutdown_started = Instant::now();
                self.shutdown_transfers();
                log::debug(&format!(
                    "取消：关通道用了 {} ms",
                    shutdown_started.elapsed().as_millis()
                ));
            }
            for (i, (_, handle)) in handles.iter().enumerate() {
                if !finished[i] && handle.is_finished() {
                    finished[i] = true;
                    remaining -= 1;
                }
            }
            if remaining > 0 {
                // 宽限期到了还剩下的线程不再等：它们自己会随取消标志 / socket shutdown
                // 走掉，等在这里只会让取消这条路径没有终点。
                if let Some(deadline) = grace_deadline {
                    if Instant::now() >= deadline {
                        log::warn(&format!(
                            "{}（{} 条通道）",
                            crate::i18n::tr(
                                "取消：传输线程还没全部退出，先继续收尾（不再等它们）",
                                "cancel: some transfer threads are still running; going on without waiting for them",
                            ),
                            remaining
                        ));
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = self
            .reader
            .set_read_timeout(Some(Duration::from_secs(120)));
        // 等各通道线程退出，但**整段有上限**：grace_deadline 是取消 / 停止那一刻算好的
        // （正常传完时没有它，也不会有线程没退），过了它还没退的线程就脱离 join 继续跑。
        //
        // 为什么要这条：暂停态下传输线程正停在阻塞读里等手机发下一块，
        // 这里无条件的 join 就是取消流程真正的「墙」—— 会话线程钉在这儿，
        // 后面的 finish_with / rebuild_transfers 一步都走不到，日志也就停了。
        // 脱离 join 是安全的：线程持有的是 Arc<Receiver>，取消标志 + socket 已 shutdown
        // 之后它自己会退出；代价只是这一次少了一条通道的退出原因。
        let join_deadline = grace_deadline.unwrap_or_else(Instant::now);
        for (name, handle) in handles {
            while !handle.is_finished() && Instant::now() < join_deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if !handle.is_finished() {
                log::warn(&format!(
                    "{}: {}",
                    name,
                    crate::i18n::tr(
                        "传输线程还没退出（不再等它，先走取消收尾）",
                        "the transfer thread has not finished; not waiting for it, going on with the wrap-up",
                    )
                ));
                continue;
            }
            match handle.join() {
                Ok(Ok(ChannelEnd::Eof)) => {}
                Ok(Ok(ChannelEnd::RemoteAbort(reason))) => {
                    log::warn(&format!("{}: {}", name, reason));
                    aborts.push(format!("{}: {}", name, reason));
                }
                Ok(Err(e)) => {
                    // 用户取消时线程是以 Interrupted/连接被 reset 退出的，
                    // 那不是"接收失败"，别当成错误弹窗，也别写进错误结论帧。
                    // （卡在帧体里的线程由 TransferReader 叫醒，它故意用 Other
                    //   而不是 Interrupted —— read_exact 会重试 Interrupted，见那里的注释。）
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
            receiver.progress.reset();
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
            receiver.progress.reset();
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
            // 界面也要知道：失败不能亮「已完成」然后自己收起来（见 receiver::set_failure）
            crate::receiver::set_failure(&error);
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
            receiver.progress.reset();
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
                // 手机端报的失败（多半是 SHA-256 不一致）同样要让用户在窗口上看见
                crate::receiver::set_failure(&match peer.message.clone() {
                    Some(message) if !message.is_empty() => message,
                    _ => crate::i18n::tr(
                        "手机端报告传输或校验失败",
                        "the phone reported a failed transfer or check",
                    )
                    .to_string(),
                });
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

/// 传输通道的读包了一层：每次读最多等 TRANSFER_POLL，回来先看一眼「取消」。
///
/// 为什么不能直接用 read_exact：本端 shutdown 一个 socket **叫不醒**另一个线程里
/// 已经阻塞住的读（实测 Windows 上：本端 shutdown 立刻返回，阻塞中的读一直等到
/// 对端关连接才醒）。手机暂停时它就停在块中间不发数据，那一块永远凑不齐，
/// 于是这个线程会挂到 120 秒的读超时才回来 —— 而这期间取消流程还得等它退出，
/// 用户看到的就是「点了取消，界面和日志一起定格」。
///
/// 这里把「等 120 秒」换成了「每 50 毫秒看一眼取消标志」，顺带把「多久没有任何
/// 进展就当连接死了」也管起来（语义和原来的读超时一致）。
struct TransferReader<'a> {
    stream: &'a mut TcpStream,
    stall: Duration,
    last_progress: Instant,
}

impl<'a> TransferReader<'a> {
    fn new(stream: &'a mut TcpStream, stall: Duration) -> Self {
        TransferReader {
            stream,
            stall,
            last_progress: Instant::now(),
        }
    }

    /// peek 不消费数据，只是把还没处理的那几个字节打出来（日志用）
    fn peek(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.peek(buf)
    }
}

impl Read for TransferReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if crate::control::cancelled() {
                // 取消：先把这条通道关掉，手机端那次阻塞的 write 才会立刻失败
                // （见下面 read_transfer_channel 里那段注释），再把错误交上去。
                //
                // 这里**不能**用 ErrorKind::Interrupted：Read::read_exact 的默认实现
                // 会把它当成「系统调用被打断」重新再读一遍，取消就又被吞掉了
                // （第一版就是这么写的，线程照样卡在帧体里到 120 秒超时）。
                let _ = self.stream.shutdown(std::net::Shutdown::Both);
                return Err(io::Error::new(io::ErrorKind::Other, "cancelled by user"));
            }
            match self.stream.read(buf) {
                Ok(n) => {
                    if n > 0 {
                        self.last_progress = Instant::now();
                    }
                    return Ok(n);
                }
                Err(ref e) if is_timeout(e) => {
                    if self.last_progress.elapsed() >= self.stall {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "the transfer channel stalled",
                        ));
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
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
        let _ = stream.set_read_timeout(Some(TRANSFER_POLL));
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
            thread::sleep(TRANSFER_POLL);
        }
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
        // 等下一帧的帧头：用短超时的 peek 轮询，每一圈都看一眼「取消」。
        //
        // 以前这里直接拿 120 秒的读超时去 read_i16：手机在块与块之间暂停时它不发数据，
        // 这一读就要挂满 120 秒，而取消之后会话线程还得 join 它 —— 用户看到的「点了取消
        // 卡死」里就有一大半是这种没人打断的阻塞读。
        //
        // 帧头只有 2 字节，所以先用 peek 等它到齐（peek 不消费数据，等到了再正式读，
        // 不会把半截帧头吃到肚子里）；帧体那一段交给下面的 TransferReader，
        // 同样是每 50 毫秒看一眼取消。整条通道的读从此都随时可以放手。
        let mut head = [0u8; 2];
        loop {
            if crate::control::cancelled() {
                // 和下面那段「取消必须关通道」同一个道理：先让手机那次阻塞的 write 失败
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled by user"));
            }
            match stream.peek(&mut head) {
                // 对端把这条通道关了（手机取消 / 断线）：报给上层，别在这里转圈
                Ok(0) => {
                    return Ok(ChannelEnd::RemoteAbort(
                        crate::i18n::tr("手机端取消了传输", "the phone cancelled the transfer")
                            .to_string(),
                    ))
                }
                // 整帧头到齐。peek 不消费数据，下面 read_i16 读到的还是这两个字节
                Ok(n) if n >= 2 => break,
                // 半个帧头：它在 TCP 的下一段里，歇一下再看，别空转
                Ok(_) => thread::sleep(Duration::from_millis(2)),
                Err(ref e) if is_timeout(e) => {}
                Err(e) if is_disconnect(&e) => {
                    return Ok(ChannelEnd::RemoteAbort(
                        crate::i18n::tr("手机端断开了连接", "the phone closed the connection")
                            .to_string(),
                    ))
                }
                Err(e) => return Err(e),
            }
        }
        // 帧头已经在缓冲区里了（上面 peek 确认过），从这里开始整帧都走取消感知的读：
        // 帧体最大 1 MiB，手机暂停就是停在块中间，裸 read_exact 会把这一等变成 120 秒。
        let mut reader = TransferReader::new(&mut stream, TRANSFER_STALL);
        let header = match proto::read_i16(&mut reader) {
            Ok(h) => h,
            Err(e) => {
                log::debug(&format!("ch{}: read header failed: {}", channel, e));
                return Err(e);
            }
        };
        match header {
            transfer::FILE => {
                let _file_index = proto::read_i32(&mut reader)?;
                let path = proto::read_utf(&mut reader)?;
                let modified_ms = proto::read_i64(&mut reader)?;
                let total_size = proto::read_i64(&mut reader)?;
                let index = proto::read_i32(&mut reader)?;
                let length = proto::read_i32(&mut reader)?;
                // 303：这一块的 CRC32，写盘前必须一致
                let expected_crc = proto::read_i32(&mut reader)? as u32;
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
                    reader.read_exact(&mut buffer[..length])?;
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
                let _file_index = proto::read_i32(&mut reader)?;
                let path = proto::read_utf(&mut reader)?;
                let _modified_ms = proto::read_i64(&mut reader)?;
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
                let n = reader.peek(&mut tail).unwrap_or(0);
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

    /// 电脑端主动推给手机的限速值（308）。由 {@link next_opcode} 记录。
    static PUSHED_LIMITS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

    /// 读下一个控制帧的 opcode，顺手吞掉电脑端主动推来的 SPEED_LIMIT（308）。
    ///
    /// 那一帧是电脑端「本机限速被改过」时在轮询里补发的，落在对端哪一次读帧上
    /// 只取决于线程调度 —— mock 里凡是「等某个特定帧」的地方都走这里，断言
    /// 才不会因为它的先后顺序随机红。
    fn next_opcode(cr: &mut &TcpStream) -> i16 {
        loop {
            let code = proto::read_i16(cr).unwrap();
            if code == controller::SPEED_LIMIT {
                // 载荷是 i32 MB/s，读掉它才算把这一帧消费干净。
                // 同时记下来，测试末尾断言「电脑改了限速确实推了」。
                PUSHED_LIMITS
                    .lock()
                    .unwrap()
                    .push(proto::read_i32(cr).unwrap());
                continue;
            }
            return code;
        }
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
            // 308 起电脑端改了限速会在轮询里补发一帧 SPEED_LIMIT，它落到对端哪一次
            // 读帧上取决于线程调度 —— mock 里所有「读下一个 opcode」都走这个助手，
            // 顺手把这种同步帧吞掉，断言就不会随机红。
            let mut header = [0u8; 4];
            cr.read_exact(&mut header).unwrap();
            assert_eq!(&header, proto::CLIENT_HEADER);
            assert_eq!(proto::read_i32(&mut cr).unwrap(), proto::VERSION_CODE);
            proto::write_bool(&mut cw, true).unwrap();
            // 限速协商：版本一致之后先交换（电脑先报自己的配置值，手机把两边
            // 认下来的共用值写回来）。这一段是握手的一部分，漏掉它电脑端就会
            // 一直等这个 i32 而超时 —— 加协议字段时忘了同步 mock，测试就是从这儿红的。
            let pc_limit = proto::read_i32(&mut cr).unwrap();
            assert!(
                pc_limit >= 0,
                "电脑端报的限速应该是非负整数（0 = 不限速），实际 {}",
                pc_limit
            );
            // mock 手机这一侧：本机值是 0（不限速），共用值 = 电脑非 0 就用电脑的
            let shared = crate::ratelimit::shared_value(pc_limit as i64, 0) as i32;
            proto::write_i32(&mut cw, shared).unwrap();
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
            // 308：连上之后手机改了限速，推给电脑一帧。电脑必须就地采纳 ——
            // 认不出来就会被当成「帧序错位」把整条会话断掉。
            proto::write_i16(&mut cw, controller::SPEED_LIMIT).unwrap();
            proto::write_i32(&mut cw, 9).unwrap();
            proto::write_i16(&mut cw, controller::REQUEST_RECEIVE).unwrap();
            // 309：手机可能在这一帧之后、RESUME_QUERY 之前插一帧队列快照
            // （用户在手机上点了「发送」又顺手改了队列，就是这个时序）。
            // 电脑端必须顺着载荷把它读完、存进登记表 —— 认成「等错了帧」直接断开，
            // 或只读帧头不读载荷，都会让帧流错位。
            proto::write_i16(&mut cw, controller::QUEUE_STATE).unwrap();
            crate::queue::write_items(
                &mut cw,
                &[
                    crate::queue::QueueItem {
                        name: "排队一.mp4".to_string(),
                        size: 4096,
                        state: crate::queue::state::WAITING,
                    },
                    crate::queue::QueueItem {
                        name: "排队二.mp4".to_string(),
                        size: 8192,
                        state: crate::queue::state::RUNNING,
                    },
                ],
            )
            .unwrap();
            // 305：REQUEST_RECEIVE 之后紧跟着必须是 RESUME_QUERY（电脑端同步读这一帧），
            // 一个文件都不查也要发，只是 count = 0。
            proto::write_i16(&mut cw, controller::RESUME_QUERY).unwrap();
            proto::write_i32(&mut cw, 0).unwrap();
            // 本机（电脑）在连上之后把限速改成了 11，会话线程会在轮询里补发一帧
            // SPEED_LIMIT。它插在哪两次读之间只取决于线程调度，所以不赌顺序：
            // next_opcode 遇到就记进 PUSHED_LIMITS，测试末尾统一断言。
            assert_eq!(
                next_opcode(&mut cr),
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
            // 电脑**可能**在收到这个查询之前就已经把块全收完、把结论帧发出来了 ——
            // 两个都是合法时序，所以两种都接：拿到 PAUSE_REPLY 就回读那个 bool，
            // 拿到 RESULT 就当「已经收尾」，下面那段结论帧不再重读 opcode。
            // （以前这里写死了「下一个帧一定是 PAUSE_REPLY」，偶发红就是撞上另一条路。）
            let mut result_opcode_consumed = false;
            match next_opcode(&mut cr) {
                controller::PAUSE_REPLY => {
                    assert!(!proto::read_bool(&mut cr).unwrap());
                }
                controller::RESULT => result_opcode_consumed = true,
                other => panic!("暂停查询之后来了一帧没预料到的 {other}"),
            }

            writer_a.join().unwrap();
            writer_b.join().unwrap();

            // 302 收尾：电脑先给落盘结论（RESULT），手机再回读文件结论（RESULT）
            if !result_opcode_consumed {
                assert_eq!(next_opcode(&mut cr), controller::RESULT);
            }
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

    /// 这几个用例都会跑握手，握手会写全局的限速（ratelimit），
    /// 和 ratelimit 自己的用例抢同一套量 —— 并行跑就会偶发红，所以统一排队。
    fn rate_serial() -> std::sync::MutexGuard<'static, ()> {
        crate::ratelimit::TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn receives_files_from_a_mock_phone() {
        let _serial = rate_serial();
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

        // 308：本机配置 7，mock 手机报 0（不限速）→ 共用值必须是 7，
        // 而且电脑要真的采纳它（连上就生效，不等第一块数据）。
        PUSHED_LIMITS.lock().unwrap().clear();
        // 309：队列登记表是进程级的，先清干净再断言 mock 推来的那一帧
        crate::queue::clear("MOCK-QUEUE-PHONE");
        crate::ratelimit::set_configured_mb(7);
        let server = spawn_mock_phone(listener, folder_path.clone(), blocks, home.clone());
        let mut session = Session::connect(IpAddr::from([127, 0, 0, 1]), port, &home, 0).unwrap();
        // 串号是队列登记表的键（会话里用 self.device 入库，界面用设备串号查）
        session.device = "MOCK-QUEUE-PHONE".to_string();
        assert_eq!(session.interface_names(), "USB_ADB, wlan0");
        assert_eq!(
            crate::ratelimit::configured_mb(),
            7,
            "两端共用值应当是电脑配置值（本机非 0 以本机为准）"
        );
        let epoch_after_connect = crate::ratelimit::configured_epoch();
        assert_eq!(
            crate::ratelimit::limit(),
            7 * 1024 * 1024,
            "采纳共用值之后要立即对发送路径生效"
        );
        // 连上之后用户在电脑上把限速改成 11：会话线程要主动推给手机
        // （mock 那边会断言收到 SPEED_LIMIT + 11）。
        crate::ratelimit::set_configured_mb(11);
        let epoch_after_local_change = crate::ratelimit::configured_epoch();
        assert_eq!(
            epoch_after_local_change,
            epoch_after_connect + 1,
            "本机改一次只该提一代"
        );
        let receiver = Arc::new(Receiver::new(util::normalize_dir(&dir)));
        let stop = Arc::new(AtomicBool::new(false));
        match session.run(&receiver, &stop) {
            SessionEnd::Shutdown => {}
            SessionEnd::Disconnected(reason) => panic!("unexpected disconnect: {}", reason),
            SessionEnd::Stopped => panic!("unexpected stop"),
        }
        server.join().unwrap();

        // mock 在握手之后推了一帧 SPEED_LIMIT 9（手机那边改的）：它是在 run() 的
        // 主循环里被吃掉的，所以断言必须放在 run 之后。
        assert_eq!(
            crate::ratelimit::configured_mb(),
            9,
            "SPEED_LIMIT 帧应当被就地采纳（而不是当成帧序错位断开）"
        );
        assert_eq!(crate::ratelimit::limit(), 9 * 1024 * 1024);
        assert_eq!(
            crate::ratelimit::configured_epoch(),
            epoch_after_local_change,
            "采纳对端推来的值不得提升代次，否则两端会互相把同一个数推个没完"
        );
        // 反方向：本机（电脑）改了限速，会话线程要在轮询里把它推给手机。
        // 这一帧可能插在任何两次读之间，所以由 next_opcode 一路记下来，这里才断言。
        assert!(
            PUSHED_LIMITS.lock().unwrap().contains(&11),
            "电脑改了限速要把新值推给手机，实际推送：{:?}",
            PUSHED_LIMITS.lock().unwrap()
        );

        // 309：mock 在 REQUEST_RECEIVE 之后插的那一帧 QUEUE_STATE 必须落在登记表里，
        // 而且只有「等待中」的那一条会被界面画成等待块（state == 1 就是正在传的那个块）。
        let queued = crate::queue::get_for("MOCK-QUEUE-PHONE");
        assert_eq!(queued.len(), 2, "整表覆盖要存下手机报来的全部条目");
        assert_eq!(queued[0].name, "排队一.mp4");
        assert_eq!(queued[0].size, 4096);
        assert_eq!(queued[0].state, crate::queue::state::WAITING);
        let waiting = crate::queue::waiting_for("MOCK-QUEUE-PHONE");
        assert_eq!(waiting.len(), 1, "只有 state == 0 的那条才是等待块");
        assert_eq!(waiting[0].name, "排队一.mp4");
        crate::queue::clear("MOCK-QUEUE-PHONE");

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
        let _serial = rate_serial();
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

    /// 这一轮的核心验收：取消之后手机立刻开「下一单」，不许把重建和会话搞死。
    ///
    /// 复现真机 06:54:45 的时序（逐字对应日志里那三条「意外的控制帧」）：
    ///   1. 电脑点了「取消」→ rebuild_transfers 发出 CANCEL，等 REBUILD_ACK；
    ///   2. 手机端（多选排队）不发 ACK，先发 REQUEST_RECEIVE(10)，
    ///      紧跟一帧 RESUME_QUERY(28)，载荷是下一单的整份文件清单；
    ///   3. 再回 REBUILD_ACK。
    ///
    /// 以前第 2 步那支 other => break 会把 10 的帧头读走、把 28 整帧留在流里，
    /// 之后读到的「opcode」就是清单里的字节（日志上的 0 / 75 / -20960 / 418），
    /// 重建失败 → 会话被判成断线 → 进度窗口整个消失。
    ///
    /// 现在断言：这两帧的载荷都被读走（清单进了 pending_resume）、REBUILD_ACK 照常
    /// 收到、重建成功、下一单在主循环里被补办（回 RESUME_REPLY）、整条帧流一帧不差
    /// （REBUILD_ACK / RESUME_REPLY / EOF / RESULT / SHUTDOWN 全按顺序对上），
    /// 会话以 Shutdown 正常收场 —— 不是 Disconnected。
    #[test]
    fn a_queued_next_task_does_not_break_the_cancel_rebuild() {
        let _serial = rate_serial();
        let dir = temp_dir("rebuild-next-task");
        let home = util::normalize_dir(&dir).to_string_lossy().to_string();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let next_path = dir.join("下一单.bin").to_string_lossy().to_string();
        let next_size = 2 * BLOCK_SIZE;
        let next_mtime = 1_700_000_000_000i64;

        let next_path_for_mock = next_path.clone();
        let home_for_mock = home.clone();
        let server = thread::spawn(move || {
            let (control, _) = listener.accept().unwrap();
            control
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            control
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut cr: &TcpStream = &control;
            let mut cw: &TcpStream = &control;
            // ---- 握手：和 spawn_mock_phone 那一套完全一致 ----
            let mut header = [0u8; 4];
            cr.read_exact(&mut header).unwrap();
            assert_eq!(&header, proto::CLIENT_HEADER);
            assert_eq!(proto::read_i32(&mut cr).unwrap(), proto::VERSION_CODE);
            proto::write_bool(&mut cw, true).unwrap();
            let pc_limit = proto::read_i32(&mut cr).unwrap();
            let shared = crate::ratelimit::shared_value(pc_limit as i64, 0) as i32;
            proto::write_i32(&mut cw, shared).unwrap();
            proto::write_i32(&mut cw, 2).unwrap();
            for name in ["USB_ADB", "wlan0"] {
                proto::write_utf(&mut cw, name).unwrap();
                proto::write_u8(&mut cw, 4).unwrap();
                cw.write_all(&[127, 0, 0, 1]).unwrap();
                proto::write_u8(&mut cw, 0).unwrap();
            }
            let mut old = Vec::new();
            for _ in 0..2 {
                let (transfer, _) = listener.accept().unwrap();
                assert!(proto::read_bool(&mut cr).unwrap());
                assert!(!proto::read_utf(&mut cr).unwrap().is_empty());
                proto::write_bool(&mut cw, true).unwrap();
                old.push(transfer);
            }
            proto::write_i32(&mut cw, 512).unwrap();
            assert!(proto::read_bool(&mut cr).unwrap());
            proto::write_bool(&mut cw, true).unwrap();
            assert_eq!(
                proto::read_i32(&mut cr).unwrap(),
                proto::current_file_system()
            );
            assert_eq!(proto::read_utf(&mut cr).unwrap(), home_for_mock);

            // ---- 电脑点了「取消」：重建的敲门砖是 CANCEL ----
            assert_eq!(
                next_opcode(&mut cr),
                controller::CANCEL,
                "重建开始时应先发出一帧 CANCEL"
            );

            // ---- 手机端立刻开下一单，全部插在 REBUILD_ACK 之前 ----
            // 10 自己不带载荷，带载荷的是紧跟其后的 28（整份文件清单）
            proto::write_i16(&mut cw, controller::REQUEST_RECEIVE).unwrap();
            proto::write_i16(&mut cw, controller::RESUME_QUERY).unwrap();
            proto::write_i32(&mut cw, 1).unwrap();
            proto::write_utf(&mut cw, &next_path_for_mock).unwrap();
            proto::write_i64(&mut cw, next_size as i64).unwrap();
            proto::write_i64(&mut cw, next_mtime).unwrap();
            // 306 的反方向续传状态帧：重建窗口里没人处理它，但载荷是变长的，
            // 必须被读干净丢掉（留下来同样是帧序错位）
            proto::write_i16(&mut cw, controller::RESUME_STATE).unwrap();
            proto::write_i32(&mut cw, 1).unwrap();
            proto::write_utf(&mut cw, "back.bin").unwrap();
            proto::write_i64(&mut cw, BLOCK_SIZE as i64).unwrap();
            proto::write_i64(&mut cw, next_mtime).unwrap();
            proto::write_i32(&mut cw, 1).unwrap();
            cw.write_all(&[0xFF]).unwrap();
            // 发送方向的「下一单」（手机来电脑取文件）：重建窗口里跑不了 handle_send，
            // 但它同样是一整段变长载荷，必须读干净丢掉，不能留在流里
            proto::write_i16(&mut cw, controller::REQUEST_SEND).unwrap();
            proto::write_i32(&mut cw, 1).unwrap();
            proto::write_utf(&mut cw, "/sdcard/取这个.mp4").unwrap();
            proto::write_utf(&mut cw, "/sdcard/DCIM").unwrap();
            proto::write_i32(&mut cw, proto::current_file_system()).unwrap();
            proto::write_utf(&mut cw, &home_for_mock).unwrap();
            // 这一帧后面紧跟的续传状态帧（这一单没有已收到的块）
            proto::write_i16(&mut cw, controller::RESUME_STATE).unwrap();
            proto::write_i32(&mut cw, 0).unwrap();

            // ---- 现在才回两条 REBUILD_ACK ----
            let mut rebuilt = Vec::new();
            for _ in 0..2 {
                let (transfer, _) = listener.accept().unwrap();
                assert_eq!(
                    next_opcode(&mut cr),
                    controller::REBUILD_REQ,
                    "手机端开下一单不能影响重建握手的第一帧"
                );
                assert!(!proto::read_utf(&mut cr).unwrap().is_empty());
                proto::write_i16(&mut cw, controller::REBUILD_ACK).unwrap();
                proto::write_bool(&mut cw, true).unwrap();
                rebuilt.push(transfer);
            }
            drop(old);

            // ---- 主循环把被推迟的那一单办了：回 RESUME_REPLY ----
            assert_eq!(
                next_opcode(&mut cr),
                controller::RESUME_REPLY,
                "重建之后必须补办下一单：回 RESUME_REPLY"
            );
            assert_eq!(
                proto::read_i32(&mut cr).unwrap(),
                1,
                "RESUME_REPLY 的文件数要和手机发来的清单一致（帧没错位才读得对）"
            );
            assert!(
                !proto::read_bool(&mut cr).unwrap(),
                "文件不在盘上，不该报可续传"
            );
            assert_eq!(proto::read_i32(&mut cr).unwrap(), 0);

            // ---- 两条重建回来的通道各来一帧 EOF：这一单没有数据块 ----
            for transfer in &rebuilt {
                let mut w: &TcpStream = transfer;
                proto::write_i16(&mut w, transfer::EOF).unwrap();
            }

            // ---- 收尾：电脑先给落盘结论，手机回读文件结论，然后断开 ----
            assert_eq!(
                next_opcode(&mut cr),
                controller::RESULT,
                "帧流错了才会在这儿读到别的"
            );
            assert!(proto::read_bool(&mut cr).unwrap());
            assert_eq!(proto::read_i32(&mut cr).unwrap(), 0);
            proto::write_i16(&mut cw, controller::RESULT).unwrap();
            proto::write_bool(&mut cw, true).unwrap();
            proto::write_i32(&mut cw, 0).unwrap();
            proto::write_i16(&mut cw, controller::SHUTDOWN).unwrap();
            thread::sleep(Duration::from_millis(150));
        });

        crate::control::reset();
        let mut session = Session::connect(IpAddr::from([127, 0, 0, 1]), port, &home, 0).unwrap();
        session.device = "MOCK-NEXT-TASK-PHONE".to_string();
        // 界面上点「取消 · 不保留」= 先记选择，再置取消标志（见 control::cancel_receive）
        crate::control::set_discard_partial(true);
        crate::control::cancel();

        let rebuilt = match session.rebuild_transfers() {
            Ok(count) => count,
            Err(e) => panic!("手机端开下一单不该让重建失败（真机就是这儿断的）：{}", e),
        };
        assert_eq!(rebuilt, 2, "两条通道都该重建回来");

        // 这两帧必须已经读走并记下来 —— 留在流里就是下一条帧的头被读歪
        assert!(session.pending_receive, "REQUEST_RECEIVE 要记下来交给主循环");
        let pending = match session.pending_resume.as_ref() {
            Some(entries) => entries.clone(),
            None => panic!("RESUME_QUERY 的载荷必须已经被读进来"),
        };
        assert_eq!(pending.len(), 1, "清单里就那一个文件");
        assert_eq!(pending[0].0, next_path);
        assert_eq!(pending[0].1, next_size);
        assert_eq!(pending[0].2, next_mtime);

        let receiver = Arc::new(Receiver::new(util::normalize_dir(&dir)));
        let stop = Arc::new(AtomicBool::new(false));
        match session.run(&receiver, &stop) {
            SessionEnd::Shutdown => {}
            SessionEnd::Disconnected(reason) => panic!("会话不该断（真机就是这个症状）：{}", reason),
            SessionEnd::Stopped => panic!("unexpected stop"),
        }
        server.join().unwrap();
        assert!(!session.pending_receive, "下一单办完就该把记号清掉");
        assert!(
            session.pending_resume.is_none(),
            "清单用完就该清掉，别带到再下一单"
        );
        crate::control::reset();
    }

    /// 手机已经暂停（PAUSE_STATE = true）时，用户在电脑上点「取消」。
    ///
    /// 这是「取消卡死」那条 bug 的现场：暂停态下手机既不往传输通道写、也不读控制通道，
    /// 于是电脑这边「关通道 / 写 CANCEL / 等传输线程退出」任何一步都可能永远等下去 ——
    /// 实测日志停在「取消接收：先关传输通道，再通知手机端」那一行，之后一行都没有，
    /// 界面也没有任何重建 / 报错。
    ///
    /// 现在断言三件事：
    ///   1. CANCEL 帧确实写进控制通道了（mock 手机在 1 秒内读得到它）；
    ///   2. 整条取消路径在有限时间内走完（会话线程不会为了等传输线程而挂住）；
    ///   3. 会话继续走到重建阶段：两条通道都重建回来，最后正常 SHUTDOWN 收场，
    ///      既不是挂死，也不是被当成断线。
    #[test]
    fn cancel_while_the_phone_is_paused_does_not_hang() {
        let _serial = rate_serial();
        let dir = temp_dir("cancel-paused");
        let home = util::normalize_dir(&dir).to_string_lossy().to_string();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        // 手机暂停之后「不读控制通道」多久（模拟它那边停在暂停等待里）
        const PHONE_DEAF_MS: u64 = 400;
        // 电脑端确实把暂停应用到本地了（收到它回的 PAUSE_REPLY 才算数）
        let paused_confirmed = Arc::new(AtomicBool::new(false));
        // mock 手机读到 CANCEL 帧的时刻 —— 这就是「取消到 CANCEL 发出」的实测点
        let cancel_seen = Arc::new(Mutex::new(None::<Instant>));
        let rebuilt_count = Arc::new(AtomicU64::new(0));

        // 手机停在「一块的中间」用的那个文件（这一块永远传不完，因为手机随后就暂停了）
        let partial_path = dir.join("半块.bin").to_string_lossy().to_string();
        let home_for_mock = home.clone();
        let partial_for_mock = partial_path.clone();
        let paused_flag = Arc::clone(&paused_confirmed);
        let cancel_stamp = Arc::clone(&cancel_seen);
        let rebuilt = Arc::clone(&rebuilt_count);
        let server = thread::spawn(move || {
            let (control, _) = listener.accept().unwrap();
            control
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            control
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut cr: &TcpStream = &control;
            let mut cw: &TcpStream = &control;
            // ---- 握手：和 spawn_mock_phone 那一套完全一致 ----
            let mut header = [0u8; 4];
            cr.read_exact(&mut header).unwrap();
            assert_eq!(&header, proto::CLIENT_HEADER);
            assert_eq!(proto::read_i32(&mut cr).unwrap(), proto::VERSION_CODE);
            proto::write_bool(&mut cw, true).unwrap();
            let pc_limit = proto::read_i32(&mut cr).unwrap();
            let shared = crate::ratelimit::shared_value(pc_limit as i64, 0) as i32;
            proto::write_i32(&mut cw, shared).unwrap();
            proto::write_i32(&mut cw, 2).unwrap();
            for name in ["USB_ADB", "wlan0"] {
                proto::write_utf(&mut cw, name).unwrap();
                proto::write_u8(&mut cw, 4).unwrap();
                cw.write_all(&[127, 0, 0, 1]).unwrap();
                proto::write_u8(&mut cw, 0).unwrap();
            }
            let mut old = Vec::new();
            for _ in 0..2 {
                let (transfer, _) = listener.accept().unwrap();
                assert!(proto::read_bool(&mut cr).unwrap());
                assert!(!proto::read_utf(&mut cr).unwrap().is_empty());
                proto::write_bool(&mut cw, true).unwrap();
                old.push(transfer);
            }
            proto::write_i32(&mut cw, 512).unwrap();
            assert!(proto::read_bool(&mut cr).unwrap());
            proto::write_bool(&mut cw, true).unwrap();
            assert_eq!(
                proto::read_i32(&mut cr).unwrap(),
                proto::current_file_system()
            );
            assert_eq!(proto::read_utf(&mut cr).unwrap(), home_for_mock);

            // ---- 手机开始发送：这一单故意一个文件都没有（要考的是取消，不是数据）----
            proto::write_i16(&mut cw, controller::REQUEST_RECEIVE).unwrap();
            proto::write_i16(&mut cw, controller::RESUME_QUERY).unwrap();
            proto::write_i32(&mut cw, 0).unwrap();
            assert_eq!(
                next_opcode(&mut cr),
                controller::RESUME_REPLY,
                "电脑端应该用 RESUME_REPLY 回答续传查询"
            );
            assert_eq!(proto::read_i32(&mut cr).unwrap(), 0);

            // ---- 手机在「一块的中间」停下：先写半块，再暂停 ----
            //
            // 这是真机上的停法：暂停不是停在块边界上，而是手机写了一半就停了。
            // 电脑那条通道的读循环此刻正卡在帧体中间（还差 1 MiB - 4 KiB），
            // 以前那里是一次 120 秒的 read_exact：本端 shutdown 根本叫不醒它
            // （实测：本端 shutdown 立刻返回，阻塞中的读要等对端关连接才醒），
            // 于是取消之后会话线程只能干等 —— 这就是「卡死在取消路径上」的真相。
            {
                let mut w: &TcpStream = &old[0];
                proto::write_i16(&mut w, transfer::FILE).unwrap();
                proto::write_i32(&mut w, 1).unwrap();
                proto::write_utf(&mut w, &partial_for_mock).unwrap();
                proto::write_i64(&mut w, 1_700_000_000_000).unwrap();
                proto::write_i64(&mut w, BLOCK_SIZE as i64).unwrap();
                proto::write_i32(&mut w, 0).unwrap();
                proto::write_i32(&mut w, BLOCK_SIZE as i32).unwrap();
                // 这一块的 CRC32：反正永远收不齐，写什么都无所谓
                proto::write_i32(&mut w, 0).unwrap();
                w.write_all(&[7u8; 4096]).unwrap();
                w.flush().unwrap();
            }
            // 让电脑那条通道先把帧头读进去、停在帧体上，再让暂停生效（时序才确定）
            thread::sleep(Duration::from_millis(150));

            // ---- 手机点了暂停：电脑回 PAUSE_REPLY 就说明它已经应用了 ----
            proto::write_i16(&mut cw, controller::PAUSE_STATE).unwrap();
            proto::write_bool(&mut cw, true).unwrap();
            assert_eq!(
                next_opcode(&mut cr),
                controller::PAUSE_REPLY,
                "暂停之后电脑端要回 PAUSE_REPLY"
            );
            assert!(
                proto::read_bool(&mut cr).unwrap(),
                "电脑端应当已经跟着暂停（否则下面这一轮就不是暂停态下的取消）"
            );
            // 从这里开始电脑端确实处于「手机已暂停」：传输线程停在暂停等待里，
            // 手机这边也不读控制通道 —— 正是取消会卡死的那个状态。
            paused_flag.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(PHONE_DEAF_MS));

            // ---- 现在才回头读控制通道：取消那一帧必须已经在这儿 ----
            assert_eq!(
                next_opcode(&mut cr),
                controller::CANCEL,
                "暂停态下点了取消，CANCEL 帧必须真的写出来（不能卡在写控制帧上）"
            );
            *cancel_stamp.lock().unwrap() = Some(Instant::now());

            // ---- 取消之后电脑会把传输通道重建回来（会话不断）----
            let mut rebuilt_streams = Vec::new();
            for _ in 0..2 {
                let (transfer, _) = listener.accept().unwrap();
                assert_eq!(
                    next_opcode(&mut cr),
                    controller::REBUILD_REQ,
                    "取消之后紧接着就该是重建握手"
                );
                assert!(!proto::read_utf(&mut cr).unwrap().is_empty());
                proto::write_i16(&mut cw, controller::REBUILD_ACK).unwrap();
                proto::write_bool(&mut cw, true).unwrap();
                rebuilt.fetch_add(1, Ordering::SeqCst);
                rebuilt_streams.push(transfer);
            }
            drop(old);

            proto::write_i16(&mut cw, controller::SHUTDOWN).unwrap();
            thread::sleep(Duration::from_millis(150));
            drop(rebuilt_streams);
        });

        crate::control::reset();
        let mut session = Session::connect(IpAddr::from([127, 0, 0, 1]), port, &home, 0).unwrap();
        session.device = "MOCK-PAUSED-CANCEL".to_string();
        let receiver = Arc::new(Receiver::new(util::normalize_dir(&dir)));
        let stop = Arc::new(AtomicBool::new(false));
        let receiver_for_run = Arc::clone(&receiver);
        let stop_for_run = Arc::clone(&stop);
        // run() 会一直转到会话结束，所以放到自己的线程里：主线程要在「暂停中」点取消
        let runner = thread::spawn(move || session.run(&receiver_for_run, &stop_for_run));

        // 等到「接收已经在进行、手机已经暂停」—— 界面上那颗「取消」这时才点得下去
        let ready_by = Instant::now() + Duration::from_secs(5);
        while Instant::now() < ready_by {
            if paused_confirmed.load(Ordering::SeqCst) && crate::control::receiving() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            crate::control::receiving(),
            "接收应当已经开始（否则取消点不下去，这个用例就没意义了）"
        );
        assert!(
            crate::control::paused(),
            "手机上报的暂停必须已经应用到本地"
        );

        // ---- 用户点「取消 · 保留进度」（界面上先记选择、再置取消标志）----
        let started = Instant::now();
        assert!(
            crate::control::cancel_receive(crate::control::CancelChoice::Keep),
            "这一次取消必须被受理"
        );

        // 会话线程必须在有限时间内收场。这里只轮询 -- 不用无超时的 join，
        // 否则这个用例自己也会跟着挂死（真机上挂的就是这条 join）。
        let finished_by = Instant::now() + Duration::from_secs(8);
        while !runner.is_finished() && Instant::now() < finished_by {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            runner.is_finished(),
            "取消路径必须在有限时间内走完：会话线程到现在还挂着（暂停态下就是死等）"
        );
        let total = started.elapsed();
        match runner.join().unwrap() {
            SessionEnd::Shutdown => {}
            SessionEnd::Disconnected(reason) => {
                panic!("取消不该被当成断线（会话要保持连接并重建通道）：{}", reason)
            }
            SessionEnd::Stopped => panic!("unexpected stop"),
        }
        server.join().unwrap();

        // 1) CANCEL 帧写出去了，而且够快：mock 手机暂停 400ms 之后才回来读，
        //    它读到的时刻减去点取消的时刻必须在 1 秒以内（旧代码这里要等十几秒）。
        let seen = (*cancel_seen.lock().unwrap()).expect("CANCEL 帧必须真的写进控制通道");
        let to_frame = seen.saturating_duration_since(started);
        assert!(
            to_frame < Duration::from_secs(1),
            "从点取消到 CANCEL 发出花了 {:?}，必须在 1 秒以内",
            to_frame
        );
        // 2) 取消之后走到了重建阶段（不是挂死、也不是直接断线）
        assert_eq!(
            rebuilt_count.load(Ordering::SeqCst),
            2,
            "取消之后两条传输通道都要重建回来"
        );
        // 3) 整条路径（关通道 + 写 CANCEL + 收尾 + 重建）的总时长
        assert!(
            total < Duration::from_secs(5),
            "整条取消路径用了 {:?}，太久了",
            total
        );
        crate::control::reset();
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

/// 限速值的说法（日志用）：0 写「不限速」，别写成「0 MB/s」—— 那读起来像堵死。
fn limit_text(mb: i32) -> String {
    if mb <= 0 {
        crate::i18n::tr("不限速", "unlimited").to_string()
    } else {
        format!("{mb} MB/s")
    }
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
            // 307：限速。每一块写完之后才睡（和手机端 SendFileCall 里那次调用对称）。
            //
            // 放在 mark_sent **之后**：这一块确实已经出去了，进度就该认它；
            // 放到前面会让「进度」比实际发送慢一拍，限速时那点延迟肉眼可见。
            //
            // 多通道共享同一个全局账本，所以 N 条通道并发跑出来的是**总**速率 = 上限，
            // 不是每条各一份（后者实测限 1MB/s 能跑到 ~60MB/s）。
            crate::ratelimit::throttle(*length as usize);
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