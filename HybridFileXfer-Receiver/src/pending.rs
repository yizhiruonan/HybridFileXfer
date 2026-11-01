//! 待发送队列：Explorer「发送到 → 手机」把文件塞进来，手机轮询时取走。
//!
//! 多设备：每条可以指定目标设备（串号）。没有指定时任何一台都能取走（谁先来谁拿到）。

use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

#[derive(Clone)]
pub struct Item {
    pub path: PathBuf,
    /// None = 任意设备
    pub target: Option<String>,
}

static PENDING: LazyLock<Mutex<Vec<Item>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// 追加一批待发送文件（已存在的去重）
pub fn push(paths: Vec<PathBuf>) {
    push_with_target(paths, None);
}

/// 追加一批待发送文件，指定目标设备（串号）
pub fn push_with_target(paths: Vec<PathBuf>, target: Option<String>) {
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

pub fn is_empty() -> bool {
    PENDING.lock().map(|g| g.is_empty()).unwrap_or(true)
}
