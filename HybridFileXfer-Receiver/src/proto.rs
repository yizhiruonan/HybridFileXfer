//! Wire format of the HybridFileXfer protocol.
//!
//! Everything is big endian, matching Java's `DataOutput`. Strings use Java's
//! "modified UTF-8" (UTF-16 code units, surrogate pairs encoded as two 3-byte
//! sequences, NUL as `C0 80`), prefixed by an unsigned 16 bit byte length.

use std::io::{self, Read, Write};

pub const CLIENT_HEADER: &[u8; 4] = b"HFXC";
/// 协议版本：300 → 301 增加了暂停状态同步（PAUSE_STATE）
///
/// 301 → 302：传输期间的控制通道改成带 opcode 的帧。
/// 以前收尾是一串裸 bool，接收方在整段传输里死等它，只能把控制通道的锁和读取权一起占住 ——
/// 于是会话线程既同步不了暂停状态、也发不了心跳（3.25GB 传到一半就被判"没有收到心跳"断开）。
/// 现在：结论帧 `RESULT`、暂停回报帧 `PAUSE_REPLY`，收发两端都按帧循环读，
/// 传输线程成为控制通道唯一的读者，别人的暂停同步就地处理掉，不会再读串。
/// 302 → 303：完整性校验。
///  - 每个文件块头多一个 i32 CRC32（发送端随路算、接收端写盘前校验，坏块立刻报错）；
///  - 结论帧 RESULT 多一段「文件指纹清单」，收尾时双方各自复算整文件 SHA-256 比对。
///
/// 307 → 308：限速从「两端各自设、取较小值」改成「两端同步一个共用值」，
///          新增 SPEED_LIMIT 帧（连上之后改设置时用）。
///
/// 308 → 309：手机把「排队中」的任务也报给电脑。
///  - 手机上一次选几十个文件时，同一刻只有一个在传，其余都在排队；电脑端以前只看得见
///    正在传的那一个，界面上凭空少了一截（用户以为选中的文件丢了）。
///  - 新增 QUEUE_STATE 帧（手机 → 电脑、只发不答、整表覆盖），电脑端照它把排队的那些
///    画成「等待中」的块。
///  - 必须升版本：旧电脑端遇到不认识的帧会先塞进 pending_opcode，之后按预期帧比对时
///    报 unexpected control frame，把整条会话当成「帧序错位」拆掉 —— 两端版本相等才握手，
///    所以老电脑端 + 新手机端只会停在「版本不一致」，不会连上之后莫名断开。
///
/// 303 → 304：取消不断连。
///  - CANCEL 的语义变了：手机发完 CANCEL 只关传输通道，控制通道保持打开
///    （以前连控制通道一起关，于是取消 = 整条会话拆除，两端都要重连）；
///  - 新增 REBUILD_REQ / REBUILD_ACK：电脑收尾完把传输通道重新连一批回来，会话不断。
///
/// 305 → 306：电脑→手机方向的断点续传。
///  - 手机（接收端）在 REQUEST_SEND 之后回一帧 RESUME_STATE：每个文件"已经收到哪些块"；
///  - 电脑按它跳过已经收到的块，只补缺的。
///
/// 304 → 305：断点续传。
///  - 接收方给每个文件留一份 `<文件名>.hfxpart`（已完成块的位图）；
///  - 发送方开传之前先发 RESUME_QUERY（文件清单），接收方回 RESUME_REPLY（每个文件的位图），
///    发送方把已经收到的块跳过，只补缺的；
///  - 取消/中断时半截文件不再删除，留着下次续传（有位图就不会出现"大小对但内容有洞"的假文件）。
///
/// 306 → 307：双端限速协商。
///  - 原来限速只是手机端的**本地**设置，且只挂在手机的发送路径上（手机发文件时生效）。
///    电脑推文件给手机时，数据源在电脑，电脑却不限速 —— 那条方向完全没有限速。
///  - 307 起握手阶段交换两端的上限，两端各自算 `min(非零上限)`，对**自己那条发送路径**生效：
///    手机发文件用手机的 SpeedLimiter，电脑推文件用电脑的 ratelimit，上限是同一个协商值。
///  - 0 的语义是「我这端不限速」，**不参与 min**：一端填 0 不会抹掉另一端的限制。
///  - 线序（追加在版本号之后，不改变已有字段的次序）：
///     电脑 → 手机：i32 电脑上限（MB/s）
///     手机 → 电脑：i32 手机上限定（MB/s）
///    都在「版本匹配」这一步之后、网卡清单之前。版本不匹配时这一对帧不会交换，
///    所以老版本对端看到的错误仍是原来那句「版本不一致」。
pub const VERSION_CODE: i32 = 309;

pub const BLOCK_SIZE: u64 = 1024 * 1024;

pub mod controller {
    pub const SHUTDOWN: i16 = 0;
    pub const LIST_FILES: i16 = 1;
    pub const DELETE_FILE: i16 = 2;
    pub const MKDIR: i16 = 3;
    pub const REQUEST_RECEIVE: i16 = 10;
    pub const REQUEST_SEND: i16 = 11;
    /// Not part of the original protocol: the phone sends this short while the
    /// connection is idle so that a dead link is detected. Unknown ids are
    /// ignored by every client implementation, so this is backwards compatible.
    pub const PING: i16 = 20;
/// 手机轮询：电脑有没有要推给我的文件（本项目新增）
pub const CHECK_PENDING: i16 = 21;
/// 暂停状态同步（本项目新增）：手机发 [bool 手机是否暂停]，电脑回 [bool 电脑是否暂停]。
/// 任何一端点了暂停/继续，另一端在下次同步（约 250ms）就跟上，界面一起显示已暂停。
pub const PAUSE_STATE: i16 = 22;
/// 302：一次传输的结论帧 `[bool ok][utf 错误信息?]`（成功时不带错误信息）。
/// 两个方向共用：接收方写"落盘结论"，发送方写"读文件结论"。
pub const RESULT: i16 = 23;
/// 302：PAUSE_STATE 的回报帧 `[bool 电脑是否暂停]`。
/// 以前是裸 bool，传输期间会和结论帧读串，所以升版本一起改。
pub const PAUSE_REPLY: i16 = 24;
/// 303：手机端主动取消本次传输（用户在手机上点了「取消」）。
///
/// 以前取消只有硬断 socket 一条路，电脑端只能看到一堆 ConnectionReset：
///  - 非暂停时判成「接收失败: failed to fill whole buffer」，当成错误弹窗；
///  - 暂停时更糟：接收线程停在暂停等待里不读 socket，永远发现不了对端已经走了，
///    `handle_receive` 的等待循环因此永不结束，界面和日志都定格在「已暂停」。
///
/// 现在取消先发这一帧（不带载荷），电脑端就地置取消标志，各通道干净退出。
pub const CANCEL: i16 = 25;
    /// 304：取消后重建传输通道。手机发完 CANCEL 只关传输通道、保留控制通道，
    /// 电脑收尾完再连一批新的传输通道回来，会话不断。
    ///
    /// 两个方向都带 opcode 分帧，不能用裸 bool —— 手机端随时可能在飞 PAUSE_STATE，
    /// 裸 bool 会和它的帧头撞车，帧流一错位整条会话就报废：
    ///   电脑 → 手机：{i16 REBUILD_REQ}{UTF 通道名}
    ///   手机 → 电脑：{i16 REBUILD_ACK}{bool 是否接受}
    pub const REBUILD_REQ: i16 = 26;
    pub const REBUILD_ACK: i16 = 27;
    /// 305：断点续传查询（发送方 → 接收方）
    ///
    /// `{i32 count}{ (UTF 目标路径, i64 总大小, i64 源修改时间) * count }`
    pub const RESUME_QUERY: i16 = 28;
    /// 305：断点续传应答（接收方 → 发送方）
    ///
    /// `{i32 count}{ (bool 可续传, i32 位图字节数, u8[位图]) * count }`
    /// 位图第 i 位 = 第 i 块（1MiB）已经落盘。
    pub const RESUME_REPLY: i16 = 29;
    /// 306：手机 → 电脑，手机作为接收端上报自己已经收到哪些块（电脑→手机方向的续传）
    ///
    /// `{i32 count}{ (UTF 目标路径, i64 总大小, i64 源修改时间, i32 位图字节数, u8[位图]) * count }`
    pub const RESUME_STATE: i16 = 30;
    /// 手机端取消这次传输，并且明确要求「不保留进度」：把这次写到一半的文件和
    /// `<文件名>.hfxpart` 续传记录一起删掉，下次同一个文件从头传。
    ///
    /// 和 `CANCEL` 一样不带载荷、语义只多一条：收到它就先当取消处理，
    /// 收尾时按"丢弃"收尾（`Receiver::finish_with(false)`：删掉半截文件 + 续传日志）。
    /// 没有为它升版本号 —— 旧版本电脑端会把它当成"不认识的控制指令"忽略掉，
    /// 取消本身仍然会因为传输通道被关掉而被发现，只是半截文件会被保留（退化成"保留进度"），
    /// 不会出错；和当初 PING / CHECK_PENDING 一样是纯增量、可忽略的指令。
    pub const CANCEL_DISCARD: i16 = 31;
    /// 308：限速值同步（本项目新增）。{i32 MB/s}，0 = 不限速。
    ///
    /// 307 那套是「两端各报一个上限，取非零里的最小值」—— 结果是两端界面上
    /// 显示的数字可能不一样（电脑写 20、手机写 5，实际跑 5，但两边都还写着各自的数）。
    /// 308 改成**一个共用值**：谁改了都推给对方，两端永远显示同一个数。
    ///
    /// 握手时报的仍是这个值（见 session.rs 的握手段），本帧只负责「连上之后又改了」。
    /// 载荷固定 4 字节，所以 peek 判帧长的地方要按 6 字节等 —— 和 PAUSE_STATE
    /// 那种 1 字节载荷的帧不能共用一套判断。
    pub const SPEED_LIMIT: i16 = 32;
    /// 309：手机队列快照（手机 → 电脑，只发不答，整表覆盖）
    ///
    /// `{i32 count}{ (UTF 文件名, i64 字节数, u8 状态) * count }`
    /// 状态：0 = 等待中、1 = 进行中、2 = 已完成、3 = 失败。
    ///
    /// 为什么是整表覆盖而不是增量：手机端的队列随用户操作随时重排（再勾几个文件、
    /// 删掉排队的某一条），发增量就得再约定「第几条被删」的编号规则，而两边的编号
    /// 一旦对不上，界面上就会多出几条永远不消失的「等待中」。
    /// 整表覆盖只要求「这一次手机眼里的队列长什么样」，电脑端直接替换，怎么都不会错位。
    ///
    /// UTF 长度前缀是 i16 + 修改版 UTF-8，读法同 RESUME_QUERY（proto::read_utf / write_utf）。
    /// 载荷是**变长**的：session.rs 里那些「peek 够不够一整帧」的地方不能按定长帧算，
    /// 只能在读到 opcode 之后顺着载荷读（见 read_control_frame_skipping_queue）。
    pub const QUEUE_STATE: i16 = 33;
}

pub mod transfer {
    pub const FILE: i16 = 0;
    pub const FOLDER: i16 = 1;
    #[allow(dead_code)]
    pub const FILE_SLICE: i16 = 2;
    pub const EOF: i16 = 3;
    pub const END_OF_INTERRUPTED: i16 = 4;
    pub const END_OF_READ_ERROR: i16 = 5;
    pub const END_OF_WRITE_ERROR: i16 = 6;
}

pub mod fs_kind {
    pub const UNIX: i32 = 0;
    pub const WINDOWS: i32 = 1;
}

pub fn current_file_system() -> i32 {
    if std::path::MAIN_SEPARATOR == '\\' {
        fs_kind::WINDOWS
    } else {
        fs_kind::UNIX
    }
}

pub fn write_u8<W: Write>(w: &mut W, v: u8) -> io::Result<()> {
    w.write_all(&[v])
}

pub fn write_bool<W: Write>(w: &mut W, v: bool) -> io::Result<()> {
    write_u8(w, if v { 1 } else { 0 })
}

pub fn write_i16<W: Write>(w: &mut W, v: i16) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

pub fn write_i32<W: Write>(w: &mut W, v: i32) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

pub fn write_i64<W: Write>(w: &mut W, v: i64) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

pub fn write_utf<W: Write>(w: &mut W, s: &str) -> io::Result<()> {
    let bytes = encode_utf(s);
    if bytes.len() > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "string too long for writeUTF",
        ));
    }
    write_i16(w, bytes.len() as i16)?;
    w.write_all(&bytes)
}

pub fn read_u8<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

pub fn read_bool<R: Read>(r: &mut R) -> io::Result<bool> {
    Ok(read_u8(r)? != 0)
}

pub fn read_i16<R: Read>(r: &mut R) -> io::Result<i16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(i16::from_be_bytes(b))
}

pub fn read_i32<R: Read>(r: &mut R) -> io::Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_be_bytes(b))
}

pub fn read_i64<R: Read>(r: &mut R) -> io::Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_be_bytes(b))
}

pub fn read_utf<R: Read>(r: &mut R) -> io::Result<String> {
    let len = read_i16(r)?;
    if len < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "negative UTF length",
        ));
    }
    let mut bytes = vec![0u8; len as usize];
    r.read_exact(&mut bytes)?;
    Ok(decode_utf_lenient(&bytes))
}

/// 写字符串：u16 大端字节长度 + **标准 UTF-8**。
///
/// 为什么不是 Java 的 writeUTF（修改版 UTF-8）：手机端读字符串走的是
/// DataByteChannel.readUTF，内部就是 `new String(bytes, StandardCharsets.UTF_8)`，
/// 按标准 UTF-8 解。修改版 UTF-8 会把 U+1F680 这类星平面字符拆成两个 3 字节代理
/// （ED A0 BD ED B8 80），标准解码器看到这一段只会吐出 6 个 U+FFFD。
/// 真机症状（2026-11-26 实测）：日志里 `报告 50% ������.bin: 系统找不到指定的文件。`、
/// 推送 0 B、文件永远到不了手机。BMP 字符（含全部中文）两种编码完全一致，
/// 所以只有 emoji 这类星平面字符会踩到 —— 也正因为这样，以前一直没人发现。
///
/// 读的时候仍然两种都收（见 [`decode_utf_lenient`]）：手机端有一处 —— 队列快照 ——
/// 是用真正的 DataOutputStream.writeUTF 拼的整帧，那是修改版 UTF-8。
pub fn encode_utf(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

/// 读字符串：标准 UTF-8 和 Java 修改版 UTF-8（CESU-8）都收。
///
/// 手机上绝大多数字符串是标准 UTF-8；只有队列快照那一帧是真正的
/// DataOutputStream.writeUTF —— 星平面字符在那里会变成两个 3 字节代理，
/// 所以这里按 UTF-16 码元收，最后统一 from_utf16_lossy 让代理对拼回一个字符。
/// 标准 UTF-8 的 4 字节序列（0xF0..）也照收。
pub fn decode_utf_lenient(bytes: &[u8]) -> String {
    let mut units: Vec<u16> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x00..=0x7F => {
                units.push(b as u16);
                i += 1;
            }
            0xC0..=0xDF => {
                if i + 1 >= bytes.len() {
                    units.push(0xFFFD);
                    break;
                }
                let b2 = bytes[i + 1];
                let cp = (((b & 0x1F) as u32) << 6) | (b2 & 0x3F) as u32;
                units.push(cp as u16);
                i += 2;
            }
            0xE0..=0xEF => {
                if i + 2 >= bytes.len() {
                    units.push(0xFFFD);
                    break;
                }
                let b2 = bytes[i + 1];
                let b3 = bytes[i + 2];
                let cp = (((b & 0x0F) as u32) << 12)
                    | (((b2 & 0x3F) as u32) << 6)
                    | (b3 & 0x3F) as u32;
                units.push(cp as u16);
                i += 3;
            }
            0xF0..=0xF7 => {
                if i + 3 >= bytes.len() {
                    units.push(0xFFFD);
                    break;
                }
                let b2 = bytes[i + 1];
                let b3 = bytes[i + 2];
                let b4 = bytes[i + 3];
                let cp = (((b & 0x07) as u32) << 18)
                    | (((b2 & 0x3F) as u32) << 12)
                    | (((b3 & 0x3F) as u32) << 6)
                    | (b4 & 0x3F) as u32;
                if (0x10000..=0x10FFFF).contains(&cp) {
                    let mut buf = [0u16; 2];
                    for u in char::from_u32(cp).unwrap_or('\u{FFFD}').encode_utf16(&mut buf) {
                        units.push(*u);
                    }
                } else {
                    units.push(0xFFFD);
                }
                i += 4;
            }
            _ => {
                units.push(0xFFFD);
                i += 1;
            }
        }
    }
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf_is_standard_utf8() {
        assert_eq!(encode_utf("abc"), vec![0x61, 0x62, 0x63]);
        assert_eq!(encode_utf("测试"), vec![0xE6, 0xB5, 0x8B, 0xE8, 0xAF, 0x95]);
        // 星平面字符必须是 4 字节标准 UTF-8。写成 CESU-8（ED A0 BD ED B8 80）的话，
        // 手机端 new String(bytes, UTF_8) 会解出 6 个 U+FFFD —— 就是真机上那条
        // 「报告 50% ������.bin: 系统找不到指定的文件」。
        assert_eq!(encode_utf("😀"), vec![0xF0, 0x9F, 0x98, 0x80]);
        assert_eq!(encode_utf("a\0b"), vec![0x61, 0x00, 0x62]);
    }

    #[test]
    fn utf_round_trip() {
        for s in ["", "hello", "中文/路径", "😀 emoji", "报告 50% 🚀.bin", "a\0b", "ümlaut"] {
            let bytes = encode_utf(s);
            assert_eq!(decode_utf_lenient(&bytes), s, "round trip for {:?}", s);
        }
        // Cross-check a Java style UTF length prefix.
        let mut buf = Vec::new();
        write_utf(&mut buf, "文件").unwrap();
        assert_eq!(&buf[..2], &[0, 6]);
        assert_eq!(read_utf(&mut buf.as_slice()).unwrap(), "文件");
    }

    /// 手机端那一帧队列快照是真正的 DataOutputStream.writeUTF（修改版 UTF-8），
    /// 星平面字符在里面是两个 3 字节代理 —— 读进来必须还能还原成一个字符。
    #[test]
    fn cesu8_from_java_still_reads() {
        let cesu = [0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80];
        assert_eq!(decode_utf_lenient(&cesu), "😀");
        let mut buf = Vec::new();
        write_i16(&mut buf, cesu.len() as i16).unwrap();
        buf.extend_from_slice(&cesu);
        assert_eq!(read_utf(&mut buf.as_slice()).unwrap(), "😀");
    }

    #[test]
    fn integers_are_big_endian() {
        let mut buf = Vec::new();
        write_i32(&mut buf, 0x0102_0304).unwrap();
        assert_eq!(buf, vec![1, 2, 3, 4]);
        write_i64(&mut buf, -1).unwrap();
        assert_eq!(&buf[4..], &[0xFF; 8]);
        write_i16(&mut buf, 0x0102).unwrap();
        assert_eq!(&buf[12..], &[1, 2]);
    }
}