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
    /// 文件总字节（0 = 未知）
    total: u64,
    /// 已写入字节（判断收没收完）
    done: AtomicU64,
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

    /// 已置位的块数
    pub fn count(&self) -> usize {
        self.bits.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// 这块是否已完成
    fn is_done(&self, index: usize) -> bool {
        index < self.total_blocks && (self.bits[index / 64] & (1u64 << (index % 64))) != 0
    }

    /// 位图覆盖的块数（不是已完成块数）
    fn blocks(&self) -> usize {
        self.total_blocks
    }
}

/// 进度条上的一段（一段 = 一条通道负责的连续块区间，IDM / 迅雷 那种）
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct XferSeg {
    /// 这一段占整条的比例（按块数算宽度）
    pub span: f32,
    /// 这一段已完成比例 0..1
    pub filled: f32,
}

/// 一个文件（一次发送 / 接收）的进度：块位图 + 每块是哪条通道做的。
///
/// 多文件并发时每个文件一份，互不干扰；段数由"相邻块的通道归属"还原。
#[derive(Default)]
pub struct FileProgress {
    total: u64,
    blocks: BlockMap,
    /// 每个块由哪条通道负责（u8::MAX = 未知）
    owners: Vec<u8>,
}

impl FileProgress {
    fn mark(&mut self, channel: u8, index: u32, total: u64) {
        self.total = total;
        self.blocks.mark(index, total, BLOCK_SIZE);
        let i = index as usize;
        if i >= self.owners.len() {
            self.owners.resize(i + 1, u8::MAX);
        }
        self.owners[i] = channel;
    }

    fn done_bytes(&self) -> u64 {
        (self.blocks.count() as u64)
            .saturating_mul(BLOCK_SIZE)
            .min(self.total)
    }

    /// IDM 式分段：相邻块的通道归属一变就是一段。
    /// 段数超过上限（旧的轮询式调度会让它爆炸）时退化成一条整进度。
    fn segments(&self, max_segments: usize) -> Vec<XferSeg> {
        let total = self.blocks.blocks();
        if total == 0 {
            return Vec::new();
        }
        let mut bounds: Vec<(usize, u8)> = Vec::new();
        let mut previous: Option<u8> = None;
        let mut segment_start = 0usize;
        for i in 0..total {
            if !self.blocks.is_done(i) {
                continue;
            }
            let owner = self.owners.get(i).copied().unwrap_or(u8::MAX);
            match previous {
                None => {
                    previous = Some(owner);
                    segment_start = 0;
                }
                Some(prev) if prev != owner => {
                    bounds.push((segment_start, prev));
                    segment_start = i;
                    previous = Some(owner);
                }
                _ => {}
            }
        }
        let Some(last) = previous else {
            return Vec::new();
        };
        bounds.push((segment_start, last));
        if bounds.len() > max_segments.max(1) {
            // 每块都换通道：画出来全是碎条，退回整条
            let done = self.blocks.count() as f32;
            return vec![XferSeg {
                span: total as f32,
                filled: (done / total as f32).clamp(0.0, 1.0),
            }];
        }
        bounds
            .iter()
            .enumerate()
            .map(|(i, &(start, _owner))| {
                let end = bounds
                    .get(i + 1)
                    .map(|b| b.0)
                    .unwrap_or(total)
                    .clamp(start + 1, total);
                let span = end - start;
                let mut filled = 0usize;
                for b in start..end {
                    if self.blocks.is_done(b) {
                        filled += 1;
                    }
                }
                XferSeg {
                    span: span as f32,
                    filled: (filled as f32 / span as f32).clamp(0.0, 1.0),
                }
            })
            .collect()
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
///
/// 每个文件一份 {@link FileProgress}：多文件并发时各算各的，文件数 = 表里的条目数，
/// 进度条按"哪个通道做了第几块"还原 IDM 式的分段。旧实现只有一个共享位图，
/// 并发发多个文件时会被反复重置（进度停在半路、文件数按切换次数虚高）。
#[derive(Default)]
pub struct ProgressState {
    /// 当前正在写的文件（显示用）
    current: RwLock<String>,
    /// 当前文件已写字节
    file_done: AtomicU64,
    /// 当前文件总字节
    file_total: AtomicU64,
    /// 本次会话累计字节
    session_bytes: AtomicU64,
    /// 本次会话已开始的文件数（= 表里条目数，不是切换次数）
    files_started: AtomicU64,
    /// 每个文件一份进度，按目标路径分开
    files: Mutex<HashMap<String, FileProgress>>,
}

/// 进度条上最多画几段；超过就退回成一条整进度
const MAX_SEGMENTS: usize = 24;

impl ProgressState {
    pub fn reset(&self) {
        if let Ok(mut guard) = self.current.write() {
            guard.clear();
        }
        self.file_done.store(0, Ordering::Relaxed);
        self.file_total.store(0, Ordering::Relaxed);
        self.session_bytes.store(0, Ordering::Relaxed);
        self.files_started.store(0, Ordering::Relaxed);
        if let Ok(mut files) = self.files.lock() {
            files.clear();
        }
    }

    /// 发送方向（电脑 -> 手机）：标记一块已由某条通道发出
    pub fn mark_sent(&self, channel: u8, dest: &str, index: u32, total: u64, block_len: u64) {
        self.mark_block(channel, dest, index, total, block_len);
    }

    /// 接收方向（手机 -> 电脑）：一块已写盘
    pub fn mark_received(&self, channel: u8, dest: &str, index: u32, total: u64, written: u64) {
        self.mark_block(channel, dest, index, total, written);
    }

    fn mark_block(&self, channel: u8, path: &str, index: u32, total: u64, bytes: u64) {
        let (is_new, done, total_now) = match self.files.lock() {
            Ok(mut files) => {
                let is_new = !files.contains_key(path);
                let entry = files.entry(path.to_string()).or_default();
                entry.mark(channel, index, total);
                let done = entry.done_bytes();
                (is_new, done, entry.total.max(total))
            }
            Err(_) => return,
        };
        if is_new {
            self.files_started.fetch_add(1, Ordering::Relaxed);
        }
        self.file_total.store(total_now, Ordering::Relaxed);
        self.file_done.store(done.min(total_now), Ordering::Relaxed);
        if let Ok(mut name) = self.current.write() {
            if name.as_str() != path {
                *name = path.to_string();
            }
        }
        self.session_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// 当前文件的 IDM 式分段（一段 = 一条通道负责的连续块区间）
    pub fn segments(&self) -> Vec<XferSeg> {
        let name = match self.current.read() {
            Ok(g) => g.clone(),
            Err(_) => return Vec::new(),
        };
        if name.is_empty() {
            return Vec::new();
        }
        match self.files.lock() {
            Ok(files) => files
                .get(&name)
                .map(|f| f.segments(MAX_SEGMENTS))
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    pub fn snapshot(&self) -> (String, u64, u64, u64, u64) {
        let name = self.current.read().map(|g| g.clone()).unwrap_or_default();
        (
            name,
            self.file_done.load(Ordering::Relaxed),
            self.file_total.load(Ordering::Relaxed),
            self.session_bytes.load(Ordering::Relaxed),
            self.files_started.load(Ordering::Relaxed),
        )
    }
}

/// 每台设备一份进度（多设备并行互不干扰）
static PROGRESS_MAP: LazyLock<Mutex<std::collections::HashMap<String, Arc<ProgressState>>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// 取某台设备的进度对象，没有就建一个
pub fn progress_for(device: &str) -> Arc<ProgressState> {
    let mut map = PROGRESS_MAP.lock().unwrap();
    Arc::clone(
        map.entry(device.to_string())
            .or_insert_with(|| Arc::new(ProgressState::default())),
    )
}

/// 第一台设备的进度（单设备场景 / CLI 沿用）
pub static PROGRESS: LazyLock<ProgressState> = LazyLock::new(ProgressState::default);

pub struct Receiver {
    /// 本设备独立的进度（多设备并行）
    pub progress: Arc<ProgressState>,
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
        Receiver::with_device(dir, "default")
    }

    /// 指定设备 ID 的接收器：进度对象按设备隔离
    pub fn with_device(dir: SharedDir, device: &str) -> Receiver {
        Receiver {
            progress: progress_for(device),
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

    /// 一次新的接收开始：把"本次已接收字节 / 文件数"清零。
    ///
    /// 不清的话跨多次传输一直累加：收尾日志和界面会显示
    /// 「4 个文件 5.48 GB」（其实这次只传了 800MB），速率也跟着算飞。
    pub fn reset_session_counters(&self) {
        self.bytes.store(0, Ordering::Relaxed);
        self.files.store(0, Ordering::Relaxed);
    }

    /// 303：本次会话落盘的文件列表（收尾算整文件校验和用）。
    /// 必须在 finish_with() 之前调用 —— 它会把这些条目清空。
    pub fn received_files(&self) -> Vec<PathBuf> {
        match self.entries.lock() {
            Ok(entries) => entries.keys().cloned().collect(),
            Err(_) => Vec::new(),
        }
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
        channel: u8,
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
        state.done.fetch_add(data.len() as u64, Ordering::Relaxed);

        // ---- 进度上报 ----
        // 本设备的进度（设备小卡片） + 全局进度（GUI 大卡片读这一份）
        self.progress
            .mark_received(channel, raw_path, index, total_size, data.len() as u64);
        PROGRESS.mark_received(channel, raw_path, index, total_size, data.len() as u64);

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
            total: total_size,
            done: AtomicU64::new(0),
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
        self.finish_with(true).map(|_| ())
    }

    /// 收尾：恢复修改时间、放掉所有文件句柄。
    /// `keep_incomplete == false`（传输中断/出错）时，把这次没接收完的半截文件删掉，
    /// 免得电脑上留一堆"传了一半"的文件。返回（删掉的文件数, 丢掉的字节）。
    pub fn finish_with(&self, keep_incomplete: bool) -> io::Result<(usize, u64)> {
        let mut remove: Vec<(PathBuf, u64)> = Vec::new();
        {
            let mut entries = self
                .entries
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "poisoned entry lock"))?;
            for (path, state) in entries.iter() {
                if let Err(e) =
                    apply_modified(&state.file, state.modified_ms.load(Ordering::Relaxed))
                {
                    crate::log::debug(&format!("cannot set modification time: {}", e));
                }
                let total = state.total;
                let got = state.done.load(Ordering::Relaxed);
                if !keep_incomplete && total > 0 && got < total {
                    remove.push((path.clone(), got));
                }
            }
            // 句柄必须全部放掉：Windows 上还开着的文件删不掉
            let _handles: Vec<_> = entries.drain().collect();
        }
        let mut files = 0usize;
        let mut bytes = 0u64;
        for (path, got) in remove {
            match fs::remove_file(&path) {
                Ok(_) => {
                    files += 1;
                    bytes += got;
                    crate::log::warn(&format!(
                        "{} {}",
                        crate::i18n::tr("删除未接收完的文件", "removed incomplete file"),
                        path.display()
                    ));
                }
                Err(e) => crate::log::warn(&format!("{}: {}", path.display(), e)),
            }
        }
        Ok((files, bytes))
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
            .write_block(0, &base_str, 2_097_152, 1, 1_700_000_000_000, &[7u8; 1024])
            .unwrap();
        receiver
            .write_block(0, &base_str, 2_097_152, 0, 1_700_000_000_000, &[3u8; 16])
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
    fn incomplete_files_are_removed() {
        let dir = temp_dir("incomplete");
        let receiver = Receiver::new(util::normalize_dir(&dir));
        let partial = dir.join("partial.bin");
        let partial_str = partial.to_string_lossy().to_string();
        // 2 块的文件只写第一块：没接收完，收尾时应该被删掉
        receiver
            .write_block(0, &partial_str, 2_097_152, 0, 1_700_000_000_000, &[1u8; 1024])
            .unwrap();
        assert!(partial.exists());
        let (files, lost) = receiver.finish_with(false).unwrap();
        assert_eq!(files, 1);
        assert_eq!(lost, 1024);
        assert!(!partial.exists(), "半截文件应该被删掉");
    }

    #[test]
    fn complete_files_are_kept() {
        let dir = temp_dir("complete");
        let receiver = Receiver::new(util::normalize_dir(&dir));
        let target = dir.join("done.bin");
        let target_str = target.to_string_lossy().to_string();
        receiver
            .write_block(
                0,
                &target_str,
                1_048_576,
                0,
                1_700_000_000_000,
                &[1u8; 1_048_576],
            )
            .unwrap();
        let (files, _) = receiver.finish_with(true).unwrap();
        assert_eq!(files, 0);
        assert_eq!(fs::read(&target).unwrap().len(), 1_048_576);
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