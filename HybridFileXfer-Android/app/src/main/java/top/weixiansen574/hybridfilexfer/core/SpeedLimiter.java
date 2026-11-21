package top.weixiansen574.hybridfilexfer.core;

import java.util.concurrent.atomic.AtomicInteger;

/**
 * 全局限速：按字节令牌桶限制发送速度，0 表示不限速。
 *
 * <p>308 起限速值是<b>两端共用</b>的一个数（307 是两端各报各的取 min）：
 * 谁改了都推给对方，两端永远显示、也永远按同一个数跑。
 */
public final class SpeedLimiter {
    /** 1 MB/s = 多少个字节/秒（所有换算都用它，别各写各的） */
    private static final long MB = 1024L * 1024L;
    /** 当前**生效**的上限（0 = 不限速）。 */
    private static volatile long limitBytesPerSecond;
    /**
     * 本机这一端记着的**共用值**（0 = 不限速）。
     *
     * <p>和 limitBytesPerSecond 分开存着：握手时要把它报给电脑，
     * 而两边可能在握手之后又把它改掉。308 起它和生效值总是同时被设成同一个数
     * （两端共用一个值，不再有"配置 20、生效 5"这种情况），分开只是为了
     * 语义清楚 —— 一个是"要告诉对方的数"，一个是"我现在按多少跑"。
     */
    private static volatile long configuredBytesPerSecond;
    /**
     * 共用值是否**真的初始化过**。
     * <p>
     * 光看 configuredBytesPerSecond == 0 分不清两件事：「用户就是不限速」和
     * 「这个进程还没从配置文件里读出来」。后者发生在服务进程刚拉起的瞬间，
     * 如果那时握手，手机就会骗电脑说「我不限速」，把用户设的上限**静默抹掉**。
     * 所以单独记一个标志，未初始化时由握手方改用持久化配置兜底。
     */
    private static volatile boolean configuredKnown;
    /**
     * 共用值的**代次**：只有「手机端用户自己改了限速」才 +1。
     *
     * <p>会话线程不能由界面直接写 socket（控制通道有且只有一个写者），所以界面
     * 只改值 + 提代次，会话线程每 250ms 的轮询里比对一次代次，发现变了就补发一帧
     * {@link ControllerIdentifiers#SPEED_LIMIT}。和电脑端 ratelimit::configured_epoch
     * 是同一套设计。
     *
     * <p><b>采纳电脑推来的值时不提升代次</b>：那个值本来就是对方定的，再推回去
     * 就会让两端把同一个数来回推一辈子（每 250ms 一帧，控制通道上全是废话）。
     * 所以「本机用户改」走 {@link #applyLocalBytesPerSecond(long)}，
     * 「采纳对端」走 {@link #applySharedBytesPerSecond(long)}，两条路径必须分开。
     */
    private static final AtomicInteger sharedEpoch = new AtomicInteger();
    private static long startNanos = System.nanoTime();
    private static long sentBytes;

    private SpeedLimiter() {
    }

    /**
     * 采纳两端共用的限速值（握手定下来的 / 电脑推来的）：**配置值和生效值都**设成它，
     * 并重置计时窗。语义上等同「这个数就是两端共用的，本机也认它」。
     *
     * <p><b>不动代次</b>，理由见 {@link #sharedEpoch}：这是对端给的数，不是本机改的，
     * 一提代次就会把同一个数又推回去，两端无限对推。
     */
    public static void applySharedBytesPerSecond(long bytesPerSecond) {
        synchronized (SpeedLimiter.class) {
            setLocked(bytesPerSecond);
        }
    }

    /**
     * 手机端用户自己改了限速：设值**并且**提升代次，让会话线程把新值推给电脑。
     *
     * <p>只在服务进程里调用（见 {@code ServerService.onLocalSpeedLimitChanged}）——
     * SpeedLimiter 的静态字段按进程隔离，界面进程里改一份是不会传到服务进程的。
     *
     * <p>值真的变了才提代次（和电脑端 ratelimit::set_configured_mb 一致）：
     * 用户把滑块拖回原值、或者采纳电脑的值之后又确认了同一个数，都不该再发一帧。
     */
    public static void applyLocalBytesPerSecond(long bytesPerSecond) {
        long bps = Math.max(0, bytesPerSecond);
        boolean changed;
        synchronized (SpeedLimiter.class) {
            changed = !configuredKnown || configuredBytesPerSecond != bps;
            setLocked(bps);
        }
        if (changed) {
            sharedEpoch.incrementAndGet();
        }
    }

    /** 共用限速值本机用户改过几次（会话线程拿它判断"该不该把新值推给电脑"）。 */
    public static int sharedEpoch() {
        return sharedEpoch.get();
    }

    /** 配置值 + 生效值一起设成 bps，并重置计时窗。调用方自己持有 SpeedLimiter.class 的锁。 */
    private static void setLocked(long bytesPerSecond) {
        long bps = Math.max(0, bytesPerSecond);
        configuredBytesPerSecond = bps;
        configuredKnown = true;
        limitBytesPerSecond = bps;
        startNanos = System.nanoTime();
        sentBytes = 0;
    }

    /** 本机记着的共用上限（字节/秒），握手时报给电脑。 */
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

    /** MB/s → 字节/秒（0 仍然是"不限速"，不是"限到 0"，所以这里不出现除法）。 */
    public static long mbpsToBytesPerSecond(int mbps) {
        return Math.max(0, mbps) * MB;
    }

    /** 字节/秒 → MB/s（向下取整：判断"是不是不限速"只看 == 0，取整不会把它变成别的意思）。 */
    public static int bytesPerSecondToMbps(long bytesPerSecond) {
        return (int) (Math.max(0, bytesPerSecond) / MB);
    }

    /**
     * 每次传输开始时重置计时窗。不清的话，设值之后隔了很久才发起传输，
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
