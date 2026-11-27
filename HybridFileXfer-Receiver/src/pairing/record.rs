//! 记录层：把裸 TCP 换成"一条记录一次加密"的外壳。
//!
//! 设计要点（文档 §5.6、§5.15，改动前请先读）：
//!
//! 1. **每次 write() 封一条记录，不做缓冲。** 既有代码是直写裸 TcpStream，
//!    大量地方写完不 flush（例如 session.rs:1044-1049 连写 4 个字段都不 flush），
//!    一旦加缓冲就会在"对端等数据"的地方死锁。代价是一个 1 MiB 块被拆成约 9 条记录
//!    （8 个小字段 + 数据），额外开销 0.02%，可以接受。
//! 2. **一条记录一次 send，所以 TCP_NODELAY 必须一直开着。** 关掉的话 Nagle 会扣住
//!    后面 8 个小段等 ACK，配合对端延迟 ACK（Android 约 40ms）每块多等 300ms 量级，
//!    吞吐掉一个数量级，而现象看起来像"配对把速度弄慢了"。
//! 3. **nonce 永不复用**：同一把密钥只服务一条连接，连接一换（包括取消后重建通道）
//!    就换 csalt ⇒ 换密钥 ⇒ 计数器才允许归零。IS-5。
//! 4. **AAD 里钉住通道号和方向**：AEAD 只保证"这串字节没被改"，不保证"它属于这条连接"。
//!    不带 AAD 就能把 A 通道的密文搬到 B 通道，两端都不会报错。

use std::io::{self, Read, Write};

use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};

use super::{PairError, PairResult};

/// 记录类型：2 = 应用数据。阶段 1 只定义这一个。
pub const RECORD_TYPE_APP: u8 = 2;
/// AES-GCM 认证标签长度。
pub const TAG_LEN: usize = 16;
/// 记录头长度：1 字节类型 + 4 字节长度。
pub const HEADER_LEN: usize = 5;
/// 单条记录的最大明文字节数。
///
/// 取 1 MiB + 16：合法路径里最大的一次 write 就是一个整块（proto::BLOCK_SIZE），
/// 留 16 字节余量给"块 + 尾部小字段"这种可能的合并写。超了就判错而不是截断 ——
/// 对端送来的长度是攻击者可控的，按它分配内存等于把内存交给对面。
pub const MAX_PLAINTEXT: usize = 1024 * 1024 + 16;

/// 一条连接的一个方向所用的记录密钥。
pub struct RecordKey {
    aead: LessSafeKey,
    /// nonce 前 4 字节，由 HKDF 派生（两端一致）。
    prefix: [u8; 4],
    /// 通道号：控制通道用 0xFF，传输通道用网卡清单下标。
    cid: u8,
    /// 方向：0 = 电脑→手机，1 = 手机→电脑。
    dir: u8,
}

impl RecordKey {
    pub fn new(key: &[u8; 32], prefix: [u8; 4], cid: u8, dir: u8) -> PairResult<RecordKey> {
        let unbound = UnboundKey::new(&aead::AES_256_GCM, key).map_err(|_| PairError::BadRecord)?;
        Ok(RecordKey { aead: LessSafeKey::new(unbound), prefix, cid, dir })
    }

    fn nonce(&self, counter: u64) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&self.prefix);
        nonce[4..].copy_from_slice(&counter.to_be_bytes());
        nonce
    }

    /// AAD = {type, len, cid, dir}，len 是**密文**长度（明文 + 16）。
    fn aad(&self, cipher_len: usize) -> [u8; 7] {
        let mut aad = [0u8; 7];
        aad[0] = RECORD_TYPE_APP;
        aad[1..5].copy_from_slice(&(cipher_len as u32).to_be_bytes());
        aad[5] = self.cid;
        aad[6] = self.dir;
        aad
    }
}

fn seal_err() -> io::Error {
    io::Error::new(io::ErrorKind::Other, "记录层加密失败（几乎不可能发生）")
}

fn open_err() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "记录层解密失败（被篡改或密钥不对）")
}

/// 加密发送端：实现 Write，可以直接顶替原来的 TcpStream / &mut TcpStream。
pub struct RecordWriter<W: Write> {
    inner: W,
    key: RecordKey,
    /// 从 1 开始（文档 §5.13.9）；计数器溢出理论上到不了，但还是判错而不是回绕。
    counter: u64,
    /// 复用的明文缓冲，避免每写一条就分配一次（控制帧很小但很频繁）。
    scratch: Vec<u8>,
}

impl<W: Write> RecordWriter<W> {
    pub fn new(inner: W, key: RecordKey) -> RecordWriter<W> {
        RecordWriter { inner, key, counter: 1, scratch: Vec::new() }
    }

    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    pub fn into_inner(self) -> W {
        self.inner
    }

    /// 已发出的记录条数（调试/日志用）。
    pub fn records_sent(&self) -> u64 {
        self.counter - 1
    }
}

impl<W: Write> Write for RecordWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            // 不产出空记录：对端会把 len < TAG_LEN 当成坏帧，等于凭空制造一个错误。
            return Ok(0);
        }
        if buf.len() > MAX_PLAINTEXT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("一次写入 {} 字节超过单条记录上限 {}", buf.len(), MAX_PLAINTEXT),
            ));
        }
        let cipher_len = buf.len() + TAG_LEN;
        let header = {
            let mut h = [0u8; HEADER_LEN];
            h[0] = RECORD_TYPE_APP;
            h[1..].copy_from_slice(&(cipher_len as u32).to_be_bytes());
            h
        };
        self.scratch.clear();
        self.scratch.extend_from_slice(buf);
        let nonce = self.key.nonce(self.counter);
        let aad = self.key.aad(cipher_len);
        self.key
            .aead
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(&aad[..]),
                &mut self.scratch,
            )
            .map_err(|_| seal_err())?;

        // 头和密文分两次 write_all 会多出 5 字节的独立段，所以拼成一次写。
        let mut out = Vec::with_capacity(HEADER_LEN + cipher_len);
        out.extend_from_slice(&header);
        out.extend_from_slice(&self.scratch);
        self.inner.write_all(&out)?;

        self.counter = self
            .counter
            .checked_add(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "记录计数器溢出"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// 解密接收端：实现 Read，可以顶替原来的 BufReader / &mut TcpStream。
pub struct RecordReader<R: Read> {
    inner: R,
    key: RecordKey,
    counter: u64,
    /// 当前记录解密后的明文。
    plain: Vec<u8>,
    /// 已消费到的位置（Read 允许一次只取一部分，剩下的留着下次给）。
    pos: usize,
    /// 是否已经读到 EOF，避免反复对底层做 read_exact 拿 UnexpectedEof。
    eof: bool,
}

impl<R: Read> RecordReader<R> {
    pub fn new(inner: R, key: RecordKey) -> RecordReader<R> {
        RecordReader { inner, key, counter: 1, plain: Vec::new(), pos: 0, eof: false }
    }

    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    pub fn records_read(&self) -> u64 {
        self.counter - 1
    }

    fn next_record(&mut self) -> io::Result<()> {
        // EOF 在这里如实抛出（read_exact 报 UnexpectedEof），由 read() 统一翻译成 Ok(0)：
        // Read 的约定是"读到结尾返回 0"，返回错误会让 read_to_end 直接失败，
        // 而既有代码大量用 read_exact 读定长字段，把 EOF 变成错误会让"对方正常断开"
        // 表现成"协议错误"，日志再也没法区分。
        let mut header = [0u8; HEADER_LEN];
        self.inner.read_exact(&mut header)?;
        if header[0] != RECORD_TYPE_APP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("未知记录类型 {}", header[0]),
            ));
        }
        let cipher_len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        if cipher_len < TAG_LEN || cipher_len - TAG_LEN > MAX_PLAINTEXT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("记录长度 {} 越界", cipher_len),
            ));
        }
        let mut cipher = vec![0u8; cipher_len];
        self.inner.read_exact(&mut cipher)?;

        let nonce = self.key.nonce(self.counter);
        let aad = self.key.aad(cipher_len);
        let plain = self
            .key
            .aead
            .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(&aad[..]), &mut cipher)
            .map_err(|_| open_err())?;
        self.plain.clear();
        self.plain.extend_from_slice(plain);
        self.pos = 0;
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "记录计数器溢出"))?;
        Ok(())
    }
}

impl<R: Read> Read for RecordReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.pos >= self.plain.len() {
            if self.eof {
                return Ok(0);
            }
            match self.next_record() {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    self.eof = true;
                    return Ok(0);
                }
                Err(e) => return Err(e),
            }
        }
        let n = std::cmp::min(buf.len(), self.plain.len() - self.pos);
        buf[..n].copy_from_slice(&self.plain[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::testutil::hex;

    /// 附录 A.5 的通道密钥。
    fn kat_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        key.copy_from_slice(&hex(
            "ff51c5b3a3be350b604b536bc3d3cf3094e2586cc8222c624184a394783e7bc0",
        ));
        key
    }

    fn kat_c2s(cid: u8) -> RecordKey {
        RecordKey::new(&kat_key(), hex("f8d52559").try_into().unwrap(), cid, 0).unwrap()
    }

    fn kat_s2c(cid: u8) -> RecordKey {
        RecordKey::new(&kat_key(), hex("7c6ac1ce").try_into().unwrap(), cid, 1).unwrap()
    }

    /// 附录 A.6：一条记录的**完整线上字节**必须逐字节等于文档。
    ///
    /// 这是整条链路里唯一一条"密文可预期"的向量，两端任何一处（AAD 顺序、长度字段含义、
    /// 计数器起点、nonce 拼法）不一致都会在这里当场露出来。
    #[test]
    fn appendix_a6_record_bytes() {
        let mut out: Vec<u8> = Vec::new();
        {
            let mut w = RecordWriter::new(&mut out, kat_c2s(5));
            w.write_all(b"HFX-RECORD-KAT").unwrap();
        }
        let expected = hex("02             0000001e             66347903c3dbf49e3d2c3b00cdbcf8ade40f9d6edaaf35a027f5f886ca41");
        assert_eq!(out, expected, "记录的线上字节与附录 A.6 不一致");
        assert_eq!(out.len(), 5 + 30);
    }

    /// 往返：写进去什么，读出来必须一模一样。
    #[test]
    fn roundtrip_single_record() {
        let mut wire: Vec<u8> = Vec::new();
        // 用 \x 转义写"你好"，别直接粘贴中文：字节串字面量只许 ASCII。
        let payload = b"hello \xe4\xbd\xa0\xe5\xa5\xbd".to_vec();
        {
            let mut w = RecordWriter::new(&mut wire, kat_c2s(0));
            w.write_all(&payload).unwrap();
        }
        let mut r = RecordReader::new(&wire[..], kat_c2s(0));
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, payload);
    }

    /// 模拟真实发送顺序：一个块 = 9 次 write = 9 条记录，读回来必须严格对应。
    ///
    /// 这条守的是"每次 write 一条记录"这个约定：如果哪天有人给写端加了缓冲，
    /// 这里读到的分片数就变了，而真实协议里两端都按 read_exact 取字段，分片一变就错位。
    #[test]
    fn nine_writes_nine_records() {
        let mut wire: Vec<u8> = Vec::new();
        let data = vec![0xABu8; 4096];
        {
            let mut w = RecordWriter::new(&mut wire, kat_c2s(3));
            w.write_all(&2i16.to_be_bytes()).unwrap();
            w.write_all(&7i32.to_be_bytes()).unwrap();
            w.write_all(b"wlan0#2").unwrap();
            w.write_all(&1_700_000_000_000i64.to_be_bytes()).unwrap();
            w.write_all(&8192i64.to_be_bytes()).unwrap();
            w.write_all(&0i32.to_be_bytes()).unwrap();
            w.write_all(&(data.len() as i32).to_be_bytes()).unwrap();
            w.write_all(&0x1234_5678i32.to_be_bytes()).unwrap();
            w.write_all(&data).unwrap();
        }
        assert_eq!(count_records(&wire), 9, "9 次 write 应该是 9 条记录");
        let mut r = RecordReader::new(&wire[..], kat_c2s(3));
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(&2i16.to_be_bytes());
        expected.extend_from_slice(&7i32.to_be_bytes());
        expected.extend_from_slice(b"wlan0#2");
        expected.extend_from_slice(&1_700_000_000_000i64.to_be_bytes());
        expected.extend_from_slice(&8192i64.to_be_bytes());
        expected.extend_from_slice(&0i32.to_be_bytes());
        expected.extend_from_slice(&(data.len() as i32).to_be_bytes());
        expected.extend_from_slice(&0x1234_5678i32.to_be_bytes());
        expected.extend_from_slice(&data);
        assert_eq!(got, expected);
    }

    /// 统计线上有多少条记录（测试辅助）。
    fn count_records(mut wire: &[u8]) -> usize {
        let mut count = 0;
        while !wire.is_empty() {
            assert!(wire.len() >= HEADER_LEN);
            assert_eq!(wire[0], RECORD_TYPE_APP);
            let len = u32::from_be_bytes([wire[1], wire[2], wire[3], wire[4]]) as usize;
            wire = &wire[HEADER_LEN + len..];
            count += 1;
        }
        count
    }

    /// 只改一个密文字节，读端必须报错而不是交出一段被污染的明文。
    #[test]
    fn tampered_ciphertext_is_rejected() {
        let mut wire: Vec<u8> = Vec::new();
        {
            let mut w = RecordWriter::new(&mut wire, kat_c2s(0));
            w.write_all(b"attack at dawn").unwrap();
        }
        wire[HEADER_LEN + 2] ^= 0x01;
        let mut r = RecordReader::new(&wire[..], kat_c2s(0));
        let mut got = Vec::new();
        assert!(r.read_to_end(&mut got).is_err(), "篡改必须被发现");
    }

    /// 改记录头里的长度字段，必须报错。
    #[test]
    fn tampered_header_is_rejected() {
        let mut wire: Vec<u8> = Vec::new();
        {
            let mut w = RecordWriter::new(&mut wire, kat_c2s(0));
            w.write_all(b"abc").unwrap();
        }
        wire[4] ^= 0x01;
        let mut r = RecordReader::new(&wire[..], kat_c2s(0));
        let mut got = Vec::new();
        assert!(r.read_to_end(&mut got).is_err(), "长度被改必须被发现");
    }

    /// 跨通道搬运：把通道 5 的记录喂给通道 6 的读端，必须失败。
    /// （AAD 里钉了 cid，就是为了这个。）
    #[test]
    fn cross_channel_replay_is_rejected() {
        let mut wire: Vec<u8> = Vec::new();
        {
            let mut w = RecordWriter::new(&mut wire, kat_c2s(5));
            w.write_all(b"channel five").unwrap();
        }
        let mut r = RecordReader::new(&wire[..], kat_c2s(6));
        let mut got = Vec::new();
        assert!(r.read_to_end(&mut got).is_err(), "跨通道搬运必须被发现");
    }

    /// 跨方向搬运同样必须失败（c2s 的记录喂给 s2c 的读端）。
    #[test]
    fn cross_direction_replay_is_rejected() {
        let mut wire: Vec<u8> = Vec::new();
        {
            let mut w = RecordWriter::new(&mut wire, kat_c2s(0));
            w.write_all(b"pc to phone").unwrap();
        }
        let mut r = RecordReader::new(&wire[..], kat_s2c(0));
        let mut got = Vec::new();
        assert!(r.read_to_end(&mut got).is_err(), "跨方向搬运必须被发现");
    }

    /// 长度越界：声明 4 GiB 的记录必须立刻判错，不许按它分配内存。
    #[test]
    fn oversized_length_is_rejected_without_allocating() {
        let mut wire = Vec::new();
        wire.push(RECORD_TYPE_APP);
        wire.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        let mut r = RecordReader::new(&wire[..], kat_c2s(0));
        let mut got = Vec::new();
        assert!(r.read_to_end(&mut got).is_err());
    }

    /// 空写不产生记录（否则对端会读到 len=16 的空负载，白跑一次 AEAD）。
    #[test]
    fn empty_write_emits_nothing() {
        let mut wire: Vec<u8> = Vec::new();
        {
            let mut w = RecordWriter::new(&mut wire, kat_c2s(0));
            assert_eq!(w.write(b"").unwrap(), 0);
            assert_eq!(w.records_sent(), 0);
        }
        assert!(wire.is_empty());
    }

    /// 计数器递增：两条相同明文的记录，密文必须不同（证明 nonce 在变）。
    /// 若哪天有人把计数器写成常量，GCM 立刻变成"同密钥同 nonce"，这条会先炸。
    #[test]
    fn counter_changes_ciphertext() {
        let mut wire: Vec<u8> = Vec::new();
        {
            let mut w = RecordWriter::new(&mut wire, kat_c2s(0));
            w.write_all(b"same plaintext").unwrap();
            w.write_all(b"same plaintext").unwrap();
        }
        let first = &wire[HEADER_LEN..HEADER_LEN + 14 + TAG_LEN];
        let second_start = HEADER_LEN + 14 + TAG_LEN + HEADER_LEN;
        let second = &wire[second_start..second_start + 14 + TAG_LEN];
        assert_ne!(first, second, "两条相同明文记录密文相同 = nonce 复用了");
    }
}
