//! 信任表：哪些设备的身份指纹被这台电脑接受过。
//!
//! 三条设计取舍（都不是随手定的）：
//!
//! 1. **键是指纹，不是 IP、不是设备名、不是 adb serial**。前两个手机会变（换 Wi-Fi、
//!    改主机名），serial 只在插线时才有；而指纹是长期身份公钥的 SHA-256，换不了。
//!    同一个设备名出现在两台设备上是完全正常的（两台 Pixel 都叫 Pixel），用名字当键
//!    会让"另一台手机"直接继承信任。
//! 2. **单独一个文件，不塞进 config.ini**：配置是给人手改的（会被同步盘合并、被编辑器
//!    重排），信任表是机器数据，混在一起迟早被改坏；而且"解除配对"应该是删一条记录，
//!    不该冒险动到端口/接收目录那些设置。
//! 3. **原子写 + 坏文件不静默丢**：写到一半断电不能变成"信任表空了"（那会让所有设备
//!    都要重新配对，而用户完全不知道为什么）。解析失败时把坏文件改名留证、按空表继续，
//!    这样用户还能看到磁盘上有个 `.corrupt` 文件可查。

use std::fs;
use std::path::{Path, PathBuf};

use super::identity::{FINGERPRINT_LEN, FINGERPRINT_SHOW};
use super::{PairError, PairResult};

/// 文件格式版本。以后加字段时靠它判断"这份文件是谁写的"。
const FORMAT_VERSION: u64 = 1;

/// 一条被信任的设备。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedDevice {
    /// 长期身份公钥的 SHA-256（32 字节）。
    pub fingerprint: [u8; FINGERPRINT_LEN],
    /// 最后一次见到的显示名（纯展示用，**不参与任何判断**）。
    pub name: String,
    /// 首次配对时间（Unix 秒）。0 = 老文件里没记。
    pub paired_at: u64,
    /// 最后连接时间（Unix 秒）。
    pub last_seen: u64,
}

impl TrustedDevice {
    /// 给界面看的一句话：名字 + 指纹前 16 字节。
    pub fn display(&self) -> String {
        format!("{} ({})", self.name, super::identity::fingerprint_text(&self.fingerprint))
    }
}

/// 信任表本身。全部在内存里，`save` 才落盘 —— 握手过程中绝不写盘：
/// 一次失败的握手不该在磁盘上留下任何痕迹（文档 §5.14.3：配对失败/超时绝不写信任表，
/// 也不留"半配对"）。
#[derive(Clone, Debug, Default)]
pub struct TrustStore {
    devices: Vec<TrustedDevice>,
    /// 这张表是从哪个文件读来的（`save` 用它；空表示纯内存，测试用）。
    path: Option<PathBuf>,
}

impl TrustStore {
    /// 空表（还没配过任何设备）。
    pub fn empty() -> TrustStore {
        TrustStore { devices: Vec::new(), path: None }
    }

    /// 从文件读；文件不存在 = 空表（首次运行是正常状态，不是错误）。
    pub fn load(path: &Path) -> TrustStore {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => return TrustStore { devices: Vec::new(), path: Some(path.to_path_buf()) },
        };
        match parse(&text) {
            Ok(devices) => TrustStore { devices, path: Some(path.to_path_buf()) },
            Err(_) => {
                // 解析不了的信任表**不能**当成空表继续用还不留痕迹：那样用户会以为
                // "它自己把我的手机忘了"。改名留证，新表重新开始，旧的还能人工救回来。
                let broken = path.with_extension("corrupt");
                let _ = fs::rename(path, &broken);
                TrustStore { devices: Vec::new(), path: Some(path.to_path_buf()) }
            }
        }
    }

    /// 换个保存路径（`load` 之后想"另存"时用；正常路径不需要）。
    pub fn with_path(mut self, path: PathBuf) -> TrustStore {
        self.path = Some(path);
        self
    }

    pub fn is_trusted(&self, fingerprint: &[u8; FINGERPRINT_LEN]) -> bool {
        self.devices.iter().any(|d| &d.fingerprint == fingerprint)
    }

    pub fn get(&self, fingerprint: &[u8; FINGERPRINT_LEN]) -> Option<&TrustedDevice> {
        self.devices.iter().find(|d| &d.fingerprint == fingerprint)
    }

    pub fn devices(&self) -> &[TrustedDevice] {
        &self.devices
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// 加一条（或更新已有的那条的名字/时间）。返回是否**新增**。
    ///
    /// 同一个指纹重复加不会产生第二条：指纹是主键，去重放在这里而不是让每个调用方
    /// 自己先 `is_trusted` 一下（少一个能被忘掉的步骤）。
    pub fn add(&mut self, fingerprint: [u8; FINGERPRINT_LEN], name: &str, now: u64) -> bool {
        if let Some(existing) = self.devices.iter_mut().find(|d| d.fingerprint == fingerprint) {
            // 名字是展示用的，允许跟着对端改；时间只往前走，避免时钟回拨把记录改旧。
            existing.name = name.to_string();
            existing.last_seen = existing.last_seen.max(now);
            return false;
        }
        self.devices.push(TrustedDevice {
            fingerprint,
            name: name.to_string(),
            paired_at: now,
            last_seen: now,
        });
        true
    }

    /// 只更新"最后见到"的时间（已配对设备重连成功时调用）。
    pub fn touch(&mut self, fingerprint: &[u8; FINGERPRINT_LEN], now: u64) -> bool {
        match self.devices.iter_mut().find(|d| &d.fingerprint == fingerprint) {
            Some(d) => {
                d.last_seen = d.last_seen.max(now);
                true
            }
            None => false,
        }
    }

    /// 解除配对。返回是否真的删掉了。
    pub fn remove(&mut self, fingerprint: &[u8; FINGERPRINT_LEN]) -> bool {
        let before = self.devices.len();
        self.devices.retain(|d| &d.fingerprint != fingerprint);
        self.devices.len() != before
    }

    /// 落盘（原子：写 `.tmp` 再 rename）。
    pub fn save(&self) -> PairResult<()> {
        match &self.path {
            Some(path) => save_to(path, &self.devices),
            // 纯内存表没有路径：当成编程错误报出来，而不是悄悄丢掉用户刚配好的设备。
            None => Err(PairError::Io),
        }
    }

    /// 序列化成 JSON 文本（测试与 `save` 共用）。
    pub fn to_json(&self) -> String {
        let items: Vec<String> = self
            .devices
            .iter()
            .map(|d| {
                // 名字可能含引号/反斜杠/emoji：用 serde_json 转义，别自己拼。
                let name = serde_json::to_string(&d.name).unwrap_or_else(|_| "\"\"".to_string());
                format!(
                    "{{\"fp\":\"{}\",\"name\":{},\"paired_at\":{},\"last_seen\":{}}}",
                    hex_lower(&d.fingerprint),
                    name,
                    d.paired_at,
                    d.last_seen
                )
            })
            .collect();
        format!(
            "{{\"version\":{},\"devices\":[{}]}}",
            FORMAT_VERSION,
            items.join(",")
        )
    }

    /// 指纹的显示形式（给日志用）。
    pub fn fingerprint_hint(&self, fingerprint: &[u8; FINGERPRINT_LEN]) -> String {
        super::identity::fingerprint_text(fingerprint)
    }
}

fn save_to(path: &Path, devices: &[TrustedDevice]) -> PairResult<()> {
    let store = TrustStore { devices: devices.to_vec(), path: None };
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, store.to_json()).map_err(|_| PairError::Io)?;
    fs::rename(&tmp, path).map_err(|_| PairError::Io)?;
    Ok(())
}

fn parse(text: &str) -> PairResult<Vec<TrustedDevice>> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| PairError::BadRecord)?;
    let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
    if version != FORMAT_VERSION {
        // 未来版本的格式不猜：猜错会静默丢掉条目，用户看到的是"配对突然没了"。
        return Err(PairError::Version);
    }
    let list = value
        .get("devices")
        .and_then(|v| v.as_array())
        .ok_or(PairError::BadRecord)?;
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        let fp_text = item.get("fp").and_then(|v| v.as_str()).ok_or(PairError::BadRecord)?;
        let fp = parse_hex32(fp_text)?;
        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let paired_at = item.get("paired_at").and_then(|v| v.as_u64()).unwrap_or(0);
        let last_seen = item.get("last_seen").and_then(|v| v.as_u64()).unwrap_or(0);
        out.push(TrustedDevice { fingerprint: fp, name, paired_at, last_seen });
    }
    Ok(out)
}

fn parse_hex32(text: &str) -> PairResult<[u8; FINGERPRINT_LEN]> {
    if text.len() != FINGERPRINT_LEN * 2 {
        return Err(PairError::BadRecord);
    }
    let bytes = text.as_bytes();
    let mut out = [0u8; FINGERPRINT_LEN];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_val(bytes[i * 2])?;
        let lo = hex_val(bytes[i * 2 + 1])?;
        *slot = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_val(c: u8) -> PairResult<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(PairError::BadRecord),
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(n: u8) -> [u8; FINGERPRINT_LEN] {
        let mut out = [0u8; FINGERPRINT_LEN];
        out[0] = n;
        out[FINGERPRINT_LEN - 1] = n;
        out
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hfx-trust-{}-{}", std::process::id(), tag));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    /// 增删查改 + 同一个指纹不许有两条。
    #[test]
    fn add_dedupe_remove() {
        let mut store = TrustStore::empty();
        assert!(store.is_empty());
        assert!(store.add(fp(1), "Pixel-9", 100));
        assert!(!store.is_trusted(&fp(2)));
        assert!(store.is_trusted(&fp(1)));
        assert_eq!(store.len(), 1);

        // 重复加：是更新不是新增，配对时间不能被后来的 touched 时间顶掉。
        assert!(!store.add(fp(1), "Pixel-9-改名", 200));
        assert_eq!(store.len(), 1);
        let d = store.get(&fp(1)).unwrap();
        assert_eq!(d.name, "Pixel-9-改名");
        assert_eq!(d.paired_at, 100, "首次配对时间被覆盖了");
        assert_eq!(d.last_seen, 200);

        // touch 只动 last_seen，且不许把时间改旧（时钟回拨）。
        assert!(store.touch(&fp(1), 50));
        assert_eq!(store.get(&fp(1)).unwrap().last_seen, 200);
        assert!(!store.touch(&fp(9), 10), "不存在的设备不该被 touch 出来");

        assert!(store.remove(&fp(1)));
        assert!(!store.remove(&fp(1)));
        assert!(store.is_empty());
    }

    /// JSON 往返：名字里的引号/反斜杠/emoji 不能破坏格式。
    #[test]
    fn json_roundtrip_keeps_names() {
        let mut store = TrustStore::empty();
        store.add(fp(3), "Pixel \"9\" \\ 手机 🚀", 7);
        let text = store.to_json();
        let back = parse(&text).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].fingerprint, fp(3));
        assert_eq!(back[0].name, "Pixel \"9\" \\ 手机 🚀");
        assert_eq!(back[0].paired_at, 7);
    }

    /// 保存/读取走真实文件；空表也能存（"解除全部配对"之后要能落盘）。
    #[test]
    fn save_and_load() {
        let dir = temp_dir("save");
        let path = dir.join("trusted.json");

        let mut store = TrustStore::load(&path);
        assert!(store.is_empty(), "文件不存在时应该是空表");
        store.add(fp(4), "Pixel-Test", 1_700_000_000);
        store.save().unwrap();

        let back = TrustStore::load(&path);
        assert_eq!(back.len(), 1);
        assert!(back.is_trusted(&fp(4)));
        assert!(!path.with_extension("tmp").exists(), "临时文件没被 rename 掉");

        let mut back = back;
        back.remove(&fp(4));
        back.save().unwrap();
        assert!(TrustStore::load(&path).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// 坏文件不许静默丢：改名成 .corrupt 留证，然后按空表继续。
    #[test]
    fn corrupt_file_is_quarantined() {
        let dir = temp_dir("corrupt");
        let path = dir.join("trusted.json");
        fs::write(&path, "{ 这不是 JSON").unwrap();

        let store = TrustStore::load(&path);
        assert!(store.is_empty());
        assert!(!path.exists(), "坏文件应该被改名");
        assert!(path.with_extension("corrupt").exists(), "坏文件应该被留下证据");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 未来版本号不猜：报 Version，由调用方决定（而不是静默丢弃条目）。
    #[test]
    fn future_format_version_is_rejected() {
        let text = format!("{{\"version\":{},\"devices\":[]}}", FORMAT_VERSION + 1);
        assert_eq!(parse(&text).unwrap_err(), PairError::Version);
    }

    /// 指纹字段必须正好 64 个十六进制字符，非十六进制字符要拒。
    #[test]
    fn malformed_fingerprint_is_rejected() {
        assert_eq!(parse_hex32("zz").unwrap_err(), PairError::BadRecord);
        let short = "ab".repeat(FINGERPRINT_LEN - 1);
        assert_eq!(parse_hex32(&short).unwrap_err(), PairError::BadRecord);
        let ok = "cd".repeat(FINGERPRINT_LEN);
        assert_eq!(parse_hex32(&ok).unwrap()[0], 0xcd);
        // 未加引号的 fp 一律当坏文件。
        let bad = "{\"version\":1,\"devices\":[{\"fp\":123}]}";
        assert_eq!(parse(bad).unwrap_err(), PairError::BadRecord);
    }

    /// 显示形式：名字 + 指纹前 16 字节（32 个 hex 字符）。
    #[test]
    fn display_shows_short_fingerprint() {
        let mut store = TrustStore::empty();
        store.add(fp(0xab), "Pixel", 0);
        let text = store.devices()[0].display();
        assert!(text.starts_with("Pixel ("));
        assert_eq!(
            text.len(),
            "Pixel ()".len() + FINGERPRINT_SHOW * 2,
            "指纹显示长度不对：{}",
            text
        );
    }
}
