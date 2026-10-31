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
use std::sync::{Arc, Mutex, RwLock};
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