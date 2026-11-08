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
        if (sleepMillis > 0) {
            try {
                Thread.sleep(Math.min(sleepMillis, 100));
            } catch (InterruptedException ignored) {
            }
        }
    }
}
