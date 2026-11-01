//! 待发送队列：Explorer「发送到 → 手机」把文件塞进来，手机轮询时取走。

use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

static PENDING: LazyLock<Mutex<Vec<PathBuf>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// 追加一批待发送文件（已存在的去重）
pub fn push(paths: Vec<PathBuf>) {
    if let Ok(mut guard) = PENDING.lock() {
        for path in paths {
            if !guard.iter().any(|item| item == &path) {
                guard.push(path);
            }
        }
    }
}

/// 当前待发送列表（路径 + 文件大小），不发出去那部分
pub fn snapshot() -> Vec<(PathBuf, u64)> {
    let Ok(guard) = PENDING.lock() else {
        return Vec::new();
    };
    guard
        .iter()
        .map(|path| {
            let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
            (path.clone(), size)
        })
        .collect()
}

/// 取走全部待发送项（手机真正开始拉取时调用，避免重复推送）
pub fn take_all() -> Vec<PathBuf> {
    PENDING
        .lock()
        .map(|mut guard| guard.drain(..).collect())
        .unwrap_or_default()
}

pub fn is_empty() -> bool {
    PENDING.lock().map(|guard| guard.is_empty()).unwrap_or(true)
}
