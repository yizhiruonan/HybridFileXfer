//! Turns the file blocks coming from the phone into files on disk.
//!
//! Every block carries its file, its total size and its block index, so blocks
//! may arrive interleaved from several channels: each block is written at
//! `index * 1 MiB` with a positional write, no ordering required.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::proto::BLOCK_SIZE;
use crate::util;

struct FileState {
    file: File,
    modified_ms: std::sync::atomic::AtomicI64,
}

/// 接收目录，运行时可改（手机端浏览电脑文件夹后可以切换）。
#[derive(Clone)]
pub struct SharedDir(Arc<RwLock<PathBuf>>);

impl SharedDir {
    pub fn new(dir: PathBuf) -> SharedDir {
        SharedDir(Arc::new(RwLock::new(dir)))
    }

    pub fn get(&self) -> PathBuf {
        self.0
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| PathBuf::from("."))
    }

    pub fn set(&self, dir: PathBuf) {
        if let Ok(mut guard) = self.0.write() {
            *guard = dir;
        }
    }
}

/// 当前文件的"已收到块"位图，用来画 IDM 那种分块进度
#[derive(Default)]
pub struct BlockMap {
    total_blocks: usize,
    bits: Vec<u64>,
}

impl BlockMap {
    pub fn reset(&mut self) {
        self.total_blocks = 0;
        self.bits.clear();
    }

    pub fn mark(&mut self, index: u32, total_size: u64, block_size: u64) {
        let total = ((total_size + block_size - 1) / block_size.max(1)).max(1) as usize;
        if total != self.total_blocks {
            self.total_blocks = total;
            self.bits = vec![0u64; (total + 63) / 64];
        }
        let i = index as usize;
        if i < self.total_blocks {
            self.bits[i / 64] |= 1u64 << (i % 64);
        }
    }

    /// 把块聚合成 n 段，每段返回完成比例 0..1
    pub fn segments(&self, n: usize) -> Vec<f32> {
        if self.total_blocks == 0 || n == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(n);
        for s in 0..n {
            let start = s * self.total_blocks / n;
            let end = ((s + 1) * self.total_blocks / n).max(start);
            if end <= start {
                out.push(0.0);
                continue;
            }
            let mut got = 0usize;
            for b in start..end {
                if self.bits[b / 64] & (1u64 << (b % 64)) != 0 {
                    got += 1;
                }
            }
            out.push(got as f32 / (end - start) as f32);
        }
        out
    }
}

/// 字节数 -> 人类可读（供其它模块复用）
pub fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KB * KB * KB {
        format!("{:.2} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// 传输进度（全局唯一：GUI / CLI 都只有一份接收器实例）
pub struct ProgressState {
    /// 当前正在写的文件（显示用短名）
    pub file_name: RwLock<String>,
    /// 当前文件已写字节
    pub file_done: AtomicU64,
    /// 当前文件总字节
    pub file_total: AtomicU64,
    /// 本次会话累计字节
    pub session_bytes: AtomicU64,
    /// 本次会话已开始的文件数
    pub files_started: AtomicU64,
    /// 当前文件的块位图
    pub blocks: Mutex<BlockMap>,
}

impl ProgressState {
    pub fn reset(&self) {
        if let Ok(mut guard) = self.file_name.write() {
            guard.clear();
        }
        self.file_done.store(0, Ordering::Relaxed);
        self.file_total.store(0, Ordering::Relaxed);
        self.session_bytes.store(0, Ordering::Relaxed);
        self.files_started.store(0, Ordering::Relaxed);
        if let Ok(mut b) = self.blocks.lock() {
            b.reset();
        }
    }

    /// 发送方向（电脑 -> 手机）更新进度
    pub fn set_sending(&self, dest: &str, done: u64, total: u64, delta: u64) {
        if let Ok(mut name) = self.file_name.write() {
            if name.as_str() != dest {
                *name = dest.to_string();
            }
        }
        self.file_done.store(done, Ordering::Relaxed);
        self.file_total.store(total, Ordering::Relaxed);
        self.session_bytes.fetch_add(delta, Ordering::Relaxed);
    }

    /// 当前文件的 n 段完成比例
    pub fn segments(&self, n: usize) -> Vec<f32> {
        self.blocks
            .lock()
            .map(|b| b.segments(n))
            .unwrap_or_default()
    }

    pub fn snapshot(&self) -> (String, u64, u64, u64, u64) {
        let name = self
            .file_name
            .read()
            .map(|g| g.clone())
            .unwrap_or_default();
        (
            name,
            self.file_done.load(Ordering::Relaxed),
            self.file_total.load(Ordering::Relaxed),
            self.session_bytes.load(Ordering::Relaxed),
            self.files_started.load(Ordering::Relaxed),
        )
    }
}

pub static PROGRESS: LazyLock<ProgressState> = LazyLock::new(|| ProgressState {
    file_name: RwLock::new(String::new()),
    file_done: AtomicU64::new(0),
    file_total: AtomicU64::new(0),
    session_bytes: AtomicU64::new(0),
    files_started: AtomicU64::new(0),
    blocks: Mutex::new(BlockMap::default()),
});

pub struct Receiver {
    dir: SharedDir,
    entries: Mutex<HashMap<PathBuf, Arc<FileState>>>,
    bytes: AtomicU64,
    files: AtomicU64,
}

impl Receiver {
    pub fn new(receive_dir: PathBuf) -> Receiver {
        Receiver::with_shared(SharedDir::new(receive_dir))
    }

    pub fn with_shared(dir: SharedDir) -> Receiver {
        Receiver {
            dir,
            entries: Mutex::new(HashMap::new()),
            bytes: AtomicU64::new(0),
            files: AtomicU64::new(0),
        }
    }

    #[allow(dead_code)]
    pub fn receive_dir(&self) -> PathBuf {
        self.dir.get()
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn file_count(&self) -> u64 {
        self.files.load(Ordering::Relaxed)
    }

    /// Map a path sent by the phone into the receive directory. Paths outside of
    /// it (should never happen) are flattened into the directory.
    pub fn resolve(&self, raw: &str) -> PathBuf {
        let normalized = if std::path::MAIN_SEPARATOR == '\\' {
            PathBuf::from(raw.replace('/', "\\"))
        } else {
            PathBuf::from(raw)
        };
        let base = self.dir.get();
        if util::path_starts_with(&normalized, &base) {
            normalized
        } else {
            let name = normalized
                .file_name()
                .map(|n| n.to_owned())
                .unwrap_or_else(|| "received".into());
            base.join(name)
        }
    }

    pub fn write_block(
        &self,
        raw_path: &str,
        total_size: u64,
        index: u32,
        modified_ms: i64,
        data: &[u8],
    ) -> io::Result<()> {
        let target = self.resolve(raw_path);
        let state = self.state_for(&target, total_size)?;
        let offset = index as u64 * BLOCK_SIZE;
        let current = state.modified_ms.load(Ordering::Relaxed);
        if modified_ms > current {
            state.modified_ms.store(modified_ms, Ordering::Relaxed);
        }
        let mut written = 0usize;
        while written < data.len() {
            let n = write_at(&state.file, &data[written..], offset + written as u64)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!("cannot write to {}", target.display()),
                ));
            }
            written += n;
        }
        self.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);

        // ---- 进度上报 ----
        PROGRESS
            .file_total
            .store(total_size, Ordering::Relaxed);
        let changed = {
            let mut guard = match PROGRESS.file_name.write() {
                Ok(g) => g,
                Err(_) => return Ok(()),
            };
            if *guard != raw_path {
                *guard = raw_path.to_string();
                true
            } else {
                false
            }
        };
        if changed {
            PROGRESS.file_done.store(0, Ordering::Relaxed);
            PROGRESS.files_started.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut blocks) = PROGRESS.blocks.lock() {
                blocks.reset();
            }
        }
        if let Ok(mut blocks) = PROGRESS.blocks.lock() {
            blocks.mark(index, total_size, BLOCK_SIZE);
        }
        PROGRESS
            .file_done
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        PROGRESS
            .session_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);

        Ok(())
    }

    pub fn make_dir(&self, raw_path: &str) -> io::Result<()> {
        let target = self.resolve(raw_path);
        fs::create_dir_all(target)?;
        Ok(())
    }

    fn state_for(&self, target: &Path, total_size: u64) -> io::Result<Arc<FileState>> {
        {
            let entries = self
                .entries
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "poisoned entry lock"))?;
            if let Some(state) = entries.get(target) {
                return Ok(state.clone());
            }
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(target)?;
        file.set_len(total_size)?;
        let state = Arc::new(FileState {
            file,
            modified_ms: std::sync::atomic::AtomicI64::new(0),
        });
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "poisoned entry lock"))?;
        let existing = entries
            .entry(target.to_path_buf())
            .or_insert_with(|| {
                self.files.fetch_add(1, Ordering::Relaxed);
                state
            });
        Ok(existing.clone())
    }

    /// Restore modification times and close every file handle, so that the
    /// received files are not locked on Windows.
    pub fn finish(&self) -> io::Result<()> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "poisoned entry lock"))?;
        for (_path, state) in entries.drain() {
            if let Err(e) = apply_modified(&state.file, state.modified_ms.load(Ordering::Relaxed)) {
                crate::log::debug(&format!("cannot set modification time: {}", e));
            }
        }
        Ok(())
    }
}

fn apply_modified(file: &File, modified_ms: i64) -> io::Result<()> {
    if modified_ms <= 0 {
        return Ok(());
    }
    let time = UNIX_EPOCH + Duration::from_millis(modified_ms as u64);
    file.set_times(fs::FileTimes::new().set_modified(time))
}

#[cfg(windows)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

#[allow(dead_code)]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("hfx-receiver-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn blocks_are_written_at_their_offset() {
        let dir = temp_dir("blocks");
        let receiver = Receiver::new(util::normalize_dir(&dir));
        let base = dir.join("sample.bin");
        let base_str = base.to_string_lossy().to_string();
        receiver
            .write_block(&base_str, 2_097_152, 1, 1_700_000_000_000, &[7u8; 1024])
            .unwrap();
        receiver
            .write_block(&base_str, 2_097_152, 0, 1_700_000_000_000, &[3u8; 16])
            .unwrap();
        receiver.finish().unwrap();
        let data = fs::read(&base).unwrap();
        assert_eq!(data.len(), 2_097_152);
        assert_eq!(&data[..16], &[3u8; 16]);
        assert_eq!(&data[1024 * 1024..1024 * 1024 + 1024], &[7u8; 1024]);
        assert_eq!(receiver.bytes(), 1040);
        assert_eq!(receiver.file_count(), 1);
    }

    #[test]
    fn paths_outside_are_flattened() {
        let dir = temp_dir("outside");
        let normalized = util::normalize_dir(&dir);
        let receiver = Receiver::new(normalized.clone());
        let target = receiver.resolve("/etc/elsewhere/file.txt");
        assert!(util::path_starts_with(&target, &normalized));
        assert_eq!(target.file_name().unwrap(), "file.txt");
    }
}