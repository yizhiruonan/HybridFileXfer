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

/// 账本里"这一块还没收到"的标记（u32::MAX：一个块的字节数不可能这么多）
const BLOCK_MISSING: u32 = u32::MAX;

/// 一个文件"每块应该写多少字节 / 实际写下去多少字节"的账本。
///
/// 为什么非要有它：位图（{@link BlockMap}、{@link crate::journal::ReceiveJournal}）
/// 每块只记 1 个 bit =「这一块来过」，**不记长度**。而 `write_block` 以前对
/// index 和 length 一个都不校验，于是只要发来一个「短块」（或重复的块、越界的块），
/// 落盘结果就是：文件大小完全正确（`set_len` 先预分配了整份大小）、中间那一段是 0、
/// 位图却照样置位说「这块收齐了」。这就是 NEXT-TODO 第 11 条
/// 「两边大小完全一致、SHA-256 不一致」在落盘这一侧的成因 ——
/// 只能靠收尾复算 SHA-256 才发现，而且发现之后位图还认为那块是好的，
/// 重传时会被当成「已收到」跳过，于是永远修不好（要清掉 .hfxpart 才恢复）。
///
/// 现在每块都记下期望长度和实际长度：
///  - 短块 / 越界块当场在日志里指名道姓（第几块、应为多少、实际多少）；
///  - 短块不会进续传位图，重传时会被重新要一遍，洞补得回来；
///  - 收尾判断"传完没有"只看这本账，不再看漏块也没人发现的位图。
struct BlockLedger {
    total_size: u64,
    /// 每块的期望字节数（末块可能不满 1MiB）
    expected: Vec<u32>,
    /// 每块实际写下去的字节数；`BLOCK_MISSING` = 还没收到
    written: Vec<u32>,
    /// 同一个块重复到达的次数（同一个 index 写了不止一次）
    duplicates: u32,
}

impl BlockLedger {
    fn new(total_size: u64) -> BlockLedger {
        let blocks = (total_size + BLOCK_SIZE - 1) / BLOCK_SIZE.max(1);
        let mut expected = Vec::with_capacity(blocks as usize);
        for index in 0..blocks {
            let start = index * BLOCK_SIZE;
            expected.push((total_size - start).min(BLOCK_SIZE) as u32);
        }
        BlockLedger {
            total_size,
            expected,
            written: vec![BLOCK_MISSING; blocks as usize],
            duplicates: 0,
        }
    }

    fn blocks(&self) -> usize {
        self.expected.len()
    }

    /// 第 index 块应有的字节数；越界（不属于这个文件）返回 None
    fn expected_len(&self, index: u32) -> Option<u32> {
        self.expected.get(index as usize).copied()
    }

    /// 305：续传时把「磁盘上本来就有」的块补进账本（位图是唯一凭据）。
    /// 返回补进来的块数。
    fn seed_from_bits(&mut self, bits: &[u8]) -> usize {
        let mut seeded = 0usize;
        for index in 0..self.blocks() {
            let byte = index / 8;
            if byte >= bits.len() {
                break;
            }
            if bits[byte] & (1u8 << (index % 8)) != 0 && self.written[index] == BLOCK_MISSING {
                self.written[index] = self.expected[index];
                seeded += 1;
            }
        }
        seeded
    }

    /// 记下一块。返回 true = 这块之前已经到过（重复块，内容可能不一样）
    fn record(&mut self, index: u32, len: u32) -> bool {
        let i = index as usize;
        if i >= self.written.len() {
            return false;
        }
        let duplicate = self.written[i] != BLOCK_MISSING;
        if duplicate {
            self.duplicates += 1;
        }
        self.written[i] = len;
        duplicate
    }

    /// 一个字节都没收到的块
    fn missing(&self) -> Vec<u32> {
        self.written
            .iter()
            .enumerate()
            .filter(|(_, written)| **written == BLOCK_MISSING)
            .map(|(index, _)| index as u32)
            .collect()
    }

    /// 收到过、但长度不够的块（这正是"大小对、内容错"的那一段）
    fn short(&self) -> Vec<(u32, u32, u32)> {
        self.written
            .iter()
            .zip(self.expected.iter())
            .enumerate()
            .filter(|(_, (written, expected))| **written != BLOCK_MISSING && written != expected)
            .map(|(index, (written, expected))| (index as u32, *written, *expected))
            .collect()
    }

    /// 每一块都按应有长度写全了
    fn is_complete(&self) -> bool {
        self.written
            .iter()
            .zip(self.expected.iter())
            .all(|(written, expected)| written == expected)
    }

    fn written_bytes(&self) -> u64 {
        self.written
            .iter()
            .filter(|written| **written != BLOCK_MISSING)
            .map(|written| *written as u64)
            .sum()
    }

    /// 由账本生成续传位图（和 {@link crate::journal::ReceiveJournal} 同一套位序：
    /// 最低位是这一字节里的第一块）。只有长度对得上的块才算完成。
    fn to_bits(&self) -> Vec<u8> {
        let mut bits = vec![0u8; (self.blocks() + 7) / 8];
        for (index, (written, expected)) in self.written.iter().zip(self.expected.iter()).enumerate() {
            if written == expected {
                bits[index / 8] |= 1u8 << (index % 8);
            }
        }
        bits
    }
}

struct FileState {
    file: File,
    modified_ms: std::sync::atomic::AtomicI64,
    /// 文件总字节（0 = 未知）
    total: u64,
    /// 已写入字节（判断收没收完）
    done: AtomicU64,
    /// 每块期望长度 / 实际长度的账本（2026-11-17：位图漏块、短块都靠它对出来）
    ledger: Mutex<BlockLedger>,
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
    /// 这一段是谁在负责（通道号；u8::MAX = 还没分配 / 多通道合并而成，
    /// OWNER_RESUMED = 续传时磁盘上本来就有的块）
    pub owner: u8,
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

    /// 段数上限、属主区间上限 —— 和手机端 `BlockProgress` 的两个常量对齐。
    const MAX_SEGMENTS: usize = 24;
    const MAX_RUNS: usize = 512;

    /// 进度条上的分段：**和手机端 `BlockProgress.segments()` 是同一套算法**。
    ///
    /// 这里曾经按文件位置均分成 10 截（`XFER_SLICES`），那只是为了让条子「看起来像 IDM」，
    /// 段边界和手机端那条根本对不上 —— 用户要的是两边**同一条**进度条：手机怎么切，电脑就怎么切。
    /// 所以下面的算法照抄 Java 版 `BlockProgress.segmentsLocked()`，一步一步对着来：
    ///
    /// ① 整条块空间按属主切区间，属主一变就是一条边界。未完成的块属主未知（`u8::MAX`），
    ///    它们自己成段 —— 这一条很关键：只扫「已完成」的块会让最后一段一路吃到文件末尾，
    ///    正在传的那几块没有自己的段，切分线就没了；
    ///    随后再按左右邻居把「未知」的空档补上（见下面的 ①-补）：手机端连还没发的块都
    ///    知道归谁，电脑端只有收到的才知道，不补就会把一条通道的区间拆成两截，两边条子对不上；
    /// ② 相邻两段**都填满**才拼成一条（IDM 的「传完就拼成整条」）。左边满了就吞掉右边
    ///    还在传的段，会出现「还剩几条通道在传、切分线却全没了」；
    /// ③ 段数超过 {@link MAX_SEGMENTS} 时合并**最短**的那段，不是整条并成一根；
    /// ④ 每段的填充 = 段内已完成块 / 段总块，只从左边填（和主进度条同一套口径）。
    fn segments(&self) -> Vec<XferSeg> {
        let total = self.blocks.blocks().max(self.owners.len());
        if total == 0 {
            return Vec::new();
        }
        let filled_in = |start: usize, end: usize| -> usize {
            (start..end).filter(|&b| self.blocks.is_done(b)).count()
        };

        // ① 属主 run
        let mut runs: Vec<(usize, usize, u8)> = Vec::new();
        let mut previous: Option<u8> = None;
        let mut run_start = 0usize;
        for i in 0..total {
            let owner = self.owners.get(i).copied().unwrap_or(u8::MAX);
            match previous {
                None => {
                    previous = Some(owner);
                    run_start = 0;
                }
                Some(p) if p != owner => {
                    runs.push((run_start, i, p));
                    run_start = i;
                    previous = Some(owner);
                }
                _ => {}
            }
        }
        if let Some(p) = previous {
            runs.push((run_start, total, p));
        }
        // ①-补：已知归属之间的空档按邻居补上（手机端发块时就记了归属，连还没发的块
        // 都知道归谁；电脑端只有收到的块才知道）。不补的话，一条通道的区间会被拆成
        // 「已收前缀 + 待收剩余」两截，6 条通道就画出 12 截 —— 和手机那条对不上。
        // 补法只看邻居、不看节流：两边都是同一条通道就整段给它；两边不是同一条就在中间分开。
        let mut filled_runs: Vec<(usize, usize, u8)> = Vec::with_capacity(runs.len());
        for (idx, &(s, e, own)) in runs.iter().enumerate() {
            if own != u8::MAX || s == e {
                filled_runs.push((s, e, own));
                continue;
            }
            let prev_known = runs[..idx].iter().rev().find(|r| r.2 != u8::MAX).map(|r| r.2);
            let next_known = runs[idx + 1..].iter().find(|r| r.2 != u8::MAX).map(|r| r.2);
            // 空档**整段给左边那条通道**，不是两边各一半。
            //
            // 一开始写成「不同属主就在中间分开」，真机上就出问题：刚开传时每条通道只交了
            // 前几块，空档很大，从中间一分，画出来就是「从条子中间往两边涨、然后整体往右平移」
            // —— 那不是 IDM，是切错了。
            // 按左邻一条通道算才对：一条通道负责的是一段连续区间，它先交前缀、再补后面的，
            // 前缀和剩下的属于同一条通道，所以空档跟着左边走（末尾的空档本来就归最后那条）。
            match (prev_known, next_known) {
                (Some(a), _) => filled_runs.push((s, e, a)),
                (None, Some(b)) => filled_runs.push((s, e, b)),
                (None, None) => filled_runs.push((s, e, u8::MAX)),
            }
        }
        // 补完以后相邻 run 可能变成同一条通道，再压一遍
        let mut runs: Vec<(usize, usize, u8)> = Vec::with_capacity(filled_runs.len());
        for (s, e, own) in filled_runs {
            match runs.last_mut() {
                Some(last) if last.2 == own && last.1 == s => last.1 = e,
                _ => runs.push((s, e, own)),
            }
        }
        if runs.len() > Self::MAX_RUNS {
            // 每块都换通道（旧电脑端按块轮询）：画出来全是碎条，退回整条
            let done = self.blocks.count() as f32;
            return vec![XferSeg {
                span: total as f32,
                filled: (done / total as f32).clamp(0.0, 1.0),
                owner: u8::MAX,
            }];
        }

        // ② 相邻两段都满了才拼（属主不同就记成「混合」= -2）
        let mut merged: Vec<(usize, usize, i16)> = Vec::new();
        let mut current: Option<(usize, usize, i16)> = None;
        let mut current_full = false;
        for &(s, e, own) in &runs {
            let full = filled_in(s, e) >= e - s;
            match current {
                None => {
                    current = Some((s, e, own as i16));
                    current_full = full;
                }
                Some((cs, ce, co)) => {
                    if current_full && full {
                        current = Some((cs, e, if co != own as i16 { -2 } else { co }));
                    } else {
                        merged.push((cs, ce, co));
                        current = Some((s, e, own as i16));
                        current_full = full;
                    }
                }
            }
        }
        if let Some(c) = current {
            merged.push(c);
        }

        // ③ 段数超上限时合并最短的那段
        while merged.len() > Self::MAX_SEGMENTS {
            let mut shortest = 0usize;
            for i in 1..merged.len() {
                if merged[i].1 - merged[i].0 < merged[shortest].1 - merged[shortest].0 {
                    shortest = i;
                }
            }
            let into = if shortest == 0 { 1 } else { shortest - 1 };
            let (gs, ge, go) = merged[shortest];
            let t = merged[into];
            merged[into] = (t.0.min(gs), t.1.max(ge), if t.2 != go { -2 } else { t.2 });
            merged.remove(shortest);
        }

        // ④ 每段的填充
        merged
            .iter()
            .map(|&(s, e, owner)| {
                let span = (e - s).max(1);
                let filled = filled_in(s, e);
                XferSeg {
                    span: span as f32,
                    filled: (filled as f32 / span as f32).clamp(0.0, 1.0),
                    // -2 = 多通道拼起来的，和「还没分配」一样显示成未知
                    owner: if owner < 0 { u8::MAX } else { owner as u8 },
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
    /// 通道名表：下标 = 通道号（USB_ADB / wlan1#2 …），明细里按段显示「谁在负责」
    channels: Mutex<Vec<String>>,
    /// 这一轮（一次「发送 / 接收」）要传的文件清单，**按队列顺序**：(路径, 总大小)。
    ///
    /// 为什么必须单独存一份：`current` 是「最后收到一块的那个文件」，多文件并行时
    /// 它一秒能被改写十几次 —— 界面的标题、分段条、进度数字全都跟着它跳（用户报的
    /// 「文件名一秒换好几次」「分段条来回跳」）。回合身份只能按**队列顺序**认：
    /// 主文件 = 清单里第一个还没传完的文件，传完才轮到下一个。
    round_files: RwLock<Vec<(String, u64)>>,
}

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
        // channels（通道名表）**故意不在这里清**。
        //
        // 它是「本次连接的通道拓扑」，不是单轮传输的计数：set_channels 本来就是全量覆盖，
        // 根本不需要靠 reset 来清。反过来，一清就出事 —— reset 是全局的，
        // 会话线程结束（engine.rs run_session）、取消收尾（session.rs 好几处）、
        // 点「开始接收」（gui_slint 里那次）都会调它。旧会话/另一台设备那一条
        // 一退出就把新一轮刚登记好的名字抹掉，且没有任何代码会重新登记，
        // 于是 channel_name() 全返回 None，明细整张表退成「通道 1 / 通道 2 …」。
        // 真机 2026-11-21 复现：取消 → 停止 → 重连后名字又没了，就是这个。
        if let Ok(mut files) = self.files.lock() {
            files.clear();
        }
        // 文件清单是本轮的东西，reset = 新一轮，必须一起清（会话层紧接着会重新登记）。
        if let Ok(mut plan) = self.round_files.write() {
            plan.clear();
        }
    }

    /// 登记这一轮要传的整份文件清单（按队列顺序）。收 / 发两个方向都由会话层调用一次。
    pub fn set_round_files(&self, files: Vec<(String, u64)>) {
        if let Ok(mut plan) = self.round_files.write() {
            *plan = files;
        }
    }

    /// 当前回合的「主文件」：清单里第一个还没传完的文件（全传完就停在最后一个）。
    ///
    /// 这里返回 None 有两种情况：清单还没登记（老路径 / 发送方向没登记），
    /// 调用方就退回 `current`（原来那套「最后到达的文件」）。
    pub fn primary_path(&self) -> Option<String> {
        let plan = match self.round_files.read() {
            Ok(g) => g.clone(),
            Err(_) => return None,
        };
        if plan.is_empty() {
            return None;
        }
        // 每个文件已完成多少字节（没开始过的文件不在表里 = 0）
        let done: HashMap<String, u64> = match self.files.lock() {
            Ok(files) => files.iter().map(|(k, v)| (k.clone(), v.done_bytes())).collect(),
            Err(_) => HashMap::new(),
        };
        for (path, total) in &plan {
            // 0 字节文件（目录 / 空文件）没有数据要传，不该占着标题不动 —— 这里必须
            // **跳过**它继续看下一个。以前写成了 `total == 0 || d < total` 直接返回，
            // 于是清单里只要有一个 0 字节文件排在前面，整轮标题就钉死在这个文件上：
            // 它自己没有 FileProgress 条目 → snapshot 拿到 0/0、segments 为空，
            // 标题、分段进度条、百分比三项一起停住不动。
            if *total == 0 {
                continue;
            }
            let d = done.get(path).copied().unwrap_or(0);
            if d < *total {
                return Some(path.clone());
            }
        }
        // 全都传完了：停在最后一个（收尾那几帧标题不该突然变成空的）
        plan.last().map(|(p, _)| p.clone())
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

    /// 登记通道名（下标 = 通道号）。收到传输前由会话层调用一次，明细里就能写出
    /// 「USB_ADB / wlan0#2」而不是「通道段 3」。
    pub fn set_channels(&self, names: Vec<String>) {
        if let Ok(mut guard) = self.channels.lock() {
            *guard = names;
        }
    }

    /// 已登记的通道名条数（诊断用：能看出「名字没登记」还是「下标越界」）
    pub fn channel_count(&self) -> usize {
        self.channels.lock().map(|c| c.len()).unwrap_or(0)
    }

    /// 通道号 -> 通道名（未登记 / 未分配时返回 None，界面自己决定显示什么）
    pub fn channel_name(&self, owner: u8) -> Option<String> {
        self.channels
            .lock()
            .ok()
            .and_then(|names| names.get(owner as usize).cloned())
    }

    /// 当前文件的分段（一段 = 一条通道负责的连续块区间，算法和手机端一致，见 FileProgress::segments）
    pub fn segments(&self) -> Vec<XferSeg> {
        // 和标题同一个口径：锁主文件。以前跟着 current 走，每换一次「最后到达的文件」
        // 整组分段就被整体换掉一次 —— 界面看到的就是分段条每秒重排好几遍。
        let name = match self.primary_path() {
            Some(p) => p,
            None => match self.current.read() {
                Ok(g) => g.clone(),
                Err(_) => return Vec::new(),
            },
        };
        let name = if name.is_empty() {
            return Vec::new();
        } else {
            name
        };
        // 主文件还没开始（清单刚登记、首块还没到）时它没有 FileProgress 条目，
        // 分组是空的 —— 界面看到的就是「进度条整条消失」。这时候退回 current
        // （最后真正有数据到达的那个文件）的分段，别让分段条凭空消失。
        let files = match self.files.lock() {
            Ok(files) => files,
            Err(_) => return Vec::new(),
        };
        if let Some(progress) = files.get(&name) {
            let segments = progress.segments();
            if !segments.is_empty() {
                return segments;
            }
        }
        let fallback = match self.current.read() {
            Ok(g) => g.clone(),
            Err(_) => String::new(),
        };
        if fallback.is_empty() || fallback == name {
            return Vec::new();
        }
        files.get(&fallback).map(|f| f.segments()).unwrap_or_default()
    }

    /// (文件名, 当前文件已完成字节, 当前文件总字节, 本次会话字节（含续传基线）, 文件数)
    ///
    /// 第三个数字（已接收）和进度条同源：当前文件的字节数取自块位图，
    /// 会话字节 = 本轮真收到 + 续传基线，续传时一开始就是「480.0 MB」而不是 0。
    pub fn snapshot(&self) -> (String, u64, u64, u64, u64) {
        // 标题取「主文件」（队列顺序里的第一个未完成项），不是「最后到达的那个」：
        // 后者多文件并行时一秒能换十几次名（用户截图：super.img / data.img 来回跳）。
        let name = match self.primary_path() {
            Some(p) => p,
            None => self
                .current
                .read()
                .map(|g| g.clone())
                .unwrap_or_default(),
        };
        let session = self
            .session_bytes
            .load(Ordering::Relaxed)
            .saturating_add(self.resumed_bytes.load(Ordering::Relaxed));
        // 「已完成 / 总量」必须和标题指的是同一个文件，否则数字会跟着另一个文件跳。
        // 两个数都从块位图里按同一个文件名取；还没开始的文件用清单里的计划大小。
        let planned_total = self
            .round_files
            .read()
            .ok()
            .and_then(|g| g.iter().find(|(p, _)| *p == name).map(|(_, t)| *t))
            .unwrap_or(0);
        let mut done_total: Option<(u64, u64)> = if name.is_empty() {
            // 没有主文件（老路径还没走到登记这步）：退回原来那两个全局计数
            None
        } else {
            match self.files.lock() {
                Ok(files) => match files.get(&name) {
                    Some(f) => Some((f.done_bytes(), f.total.max(planned_total))),
                    None => Some((0u64, planned_total)),
                },
                Err(_) => Some((0u64, planned_total)),
            }
        };
        // 总量是 0 时这条信息等于没有：界面拿着 0/0 算不出百分比，只能停在原地。
        // 两种来路：清单里那个文件本来就没登记大小（0 字节 / 目录），或者这一轮还没开始。
        // 那就退回全局计数 —— 数字可能不是「这个文件」的，但至少是活的；
        // 以前这里直接给 0/0，标题、分段、百分比三项一起钉死（比改之前更糟）。
        if done_total.map(|(_, total)| total == 0).unwrap_or(true) {
            done_total = Some((
                self.file_done.load(Ordering::Relaxed),
                self.file_total.load(Ordering::Relaxed),
            ));
        }
        let (done, total) = done_total.unwrap_or_default();
        (
            name,
            done,
            total,
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

/// 上一轮传输的失败原因（GUI 用）。
///
/// 为什么要有这一份：块账本对不上、手机端说指纹不一致这类失败，以前只写进日志和
/// 结论帧，GUI 一概不知道 —— 于是传输窗口亮一下「已完成」然后 3 秒自己收起来，
/// 用户以为传完了（TRANSFER-WINDOW.md §2.1 明确要求「失败/取消停在窗口上，等用户看见」）。
/// 这里放一个只进不出的信号，GUI 每帧读一次，用户关掉窗口（或下次开始接收）时清掉。
static LAST_FAILURE: LazyLock<Mutex<Option<(String, i32)>>> = LazyLock::new(|| Mutex::new(None));

/// 记下这一轮为什么失败（给 GUI 看；日志和结论帧照旧）。
///
/// 顺手把失败那一刻的进度百分比（按块算）也拍进快照：失败之后 `PROGRESS` 会停在原地、
/// 下一轮开始时又会被清掉，界面那边「等下一帧再读」读到的是别的轮次的数字 ——
/// 真机第一次验证就撞见了：开门就失败的传输，窗口上挂着上一轮的「未完成 100% / 3.00 GB」。
pub fn set_failure(reason: &str) {
    let percent = {
        let segs = PROGRESS.segments();
        let total: f32 = segs.iter().map(|s| s.span).sum::<f32>().max(1.0);
        let filled: f32 = segs.iter().map(|s| s.span * s.filled).sum::<f32>();
        (((filled / total) * 100.0).floor() as i32).clamp(0, 99)
    };
    if let Ok(mut slot) = LAST_FAILURE.lock() {
        *slot = Some((reason.to_string(), percent));
    }
}

/// 上一轮的失败原因；没有失败（或者已经被界面消费掉）就是 None。
pub fn last_failure() -> Option<String> {
    LAST_FAILURE.lock().ok().and_then(|slot| slot.as_ref().map(|f| f.0.clone()))
}

/// 失败那一刻的进度百分比（0..=99），界面直接用它，别再去读已经过期的 PROGRESS。
pub fn failure_percent() -> Option<i32> {
    LAST_FAILURE.lock().ok().and_then(|slot| slot.as_ref().map(|f| f.1))
}

/// 清掉失败提示：下一轮开始接收、或者用户把失败的窗口关掉时调用。
pub fn clear_failure() {
    if let Ok(mut slot) = LAST_FAILURE.lock() {
        *slot = None;
    }
}

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

    /// 把手机报来的路径映射成落盘路径。**防御点，改之前先读 util::relative_under 的注释。**
    ///
    /// 手机发来的路径是不可信输入：接收端零认证、同一个 Wi-Fi 里谁都能连上来推文件，
    /// 所以这条路径等于「陌生人能指定往哪写」。老实现用字符串前缀判断「在不在接收目录里」，
    /// `C:\接收\..\..\..\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\x.exe`
    /// 前缀就是接收目录，判成「在里面」原样放行，操作系统落盘时才按 `..` 往上爬到目录外
    /// —— 陌生人就能往你的自启动目录塞程序，开机即执行。
    ///
    /// 现在两道防线，这里是**第一道**（第二道见 `ensure_inside`，在真正写盘前）：
    ///  1. 在接收目录里（按组件比，含子目录）→ 只允许普通名字段，拼回 base 下，
    ///     结果和以前完全一样，正常的子目录传输不受影响；
    ///  2. 不在里面、或者带 `..` 想绕出去 → 保留原来「拍平到 base/文件名」的行为，
    ///     `..` 直接变成文件名的一部分被丢掉（`..` 结尾没有名字就给 received）。
    ///
    /// 关键点：两个分支返回的路径都是 `base` 下面**词法上**拼出来的，路径组件里不可能
    /// 残留 `..`，所以不存在「靠操作系统去解析 ..」的空间。
    pub fn resolve(&self, raw: &str) -> PathBuf {
        let normalized = if std::path::MAIN_SEPARATOR == '\\' {
            PathBuf::from(raw.replace('/', "\\"))
        } else {
            PathBuf::from(raw)
        };
        let base = self.dir.get();
        if let Some(rest) = util::relative_under(&normalized, &base) {
            // 在接收目录里：行为和以前一致（原来直接返回 normalized，这里只是
            // 用 base + 普通名字段重新拼一遍，两者指向同一个路径）
            if rest.as_os_str().is_empty() {
                // 路径正好等于接收目录本身（或者 `base\..` 这种绕一圈又回来的），
                // 那是目录不是文件，落到下面拍平分支去
            } else {
                return base.join(rest);
            }
        }
        // 不在接收目录里（正常不会发生），或者想用 `..` 绕出去：拍平到接收目录，
        // 和改之前一样，只是取名从 file_name() 换成「最后一段普通名字」——
        // file_name() 对 `a\..` 会返回 `a`（因为末尾是 ..），这里要的是「没有名字」。
        let name = util::last_normal_name(&normalized).unwrap_or_else(|| "received".into());
        let flattened = base.join(name);
        if self.ensure_inside(&flattened).is_ok() {
            flattened
        } else {
            // 万一连「拍平到接收目录」都被第二道防线拒了（软链接把接收目录本身指走），
            // 至少别 panic：给一个固定名字，still 在 base 下面。真正的错误会在
            // state_for 里再报一次并让这一块失败，上层照常按错误处理。
            base.join("received")
        }
    }

    /// 第二道防线：真的动磁盘之前，再确认一次目标路径就在接收目录里。
    ///
    /// 第一道防线（`resolve`）已经把路径拆成组件重拼过了，理论上不可能跑出去；
    /// 这里再查一次是**故意的冗余**，别删：
    ///  - `resolve` 是人写的，将来谁改一处忘了保持约定，这里能挡住；
    ///  - 微软那套「盘符相对路径」（`C:foo`）、`\\?\` 前缀、8.3 短名这些边角形态，
    ///    拆组件时看着像普通名字，实际落点和字面不一样；
    ///  - 接收目录本身可能是别人建的软链接 / 目录联接，组件看着在目录里，
    ///    真实落点在外面 —— 这种情况只有 `canonicalize` 能看出来。
    ///
    /// 已存在的部分用 `canonicalize`（它会解析软链接和 `..`），还不存在的部分
    /// 只能做词法校验（按组件比，且不许有 `..`）—— 两边都过了才放行。
    fn ensure_inside(&self, target: &Path) -> io::Result<()> {
        let base = self.dir.get();
        // 词法校验：目标必须在 base 下，且相对段里没有 ParentDir
        let rest = util::relative_under(target, &base).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("路径不在接收目录内：{}", target.display()),
            )
        })?;
        for component in rest.components() {
            debug_assert!(
                matches!(component, std::path::Component::Normal(_)),
                "resolve 返回的相对路径里混进了非普通组件"
            );
        }
        // 物理校验：最深的一个**已存在**的祖先 canonicalize 之后仍要在 base（也 canonicalize）里。
        // 目标自己可能还没创建（首块落盘前），那就逐级往上找一个存在的。
        let real_base = match std::fs::canonicalize(&base) {
            Ok(path) => path,
            // base 还没建出来（极少见）：词法校验已经是全部能做的了
            Err(_) => return Ok(()),
        };
        let mut probe = target.to_path_buf();
        loop {
            match std::fs::canonicalize(&probe) {
                Ok(real) => {
                    if !real.starts_with(&real_base) {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!(
                                "路径的真实落点在接收目录之外（可能是软链接）：{} -> {}",
                                target.display(),
                                real.display()
                            ),
                        ));
                    }
                    return Ok(());
                }
                Err(_) => match probe.parent() {
                    Some(parent) if parent != probe.as_path() => probe = parent.to_path_buf(),
                    _ => return Ok(()),
                },
            }
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
        // 2026-11-17：先按"块栅格"核对这一块。
        //
        // 落盘偏移是写死的 index * 1MiB，文件长度又由 set_len 提前占满，所以
        // **只要 index 或 length 有一个不对，写下去的东西就是错的，而文件大小看着完全正常**：
        //  - 短块（该 1MiB 只来 100KB）→ 这一块剩下的部分是 0，位图却会记成已完成；
        //  - 块号相同、内容不同（发送端把 A 块的数据配了 B 块的块号）→ 覆盖掉别人，
        //    另一块的位置永远是 0；
        //  - 块号越界 → 把文件写到声明大小之外。
        // 这三种以前全都静默通过，只能等收尾 SHA-256 报「大小一致、内容不同」。
        let expected_len = {
            let ledger = state
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            ledger.expected_len(index)
        };
        let expected_len = match expected_len {
            Some(expected) => expected,
            None => {
                if data.is_empty() {
                    // 空文件 / 末尾多出来的一块空块：不写盘、不记账，进度照样报上去
                    crate::log::debug(&format!(
                        "empty block {} ignored for {}",
                        index,
                        target.display()
                    ));
                    self.progress
                        .mark_received(channel, raw_path, index, total_size, 0);
                    PROGRESS.mark_received(channel, raw_path, index, total_size, 0);
                    return Ok(());
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "block {} is past the end of {} ({} bytes)",
                        index,
                        target.display(),
                        state.total
                    ),
                ));
            }
        };
        let len_ok = data.len() as u32 == expected_len;
        if !len_ok {
            crate::log::error(&format!(
                "{} {} #{} ({}: {} {}): {} {}",
                crate::i18n::tr(
                    "块长度与文件栅格不符，这一段会留在盘上是空/旧的",
                    "block length does not match the file grid; this range stays zero/stale"
                ),
                target.display(),
                index,
                crate::i18n::tr("应为", "expected"),
                expected_len,
                crate::i18n::tr("实际", "got"),
                data.len(),
                crate::i18n::tr("字节（继续写，收尾会按缺块报出来）", "bytes (kept, reported as a gap)")
            ));
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
        let (duplicate, written_bytes, total_bytes) = {
            let mut ledger = state
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let duplicate = ledger.record(index, data.len() as u32);
            (duplicate, ledger.written_bytes(), ledger.total_size)
        };
        if duplicate {
            // 同一个块号又来了一次：这就是「大小对、内容不同」最典型的现场（另一块被顶掉/从没来过）
            crate::log::warn(&format!(
                "{} {} #{} ({}/{} bytes)",
                crate::i18n::tr(
                    "重复的块号（同一个块收到两次，另一个块可能永远不来）",
                    "duplicate block index (same block arrived twice; another may never come)"
                ),
                target.display(),
                index,
                written_bytes,
                total_bytes
            ));
        }
        // 305：记下这一块已经落盘（下次中断时就能从这里续上）。
        // 只有长度对得上的块才记：短块记进去以后重传会被当成"已收到"跳过，那个洞就永远补不上。
        if len_ok {
            if let Ok(mut journal) = state.journal.lock() {
                if let Some(journal) = journal.as_mut() {
                    journal.mark(index as u64);
                }
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
        // 第二道防线（见 ensure_inside 的注释）：建目录同样会落在磁盘上，
        // 一个软链接就能把 create_dir_all 引到接收目录外面去，所以这里也要过
        self.ensure_inside(&target)?;
        fs::create_dir_all(target)?;
        Ok(())
    }

    fn state_for(&self, target: &Path, total_size: u64, modified_ms: i64) -> io::Result<Arc<FileState>> {
        // 第二道防线（见 ensure_inside 的注释）：这里是所有「落盘」的唯一入口
        // （写块、续传登记都走它），所以检查放这里最省事也最难绕过。
        self.ensure_inside(target)?;
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
        // 2026-11-17：账本也要把「盘上本来就有」的块补上，否则续传时收尾会把它们算成缺块
        let mut ledger = BlockLedger::new(total_size);
        if let Some(journal) = journal.as_ref() {
            let seeded = ledger.seed_from_bits(&journal.bits);
            if seeded > 0 {
                crate::log::info(&format!(
                    "{} {} {} {}",
                    crate::i18n::tr("续传：账本按盘上已有块补记", "resume: ledger seeded from disk"),
                    seeded,
                    crate::i18n::tr("块", "block(s)"),
                    target.display()
                ));
            }
        }
        let state = Arc::new(FileState {
            file,
            modified_ms: std::sync::atomic::AtomicI64::new(0),
            total: total_size,
            done: AtomicU64::new(0),
            ledger: Mutex::new(ledger),
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
                // 2026-11-17：以"块账本"为准判断传完没有。
                //
                // 以前这里看的是位图（journal.all_done()）：位图每块只有 1 bit，
                // 短块、漏标记都看不出来，于是出现过「文件明明完整、校验也通过了，
                // 旁边却留下一个 .hfxpart」（BUG-2），也出现过「位图说全了、
                // 其实中间有一块是 0」的坏文件。账本记的是每块的真实字节数，
                // 这两种情况都能对上账。
                let (complete, missing, short, bits) = {
                    let ledger = state
                        .ledger
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    (
                        ledger.is_complete(),
                        ledger.missing(),
                        ledger.short(),
                        ledger.to_bits(),
                    )
                };
                if complete {
                    // 每一块都按应有的长度落过盘了：续传日志不再需要
                    crate::journal::ReceiveJournal::remove(path);
                    continue;
                }
                if !keep_incomplete {
                    // 「不保留」：续传日志一行都不写，收尾时连文件一起删
                    dropped.push(path.clone());
                    continue;
                }
                // 用账本重建位图再落盘：位图漏掉的标记在这里被纠正，
                // 写出去的 .hfxpart 一定是真账（缺哪块就缺哪块，重传时补得回来）
                let mtime = {
                    let journal = state
                        .journal
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    journal
                        .as_ref()
                        .map(|journal| journal.mtime_ms)
                        .unwrap_or_else(|| state.modified_ms.load(Ordering::Relaxed))
                };
                let mut fresh = crate::journal::ReceiveJournal::new(state.total, mtime, BLOCK_SIZE);
                fresh.bits = bits;
                if let Err(e) = fresh.save(path) {
                    crate::log::warn(&format!(
                        "{} {}: {}",
                        crate::i18n::tr("续传日志写入失败", "cannot write resume journal"),
                        path.display(),
                        e
                    ));
                } else {
                    kept.push((path.clone(), got));
                    crate::log::warn(&format!(
                        "{} {} · {}",
                        crate::i18n::tr(
                            "保留未接收完的文件（下次可续传）",
                            "kept the incomplete file so it can resume"
                        ),
                        path.display(),
                        describe_gaps(&missing, &short)
                    ));
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
    ///
    /// 2026-11-17：位图不再直接抄日志里的那份，而是由"块账本"生成 ——
    /// 账本只有长度对得上的块才算完成。这样即使日志被写坏 / 漏了标记，
    /// 也不会把一块"只写了一半"的区域告诉发送端"已经有了"。
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
        let state = self.state_for(&target, total_size, modified_ms).ok()?;
        let bits = {
            let ledger = state
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            ledger.to_bits()
        };
        if bits.iter().all(|byte| *byte == 0) {
            return None;
        }
        Some(bits)
    }

    /// 2026-11-17：收尾前对一次"块账本"——哪些文件还有块没落盘 / 长度不够。
    ///
    /// 这是「两边大小一致、SHA-256 不一致」唯一能在电脑端被抓住的地方：
    /// 传输线程全部正常退出（手机端认为发完了）、文件大小也和声明的一模一样
    /// （set_len 预分配），但账本里还缺块 / 缺字节 —— 那就是落盘内容有洞。
    /// 返回 (路径, 缺块索引（最多列 16 个）, 缺块数, 短块列表, 短块数)。
    pub fn missing_blocks(&self) -> Vec<(PathBuf, Vec<u32>, usize, Vec<(u32, u32, u32)>, usize)> {
        let entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for (path, state) in entries.iter() {
            let ledger = state
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let missing = ledger.missing();
            let short = ledger.short();
            if missing.is_empty() && short.is_empty() {
                continue;
            }
            out.push((
                path.clone(),
                missing.iter().copied().take(16).collect(),
                missing.len(),
                short.iter().copied().take(16).collect(),
                short.len(),
            ));
        }
        out
    }
}

/// 缺块 / 短块的简要说明（写进日志）
fn describe_gaps(missing: &[u32], short: &[(u32, u32, u32)]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !missing.is_empty() {
        let list: Vec<String> = missing
            .iter()
            .take(8)
            .map(|index| index.to_string())
            .collect();
        parts.push(format!(
            "{} {} (#{}{})",
            crate::i18n::tr("缺", "missing"),
            missing.len(),
            list.join(" #"),
            if missing.len() > list.len() { " …" } else { "" }
        ));
    }
    if !short.is_empty() {
        let list: Vec<String> = short
            .iter()
            .take(4)
            .map(|(index, written, expected)| format!("#{}({}/{})", index, written, expected))
            .collect();
        parts.push(format!(
            "{} {} {}",
            crate::i18n::tr("长度不足", "short"),
            short.len(),
            list.join(" ")
        ));
    }
    parts.join(" · ")
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
    fn 每台设备的进度互不干扰() {
        // 主界面每台设备那一行读 progress_for(串号)，传窗口读全局 PROGRESS。
        // 这里只验「按设备的那一份确实是隔开的」—— 多设备并行时不能互相串数。
        let dir_a = temp_dir("dev-progress-a");
        let dir_b = temp_dir("dev-progress-b");
        let a = Receiver::with_device(SharedDir::new(util::normalize_dir(&dir_a)), "test-dev-a");
        let b = Receiver::with_device(SharedDir::new(util::normalize_dir(&dir_b)), "test-dev-b");
        let pa = progress_for("test-dev-a");
        let pb = progress_for("test-dev-b");
        pa.reset();
        pb.reset();

        let file_a = dir_a.join("a.bin").to_string_lossy().to_string();
        a.write_block(0, &file_a, 2_097_152, 0, 1_700_000_000_000, &[1u8; 1024])
            .unwrap();

        let (name_a, done_a, total_a, sess_a, files_a) = pa.snapshot();
        let (name_b, done_b, total_b, sess_b, files_b) = pb.snapshot();
        assert!(sess_a >= 1024, "A 的会话字节应该涨：{sess_a}");
        assert_eq!(files_a, 1, "A 应该记到一个文件");
        assert_eq!(total_a, 2_097_152);
        assert!(done_a > 0);
        assert_eq!(name_a, file_a);

        assert_eq!(sess_b, 0, "B 不该被 A 的写入影响");
        assert_eq!(files_b, 0);
        assert_eq!(done_b, 0);
        assert_eq!(total_b, 0);
        assert!(name_b.is_empty());
        let _ = b;
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

    /// 2026-11-17：短块 = 文件大小对、内容错（NEXT-TODO 第 11 条在落盘这一侧的成因）
    ///
    /// 落盘偏移是写死的 index * 1MiB，文件长度又由 set_len 提前占满，
    /// 所以"第 0 块只来了 1024 字节"这种事的后果是：文件大小完全正确、
    /// 中间一大段是 0，而位图以前还会把这一块记成"已完成"（重传永远跳过它）。
    #[test]
    fn short_block_leaves_a_hole_and_is_reported() {
        let dir = temp_dir("short-block");
        let receiver = Receiver::new(util::normalize_dir(&dir));
        let target = dir.join("short.bin");
        let path = target.to_string_lossy().to_string();
        let total = 2 * BLOCK_SIZE;
        let block = BLOCK_SIZE as usize;
        // 第 0 块只来了 1024 字节（本该 1MiB），第 1 块正常
        receiver
            .write_block(0, &path, total, 0, 1_700_000_000_000, &[1u8; 1024])
            .unwrap();
        receiver
            .write_block(0, &path, total, 1, 1_700_000_000_000, &vec![2u8; block])
            .unwrap();

        // 账本当场就能点名：没有整块缺失，但第 0 块只有 1024/1MiB
        let gaps = receiver.missing_blocks();
        assert_eq!(gaps.len(), 1, "有一个文件对不上账");
        assert_eq!(gaps[0].0, target);
        assert_eq!(gaps[0].1, Vec::<u32>::new(), "没有整块缺失");
        assert_eq!(gaps[0].3, vec![(0u32, 1024u32, block as u32)], "第 0 块长度不足");

        // 盘上的样子：大小一模一样，第 0 块的尾巴是 0
        let data = fs::read(&target).unwrap();
        assert_eq!(data.len() as u64, total);
        assert_eq!(&data[..1024], &[1u8; 1024]);
        assert!(
            data[1024..block].iter().all(|byte| *byte == 0),
            "短块后面的洞就是「大小对、内容错」"
        );

        // 收尾：这一块不许算完成 —— 续传位图里第 0 块保持"没收到"，重传才补得回来
        assert_eq!(receiver.finish_with(true).unwrap().0, 1);
        let sidecar = crate::journal::ReceiveJournal::sidecar_path(&target);
        let raw = fs::read(&sidecar).unwrap();
        let bitmap = &raw[32..];
        assert_eq!(
            bitmap[0] & 0b0000_0001,
            0,
            "第 0 块没写全，不能告诉发送端已经有了"
        );
        assert_eq!(bitmap[0] & 0b0000_0010, 0b10, "第 1 块是完整的");
    }

    /// 2026-11-17：同一个块号来了两次、另一个块号一次都没来。
    ///
    /// 后果和 NEXT-TODO 第 11 条现场一模一样：**收到的字节数刚好等于文件大小**
    /// （所以"数据收齐 599.0 MB"这样的日志照样成立），文件大小也完全正确，
    /// 但中间有一块从来没落盘。只看字节数 / 只看大小的判断永远发现不了它，
    /// 只有"每块写了多少字节"的账本能。
    #[test]
    fn duplicate_block_hides_a_missing_one() {
        let dir = temp_dir("duplicate");
        let receiver = Receiver::new(util::normalize_dir(&dir));
        let target = dir.join("dup.bin");
        let path = target.to_string_lossy().to_string();
        let total = 2 * BLOCK_SIZE;
        let block = BLOCK_SIZE as usize;
        receiver
            .write_block(0, &path, total, 0, 1_700_000_000_000, &vec![1u8; block])
            .unwrap();
        // 第 0 块又来一遍（内容不同），第 1 块永远没来
        receiver
            .write_block(0, &path, total, 0, 1_700_000_000_000, &vec![9u8; block])
            .unwrap();

        assert_eq!(
            receiver.bytes(),
            total,
            "字节数刚好等于文件大小 —— 只数字节永远发现不了这个洞"
        );
        let gaps = receiver.missing_blocks();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].1, vec![1u32], "缺的是第 1 块");
        assert_eq!(gaps[0].2, 1);

        let data = fs::read(&target).unwrap();
        assert_eq!(data.len() as u64, total);
        assert!(
            data[..block].iter().all(|byte| *byte == 9),
            "第 0 块留下的是后到的那份数据"
        );
        assert!(
            data[block..].iter().all(|byte| *byte == 0),
            "第 1 块从没来过，整块都是 0"
        );
    }

    /// 2026-11-17：块号越界（不属于这个文件）必须当场报错，
    /// 不能靠写盘把文件"撑"到声明大小之外
    #[test]
    fn block_past_the_end_is_rejected() {
        let dir = temp_dir("past-end");
        let receiver = Receiver::new(util::normalize_dir(&dir));
        let target = dir.join("one.bin");
        let path = target.to_string_lossy().to_string();
        let error = receiver
            .write_block(0, &path, BLOCK_SIZE, 1, 1_700_000_000_000, &[1u8; 16])
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::metadata(&target).unwrap().len(), BLOCK_SIZE);
        assert_eq!(receiver.bytes(), 0, "越界的块一个字节都不能记");
    }

    /// 2026-11-17：多条通道并发写同一个文件的块，每块必须落在自己的偏移上。
    ///
    /// 这条把"多通道不会互相串数据"的前提钉住：写盘全程用 seek_write / write_at
    /// 定位写（不是共享 File + seek + write），所以并发偏移是安全的。
    #[test]
    fn concurrent_blocks_land_on_their_own_offsets() {
        use std::sync::Arc;
        use std::thread;
        const BLOCKS: u32 = 8;
        let dir = temp_dir("concurrent");
        let receiver = Arc::new(Receiver::new(util::normalize_dir(&dir)));
        let target = dir.join("multi.bin");
        let path = target.to_string_lossy().to_string();
        let total = BLOCKS as u64 * BLOCK_SIZE;
        let block = BLOCK_SIZE as usize;
        let workers: Vec<_> = (0..BLOCKS)
            .map(|index| {
                let receiver = Arc::clone(&receiver);
                let path = path.clone();
                thread::spawn(move || {
                    // 每块一种填充值：串了立刻看得出来
                    let data = vec![(index + 1) as u8; block];
                    receiver
                        .write_block(index as u8, &path, total, index, 1_700_000_000_000, &data)
                        .unwrap();
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(receiver.missing_blocks().is_empty(), "八块都该收齐");
        let data = fs::read(&target).unwrap();
        assert_eq!(data.len() as u64, total);
        for index in 0..BLOCKS as usize {
            let start = index * block;
            assert!(
                data[start..start + block]
                    .iter()
                    .all(|byte| *byte as usize == index + 1),
                "第 {} 块被别的通道串了",
                index
            );
        }
    }

    #[test]
    fn paths_outside_are_flattened() {
        let dir = temp_dir("outside");
        let normalized = util::normalize_dir(&dir);
        let receiver = Receiver::new(normalized.clone());
        let target = receiver.resolve("/etc/elsewhere/file.txt");
        assert!(util::relative_under(&target, &normalized).is_some());
        assert_eq!(target.file_name().unwrap(), "file.txt");
    }
}