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

/// 308：两端共用一个限速值 —— 谁说了算。
///
/// `local` 是本机配置值，`remote` 是握手时对端报上来的值（MB/s）：
///   * 本机非 0：用本机的（用户就在这台机器上改的，最直观）；
///   * 本机为 0（「我这端不限速」）：才用对端的。
///
/// 307 那套是取「非零里最小的」—— 两端算出来的生效值一样，但界面上各写各的数，
/// 用户看到的和实际跑的对不上（电脑写 20、手机写 5、实际 5，两边都还显示自己的），
/// 所以 308 改成这一个值：定下来之后两端都存它、都显示它、都按它跑。
///
/// 0 仍然只是「不限速」，不是「限速到 0」。
pub fn shared_value(local: i64, remote: i64) -> i64 {
    if local > 0 { local } else { remote.max(0) }
}

/// 配置值的代次：本机用户每改一次 +1，没变不加。
///
/// 308 起限速是**两端共用一个值**：谁改了，谁就把新值推给对方。会话线程不直接
/// 由界面去写 socket（那是跨线程抢同一根控制通道），而是每转一圈比对一次代次，
/// 发现变了就发一帧 SPEED_LIMIT。代次只在「本机用户改」时增长 —— 采纳对端发来的值
/// 不动它，否则两端会互相把同一个值推来推去。
static CONFIGURED_EPOCH: AtomicI64 = AtomicI64::new(0);

/// 本机用户改了限速（配置文件 + 界面）。
///
/// 立即对这条发送路径生效**并且**提升代次 —— 连着的会话会在下一次轮询里把新值
/// 推给手机，两端从此显示同一个数（307 那套是各报各的、跑 min，两边数字可以不一样）。
pub fn set_configured_mb(mb: i64) {
    let mb = mb.max(0);
    let old = CONFIGURED_MB.swap(mb, Ordering::Relaxed);
    if old != mb {
        CONFIGURED_EPOCH.fetch_add(1, Ordering::Relaxed);
        set_limit(mb * 1024 * 1024);
    }
}

/// 采纳对端同步过来的值（握手或 SPEED_LIMIT 帧）：改配置 + 立即生效，**不**提升代次。
///
/// 不提升是有意的：这个值本来就是对方定的，再推回去只会让两端把同一个数
/// 来回发一辈子（每 250ms 一帧，控制通道上全是废话）。
pub fn adopt_configured_mb(mb: i64) {
    let mb = mb.max(0);
    if CONFIGURED_MB.swap(mb, Ordering::Relaxed) != mb {
        set_limit(mb * 1024 * 1024);
    }
}

/// 本机配置值的代次（会话线程用它判断「该不该把新值推给手机」）。
pub fn configured_epoch() -> i64 {
    CONFIGURED_EPOCH.load(Ordering::Relaxed)
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

/// 测试用的全局量互斥锁：LIMIT_BPS / CONFIGURED_MB / 代次都是进程级的，
/// 而 cargo test 并行跑 —— 不排队的话，一个用例刚设好的上限会被另一个用例的
/// set_configured_mb 顺手改掉，于是偶发失败（真出现过）。
/// session.rs 里的握手用例也在动这套量，所以它是 pub(crate) 的。
#[cfg(test)]
pub(crate) static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    fn serial() -> MutexGuard<'static, ()> {
        TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn shared_value_prefers_the_machine_that_set_it() {
        // 本机填了就以本机为准（不再是 307 的「取小的」：那样两端显示的数对不上）
        assert_eq!(shared_value(10, 5), 10);
        assert_eq!(shared_value(5, 10), 5);
        // 本机是 0（不限速）才听对端的
        assert_eq!(shared_value(0, 7), 7);
        assert_eq!(shared_value(7, 0), 7);
        // 两端都不限：不限速
        assert_eq!(shared_value(0, 0), 0);
        // 负数一律当 0，不能把负值当限速值传下去
        assert_eq!(shared_value(-3, 9), 9);
    }

    #[test]
    fn local_change_bumps_epoch_but_adopting_does_not() {
        let _serial = serial();
        // 308：本机用户改值要提升代次（会话线程据此推给手机），
        // 采纳对端同步来的值不能提升（否则两端把同一个数来回推一辈子）。
        let before = configured_epoch();
        set_configured_mb(12);
        let after_local = configured_epoch();
        assert_eq!(after_local, before + 1, "本机改值必须提升代次");
        assert_eq!(configured_mb(), 12);
        assert_eq!(limit(), 12 * 1024 * 1024, "本机改值要立即对发送路径生效");

        adopt_configured_mb(30);
        assert_eq!(configured_epoch(), after_local, "采纳对端不得提升代次");
        assert_eq!(configured_mb(), 30);
        assert_eq!(limit(), 30 * 1024 * 1024);

        // 同一个值再设一次：代次不动（否则每帧都在发重复的同步帧）
        set_configured_mb(30);
        assert_eq!(configured_epoch(), after_local);

        // 收尾：别把测试值留给后面的用例
        set_configured_mb(0);
    }

    #[test]
    fn zero_limit_never_sleeps() {
        let _serial = serial();
        set_limit(0);
        let started = Instant::now();
        for _ in 0..100 {
            throttle(1024);
        }
        assert!(started.elapsed().as_millis() < 50);
    }

    /// 暂停期间不许攒"免费额度"：恢复之后那一次限速必须照常生效。
    ///
    /// 真机踩到过（限 5MB/s、200MB 传一半暂停 17 秒，恢复后整单只花 23 秒 =
    /// 平均 8.7MB/s，多出来正好是 17.3 × 5 ≈ 85MB）。
    #[test]
    fn resuming_after_pause_does_not_grant_free_quota() {
        let _serial = serial();
        set_limit(1024 * 1024);
        // 第一块是即时额度，不睡
        throttle(1024 * 1024);
        // 暂停 → 恢复：控制层必须把计时窗作废，否则下面那块的 elapsed 里
        // 会带着这段空闲时间，算出来的债是负的，一个字节都不用睡。
        crate::control::pause(true);
        crate::control::pause(false);
        let started = Instant::now();
        throttle(1024 * 1024);
        let took = started.elapsed().as_millis();
        crate::control::reset();
        set_limit(0);
        assert!(took >= 800, "暂停恢复后攒出了免费额度，只花了 {took}ms");
    }

    #[test]
    fn heavy_limit_actually_sleeps() {
        let _serial = serial();
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
