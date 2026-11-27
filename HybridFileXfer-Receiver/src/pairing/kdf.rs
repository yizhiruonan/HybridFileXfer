//! 密钥派生：RFC 5869 的 HKDF-SHA256，以及本项目在它之上的那棵派生树。
//!
//! **为什么 Extract 和 Expand 都自己拼、而不是用 ring::hkdf**：
//! 1. ring 的 hkdf::Prk 拿不到原始 PRK 字节（故意不暴露），而附录 A.4 要求两端逐字节
//!    比对 PRK 本身，没有原始字节就没法断言；
//! 2. 更硬的原因：hkdf::Prk::expand 的输出长度参数必须实现 ring 的 KeyType，
//!    而 KeyType 只被实现给 Algorithm / &'static Algorithm（ring-0.17.14/src/hkdf.rs:48、
//!    aead/algorithm.rs:29），**不接受任意 usize**。本项目要 32/8/4/3 字节这些非标准长度，
//!    用不了它。
//!
//! 所以这里按 RFC 5869 的伪码逐行拼 HMAC 调用：**算术全部由 ring 的 HMAC-SHA256 完成**，
//! 我们只写了"谁拼在谁前面"。这不算自造密码学——它和 RFC 附录里的伪码是同一份东西，
//! 而且 A.1 的官方测试向量 + A.4 的项目向量会把任何一处偏差当场打出来。
//!
//! 附带好处：手机端（javax.crypto 的 Mac）**也必须**手写这 15 行，两端结构一致，
//! 差异面比"一边用库、一边手写"小得多。

use ring::hmac;

use super::{PairError, PairResult};

/// 域分隔前缀。**两端必须完全一致**：它把"配对协议的密钥"和"任何别的用途的密钥"隔开，
/// 防止将来某个功能复用了同一对密钥而互相穿透。
pub const LABEL: &[u8] = b"hfx-pair-v1";
const INFO_MASTER: &[u8] = b"hfx-pair-v1 master";
const INFO_C2S: &[u8] = b"hfx-pair-v1 c2s";
const INFO_S2C: &[u8] = b"hfx-pair-v1 s2c";
const INFO_SAS: &[u8] = b"hfx-pair-v1 sas";
const INFO_NPREFIX_C2S: &[u8] = b"hfx-pair-v1 nprefix c2s";
const INFO_NPREFIX_S2C: &[u8] = b"hfx-pair-v1 nprefix s2c";
const INFO_CID: &[u8] = b"hfx-cid";
const INFO_CH: &[u8] = b"hfx-ch";

/// RFC 5869 §2.2 的 HKDF-Extract：PRK = HMAC-SHA256(salt, IKM)。
pub fn extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    let key = hmac::Key::new(hmac::HMAC_SHA256, salt);
    let tag = hmac::sign(&key, ikm);
    let mut prk = [0u8; 32];
    prk.copy_from_slice(tag.as_ref());
    prk
}

/// RFC 5869 §2.3 的 HKDF-Expand，逐行照 RFC 伪码写。
///
/// RFC 的伪码：
///   N    = ceil(L / HashLen)
///   T(0) = 空串
///   T(i) = HMAC(PRK, T(i-1) ‖ info ‖ i)      i = 1..N
///   OKM  = T(1) ‖ T(2) ‖ … 的前 L 字节
///
/// 长度上限 255*32 是 RFC 硬规定（计数器只有一个字节）；超过它说明调用方算错了，
/// 这里判错而不是截断 —— 截断会安静地产出一把"看起来对"的短密钥。
pub fn expand(prk: &[u8], info: &[u8], len: usize) -> PairResult<Vec<u8>> {
    if len == 0 || len > 255 * 32 {
        return Err(PairError::BadRecord);
    }
    let key = hmac::Key::new(hmac::HMAC_SHA256, prk);
    let mut out: Vec<u8> = Vec::with_capacity(len + 32);
    // T(i-1)：第一轮是空串，所以用空 Vec 而不是判空标志位。
    let mut prev: Vec<u8> = Vec::new();
    let mut counter: u8 = 1;
    loop {
        let mut block: Vec<u8> = Vec::with_capacity(prev.len() + info.len() + 1);
        block.extend_from_slice(&prev);
        block.extend_from_slice(info);
        block.push(counter);
        let tag = hmac::sign(&key, &block);
        prev.clear();
        prev.extend_from_slice(tag.as_ref());
        out.extend_from_slice(&prev);
        if out.len() >= len {
            break;
        }
        // 计数器只在"还差一轮"时才加：加到第 255 块之后如果再无条件 +1，
        // u8 会溢出 —— 于是 len = 255*32（RFC 允许的最大长度）这条最边界的调用
        // 反而会报错。这个 bug 就是被 expand_rejects_bad_length 抓出来的。
        // 同时也保证 counter 永远不会回绕到 0（RFC 明确规定计数器从 1 开始）。
        counter = match counter.checked_add(1) {
            Some(next) => next,
            None => return Err(PairError::BadRecord),
        };
    }
    out.truncate(len);
    Ok(out)
}

/// 把 32 个字节的定长切片转成数组，避免调用方到处 unwrap。
fn to32(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(bytes);
    out
}

fn to4(bytes: &[u8]) -> [u8; 4] {
    let mut out = [0u8; 4];
    out.copy_from_slice(bytes);
    out
}

/// 转录哈希 T = SHA-256(LABEL ‖ 0x01 ‖ HELLO.body ‖ 0x02 ‖ REPLY.body)。
///
/// 这是全部认证的锚点（文档 IS-7）：body 必须是**上线时的那串字节**，
/// 两端都不许"解出来再重新序列化"——修改版 UTF-8 与标准 UTF-8 在 emoji 上差一截，
/// 重新序列化会让一台设备算出不同的 T，然后签名验不过，看起来像"有人在冒充"。
pub fn transcript(hello_body: &[u8], reply_body: &[u8]) -> [u8; 32] {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(LABEL);
    ctx.update(&[0x01]);
    ctx.update(hello_body);
    ctx.update(&[0x02]);
    ctx.update(reply_body);
    to32(ctx.finish().as_ref())
}

/// 一次握手派生出来的全部密钥。
#[derive(Clone, Debug)]
pub struct DerivedKeys {
    /// PRK 本身。留着是为了单测能逐字节比对 A.4，以及将来做"重连票据"。
    pub prk: [u8; 32],
    pub master: [u8; 32],
    /// 控制通道电脑→手机方向。
    pub c2s: [u8; 32],
    /// 控制通道手机→电脑方向。
    pub s2c: [u8; 32],
    /// 6 位配对码（已经补零成 6 个字符）。
    pub sas_code: String,
    pub nprefix_c2s: [u8; 4],
    pub nprefix_s2c: [u8; 4],
}

/// 由共享秘密 Z 与转录 T 派生全部密钥（文档 §5.13.7）。
pub fn derive(z: &[u8], t: &[u8]) -> PairResult<DerivedKeys> {
    let prk = extract(t, z);
    let master = to32(&expand(&prk, INFO_MASTER, 32)?);
    let c2s = to32(&expand(&prk, INFO_C2S, 32)?);
    let s2c = to32(&expand(&prk, INFO_S2C, 32)?);
    let sas_code = sas_code_from_prk(&prk)?;
    let nprefix_c2s = to4(&expand(&prk, INFO_NPREFIX_C2S, 4)?);
    let nprefix_s2c = to4(&expand(&prk, INFO_NPREFIX_S2C, 4)?);
    Ok(DerivedKeys { prk, master, c2s, s2c, sas_code, nprefix_c2s, nprefix_s2c })
}

/// 6 位配对码（文档 §5.13.8、IS-9）。
///
/// 取 SAS 原始三个字节的**前 20 位**（高位对齐）再对 10^6 取模。
/// 注意别"优化"成 (r[0] as u32 * 65536 + ...) % 1e6 之类：那样用的是低 20 位，
/// 分布会偏斜，而且两端只要有一边动了这个算式，码就永远对不上——用户只会说"它让我核对，但两边不一样"。
pub fn sas_code_from_prk(prk: &[u8]) -> PairResult<String> {
    let r = expand(prk, INFO_SAS, 3)?;
    let v20 = ((r[0] as u32) << 12) | ((r[1] as u32) << 4) | ((r[2] as u32) >> 4);
    Ok(format!("{:06}", v20 % 1_000_000))
}

/// 传输通道的密钥（文档 §5.13.7 最后两行）。
///
/// csalt 是**每条连接**新抽的 16 字节随机数：通道会被重建（取消之后电脑重连一批），
/// 若密钥只由 (cid, 通道名) 决定，重建后就是同一把密钥 + 计数器归零 = AES-GCM 的 nonce 复用，
/// 会直接泄露明文异或并毁掉认证。这是 IS-5，别去掉这个参数。
pub fn channel_key(
    prk: &[u8],
    cid: u8,
    name: &[u8],
    csalt: &[u8; 16],
) -> PairResult<[u8; 32]> {
    let mut info = Vec::with_capacity(INFO_CH.len() + 1 + 2 + name.len() + 16);
    info.extend_from_slice(INFO_CH);
    info.push(cid);
    info.extend_from_slice(&(name.len() as u16).to_be_bytes());
    info.extend_from_slice(name);
    info.extend_from_slice(csalt);
    Ok(to32(&expand(prk, &info, 32)?))
}

/// 通道的 8 字节"查找索引"，放在 HFXT 报头里给手机认领用。
///
/// 它**不是**密钥（明文传输），只是让手机不必等 100ms 就能判断"这条连接属于哪条通道"，
/// 真正的认证靠后续记录层解密是否成功。所以这里只取 8 字节，且不含 csalt（csalt 每次连接都变，
/// 放进索引就没法匹配了）。
pub fn kcommit(prk: &[u8], cid: u8, name: &[u8]) -> PairResult<[u8; 8]> {
    let mut info = Vec::with_capacity(INFO_CID.len() + 1 + 2 + name.len());
    info.extend_from_slice(INFO_CID);
    info.push(cid);
    info.extend_from_slice(&(name.len() as u16).to_be_bytes());
    info.extend_from_slice(name);
    let bytes = expand(prk, &info, 8)?;
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::testutil::hex;

    /// 附录 A.1：RFC 5869 官方测试向量。
    ///
    /// 这条必须先过，否则后面所有向量都只是在验证"我们两处犯了一样的错"。
    #[test]
    fn rfc5869_a1() {
        let ikm = hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let salt = hex("000102030405060708090a0b0c");
        let info = hex("f0f1f2f3f4f5f6f7f8f9");
        let prk = extract(&salt, &ikm);
        assert_eq!(
            prk.to_vec(),
            hex("077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5"),
            "PRK 与 RFC 5869 A.1 不一致"
        );
        let okm = expand(&prk, &info, 42).unwrap();
        assert_eq!(
            okm,
            hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf                 34007208d5b887185865"),
            "OKM 与 RFC 5869 A.1 不一致"
        );
    }

    /// 附录 A.4：转录、PRK 与整棵派生树（用 A.2 的 Z）。
    #[test]
    fn appendix_a4_key_tree() {
        let z = hex("917c981549c1437dbf0829d995bba52f96b8064157355738ac77e737ebc2a17f");
        let hello_body = hex(
            "00010001             0460fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6             7903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299             0460fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6             7903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299             000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f             000c4445534b544f502d54455354",
        );
        let reply_body = hex(
            "00010001             045bdde352eaedf5dcdd22590c240d01598cc41349abe432f5932c9ca0b311f3e6             dc795473b51e84937a465d56d8763e6c5a5d5b334e56cf81db9f46178bb67e54             045bdde352eaedf5dcdd22590c240d01598cc41349abe432f5932c9ca0b311f3e6             dc795473b51e84937a465d56d8763e6c5a5d5b334e56cf81db9f46178bb67e54             202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f             000a506978656c2d5465737400",
        );
        let t = transcript(&hello_body, &reply_body);
        assert_eq!(
            t.to_vec(),
            hex("7eb2728b1736789435516a64f91a4833ca341ca0aeb6385164e02a4f6144fb18"),
            "转录 T 不一致（多半是 body 的边界或 LABEL 错了）"
        );

        let keys = derive(&z, &t).unwrap();
        assert_eq!(
            keys.prk.to_vec(),
            hex("662d1268227dd416468ee6c24d5fc75871e7d9599a234a501b85cd737bdde09c"),
            "PRK 不一致"
        );
        assert_eq!(
            keys.master.to_vec(),
            hex("82695b4745cb5e4562aefe4e6c7c5717c3c068ce5d74486cdc42d10c0bdf482c")
        );
        assert_eq!(
            keys.c2s.to_vec(),
            hex("2285cb3287f57fc2783c008cd5ac1b68d5d3ff8575483b2ff52a80e16af5d56f")
        );
        assert_eq!(
            keys.s2c.to_vec(),
            hex("59a6f10489e0bdca4bb20572ef4a612ce8eb43acb1d557e5efea11d10fbe1e42")
        );
        assert_eq!(keys.sas_code, "030201", "6 位配对码不一致");
        assert_eq!(keys.nprefix_c2s.to_vec(), hex("f8d52559"));
        assert_eq!(keys.nprefix_s2c.to_vec(), hex("7c6ac1ce"));
    }

    /// 附录 A.5：通道密钥与 kcommit。
    #[test]
    fn appendix_a5_channel_key() {
        let prk = hex("662d1268227dd416468ee6c24d5fc75871e7d9599a234a501b85cd737bdde09c");
        let csalt = {
            let mut s = [0u8; 16];
            s.copy_from_slice(&hex("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf"));
            s
        };
        assert_eq!(
            kcommit(&prk, 5, b"wlan0#2").unwrap().to_vec(),
            hex("97fef8c0297d8e72")
        );
        assert_eq!(
            channel_key(&prk, 5, b"wlan0#2", &csalt).unwrap().to_vec(),
            hex("ff51c5b3a3be350b604b536bc3d3cf3094e2586cc8222c624184a394783e7bc0")
        );
    }

    /// IS-5：换了 csalt 就必须换密钥。这条防的是"重建通道后 nonce 复用"。
    #[test]
    fn channel_key_changes_with_csalt() {
        let prk = hex("662d1268227dd416468ee6c24d5fc75871e7d9599a234a501b85cd737bdde09c");
        let a = channel_key(&prk, 0, b"wlan0", &[1u8; 16]).unwrap();
        let b = channel_key(&prk, 0, b"wlan0", &[2u8; 16]).unwrap();
        assert_ne!(a, b, "csalt 变了密钥却没变，nonce 复用风险回来了");
    }

    /// 取位算式：v20 必须等于附录 A.4 里算出来的十进制值，否则码会偏。
    #[test]
    fn sas_bit_selection() {
        // r = fb 83 94 -> 前 20 位 = 0xFB839 >> 4 = 0xF B839? 直接用文档给的十进制值反算。
        let r = [0xfbu8, 0x83, 0x94];
        let v20 = ((r[0] as u32) << 12) | ((r[1] as u32) << 4) | ((r[2] as u32) >> 4);
        assert_eq!(v20, 1_030_201);
        assert_eq!(v20 % 1_000_000, 30_201);
        assert_eq!(format!("{:06}", v20 % 1_000_000), "030201");
    }

    /// expand 的长度守卫：0 和超 RFC 上限都必须报错，不许静默截断。
    #[test]
    fn expand_rejects_bad_length() {
        let prk = [7u8; 32];
        assert!(expand(&prk, b"x", 0).is_err());
        assert!(expand(&prk, b"x", 255 * 32 + 1).is_err());
        assert!(expand(&prk, b"x", 255 * 32).is_ok());
    }

    /// 不同 info 必须派生出不同密钥（防止哪天有人把 info 写死成常量）。
    #[test]
    fn different_infos_give_different_keys() {
        let prk = [9u8; 32];
        let a = expand(&prk, b"hfx-pair-v1 c2s", 32).unwrap();
        let b = expand(&prk, b"hfx-pair-v1 s2c", 32).unwrap();
        assert_ne!(a, b);
    }
}
