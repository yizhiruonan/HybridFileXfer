//! 断点续传用的"已收到哪些块"日志（sidecar）。
//!
//! 每个接收中的文件旁边放一个 `<文件名>.hfxpart`：魔数 + 版本 + 块大小 + 总大小 +
//! 源修改时间 + 已完成块的位图。发送端在开始传之前问一次这份位图，已经收到的块就跳过。
//!
//! 为什么不用"看文件大小"判断：落盘是先 `set_len` 再按块随机写，文件大小一直是满的，
//! 有洞的地方看不出来（303 当初就是因此把半截文件整块删掉）。有了位图，半截文件
//! 是安全的，取消后可以留着下次续传。

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub const SIDECAR_SUFFIX: &str = ".hfxpart";
const MAGIC: &[u8; 4] = b"HFXP";
const VERSION: u8 = 1;

/// 每个文件旁边的续传日志
#[derive(Clone, Debug)]
pub struct ReceiveJournal {
    pub block_size: u64,
    pub total_size: u64,
    /// 源文件（手机/电脑那一侧）的最后修改时间，用来判断"还是不是同一个文件"
    pub mtime_ms: i64,
    pub bits: Vec<u8>,
}

impl ReceiveJournal {
    pub fn sidecar_path(target: &Path) -> PathBuf {
        let mut name = target.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(SIDECAR_SUFFIX);
        target.with_file_name(name)
    }

    pub fn new(total_size: u64, mtime_ms: i64, block_size: u64) -> ReceiveJournal {
        let block_size = block_size.max(1);
        let blocks = (total_size + block_size - 1) / block_size;
        let bytes = ((blocks + 7) / 8) as usize;
        ReceiveJournal {
            block_size,
            total_size,
            mtime_ms,
            bits: vec![0u8; bytes],
        }
    }

    /// 读旁边那份日志；不存在 / 格式不对 / 大小或时间对不上都返回 None（当新文件重传）
    pub fn load(target: &Path, total_size: u64, mtime_ms: i64, block_size: u64) -> Option<ReceiveJournal> {
        let data = fs::read(Self::sidecar_path(target)).ok()?;
        if data.len() < 4 + 1 + 3 + 4 + 8 + 8 + 4 || &data[0..4] != MAGIC || data[4] != VERSION {
            return None;
        }
        let stored_block = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as u64;
        let stored_total = u64::from_be_bytes([
            data[12], data[13], data[14], data[15], data[16], data[17], data[18], data[19],
        ]);
        let stored_mtime = i64::from_be_bytes([
            data[20], data[21], data[22], data[23], data[24], data[25], data[26], data[27],
        ]);
        let bitmap_len = u32::from_be_bytes([data[28], data[29], data[30], data[31]]) as usize;
        if data.len() < 32 + bitmap_len {
            return None;
        }
        if stored_block != block_size.max(1) || stored_total != total_size || stored_mtime != mtime_ms {
            // 同一个路径但内容变了（大小/时间不同）：旧日志作废
            return None;
        }
        Some(ReceiveJournal {
            block_size: stored_block,
            total_size: stored_total,
            mtime_ms: stored_mtime,
            bits: data[32..32 + bitmap_len].to_vec(),
        })
    }

    pub fn save(&self, target: &Path) -> io::Result<()> {
        let mut out = Vec::with_capacity(32 + self.bits.len());
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&[0u8; 3]);
        out.extend_from_slice(&(self.block_size as u32).to_be_bytes());
        out.extend_from_slice(&self.total_size.to_be_bytes());
        out.extend_from_slice(&self.mtime_ms.to_be_bytes());
        out.extend_from_slice(&(self.bits.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.bits);
        let path = Self::sidecar_path(target);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::create(&path)?;
        file.write_all(&out)?;
        Ok(())
    }

    pub fn remove(target: &Path) {
        let _ = fs::remove_file(Self::sidecar_path(target));
    }

    pub fn block_count(&self) -> u64 {
        (self.total_size + self.block_size - 1) / self.block_size
    }

    pub fn is_done(&self, index: u64) -> bool {
        let byte = (index / 8) as usize;
        byte < self.bits.len() && (self.bits[byte] & (1u8 << (index % 8))) != 0
    }

    pub fn mark(&mut self, index: u64) {
        let byte = (index / 8) as usize;
        if byte < self.bits.len() {
            self.bits[byte] |= 1u8 << (index % 8);
        }
    }

    pub fn all_done(&self) -> bool {
        let blocks = self.block_count();
        (0..blocks).all(|index| self.is_done(index))
    }

    /// 已置位的块数
    pub fn blocks_done(&self) -> u64 {
        (0..self.block_count()).filter(|index| self.is_done(*index)).count() as u64
    }

    /// 已经收到的字节数（续传时用来算"从哪继续"和进度基线）
    pub fn done_bytes(&self) -> u64 {
        let blocks = self.block_count();
        let full = self.block_size;
        (0..blocks)
            .filter(|index| self.is_done(*index))
            .map(|index| {
                let start = index * full;
                (self.total_size - start).min(full)
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_round_trip() {
        let dir = std::env::temp_dir().join(format!("hfx-journal-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("big.bin");
        fs::write(&target, b"0123456789").unwrap();
        let mut journal = ReceiveJournal::new(10, 1_700_000_000_000, 4);
        assert_eq!(journal.block_count(), 3);
        assert!(!journal.all_done());
        journal.mark(0);
        journal.mark(2);
        assert_eq!(journal.done_bytes(), 4 + 2);
        journal.save(&target).unwrap();
        let loaded = ReceiveJournal::load(&target, 10, 1_700_000_000_000, 4).unwrap();
        assert!(loaded.is_done(0));
        assert!(!loaded.is_done(1));
        assert!(loaded.is_done(2));
        assert_eq!(loaded.done_bytes(), 6);
        // 版本对不上就不复用
        assert!(ReceiveJournal::load(&target, 11, 1_700_000_000_000, 4).is_none());
        ReceiveJournal::remove(&target);
        assert!(ReceiveJournal::load(&target, 10, 1_700_000_000_000, 4).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
