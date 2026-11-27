//! Small utilities: timestamps, byte formatting, path helpers.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Returns (local "HH:MM:SS", utc "YYYY-MM-DD HH:MM:SS").
pub fn timestamps() -> (String, String) {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let utc = format_utc(since_epoch.as_secs() as i64, since_epoch.subsec_millis());
    let local = local_hms();
    (local, utc)
}

fn format_utc(unix_secs: i64, millis: u32) -> String {
    let days = unix_secs.div_euclid(86_400);
    let rem = unix_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} UTC",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        millis
    )
}

/// Howard Hinnant's civil_from_days algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

#[cfg(windows)]
fn local_hms() -> String {
    #[repr(C)]
    #[derive(Default)]
    struct WinSystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    extern "system" {
        fn GetLocalTime(lp_system_time: *mut WinSystemTime);
    }

    let mut st = WinSystemTime::default();
    unsafe { GetLocalTime(&mut st) };
    // 带上毫秒：这一轮排查的是「取消之后到下一单开跑隔了几秒」这种秒级延迟，
    // 只到秒的日志根本分不清是哪一段慢（用户的体感和日志对不上）。
    format!("{:02}:{:02}:{:02}.{:03}", st.hour, st.minute, st.second, st.milliseconds)
}

#[cfg(not(windows))]
fn local_hms() -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let secs = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        (secs % 86_400) / 3600,
        (secs % 3600) / 60,
        secs % 60,
        millis
    )
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{:.0}{}", value, UNITS[unit])
    } else {
        format!("{:.2}{}", value, UNITS[unit])
    }
}

pub fn format_speed(bytes_per_second: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_second.max(0.0) as u64))
}

/// Normalise a directory to an absolute path, keeping a trailing separator.
pub fn normalize_dir(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut s = abs.to_string_lossy().to_string();
    if !s.ends_with(std::path::MAIN_SEPARATOR) {
        s.push(std::path::MAIN_SEPARATOR);
    }
    PathBuf::from(s)
}

/// 按**组件**判断 `path` 是否在 `base` 里面；是则返回 base 之后那一段（只含普通名字）。
///
/// 这是**路径穿越防护**，不是可以省掉的冗余判断，别当成「前缀判断换个写法」：
///
/// 以前用字符串前缀（`path_starts_with`）判「在不在接收目录里」，手机发来
/// `C:\接收\..\..\..\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\x.exe`
/// 时字符串前缀确实是 `C:\接收\`，判成「在里面」原样放行；落盘时**操作系统**才按
/// `..` 一路往上走，文件就写到接收目录外面去了。接收端是零认证、同一个 Wi-Fi 里
/// 谁都能连的，这条路径等于「陌生人可写你的自启动目录，重启即执行」。
///
/// 所以这里按 `Component` 走：base 的每一段都要和 path 对得上（Windows 不分大小写），
/// 剩下的只能是普通名字 —— 出现 `..`（ParentDir）、`.`、盘符、根，一律判「不在里面」，
/// 由调用方按「拍平到接收目录」处理。返回的相对段随后是 `base.join(...)` 拼回去的，
/// 组件里没有 `..`，拼出来的路径词法上不可能跑到 base 之外。
pub fn relative_under(path: &Path, base: &Path) -> Option<PathBuf> {
    let base_parts = components_lower(base);
    let path_parts = components_lower(path);
    if base_parts.is_empty() || path_parts.len() < base_parts.len() {
        return None;
    }
    if path_parts[..base_parts.len()] != base_parts[..] {
        return None;
    }
    // 剩下的每一段都必须是普通名字：`..` / `.` / 盘符 / 根全都不接受
    let mut rest = PathBuf::new();
    for component in path.components().skip(base_parts.len()) {
        match component {
            std::path::Component::Normal(name) => rest.push(name),
            _ => return None,
        }
    }
    Some(rest)
}

/// 路径的每一段（转小写），用来做「按组件」的前缀比较。
fn components_lower(path: &Path) -> Vec<String> {
    path.components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy().to_string();
            #[cfg(windows)]
            {
                s.to_lowercase()
            }
            #[cfg(not(windows))]
            {
                s
            }
        })
        .collect()
}

/// 最后一段普通名字（`..` / `.` / 根结尾都算没有名字）—— 拍平时用它。
///
/// 用 `components().next_back()` 而不是 `file_name()`：两者对「`a\..`」这种结尾的
/// 结果一致（都没有名字），但这里能明确表达「我只接受 Normal」，不会哪天有人把
/// CurDir / ParentDir 当成名字拼出去。
pub fn last_normal_name(path: &Path) -> Option<std::ffi::OsString> {
    match path.components().next_back() {
        Some(std::path::Component::Normal(name)) => Some(name.to_os_string()),
        _ => None,
    }
}

// 这里原来有个 path_starts_with()（字符串前缀判断）。**已经删掉，不要加回来**：
// 它就是路径穿越漏洞本身 —— 字符串前缀能对上、但路径里带 `..` 时，前缀判断说
// 「在接收目录里」，操作系统落盘时却按 `..` 走到目录外面。判「在不在里面」一律用
// relative_under()（按组件比，只认普通名字）。

#[allow(dead_code)]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as i64
}