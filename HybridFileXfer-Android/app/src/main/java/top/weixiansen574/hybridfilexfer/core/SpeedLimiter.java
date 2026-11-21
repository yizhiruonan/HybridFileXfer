package top.weixiansen574.hybridfilexfer.core;

/**
 * 全局限速：按字节令牌桶限制发送速度，0 表示不限速。
 */
public final class SpeedLimiter {
    /** 当前**生效**的上限（协商之后的值，0 = 不限速）。 */
    private static volatile long limitBytesPerSecond;
    /** 本机**配置**的上限（用户在设置里填的值，0 = 不限速）。
     * <p>
     * 和 limitBytesPerSecond 分开存是必要的：这个要报给电脑去协商，
     * 而协商出来的生效值可能比它小（电脑那边填了更小的）。
     * 如果只存一个，第二次握手就会把「协商结果」当成「我的配置」再报出去，
     * 限制会一轮比一轮紧，最后锁死。
     */
    private static volatile long configuredBytesPerSecond;
    /**
     * 配置值是否**真的初始化过**。
     * <p>
     * 光看 configuredBytesPerSecond == 0 分不清两件事：「用户就是不限速」和
     * 「这个进程还没从配置文件里读出来」。后者发生在服务进程刚拉起的瞬间，
     * 如果那时握手，手机就会骗电脑说「我不限速」，把用户设的上限**静默抹掉**。
     * 所以单独记一个标志，未初始化时由握手方改用持久化配置兜底。
     */
    private static volatile boolean configuredKnown;
    private static long startNanos = System.nanoTime();
    private static long sentBytes;

    private SpeedLimiter() {
    }

    /** 用户改了设置：同时更新「配置值」和「生效值」（握手前先按本机配置跑）。 */
    public static void setLimitBytesPerSecond(long bytesPerSecond) {
        synchronized (SpeedLimiter.class) {
            long bps = Math.max(0, bytesPerSecond);
            configuredBytesPerSecond = bps;
            configuredKnown = true;
            limitBytesPerSecond = bps;
            startNanos = System.nanoTime();
            sentBytes = 0;
        }
    }

    /** 307：握手协商之后只改**生效值**，不动配置值。
     * <p>
     * 配置值是「我这端想限到多少」，下一轮握手还要拿它去报数；
     * 要是这里顺手把它覆盖成协商结果，两端会越协商越紧。
     */
    public static void applyNegotiatedBytesPerSecond(long bytesPerSecond) {
        synchronized (SpeedLimiter.class) {
            limitBytesPerSecond = Math.max(0, bytesPerSecond);
            startNanos = System.nanoTime();
            sentBytes = 0;
        }
    }

    /** 本机配置的上限（字节/秒），握手时报给电脑。 */
    public static long getConfiguredBytesPerSecond() {
        return configuredBytesPerSecond;
    }

    /** 配置值是否已经就绪（false 表示这个进程还没从配置文件里读出来）。 */
    public static boolean isConfiguredKnown() {
        return configuredKnown;
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
