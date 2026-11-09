package top.weixiansen574.hybridfilexfer.core;

/**
 * 全局限速：按字节令牌桶限制发送速度，0 表示不限速。
 */
public final class SpeedLimiter {
    private static volatile long limitBytesPerSecond;
    private static long startNanos = System.nanoTime();
    private static long sentBytes;

    private SpeedLimiter() {
    }

    public static void setLimitBytesPerSecond(long bytesPerSecond) {
        synchronized (SpeedLimiter.class) {
            limitBytesPerSecond = Math.max(0, bytesPerSecond);
            startNanos = System.nanoTime();
            sentBytes = 0;
        }
    }

    public static long getLimitBytesPerSecond() {
        return limitBytesPerSecond;
    }

    /**
     * 每次传输开始时重置计时窗。不清的话，setLimitBytesPerSecond 之后隔了很久才发起传输，
     * elapsed 里积累的"空闲时间"会变成等额的免费流量额度（限 10MB/s、隔 90 秒再传，
     * 前 900MB 完全不睡，实测 13MB/s 超速就是这么来的）。
     */
    public static void resetWindow() {
        synchronized (SpeedLimiter.class) {
            startNanos = System.nanoTime();
            sentBytes = 0;
        }
    }

    public static void throttle(int bytes) {
        if (limitBytesPerSecond <= 0 || bytes <= 0) {
            return;
        }
        long sleepMillis;
        synchronized (SpeedLimiter.class) {
            sentBytes += bytes;
            long elapsed = System.nanoTime() - startNanos;
            // expected 也是纳秒：按 limit 匀速发完 sentBytes 该花的时间。
            // 旧写法算出来是微秒、拿去和纳秒比，差 1000 倍 —— 永远不用睡，限速形同虚设。
            long expected = sentBytes * 1_000_000_000L / Math.max(1, limitBytesPerSecond);
            sleepMillis = (expected - elapsed) / 1_000_000L;
        }
        // 必须把债睡满：之前只睡 min(sleepMillis,100)ms，剩余的债不累积补偿，
        // 实际效果是"每块最多停 100ms"——6 通道并发实测限 1MB/s 能跑到 ~60MB/s。
        // 分段睡（每段 ≤100ms），既睡满债务又保持中断响应：
        // 通道被取消时 writeFully 会抛 ClosedChannelException 退出传输循环，
        // throttle 不再被调用，不会卡在睡眠里出不来。
        long remaining = sleepMillis;
        while (remaining > 0) {
            long slice = Math.min(remaining, 100);
            try {
                Thread.sleep(slice);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return;
            }
            remaining -= slice;
        }
    }
}
