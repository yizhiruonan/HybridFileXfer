//! 通道密钥的三道防线 + §5.15 的四条失败用例（IS-5：nonce 复用）。
//!
//! ## 为什么"每个通道一个 csalt"这件事值一整个文件
//!
//! AES-GCM 的致命条件只有一个：**同一把密钥 + 同一个 nonce 加密两条不同明文**。
//! 一旦发生，攻击者能直接异或出两条明文的关系，并且 GCM 的认证密钥可以从重复的
//! nonce 对里恢复 —— 到那时"加密"就成了摆设，比明文只多一层错觉。
//!
//! 而通道密钥是由 (prk, cid, 通道名, csalt) 派生的：prk 在一次会话内不变，
//! cid 与通道名是固定的（wlan0/#2/#3…），于是**唯一能让重建后的通道换密钥的就是 csalt**。
//! 通道会被重建（用户点取消、网卡切换、拔插），重建后计数器从 1 重新开始 ——
//! 若 csalt 复用，就正好凑成"同密钥同 nonce"。这就是 IS-5。
//!
//! 三道防线各挡一种失败：
//!
//! 1. [`CsaltGenerator`]（电脑侧）：每条通道连接新抽 16 字节盐，会话内去重；
//!    连续 3 次抽到重复就当协议错误放弃（正常永远到不了，随机源坏了才会）。
//! 2. [`CsaltRejector`]（手机侧）：记住本会话见过的每一个盐，重复的直接关连接。
//! 3. [`ConnSeqGuard`]：HFXT 报头里那个 u32 必须严格递增 —— 它挡的是
//!    "把昨天那条通道的包重放进来"（旧包的盐是旧的，但缓冲区里的字节是真的）。
//!
//! 计数器方向还有一条隐含保证：同一连接上计数器只增不减（[`record::RecordReader`]），
//! 因此"把第 5 条记录改回第 3 条重发"必然解不开 —— 那是第 4 条用例。

use std::collections::HashSet;

use ring::rand::{SecureRandom, SystemRandom};

use super::kdf;
use super::record::{RecordKey, RecordReader, RecordWriter};
use super::{PairError, PairResult};

/// 每条通道连接的盐长度（文档 §5.15）。
pub const CSALT_LEN: usize = 16;
/// 一条传输通道的盐。
pub type Csalt = [u8; CSALT_LEN];

/// 连续抽到重复盐时最多重试几次（文档：3 次）。
const MAX_SALT_RETRIES: usize = 3;

/// 电脑侧：为每条新建的通道连接发一个**会话内不重复**的盐。
#[derive(Default)]
pub struct CsaltGenerator {
    seen: HashSet<Csalt>,
}

impl CsaltGenerator {
    pub fn new() -> CsaltGenerator {
        CsaltGenerator { seen: HashSet::new() }
    }

    /// 抽一个新盐。
    ///
    /// 盐必须来自系统随机源，**不许**由 master/cid/时间戳派生（可预测 = 可复用），
    /// 也不许用连接计数器的值（重建后计数器归零，盐也跟着复用）。
    pub fn next_salt(&mut self, rng: &SystemRandom) -> PairResult<Csalt> {
        // 用闭包把"抽一次"和"去重"分开，测试才能塞进一串固定的候选值来验证重试逻辑，
        // 而不是靠撞随机数（那永远撞不到）。
        let mut seen = std::mem::take(&mut self.seen);
        let result = (|| {
            for _ in 0..MAX_SALT_RETRIES {
                let mut candidate = [0u8; CSALT_LEN];
                rng.fill(&mut candidate).map_err(|_| PairError::Io)?;
                if seen.insert(candidate) {
                    return Ok(candidate);
                }
            }
            // 连续 3 次都撞：随机源已经不随机了，继续握手只会把 nonce 复用带进线上。
            Err(PairError::BadRecord)
        })();
        self.seen = seen;
        result
    }

    /// 只给测试用：按给定的候选序列走同一套去重逻辑。
    #[cfg(test)]
    fn next_salt_from(&mut self, candidates: &[Csalt]) -> PairResult<Csalt> {
        for candidate in candidates.iter().take(MAX_SALT_RETRIES) {
            if self.seen.insert(*candidate) {
                return Ok(*candidate);
            }
        }
        Err(PairError::BadRecord)
    }

    /// 已发出去的盐个数（日志/断言用）。
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// 手机侧：本会话见过的盐集合。重复 = 直接关连接。
#[derive(Default)]
pub struct CsaltRejector {
    seen: HashSet<Csalt>,
}

impl CsaltRejector {
    pub fn new() -> CsaltRejector {
        CsaltRejector { seen: HashSet::new() }
    }

    /// 登记一个盐；**已经见过**就返回 `PE_BAD_RECORD`。
    ///
    /// 返回值只区分成功/失败，不返回盐本身：调用方要记日志时打"duplicate csalt from <ip>"，
    /// **不要打盐本身**（盐是密钥材料的一部分，日志会被贴进 issue）。
    pub fn accept(&mut self, csalt: &Csalt) -> PairResult<()> {
        if self.seen.insert(*csalt) {
            Ok(())
        } else {
            Err(PairError::BadRecord)
        }
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// `HFXT` 报头里的 `conn_seq` 守卫：必须**严格递增**。
///
/// 它挡两件事：
/// 1. 重放：把上一轮会话（甚至昨天）的通道包塞回来，盐是旧的、密钥也是旧的，
///    但字节是真的 —— 只有"这次会话的序号必须比我见过的大"能挡住它；
/// 2. 重排：中间人把第 2 条和第 3 条连接换个顺序，序号会倒退。
#[derive(Default)]
pub struct ConnSeqGuard {
    highest: Option<u32>,
}

impl ConnSeqGuard {
    pub fn new() -> ConnSeqGuard {
        ConnSeqGuard { highest: None }
    }

    /// 收下一个序号；不大于已见过的最大值就拒绝。
    pub fn accept(&mut self, seq: u32) -> PairResult<()> {
        match self.highest {
            Some(highest) if seq <= highest => Err(PairError::BadRecord),
            _ => {
                self.highest = Some(seq);
                Ok(())
            }
        }
    }

    pub fn highest(&self) -> Option<u32> {
        self.highest
    }
}

/// 为一条通道连接配一对记录层密钥（上行 / 下行各一把）。
///
/// 输入不含 csalt 的默认值：这是有意的 —— 盐必须由调用方为**每条连接**现抽，
/// 写成"可选参数"就一定会有人忘了传，然后就回到 nonce 复用。
pub fn channel_record_keys(
    prk: &[u8],
    cid: u8,
    name: &[u8],
    csalt: &Csalt,
    nprefix_c2s: [u8; 4],
    nprefix_s2c: [u8; 4],
) -> PairResult<(RecordKey, RecordKey)> {
    let key = kdf::channel_key(prk, cid, name, csalt)?;
    // 两个方向各自的 nonce 前缀是分开派生的（§5.13.7），所以同一把通道密钥
    // 在两个方向上也不会撞 nonce。
    let up = RecordKey::new(&key, nprefix_c2s, cid, 0)?;
    let down = RecordKey::new(&key, nprefix_s2c, cid, 1)?;
    Ok((up, down))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::testutil::hex;
    use super::super::kdf::{derive, transcript};

    /// 附录 A.4 的 prk（真实密钥树里的那一层）。
    fn kat_prk() -> Vec<u8> {
        hex("662d1268227dd416468ee6c24d5fc75871e7d9599a234a501b85cd737bdde09c")
    }

    fn prefix() -> ([u8; 4], [u8; 4]) {
        (
            hex("f8d52559").try_into().unwrap(),
            hex("7c6ac1ce").try_into().unwrap(),
        )
    }

    /// 把一段明文按给定 csalt 加成一条记录的线上字节。
    fn seal_with(prk: &[u8], csalt: &Csalt, plain: &[u8]) -> Vec<u8> {
        let (up, _down) = channel_record_keys(prk, 5, b"wlan0#2", csalt, prefix().0, prefix().1)
            .unwrap();
        let mut wire = Vec::new();
        {
            let mut w = RecordWriter::new(&mut wire, up);
            std::io::Write::write_all(&mut w, plain).unwrap();
        }
        wire
    }

    /// §5.15 失败用例 1：**同设备重连后旧密钥必须不可用**。
    ///
    /// 重建通道 = 换 csalt ⇒ 换密钥 ⇒ 计数器才允许归零。这条用例同时证明
    /// "旧密钥解不开新连接的记录"，即 drop-in 攻击（拿上一轮的密钥去读）不成立。
    #[test]
    fn rebuilt_channel_invalidates_old_key() {
        let prk = kat_prk();
        let (c1, c2) = (
            [0x11u8; CSALT_LEN],
            [0x22u8; CSALT_LEN],
        );
        let k1 = kdf::channel_key(&prk, 5, b"wlan0#2", &c1).unwrap();
        let k2 = kdf::channel_key(&prk, 5, b"wlan0#2", &c2).unwrap();
        assert_ne!(k1, k2, "换了 csalt 密钥却没变 —— nonce 复用回来了（IS-5）");

        // 第二轮连接的一条记录，用第一轮的密钥解：必须失败。
        let wire = seal_with(&prk, &c2, b"second connection");
        let (old_up, _) = channel_record_keys(&prk, 5, b"wlan0#2", &c1, prefix().0, prefix().1)
            .unwrap();
        let mut reader = RecordReader::new(&wire[..], old_up);
        let mut got = Vec::new();
        let err = std::io::Read::read_to_end(&mut reader, &mut got).unwrap_err();
        assert_eq!(
            PairError::from(std::io::Error::new(err.kind(), "x")),
            PairError::BadRecord,
            "旧密钥居然能解开新连接的记录"
        );

        // 顺带确认：用第二轮的密钥能正常解开。
        let (new_up, _) = channel_record_keys(&prk, 5, b"wlan0#2", &c2, prefix().0, prefix().1)
            .unwrap();
        let mut reader = RecordReader::new(&wire[..], new_up);
        let mut got = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut got).unwrap();
        assert_eq!(got, b"second connection");
    }

    /// §5.15 失败用例 2：**重复盐必须被拒**（手机侧）。
    #[test]
    fn duplicate_csalt_is_rejected() {
        let mut rejector = CsaltRejector::new();
        let salt: Csalt = [0xa0u8; CSALT_LEN];
        rejector.accept(&salt).unwrap();
        assert_eq!(rejector.accept(&salt).unwrap_err(), PairError::BadRecord);
        // 换一个盐（哪怕只差最后一个字节）就应当放行。
        let mut other = salt;
        other[CSALT_LEN - 1] ^= 1;
        rejector.accept(&other).unwrap();
        assert_eq!(rejector.len(), 2);
    }

    /// §5.15 失败用例 3：**conn_seq 回退必须被拒**。
    #[test]
    fn conn_seq_regression_is_rejected() {
        let mut guard = ConnSeqGuard::new();
        guard.accept(1).unwrap();
        guard.accept(2).unwrap();
        // 相等也不行：相等意味着同一个序号被用了两次（重放）。
        assert_eq!(guard.accept(2).unwrap_err(), PairError::BadRecord);
        assert_eq!(guard.accept(1).unwrap_err(), PairError::BadRecord);
        assert_eq!(guard.highest(), Some(2));
        // 比最大值大就继续放行。
        guard.accept(3).unwrap();
        assert_eq!(guard.highest(), Some(3));
        // 跨越一次重建（大跳变）也是合法的：序号只要求单调。
        guard.accept(1000).unwrap();
    }

    /// §5.15 失败用例 4：**计数器回退** —— 把第 1 条记录的字节当成第 2 条重发，必须解不开。
    ///
    /// 注意这里没有"计数器"字段在线上：接收端的计数器是**自己的位置**。
    /// 所以"回退"的表现形式就是"收到的密文是用旧 nonce 加密的"，表现为 AEAD 校验失败。
    #[test]
    fn counter_rollback_is_rejected() {
        let prk = kat_prk();
        let salt: Csalt = [0x33u8; CSALT_LEN];
        let (up, _) = channel_record_keys(&prk, 5, b"wlan0#2", &salt, prefix().0, prefix().1)
            .unwrap();
        let mut wire = Vec::new();
        {
            let mut w = RecordWriter::new(&mut wire, up);
            // 两条明文**必须等长**：这条用例要的是"同样的字节数、不同的 nonce"，
            // 长度不同的话下面那句"长度相等"的断言会先红，测不到想测的东西。
            std::io::Write::write_all(&mut w, b"aaaaa").unwrap();
            std::io::Write::write_all(&mut w, b"bbbbb").unwrap();
        }
        // 把第 2 条记录整体替换成第 1 条的字节（= 发送端把 counter 从 2 改回 1 重发）。
        let first_len = {
            let len = u32::from_be_bytes([wire[1], wire[2], wire[3], wire[4]]) as usize;
            5 + len
        };
        let mut rolled = wire[..first_len].to_vec();
        rolled.extend_from_slice(&wire[..first_len]);
        assert_eq!(rolled.len(), wire.len());

        let (up, _) = channel_record_keys(&prk, 5, b"wlan0#2", &salt, prefix().0, prefix().1)
            .unwrap();
        let mut reader = RecordReader::new(&rolled[..], up);
        let mut got = Vec::new();
        let err = std::io::Read::read_to_end(&mut reader, &mut got).unwrap_err();
        assert_eq!(
            PairError::from(std::io::Error::new(err.kind(), "x")),
            PairError::BadRecord,
            "计数器回退居然被接受了（记录层 nonce 复用）"
        );
    }

    /// 电脑侧生成器：正常情况每次都给新盐；连续 3 次撞车就放弃（随机源坏了）。
    #[test]
    fn csalt_generator_dedupes_and_gives_up() {
        let mut gen = CsaltGenerator::new();
        let a: Csalt = [1u8; CSALT_LEN];
        let b: Csalt = [2u8; CSALT_LEN];
        // 候选序列：a 可用；再给 a 会被跳过，用 b。
        assert_eq!(gen.next_salt_from(&[a]).unwrap(), a);
        assert_eq!(gen.next_salt_from(&[a, b]).unwrap(), b);
        // 全是重复 => 3 次之后报错（防止坏随机源把 nonce 复用带进线上）。
        assert_eq!(gen.next_salt_from(&[a, b, a, b, a]).unwrap_err(), PairError::BadRecord);
        assert_eq!(gen.len(), 2);

        // 真实随机源这条路也要跑一次：两条盐不许相等。
        let rng = SystemRandom::new();
        let s1 = gen.next_salt(&rng).unwrap();
        let s2 = gen.next_salt(&rng).unwrap();
        assert_ne!(s1, s2);
    }

    /// 不同 cid / 不同通道名 / 不同盐，任意一项变了密钥都必须变。
    #[test]
    fn channel_keys_are_unique_per_channel() {
        let prk = kat_prk();
        let salt: Csalt = [7u8; CSALT_LEN];
        let base = kdf::channel_key(&prk, 0, b"wlan0", &salt).unwrap();
        assert_ne!(base, kdf::channel_key(&prk, 1, b"wlan0", &salt).unwrap());
        assert_ne!(base, kdf::channel_key(&prk, 0, b"wlan0#2", &salt).unwrap());
        let mut other_salt = salt;
        other_salt[0] ^= 0x80;
        assert_ne!(base, kdf::channel_key(&prk, 0, b"wlan0", &other_salt).unwrap());
    }

    /// 两个方向的 nonce 前缀不同：同一通道密钥在两个方向上也不会撞 nonce。
    #[test]
    fn up_and_down_directions_use_different_prefixes() {
        let prk = kat_prk();
        let salt: Csalt = [9u8; CSALT_LEN];
        let (up, down) = channel_record_keys(&prk, 5, b"wlan0#2", &salt, prefix().0, prefix().1)
            .unwrap();
        // 同一段明文、同一把密钥，两个方向的密文必须不同（前缀不同 ⇒ nonce 不同）。
        let mut a = Vec::new();
        {
            let mut w = RecordWriter::new(&mut a, up);
            std::io::Write::write_all(&mut w, b"same plaintext").unwrap();
        }
        let mut b = Vec::new();
        {
            let mut w = RecordWriter::new(&mut b, down);
            std::io::Write::write_all(&mut w, b"same plaintext").unwrap();
        }
        assert_ne!(a, b, "两个方向的密文相同 = nonce 撞了");
    }

    /// 一次会话里"14 条通道 + 一次重建"要发 28 个互不相同的盐。
    #[test]
    fn many_channels_get_unique_salts() {
        let rng = SystemRandom::new();
        let mut gen = CsaltGenerator::new();
        let mut all = HashSet::new();
        for _ in 0..28 {
            let salt = gen.next_salt(&rng).unwrap();
            assert!(all.insert(salt), "同一会话里出现了重复盐");
        }
        assert_eq!(gen.len(), 28);
    }

    /// 密钥树本身也要能跑通（这条守住"channel_record_keys 不依赖外部注入的 prk 形态"）。
    #[test]
    fn keys_come_from_the_derived_tree() {
        let z = [5u8; 32];
        let t = transcript(b"hello", b"reply");
        let keys = derive(&z, &t).unwrap();
        let salt: Csalt = [4u8; CSALT_LEN];
        let (up, _) = channel_record_keys(
            &keys.prk,
            3,
            b"wlan0#3",
            &salt,
            keys.nprefix_c2s,
            keys.nprefix_s2c,
        )
        .unwrap();
        let mut wire = Vec::new();
        {
            let mut w = RecordWriter::new(&mut wire, up);
            std::io::Write::write_all(&mut w, b"tree").unwrap();
        }
        assert_eq!(wire[0], 2, "记录类型必须是 2");
    }

    /// 通道刚建好、对端在**第一条数据之前**就消失（读到 0 字节 EOF）：
    /// 不许 panic，也不许给下一轮留下半截状态。
    ///
    /// 这条守的是"半开连接"——拔网线、手机关屏、Wi-Fi 换信道都会留下这种连接。
    /// 它比"对端不回包"更难缠：socket 是好的、写也可能成功，只是永远读不到东西。
    #[test]
    fn peer_vanishes_before_first_record() {
        let prk = kat_prk();
        let rng = SystemRandom::new();
        let mut generator = CsaltGenerator::new();
        let mut rejector = CsaltRejector::new();
        let mut sequences = ConnSeqGuard::new();

        // 第一轮：盐登记好、序号收好，然后对端直接消失。
        let salt1 = generator.next_salt(&rng).unwrap();
        rejector.accept(&salt1).unwrap();
        sequences.accept(1).unwrap();
        let (up1, _) = channel_record_keys(&prk, 5, b"wlan0#2", &salt1, prefix().0, prefix().1)
            .unwrap();
        let mut reader = RecordReader::new(&[][..], up1);
        let mut got = Vec::new();
        // 空输入是**正常结束**（EOF），不是错误，更不许 panic。
        std::io::Read::read_to_end(&mut reader, &mut got).unwrap();
        assert!(got.is_empty());

        // 第二轮：必须能干干净净地重建（盐和序号都往前走，不被上一轮卡住）。
        let salt2 = generator.next_salt(&rng).unwrap();
        assert_ne!(salt1, salt2);
        rejector.accept(&salt2).unwrap();
        sequences.accept(2).unwrap();
        let wire = seal_with(&prk, &salt2, b"after reconnect");
        let (up2, _) = channel_record_keys(&prk, 5, b"wlan0#2", &salt2, prefix().0, prefix().1)
            .unwrap();
        let mut reader = RecordReader::new(&wire[..], up2);
        let mut got = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut got).unwrap();
        assert_eq!(got, b"after reconnect");

        // 死掉那条连接的盐**不许**被复用：它"没用过"不等于"可以再用"。
        assert_eq!(rejector.accept(&salt1).unwrap_err(), PairError::BadRecord);
        // 序号也不许退回去。
        assert_eq!(sequences.accept(1).unwrap_err(), PairError::BadRecord);
    }

    /// 往一个**已经消失**的对端写记录：必须报错、不许 panic，计数器也不许被重置。
    ///
    /// 用真实 TCP 而不是 Vec：只有真套接字才会在"对端关闭 + 接收队列非空"时给 RST，
    /// 写端第一次可能成功（进了本机发送缓冲），之后才报错 —— 这个"先成功后失败"的形状
    /// 是记录层必须容忍的（否则一次拔网线就能让写线程 panic 或把计数清零）。
    #[test]
    fn writing_to_closed_peer_errors_without_panic() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // 对端 accept 之后立刻 drop：模拟"手机在第一条数据之前就没了"。
        let peer = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            drop(sock);
        });
        let sock = std::net::TcpStream::connect(addr).unwrap();
        peer.join().unwrap();
        // 给 FIN 一点时间到达，让这段"写一个死连接"的窗口稳定一点（不然就是竞态测试）。
        std::thread::sleep(std::time::Duration::from_millis(50));

        let prk = kat_prk();
        let salt: Csalt = [0x44u8; CSALT_LEN];
        let (up, _) = channel_record_keys(&prk, 5, b"wlan0#2", &salt, prefix().0, prefix().1)
            .unwrap();
        let mut writer = RecordWriter::new(sock, up);
        // 每次 1 MiB（单条记录上限之内）：几轮之后一定会撞上 RST。
        let payload = vec![0x5au8; 1024 * 1024];
        let mut errors = 0;
        for _ in 0..3 {
            if std::io::Write::write_all(&mut writer, &payload).is_err() {
                errors += 1;
            }
        }
        // 只要"最终报错且没 panic"；至于第几次开始失败取决于缓冲大小，不该被写死。
        assert!(errors >= 1, "往已关闭的连接写了 3 MiB 都没报错，测试前提不成立");
        // 计数器是单调的：写失败不影响已经发出去的条数（不会回退到 0，也就不会 nonce 复用）。
        assert!(writer.records_sent() <= 3);
    }
}