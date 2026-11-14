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
    /// 305：断点续传日志（已经收到哪些块）。收尾时按它决定删除日志还是留着重传。
    journal: Mutex<Option<crate::journal::ReceiveJournal>>,
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

    /// 305：按「每块 1 bit」的字节位图整片置位（续传日志就是这种格式，最低位是这一字节的第一个块）。
    /// 返回这次新标上的块数（本来就已经是完成的块不重复计数）。
    fn mark_bits(&mut self, bits: &[u8], total_size: u64, block_size: u64) -> usize {
        let total = ((total_size + block_size - 1) / block_size.max(1)).max(1) as usize;
        if total != self.total_blocks {
            self.total_blocks = total;
            self.bits = vec![0u64; (total + 63) / 64];
        }
        let mut marked = 0usize;
        for (byte_index, byte) in bits.iter().enumerate() {
            if *byte == 0 {
                continue;
            }
            for bit in 0..8u32 {
                if (byte & (1u8 << bit)) == 0 {
                    continue;
                }
                let index = byte_index * 8 + bit as usize;
                if index >= total {
                    break;
                }
                if !self.is_done(index) {
                    marked += 1;
                }
                self.bits[index / 64] |= 1u64 << (index % 64);
            }
        }
        marked
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

/// 续传时「磁盘上本来就有」的块用这个通道号记账。
///
/// 它和真正的通道号（0/1/2…）分开：这些块是一整片区间，画进度条时自然连成一段，
/// 不会被本轮新收到的块挤成碎条。u8::MAX 已经表示「归属未知」，所以取它前一个。
pub const OWNER_RESUMED: u8 = u8::MAX - 1;

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

    /// 305：续传时把「磁盘上已经有的块」一次性标成完成，归属记作 {@link OWNER_RESUMED}。
    ///
    /// 手机端续传时把这些块直接当完成（不再重发），电脑端以前只记「本轮真收到的块」，
    /// 于是这些区间在进度条上一直是空的：数字说 480MB 已收，条子上却只有后面新传的那点碎段。
    fn mark_resumed(&mut self, total: u64, bits: &[u8]) -> usize {
        self.total = total;
        let marked = self.blocks.mark_bits(bits, total, BLOCK_SIZE);
        let blocks = self.blocks.blocks();
        if self.owners.len() < blocks {
            self.owners.resize(blocks, u8::MAX);
        }
        for i in 0..blocks {
            if self.blocks.is_done(i) && self.owners.get(i).copied().unwrap_or(u8::MAX) == u8::MAX {
                self.owners[i] = OWNER_RESUMED;
            }
        }
        marked
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
                    // 第一段的起点是「第一个已完成的块」，不是 0：
                    // 续传时前面的块可能还没到，写死 0 会让这一段从最左边开始填，
                    // 和它真实所在的位置错位（看着就是「左边一小段蓝 + 中间大片空白」）。
                    previous = Some(owner);
                    segment_start = i;
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
    /// 本次会话真收到的字节（不含续传时「磁盘上本来就有」的部分，速率按它算）
    session_bytes: AtomicU64,
    /// 续传基线：这次续传开始时，磁盘上已经完成的字节（算进「已接收」显示，但不进速率）
    resumed_bytes: AtomicU64,
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
        self.resumed_bytes.store(0, Ordering::Relaxed);
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

    /// 305：续传开始前，把「磁盘上已经有的块」直接标成完成（手机端也是这么算的）。
    ///
    /// 这样进度条从「已经传过的位置」接着往上填，而不是把这一片当成没传；
    /// 「已接收 x / y」和条子读的是同一份块位图，数字和填充自然对得上。
    /// 返回这次新标上的块数（日志用）。
    pub fn mark_resumed(&self, path: &str, total: u64, bits: &[u8]) -> usize {
        if bits.is_empty() || total == 0 {
            return 0;
        }
        let (is_new, added, done, total_now) = match self.files.lock() {
            Ok(mut files) => {
                let is_new = !files.contains_key(path);
                let entry = files.entry(path.to_string()).or_default();
                let before = entry.done_bytes();
                entry.mark_resumed(total, bits);
                let done = entry.done_bytes();
                let total_now = entry.total.max(total);
                (is_new, done.saturating_sub(before), done.min(total_now), total_now)
            }
            Err(_) => return 0,
        };
        if is_new {
            self.files_started.fetch_add(1, Ordering::Relaxed);
        }
        // 只把「因为续传而多出来的字节」记进基线：后面真收到的块走 mark_block 的 session_bytes
        self.resumed_bytes.fetch_add(added, Ordering::Relaxed);
        self.file_total.store(total_now, Ordering::Relaxed);
        self.file_done.store(done, Ordering::Relaxed);
        if let Ok(mut name) = self.current.write() {
            if name.as_str() != path {
                *name = path.to_string();
            }
        }
        added as usize
    }

    /// 本次会话真收到的字节数（续传基线不算）：界面用它算速率，避免续传一开始跳出一个假峰值
    pub fn received_bytes(&self) -> u64 {
        self.session_bytes.load(Ordering::Relaxed)
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

    /// (文件名, 当前文件已完成字节, 当前文件总字节, 本次会话字节（含续传基线）, 文件数)
    ///
    /// 第三个数字（已接收）和进度条同源：当前文件的字节数取自块位图，
    /// 会话字节 = 本轮真收到 + 续传基线，续传时一开始就是「480.0 MB」而不是 0。
    pub fn snapshot(&self) -> (String, u64, u64, u64, u64) {
        let name = self.current.read().map(|g| g.clone()).unwrap_or_default();
        let session = self
            .session_bytes
            .load(Ordering::Relaxed)
            .saturating_add(self.resumed_bytes.load(Ordering::Relaxed));
        (
            name,
            self.file_done.load(Ordering::Relaxed),
            self.file_total.load(Ordering::Relaxed),
            session,
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
        let state = self.state_for(&target, total_size, modified_ms)?;
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
        // 305：记下这一块已经落盘（下次中断时就能从这里续上）
        if let Ok(mut journal) = state.journal.lock() {
            if let Some(journal) = journal.as_mut() {
                journal.mark(index as u64);
            }
        }

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

    fn state_for(&self, target: &Path, total_size: u64, modified_ms: i64) -> io::Result<Arc<FileState>> {
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
        // 有匹配的续传日志就接着用（同一路径、同样大小、同样修改时间），否则新建一份
        let journal = crate::journal::ReceiveJournal::load(target, total_size, modified_ms, BLOCK_SIZE)
            .or_else(|| Some(crate::journal::ReceiveJournal::new(total_size, modified_ms, BLOCK_SIZE)));
        let state = Arc::new(FileState {
            file,
            modified_ms: std::sync::atomic::AtomicI64::new(0),
            total: total_size,
            done: AtomicU64::new(0),
            journal: Mutex::new(journal),
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
    ///
    /// 305：没传完的文件默认**不删除** —— 把"已经收到哪些块"的日志写到旁边
    /// （`<文件名>.hfxpart`），下次同一个文件（大小/修改时间一致）再传时从这里续上。
    /// 有位图就不会出现 303 当初担心的"大小对、内容有洞"的假文件。
    ///
    /// @param keep_incomplete 没传完的文件要不要留（用户取消时就问过这一句）：
    ///   - true（默认）＝ 留文件和续传日志，下次续传；
    ///   - false ＝ 用户取消时选的是「不保留进度」（手机端发 CANCEL_DISCARD，或电脑端
    ///     「当前文件」卡片上点「取消」→「不保留，删掉」）：半截文件和 `.hfxpart` 一起删掉，
    ///     下次同一个文件从头传（1.7GB 的文件预分配了整份大小，留着就是白占盘）。
    /// 返回（保留的未完成文件数, 这些文件已经收到的字节数）。
    pub fn finish_with(&self, keep_incomplete: bool) -> io::Result<(usize, u64)> {
        let mut kept: Vec<(PathBuf, u64)> = Vec::new();
        // 用户选了「不保留」的文件：句柄放掉之后再删（Windows 上开着删不掉）
        let mut dropped: Vec<PathBuf> = Vec::new();
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
                let got = state.done.load(Ordering::Relaxed);
                let mut journal = state
                    .journal
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match journal.as_mut() {
                    Some(journal) if !journal.all_done() => {
                        if !keep_incomplete {
                            // 「不保留」：续传日志一行都不写，收尾时连文件一起删
                            dropped.push(path.clone());
                            continue;
                        }
                        if let Err(e) = journal.save(path) {
                            crate::log::warn(&format!(
                                "{} {}: {}",
                                crate::i18n::tr("续传日志写入失败", "cannot write resume journal"),
                                path.display(),
                                e
                            ));
                        } else {
                            kept.push((path.clone(), got));
                            crate::log::warn(&format!(
                                "{} {}",
                                crate::i18n::tr(
                                    "保留未接收完的文件（下次可续传）",
                                    "kept the incomplete file so it can resume"
                                ),
                                path.display()
                            ));
                        }
                    }
                    // 传完了：续传日志不再需要
                    _ => crate::journal::ReceiveJournal::remove(path),
                }
            }
            // 句柄必须全部放掉：Windows 上还开着的文件删不掉
            let _handles: Vec<_> = entries.drain().collect();
        }
        // 手机端点了「不保留」：文件和续传记录都删掉，下次同一个文件从头传
        for path in &dropped {
            crate::journal::ReceiveJournal::remove(path);
            match fs::remove_file(path) {
                Ok(()) => crate::log::warn(&format!(
                    "{} {}",
                    crate::i18n::tr(
                        "已删除未接收完的文件（选择不保留进度）",
                        "removed the incomplete file (discard chosen)"
                    ),
                    path.display()
                )),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => crate::log::warn(&format!(
                    "{} {}: {}",
                    crate::i18n::tr("删除未接收完的文件失败", "cannot remove the incomplete file"),
                    path.display(),
                    e
                )),
            }
        }
        Ok((kept.len(), kept.iter().map(|(_, got)| *got).sum()))
    }

    /// 305：这个文件已经收到哪些块（续传用）。返回位图；None 表示不能续传（当新文件传）。
    pub fn resume_bitmap(&self, raw_path: &str, total_size: u64, modified_ms: i64) -> Option<Vec<u8>> {
        let target = self.resolve(raw_path);
        let journal = crate::journal::ReceiveJournal::load(&target, total_size, modified_ms, BLOCK_SIZE)?;
        // 日志说收到过，但文件不见了 / 大小不对：别信它，从头传
        let metadata = fs::metadata(&target).ok()?;
        if metadata.len() != total_size {
            return None;
        }
        if journal.blocks_done() == 0 {
            return None;
        }
        // 把这个文件登记进本次会话：即使一块都不用收（已经全在），收尾也要复算它的
        // SHA-256 参与两端比对，否则"文件数不一致"会被判成校验失败。
        let _ = self.state_for(&target, total_size, modified_ms);
        Some(journal.bits.clone())
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
    fn incomplete_files_are_removed_when_discarding() {
        let dir = temp_dir("incomplete");
        let receiver = Receiver::new(util::normalize_dir(&dir));
        let partial = dir.join("partial.bin");
        let partial_str = partial.to_string_lossy().to_string();
        // 2 块的文件只写第一块：没接收完
        receiver
            .write_block(0, &partial_str, 2_097_152, 0, 1_700_000_000_000, &[1u8; 1024])
            .unwrap();
        assert!(partial.exists());
        // 手机端取消时选了「不保留进度」：文件和续传日志都要删掉
        let (kept, lost) = receiver.finish_with(false).unwrap();
        assert_eq!(kept, 0);
        assert_eq!(lost, 0);
        assert!(!partial.exists(), "不保留时半截文件应该被删掉");
        assert!(
            !crate::journal::ReceiveJournal::sidecar_path(&partial).exists(),
            "不保留时续传日志也应该被删掉"
        );
    }

    #[test]
    fn incomplete_files_are_kept_for_resume() {
        let dir = temp_dir("incomplete-keep");
        let receiver = Receiver::new(util::normalize_dir(&dir));
        let partial = dir.join("keep.bin");
        let partial_str = partial.to_string_lossy().to_string();
        receiver
            .write_block(0, &partial_str, 2_097_152, 0, 1_700_000_000_000, &[1u8; 1024])
            .unwrap();
        // 默认（保留）：文件和 <文件名>.hfxpart 都留着，下次同一个文件从这里续传
        let (kept, lost) = receiver.finish_with(true).unwrap();
        assert_eq!(kept, 1);
        assert_eq!(lost, 1024);
        assert!(partial.exists(), "保留时半截文件要留着");
        assert!(
            crate::journal::ReceiveJournal::sidecar_path(&partial).exists(),
            "保留时要写出续传日志"
        );
    }

    /// 一次取消里同时有「收全了」和「传了一半」的文件：
    /// 选「不保留」时只删没传完的那个，收全的文件一个字节都不能动
    /// （电脑端「取消」→「不保留」走的就是这条路径）。
    #[test]
    fn discarding_keeps_already_complete_files() {
        let dir = temp_dir("incomplete-mixed");
        let receiver = Receiver::new(util::normalize_dir(&dir));
        let done = dir.join("done.bin");
        let partial = dir.join("partial.bin");
        let done_str = done.to_string_lossy().to_string();
        let partial_str = partial.to_string_lossy().to_string();
        // 一个文件收全（1 块 = 1MB，长度刚好对上）
        receiver
            .write_block(0, &done_str, 1_048_576, 0, 1_700_000_000_000, &[9u8; 1_048_576])
            .unwrap();
        // 另一个只写了第一块
        receiver
            .write_block(0, &partial_str, 2_097_152, 0, 1_700_000_000_000, &[1u8; 1024])
            .unwrap();
        let (kept, lost) = receiver.finish_with(false).unwrap();
        assert_eq!((kept, lost), (0, 0));
        assert!(done.exists(), "已经收全的文件不能因为取消被删掉");
        assert!(!crate::journal::ReceiveJournal::sidecar_path(&done).exists());
        assert!(!partial.exists(), "没传完的文件要删掉");
        assert!(!crate::journal::ReceiveJournal::sidecar_path(&partial).exists());
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