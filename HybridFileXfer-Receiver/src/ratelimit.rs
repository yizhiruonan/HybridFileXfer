//! 全局限速：按字节令牌桶限制**本机发送**速度，0 表示不限速。
//!
//! 和手机端的 `core/SpeedLimiter.java` 是同一套算法，两边必须同构 ——
//! 否则「协商出来的同一个上限」在两端跑出来的实际速率会不一样。
//!
//! ## 算法
//!
//! 记下本轮从「开始」到现在的累计发送字节数 `sent`，按限速值算出这些字节
//! 「本该花掉」的时间 `expected = sent / limit`。实际已经过去 `elapsed`。
//! `expected > elapsed` 就是欠了债，把差额睡满。
//!
//! 这是**累计式**而不是「每秒清一次计数器」式的令牌桶，两个关键性质：
//!   1. 债会累积。突发之后超发的部分会被后续的睡眠补回来，不会变成永久的免费额度；
//!   2. 多通道共享同一个全局账本。N 条通道并发时，**总**速率才等于 limit，
//!      而不是每条通道各 limit（后者实测限 1MB/s 能跑到 ~60MB/s）。
//!
//! ## 踩过的坑（手机端注释里记着，这里同步一份，别改回去）
//!
//!   * 单位必须统一到纳秒。旧写法算出**微秒**却拿去和**纳秒**比，差 1000 倍，
//!     `expected - elapsed` 永远为负，于是永不睡眠 —— 限速形同虚设。
//!   * 债务必须**睡满**。旧写法只睡 `min(债务, 100ms)`，剩下的不累积补偿，
//!     等效于「每块最多停 100ms」，6 通道并发时限 1MB/s 实测跑到 ~60MB/s。
//!     现在是分段睡（每段 ≤100ms），既睡满债，又能及时响应取消。

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 本机**配置**的上限（MB/s），0 = 不限速。
///
/// 和 [LIMIT_BPS] 分开存是有意的：这个是「本机想限到多少」，握手时要报给对方；
/// [LIMIT_BPS] 是「协商之后实际生效多少」。两者可以不同 ——
/// 比如本机填 20、手机填 5，配置仍是 20，但实际只跑 5。
static CONFIGURED_MB: AtomicI64 = AtomicI64::new(0);

/// 当前生效的上限（字节/秒）。0 = 不限速。
///
/// 注意这里存的是**协商后**的值，不是本机配置值：双端握手时两边各报自己配的
/// 上限，取「非零里的最小值」，两端算出同一个数（见 [effective]）。
static LIMIT_BPS: AtomicI64 = AtomicI64::new(0);

/// 本轮的计时窗：(起点, 起点之后发出去的字节数)。
///
/// `None` = 还没开始记，第一块到达时惰性建立。
static WINDOW: Mutex<Option<(Instant, u64)>> = Mutex::new(None);

/// 协商：两端各报一个上限，取「非零里最小的那个」；都为 0 就是都不限速。
///
/// 0 在这套协议里的语义是「我这端不限速」，**不是**「限速到 0」，
/// 所以它不能参与 min —— 否则一端填 0 就会把另一端的限制抹掉。
pub fn effective(local: i64, remote: i64) -> i64 {
    match (local > 0, remote > 0) {
        (true, true) => local.min(remote),
        (true, false) => local,
        (false, true) => remote,
        (false, false) => 0,
    }
}

/// 把配置文件里的 `speed_limit_mb` 灌进来（握手要用它去报数）。
///
/// **只记配置值，不动生效值**。生效值一律由握手协商写入 ——
/// 否则用户在传输中途改了本机设置，会把「手机那边更小的限制」直接掀掉，
/// 那一轮就变成了单边限速（本来协商的是 5，改完立刻按 20 跑）。
/// 本协议里所有发送都发生在握手之后，所以「等握手」不会漏掉任何一次发送。
pub fn set_configured_mb(mb: i64) {
    CONFIGURED_MB.store(mb.max(0), Ordering::Relaxed);
}

/// 本机配置的上限（MB/s），握手时报给对方。
pub fn configured_mb() -> i32 {
    CONFIGURED_MB.load(Ordering::Relaxed) as i32
}

/// 设定协商后的上限（字节/秒），并按手机端 `setLimitBytesPerSecond` 的语义重置计时窗。
pub fn set_limit(bps: i64) {
    let bps = bps.max(0);
    LIMIT_BPS.store(bps, Ordering::Relaxed);
    reset_window();
}

pub fn limit() -> i64 {
    LIMIT_BPS.load(Ordering::Relaxed)
}

/// 每次传输开始时重置计时窗。
///
/// 不清的话，`set_limit` 之后隔很久才发起传输，elapsed 里积累的"空闲时间"
/// 会变成等额的免费流量额度（限 10MB/s、隔 90 秒再传，前 900MB 完全不睡）。
pub fn reset_window() {
    if let Ok(mut guard) = WINDOW.lock() {
        *guard = None;
    }
}

/// 刚要写出 `bytes` 字节：把该欠的债睡满再返回。
///
/// 调用点是**每写完一个块之后**（和手机端 `SendFileCall` 里那次调用对称），
/// 不是每写一个 TCP 报文 —— 块是一个 1MB 的完整单位，粒度足够。
pub fn throttle(bytes: usize) {
    let limit = LIMIT_BPS.load(Ordering::Relaxed);
    if limit <= 0 || bytes == 0 {
        return;
    }
    let mut sleep_ms = {
        let mut guard = match WINDOW.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        let now = Instant::now();
        let (start, sent) = guard.unwrap_or((now, 0));
        let sent = sent + bytes as u64;
        *guard = Some((start, sent));
        // 全程纳秒：expected 是「发完 sent 字节本该花掉的时间」
        let elapsed_ns = now.duration_since(start).as_nanos() as i64;
        let expected_ns = sent as i64 * 1_000_000_000 / limit;
        (expected_ns - elapsed_ns) / 1_000_000
    };
    if sleep_ms <= 0 {
        return;
    }
    // 分段睡：睡满债务，但每段不超过 100ms，取消时能在 100ms 内退出。
    while sleep_ms > 0 {
        if crate::control::cancelled() || crate::control::paused() {
            // 暂停 / 取消：把这一轮的账本作废，恢复后重新开始计时 ——
            // 否则暂停期间 elapsed 一直在涨，恢复瞬间会攒出一大笔"免费额度"。
            reset_window();
            return;
        }
        let slice = sleep_ms.min(100);
        std::thread::sleep(Duration::from_millis(slice as u64));
        sleep_ms -= slice;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_takes_smaller_nonzero() {
        // 都填了：取小的
        assert_eq!(effective(10, 5), 5);
        assert_eq!(effective(5, 10), 5);
        // 一端填 0（不限速）：听另一端的
        assert_eq!(effective(0, 7), 7);
        assert_eq!(effective(7, 0), 7);
        // 两端都不限：不限速
        assert_eq!(effective(0, 0), 0);
    }

    #[test]
    fn zero_limit_never_sleeps() {
        set_limit(0);
        let started = Instant::now();
        for _ in 0..100 {
            throttle(1024);
        }
        assert!(started.elapsed().as_millis() < 50);
    }

    #[test]
    fn heavy_limit_actually_sleeps() {
        // 1MB/s 理论上 2MB 要睡约 1000ms（第一块是即时额度，第二块要补）
        set_limit(1024 * 1024);
        let started = Instant::now();
        throttle(1024 * 1024);
        throttle(1024 * 1024);
        let took = started.elapsed().as_millis();
        assert!(took >= 800, "限速没生效，只花了 {took}ms");
        set_limit(0);
    }
}
