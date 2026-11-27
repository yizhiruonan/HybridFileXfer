//! P-256 临时密钥交换（ECDH），以及那个**必须**做的 32 字节归一化。
//!
//! 为什么是 P-256 而不是 X25519：Android 的 XDH 要 API 31+，本项目 minSdk 是 26；
//! 而 secp256r1 在 Java 侧是 KeyPairGenerator("EC") + KeyAgreement("ECDH") 的默认能力，
//! 零依赖。两侧唯一共同的分母就是它。
//!
//! ring 出于安全考虑**不允许**用固定私钥做 ECDH（EphemeralPrivateKey 无法从字节构造），
//! 所以附录 A.2/A.3 的固定向量只能在手机端跑完整流程；Rust 侧改为：
//! 用 normalize_z 直接吃向量里的 Z，把"归一化"这一步钉死，其余算术交给 ring。

use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey};
use ring::rand::SystemRandom;

use super::{PairError, PairResult};

/// 未压缩点长度：0x04 开头 + X(32) + Y(32)。
pub const PUBKEY_LEN: usize = 65;
/// 共享秘密（X 坐标）长度。
pub const Z_LEN: usize = 32;

/// 生成一次性的 ECDH 密钥对，返回私钥句柄与 65 字节公钥。
///
/// 私钥用 ring 的 EphemeralPrivateKey：它没有 Clone/Serialize，用完就被 agree_ephemeral 吃掉，
/// 这样就没人能把它存进文件或日志里——比"约定好不要存"可靠。
pub fn generate(rng: &SystemRandom) -> PairResult<(EphemeralPrivateKey, [u8; PUBKEY_LEN])> {
    let private = EphemeralPrivateKey::generate(&agreement::ECDH_P256, rng)
        .map_err(|_| PairError::BadRecord)?;
    let public = private.compute_public_key().map_err(|_| PairError::BadRecord)?;
    let bytes = public.as_ref();
    if bytes.len() != PUBKEY_LEN || bytes[0] != 0x04 {
        return Err(PairError::BadRecord);
    }
    let mut out = [0u8; PUBKEY_LEN];
    out.copy_from_slice(bytes);
    Ok((private, out))
}

/// 把 ECDH 出来的共享秘密归一化成定长 32 字节。
///
/// **为什么必须有这一步（本次规范里最值钱的一条校验）**：
/// Java 的 KeyAgreement.generateSecret() 在历史实现里返回**去掉前导零**的可变长数组 ——
/// 若共享点 X 的首字节恰好是 0x00，它会返回 31 字节，而 ring 永远返回 32 字节。
/// 于是两端送进 HKDF 的 IKM 差一个字节，派生出的密钥全都不同，表现出来就是"配对永远失败"。
/// 这种密钥出现的概率约 1/256，测试里几乎撞不到，一旦撞到就是某个用户"怎么都配不上"。
/// 所以这里做成**函数内部的硬校验**，不依赖调用方记得补零；文档 §11 附录 A.3 是配套向量。
///
/// 规定：len <= 32 左补零；len > 32 直接判错（截断等于悄悄换了一把密钥）。
pub fn normalize_z(raw: &[u8]) -> PairResult<[u8; Z_LEN]> {
    if raw.is_empty() || raw.len() > Z_LEN {
        return Err(PairError::BadRecord);
    }
    let mut out = [0u8; Z_LEN];
    out[Z_LEN - raw.len()..].copy_from_slice(raw);
    // 全零的共享秘密意味着对端给了个无效点（无穷远点），ECDH 里这是攻击信号，直接拒绝。
    if out.iter().all(|b| *b == 0) {
        return Err(PairError::BadSignature);
    }
    Ok(out)
}

/// 用自己的临时私钥与对方的 65 字节公钥算出共享秘密 Z。
pub fn agree(private: EphemeralPrivateKey, peer_public: &[u8]) -> PairResult<[u8; Z_LEN]> {
    if peer_public.len() != PUBKEY_LEN {
        return Err(PairError::BadSignature);
    }
    let peer = UnparsedPublicKey::new(&agreement::ECDH_P256, peer_public);
    let mut raw: Vec<u8> = Vec::with_capacity(Z_LEN);
    // 闭包的 Err 类型必须写出来：agree_ephemeral 的签名只约定了 FnOnce(&[u8]) -> Result<R, Unspecified>，
    // 光写 Ok(()) 时 rustc 推断不出 E（实测 E0282）。
    // ring 0.17 的签名是 kdf: impl FnOnce(&[u8]) -> R，返回 Result<R, Unspecified>
    // （agreement.rs:253-257）—— 闭包返回的是"值"，不是 Result。所以这里闭包返回 ()，
    // 写成 Ok(()) 会得到 Result<Result<(), _>, _>（实测 E0308），再往下就会悄悄丢掉一层错误。
    let agreed: Result<(), ring::error::Unspecified> =
        agreement::agree_ephemeral(private, &peer, |secret| {
            raw.extend_from_slice(secret);
        });
    // 到这里失败只有一种原因：对端公钥不是曲线上的合法点。当成"身份校验失败"报给用户，
    // 因为它确实等价于"对面不是我以为的那台设备"。
    agreed.map_err(|_| PairError::BadSignature)?;
    normalize_z(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::testutil::hex;

    /// 附录 A.3：Z 首字节是 0x00 的向量。
    ///
    /// 这条测试存在的唯一目的：把一个 31 字节的输入"补"回 32 字节，
    /// 并验证 32 字节原样不动、33 字节被判错。三者缺一，前导零 bug 就还会漏过去。
    #[test]
    fn appendix_a3_leading_zero_z() {
        let full = hex("004744a6be55bdc45f904dac64e4c4db5e55063dd7947c779ac5c2120f070b92");
        assert_eq!(full.len(), 32);

        // 32 字节且首字节为 00：必须一个字节都不变。
        assert_eq!(normalize_z(&full).unwrap().to_vec(), full);

        // 模拟 Java 去掉前导零后的 31 字节：补零后必须等于原值（第 0 个字节补回 00）。
        let truncated = &full[1..];
        assert_eq!(truncated.len(), 31);
        assert_eq!(normalize_z(truncated).unwrap().to_vec(), full);

        // 33 字节（前面多一个 00）：判错，不许截断。
        let mut too_long = vec![0u8];
        too_long.extend_from_slice(&full);
        assert!(normalize_z(&too_long).is_err());

        // 空与全零：判错。
        assert!(normalize_z(&[]).is_err());
        assert!(normalize_z(&[0u8; 32]).is_err());
    }

    /// 附录 A.2 的公钥编码必须是 65 字节未压缩点。
    #[test]
    fn appendix_a2_pubkey_shape() {
        let pub_a = hex(
            "0460fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb679             03fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299",
        );
        assert_eq!(pub_a.len(), PUBKEY_LEN);
        assert_eq!(pub_a[0], 0x04);
    }

    /// 功能测试：两端各生成一对，双向协商出的 Z 必须相同且 32 字节。
    #[test]
    fn two_party_agreement() {
        let rng = SystemRandom::new();

        // generate 返回的私钥是被 agree 吃掉的，所以要成对生成、交换公钥后再各自协商。
        let (priv_a, pub_a) = generate(&rng).unwrap();
        let (priv_b, pub_b) = generate(&rng).unwrap();
        assert_eq!(pub_a.len(), PUBKEY_LEN);
        assert_eq!(pub_b.len(), PUBKEY_LEN);
        assert_ne!(pub_a, pub_b, "两次生成的公钥撞了，随机源有问题");

        let z_ab = agree(priv_a, &pub_b).unwrap();
        let z_ba = agree(priv_b, &pub_a).unwrap();
        assert_eq!(z_ab, z_ba, "两端算出的共享秘密不同");
        assert_eq!(z_ab.len(), Z_LEN);
    }

    /// 用非法公钥协商必须返回错误，且不能 panic（这是会走网络的输入）。
    #[test]
    fn agree_rejects_garbage_public_key() {
        let rng = SystemRandom::new();
        let (priv_a, _) = generate(&rng).unwrap();
        // 长度对但点不在曲线上
        let mut bogus = [0x04u8; PUBKEY_LEN];
        bogus[1..].fill(0x11);
        assert!(agree(priv_a, &bogus).is_err());

        let (priv_b, _) = generate(&rng).unwrap();
        assert!(agree(priv_b, &[0x04u8; 10]).is_err(), "长度不对也必须拒");
    }
}
