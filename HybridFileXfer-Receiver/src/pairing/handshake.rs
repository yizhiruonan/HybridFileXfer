//! PAIR 状态机（电脑侧）：认证与配对**两条时序分开**（文档 §5.14）。
//!
//! ## 为什么两台设备的形状不一样（这点必须写清，否则实现会互相抄错）
//!
//! 手机侧受"accept 只有一根线程"限制，配对时**必须**把 socket 交给 PendingPairing
//! 立刻回 accept 循环，否则一台电脑来配对，全世界的电脑都进不来 180 秒。
//! 电脑侧没有这个约束：引擎线程本来就是每台设备一条（engine.rs），没有 accept
//! 循环要守。所以**电脑侧不需要 PairWorker** —— 就在引擎线程里阻塞等待，最多 180s，
//! 界面上的"等待手机确认"由既有状态机推给 UI 即可。
//!
//! ## 两条时序的差别只有"预算"与"门禁"
//!
//! - **认证**（已配对重连）：HELLO -> REPLY -> CONFIRM <-> CONFIRM，全程 2s 级，0 次人工交互；
//! - **配对**（首次）：前面完全一样，只是 CONFIRM 那一步要等用户在两块屏幕上比对 6 位数，
//!   预算换成 180s（§5.14.3），并且只有用户显式发起（Mode::RequestPairing）才会走到
//!   这里 —— 自动/无线路径拿到的未配对设备一律 PE_UNKNOWN_DEVICE 拒绝。
//!
//! ## 两个不显眼但致命的细节
//!
//! 1. **双端都"先写后读"**。每条 CONFIRM 只有不超过 83 字节，任何 socket 缓冲都装得下，
//!    所以先写不会阻塞；反过来若某一端"先读后写"，两端就会互等，直到超时。
//! 2. **签名覆盖转录 T，用的是上线字节**（IS-7/IS-8）。所以 Hello::body() 的结果在发出
//!    之前就存好，解析侧则直接用 Raw::body，两边都不重新序列化。

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use ring::rand::{SecureRandom, SystemRandom};

use super::ecdh::{self, PUBKEY_LEN};
use super::identity::{self, Identity, FINGERPRINT_LEN};
use super::kdf::{self, DerivedKeys};
use super::message::{
    self, Confirm, Hello, Incoming, Reply, ERR_NONE, FLAG_REQUEST_PAIRING, NAME_MAX, NONCE_LEN,
};
use super::trust::TrustStore;
use super::{PairError, PairResult};

/// §5.14.3 的五种超时，**全部做成数据**，而不是散落在代码里的常量。
///
/// 为什么参数化：180s 的配对待确认在单元测试里不可能真等，而"代码看起来对"正是这类
/// 状态机最容易骗过评审的地方。做成结构体之后，测试传几十毫秒就能真跑一遍超时路径，
/// 生产值由 Default 给出，两者不会漂移。
///
/// 与文档 §5.14.3 那张表的对应关系（表里 3 行，这里 5 项，多出来的两项是被拆开的两步）：
///
/// | 本字段 | §5.14.3 | 生产值 | 说明 |
/// |---|---|---|---|
/// | version | 版本交换 | 15s | 本层不发版本帧，给调用方用；放在同一个结构体里，避免两处各写一个 15 |
/// | auth_reply | A1（HELLO -> REPLY） | 2s | 局域网实测 <10ms，留 200 倍余量 |
/// | auth_confirm | A2（验签 + CONFIRM） | 2s | 单独一项才能区分"对端不答 HELLO"和"对端不确认" |
/// | pair_pending | P1~P2 配对待确认 | 180s | 用户要从手机走到电脑前、读 6 个数、两边各点一次 |
/// | abort_drain | （无） | 500ms | 放弃时尽力把 ABORT 送出去；不许为"通知对方"再等下去 |
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    pub version: Duration,
    pub auth_reply: Duration,
    pub auth_confirm: Duration,
    pub pair_pending: Duration,
    pub abort_drain: Duration,
}

impl Default for Timeouts {
    fn default() -> Timeouts {
        Timeouts {
            version: Duration::from_secs(15),
            auth_reply: Duration::from_secs(2),
            auth_confirm: Duration::from_secs(2),
            pair_pending: Duration::from_secs(180),
            abort_drain: Duration::from_millis(500),
        }
    }
}

/// 这条连接该怎么对待"不认识的设备"。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 只接受已配对设备：对端指纹不在信任表里就直接 PE_UNKNOWN_DEVICE，**不弹窗**。
    /// 自动重试、无线回退、发现流程都走这个（§5.18.2 门禁）。
    ConnectOnly,
    /// 用户显式发起（界面「配对手机」/ CLI --pair）：允许 180s 的人工确认。
    RequestPairing,
}

impl Mode {
    /// PAIR_HELLO.flags 的 bit0。
    fn flags(self) -> u16 {
        match self {
            Mode::ConnectOnly => 0,
            Mode::RequestPairing => FLAG_REQUEST_PAIRING,
        }
    }

    fn wants_pairing(self) -> bool {
        self == Mode::RequestPairing
    }
}

/// 一次成功握手的结果。
pub struct Established {
    /// 整棵密钥树（含 PRK、控制通道双向密钥、nonce 前缀、6 位配对码）。
    pub keys: DerivedKeys,
    /// 对端长期身份公钥（65 字节）。
    pub peer_public: [u8; PUBKEY_LEN],
    /// 对端身份指纹（信任表的键）。
    pub peer_fingerprint: [u8; FINGERPRINT_LEN],
    /// 对端显示名（**只用于展示**：它下次连接时可以变，不能当身份）。
    pub peer_name: String,
    /// 对端本次的 ephemeral 公钥（调试/日志用）。
    pub peer_ephemeral: [u8; PUBKEY_LEN],
    /// 本次是不是"首次配对"（= 对端原本不在信任表里）。
    ///
    /// 调用方只有在这为 true 时才写信任表：握手本身绝不写盘（§5.14.3——
    /// 配对失败/超时不许留下"半配对"）。
    pub newly_paired: bool,
}

impl Established {
    /// 给界面的一句话（配对成功后弹的那条）。
    pub fn summary(&self) -> String {
        if self.newly_paired {
            format!(
                "已与「{}」配对（指纹 {}）",
                self.peer_name,
                identity::fingerprint_text(&self.peer_fingerprint)
            )
        } else {
            format!("已连接「{}」", self.peer_name)
        }
    }
}

/// 手写 Debug，**不 derive**。
///
/// 这个结构体里裹着整棵密钥树（PRK、控制通道双向密钥、nonce 前缀）。derive 之后
/// 任何一个 `{:?}` 打日志的地方都会把会话密钥写进日志文件 —— 而日志是会被
/// 贴进 issue、发到群里的东西。所以只打"可以给人看"的那几项。
impl std::fmt::Debug for Established {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Established")
            .field("peer_name", &self.peer_name)
            .field("peer_fingerprint", &identity::fingerprint_text(&self.peer_fingerprint))
            .field("newly_paired", &self.newly_paired)
            .field("keys", &"<密钥不打印>")
            .finish()
    }
}

/// 握手需要的 socket 能力：既读又写，还要能在中途改超时。
///
/// 单独抽一个 trait 而不是直接写 TcpStream：配对那一步要把读超时从 2s 临时放宽到
/// 180s（§5.14.2 末条），所以状态机必须能在流程中间改超时；测试也要能塞一个假的。
pub trait Handshaker: Read + Write {
    /// 同时设置读/写超时。
    ///
    /// 读和写用同一个值是有意的：握手期每条消息都很小（最大 369 字节），
    /// "写不完"只可能是对端不读，和"读不到"是同一类问题，没必要给两个预算。
    fn set_rw_timeout(&mut self, timeout: Duration) -> io::Result<()>;

    /// 失败之后、关连接之前的"礼让"：半关写方向，再把对端还可能发来的字节读掉。
    ///
    /// 为什么必须做（这是一个真实被抓到的竞态）：本端如果**接收缓冲里还有没读的字节**
    /// 就 close，内核会发 RST；而 RST 会让刚写出去的那条 PAIR_ABORT 在对端那边被丢弃。
    /// 表现出来是"手机只说了连接被重置"，而不是我们想告诉它的"未配对，已拒绝"，
    /// 于是它会按断线处理、3 秒后自动重连 —— 两个设备反复连反复断。
    ///
    /// 默认实现什么都不做：内存里的假 socket 没有这个语义。
    fn drain_before_close(&mut self) {}
}

impl Handshaker for TcpStream {
    fn set_rw_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(timeout))?;
        self.set_write_timeout(Some(timeout))
    }

    fn drain_before_close(&mut self) {
        // 先半关写方向：ABORT 已经 flush 过，这样对端读到的是 ABORT 再 EOF（正常的 FIN），
        // 不会被 RST 打断。
        let _ = self.shutdown(std::net::Shutdown::Write);
        let _ = self.set_read_timeout(Some(Duration::from_millis(60)));
        let deadline = std::time::Instant::now() + Duration::from_millis(60);
        let mut buf = [0u8; 256];
        // 读掉对端已经发出的字节（通常是它那张我们不再需要的 CONFIRM）。
        // 60ms 是上限不是期望值：正常情况一次 read 就返回 0（EOF）立刻退出。
        while std::time::Instant::now() < deadline {
            match self.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
    }
}

/// 电脑侧握手（认证 + 首次配对同一入口，靠 Mode 区分）。
///
/// 失败时**尽力**把 PAIR_ABORT 送给对端再返回错误：手机需要知道"是它被拒了"而不是
/// "网络断了"——否则它会按断线处理、3 秒后自动重连，用户看到两台设备反复连反复断。
pub fn pc_handshake<H: Handshaker>(
    io: &mut H,
    identity: &Identity,
    trust: &TrustStore,
    mode: Mode,
    timeouts: &Timeouts,
    rng: &SystemRandom,
) -> PairResult<Established> {
    match run(io, identity, trust, mode, timeouts, rng) {
        Ok(done) => Ok(done),
        Err(err) => {
            // 顺序不能反：先尽力把 ABORT 送出去，再礼让关连接。反了就是"先 RST 后送信"。
            let _ = send_abort(io, err, timeouts);
            io.drain_before_close();
            Err(err)
        }
    }
}

fn run<H: Handshaker>(
    io: &mut H,
    identity: &Identity,
    trust: &TrustStore,
    mode: Mode,
    timeouts: &Timeouts,
    rng: &SystemRandom,
) -> PairResult<Established> {
    // ---- 第一步：HELLO（A1 上半段） ----
    let (eph_private, eph_public) = ecdh::generate(rng)?;
    let hello = Hello {
        flags: mode.flags(),
        eph_pub: eph_public,
        stat_pub: *identity.public(),
        nonce: random_nonce(rng)?,
        name: local_name(),
    };
    // 上线字节先算一次并留着：签名要覆盖它（IS-7），事后重新序列化就有出错的机会。
    let hello_body = hello.body()?;
    io.set_rw_timeout(timeouts.auth_reply)?;
    io.write_all(&hello.encode()?)?;
    io.flush()?;

    // ---- 第二步：REPLY（A1 下半段） ----
    let reply = match next_message(io)? {
        Incoming::Reply(raw) => raw,
        _ => return Err(PairError::BadRecord),
    };
    if reply.value.err != ERR_NONE {
        // 手机明确说了不继续（例如它已经有一个 PendingPairing，回 PE_BUSY）：
        // 此时它不会再看我们后面发的任何东西，直接按它给的码收尾。
        return Err(PairError::from_code(reply.value.err));
    }

    // ---- 第三步：ECDH + 转录 + 派生（全是本地计算，不占超时预算） ----
    //
    // 先做 ECDH 再做验签：ecdh::agree 会拒掉"不在曲线上的公钥"，这比一次验签便宜，
    // 而且能把"对端给了一个垃圾公钥"和"签名不对"分开——前者说明对面根本不是这套协议。
    let z = ecdh::agree(eph_private, &reply.value.eph_pub)?;
    let transcript = kdf::transcript(&hello_body, &reply.body);
    let keys = kdf::derive(&z, &transcript)?;

    let peer_public = reply.value.stat_pub;
    let peer_fingerprint = identity::fingerprint(&peer_public)?;
    let trusted = trust.is_trusted(&peer_fingerprint);

    // ---- 门禁：不认识的设备，且这次不是用户发起的配对 ----
    //
    // 这一条挡的是"自动重连顺手把一个陌生设备配进来"：无线回退/发现流程里用户不在场，
    // 不能弹窗，所以只能拒。拒完调用方还要停止本轮自动重试（§5.18.2 门禁 2）。
    if !trusted && !mode.wants_pairing() {
        return Err(PairError::UnknownDevice);
    }

    // ---- 第四步：CONFIRM 互换（A2 / P2） ----
    //
    // 预算在这里换挡：已配对 = 2s；首次配对 = 180s（等用户比对 6 位数）。
    let wait = if trusted { timeouts.auth_confirm } else { timeouts.pair_pending };
    io.set_rw_timeout(wait)?;

    // 自己的签名先发出去，再把读留给对端（双端都"先写后读"，见文件头第 1 条）。
    let my_signature = identity.sign(&transcript, rng)?;
    io.write_all(&Confirm { sig: my_signature, err: ERR_NONE }.encode()?)?;
    io.flush()?;

    let confirm = match next_message(io)? {
        Incoming::Confirm(raw) => raw,
        _ => return Err(PairError::BadRecord),
    };
    if confirm.value.err != ERR_NONE {
        // 用户拒绝（2）、码不一致（5）等：这是**对端**的结论，原样上抛。
        return Err(PairError::from_code(confirm.value.err));
    }
    // 关键一步（IS-8）：用对端**长期**公钥验它对 T 的签名。
    // 不验这一步，整个握手就退化成了"对面随便给了个公钥"，配对等于没做。
    identity::verify(&peer_public, &transcript, &confirm.value.sig)?;

    Ok(Established {
        keys,
        peer_public,
        peer_fingerprint,
        peer_name: reply.value.name.clone(),
        peer_ephemeral: reply.value.eph_pub,
        newly_paired: !trusted,
    })
}

/// 发一条 ABORT（尽力而为）。
fn send_abort<H: Handshaker>(io: &mut H, err: PairError, timeouts: &Timeouts) -> io::Result<()> {
    // 用一个独立的小预算：这里再等下去只会让"已经失败"变成"卡住 2 秒才失败"。
    io.set_rw_timeout(timeouts.abort_drain)?;
    io.write_all(&message::encode_abort(err))?;
    io.flush()
}

/// 读一条消息，并把对端的 PAIR_ABORT 直接翻译成错误。
///
/// 单独一层的价值：后续每一步的 match 都不必再写一遍 Abort 分支 ——
/// 少一个能被忘掉的分支，就少一次"对端已经放弃而我们还在等下一个字段"。
fn next_message<H: Read>(io: &mut H) -> PairResult<Incoming> {
    let incoming = message::read_message(io)?;
    if let Some(err) = incoming.abort_err() {
        return Err(err);
    }
    Ok(incoming)
}

fn random_nonce(rng: &SystemRandom) -> PairResult<[u8; NONCE_LEN]> {
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut nonce).map_err(|_| PairError::Io)?;
    Ok(nonce)
}

/// 本机显示名（主机名）。**只用于展示**，所以超长可以截断；
/// 协议字段一律不许截断（那是"两边看起来不一样而没人报错"的来源）。
fn local_name() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "HybridFileXfer-PC".to_string());
    let mut out = String::new();
    for ch in raw.chars() {
        if out.len() + ch.len_utf8() > NAME_MAX {
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;
    use std::time::Instant;

    /// 测试用的超时：全在几十~几百毫秒量级，这样"真跑一遍超时路径"是可行的。
    fn fast_timeouts() -> Timeouts {
        Timeouts {
            version: Duration::from_millis(100),
            auth_reply: Duration::from_millis(120),
            auth_confirm: Duration::from_millis(120),
            pair_pending: Duration::from_millis(400),
            abort_drain: Duration::from_millis(200),
        }
    }

    /// 手机侧要扮演成什么样子。
    #[derive(Clone, Copy)]
    enum Behaviour {
        /// 正常：收到 HELLO 后立刻回 REPLY，等指定时长再发 CONFIRM。
        Normal { confirm_after: Duration },
        /// 回了 REPLY 之后再不发 CONFIRM（用户走开 / 手机卡死）。
        NeverConfirm { hold: Duration },
        /// 连 REPLY 都不发，只是占着连接（对端不吭声）。
        Silent { hold: Duration },
        /// CONFIRM 里签的是被改过一个字节的 T。
        BadSignature,
        /// 用 ABORT 代替 REPLY。
        AbortInsteadOfReply(PairError),
        /// REPLY 里带上错误码（例如已经有 PendingPairing 回 PE_BUSY）。
        ReplyErr(u8),
        /// 用 HELLO 代替 REPLY（协议类型错位）。
        HelloInsteadOfReply,
        /// REPLY 的 proto 字段写成 2（版本不一致）。
        BadProto,
    }

    /// 手机侧跑完一轮之后交回的结论，测试用它断言"两边真的谈成了"。
    struct PhoneReport {
        /// 手机侧独立算出的密钥树（与电脑侧比对）。
        keys: Option<DerivedKeys>,
        /// 手机侧验证电脑签名是否通过（None = 没走到这一步）。
        pc_signature: Option<bool>,
        /// 手机侧是否收到了电脑的 ABORT。
        saw_abort: Option<PairError>,
    }

    /// 起一个"手机"：监听回环端口，把端口号交回来，然后把连接交给脚本。
    ///
    /// 用真实 TCP 而不是内存管道，是因为这里要测的正是 set_read_timeout 的行为、
    /// 超时的错误种类、以及"先写后读不会互等"——内存管道测不出这些。
    fn spawn_phone(
        phone_identity: Identity,
        phone_name: &'static str,
        flags: u16,
        behaviour: Behaviour,
    ) -> (u16, thread::JoinHandle<PhoneReport>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut sock, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => {
                    return PhoneReport { keys: None, pc_signature: None, saw_abort: None };
                }
            };
            phone_script(&mut sock, phone_identity, phone_name, flags, behaviour)
        });
        (port, handle)
    }

    fn phone_script(
        sock: &mut TcpStream,
        identity: Identity,
        name: &'static str,
        flags: u16,
        behaviour: Behaviour,
    ) -> PhoneReport {
        let rng = SystemRandom::new();
        let mut report = PhoneReport { keys: None, pc_signature: None, saw_abort: None };

        // ---- 读 HELLO ----
        let hello = match message::read_message(sock) {
            Ok(Incoming::Hello(raw)) => raw,
            Ok(Incoming::Abort(code)) => {
                report.saw_abort = Some(PairError::from_code(code));
                return report;
            }
            _ => return report,
        };

        if let Behaviour::AbortInsteadOfReply(err) = behaviour {
            let _ = sock.write_all(&message::encode_abort(err));
            return report;
        }
        if let Behaviour::Silent { hold } = behaviour {
            // 占着连接不吭声：电脑应当在 auth_reply 预算内报 Timeout。
            thread::sleep(hold);
            return report;
        }

        // ---- 造 REPLY ----
        let (eph_private, eph_public) = match ecdh::generate(&rng) {
            Ok(v) => v,
            Err(_) => return report,
        };
        let reply = Reply {
            flags,
            eph_pub: eph_public,
            stat_pub: *identity.public(),
            nonce: [0x5au8; NONCE_LEN],
            name: name.to_string(),
            err: match behaviour {
                Behaviour::ReplyErr(code) => code,
                _ => ERR_NONE,
            },
        };
        let mut wire = match reply.encode() {
            Ok(v) => v,
            Err(_) => return report,
        };
        if matches!(behaviour, Behaviour::BadProto) {
            // body 的头两个字节就是 proto：直接改成 2，模拟"两代实现混用"。
            wire[1] = 0x00;
            wire[2] = 0x02;
        }
        if let Behaviour::HelloInsteadOfReply = behaviour {
            // 类型错位：发一条 HELLO 过去，电脑必须报 BadRecord 而不是当成 REPLY 解。
            let wrong = Hello {
                flags: 0,
                eph_pub: eph_public,
                stat_pub: *identity.public(),
                nonce: [0x11u8; NONCE_LEN],
                name: "confused".to_string(),
            };
            let _ = sock.write_all(&wrong.encode().unwrap());
            return report;
        }
        if sock.write_all(&wire).is_err() {
            return report;
        }
        if reply.err != ERR_NONE {
            // 已经明确拒绝，后面不会有 CONFIRM。
            return report;
        }

        // ---- 算 Z 与 T（手机侧独立算一遍，用来和电脑侧比对） ----
        let z = match ecdh::agree(eph_private, &hello.value.eph_pub) {
            Ok(z) => z,
            Err(_) => return report,
        };
        let body = match reply.body() {
            Ok(body) => body,
            Err(_) => return report,
        };
        let transcript = kdf::transcript(&hello.body, &body);
        report.keys = kdf::derive(&z, &transcript).ok();

        // ---- 用户确认（配对时才有人等） ----
        match behaviour {
            Behaviour::NeverConfirm { hold } => {
                thread::sleep(hold);
                return report;
            }
            Behaviour::Normal { confirm_after } => thread::sleep(confirm_after),
            _ => {}
        }

        // ---- 发 CONFIRM（可能故意签错） ----
        let signed_message = if matches!(behaviour, Behaviour::BadSignature) {
            let mut bad = transcript.clone();
            bad[0] ^= 0x80;
            bad
        } else {
            transcript.clone()
        };
        let sig = match identity.sign(&signed_message, &rng) {
            Ok(sig) => sig,
            Err(_) => return report,
        };
        // 写失败**不**直接 return：电脑在"拒绝"路径上会先发 ABORT 再关连接，
        // 我们这张 CONFIRM 撞在它的关闭窗口上是很正常的。此时仍然要读一次，
        // 否则这条用例会变成"看两边谁先跑到"的竞态（全量跑真的红过一次）。
        let _ = sock.write_all(&Confirm { sig, err: ERR_NONE }.encode().unwrap());
        // 读之前给一个短超时：对端异常时这线程不许挂到测试超时。
        let _ = sock.set_read_timeout(Some(Duration::from_millis(400)));

        // ---- 读电脑的 CONFIRM 并验签 ----
        match message::read_message(sock) {
            Ok(Incoming::Confirm(raw)) => {
                let ok = identity::verify(&hello.value.stat_pub, &transcript, &raw.value.sig).is_ok();
                report.pc_signature = Some(ok);
            }
            Ok(Incoming::Abort(code)) => {
                report.saw_abort = Some(PairError::from_code(code));
            }
            _ => {}
        }
        report
    }

    /// 电脑侧握手（连回环端口）。
    fn pc_connect_and_handshake(
        port: u16,
        identity: &Identity,
        trust: &TrustStore,
        mode: Mode,
        timeouts: &Timeouts,
    ) -> PairResult<Established> {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let rng = SystemRandom::new();
        pc_handshake(&mut sock, identity, trust, mode, timeouts, &rng)
    }

    /// 两台设备的身份 + 一张（可能空的）信任表。
    fn setup(trusted: bool) -> (Identity, Identity, TrustStore) {
        let rng = SystemRandom::new();
        let (pc, _) = Identity::generate(&rng).unwrap();
        let (phone, _) = Identity::generate(&rng).unwrap();
        let mut trust = TrustStore::empty();
        if trusted {
            trust.add(*phone.fingerprint(), "Pixel-Test", 1_700_000_000);
        }
        (pc, phone, trust)
    }

    /// 认证路径（已配对重连）：0 次交互、2s 级预算，两边密钥一致、签名互验通过。
    #[test]
    fn auth_success_with_trusted_device() {
        let (pc, phone, trust) = setup(true);
        let (port, handle) = spawn_phone(
            phone,
            "Pixel-Test",
            0,
            Behaviour::Normal { confirm_after: Duration::ZERO },
        );
        let started = Instant::now();
        let done =
            pc_connect_and_handshake(port, &pc, &trust, Mode::ConnectOnly, &fast_timeouts()).unwrap();
        let report = handle.join().unwrap();

        assert!(!done.newly_paired, "已配对设备不该被当成首次配对");
        assert_eq!(done.peer_name, "Pixel-Test");
        assert_eq!(done.peer_public[0], 0x04);
        let phone_keys = report.keys.expect("手机侧没算出密钥");
        assert_eq!(done.keys.c2s, phone_keys.c2s, "电脑到手机方向密钥两端不一致");
        assert_eq!(done.keys.s2c, phone_keys.s2c, "手机到电脑方向密钥两端不一致");
        assert_eq!(done.keys.sas_code, phone_keys.sas_code, "6 位配对码两端不一致");
        assert_eq!(report.pc_signature, Some(true), "手机侧验电脑签名失败");
        assert!(started.elapsed() < Duration::from_secs(2), "认证路径不该这么慢");
        assert!(done.summary().contains("Pixel-Test"));
    }

    /// 配对路径（首次）：手机要人工确认，电脑按 pair_pending 的预算等。
    #[test]
    fn pairing_success_waits_for_user() {
        let (pc, phone, trust) = setup(false);
        let (port, handle) = spawn_phone(
            phone,
            "Pixel-Test",
            FLAG_REQUEST_PAIRING,
            Behaviour::Normal { confirm_after: Duration::from_millis(80) },
        );
        let done =
            pc_connect_and_handshake(port, &pc, &trust, Mode::RequestPairing, &fast_timeouts())
                .unwrap();
        let report = handle.join().unwrap();

        assert!(done.newly_paired, "首次配对必须标成 newly_paired，调用方才会写信任表");
        assert_eq!(done.keys.sas_code.len(), 6);
        assert_eq!(report.keys.unwrap().sas_code, done.keys.sas_code);
        assert_eq!(report.pc_signature, Some(true));
        // 握手本身绝不写盘：信任表还是空的，写与不写由调用方决定。
        assert!(trust.is_empty());
    }

    /// 只重连 + 陌生设备：必须拒，并如实通知对端（这样它不会 3 秒后自动重连）。
    #[test]
    fn connect_only_rejects_unknown_device() {
        let (pc, phone, trust) = setup(false);
        let (port, handle) = spawn_phone(
            phone,
            "Stranger",
            0,
            Behaviour::Normal { confirm_after: Duration::ZERO },
        );
        let err = pc_connect_and_handshake(port, &pc, &trust, Mode::ConnectOnly, &fast_timeouts())
            .unwrap_err();
        assert_eq!(err, PairError::UnknownDevice);
        let report = handle.join().unwrap();
        assert_eq!(report.saw_abort, Some(PairError::UnknownDevice), "对端没被告知拒绝原因");
    }

    /// 超时 1：对端连 REPLY 都不发 → auth_reply 预算内报 Timeout。
    #[test]
    fn silent_phone_times_out() {
        let (pc, phone, trust) = setup(true);
        let (port, handle) = spawn_phone(
            phone,
            "Pixel-Test",
            0,
            Behaviour::Silent { hold: Duration::from_millis(600) },
        );
        let started = Instant::now();
        let err = pc_connect_and_handshake(port, &pc, &trust, Mode::ConnectOnly, &fast_timeouts())
            .unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(err, PairError::Timeout);
        assert!(elapsed < Duration::from_millis(600), "没按 auth_reply 预算退出：{:?}", elapsed);
        let _ = handle.join();
    }

    /// 超时 2：回了 REPLY 但从不发 CONFIRM → auth_confirm 预算内报 Timeout。
    #[test]
    fn phone_never_confirms_times_out() {
        let (pc, phone, trust) = setup(true);
        let (port, handle) = spawn_phone(
            phone,
            "Pixel-Test",
            0,
            Behaviour::NeverConfirm { hold: Duration::from_millis(600) },
        );
        let started = Instant::now();
        let err = pc_connect_and_handshake(port, &pc, &trust, Mode::ConnectOnly, &fast_timeouts())
            .unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(err, PairError::Timeout);
        assert!(elapsed < Duration::from_millis(600), "没按 auth_confirm 预算退出：{:?}", elapsed);
        let _ = handle.join();
    }

    /// 超时 3：配对时用户一直不点「一致」 → 走的是 pair_pending（180s）那个预算，
    /// 而不是认证的 2s。这条用"耗时明显超过 auth_confirm"来证明 —— 两个预算写反了
    /// 这条才会红。
    #[test]
    fn pairing_uses_pair_pending_budget() {
        let (pc, phone, trust) = setup(false);
        let (port, handle) = spawn_phone(
            phone,
            "Pixel-Test",
            FLAG_REQUEST_PAIRING,
            Behaviour::NeverConfirm { hold: Duration::from_millis(900) },
        );
        let timeouts = Timeouts { auth_confirm: Duration::from_millis(60), ..fast_timeouts() };
        let started = Instant::now();
        let err = pc_connect_and_handshake(port, &pc, &trust, Mode::RequestPairing, &timeouts)
            .unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(err, PairError::Timeout);
        assert!(
            elapsed >= Duration::from_millis(300),
            "配对用的是认证预算（{:?}），§5.14.3 的 180s 没生效",
            elapsed
        );
        let _ = handle.join();
    }

    /// 默认值与文档 §5.14.3 一致（生产值不许在别处再写一遍）。
    #[test]
    fn default_timeouts_match_doc() {
        let t = Timeouts::default();
        assert_eq!(t.auth_reply, Duration::from_secs(2));
        assert_eq!(t.auth_confirm, Duration::from_secs(2));
        assert_eq!(t.pair_pending, Duration::from_secs(180));
        assert_eq!(t.version, Duration::from_secs(15));
        assert!(t.abort_drain <= Duration::from_secs(1));
    }

    /// IS-8：对端签名没覆盖 T 时必须拒（这里让手机签一个改过一字节的 T）。
    #[test]
    fn bad_signature_is_rejected() {
        let (pc, phone, trust) = setup(true);
        let (port, handle) = spawn_phone(phone, "Pixel-Test", 0, Behaviour::BadSignature);
        let err = pc_connect_and_handshake(port, &pc, &trust, Mode::ConnectOnly, &fast_timeouts())
            .unwrap_err();
        assert_eq!(err, PairError::BadSignature);
        let _ = handle.join();
    }

    /// 对端用 ABORT 报的错要原样上抛（用户点拒绝 = 2）。
    #[test]
    fn peer_abort_user_declined() {
        let (pc, phone, trust) = setup(true);
        let (port, handle) = spawn_phone(
            phone,
            "Pixel-Test",
            0,
            Behaviour::AbortInsteadOfReply(PairError::UserDeclined),
        );
        let err = pc_connect_and_handshake(port, &pc, &trust, Mode::ConnectOnly, &fast_timeouts())
            .unwrap_err();
        assert_eq!(err, PairError::UserDeclined);
        let _ = handle.join();
    }

    /// 失败后必须主动发 ABORT，别让对端傻等（§5.14.3 的"如实通知"）。
    #[test]
    fn failure_sends_abort_to_peer() {
        let (pc, phone, trust) = setup(true);
        let (port, handle) = spawn_phone(
            phone,
            "Pixel-Test",
            0,
            Behaviour::AbortInsteadOfReply(PairError::BadSignature),
        );
        let err = pc_connect_and_handshake(port, &pc, &trust, Mode::ConnectOnly, &fast_timeouts())
            .unwrap_err();
        assert_eq!(err, PairError::BadSignature);
        let _ = handle.join();
    }

    /// REPLY 里的 err 要上抛（7 = 尝试次数过多 / 手机那边已经有配对在等）。
    #[test]
    fn reply_error_is_propagated() {
        let (pc, phone, trust) = setup(true);
        let (port, handle) = spawn_phone(phone, "Pixel-Test", 0, Behaviour::ReplyErr(7));
        let err = pc_connect_and_handshake(port, &pc, &trust, Mode::ConnectOnly, &fast_timeouts())
            .unwrap_err();
        assert_eq!(err, PairError::TooManyAttempts);
        let _ = handle.join();
    }

    /// 类型错位（该来 REPLY 却来了 HELLO）必须当协议错误拒掉。
    #[test]
    fn wrong_message_type_is_rejected() {
        let (pc, phone, trust) = setup(true);
        let (port, handle) = spawn_phone(phone, "Pixel-Test", 0, Behaviour::HelloInsteadOfReply);
        let err = pc_connect_and_handshake(port, &pc, &trust, Mode::ConnectOnly, &fast_timeouts())
            .unwrap_err();
        assert_eq!(err, PairError::BadRecord);
        let _ = handle.join();
    }

    /// 配对消息版本不一致 → Version（文案是"两端都要升级"，不是"被篡改"）。
    #[test]
    fn pair_proto_mismatch_is_version_error() {
        let (pc, phone, trust) = setup(true);
        let (port, handle) = spawn_phone(phone, "Pixel-Test", 0, Behaviour::BadProto);
        let err = pc_connect_and_handshake(port, &pc, &trust, Mode::ConnectOnly, &fast_timeouts())
            .unwrap_err();
        assert_eq!(err, PairError::Version);
        let _ = handle.join();
    }

    /// flags 的 bit0 必须如实反映模式：门禁靠它区分"用户在场"和"自动重连"。
    #[test]
    fn mode_maps_to_flags() {
        assert_eq!(Mode::ConnectOnly.flags(), 0);
        assert_eq!(Mode::RequestPairing.flags(), FLAG_REQUEST_PAIRING);
        assert!(!Mode::ConnectOnly.wants_pairing());
        assert!(Mode::RequestPairing.wants_pairing());
    }

    /// 主机名截断只截字符边界（不能把一个多字节字符切成两半）。
    #[test]
    fn local_name_is_valid_utf8() {
        let name = local_name();
        assert!(name.len() <= NAME_MAX);
        assert_eq!(String::from_utf8(name.clone().into_bytes()).unwrap(), name);
    }
}
