package top.weixiansen574.hybridfilexfer.core;

import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.LinkedBlockingDeque;
import java.util.concurrent.TimeUnit;

import top.weixiansen574.hybridfilexfer.core.bean.Directory;
import top.weixiansen574.hybridfilexfer.core.bean.RemoteFile;

public abstract class ReadFileCall implements Callable<Void> {
    public static final FileBlock END_POINT = new FileBlock(true, -1, "END_POINT", 0, 0, -1, null);
    public static final FileBlock INTERRUPT = new FileBlock(true, -1, "INTERRUPT", 0, 0, -1, null);
    public static final FileBlock READ_ERROR = new FileBlock(true, -1, "READ_ERROR", 0, 0, -1, null);
    public static final FileBlock WRITE_ERROR = new FileBlock(true, -1, "WRITE_ERROR", 0, 0, -1, null);

    private final LinkedBlockingDeque<ByteBuffer> buffers;
    private final List<RemoteFile> files;
    private final Directory localDir;
    private final Directory remoteDir;
    private final int operateThreadCount;
    private int fileIndex = -1;
    /** 303：本次发出去的文件（手机本地绝对路径），收尾复算 SHA-256 用 */
    private final List<String> sourcePaths = new java.util.ArrayList<>();
    /**
     * 303：收尾要复算整文件 SHA-256。那一步等于把文件再读一遍（1.78GB 实测约 2 秒，
     * 用户反馈「校验时间有点长」），而这段时间电脑端同时也在算，谁慢谁拖后腿。
     * 现在文件一登记就丢给后台线程先算，收尾只是 get() 一下 —— 这几秒藏进传输过程里了。
     */
    private final java.util.Map<String, java.util.concurrent.Future<HFXService.FileDigest>> digestTasks =
            new java.util.concurrent.ConcurrentHashMap<>();
    /** 后台算指纹的线程池（懒建；2 条就够，再多人也是抢同一块闪存带宽） */
    private java.util.concurrent.ExecutorService digestExecutor;

    /** 收尾按路径取「提前算好的指纹」，取不到的再同步算（见 HFXService.digestOf 重载） */
    public java.util.Map<String, java.util.concurrent.Future<HFXService.FileDigest>> digestTasks() {
        return digestTasks;
    }

    /** 收尾取完指纹后关掉后台线程池（shutdown 不打断正在算的那个文件） */
    public void shutdownDigest() {
        java.util.concurrent.ExecutorService executor;
        synchronized (this) {
            executor = digestExecutor;
            digestExecutor = null;
        }
        if (executor != null) {
            executor.shutdown();
        }
    }

    private void prefetchDigest(String path) {
        if (digestTasks.containsKey(path)) {
            return;
        }
        synchronized (this) {
            if (digestExecutor == null) {
                digestExecutor = java.util.concurrent.Executors.newFixedThreadPool(2, r -> {
                    Thread t = new Thread(r, "hfx-digest");
                    t.setDaemon(true);
                    return t;
                });
            }
        }
        try {
            digestTasks.put(path, digestExecutor.submit(() -> HFXService.digestOne(path)));
        } catch (Throwable e) {
            // 线程池已经关了（传输收尾）之类：收尾时同步算，行为退回老样子
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("prefetchDigest failed: " + path, e);
        }
    }
    /** 暂停闸门：暂停时不再往队列里读块 */
    private volatile TransferPause pause;

    /**
     * 每条通道自己的队列：块按"谁负责这一段"投递，发块线程只从自己的队列取。
     *
     * <p>以前是一条共享队列，谁抢到下一块谁发 —— 块在通道之间轮流，段边界每块都变，
     * 进度条只能画等分的假分段（传输过程中不会出现新的切割线）。现在按
     * {@link SendRanges} 的连续段投递，某条通道干完一段后还会去切别人一段的尾巴，
     * 段数会随传输变多，界面上就有 IDM 那种动态切割线。
     */
    private final LinkedBlockingDeque<FileBlock>[] queues;
    /**
     * 每条通道「真的写出去了多少字节」（SendFileCall 每发完一块汇报一次）。
     *
     * <p>为什么要按速度区别对待：通道能力差着两个数量级（USB/无线 40~100MB/s，
     * 蓝牙共享网络 0.2MB/s）。以前所有通道一视同仁地预读 8 块，于是传输最后阶段
     * 快通道都干完了、蓝牙队列里还压着 8 块（8MB ÷ 0.2MB/s ≈ 40 秒），
     * 用户看到的就是「蓝牙的块卡住最后的传输」。
     */
    private final long[] channelBytes;
    /** 上一轮采样时各通道的字节数（算增量用） */
    private final long[] speedBaseBytes;
    /** 最近一次测出的通道速度，字节/毫秒；0 表示还没测出来 */
    private final double[] channelSpeed;
    /** 上一次采样速度的时刻 */
    private long speedSampledAt;
    /** 慢通道判定只在第一次记日志，别每秒刷屏 */
    private final boolean[] hopelessLogged;
    /** 连续低于阈值的采样次数：连中两次（约 1 秒）才认定慢，起步抖动不算 */
    private final int[] slowStreak;
    /**
     * 各通道「最后一次真正把字节发出去」的时刻。
     *
     * <p>光看速度判不出「卡死」：卡住的通道速度还停在 0，被判成"还没测出来"，
     * 于是照样预读 8 块、照样占着一段尾巴 —— 实测 wlan0 / wlan0#2 就是这样，
     * 自己各押着 9MB 一动不动，最后两条把收尾拖了 2 分 17 秒。
     * 所以还要有一个「多久没进展」的口径。
     */
    private final long[] lastProgressAt;
    /** 「救回块」这件事别每块都刷日志：每条通道最多 3 秒记一条 */
    private final long[] rescueLoggedAt;
    /** 每条通道被救回多少块（日志汇总用） */
    private final long[] rescueTotal;
    /** 本次传输开始时刻：起步阶段别急着判卡死 */
    private long transferStartedAt;
    /** 多久没发出一个字节就算卡死（蓝牙 0.1MB/s 时 1 块要 10 秒，但它速度是测出来的，走速度判据） */
    private static final long STALL_MS = 3000L;
    /**
     * 判定「这条链路没救」的绝对速度门槛（字节/毫秒 ≈ 1.5MB/s）。
     *
     * <p>实测蓝牙共享网络 0.2~0.6MB/s，而无线/adb 至少 5MB/s 起，1.5MB/s 这条线
     * 能把两者彻底分开。只用「相对最快通道的比例」判会把起步慢半拍的无线通道也误判掉，
     * 那样反而拖慢整体吞吐（实测 600MB 从 25 秒劣化到 110 秒）。
     */
    private static final double HOPELESS_SPEED = 1.5 * 1048576 / 1000;
    /** 采样间隔：太短会被单个块的抖动带偏，500ms 足够分辨蓝牙和无线 */
    private static final long SPEED_SAMPLE_MS = 500;
    /** 文件夹这类"谁发都行"的块 */
    private final LinkedBlockingDeque<FileBlock> specialQueue = new LinkedBlockingDeque<>();
    /** 当前文件的分段计划（界面画进度条读它） */
    private volatile SendRanges ranges;
    private final String[] channelNames;
    /** 305：目标路径 → 接收端已经有的块（断点续传时要跳过的块） */
    private final java.util.Map<String, java.util.BitSet> resumeBits;

    /**
     * 本次传输的分块进度（HFXService.sendProgress）。
     *
     * <p>两块来源：一条通道真正把块写出去以后（{@link #noteBlockSent}），
     * 以及 305 续传时"接收端已经有了"而跳过的块 —— 后者直接算已完成，
     * 否则进度条会永远差一截（跳到的那几百 MB 永远不会出现在进度里）。
     */
    private volatile BlockProgress progress;

    /** 这次传输已经被取消 / 通道出错：读文件的线程必须自己停下来 */
    private volatile boolean shutdown;
    /** 读文件线程已经把该产出的块都产完了 */
    private volatile boolean readerFinished;

    @SuppressWarnings("unchecked")
    public ReadFileCall(LinkedBlockingDeque<ByteBuffer> buffers, List<RemoteFile> files,
                        Directory localDir, Directory remoteDir, int operateThreadCount) {
        this(buffers, files, localDir, remoteDir, operateThreadCount, null);
    }

    @SuppressWarnings("unchecked")
    public ReadFileCall(LinkedBlockingDeque<ByteBuffer> buffers, List<RemoteFile> files,
                        Directory localDir, Directory remoteDir, int operateThreadCount,
                        String[] channelNames) {
        this(buffers, files, localDir, remoteDir, operateThreadCount, channelNames, null);
    }

    /** 305：带续传位图（目标路径 → 已经收到的块），跳过的块不再读、不再发 */
    @SuppressWarnings("unchecked")
    public ReadFileCall(LinkedBlockingDeque<ByteBuffer> buffers, List<RemoteFile> files,
                        Directory localDir, Directory remoteDir, int operateThreadCount,
                        String[] channelNames, java.util.Map<String, java.util.BitSet> resumeBits) {
        this.buffers = buffers;
        this.files = files;
        this.localDir = localDir;
        this.remoteDir = remoteDir;
        this.operateThreadCount = Math.max(1, operateThreadCount);
        this.channelNames = channelNames;
        this.resumeBits = resumeBits;
        this.queues = new LinkedBlockingDeque[this.operateThreadCount];
        for (int i = 0; i < this.operateThreadCount; i++) {
            this.queues[i] = new LinkedBlockingDeque<>();
        }
        this.channelBytes = new long[this.operateThreadCount];
        this.speedBaseBytes = new long[this.operateThreadCount];
        this.channelSpeed = new double[this.operateThreadCount];
        this.hopelessLogged = new boolean[this.operateThreadCount];
        this.slowStreak = new int[this.operateThreadCount];
        this.lastProgressAt = new long[this.operateThreadCount];
        this.rescueLoggedAt = new long[this.operateThreadCount];
        this.rescueTotal = new long[this.operateThreadCount];
        // 起步阶段从"现在"算：3 秒内没进展不算卡死，得先给通道一个跑起来的机会
        this.transferStartedAt = System.currentTimeMillis();
        for (int i = 0; i < this.operateThreadCount; i++) {
            this.lastProgressAt[i] = this.transferStartedAt;
        }
    }

    public void setPause(TransferPause pause) {
        this.pause = pause;
    }

    /** 分块进度账本（发送方向），由 HFXService.sendFiles 注入 */
    public void setProgress(BlockProgress progress) {
        this.progress = progress;
    }

    /**
     * 这一块已经真正写到通道上了（SendFileCall 写完数据后回调）。
     * 进度条只认这个真值：投递进队列的那一刻还不算，队列里可能压着几百 MB。
     */
    public void noteBlockSent(String owner, String path, int index, long totalSize) {
        BlockProgress tracker = progress;
        if (tracker != null) {
            tracker.onBlock(owner, path, index, totalSize);
        }
    }

    /** SendFileCall 每发完一块汇报一次：这条通道累计真正写出去多少字节 */
    public void noteChannelBytes(int channel, long bytes) {
        if (channel < 0 || channel >= operateThreadCount) {
            return;
        }
        if (bytes > channelBytes[channel]) {
            channelBytes[channel] = bytes;
            lastProgressAt[channel] = System.currentTimeMillis();
        }
    }

    /**
     * 采一次各通道速度（生产者循环每轮调，实际最多 500ms 采一次）。
     * 单位是「字节/毫秒」，只用来比较谁快谁慢，不需要精确。
     */
    private void sampleSpeeds() {
        long now = System.currentTimeMillis();
        if (speedSampledAt == 0) {
            speedSampledAt = now;
            System.arraycopy(channelBytes, 0, speedBaseBytes, 0, operateThreadCount);
            return;
        }
        long elapsed = now - speedSampledAt;
        if (elapsed < SPEED_SAMPLE_MS) {
            return;
        }
        for (int i = 0; i < operateThreadCount; i++) {
            long delta = channelBytes[i] - speedBaseBytes[i];
            // 只信「有进展」的采样：通道卡住（或被暂停）时增量为 0，那时给它记 0
            // 会让它一恢复就被判成慢通道，所以增量 0 时保留上一次的测量值
            if (delta > 0) {
                channelSpeed[i] = (double) delta / elapsed;
            }
            speedBaseBytes[i] = channelBytes[i];
        }
        speedSampledAt = now;
    }

    /** 临时排障：每秒一行「已投递块数 + 各通道速度/窗口」，看收尾时到底卡在谁身上 */
    private void debugDump(SendRanges plan) {
        long now = System.currentTimeMillis();
        if (now - debugLoggedAt < 2000) {
            return;
        }
        debugLoggedAt = now;
        StringBuilder sb = new StringBuilder("进度 投递=" + plan.producedBlocks() + "/" + plan.totalBlocks() + " ");
        for (int i = 0; i < operateThreadCount; i++) {
            sb.append(channelNames != null && i < channelNames.length ? channelNames[i] : String.valueOf(i))
                    .append('=')
                    .append(String.format(java.util.Locale.US, "%.2f", channelSpeed[i] * 1000 / 1048576d))
                    .append('(').append(windowFor(i)).append(isHopelessChannel(i) ? "H" : "").append(')')
                    .append(' ');
        }
        HFXLog.d(sb.toString());
    }

    private long debugLoggedAt;

    /** 全场最快的通道速度（字节/毫秒），还没测出来就是 0 */
    private double fastestSpeed() {
        double fast = 0;
        for (int i = 0; i < operateThreadCount; i++) {
            if (channelSpeed[i] > fast) {
                fast = channelSpeed[i];
            }
        }
        return fast;
    }

    /**
     * 这条通道是不是慢到不值得再给它新活。
     *
     * <p>判据：已经测出速度，且不到最快通道的八分之一（蓝牙共享网络 0.2MB/s
     * 对无线 40MB/s 是两百分之一）。判成之后它不再抢新段、预读窗口缩到 1 块，
     * 收尾时最多等它一块——不再出现「快通道全干完、等蓝牙吐 8 块」的场面。
     */
    private boolean isHopelessChannel(int channel) {
        if (channel < 0 || channel >= operateThreadCount) {
            return false;
        }
        if (isStalledChannel(channel)) {
            return true;
        }
        double speed = channelSpeed[channel];
        double fast = fastestSpeed();
        if (speed <= 0 || fast <= 0) {
            return false;
        }
        // 两个条件同时满足才算没救：速度绝对值低到 1.5MB/s 以下，且明显不到最快通道的 1/8。
        // 后者是保险：整条链路都很慢时（比如信号差），谁也别被踢出局。
        if (speed < HOPELESS_SPEED) {
            slowStreak[channel]++;
        } else {
            slowStreak[channel] = 0;
        }
        boolean hopeless = slowStreak[channel] >= 2 && speed * 8 < fast;
        if (hopeless && !hopelessLogged[channel]) {
            hopelessLogged[channel] = true;
            // 记一笔：以后看日志能直接知道这条通道被降级了，不用猜
            HFXLog.d("慢通道 " + (channelNames != null && channel < channelNames.length
                    ? channelNames[channel] : String.valueOf(channel))
                    + String.format(" 速度 %.2f MB/s，最快通道 %.2f MB/s，不再分配新段",
                    speed * 1000 / 1048576d, fast * 1000 / 1048576d));
        }
        return hopeless;
    }

    /**
     * 这条通道是不是**卡死**了（3 秒一个字节都没发出去）。
     *
     * <p>和「慢」要分开：慢通道（蓝牙 0.1~0.5MB/s）也是用户特意开出来的通道，
     * 该给它分一份活干；卡死的通道才是真的不能喂（喂了也发不出去，只能靠救回）。
     * 两者以前混在一个判据里，结果蓝牙被当成卡死，一个字节都分不到。
     */
    private boolean isStalledChannel(int channel) {
        if (channel < 0 || channel >= operateThreadCount) {
            return false;
        }
        long now = System.currentTimeMillis();
        long limit = stallLimitMs(channel);
        if (now - transferStartedAt <= limit || now - lastProgressAt[channel] < limit) {
            return false;
        }
        if (!hopelessLogged[channel]) {
            hopelessLogged[channel] = true;
            HFXLog.d("卡住的通道 " + (channelNames != null && channel < channelNames.length
                    ? channelNames[channel] : String.valueOf(channel))
                    + " 已 " + (now - lastProgressAt[channel]) / 1000 + " 秒没发出字节，先不给它派活");
        }
        return true;
    }

    /**
     * 「多久没进展算卡死」的时限：按这条通道自己的速度算。
     *
     * <p>固定 3 秒对蓝牙是错的：0.1MB/s 的通道发一块（1MiB）本来就要 10 秒，
     * 按 3 秒判就把它当卡死、一个字节都不派给它（实测日志里就是
     * 「卡住的通道 bt-pan 已 3 秒没发出字节」）。现在按「发完 3 块该用的时间」算，
     * 最多 60 秒 —— 真死了（拔网线/BNEP 断）也还是会在 3 秒内发现，因为那时速度已经测出来了。
     */
    private long stallLimitMs(int channel) {
        double speed = channelSpeed[channel];
        if (speed <= 0) {
            // 速度还没测出来（它一块都还没发完）：这时候不能按 3 秒判死。
            // 蓝牙 0.1MB/s 发完第一块就要 10 秒，按 3 秒判就会把活通道当死通道，
            // 结果一个字节都分不到（实测日志「卡住的通道 bt-pan 已 3 秒没发出字节」）。
            return channelBytes[channel] <= 0 ? 15000L : STALL_MS;
        }
        long perBlock = (long) (FileBlock.BLOCK_SIZE / speed);
        return Math.min(60000L, Math.max(STALL_MS, perBlock * 3));
    }

    /** 这条通道还有没有可能把块发出去（没卡死） */
    private boolean couldSend(int channel) {
        return !isStalledChannel(channel);
    }

    /** 队里至少有一条还能发的通道 */
    private boolean hasHealthyChannel() {
        for (int i = 0; i < operateThreadCount; i++) {
            if (couldSend(i)) {
                return true;
            }
        }
        return false;
    }

    /**
     * 把「卡死通道」队列里押着的块挪到「谁发都行」的队列上。
     *
     * <p>为什么非挪不可：抢段只能抢「还没投递出去」的块，一旦投递进某条通道的队列，
     * 别的通道就再也拿不走了。卡死的通道（实测 wlan0 / wlan0#2 各押着 9MB）会把这些块
     * 一块一块慢慢吐出来，其它通道干等 —— 收尾就整个卡在它们身上。
     * 块数据本来就在内存里，换个队列，闲下来的快通道立刻就能发出去。
     */
    private void rescueStalledBlocks() {
        long now = System.currentTimeMillis();
        if (now - transferStartedAt <= STALL_MS) {
            return;
        }
        for (int ch = 0; ch < operateThreadCount; ch++) {
            if (queues[ch].isEmpty() || now - lastProgressAt[ch] < stallLimitMs(ch)) {
                continue;
            }
            // 直接塞给最闲的健康通道的队列：它们的发送线程阻塞在自己队列的 poll 上，
            // 一塞进去就被取走，比走「谁发都行」的公共队列更即时（也不会被慢通道捡回去）
            int target = -1;
            for (int i = 0; i < operateThreadCount; i++) {
                if (i == ch || isHopelessChannel(i)) {
                    continue;
                }
                if (target < 0 || queues[i].size() < queues[target].size()) {
                    target = i;
                }
            }
            FileBlock moved = queues[ch].poll();
            int count = 0;
            while (moved != null) {
                if (target >= 0) {
                    queues[target].add(moved);
                } else {
                    specialQueue.add(moved);
                }
                count++;
                moved = queues[ch].poll();
            }
            if (count > 0) {
                rescueTotal[ch] += count;
                if (now - rescueLoggedAt[ch] >= 3000) {
                    rescueLoggedAt[ch] = now;
                    HFXLog.d("救回 " + (channelNames != null && ch < channelNames.length
                            ? channelNames[ch] : String.valueOf(ch)) + " 队列里压着的块（累计 "
                            + rescueTotal[ch] + " 块），交给别的通道");
                }
            }
        }
    }

    /**
     * 这条通道现在最多预读几块。
     *
     * <p>口径是「队列里最多压这条通道 1 秒能发完的量」：
     * <ul>
     *   <li>20MB/s 的通道：8 块（4MB）也只够它发 200ms，给满上限，不影响流水线；</li>
     *   <li>蓝牙 0.13MB/s：1 块就要 8 秒才发得出去 —— 只给 1 块，收尾时就不会出现
     *       「快通道全干完了，蓝牙手里还压着 8 块，只能等它一块一块慢慢吐」；</li>
     *   <li>这些块一旦投递进某条通道的队列，别的通道就抢不走了（抢段只能抢「还没投递」的块），
     *       所以给慢通道押块等于把收尾时间压死。</li>
     * </ul>
     * 速度还没测出来的通道给满：先让所有人跑起来，500ms 后第一份测量就来了。
     */
    private int windowFor(int channel) {
        final int max = 8;
        if (isHopelessChannel(channel)) {
            return 1;
        }
        double speed = channelSpeed[channel];
        if (speed <= 0) {
            return max;   // 还没测出来：先给满，别拖慢起步
        }
        // 速度是字节/毫秒 → 1 秒能发多少字节，再折算成块数（向上取整，至少 1 块）
        int window = (int) Math.ceil(speed * 1000 / FileBlock.BLOCK_SIZE);
        return Math.max(1, Math.min(max, window));
    }

    private TransferPause pause() {
        return pause;
    }

    @Override
    public Void call() throws Exception {
        try {
            for (RemoteFile file : files) {
                if (shutdown) {
                    return null;
                }
                if (!fileExists(file.getPath())) {
                    continue;
                }
                readToDeque(file);
                if (file.isDirectory()) {
                    listFilesAndRead(file);
                }
            }
            if (shutdown) {
                return null;
            }
            finishProducing();
        } catch (Exception e) {
            //当发生读取错误时
            relay(READ_ERROR);
            throw e;
        }
        return null;
    }

    /** 所有块都产完了：通知各通道收尾（它们会发 EOF 并退出） */
    private void finishProducing() {
        SendRanges plan = ranges;
        if (plan != null) {
            plan.finish();
        }
        readerFinished = true;
        for (int i = 0; i < operateThreadCount; i++) {
            queues[i].add(END_POINT);
        }
    }

    private void listFilesAndRead(RemoteFile folder) throws Exception {
        List<RemoteFile> childList = listFiles(folder.getPath());
        if (childList != null) {
            for (RemoteFile file : childList) {
                readToDeque(file);
                if (file.isDirectory()) {
                    listFilesAndRead(file); // 递归遍历子文件夹
                }
            }
        }
    }

    private void readToDeque(RemoteFile file) throws Exception {
        fileIndex++;
        if (file.isDirectory()) {
            specialQueue.add(new FileBlock(false,
                    fileIndex, localDir.generateTransferPath(file.getPath(), remoteDir),
                    file.lastModified(), 0, 0, null));
            return;
        }
        // 303：记下这次真正发出去的文件（收尾复算 SHA-256 用），并让后台线程现在就开始算指纹
        sourcePaths.add(file.getPath());
        prefetchDigest(file.getPath());
        FileChannel channel = openFile(file.getPath());
        long length = channel.size();
        long lastModified = file.lastModified();
        String transferPath = localDir.generateTransferPath(file.getPath(), remoteDir);
        if (length == 0) {
            ByteBuffer buffer = buffers.take();
            buffer.clear();
            buffer.limit(0);
            specialQueue.add(new FileBlock(true,
                    fileIndex, transferPath, lastModified, length, 0, buffer));
            closeFile();
            return;
        }
        java.util.BitSet have = resumeBits == null ? null : resumeBits.get(transferPath);
        int blockCount = (int) ((length + FileBlock.BLOCK_SIZE - 1) / FileBlock.BLOCK_SIZE);
        // 305：接收端已经有了的块先在进度上记完成，剩下的块做成 ordinal -> 文件内块序号 的映射
        // （原来是在顺序读的循环里边读边跳过，现在生产顺序是 wave，所以先一次性标好）
        int[] blockOf = (have == null || have.isEmpty()) ? null : new int[blockCount];
        int missing = 0;
        BlockProgress skipTracker = progress;
        for (int b = 0; b < blockCount; b++) {
            if (have != null && have.get(b)) {
                if (skipTracker != null) {
                    // 305：续传跳过的块在进度条上直接算完成（owner 用空串 = 早就传输过了），
                    // 否则续传时进度条永远到不了 100%（跳过的那几百 MB 不会再出现在进度里）
                    skipTracker.onBlock("", transferPath, b, length);
                }
            } else {
                if (blockOf != null) {
                    blockOf[missing] = b;
                }
                missing++;
            }
        }
        if (missing <= 0) {
            // 接收端一块都不缺：这个文件不用再发（它已经在电脑上了）。
            // sourcePaths 上面已经记过了，收尾照样复算 SHA-256 参与两端比对。
            closeFile();
            return;
        }
        SendRanges plan = new SendRanges(operateThreadCount, missing, channelNames);
        // 让分段计划知道谁慢：慢通道不再抢新段，它手里的尾巴也一次性被快通道接手
        plan.setHopelessOracle(this::isHopelessChannel);
        ranges = plan;
        // 按需生产：哪条通道手上的块少于 prefetch 块，就把**它当前那一段**里的下一块读出来给它。
        //
        // 历史教训，两条路都走错过：
        // 1) 按通道从头到尾顺序读（块 0..n/6 全给通道 0，再给通道 1 …）：第 6 条通道要等读到
        //    文件的 5/6 处才拿到第一块 —— 实测发送时 adb 那两条立刻跑满，4 条 wlan 干等十几秒。
        // 2) 改成按轮次（wave）各产一块：起跑是一起了，但**抢段失效** —— 谁先干完自己那一段都
        //    没意义，因为剩下的块按固定轮次在产，抢到手也得等读取线程轮到。
        // 现在按通道按需：谁缺谁拿。抢到新段的通道下一轮就能把它那段的块读出来，
        // 于是"先干完的通道去切别人没传完的块"这个 IDM 行为才是真的生效。
        while (!plan.allProduced()) {
            if (shutdown) {
                closeFile();
                return;
            }
            // 每轮顺手采一次速度（内部 500ms 才真的算一次），决定各通道预读窗口的大小
            sampleSpeeds();
            debugDump(plan);
            rescueStalledBlocks();
            boolean produced = false;
            // 全队都停了就别再挑三拣四，谁都得喂（否则谁也产不出块，直接死等）
            boolean anyHealthy = hasHealthyChannel();
            for (int ch = 0; ch < operateThreadCount; ch++) {
                // 只跳**卡死**的通道：产出来它一个字节也发不出去，只能靠 rescueStalledBlocks 再搬回来。
                // 慢通道（蓝牙）照常喂 —— 用户特意开的通道就得让它干活，它的段由 trySteal 在收尾阶段接手。
                if (anyHealthy && isStalledChannel(ch)) {
                    continue;
                }
                // 一次把这条通道补到它自己的窗口：快通道给满 8 块（刚抢到新段的下一轮就能连读几块），
                // 慢通道只给 1 块 —— 队列里压着的块在收尾时是要一块块等它吐出来的，
                // 蓝牙 0.2MB/s 压 8 块就是 40 秒
                int window = windowFor(ch);
                while (queues[ch].size() < window) {
                    long ordinal = plan.takeNext(ch);
                    if (ordinal < 0) {
                        break;
                    }
                    TransferPause gate = pause();
                    if (gate != null) {
                        gate.awaitResume();
                    }
                    int blockIndex = blockOf == null ? (int) ordinal : blockOf[(int) ordinal];
                    long pos = (long) blockIndex * FileBlock.BLOCK_SIZE;
                    int blkSize = (int) Math.min(length - pos, FileBlock.BLOCK_SIZE);
                    ByteBuffer buffer = buffers.take();
                    if (shutdown) {
                        // 等到缓冲区的这一瞬间整个传输已经取消了：立刻还回去，别再往里读
                        buffers.add(buffer);
                        closeFile();
                        return;
                    }
                    buffer.clear();
                    buffer.limit(blkSize);
                    while (buffer.hasRemaining()) {
                        // 定位读：各通道的读位置互相独立，按需生产才不会互相干扰
                        int read = channel.read(buffer, pos + buffer.position());
                        if (read < 0) {
                            throw new java.io.EOFException("读文件提前结束：" + file.getPath());
                        }
                    }
                    queues[ch].add(new FileBlock(true, fileIndex, transferPath,
                            lastModified, length, blockIndex, buffer));
                    produced = true;
                }
            }
            if (!produced) {
                // 每条通道都有块在手（或者都在等抢段），别空转烧 CPU
                Thread.sleep(1);
            }
        }
        HFXLog.d("生产者收工：投递=" + plan.producedBlocks() + "/" + plan.totalBlocks());
        plan.finish();
        closeFile();
    }

    /** 303：本次发送涉及的文件（手机本地绝对路径） */
    public List<String> sourcePaths() {
        return sourcePaths;
    }

    public void recycleBuffer(ByteBuffer buffer) {
        buffers.add(buffer);
    }

    // ------------------------------------------------------------------
    // 发块线程（SendFileCall）用的接口
    // ------------------------------------------------------------------

    /** 取一条通道自己队列里的下一块；timeoutMs 内没有就返回 null */
    public FileBlock pollOwnBlock(int channel, long timeoutMs) throws InterruptedException {
        if (channel < 0 || channel >= operateThreadCount) {
            return null;
        }
        return queues[channel].poll(timeoutMs, TimeUnit.MILLISECONDS);
    }

    /** 取一个「谁发都行」的块（文件夹 / 空文件 / 没有健康通道时救回来的块）；没有就返回 null */
    public FileBlock pollSpecialBlock() {
        return specialQueue.poll();
    }

    /** 这条通道自己那段还没产完，或者抢到了别人的一段尾巴 → 还有活干 */
    public boolean trySteal(int channel) {
        SendRanges plan = ranges;
        return plan != null && plan.trySteal(channel);
    }

    public boolean isReaderFinished() {
        return readerFinished;
    }

    /** 当前文件的分段（进度条画切割线用）；没有就返回空 */
    public List<SendRanges.Segment> segments() {
        SendRanges plan = ranges;
        return plan == null ? new ArrayList<>() : plan.snapshot();
    }

    /** 等待某条通道自己的队列上出现新块（配合 trySteal 用） */
    public void awaitOwnBlock(int channel, long millis) throws InterruptedException {
        if (channel < 0 || channel >= operateThreadCount) {
            return;
        }
        FileBlock block = queues[channel].poll(millis, TimeUnit.MILLISECONDS);
        if (block != null) {
            queues[channel].addFirst(block);
        }
    }

    //当对方写入时发生错误时
    public void shutdownByWriteError() {
        shutdown = true;
        recycleAllBuffer();
        relay(WRITE_ERROR);
    }

    //当其中任意一条通道断开时
    public void shutdownByConnectionBreak() {
        shutdown = true;
        recycleAllBuffer();
        for (int i = 0; i < operateThreadCount - 1; i++) {
            relay(INTERRUPT);
        }
    }

    private void relay(FileBlock marker) {
        for (int i = 0; i < operateThreadCount; i++) {
            queues[i].addFirst(marker);
        }
    }

    /**
     * 把各队列里还没人领的块全部取出来、把缓冲区还回池子。
     *
     * <p>必须"取出来"而不是"遍历"：只遍历不摘除的话，这些块还留在队列里，
     * 另一条发块线程照样能 take 到它们 —— 可它们的缓冲区已经回到池子里、
     * 可能正被别的线程写入（cancel 后尤其明显），写出去的就是垃圾，
     * 甚至直接 {@code IllegalArgumentException: newPosition > limit}。
     */
    private void recycleAllBuffer() {
        for (LinkedBlockingDeque<FileBlock> queue : queues) {
            FileBlock fileBlock;
            while ((fileBlock = queue.pollFirst()) != null) {
                if (fileBlock.data != null) {
                    recycleBuffer(fileBlock.data);
                }
            }
        }
        FileBlock fileBlock;
        while ((fileBlock = specialQueue.pollFirst()) != null) {
            if (fileBlock.data != null) {
                recycleBuffer(fileBlock.data);
            }
        }
    }

    protected abstract boolean fileExists(String path) throws Exception;

    protected abstract List<RemoteFile> listFiles(String path) throws Exception;

    protected abstract FileChannel openFile(String path) throws Exception;

    protected abstract void closeFile() throws Exception;

}
