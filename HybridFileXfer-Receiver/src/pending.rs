//! 待发送队列：Explorer「发送到 → 手机」把文件塞进来，手机轮询时取走。
//!
//! 多设备：每条可以指定目标设备（串号）。没有指定时任何一台都能取走（谁先来谁拿到）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

#[derive(Clone)]
pub struct Item {
    pub path: PathBuf,
    /// None = 任意设备
    pub target: Option<String>,
}

static PENDING: LazyLock<Mutex<Vec<Item>>> = LazyLock::new(|| Mutex::new(Vec::new()));
/// 暂扣开关：多台设备在线时先不把手里的文件交出去，等用户选好设备
static HOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 推送代次：每来一批新推送就 +1。
///
/// GUI 主循环靠它认「有新推送进来了」—— 投递是从托盘线程（WM_COPYDATA）或启动时的
/// 命令行进来的，界面线程不该去解析那条消息，比较这个数字变没变就够了。
/// 代次一变 = 一批全新的推送：小窗回第 1 屏重新开始，不带上一次的选择。
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn set_hold(on: bool) {
    HOLD.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// 撤销暂扣（用户取消 / 关掉小窗时用）。
///
/// 撤销之后条目不标目标也能被任意设备取走 —— 文件照旧留在队列里
/// （用户要求「取消不解散队列」），只是不再卡在那儿等人选。
pub fn clear_hold() {
    HOLD.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// 追加一批待发送文件（已存在的去重）
pub fn push(paths: Vec<PathBuf>) {
    push_with_target(paths, None);
}

/// 追加一批待发送文件，指定目标设备（串号）
pub fn push_with_target(paths: Vec<PathBuf>, target: Option<String>) {
    let empty = paths.is_empty();
    // 没点名目标：**在同一个临界区里**就把暂扣打开。
    //
    // 不能等界面线程 150ms 后再扣 —— 手机侧 CHECK_PENDING 是立刻来的，
    // 中间那 150ms 足够它把文件取走：真机上「选设备」小窗刚弹出来，文件已经
    // 传给其中一台了（2026-11-21 撞见，推送 80MB 直接飞给了小米平板）。
    if target.is_none() && !empty {
        HOLD.store(true, Ordering::Relaxed);
    }
    // 点名了目标就记进 control：界面靠它分辨「这一轮是谁在收」（见 control::send_target_is）
    if !empty {
        crate::control::set_send_target(target.as_deref());
    }
    if let Ok(mut guard) = PENDING.lock() {
        for path in paths {
            if !guard.iter().any(|item| item.path == path) {
                guard.push(Item {
                    path,
                    target: target.clone(),
                });
            }
        }
    }
    // 代次**最后**才 +1，而且放在锁外面。
    //
    // 顺序在这里是有语义的：界面线程看到代次变了就会去读队列（pending::totals() 量文件数和
    // 字节数）。先 +1 再入队的话，那一帧读到的还是空队列 —— 真机上小窗标题下面写着
    // 「0 B」（文件明明推过去了），就是踩的这个。先入队、后 +1，读的人一定看得到条目。
    //
    // 而按「收到一批」算、不按「真加进去几条」算：同一个文件再推一次会被去重跳过，
    // 但用户确实又推了一次，界面照样得弹出来（否则第二次推送毫无反应，像程序死了）。
    if !empty {
        GENERATION.fetch_add(1, Ordering::Release);
    }
}

fn matches(item: &Item, device: &str) -> bool {
    match &item.target {
        None => true,
        Some(target) => device.is_empty() || target == device,
    }
}

/// 某台设备视角的待发送列表（路径 + 大小）
pub fn snapshot_for(device: &str) -> Vec<(PathBuf, u64)> {
    let Ok(guard) = PENDING.lock() else {
        return Vec::new();
    };
    // 暂扣期间且还没指定目标 → 谁都别拿，等界面里选
    if HOLD.load(std::sync::atomic::Ordering::Relaxed) && guard.iter().any(|i| i.target.is_none()) {
        return Vec::new();
    }
    guard
        .iter()
        .filter(|item| matches(item, device))
        .map(|item| {
            let size = std::fs::metadata(&item.path).map(|m| m.len()).unwrap_or(0);
            (item.path.clone(), size)
        })
        .collect()
}

/// 取走属于该设备的条目
pub fn take_for(device: &str) -> Vec<PathBuf> {
    let Ok(mut guard) = PENDING.lock() else {
        return Vec::new();
    };
    let mut taken = Vec::new();
    guard.retain(|item| {
        if matches(item, device) {
            taken.push(item.path.clone());
            false
        } else {
            true
        }
    });
    taken
}

/// 队列里是否有"未指定目标"的条目（多设备时需要让用户选）
pub fn needs_target() -> bool {
    PENDING
        .lock()
        .map(|guard| guard.iter().any(|item| item.target.is_none()))
        .unwrap_or(false)
}

/// 把未指定目标的条目统一定到某台设备
pub fn assign_all(target: &str) {
    if let Ok(mut guard) = PENDING.lock() {
        for item in guard.iter_mut() {
            if item.target.is_none() {
                item.target = Some(target.to_string());
            }
        }
    }
}

/// 多选：把未指定目标的条目**复制成 N 份**，每台一份。
///
/// 为什么要复制而不是「记一张目标表」：取件是按条目过滤的（take_for 只挑 target
/// 匹配自己的那些），一条只有一个 target，复制成每台一条才能让 N 台各取各的。
/// 复制的是路径（不是文件内容），手机来取时各自从磁盘读 —— 2 台就是各传一遍，
/// 这是「发到多台」本来就该有的语义。
pub fn assign_to(targets: &[String]) {
    if targets.is_empty() {
        return;
    }
    let Ok(mut guard) = PENDING.lock() else {
        return;
    };
    let mut out: Vec<Item> = Vec::with_capacity(guard.len() * targets.len().max(1));
    for item in guard.drain(..) {
        if item.target.is_some() {
            out.push(item);
            continue;
        }
        for target in targets {
            // 同一路径已经点过这台就别再加（重复推送同文件时也会走到这儿）
            if out
                .iter()
                .any(|other| other.path == item.path && other.target.as_deref() == Some(target))
            {
                continue;
            }
            out.push(Item {
                path: item.path.clone(),
                target: Some(target.clone()),
            });
        }
    }
    *guard = out;
}

/// 当前推送代次（每收到一批新推送 +1）。见 {@link GENERATION}。
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// 队列里已经点名要的那台设备（`--to <串号>` 推过来的）。
///
/// 点名了就不用再问用户「发送到哪台」——推送小窗直接进第 2 屏。
/// 混着点名和没点名的条目时返回 None（剩下的还得选）。
pub fn sole_target() -> Option<String> {
    let Ok(guard) = PENDING.lock() else {
        return None;
    };
    let mut found: Option<String> = None;
    for item in guard.iter() {
        match &item.target {
            None => return None,
            Some(target) => {
                if let Some(prev) = &found {
                    if prev != target {
                        return None;
                    }
                } else {
                    found = Some(target.clone());
                }
            }
        }
    }
    found
}

/// 队列里的条目数 + 字节总数（不管目标、不管暂扣）。
///
/// 推送小窗第 1 屏标题下那行「3 个文件 · 128.4 MB」用它 ——
/// 走 snapshot_for 会被暂扣逻辑挡掉（多台设备、还没选目标时它返回空），量不出总数。
pub fn totals() -> (usize, u64) {
    let Ok(guard) = PENDING.lock() else {
        return (0, 0);
    };
    let mut files = 0usize;
    let mut bytes = 0u64;
    for item in guard.iter() {
        files += 1;
        bytes += std::fs::metadata(&item.path).map(|m| m.len()).unwrap_or(0);
    }
    (files, bytes)
}

/// 某台设备视角的条目数 + 字节总数（和 snapshot_for 同一套匹配规则，但不看暂扣）。
pub fn totals_for(device: &str) -> (usize, u64) {
    let Ok(guard) = PENDING.lock() else {
        return (0, 0);
    };
    let mut files = 0usize;
    let mut bytes = 0u64;
    for item in guard.iter().filter(|item| matches(item, device)) {
        files += 1;
        bytes += std::fs::metadata(&item.path).map(|m| m.len()).unwrap_or(0);
    }
    (files, bytes)
}

/// 把已指定目标的条目退回「任意设备」。
///
/// **只在「一个字节都没发出去」的失败上用**（连不上 / 一直不来取 / 取消发送）：
/// 这一轮没人拿到文件，条目却已经点过名，不退回去就永远只有那台（还不存在的）
/// 设备能取 —— 队列看起来卡死，下次推送也带不动它。
///
/// 反过来，传了一半的失败**不能**调用它：真机上就是这样把 25MB 的 apk
/// 顺手塞给了另一台手机（另一台立刻 CHECK_PENDING 取走了），用户以为发的是 A，实际 B 收到了。
pub fn release_targets() {
    if let Ok(mut guard) = PENDING.lock() {
        for item in guard.iter_mut() {
            item.target = None;
        }
    }
}

pub fn is_empty() -> bool {
    PENDING.lock().map(|g| g.is_empty()).unwrap_or(true)
}
