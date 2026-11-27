//! PAIR 握手的四张消息表（文档 §5.13.2~§5.13.5）：**只做编解码，不含状态机**。
//!
//! 三条硬约束（违反任何一条，另一端都会在"看起来没问题"的地方失败）：
//!
//! 1. **每个 u16 长度都先判上界再读**（名字 ≤ 200B、签名 ≤ 80B）。
//!    文档 §5.13.1 写得很直白：不许按声明的长度 `read_exact` 再判断 —— 那个长度是
//!    对面（可能是攻击者）说了算的，先按它分配再校验等于把内存当赌注。
//!    本文件里所有"长度"字段都走 [`Tap::take_vec`] **之前**的那一次比较，没有例外。
//! 2. **转录 T 用的是"上线字节"**（IS-7）。所以解析器把吃进去的每一段原样存进
//!    [`Raw::body`] 一起返回，调用方必须拿它去算哈希，**不许**"用完结构体再重新序列化一遍"：
//!    标准 UTF-8 与 Java 修改版 UTF-8（CESU-8）在星平面字符上差一截，重新序列化会让
//!    两端算出不同的 T，然后签名验不过 —— 现象看起来像"有人在冒充"，实际只是编码不同。
//! 3. **字符串编码跟随既有协议**：写 = 标准 UTF-8 + i16 大端字节长（复用
//!    [`crate::proto::write_utf`]，超 i16 上限判错、绝不截断）；读 = 标准 UTF-8 优先、
//!    CESU-8 兼容（复用 [`crate::proto::decode_utf_lenient`]）。这是 LESSONS L16 修出来的
//!    成果：手机端 `readUTF` 按标准 UTF-8 解，若按 CESU-8 写过去，一个 emoji 会变成
//!    6 个 U+FFFD，真机症状是"emoji 文件名推成 0 B"。
//!
//! 为什么文档写 u16 而本层实际用 i16：两者**线上字节完全相同**（≤200 这种小值碰不到
//! 符号位），用 i16 是为了和既有 proto 层共用同一套读写函数，少一份自定义编码；
//! 差异只出现在"长度 ≥ 32768 时怎么解释"，而本层 200 字节的硬上限让这条不可能发生。

use std::io::Read;

use crate::proto;

use super::ecdh::PUBKEY_LEN;
use super::{PairError, PairResult};

/// PR 消息版本（文档 §5.13.2 字段 2）。**不是** `proto::VERSION_CODE`：
/// 前者是"配对消息自身的格式版本"，后者是"两端 App 的协议版本"，两者独立演进。
pub const PAIR_PROTO: u16 = 1;

/// `PAIR_HELLO`（电脑 → 手机，明文）。
pub const TAG_HELLO: u8 = 0x01;
/// `PAIR_REPLY`（手机 → 电脑，明文）。
pub const TAG_REPLY: u8 = 0x02;
/// `PAIR_CONFIRM`（双向，各自发一次）。
pub const TAG_CONFIRM: u8 = 0x03;
/// `PAIR_ABORT`（双向，任一时刻可发）。
pub const TAG_ABORT: u8 = 0x04;

/// 名字（电脑显示名 / 手机 `Build.MODEL`）的字节上限。
///
/// 200 是文档定的值，不是随手取的：它同时给出"本层任何一条消息最大多少字节"的上界
/// （HELLO body ≤ 368），于是整个握手阶段的单条消息解析都可以在很小的缓冲里完成。
pub const NAME_MAX: usize = 200;
/// ECDSA 签名（DER）的字节上限。DER 对 P-256 正常是 70~72，给 80 是留出余量，
/// 但**必须**有上界：签名长度同样是对方说了算的。
pub const SIG_MAX: usize = 80;
/// nonce 长度（32 字节随机数）。
pub const NONCE_LEN: usize = 32;

/// `flags` 的 bit0：请求配对（用户在场，可以弹窗）。
///
/// bit0 = 0 表示"仅重连"：对端若不在信任表里，手机侧必须回
/// `PE_UNKNOWN_DEVICE` 并立刻关闭，**不许**弹配对对话框（用户不在场点确认）。
pub const FLAG_REQUEST_PAIRING: u16 = 0b0000_0001;

/// `err` 字段里表示"继续"的值（0），其余值见 [`super::PairError::code`]。
pub const ERR_NONE: u8 = 0;

/// 解析结果：结构体 + **它对应的上线字节**（不含 tag）。
///
/// 两者必须成对使用：字段用来做判断，`body` 用来算转录 T。拆开就会有人只留结构体，
/// 到时候重新序列化，IS-7 就被破了。
#[derive(Debug)]
pub struct Raw<T> {
    pub value: T,
    /// 上线时那串字节（不含 tag），逐字节原样。
    pub body: Vec<u8>,
}

/// `PAIR_HELLO` 的字段（文档 §5.13.2）。
#[derive(Debug)]
pub struct Hello {
    pub flags: u16,
    /// 本次会话的 ephemeral P-256 公钥，65 字节未压缩点。
    pub eph_pub: [u8; PUBKEY_LEN],
    /// 长期身份公钥（进信任表的是它的指纹）。
    pub stat_pub: [u8; PUBKEY_LEN],
    pub nonce: [u8; NONCE_LEN],
    /// 电脑显示名（主机名，用户可改）。
    pub name: String,
}

/// `PAIR_REPLY` 的字段（文档 §5.13.3）。比 HELLO 多一个 `err`。
#[derive(Debug)]
pub struct Reply {
    pub flags: u16,
    pub eph_pub: [u8; PUBKEY_LEN],
    pub stat_pub: [u8; PUBKEY_LEN],
    pub nonce: [u8; NONCE_LEN],
    pub name: String,
    /// 0 = 继续；非 0 时电脑直接关连接（值见 §5.8 的错误码表）。
    pub err: u8,
}

/// `PAIR_CONFIRM` 的字段（文档 §5.13.4）。
#[derive(Debug)]
pub struct Confirm {
    /// `ECDSA-P256-SHA256(transcript)`，DER 编码，长度由线上 u16 给出。
    pub sig: Vec<u8>,
    /// 0 = 同意；2 = 用户拒绝；5 = 报告"码不一致"；其余见 §5.8。
    pub err: u8,
}

/// 收到的一条消息。
///
/// 实现 Debug 是为了测试里的 `unwrap_err()` 能打出来，不参与生产日志：
/// 这条路径上的结构体带 nonce 与公钥，不该被 ``{:?}`` 顺手打进日志。
#[derive(Debug)]
pub enum Incoming {
    Hello(Raw<Hello>),
    Reply(Raw<Reply>),
    Confirm(Raw<Confirm>),
    /// `PAIR_ABORT` 只有一个 `err` 字节，不再包一层。
    Abort(u8),
}

impl Incoming {
    /// 给日志用的一句话（消息种类）。
    pub fn kind(&self) -> &'static str {
        match self {
            Incoming::Hello(_) => "PAIR_HELLO",
            Incoming::Reply(_) => "PAIR_REPLY",
            Incoming::Confirm(_) => "PAIR_CONFIRM",
            Incoming::Abort(_) => "PAIR_ABORT",
        }
    }

    /// 对端是否发来 `PAIR_ABORT`；是则给出它报的错误。
    ///
    /// 握手循环每一步后面都该问一次：对端可以在任何时刻放弃（用户点拒绝、
    /// 手机侧发现码不一致），此时再等下去只是在等一个已经不会来的字段。
    pub fn abort_err(&self) -> Option<PairError> {
        match self {
            Incoming::Abort(code) => Some(PairError::from_code(*code)),
            _ => None,
        }
    }
}

impl Hello {
    /// body（不含 tag）的线上字节。
    pub fn body(&self) -> PairResult<Vec<u8>> {
        let mut out = Vec::with_capacity(168 + self.name.len());
        write_u16(&mut out, PAIR_PROTO);
        write_u16(&mut out, self.flags);
        out.extend_from_slice(&self.eph_pub);
        out.extend_from_slice(&self.stat_pub);
        out.extend_from_slice(&self.nonce);
        write_name(&mut out, &self.name)?;
        Ok(out)
    }

    /// 完整消息（tag + body）。
    pub fn encode(&self) -> PairResult<Vec<u8>> {
        let body = self.body()?;
        let mut out = Vec::with_capacity(1 + body.len());
        out.push(TAG_HELLO);
        out.extend_from_slice(&body);
        Ok(out)
    }
}

impl Reply {
    pub fn body(&self) -> PairResult<Vec<u8>> {
        let mut out = Vec::with_capacity(169 + self.name.len());
        write_u16(&mut out, PAIR_PROTO);
        write_u16(&mut out, self.flags);
        out.extend_from_slice(&self.eph_pub);
        out.extend_from_slice(&self.stat_pub);
        out.extend_from_slice(&self.nonce);
        write_name(&mut out, &self.name)?;
        out.push(self.err);
        Ok(out)
    }

    pub fn encode(&self) -> PairResult<Vec<u8>> {
        let body = self.body()?;
        let mut out = Vec::with_capacity(1 + body.len());
        out.push(TAG_REPLY);
        out.extend_from_slice(&body);
        Ok(out)
    }
}

impl Confirm {
    pub fn body(&self) -> PairResult<Vec<u8>> {
        // 写端也要判上界：这里放行一个 2000 字节的签名，对端会按 SIG_MAX 拒掉，
        // 于是症状变成"配对失败"，而不是"本机日志里一行明确的错"。
        if self.sig.is_empty() || self.sig.len() > SIG_MAX {
            return Err(PairError::BadRecord);
        }
        let mut out = Vec::with_capacity(3 + self.sig.len());
        write_u16(&mut out, self.sig.len() as u16);
        out.extend_from_slice(&self.sig);
        out.push(self.err);
        Ok(out)
    }

    pub fn encode(&self) -> PairResult<Vec<u8>> {
        let body = self.body()?;
        let mut out = Vec::with_capacity(1 + body.len());
        out.push(TAG_CONFIRM);
        out.extend_from_slice(&body);
        Ok(out)
    }
}

/// `PAIR_ABORT` 的完整字节：`{tag=0x04}{err}`，恒定 2 字节。
///
/// 定长是它的价值所在：任何时刻只要 socket 还能写 2 个字节，就能把"我不玩了"
/// 和原因送出去，对端不会卡在等一个它永远等不到的长字段上。
pub fn encode_abort(err: PairError) -> Vec<u8> {
    vec![TAG_ABORT, err.code()]
}

/// 从流里读一条消息（含 tag）。
///
/// 未知 tag 一律 `PE_BAD_RECORD` 并让调用方关闭连接（文档 §5.13.1：握手阶段不做
/// "忽略未知消息"——那会给"两边版本不一致但都不报错"留后门）。
pub fn read_message<R: Read>(r: &mut R) -> PairResult<Incoming> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    // Tap 只记录 body 的字节：tag 已经在上面单独吃掉了，所以不放进 raw。
    let mut tap = Tap::new(r);
    match tag[0] {
        TAG_HELLO => {
            let value = parse_hello(&mut tap)?;
            Ok(Incoming::Hello(Raw { value, body: tap.raw }))
        }
        TAG_REPLY => {
            let value = parse_reply(&mut tap)?;
            Ok(Incoming::Reply(Raw { value, body: tap.raw }))
        }
        TAG_CONFIRM => {
            let value = parse_confirm(&mut tap)?;
            Ok(Incoming::Confirm(Raw { value, body: tap.raw }))
        }
        TAG_ABORT => {
            // 只读 1 字节，不碰 body 之外的任何东西；多出来的字节会留在流里，
            // 由调用方关闭连接丢弃（握手原子的语义是"收到 abort 就结束"）。
            let err = tap.take_u8()?;
            Ok(Incoming::Abort(err))
        }
        _ => Err(PairError::BadRecord),
    }
}

fn parse_hello<R: Read>(tap: &mut Tap<'_, R>) -> PairResult<Hello> {
    let proto_v = tap.take_u16()?;
    check_proto(proto_v)?;
    let flags = tap.take_u16()?;
    let eph_pub = tap.take_pubkey()?;
    let stat_pub = tap.take_pubkey()?;
    let nonce = tap.take_arr::<NONCE_LEN>()?;
    let name = read_name(tap)?;
    Ok(Hello { flags, eph_pub, stat_pub, nonce, name })
}

fn parse_reply<R: Read>(tap: &mut Tap<'_, R>) -> PairResult<Reply> {
    let proto_v = tap.take_u16()?;
    check_proto(proto_v)?;
    let flags = tap.take_u16()?;
    let eph_pub = tap.take_pubkey()?;
    let stat_pub = tap.take_pubkey()?;
    let nonce = tap.take_arr::<NONCE_LEN>()?;
    let name = read_name(tap)?;
    let err = tap.take_u8()?;
    Ok(Reply { flags, eph_pub, stat_pub, nonce, name, err })
}

fn parse_confirm<R: Read>(tap: &mut Tap<'_, R>) -> PairResult<Confirm> {
    let declared = tap.take_u16()? as usize;
    // 0 长度也判错：DER 签名至少 8 字节（两个 1 字节整数），空签名只可能是构造出来的。
    if declared == 0 || declared > SIG_MAX {
        return Err(PairError::BadRecord);
    }
    let sig = tap.take_vec(declared)?;
    let err = tap.take_u8()?;
    Ok(Confirm { sig, err })
}

fn check_proto(v: u16) -> PairResult<()> {
    if v == PAIR_PROTO {
        Ok(())
    } else {
        // 用 Version 而不是 BadRecord：这条要能在界面上说"两端配对消息格式不一致，
        // 请把两端都升级"，而不是"通信被篡改"。两者对用户的含义完全不同。
        Err(PairError::Version)
    }
}

fn read_name<R: Read>(tap: &mut Tap<'_, R>) -> PairResult<String> {
    let declared = tap.take_u16()? as usize;
    // ⚠ 这一行是 §5.13.1 那条硬性检查的落点：先比对，再决定读多少。
    if declared > NAME_MAX {
        return Err(PairError::BadRecord);
    }
    let bytes = tap.take_vec(declared)?;
    // 读时两种 UTF-8 都收（见文件头第 3 条）：对端可能是还没跟上 L16 修复的实现，
    // 那种实现写过来的星平面字符是 CESU-8，只按标准 UTF-8 解会得到 U+FFFD。
    Ok(proto::decode_utf_lenient(&bytes))
}

fn write_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn write_name(out: &mut Vec<u8>, name: &str) -> PairResult<()> {
    let bytes = proto::encode_utf(name);
    // 写端也必须有上界：超了判错，不许"截断成 200 字节照样发"——
    // 截断会切在多字节序列中间，对端读到半个字符，症状是两边名字显示不同而没人报错。
    if bytes.len() > NAME_MAX {
        return Err(PairError::BadRecord);
    }
    // 复用既有 proto 的写函数（i16 长度前缀 + 标准 UTF-8），
    // 不在这里手写一遍：手写就有第二次机会把编码写错（L16 就是这么来的）。
    proto::write_utf(out, name).map_err(PairError::from)?;
    Ok(())
}

/// 读字节的同时把"吃进去的字节"原样记下来。
///
/// 这是 IS-7 的落点：解析器唯一能找到"上线字节"的地方就是这里，
/// 想重新序列化反而得额外写代码 —— 让正确的做法成为顺手的做法。
struct Tap<'a, R: Read> {
    inner: &'a mut R,
    raw: Vec<u8>,
}

impl<'a, R: Read> Tap<'a, R> {
    fn new(inner: &'a mut R) -> Tap<'a, R> {
        // HELLO body 最长 368 字节，一次分配到位，避免解析期间反复扩容。
        Tap { inner, raw: Vec::with_capacity(369) }
    }

    fn take_arr<const N: usize>(&mut self) -> PairResult<[u8; N]> {
        let mut buf = [0u8; N];
        self.inner.read_exact(&mut buf)?;
        self.raw.extend_from_slice(&buf);
        Ok(buf)
    }

    fn take_u8(&mut self) -> PairResult<u8> {
        Ok(self.take_arr::<1>()?[0])
    }

    fn take_u16(&mut self) -> PairResult<u16> {
        Ok(u16::from_be_bytes(self.take_arr::<2>()?))
    }

    /// 读一个 65 字节未压缩点。首字节必须是 0x04：
    /// 这既是文档的硬约束，也是一种便宜的早退 —— 不是 0x04 的东西不可能被 ring
    /// 当成合法公钥，早一点拒掉就不必走一次 ECDH。
    fn take_pubkey(&mut self) -> PairResult<[u8; PUBKEY_LEN]> {
        let key = self.take_arr::<PUBKEY_LEN>()?;
        if key[0] != 0x04 {
            return Err(PairError::BadRecord);
        }
        Ok(key)
    }

    /// 只允许在"调用方已经比对过上界"的地方调用。本文件内唯一的调用点是
    /// [`read_name`] 和 [`parse_confirm`]，两处都在调用前做了比较。
    fn take_vec(&mut self, n: usize) -> PairResult<Vec<u8>> {
        let mut buf = vec![0u8; n];
        self.inner.read_exact(&mut buf)?;
        self.raw.extend_from_slice(&buf);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::testutil::hex;

    /// 附录 A.4 的固定输入（HELLO 的 eph/stat 都用 A.2 的 pubA）。
    fn kat_hello() -> Hello {
        let pub_a: [u8; PUBKEY_LEN] = hex(
            "0460fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb679
             03fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299",
        )
        .try_into()
        .unwrap();
        let nonce_a: [u8; NONCE_LEN] =
            hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
                .try_into()
                .unwrap();
        Hello {
            flags: 1,
            eph_pub: pub_a,
            stat_pub: pub_a,
            nonce: nonce_a,
            name: "DESKTOP-TEST".to_string(),
        }
    }

    fn kat_reply() -> Reply {
        let pub_b: [u8; PUBKEY_LEN] = hex(
            "045bdde352eaedf5dcdd22590c240d01598cc41349abe432f5932c9ca0b311f3e6
             dc795473b51e84937a465d56d8763e6c5a5d5b334e56cf81db9f46178bb67e54",
        )
        .try_into()
        .unwrap();
        let nonce_b: [u8; NONCE_LEN] =
            hex("202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f")
                .try_into()
                .unwrap();
        Reply {
            flags: 1,
            eph_pub: pub_b,
            stat_pub: pub_b,
            nonce: nonce_b,
            name: "Pixel-Test".to_string(),
            err: 0,
        }
    }

    /// 附录 A.4：HELLO body 必须**逐字节**等于文档给的那串。
    ///
    /// 这是整个配对协议里唯一能一次证明"字段顺序 + 每个长度前缀 + 字符串编码"都对的地方；
    /// 两端都错在同一处的话，T 会一致地算错，签名一路通过，直到真机才发现连不上。
    #[test]
    fn appendix_a4_hello_body() {
        let expected = hex(
            "0001 0001
             0460fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6
             7903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299
             0460fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6
             7903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299
             000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
             000c 4445534b544f502d54455354",
        );
        assert_eq!(kat_hello().body().unwrap(), expected, "HELLO body 与附录 A.4 不一致");

        // 完整消息 = tag 0x01 + body。
        let mut full = vec![TAG_HELLO];
        full.extend_from_slice(&expected);
        assert_eq!(kat_hello().encode().unwrap(), full);
    }

    /// 附录 A.4：REPLY body 同理（注意末尾那个 00 的 err 字段）。
    #[test]
    fn appendix_a4_reply_body() {
        let expected = hex(
            "0001 0001
             045bdde352eaedf5dcdd22590c240d01598cc41349abe432f5932c9ca0b311f3e6
             dc795473b51e84937a465d56d8763e6c5a5d5b334e56cf81db9f46178bb67e54
             045bdde352eaedf5dcdd22590c240d01598cc41349abe432f5932c9ca0b311f3e6
             dc795473b51e84937a465d56d8763e6c5a5d5b334e56cf81db9f46178bb67e54
             202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f
             000a 506978656c2d54657374 00",
        );
        assert_eq!(kat_reply().body().unwrap(), expected, "REPLY body 与附录 A.4 不一致");

        let mut full = vec![TAG_REPLY];
        full.extend_from_slice(&expected);
        assert_eq!(kat_reply().encode().unwrap(), full);
    }

    /// IS-7：解析器必须把**上线字节**原样交回来。
    ///
    /// 断言的是 "body == 输入的那串字节"，而不是"重新编码后相等"——这两件事在
    /// ASCII 名字上恰好等价，所以下面还有一条 CESU-8 的用例把它们区分开。
    #[test]
    fn parser_returns_wire_bytes() {
        let body = kat_hello().body().unwrap();
        let mut wire = vec![TAG_HELLO];
        wire.extend_from_slice(&body);

        let got = read_message(&mut &wire[..]).unwrap();
        match got {
            Incoming::Hello(raw) => {
                assert_eq!(raw.body, body, "解析器没有原样返回上线字节（IS-7）");
                assert_eq!(raw.value.name, "DESKTOP-TEST");
                assert_eq!(raw.value.flags, 1);
                assert_eq!(raw.value.nonce[31], 0x1f);
                assert_eq!(raw.value.eph_pub[0], 0x04);
            }
            other => panic!("tag 0x01 应该解析成 HELLO，实际是 {}", other.kind()),
        }
    }

    /// IS-7 的关键用例：名字是 CESU-8 时，解码要能还原成 emoji，
    /// 而 body 必须还是那串 CESU-8 字节（不许被重写成标准 UTF-8）。
    ///
    /// 手搓这条消息，不复用编码器 —— 编码器写的是标准 UTF-8，用它构造不出 CESU-8。
    #[test]
    fn cesu8_name_is_decoded_but_body_kept_verbatim() {
        let pubkey: [u8; PUBKEY_LEN] = kat_hello().eph_pub;
        let mut body = Vec::new();
        write_u16(&mut body, PAIR_PROTO);
        write_u16(&mut body, 0);
        body.extend_from_slice(&pubkey);
        body.extend_from_slice(&pubkey);
        body.extend_from_slice(&[0x42u8; NONCE_LEN]);
        // 🚀 U+1F680 的 CESU-8：代理对 D83D DE80 -> ED A0 BD ED BA 80（6 字节）；
        // 它的标准 UTF-8 是 F0 9F 9A 80（4 字节），两者字节数都不一样 —— 这正是
        // "重新序列化就会算错 T" 的物理原因。
        let cesu = [0xEDu8, 0xA0, 0xBD, 0xED, 0xBA, 0x80];
        write_u16(&mut body, cesu.len() as u16);
        body.extend_from_slice(&cesu);

        let mut wire = vec![TAG_HELLO];
        wire.extend_from_slice(&body);
        let raw = match read_message(&mut &wire[..]).unwrap() {
            Incoming::Hello(raw) => raw,
            other => panic!("应该是 HELLO，实际 {}", other.kind()),
        };
        assert_eq!(raw.value.name, "\u{1F680}", "CESU-8 名字没被还原成 emoji");
        assert_eq!(raw.body, body, "body 被重新序列化了 —— 两端会算出不同的 T（IS-7）");
        // 顺带确认这不是标准 UTF-8：两者字节不同，正是"不许重新序列化"的原因。
        assert_ne!(proto::encode_utf("\u{1F680}"), cesu.to_vec());
    }

    /// 名字声明 201 字节：必须**在读取之前**判错。
    ///
    /// 用 BudgetReader 证明"没有按声明长度去读"：预算刚好够读到长度字段，
    /// 若解析器再多读一个字节，底层返回的是 IO 错误，测试就会拿到 Io 而不是 BadRecord。
    #[test]
    fn oversized_name_is_rejected_before_reading() {
        let mut body = Vec::new();
        write_u16(&mut body, PAIR_PROTO);
        write_u16(&mut body, 0);
        body.extend_from_slice(&[0x04u8; PUBKEY_LEN]);
        body.extend_from_slice(&[0x04u8; PUBKEY_LEN]);
        body.extend_from_slice(&[0x11u8; NONCE_LEN]);
        write_u16(&mut body, (NAME_MAX + 1) as u16);
        // 故意一个字节都不跟：若解析器按 201 去 read_exact，它会先撞上预算，
        // 报 Io，我们就知道"先判上界"这条被破坏了。
        let mut wire = vec![TAG_HELLO];
        wire.extend_from_slice(&body);

        let mut reader = BudgetReader::new(&wire, wire.len());
        let err = read_message(&mut reader).unwrap_err();
        assert_eq!(err, PairError::BadRecord, "越界名字必须报 BadRecord，不能是 {}", err);
        assert_eq!(reader.consumed, wire.len(), "解析器读了超出前缀的字节");
    }

    /// 签名长度 0 / 81 都必须判错（0 是"空签名"，81 是越界）。
    #[test]
    fn bad_signature_lengths_are_rejected() {
        for declared in [0u16, (SIG_MAX + 1) as u16] {
            let mut body = Vec::new();
            write_u16(&mut body, declared);
            let mut wire = vec![TAG_CONFIRM];
            wire.extend_from_slice(&body);
            let mut reader = BudgetReader::new(&wire, wire.len());
            let err = read_message(&mut reader).unwrap_err();
            assert_eq!(err, PairError::BadRecord, "sig_len={} 应该报 BadRecord", declared);
            assert_eq!(reader.consumed, wire.len(), "sig_len={} 时多读了字节", declared);
        }
    }

    /// 未知 tag 一律 BadRecord（§5.13.1：不做"忽略未知消息"）。
    #[test]
    fn unknown_tags_are_rejected() {
        for tag in [0x00u8, 0x05, 0x7f, 0xff] {
            let wire = [tag, 0x00, 0x01];
            let err = read_message(&mut &wire[..]).unwrap_err();
            assert_eq!(err, PairError::BadRecord, "未知 tag {} 应该报 BadRecord", tag);
        }
    }

    /// 截断的消息只许报错，不许 panic（这是会走网络的输入）。
    #[test]
    fn truncated_messages_error_without_panic() {
        let full = kat_hello().encode().unwrap();
        for cut in [1usize, 2, 5, 70, 100, full.len() - 1] {
            let err = read_message(&mut &full[..cut]).unwrap_err();
            assert_eq!(err, PairError::Io, "截断到 {} 字节应该是 Io（对端断了）", cut);
        }
    }

    /// proto 版本不一致 → Version（给用户的话是"两端消息格式不一致，请升级"）。
    #[test]
    fn pair_proto_mismatch_is_version_error() {
        let mut body = Vec::new();
        write_u16(&mut body, PAIR_PROTO + 1);
        write_u16(&mut body, 0);
        body.extend_from_slice(&[0x04u8; PUBKEY_LEN]);
        body.extend_from_slice(&[0x04u8; PUBKEY_LEN]);
        body.extend_from_slice(&[0x22u8; NONCE_LEN]);
        write_u16(&mut body, 0);
        let mut wire = vec![TAG_HELLO];
        wire.extend_from_slice(&body);
        assert_eq!(read_message(&mut &wire[..]).unwrap_err(), PairError::Version);
    }

    /// 公钥首字节不是 0x04（压缩点 / 垃圾）必须当场拒。
    #[test]
    fn compressed_point_is_rejected() {
        let mut body = Vec::new();
        write_u16(&mut body, PAIR_PROTO);
        write_u16(&mut body, 0);
        body.push(0x02);
        body.extend_from_slice(&[0x33u8; PUBKEY_LEN - 1]);
        body.extend_from_slice(&[0x04u8; PUBKEY_LEN]);
        body.extend_from_slice(&[0x44u8; NONCE_LEN]);
        write_u16(&mut body, 0);
        let mut wire = vec![TAG_HELLO];
        wire.extend_from_slice(&body);
        assert_eq!(read_message(&mut &wire[..]).unwrap_err(), PairError::BadRecord);
    }

    /// CONFIRM 往返；ABORT 恒定 2 字节。
    #[test]
    fn confirm_and_abort_encoding() {
        let confirm = Confirm { sig: vec![0x30u8; 71], err: 0 };
        let wire = confirm.encode().unwrap();
        assert_eq!(&wire[..3], &[TAG_CONFIRM, 0x00, 0x47], "sig_len 应该是 71（0x0047）");
        let raw = match read_message(&mut &wire[..]).unwrap() {
            Incoming::Confirm(raw) => raw,
            other => panic!("应该是 CONFIRM，实际 {}", other.kind()),
        };
        assert_eq!(raw.value.sig.len(), 71);
        assert_eq!(raw.value.err, 0);
        assert_eq!(raw.body.len(), 3 + 71);

        let abort = encode_abort(PairError::SasMismatch);
        assert_eq!(abort, vec![TAG_ABORT, 5]);
        let mut reader = &abort[..];
        assert_eq!(read_message(&mut reader).unwrap().abort_err(), Some(PairError::SasMismatch));
    }

    /// 写端也不许超长：201 字节名字必须报错，而不是让对端按 NAME_MAX 拒掉。
    #[test]
    fn encoder_rejects_oversized_name() {
        let mut hello = kat_hello();
        hello.name = "x".repeat(NAME_MAX + 1);
        assert_eq!(hello.body().unwrap_err(), PairError::BadRecord);
    }

    /// `flags` 的 bit0 常量与文档一致（0x0001），且位置正确。
    #[test]
    fn flag_bit0_is_pair_request() {
        assert_eq!(FLAG_REQUEST_PAIRING, 1);
        let mut hello = kat_hello();
        hello.flags = 0;
        let wire = hello.encode().unwrap();
        assert_eq!(&wire[1..5], &[0x00, 0x01, 0x00, 0x00], "proto 之后就是 flags");
    }

    /// 带预算的读端：超出预算就返回 IO 错误，并把"一共被读了多少"记下来。
    ///
    /// 它存在的意义是把"先判上界"这条约束变成**可测**的：只要解析器多读一个字节，
    /// 拿到的错误类型就从 BadRecord 变成 Io。
    struct BudgetReader<'a> {
        data: &'a [u8],
        budget: usize,
        consumed: usize,
    }

    impl<'a> BudgetReader<'a> {
        fn new(data: &'a [u8], budget: usize) -> BudgetReader<'a> {
            BudgetReader { data, budget, consumed: 0 }
        }
    }

    impl<'a> Read for BudgetReader<'a> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let left = self.budget.saturating_sub(self.consumed);
            if left == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "读超出预算：解析器按声明长度去读了",
                ));
            }
            let n = std::cmp::min(left, std::cmp::min(buf.len(), self.data.len() - self.consumed));
            buf[..n].copy_from_slice(&self.data[self.consumed..self.consumed + n]);
            self.consumed += n;
            Ok(n)
        }
    }
}
