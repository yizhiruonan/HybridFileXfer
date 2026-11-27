//! 安全配对（PAIR）：密码学原语、记录层、握手状态机。
//!
//! 设计文档在仓库外（仓库里不放文档）：`_worklog/dev-docs/PAIRING-PLAN.md`，
//! 其中 §5.13~§5.19 是**字节级**规范，§11 附录 A 是 KAT 测试向量。
//!
//! 三条不许破的规矩：
//! 1. 本模块只实现文档写死的算法，不许"顺手改进"：另一端是 Kotlin 手写的实现，
//!    任何一处的差异（多一个字节、换一个 info 字符串、改用 UTF-8 而不是修改版 UTF-8）
//!    都会让两端算出不同的密钥，而现象只是"握手失败"，极难归因。
//! 2. 每个原语都有 KAT（附录 A），**先跑通 KAT 再写协议流程**。
//! 3. 整个功能挂在 `ENABLED` 开关后面（`Config::secure_pairing`，默认关）。
//!    关闭时协议行为与 3.7.1（`VERSION_CODE = 309`）完全一致，
//!    这样真机上一旦出问题，改配置就能退回，不用回滚代码。

pub mod ecdh;
pub mod identity;
pub mod kdf;
pub mod message;
pub mod record;
pub mod trust;

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

/// 总开关：启动时由 `Config::secure_pairing` 写入，默认关闭。
///
/// 用 `AtomicBool` 而不是读 `Config`：这个开关会被握手路径上的多个线程读，
/// 而 `Config` 是克隆着到处传的，读一份可能过期的副本比读一个全局原子量更容易错。
static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 配对/记录层的错误码，与手机端一一对应（文档 §5.8）。
///
/// 编号就是上线字节：两端都按这张表写进 `PAIR_ABORT`，所以**只许追加，不许插队**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairError {
    /// 0：仅用于本机 IO/内部错误，从不上线。
    Io,
    Timeout,
    UserDeclined,
    BadSignature,
    UnknownDevice,
    SasMismatch,
    Revoked,
    TooManyAttempts,
    BadRecord,
    Version,
}

impl PairError {
    /// 上线用的错误码（文档 §5.8 的表）。
    pub fn code(self) -> u8 {
        match self {
            PairError::Io => 0,
            PairError::Timeout => 1,
            PairError::UserDeclined => 2,
            PairError::BadSignature => 3,
            PairError::UnknownDevice => 4,
            PairError::SasMismatch => 5,
            PairError::Revoked => 6,
            PairError::TooManyAttempts => 7,
            PairError::BadRecord => 8,
            PairError::Version => 9,
        }
    }

    /// 给用户看的一句话（中文；英文文案在 i18n 里）。
    pub fn msg_zh(self) -> &'static str {
        match self {
            PairError::Io => "本机读写错误",
            PairError::Timeout => "配对超时，请重新连接",
            PairError::UserDeclined => "已取消配对",
            PairError::BadSignature => "对方身份校验失败（可能有人在冒充）",
            PairError::UnknownDevice => "未完成配对",
            PairError::SasMismatch => "两台设备显示的码不一样，可能有人在中间拦截",
            PairError::Revoked => "这台设备已被移出信任列表，请重新配对",
            PairError::TooManyAttempts => "尝试次数过多，请 1 分钟后重试",
            PairError::BadRecord => "通信被篡改，连接已断开",
            PairError::Version => "版本不一致，请把两端都升级到最新版",
        }
    }

    /// 反解上线错误码；未知码一律当 `BadRecord`（宁可断开，也不要"猜一个能继续的语义"）。
    pub fn from_code(code: u8) -> PairError {
        match code {
            1 => PairError::Timeout,
            2 => PairError::UserDeclined,
            3 => PairError::BadSignature,
            4 => PairError::UnknownDevice,
            5 => PairError::SasMismatch,
            6 => PairError::Revoked,
            7 => PairError::TooManyAttempts,
            9 => PairError::Version,
            _ => PairError::BadRecord,
        }
    }
}

impl fmt::Display for PairError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.msg_zh())
    }
}

impl std::error::Error for PairError {}

impl From<std::io::Error> for PairError {
    fn from(_: std::io::Error) -> Self {
        PairError::Io
    }
}

pub type PairResult<T> = Result<T, PairError>;

#[cfg(test)]
pub(crate) mod testutil {
    /// 把附录里的十六进制串还原成字节；空格和换行会被忽略，方便直接粘文档里的向量。
    pub fn hex(s: &str) -> Vec<u8> {
        let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(clean.len() % 2 == 0, "十六进制串长度必须是偶数: {}", clean.len());
        let bytes = clean.as_bytes();
        let mut out = Vec::with_capacity(bytes.len() / 2);
        let val = |c: u8| -> u8 {
            match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => panic!("非十六进制字符: {}", c as char),
            }
        };
        for pair in bytes.chunks(2) {
            out.push((val(pair[0]) << 4) | val(pair[1]));
        }
        out
    }
}
