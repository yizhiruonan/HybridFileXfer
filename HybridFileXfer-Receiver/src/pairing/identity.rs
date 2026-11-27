//! 长期身份密钥：ECDSA P-256 + 指纹 + 签名/验签。
//!
//! 为什么长期密钥用 ECDSA 而不是再开一条 ECDH：握手里的 ephemeral 公钥每次会话都换，
//! 它能证明"这条连接没被中间人拆开"，但证明不了"对面就是我上次配过的那台设备"。
//! 后者必须有一把**跨会话不变**的密钥，它的指纹才是信任表里的那条记录。
//!
//! 三件事不许改：
//!
//! 1. **私钥只在内存里以 ring 的 `EcdsaKeyPair` 形式存在**，落盘的是 PKCS#8 字节，
//!    由调用方写文件；本模块不提供 `Debug`，避免哪天有人把整个结构体打进日志。
//! 2. **指纹 = SHA-256(65 字节公钥)**，不是"公钥前 32 字节"，也不是"含 DER 头的那串"。
//!    两端（Rust / Android）必须用同一份输入算，否则两边显示的指纹对不上，
//!    用户会以为"配对配错了设备"。
//! 3. **签名覆盖的是转录 T**（IS-8），不是"对方公钥"。只签公钥不绑 DH，MITM 可以把
//!    两条连接的公钥互换而签名依然有效 —— 那样配对就等于没做。

use std::fs;
use std::path::Path;

use ring::rand::SystemRandom;
use ring::signature::{self, EcdsaKeyPair, KeyPair, UnparsedPublicKey};

use super::ecdh::PUBKEY_LEN;
use super::{PairError, PairResult};

/// 签名算法：ECDSA over P-256 with SHA-256，**ASN.1/DER** 编码。
///
/// 不用 `..._FIXED_SIGNING`：那是 ring 自己的定长格式，不是 DER，
/// 而手机端 `Signature.getInstance("SHA256withECDSA")` 天生产出/接受 DER。
pub const SIGN_ALG: &signature::EcdsaSigningAlgorithm = &signature::ECDSA_P256_SHA256_ASN1_SIGNING;

/// 验签算法必须和签名算法成对（DER）。
pub const VERIFY_ALG: &signature::EcdsaVerificationAlgorithm = &signature::ECDSA_P256_SHA256_ASN1;

/// 指纹长度：SHA-256 输出 32 字节。
pub const FINGERPRINT_LEN: usize = 32;
/// 给用户看的指纹长度：文档规定"指纹前 16 字节"（32 个十六进制字符）。
///
/// 16 字节 = 128 位，抗撞能力远超"人要核对一次"的场景；给满 32 字节只会让
/// 用户在两块屏幕上找不同找得更累。
pub const FINGERPRINT_SHOW: usize = 16;

/// 本机（或对端）的长期身份。
pub struct Identity {
    key: EcdsaKeyPair,
    public: [u8; PUBKEY_LEN],
    fingerprint: [u8; FINGERPRINT_LEN],
}

impl Identity {
    /// 新生成一对身份密钥，同时返回 PKCS#8 字节（调用方负责落盘）。
    ///
    /// 返回 DER 而不是让调用方去取：ring 的 key 拿不回私钥字节（这是它的设计），
    /// 只有在生成的那一刻才拿得到 —— 所以"忘记保存"这种 bug 会在这里暴露。
    pub fn generate(rng: &SystemRandom) -> PairResult<(Identity, Vec<u8>)> {
        let doc = EcdsaKeyPair::generate_pkcs8(SIGN_ALG, rng).map_err(|_| PairError::Io)?;
        let der = doc.as_ref().to_vec();
        let key = EcdsaKeyPair::from_pkcs8(SIGN_ALG, &der, rng).map_err(|_| PairError::Io)?;
        let identity = Identity::wrap(key)?;
        Ok((identity, der))
    }

    /// 从 PKCS#8 字节恢复身份。
    pub fn from_pkcs8(der: &[u8], rng: &SystemRandom) -> PairResult<Identity> {
        let key = EcdsaKeyPair::from_pkcs8(SIGN_ALG, der, rng).map_err(|_| PairError::Io)?;
        Identity::wrap(key)
    }

    fn wrap(key: EcdsaKeyPair) -> PairResult<Identity> {
        let raw = key.public_key().as_ref();
        if raw.len() != PUBKEY_LEN || raw[0] != 0x04 {
            // ring 的 P-256 公钥永远是 65 字节未压缩点；走到这里说明算法常量被换错了。
            return Err(PairError::Io);
        }
        let mut public = [0u8; PUBKEY_LEN];
        public.copy_from_slice(raw);
        let fingerprint = fingerprint(&public)?;
        Ok(Identity { key, public, fingerprint })
    }

    /// 65 字节未压缩公钥（线上/指纹的输入）。
    pub fn public(&self) -> &[u8; PUBKEY_LEN] {
        &self.public
    }

    /// SHA-256 指纹（进信任表的键）。
    pub fn fingerprint(&self) -> &[u8; FINGERPRINT_LEN] {
        &self.fingerprint
    }

    /// 界面上显示的那 16 字节指纹（32 个十六进制字符）。
    pub fn fingerprint_text(&self) -> String {
        fingerprint_text(&self.fingerprint)
    }

    /// 对转录 T 签名，返回 DER 字节。
    ///
    /// 长度不在这里强制 70~72：P-256 的 r（或 s）若恰好小于 2^248，DER 会合法地缩到
    /// 69 字节（概率约 1/256）。线上只用 `SIG_MAX` 作上界，把"长度看起来不对"
    /// 当成失败会变成每 256 次配对挂一次的偶发 bug。
    pub fn sign(&self, message: &[u8], rng: &SystemRandom) -> PairResult<Vec<u8>> {
        let sig = self.key.sign(rng, message).map_err(|_| PairError::BadSignature)?;
        let der = sig.as_ref().to_vec();
        if der.is_empty() || der.len() > super::message::SIG_MAX {
            return Err(PairError::BadRecord);
        }
        Ok(der)
    }
}

/// 由 65 字节公钥算指纹。
pub fn fingerprint(public: &[u8]) -> PairResult<[u8; FINGERPRINT_LEN]> {
    if public.len() != PUBKEY_LEN {
        return Err(PairError::BadRecord);
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, public);
    let mut out = [0u8; FINGERPRINT_LEN];
    out.copy_from_slice(digest.as_ref());
    Ok(out)
}

/// 指纹的显示形式：前 16 字节、小写十六进制、不分隔。
pub fn fingerprint_text(fingerprint: &[u8]) -> String {
    let take = std::cmp::min(fingerprint.len(), FINGERPRINT_SHOW);
    let mut out = String::with_capacity(take * 2);
    for b in &fingerprint[..take] {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// 用对方的长期公钥验签。
///
/// 失败一律 `BadSignature`：公钥不是曲线上的点、DER 畸形、签名内容不对，
/// 对用户来说是同一件事 —— "对面不是我以为的那台设备"。区分它们只会让日志更花，
/// 不会让任何人做出不同的决定。
pub fn verify(public: &[u8], message: &[u8], signature: &[u8]) -> PairResult<()> {
    if public.len() != PUBKEY_LEN || public[0] != 0x04 {
        return Err(PairError::BadSignature);
    }
    if signature.is_empty() || signature.len() > super::message::SIG_MAX {
        return Err(PairError::BadSignature);
    }
    let key = UnparsedPublicKey::new(VERIFY_ALG, public);
    key.verify(message, signature).map_err(|_| PairError::BadSignature)
}

/// 身份密钥文件：读写 PKCS#8 字节。
///
/// 为什么**不**把这个文件放进 `config.ini`：配置是给人手改的（编辑器会重排、
/// 会被同步盘合并冲突），而私钥一旦被"编辑"就永久损坏。分开存还有一个好处：
/// 用户在界面上"重置身份"时可以只删这一个文件，不动接收目录与端口设置。
pub fn load(path: &Path) -> PairResult<Identity> {
    let der = fs::read(path).map_err(|_| PairError::Io)?;
    let rng = SystemRandom::new();
    Identity::from_pkcs8(&der, &rng)
}

/// 原子地保存身份密钥（先写 `.tmp` 再 rename）。
///
/// 非原子写的坏处在这个文件上尤其致命：写到一半断电 = 文件半截 = 下次启动解不出来
/// = 身份重置 = 所有已配对设备全部失效（用户看到的是"它突然不认识我的手机了"）。
/// 先写临时文件再 rename，最坏情况是"新身份没写成"，旧身份仍然完好。
pub fn save(path: &Path, pkcs8: &[u8]) -> PairResult<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, pkcs8).map_err(|_| PairError::Io)?;
    fs::rename(&tmp, path).map_err(|_| PairError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::testutil::hex;
    use crate::pairing::{kdf, PairError};

    fn rng() -> SystemRandom {
        SystemRandom::new()
    }

    /// 附录 A.4 的转录 T（签名/验签用它的原因：那就是线上真正要签的东西）。
    fn kat_transcript() -> Vec<u8> {
        hex("7eb2728b1736789435516a64f91a4833ca341ca0aeb6385164e02a4f6144fb18")
    }

    /// 附录 A.7 第 1 条：自己签的必须自己验得过。
    #[test]
    fn a7_sign_then_verify() {
        let (id, der) = Identity::generate(&rng()).unwrap();
        assert!(!der.is_empty(), "PKCS#8 字节不该是空的");
        let t = kat_transcript();
        let sig = id.sign(&t, &rng()).unwrap();
        verify(id.public(), &t, &sig).unwrap();

        // DER 长度：文档写 70~72。数学上 69 也可能出现（r 或 s 的高位字节被
        // 最短编码省掉，概率约 1/256），所以断言区间取 69~72，免得这条测试
        // 每 256 次跑挂一次 —— 那种偶发红比没有测试更糟。
        assert!(
            (69..=72).contains(&sig.len()),
            "DER 签名长度异常：{}",
            sig.len()
        );
        assert_eq!(sig[0], 0x30, "DER SEQUENCE 首字节必须是 0x30");
    }

    /// 附录 A.7 第 2 条：改 T 的任意一个字节，验签必须失败。
    #[test]
    fn a7_flipped_transcript_fails() {
        let (id, _) = Identity::generate(&rng()).unwrap();
        let mut t = kat_transcript();
        let sig = id.sign(&t, &rng()).unwrap();
        for i in [0usize, 7, 31] {
            let saved = t[i];
            t[i] ^= 0x01;
            assert_eq!(
                verify(id.public(), &t, &sig).unwrap_err(),
                PairError::BadSignature,
                "翻转 T[{}] 后验签仍然通过",
                i
            );
            t[i] = saved;
        }
    }

    /// 附录 A.7 第 3 条：拿另一把公钥验签必须失败（防"拿错公钥也能过"）。
    #[test]
    fn a7_wrong_public_key_fails() {
        let (alice, _) = Identity::generate(&rng()).unwrap();
        let (bob, _) = Identity::generate(&rng()).unwrap();
        let t = kat_transcript();
        let sig = alice.sign(&t, &rng()).unwrap();
        verify(alice.public(), &t, &sig).unwrap();
        assert_eq!(
            verify(bob.public(), &t, &sig).unwrap_err(),
            PairError::BadSignature,
            "用别人的公钥验签居然通过了"
        );
    }

    /// 附录 A.7 第 4 条：畸形 DER（69 字节垃圾）必须被拒且不 panic。
    #[test]
    fn a7_malformed_der_is_rejected() {
        let (id, _) = Identity::generate(&rng()).unwrap();
        let t = kat_transcript();
        for bad in [
            vec![0x30u8; 69],                         // 长度对，内容不是 DER
            vec![],                                   // 空
            vec![0x30, 0x00],                         // 结构合法但内容为空
            (0..=80u8).collect::<Vec<_>>(),            // 81 字节，超过 SIG_MAX
        ] {
            let err = verify(id.public(), &t, &bad).unwrap_err();
            assert_eq!(err, PairError::BadSignature, "畸形签名 {} 应该被拒", bad.len());
        }
        // 公钥本身畸形同样要拒（不是曲线上的点、长度不对）。
        assert_eq!(verify(&[0x02u8; 65], &t, &[0x30u8; 70]).unwrap_err(), PairError::BadSignature);
        assert_eq!(verify(&[0x04u8; 10], &t, &[0x30u8; 70]).unwrap_err(), PairError::BadSignature);
    }

    /// 指纹 = SHA-256(65 字节公钥)：两端必须用同一份输入。
    #[test]
    fn fingerprint_is_sha256_of_public_key() {
        let (id, _) = Identity::generate(&rng()).unwrap();
        let expected = ring::digest::digest(&ring::digest::SHA256, id.public());
        assert_eq!(id.fingerprint().as_ref(), expected.as_ref());
        // 显示形式：32 个小写十六进制字符 = 前 16 字节。
        let text = id.fingerprint_text();
        assert_eq!(text.len(), FINGERPRINT_SHOW * 2);
        assert!(text.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(&text, &fingerprint_text(id.fingerprint()));
    }

    /// 指纹对 65 字节长度以外的输入必须判错（防止有人拿"DER 公钥"去算指纹）。
    #[test]
    fn fingerprint_rejects_wrong_length() {
        assert_eq!(fingerprint(&[0x04u8; 64]).unwrap_err(), PairError::BadRecord);
        assert_eq!(fingerprint(&[]).unwrap_err(), PairError::BadRecord);
    }

    /// PKCS#8 往返：存了再读回来，公钥与指纹必须一模一样。
    ///
    /// 这条守的是"换机/重装之后已配对设备还能连上"这件事的前提：
    /// 身份一旦变形，所有指纹都对不上，用户看到的是"全部设备都要重新配对"。
    #[test]
    fn pkcs8_roundtrip_keeps_identity() {
        let (id, der) = Identity::generate(&rng()).unwrap();
        let back = Identity::from_pkcs8(&der, &rng()).unwrap();
        assert_eq!(back.public(), id.public());
        assert_eq!(back.fingerprint(), id.fingerprint());

        // 读回来的密钥也得能签、而且验得过。
        let t = kat_transcript();
        let sig = back.sign(&t, &rng()).unwrap();
        verify(id.public(), &t, &sig).unwrap();
    }

    /// 保存/读取走真实文件（临时目录），并且要原子：目录里不能残留 .tmp。
    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("hfx-pair-id-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("identity.key");

        let (id, der) = Identity::generate(&rng()).unwrap();
        save(&path, &der).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.public(), id.public());
        assert!(!path.with_extension("tmp").exists(), "临时文件没被 rename 掉");

        // 半截文件（模拟写盘中途断电）必须报错而不是 panic。
        // 用 matches! 而不是 unwrap_err()：Identity 里裹着 ring 的密钥句柄，
        // 它故意不实现 Debug，所以 unwrap_err() 编不过 —— 这是好事，别为了测试去 derive。
        fs::write(&path, &der[..der.len() / 2]).unwrap();
        assert!(matches!(load(&path), Err(PairError::Io)), "半截密钥文件必须报错");
        let _ = fs::remove_dir_all(&dir);
    }

    /// IS-8：签名必须覆盖 T，而不是"对方的公钥"。
    ///
    /// 做法：两把身份各自签同一个 T，互换公钥去验 —— 都不通过。
    /// 再加上"把 T 换成对方的公钥字节"也验不过，说明实现里签的确实是 T。
    #[test]
    fn is8_signature_covers_transcript_not_public_key() {
        let (pc, _) = Identity::generate(&rng()).unwrap();
        let (phone, _) = Identity::generate(&rng()).unwrap();
        let t = kat_transcript();
        let sig = pc.sign(&t, &rng()).unwrap();

        // 用"手机的公钥字节"当被签内容去验 pc 的签名 —— 必须失败。
        assert_eq!(
            verify(pc.public(), phone.public(), &sig).unwrap_err(),
            PairError::BadSignature
        );
        // 反过来也不能蒙混：签名对 T 有效，两把公钥都试一遍。
        verify(pc.public(), &t, &sig).unwrap();
        assert!(verify(phone.public(), &t, &sig).is_err());

        // 转录本身也得能走到密钥树（防止有人把 T 当成"可有可无的装饰"）。
        let keys = kdf::derive(&[7u8; 32], &t).unwrap();
        assert_eq!(keys.sas_code.len(), 6);
    }
}
