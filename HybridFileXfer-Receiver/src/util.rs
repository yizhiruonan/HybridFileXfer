//! Small utilities: timestamps, byte formatting, path helpers.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Returns (local "HH:MM:SS", utc "YYYY-MM-DD HH:MM:SS").
pub fn timestamps() -> (String, String) {
    let now = SystemTime::now();
    let secs = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let utc = format_utc(secs as i64);
    let local = local_hms();
    (local, utc)
}

fn format_utc(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let rem = unix_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
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
    format!("{:02}:{:02}:{:02}", st.hour, st.minute, st.second)
}

#[cfg(not(windows))]
fn local_hms() -> String {
    let now = SystemTime::now();
    let secs = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        (secs % 86_400) / 3600,
        (secs % 3600) / 60,
        secs % 60
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

/// Path equality that ignores ASCII case on Windows.
pub fn path_starts_with(path: &Path, base: &Path) -> bool {
    let p = path.to_string_lossy().replace('/', "\\");
    let b = base.to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    {
        let p = p.to_lowercase();
        let b = b.to_lowercase();
        return p.starts_with(&b);
    }
    #[cfg(not(windows))]
    {
        return p.starts_with(&b);
    }
}

#[allow(dead_code)]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as i64
}