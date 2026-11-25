//! 传输通道直连物理网卡：不让梯子（TUN 模式代理）和虚拟网卡把连接抢走。
//!
//! 问题：Clash/Netch 的 TUN 模式会给 0.0.0.0/0 加一条低度量的路由，Radmin VPN 这类
//! 虚拟网卡更是直接以度量 1 抢默认路由（本机实测：Radmin VPN 度量 1、以太网 25）。
//! 传输通道顺着默认路由走就可能绕进虚拟网卡/代理隧道，速率掉一大截，甚至根本连不上。
//!
//! 做法：connect 之前先把 socket bind 到"和目标地址同网段的物理网卡地址"。
//! Windows 是按绑定源地址选路由的，绑到物理网卡之后这条连接就只走物理网卡，
//! 路由表里虚拟网卡那条默认路由管不着它。
//!
//! 只影响传输通道（手机 ↔ 电脑的 TCP）。版本检查走的还是系统代理，那是另一回事：
//! 它需要过梯子才能访问外网。

use std::io;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

/// 连一条 TCP。能判断出该绑哪张网卡就绑上再连（绕开梯子/虚拟网卡），
/// 判断不出来（回环、跨网段、枚举失败）就退回系统默认路由。
///
/// 回环地址（USB_ADB 走的 adb 转发）不需要绑定：它本来就不过网卡。
pub fn connect(remote: SocketAddr, timeout: Duration, bind_hint: Option<IpAddr>) -> io::Result<TcpStream> {
    #[cfg(windows)]
    {
        match win::connect_bound(remote, timeout, bind_hint) {
            Ok(Some(stream)) => return Ok(stream),
            Ok(None) => {}
            // 判断该绑哪张网卡这一步失败（枚举网卡出错）不该让传输失败，退回默认路由
            Err(e) => crate::log::debug(&format!("netif: 直连物理网卡失败，改用默认路由：{e}")),
        }
    }
    let _ = bind_hint;
    TcpStream::connect_timeout(&remote, timeout)
}

/// 取消时用的「立刻断掉」：设 SO_LINGER=0 再关，close 走 RST 而不是 FIN。
///
/// 为什么需要它：手机端（发送方）在 write 里阻塞着往这条连接灌数据，它看不见
/// 控制通道上的 CANCEL。只发 FIN 是关不掉它的 —— FIN 只表示「我不发了」，
/// 对端照样能继续写，要写到自己的发送缓冲满了才失败（真机实测：PC 端已经取消，
/// 手机还在按满速往一条死连接里灌 4 秒数据，白占带宽、下一单也开不了）。
/// 设了 linger=0 之后 close 立刻发 RST，手机下一次写当场收到 reset、发块线程退出。
///
/// std 的 `TcpStream::set_linger` 到 1.19 还是 unstable，这里直接走 WinSock。
pub fn abort(stream: &TcpStream) {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{setsockopt, LINGER, SO_LINGER, SOL_SOCKET};
        let l = LINGER {
            l_onoff: 1,
            l_linger: 0,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &l as *const LINGER as *const u8,
                std::mem::size_of::<LINGER>(),
            )
        };
        unsafe {
            setsockopt(
                stream.as_raw_socket() as usize,
                SOL_SOCKET,
                SO_LINGER,
                bytes.as_ptr(),
                bytes.len() as i32,
            );
        }
    }
    // 非 Windows 平台没有这个问题（关掉就是关掉），保持原样。
    #[cfg(not(windows))]
    {
        let _ = stream;
    }
}

/// 本机所有可以用来直连的 IPv4 地址（跳过回环、隧道网卡、169.254 自动配置地址）
pub fn local_ipv4() -> Vec<std::net::Ipv4Addr> {
    #[cfg(windows)]
    {
        return win::local_ipv4();
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::os::windows::io::{FromRawSocket, RawSocket};
    use std::ptr;
    use std::sync::Once;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{
        bind, closesocket, connect, getsockopt, ioctlsocket, WSAStartup, WSAGetLastError, WSAPoll,
        WSASocketW, SOCKADDR, WSADATA, WSAPOLLFD, AF_INET, FIONBIO, INVALID_SOCKET, IPPROTO_TCP,
        POLLWRNORM, SOCK_STREAM, SOCKET_ERROR, SOL_SOCKET, SO_ERROR, WSAEWOULDBLOCK,
    };

    /// 网卡不是"运行中"的状态码（IfOperStatusUp == 1）
    const IF_OPER_STATUS_UP: i32 = 1;
    /// ERROR_BUFFER_OVERFLOW：缓冲区不够，按返回的长度重来
    const ERROR_BUFFER_OVERFLOW: u32 = 111;

    static WSA_INIT: Once = Once::new();

    fn ensure_wsa() {
        WSA_INIT.call_once(|| unsafe {
            let mut data: WSADATA = std::mem::zeroed();
            WSAStartup(0x0202, &mut data);
        });
    }

    /// 枚举本机物理网卡的 IPv4 地址。
    ///
    /// 跳过：非"已连接"状态、隧道网卡（IF_TYPE_TUNNEL = 131，TUN 模式的梯子就是它）、
    /// 回环、169.254 自动配置地址。
    pub fn local_ipv4() -> Vec<Ipv4Addr> {
        let mut out = Vec::new();
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size: u32 = 16 * 1024;
        let mut buf: Vec<u64> = Vec::new();
        loop {
            // u64 对齐，保证 IP_ADAPTER_ADDRESSES_LH 的指针是 8 字节对齐的
            buf = vec![0u64; (size as usize + 7) / 8];
            let mut used = size;
            let ret = unsafe {
                GetAdaptersAddresses(
                    AF_INET as u32,
                    flags,
                    ptr::null(),
                    buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                    &mut used,
                )
            };
            if ret == 0 {
                break;
            }
            if ret == ERROR_BUFFER_OVERFLOW {
                size = used;
                continue;
            }
            return out;
        }
        unsafe {
            let mut p = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
            while !p.is_null() {
                let adapter = &*p;
                let is_tunnel = adapter.IfType == 131; // IF_TYPE_TUNNEL
                if adapter.OperStatus == IF_OPER_STATUS_UP && !is_tunnel {
                    let mut u = adapter.FirstUnicastAddress;
                    while !u.is_null() {
                        let ua = &*u;
                        let sa = ua.Address.lpSockaddr;
                        if !sa.is_null() && (*sa).sa_family == AF_INET {
                            let d = &(*sa).sa_data;
                            let ip = Ipv4Addr::new(
                                d[2] as u8,
                                d[3] as u8,
                                d[4] as u8,
                                d[5] as u8,
                            );
                            if !ip.is_loopback() && !ip.is_unspecified() && !is_link_local(ip) {
                                out.push(ip);
                            }
                        }
                        u = ua.Next;
                    }
                }
                p = adapter.Next;
            }
        }
        out
    }

    fn is_link_local(ip: Ipv4Addr) -> bool {
        let o = ip.octets();
        o[0] == 169 && o[1] == 254
    }

    /// 挑一个"和目标同网段"的本机地址：先比 /24（局域网基本都够），再比 /16。
    fn pick_local(remote: Ipv4Addr) -> Option<Ipv4Addr> {
        let r = remote.octets();
        let addrs = local_ipv4();
        addrs
            .iter()
            .copied()
            .find(|ip| {
                let o = ip.octets();
                o[0] == r[0] && o[1] == r[1] && o[2] == r[2]
            })
            .or_else(|| {
                addrs.iter().copied().find(|ip| {
                    let o = ip.octets();
                    o[0] == r[0] && o[1] == r[1]
                })
            })
    }

    fn sockaddr(port: u16, ip: Ipv4Addr) -> SOCKADDR {
        let mut sa: SOCKADDR = unsafe { std::mem::zeroed() };
        sa.sa_family = AF_INET;
        let p = port.to_be_bytes();
        let o = ip.octets();
        sa.sa_data[0] = p[0] as i8;
        sa.sa_data[1] = p[1] as i8;
        sa.sa_data[2] = o[0] as i8;
        sa.sa_data[3] = o[1] as i8;
        sa.sa_data[4] = o[2] as i8;
        sa.sa_data[5] = o[3] as i8;
        sa
    }

    fn connect_from(local: Ipv4Addr, remote: SocketAddrV4, timeout: Duration) -> io::Result<TcpStream> {
        ensure_wsa();
        unsafe {
            let s = WSASocketW(AF_INET as i32, SOCK_STREAM, IPPROTO_TCP, ptr::null(), 0, 0);
            if s == INVALID_SOCKET {
                return Err(io::Error::last_os_error());
            }
            let fail = |err: io::Error| -> io::Result<TcpStream> {
                closesocket(s);
                Err(err)
            };
            let sa = sockaddr(0, local);
            if bind(s, &sa as *const SOCKADDR, std::mem::size_of::<SOCKADDR>() as i32)
                == SOCKET_ERROR
            {
                return fail(io::Error::last_os_error());
            }
            // 非阻塞 connect + WSAPoll 等可写：这样能自己控超时
            let mut nonblock: u32 = 1;
            ioctlsocket(s, FIONBIO, &mut nonblock);
            let dst = sockaddr(remote.port(), *remote.ip());
            let rc = connect(s, &dst as *const SOCKADDR, std::mem::size_of::<SOCKADDR>() as i32);
            if rc == SOCKET_ERROR {
                let code = WSAGetLastError();
                if code != WSAEWOULDBLOCK {
                    return fail(io::Error::from_raw_os_error(code));
                }
                let mut fds = [WSAPOLLFD { fd: s, events: POLLWRNORM, revents: 0 }];
                let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
                let polled = WSAPoll(fds.as_mut_ptr(), 1, ms);
                if polled == 0 {
                    return fail(io::Error::new(io::ErrorKind::TimedOut, "connect timed out"));
                }
                if polled < 0 {
                    return fail(io::Error::last_os_error());
                }
                let mut err: i32 = 0;
                let mut len: i32 = std::mem::size_of::<i32>() as i32;
                getsockopt(
                    s,
                    SOL_SOCKET,
                    SO_ERROR,
                    &mut err as *mut i32 as *mut u8,
                    &mut len,
                );
                if err != 0 {
                    return fail(io::Error::from_raw_os_error(err));
                }
            }
            // 交回 std 之前恢复阻塞模式
            let mut block: u32 = 0;
            ioctlsocket(s, FIONBIO, &mut block);
            Ok(TcpStream::from_raw_socket(s as RawSocket))
        }
    }

    /// Ok(Some(stream)) = 已绑定物理网卡连上了；Ok(None) = 不该/不用绑，交给默认路由
    pub fn connect_bound(
        remote: SocketAddr,
        timeout: Duration,
        bind_hint: Option<IpAddr>,
    ) -> io::Result<Option<TcpStream>> {
        if remote.is_ipv6() || remote.ip().is_loopback() {
            return Ok(None);
        }
        let target = match remote {
            SocketAddr::V4(v4) => v4,
            _ => return Ok(None),
        };
        let local = match bind_hint {
            Some(IpAddr::V4(ip)) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
            _ => pick_local(*target.ip()),
        };
        let local = match local {
            Some(ip) => ip,
            None => {
                // 留着这条日志：排查「某张网卡的通道没绑定源地址」时要看候选列表
                crate::log::debug(&format!(
                    "netif: 找不到与 {} 同网段的网卡地址，候选 {:?}",
                    target,
                    local_ipv4()
                ));
                return Ok(None);
            }
        };
        let stream = connect_from(local, target, timeout)?;
        crate::log::debug(&format!(
            "channel -> {} 绑定本机 {}（直连物理网卡，绕开梯子/虚拟网卡）",
            remote, local
        ));
        Ok(Some(stream))
    }
}
