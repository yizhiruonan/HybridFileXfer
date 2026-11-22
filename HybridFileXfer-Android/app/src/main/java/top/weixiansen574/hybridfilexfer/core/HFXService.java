package top.weixiansen574.hybridfilexfer.core;

import java.io.File;
import java.io.FileInputStream;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.zip.CRC32;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.FutureTask;
import java.util.concurrent.LinkedBlockingDeque;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;

import top.weixiansen574.hybridfilexfer.core.bean.Directory;
import top.weixiansen574.hybridfilexfer.core.bean.RemoteFile;
import top.weixiansen574.hybridfilexfer.core.callback.TransferFileCallback;
import top.weixiansen574.nio.DataByteChannel;

public abstract class HFXService {
    public static final String CLIENT_HEADER = "HFXC";
    /**
     * 301：新增暂停状态同步（PAUSE_STATE）。
     * <p>
     * 302：传输期间控制通道改成带 opcode 的帧（RESULT / PAUSE_REPLY）。
     * 以前收尾是一串裸 bool，发送线程在整段传输里死等它，只能连着 controlLock 一起占住控制通道 ——
     * 会话线程既同步不了暂停状态、也发不了心跳（大文件传到一半电脑就判心跳超时断开）。
     * 现在传输线程是控制通道<b>唯一</b>的读者，别人的暂停回报在它的读循环里就地处理。
     */
    /**
     * 303：完整性校验。
     * <ul>
     *   <li>每个文件块头多一个 int CRC32 —— 发送端随路算，接收端写盘前校验，坏块立刻中断；</li>
     *   <li>结论帧 RESULT 多一段「文件指纹清单」（字节数 + SHA-256），收尾时两端各自复算整文件哈希比对。</li>
     * </ul>
     * <p>
     * 304：取消不断连。
     * <ul>
     *   <li>CANCEL 的语义变了：发完 CANCEL <b>只关传输通道</b>，控制通道保持打开 ——
     *       以前是连控制通道一起关，手机端回主页、电脑端报「连接已断开」再重连，体验很糙；</li>
     *   <li>新增 REBUILD_REQ / REBUILD_ACK 两个 opcode，电脑收尾完把传输通道重新连一批回来，
     *       整条会话不断，取消后立刻能再传。</li>
     * </ul>
     * <p>
     * 305：断点续传。
     * <ul>
     *   <li>接收方给每个文件留一份 {@code <文件名>.hfxpart}（已完成块的位图）；</li>
     *   <li>发送方开传前先发 RESUME_QUERY（文件清单），接收方回 RESUME_REPLY（位图），
     *       发送方跳过已经收到的块，只补缺的；</li>
     *   <li>取消/中断时半截文件不再删除，留着下次续传。</li>
     * </ul>
     */
    // 306 → 307：双端限速协商（握手阶段交换两端的上限，各自算 min(非零)）。
    // 307 → 308：限速改成**两端共用一个值**（谁改了都推给对方，两端永远同一个数）。
    //   307 那套「各报各的取 min」两端算出的生效值虽然一样，但界面上各写各的数
    //   （电脑写 20、手机写 5、实际跑 5，两边都还显示自己的），用户看到的和实际跑的对不上。
    //   共用值怎么定见 HFXServer.negotiateLimitMbps（和电脑端 session.rs 严格一致）；
    //   连上之后再改走 ControllerIdentifiers.SPEED_LIMIT 帧。
    //   0 仍然只是「不限速」，不是「限到 0 字节」。
    // 308 → 309：新增 QUEUE_STATE（手机 → 电脑，只发不答）：手机上「分享发送」的队列快照。
    //   为什么要它：电脑那边只看得见"正在传的那一个文件"，看不见手机队列里还排着谁、
    //   前一个传完是成功还是失败。加上这一帧之后，握手后先推一次，队列一变就推一次，
    //   传输期间每秒兜底一次，电脑界面和手机队列说的是同一件事。
    //   它是纯增量指令：只多一帧，已有帧的载荷和顺序一个字节都不动。
    public static final int VERSION_CODE = 309;
    protected final LinkedBlockingDeque<ByteBuffer> buffers = new LinkedBlockingDeque<>();
    protected DataByteChannel ctChannel;
    /**
     * 控制通道锁：所有对控制通道的读写都要拿它。
     *
     * <p>注意只在真正读写控制通道时持有，**不要**在整段传输期间占着它 ——
     * 会话线程每 250ms 要用控制通道跑暂停状态同步（PAUSE_STATE），
     * 占着的话传输期间暂停状态就同步不过去（两边点了暂停对方都没反应）。
     */
    protected final Object controlLock = new Object();
    protected List<TransferConnection> connections;
    /** 当前传输的暂停闸门（每次传输新建一个，暂停/继续都作用在它上面） */
    protected volatile TransferPause pauseGate = new TransferPause();
    /**
     * 302：手机正在发送文件（sendFiles）。
     *
     * <p>这段时间里 {@link #sendFiles} 的那个线程是控制通道唯一的读者，
     * 会话线程只能写（心跳、暂停状态），否则两个读者会互相吃掉对方的帧。
     */
    protected volatile boolean sendingFiles;
    /** 电脑端上次告诉我们的暂停状态：只有变了才应用，避免覆盖手机端刚点的暂停 */
    protected boolean lastPcPaused;
    /**
     * 用户点了「取消」：传输线程据此把这次中断当成正常取消，而不是错误。
     *
     * <p>取消的实现是「先发 CANCEL 帧，再关全部 socket」，所以发送/接收线程
     * 必然会抛 ClosedChannelException / AsynchronousCloseException ——
     * 那是预期收尾，不能再弹「通道错误」。
     */
    protected volatile boolean transferCancelled;
    /**
     * 304：本次传输被取消，等电脑端把传输通道重建回来（会话不断）。
     *
     * <p>取消后控制通道仍然打开，电脑端收尾完会连一批新的传输通道、发 REBUILD_REQ 过来；
     * 会话线程看到这个标志就去跑重建握手，而不是当成断线。
     */
    protected volatile boolean needsRebuild = false;
    /**
     * 「电脑端取消」标记：用户在电脑上点了「取消 → 保留进度」/「中断发送」。
     *
     * <p>为什么不复用 {@link #transferCancelled}：电脑端的顺序是
     * <b>先 shutdown 传输通道，再发 CANCEL 帧</b>（它必须先把 socket 关掉，
     * 手机端阻塞在 write 里的发块线程才停得下来）。于是手机端这边有两件事同时在跑：
     * <ul>
     *   <li>发块线程先撞上那次写异常（ClosedChannelException / AsynchronousCloseException），</li>
     *   <li>读控制通道的线程过一会儿才读到 CANCEL。</li>
     * </ul>
     * 两者之间有几十毫秒的窗口：那一刻 transferCancelled 还是 false，
     * 写异常就被判成「通道错误」，界面弹「发送失败：传输时发生异常，请返回重连！」
     * （真机复现的就是这一条）。
     *
     * <p>现在这个标志是<b>唯一</b>的「对端取消」判据：
     * <ul>
     *   <li>{@link #cancelByPeer(String)} 第一行就置位（早于任何关通道动作）；</li>
     *   <li>读帧线程读到 CANCEL / REBUILD_REQ 时补一次（幂等）；</li>
     *   <li>所有异常分类都看 {@link #isCancelLike()}，写异常还会等一小会儿让它落位；</li>
     *   <li>下一次传输开跑时由 {@link #beginTransfer()} 清零，不影响下一轮。</li>
     * </ul>
     */
    protected volatile boolean peerCancelled;
    /**
     * 这一次对端取消还额外要求「把已经收到的那半截也删掉」({@link ControllerIdentifiers#CANCEL_DISCARD})。
     *
     * <p>只有<b>发件方</b>会发这一帧：它在取消确认框里选了「不保留」，语义是让收件方把这轮
     * 写出去的文件连 {@code <文件名>.hfxpart} 续传日志一起删掉，下次同一个文件从头传。
     * 手机当发件方时是电脑端删（{@code HFXServer.abortTransfer} 读 discard 选 opcode），
     * 这里是反过来 —— 电脑当发件方，手机当收件方。
     *
     * <p>默认（{@link ControllerIdentifiers#CANCEL}，或本机取消、通道出错）<b>仍然保留</b>
     * 半截文件和续传日志，下次接着传 —— 那是 305 起的既有功能，不能因为收尾走了同一个
     * 分支就变成默认删除。
     *
     * <p>volatile：写它的可能是会话线程（{@code HFXServer.handleUnexpectedControlFrame}
     * 在暂停同步里读到取消帧），读它的是接收传输线程的收尾。每次 {@link #receiveFiles}
     * 开头清零，不会串到下一轮。
     */
    protected volatile boolean discardOnCancel;
    /**
     * 304：在别的读循环里顺手读到的、还没处理的控制帧头。
     *
     * <p>取消/重建期间帧序不是我们预期的（电脑可能在飞 PAUSE_STATE 的回报、也可能已经开始
     * 重建），读到一帧却又不是当前循环要等的那一种时，把帧头记在这里交给后续循环，
     * 绝不能丢 —— 丢一个帧头，整条控制通道的帧流就错位了。
     */
    protected volatile short pendingOpcode = -1;
    /** 304：在别的读循环里读到的重建请求通道名，交给 rebuildTransfers 消费 */
    private final java.util.Queue<String> pendingRebuildNames = new java.util.concurrent.ConcurrentLinkedQueue<>();
    /** 本次发送的读文件任务：界面从它拿"当前文件被切成哪几段"画动态进度条 */
    private volatile ReadFileCall currentReadFileCall;

    /**
     * 本次传输的分块进度（一块 1MiB，记"哪条通道传了第几块"）。
     *
     * <p><b>两个方向各一份，但用的是同一个类、同一套口径</b>：
     * <ul>
     *   <li>{@link #blockProgress}：接收方向（电脑推送 / 双窗口「← 复制」拉取）——块到达时记；</li>
     *   <li>{@link #sendProgress}：发送方向（双窗口「复制 →」）——块真正写出去以后才记
     *       （不能记"投递进队列"那一刻，队列里可能压着几百 MB，进度条会跑到真值前面）。</li>
     * </ul>
     * 界面（双窗口弹窗、应用外悬浮窗）每 50ms 取一次 {@code snapshot()}，和主页接收进度条
     * 用的是同一份逻辑：分段 = 相邻块归属变化处，每段填充 = 段内已完成块占比。
     */
    public final BlockProgress blockProgress = new BlockProgress();
    public final BlockProgress sendProgress = new BlockProgress();

    /**
     * 收尾校验（整文件 SHA-256）的进度：已算字节 / 这一轮要算的总字节。
     *
     * <p>校验这一步以前在界面上是"没有进度的等待"（大文件实测一动不动两分钟，被当成卡死）。
     * 现在界面在数据传完时把总进度条清零，直接拿这两个数接着画校验进度 ——
     * 传输进度和校验进度复用同一条进度条，用户看到的是一条连续的线。
     */
    private static final java.util.concurrent.atomic.AtomicLong VERIFY_DONE =
            new java.util.concurrent.atomic.AtomicLong();
    private static final java.util.concurrent.atomic.AtomicLong VERIFY_TOTAL =
            new java.util.concurrent.atomic.AtomicLong();

    /** 校验已经算完多少字节 */
    public static long verifyDoneBytes() {
        return VERIFY_DONE.get();
    }

    /** 这一轮校验总共要算多少字节；0 表示当前没有校验在进行 */
    public static long verifyTotalBytes() {
        return VERIFY_TOTAL.get();
    }

    /** 新一轮传输开始：把校验进度清零（不然后一次传输会接着上一次的百分比走） */
    public static void resetVerifyProgress() {
        VERIFY_DONE.set(0);
        VERIFY_TOTAL.set(0);
    }

    /**
     * 发送方向的分段计划（历史实现：按 SendRanges 的百分比区间画）。
     *
     * <p>已被 {@link BlockProgress#snapshot()} 取代 —— 计划里的区间是"打算这么切"，
     * 而界面要的是"实际谁在传、传了多少"。保留这个访问器只为兼容，别再拿它画进度条。
     */
    public java.util.List<SendRanges.Segment> currentSendSegments() {
        ReadFileCall call = currentReadFileCall;
        return call == null ? java.util.Collections.emptyList() : call.segments();
    }

    /** 界面高频轮询用的单通道流量快照（累计真值，只读副本） */
    public static final class ChannelTrafficSnapshot {
        public final String name;
        public final long uploaded;
        public final long downloaded;

        ChannelTrafficSnapshot(String name, long uploaded, long downloaded) {
            this.name = name;
            this.uploaded = uploaded;
            this.downloaded = downloaded;
        }
    }

    /**
     * 取一次所有传输通道的累计流量快照（发送/接收方向通用）。
     *
     * <p>界面以前靠 {@code onSpeedInfo} 每秒一次的"这一秒流量"自己累加，再把累计值
     * 当进度用 —— 两次回调之间只能拿速率去猜，猜不准就出现"每秒回跳一格"。
     * 这里直接给真值，界面 80~100ms 轮询一次即可，和接收方向（按块统计的 done）同一套口径。
     *
     * <p>顺序与 {@code connections} 一致：进度条分段的 owner 就是这里的 name。
     */
    public java.util.List<ChannelTrafficSnapshot> trafficSnapshot() {
        List<TransferConnection> list = connections;
        if (list == null || list.isEmpty()) {
            return java.util.Collections.emptyList();
        }
        java.util.ArrayList<ChannelTrafficSnapshot> out = new java.util.ArrayList<>(list.size());
        try {
            for (TransferConnection connection : list) {
                if (connection == null) {
                    continue;
                }
                top.weixiansen574.hybridfilexfer.core.bean.TrafficInfo info = connection.snapshotTotalTraffic();
                out.add(new ChannelTrafficSnapshot(info.iName, info.uploadTraffic, info.downloadTraffic));
            }
        } catch (Exception ignored) {
            // 304：取消后重建会把 connections 整体换掉，读到一半抛异常就当这一帧没取到
        }
        return out;
    }

    /** 本次传输是否被用户主动取消 */
    public boolean isTransferCancelled() {
        return transferCancelled;
    }

    /** 304：是否需要等电脑端重建传输通道 */
    public boolean isNeedsRebuild() {
        return needsRebuild;
    }

    public void setNeedsRebuild(boolean value) {
        needsRebuild = value;
    }

    /** 本次传输是不是「电脑端取消的」（界面据此显示"已取消/未完成"，而不是「发送失败」） */
    public boolean isPeerCancelled() {
        return peerCancelled;
    }

    /**
     * 本次传输是不是已经作废（本端取消 / 对端取消 / 等电脑重建通道）。
     *
     * <p>异常分类的统一判据：传输线程的 ClosedChannelException / AsynchronousCloseException
     * 在这些状态下都是预期收尾，不能报成「通道错误」。
     */
    public boolean isCancelLike() {
        return peerCancelled || transferCancelled || needsRebuild;
    }

    /**
     * 等一次「对端取消」信号（有界，最多 {@code timeoutMs}）。
     *
     * <p>电脑端是先关传输通道、再发 CANCEL；手机端的发块线程往往先撞上写异常。
     * 写异常真伪的判据只有「对端取消没取消」，所以这里给它一个很短的窗口，
     * 让读控制通道的线程把 CANCEL 读进来（正常几毫秒到几十毫秒就到）。
     *
     * <p>超时时间要远小于"看起来像卡死"的量级：真出错（拔网线、电脑崩溃）时
     * 只是晚 {@code timeoutMs} 报出来，而取消时不会误报。
     *
     * @return true 表示这段时间里确实被判成了取消（本端或对端）
     */
    protected boolean awaitCancelSignal(long timeoutMs) {
        if (isCancelLike()) {
            return true;
        }
        long deadline = System.currentTimeMillis() + Math.max(0, timeoutMs);
        while (System.currentTimeMillis() < deadline) {
            if (isCancelLike()) {
                return true;
            }
            // 控制通道已经没了：这是真断线，不用再等（取消时控制通道一直是活的）
            DataByteChannel channel = ctChannel;
            if (channel == null || !channel.isOpen()) {
                return false;
            }
            try {
                Thread.sleep(20);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return isCancelLike();
            }
        }
        return isCancelLike();
    }

    protected void stashRebuildName(String name) {
        if (name != null) {
            pendingRebuildNames.add(name);
        }
    }

    /**
     * **电脑端抢的取消**（不是手机界面点的）：置标志 + 放开暂停闸门 + 关掉所有传输通道。
     *
     * <p>为什么必须关 socket，光置标志不够：本端是发送方时，发块线程正一门心思往
     * 传输通道里写，写满 TCP 缓冲就阻塞在 {@code writeFully} 里 —— 它自己不会回头
     * 看任何标志。电脑端点了「取消 → 保留进度」之后不再读数据，我们这边就一直
     * 「还在继续发」（用户报的第 1 个问题）。只有把 socket 关掉，那次 write 才会
     * 以 AsynchronousCloseException 立刻返回。
     *
     * <p>顺便打上 {@link TransferConnection#markCancelled()}：SendFileCall /
     * ReceiveFileCall 的 catch 据此把这次异常当"取消"而不是"通道错误"，
     * 不会再弹一条红色通道错误（用户报的第 2 个问题里那个报错）。
     *
     * <p>还要放开暂停闸门：电脑端点「取消」时是先把手机暂停住的（它弹确认框那几秒
     * 不让继续灌数据），暂停中的线程停在 {@code TransferPause.awaitResume()} 里，
     * 不放开就永远不会退出；而下一轮传输会换一个新的闸门对象，这批老线程就永远挂着，
     * 一直占着共享的 native 缓冲区池（下一次传输实测会报 newPosition > limit）。
     *
     * <p>幂等：重复调用只会再关一遍已经关掉的 socket，不会写任何控制帧。
     * 调用时**不能**持有 controlLock（{@link #readResultFrameOrNull} 在
     * receiveFiles 的收尾里是持锁调的），这里只碰通道和闸门。
     */
    protected void cancelByPeer(String reason) {
        // 第一件事就是立「对端取消」的旗：它必须早于下面任何一次关通道的动作。
        // 关通道会让发块线程立刻抛写异常，那个 catch 只看这个标志（见 SendFileCall），
        // 标志晚一步，用户看到的就还是「传输时发生异常，请返回重连」。
        boolean first = !peerCancelled;
        peerCancelled = true;
        if (first || !transferCancelled) {
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("cancelByPeer: " + reason);
        }
        transferCancelled = true;
        needsRebuild = true;
        try {
            TransferPause gate = pauseGate;
            if (gate != null) {
                gate.resume();
            }
        } catch (Throwable ignored) {
        }
        shutdownTransferChannels(reason);
    }

    /**
     * 对端取消，而且明确要求「已经收到的那半截也别留」(CANCEL_DISCARD)。
     *
     * <p>取消本身和 {@link #cancelByPeer} 一模一样（置标志 + 放开暂停闸门 + 关传输通道 +
     * 等重建），只多记一个 {@link #discardOnCancel}：接收方向的收尾据此决定删不删本轮的
     * 文件和续传日志。两帧都不带载荷，帧流不会因此错位。
     *
     * <p>可能被会话线程和传输线程同时碰到（谁先读到这一帧就谁处理），重复调用无害。
     */
    protected void cancelByPeerDiscard(String reason) {
        discardOnCancel = true;
        cancelByPeer(reason);
    }

    /**
     * 关掉当前所有传输通道（先打"取消"标，收尾时不弹通道错误）。
     *
     * <p>幂等，任何线程都能调；返回关掉了几条。重建时
     * {@code HFXServer.beginRebuild()} 会把这些已经关掉的通道从表里摘掉，
     * 不会出现"旧通道还在表里 + 新通道又加进来"的双份通道
     * （以前电脑端取消时没人关通道，重建后 6 条变 12 条，下一次传输一半线程
     * 一开始就写失败 —— 用户报的"再取消一次手机端报错"）。
     */
    protected int shutdownTransferChannels(String reason) {
        List<TransferConnection> list = connections;
        if (list == null || list.isEmpty()) {
            return 0;
        }
        int closed = 0;
        for (TransferConnection connection : list) {
            if (connection == null) {
                continue;
            }
            try {
                // 先打标再关：收尾时按"取消"处理，不当通道错误
                connection.markCancelled();
                connection.close();
                closed++;
            } catch (Throwable ignored) {
            }
        }
        top.weixiansen574.hybridfilexfer.core.HFXLog.d(
                "shutdownTransferChannels(" + reason + "): closed " + closed);
        return closed;
    }

    /** 取一个提前读到的重建通道名；没有返回 null */
    public String pollRebuildName() {
        return pendingRebuildNames.poll();
    }

    protected boolean sendFiles(List<RemoteFile> fileList, Directory localDir, Directory remoteDir, TransferFileCallback callback) throws IOException {
        return sendFiles(fileList, localDir, remoteDir, callback, null);
    }

    /**
     * 305：带续传位图的发送。
     *
     * @param resumeBits 目标路径 → "已经收到的块"位图（null/空 = 全新传输）
     */
    protected boolean sendFiles(List<RemoteFile> fileList, Directory localDir, Directory remoteDir,
                                TransferFileCallback callback,
                                java.util.Map<String, java.util.BitSet> resumeBits) throws IOException {
        // 限速计时窗随传输重新开始，避免上次 setLimit 之后的空闲时间变成免费流量额度
        SpeedLimiter.resetWindow();
        pauseGate = new TransferPause();
        lastPcPaused = false;
        // 通道名按 connections 的顺序给"分段计划"用（进度条上的 owner 就是它）
        String[] channelNames = new String[connections.size()];
        for (int i = 0; i < connections.size(); i++) {
            channelNames[i] = connections.get(i).iName;
        }
        // 305：告诉界面"已经有这么多字节在电脑上了"，进度条才不会从 0 开始涨
        long skippedTotal = resumeSkippedBytes(fileList, localDir, remoteDir, resumeBits);
        if (skippedTotal > 0) {
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("sendFiles: resume, already on PC = " + skippedTotal + " bytes");
            callback.onResumeSkipped(skippedTotal);
        }
        ReadFileCall readFileCall = createReadFileCall(buffers, fileList, localDir, remoteDir, connections.size(), channelNames, resumeBits);
        // 界面画"动态分段进度条"要读它：当前文件被切成了哪几段、每段归哪条通道
        this.currentReadFileCall = readFileCall;
        // 分块进度换成本次传输的新账本（上一轮的对象可能还有线程在写，别 reset 复用）
        sendProgress.reset();
        readFileCall.setProgress(sendProgress);
        readFileCall.setPause(pauseGate);
        FutureTask<Void> readFileTask = new FutureTask<>(readFileCall);
        Thread readThread = new Thread(readFileTask);
        readThread.setName("FileRead");
        readThread.start();
        //另开一个线程读取传输流量信息，1秒一次
        SpeedMonitorThread speedMonitorThread = new SpeedMonitorThread(connections, callback);
        speedMonitorThread.setName("SpeedMonitor");
        speedMonitorThread.start();
        long startTime = System.currentTimeMillis();
        List<FutureTask<Void>> transferTasks = new ArrayList<>(connections.size());
        for (int i = 0; i < connections.size(); i++) {
            TransferConnection connection = connections.get(i);
            // 传 this 进去：写异常的分类要问「这次传输是不是已经被对端取消了」（见 SendFileCall）
            FutureTask<Void> task = new FutureTask<>(new SendFileCall(readFileCall, connection, callback, pauseGate, i, this));
            transferTasks.add(task);
            Thread thread = new Thread(task);
            thread.setName("UL_" + connection.iName);
            thread.start();
        }

        // 302：本线程是控制通道唯一的读者（会话线程只写不读），可以放心循环读帧：
        // 电脑的落盘结论（RESULT）就地取走，暂停回报（PAUSE_REPLY）就地应用。
        // 以前这里死等一个裸 bool，controlLock 被占满整个传输过程 ——
        // 会话线程既同步不了暂停状态、也发不了心跳（大文件传到一半电脑判心跳超时断开）。
        sendingFiles = true;
        top.weixiansen574.hybridfilexfer.core.HFXLog.d("sendFiles: reader=me (sendingFiles=true)");
        String writeError = null;
        PeerResult pcResult = null;
        try {
            pcResult = readResultFrameOrNull();
            writeError = (pcResult.ok || pcResult.cancelled) ? null : pcResult.message;
        } catch (Throwable e) {
            // 用 Throwable：以前只 catch IOException，别的异常（比如缓冲区被并发写坏
            // 引起的 BufferUnderflowException）会从这里抛出去，日志只留下
            // 一句 "result=null"，看不出是异常退出还是真正的成功。
            speedMonitorThread.cancel();
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("sendFiles: readResultFrame failed", e);
            awaitTransferThreads(readThread, readFileTask, transferTasks, 3000);
            callback.onIncomplete();
            return false;
        } finally {
            sendingFiles = false;
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("sendFiles: reader=none (sendingFiles=false), result=" + writeError);
        }
        speedMonitorThread.cancel();

        // 304：取消（本端点取消，或电脑端点「中断发送」）：
        // 这次传输作废，但**不写结论帧** —— 控制通道马上要交给重建握手，
        // 多写一帧会让电脑端把 REBUILD_ACK 读成别的帧，帧流一错位整条会话就报废。
        if (pcResult == null || pcResult.cancelled) {
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("sendFiles: cancelled, skip result frame");
            // 3.0.0：取消时先把传输通道关掉再等线程 —— 发块线程此刻多半阻塞在
            // writeFully 里（电脑端不读了，缓冲写满），只有 socket 被关掉那次写才会
            // 立刻抛出来，awaitTransferThreads 也才有机会在几百毫秒内退完
            // （以前靠电脑端重建时才 shutdown，等于要等它走完 join+收尾，
            //   手机这边整整几秒还"在发"）。这行对已经关过的通道是幂等的。
            // 304：取消后不能立刻返回 —— 读文件线程（FileRead）和发块线程可能还在用
            // 这条连接共享的 native 缓冲区池。等它们退完，会话线程才会进重建；
            // 否则"取消完立刻再传"时两批线程抢同一个 ByteBuffer，实测下一次传输
            // 直接 IllegalArgumentException: newPosition > limit（缓冲区被二次回收/复用）。
            shutdownTransferChannels("sendFiles: cancelled before join");
            awaitTransferThreads(readThread, readFileTask, transferTasks, 5000);
            callback.onIncomplete();
            return false;
        }

        if (writeError != null) {
            callback.onWriteFileError(writeError);
            readFileCall.shutdownByWriteError();
            return true;
        }

        for (FutureTask<Void> transferTask : transferTasks) {
            try {
                transferTask.get();
            } catch (ExecutionException | InterruptedException e) {
                callback.onIncomplete();
                return false;
            }
        }

        long totalUploadTraffic = 0;
        for (TransferConnection connection : connections) {
            totalUploadTraffic += connection.resetTotalTrafficInfo().uploadTraffic;
        }

        try {
            readFileTask.get();
            // 303：把发出去的文件复算一遍 SHA-256，和电脑端落盘文件的指纹比对。
            // 手机上这一步等于把文件再读一遍（3.25GB 约几秒），换来的是"字节级一致"的确认。
            // 指纹在传输一开始就由后台线程在算（ReadFileCall.prefetchDigest），这里通常只是取结果
            String verifyError = pcResult == null || pcResult.digests.isEmpty()
                    ? null
                    : compareDigests(digestOf(readFileCall.sourcePaths(), readFileCall.digestTasks()),
                    pcResult.digests);
            readFileCall.shutdownDigest();
            if (verifyError != null) {
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("sendFiles: verify FAILED: " + verifyError);
            } else if (pcResult != null && !pcResult.digests.isEmpty()) {
                callback.onVerifyChecked(true, pcResult.digests.size());
            }
            synchronized (controlLock) {
                writeResultFrame(verifyError == null, verifyError, null);
            }
            if (verifyError != null) {
                callback.onReadFileError("文件校验失败：" + verifyError);
                return true;
            }
        } catch (ExecutionException | InterruptedException e) {
            Throwable cause = e.getCause();
            String ex = cause != null ? cause.toString() : e.toString();
            synchronized (controlLock) {
                writeResultFrame(false, ex, null);
            }
            callback.onReadFileError(ex);
            return true;
        }

        callback.onComplete(true,totalUploadTraffic, System.currentTimeMillis() - startTime);
        return true;
    }

    protected boolean receiveFiles(TransferFileCallback callback) throws IOException {
        return receiveFiles(callback, null);
    }

    /**
     * 306：带续传位图的接收。
     *
     * @param resumeBits 目标路径 → 接收端已有的块（null/空 = 全新传输）
     */
    protected boolean receiveFiles(TransferFileCallback callback,
                                   java.util.Map<String, java.util.BitSet> resumeBits) throws IOException {
        // 对称：接收方向的限速计时窗也随传输重置（PC 端若实现限速时同样受益）
        SpeedLimiter.resetWindow();
        pauseGate = new TransferPause();
        lastPcPaused = false;
        // 新一轮接收开始：上一轮"取消要不要删半截文件"的结论不能带进来
        discardOnCancel = false;
        WriteFileCall writeFileCall = createWriteFileCall(buffers, connections.size(), resumeBits);
        long startTime = System.currentTimeMillis();
        // 分块进度换成本次传输的新账本（和发送方向同一套口径，见 blockProgress 的注释）
        blockProgress.reset();
        // 306：手机本地已经有的块也要先记进来。不记的话续传时进度条从 0 开始涨——
        // 实测 3.25 GB 的文件已有 3.0 GB，浮层却显示「324 MB / 3.25 GB」。
        // 归属记空串（和发送方向"电脑已收到的块"同一个口径），真实通道的块到了再按通道分段。
        if (resumeBits != null) {
            for (java.util.Map.Entry<String, java.util.BitSet> entry : resumeBits.entrySet()) {
                if (entry.getValue() == null || entry.getValue().isEmpty()) {
                    continue;
                }
                for (int i = entry.getValue().nextSetBit(0); i >= 0; i = entry.getValue().nextSetBit(i + 1)) {
                    blockProgress.onBlock("", entry.getKey(), i, 0);
                }
            }
        }

        SpeedMonitorThread speedMonitorThread = new SpeedMonitorThread(connections, callback);
        speedMonitorThread.setName("SpeedMonitor");
        speedMonitorThread.start();

        List<FutureTask<Void>> transferTasks = new ArrayList<>(connections.size());
        for (int i = 0; i < connections.size(); i++) {
            TransferConnection connection = connections.get(i);
            FutureTask<Void> task = new FutureTask<>(new ReceiveFileCall(i, connection, writeFileCall, callback, pauseGate, blockProgress));
            transferTasks.add(task);
            Thread thread = new Thread(task);
            thread.setName("DL_" + connection.iName);
            thread.start();
        }
        FutureTask<Void> writeFileTask = new FutureTask<>(writeFileCall);
        Thread thread = new Thread(writeFileTask);
        thread.setName("FileWrite");
        thread.start();
        try {
            writeFileTask.get();
        } catch (InterruptedException | ExecutionException e) {
            speedMonitorThread.cancel();
            Throwable cause = e.getCause();
            String ex = cause != null ? cause.toString() : e.toString();
            synchronized (controlLock) {
                writeResultFrame(false, ex, null);
            }
            // 306：写盘线程挂了也保留半截文件，位图落盘，下次同一个文件从这里续上
            // （电脑端这次说过「不保留」的话就照它说的删，见 settleCancelledReceive）
            settleCancelledReceive(writeFileCall, writeFileTask);
            callback.onWriteFileError(ex);
            return true;
        }

        for (FutureTask<Void> task : transferTasks) {
            try {
                task.get();
            } catch (InterruptedException | ExecutionException e) {
                speedMonitorThread.cancel();
                // 304：电脑端点的是「中断发送」时，它会先发一帧 CANCEL 再关传输通道。
                // 接收期间本线程不读控制通道（读权在电脑那边），所以这里探一下：
                // 有 CANCEL 就当成"取消"收尾 —— 不写结论帧、不报错，等它重建通道。
                if (!isTransferCancelled() && probePeerCancel(800)) {
                    top.weixiansen574.hybridfilexfer.core.HFXLog.d("receiveFiles: peer cancelled");
                    // 306：默认是「半截文件 + 续传日志都保留，下次同一个文件只补缺的块」；
                    // 电脑端发的是 CANCEL_DISCARD（它选了「不保留」）就直接删，见 discardOnCancel
                    settleCancelledReceive(writeFileCall, writeFileTask);
                    callback.onIncomplete();
                    return false;
                }
                // 用户点了「取消」时控制通道可能已经跟着关了，写结论帧必然失败，
                // 那是预期内的收尾，不能让它变成异常抛到界面上。
                if (!isTransferCancelled()) {
                    //此时没有连同控制器通道一起断掉，要通知对方，写线程没问题（对方的传输线程通道已出问题）
                    try {
                        synchronized (controlLock) {
                            writeResultFrame(true, null, null);
                        }
                    } catch (IOException ignored) {
                    }
                }
                // 306：不再删半截文件 —— 续传日志（<文件名>.hfxpart）记着哪些块已经落盘，
                // 下次同一个文件（大小/修改时间一致）只补缺的块，不会出现"大小对、内容有洞"的假文件。
                // 例外：上面那行 probePeerCancel 没读到帧、但会话线程的暂停同步先把它接走了
                // （HFXServer.handleUnexpectedControlFrame），本机已被判成取消时走这里 ——
                // settleCancelledReceive 会按 discardOnCancel 决定删还是留。
                settleCancelledReceive(writeFileCall, writeFileTask);
                callback.onIncomplete();
                return false;
            }
        }
        speedMonitorThread.cancel();
        // 306：收尾时把续传日志结清（传完的文件删掉 sidecar）
        writeFileCall.finish();
        // 303 收尾：先报"落盘结论 + 落盘文件的指纹"，再读电脑端的读文件结论。
        // 306：要算的文件里包含"续传时已经全在手机上"的那些（它们这次一块都没收）
        List<FileDigest> digests = digestOf(writeFileCall.digestPaths());
        synchronized (controlLock) {
            writeResultFrame(true, null, digests);
            PeerResult pcResult = readResultFrameOrNull();
            if (pcResult.cancelled) {
                // 收尾这一瞬间对方把传输作废了（取消/开始重建）：别报"完成"
                callback.onIncomplete();
                return false;
            }
            String readError = pcResult.ok ? null : pcResult.message;
            if (readError == null) {
                long totalDownloadTraffic = 0;
                for (TransferConnection connection : connections) {
                    totalDownloadTraffic += connection.resetTotalTrafficInfo().downloadTraffic;
                }
                callback.onComplete(false,totalDownloadTraffic, System.currentTimeMillis() - startTime);
            } else {
                callback.onReadFileError(readError);
            }
        }
        return true;
    }

    /**
     * 取消 / 出错时删掉本次没收完的文件。
     *
     * <p>落盘是"先把文件预设成最终长度，再按块随机写"（createAndOpenFile），
     * 所以半途而废的文件在文件管理器里显示的是完整大小，实际是一片空洞 ——
     * 这种假文件比没有更害人，统一删掉。
     */
    private void removeIncompleteFiles(List<String> paths) {
        for (String path : paths) {
            try {
                File file = new File(path);
                if (!file.exists()) {
                    continue;
                }
                if (file.delete()) {
                    top.weixiansen574.hybridfilexfer.core.HFXLog.d("removed incomplete file: " + path);
                } else {
                    top.weixiansen574.hybridfilexfer.core.HFXLog.d("failed to remove incomplete file: " + path);
                }
            } catch (Throwable e) {
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("remove incomplete file failed: " + path, e);
            }
        }
    }

    /**
     * 接收方向「这次传不下去了」的统一收尾：留还是删，只看 {@link #discardOnCancel}。
     *
     * <ul>
     *   <li>false（默认：本机取消 / 通道出错 / 电脑端发的是普通 CANCEL）：<b>一字不变</b>，
     *       续传日志落盘、半截文件留着，下次同一个文件只补缺的块；</li>
     *   <li>true（电脑端发的是 CANCEL_DISCARD，它在自己的取消框里选了「不保留」）：
     *       本轮写出去的文件和它们的 {@code .hfxpart} 一起删掉，下次从头传。
     *       半截文件在文件管理器里显示的是完整大小、实际是一片空洞，用户既然明确说了
     *       不要，就别留这种假文件。</li>
     * </ul>
     *
     * <p>为什么先等写盘线程（有界）再删：
     * <ul>
     *   <li>它正拿着这些文件的句柄按块随机写，删掉路径后剩下的块会写进已经 unlink 的 inode
     *       （看不见的垃圾 IO）；</li>
     *   <li>更实际的是 {@link WriteFileCall#writtenPaths()} 只在它创建文件时才多一条 ——
     *       不等它退出就取，这一批里排在后面的文件可能还没被记上，就漏删了那份空洞文件。</li>
     * </ul>
     * 等不到也照删：删除本身幂等，且此后不会再有人创建这些路径。
     */
    private void settleCancelledReceive(WriteFileCall writeFileCall, FutureTask<Void> writeFileTask) {
        if (!discardOnCancel) {
            // 305/306 的既有行为：留半截文件 + 续传日志，下次续传
            writeFileCall.saveJournals();
            return;
        }
        if (writeFileTask != null) {
            try {
                writeFileTask.get(1500, TimeUnit.MILLISECONDS);
            } catch (Throwable ignored) {
                // 退不完也继续删：此时它要么已退出，要么剩下的写只会落进被删掉的 inode
            }
        }
        top.weixiansen574.hybridfilexfer.core.HFXLog.d(
                "receiveFiles: peer asked to discard the incomplete files");
        try {
            removeIncompleteFiles(writeFileCall.discardJournalsAndListWritten());
        } catch (Throwable e) {
            // 删不干净最多留个残骸，不能让这次"取消"被收尾异常改判成"传输失败"
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("discard incomplete files failed", e);
        }
    }

    /** 手机是否正在发送文件（发送线程独占控制通道的读取） */
    public boolean isSendingFiles() {
        return sendingFiles;
    }

    /**
     * 应用电脑端的暂停状态。
     *
     * <p>只有电脑端的状态<b>变了</b>才动本地闸门 —— 否则手机端刚点的暂停会被电脑端上报的旧状态
     * 立刻覆盖回来（表现为"点暂停马上又自己继续了"）。
     */
    protected void applyPcPause(boolean pcPaused) {
        if (pcPaused == lastPcPaused) {
            return;
        }
        lastPcPaused = pcPaused;
        if (pcPaused) {
            pauseGate.pause();
        } else {
            pauseGate.resume();
        }
    }

    /**
     * 308：电脑推来了共用限速值（{@code {i16 SPEED_LIMIT}{i32 MB/s}}）：就地采纳。
     *
     * <p>用 {@link SpeedLimiter#applySharedBytesPerSecond(long)} 而<b>不是</b>
     * applyLocalBytesPerSecond：这个数是电脑定的，提代次就意味着本机也「改了一次」，
     * 会话线程下一轮又会把它推回给电脑 —— 两端就此把同一个数对推一辈子。
     *
     * <p>载荷是固定 4 字节，所以每个读循环都必须把 i32 读干净再往下走：
     * 只吞掉 2 字节帧头的话，这 4 个字节会和下一帧的帧头拼成假 opcode，
     * 整条控制通道当场错位（本仓库踩过多次的坑）。
     */
    protected void onPeerSpeedLimit(int mbps) {
        int value = Math.max(0, mbps);
        SpeedLimiter.applySharedBytesPerSecond(SpeedLimiter.mbpsToBytesPerSecond(value));
        onSharedSpeedLimitAdopted(value);
    }

    /**
     * 308：共用限速值变了之后要做的「落地」动作。
     *
     * <p>默认什么也不做：纯客户端会话（HFXClient）没有持久化配置，也没有第二个进程要通知。
     * {@code HFXServer} 覆盖它去写配置 + 广播界面，并更新「已经推给电脑的代次」。
     */
    protected void onSharedSpeedLimitAdopted(int mbps) {
    }

    // ------------------------------------------------------------------
    // 303：完整性校验（块 CRC32 + 收尾整文件 SHA-256）
    // ------------------------------------------------------------------

    /** 一个文件的指纹：字节数 + SHA-256。两端各算一遍按集合比对，不带路径（两端路径本来就不一样）。 */
    public static final class FileDigest implements Comparable<FileDigest> {
        public final long size;
        public final byte[] sha256;

        public FileDigest(long size, byte[] sha256) {
            this.size = size;
            this.sha256 = sha256;
        }

        @Override
        public int compareTo(FileDigest o) {
            if (size != o.size) {
                return size < o.size ? -1 : 1;
            }
            for (int i = 0; i < sha256.length && i < o.sha256.length; i++) {
                int a = sha256[i] & 0xFF;
                int b = o.sha256[i] & 0xFF;
                if (a != b) {
                    return a - b;
                }
            }
            return 0;
        }

        @Override
        public boolean equals(Object o) {
            return o instanceof FileDigest && compareTo((FileDigest) o) == 0;
        }

        @Override
        public int hashCode() {
            return (int) (size ^ (size >>> 32));
        }
    }

    /** 对端（电脑）的结论帧 */
    public static final class PeerResult {
        public final boolean ok;
        /** 只有 ok == false 时有 */
        public final String message;
        /** 对端产出文件的指纹；本端是发送方时对端没有产出，空表 */
        public final List<FileDigest> digests;
        /**
         * 304：这次传输是被取消的（对端发来 CANCEL，或对端已经开始重建通道）。
         * 本端不要再写结论帧，也不要报成错误。
         */
        public final boolean cancelled;

        PeerResult(boolean ok, String message, List<FileDigest> digests) {
            this(ok, message, digests, false);
        }

        PeerResult(boolean ok, String message, List<FileDigest> digests, boolean cancelled) {
            this.ok = ok;
            this.message = message;
            this.digests = digests;
            this.cancelled = cancelled;
        }

        static PeerResult cancelled() {
            return new PeerResult(false, null, Collections.emptyList(), true);
        }
    }

    /** 一块数据的 CRC32（与电脑端 crc32fast 同算法：ISO-HDLC 0xEDB88320） */
    public static int crc32(ByteBuffer buffer) {
        CRC32 crc = new CRC32();
        if (buffer.hasArray()) {
            crc.update(buffer.array(), buffer.arrayOffset() + buffer.position(), buffer.remaining());
        } else {
            ByteBuffer copy = buffer.duplicate();
            byte[] chunk = new byte[Math.min(Math.max(copy.remaining(), 1), 1 << 16)];
            while (copy.hasRemaining()) {
                int n = Math.min(copy.remaining(), chunk.length);
                copy.get(chunk, 0, n);
                crc.update(chunk, 0, n);
            }
        }
        return (int) crc.getValue();
    }

    /** 单个文件的指纹；不是文件/读不出来返回 null（和"跳过这个文件"的老行为一致） */
    protected static FileDigest digestOne(String path) {
        try {
            File file = new File(path);
            if (!file.isFile()) {
                return null;
            }
            MessageDigest digest = MessageDigest.getInstance("SHA-256");
            // 4MB 一块：1MB 时 1.78GB 要读 1800 次，读的次数太多，收尾时间里有一截耗在这上面
            byte[] buffer = new byte[4 * 1024 * 1024];
            try (FileInputStream in = new FileInputStream(file)) {
                int read;
                while ((read = in.read(buffer)) > 0) {
                    digest.update(buffer, 0, read);
                    // 校验进度（界面的进度条读它）
                    VERIFY_DONE.addAndGet(read);
                }
            }
            return new FileDigest(file.length(), digest.digest());
        } catch (Throwable e) {
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("digest failed: " + path, e);
            return null;
        }
    }

    /** 把一组本地文件算成指纹表（排序后两端可直接比对）；读不出来的文件跳过 */
    protected static List<FileDigest> digestOf(List<String> paths) {
        return digestOf(paths, null);
    }

    /**
     * 把一组本地文件算成指纹表。
     *
     * <p>两条加速（用户反馈「校验时间有点长了」）：
     * <ul>
     *   <li>precomputed：发送方向在传输一开始就把文件丢给后台线程算了（{@code ReadFileCall}），
     *       这里只是 {@code get()} —— 那几秒藏进传输过程里，收尾几乎不用等；</li>
     *   <li>多文件按文件并行：SHA-256 本身没法拆，但"文件夹里几十个文件"可以几路一起读，
     *       以前是串行一个接一个。</li>
     * </ul>
     */
    protected static List<FileDigest> digestOf(List<String> paths,
                                               java.util.Map<String, java.util.concurrent.Future<FileDigest>> precomputed) {
        if (paths == null) {
            return new ArrayList<>();
        }
        // 校验进度：先把这一批要算的字节登记好、已算字节清零（预取阶段算过的那些不重复计）
        long verifyTotal = 0;
        for (String path : paths) {
            File file = new File(path);
            if (file.isFile()) {
                verifyTotal += file.length();
            }
        }
        VERIFY_TOTAL.set(verifyTotal);
        VERIFY_DONE.set(0);
        List<FileDigest> digests = java.util.Collections.synchronizedList(new ArrayList<>());
        List<String> todo = new ArrayList<>();
        for (String path : paths) {
            java.util.concurrent.Future<FileDigest> future = precomputed == null ? null : precomputed.get(path);
            if (future == null) {
                todo.add(path);
                continue;
            }
            try {
                FileDigest digest = future.get();
                if (digest != null) {
                    digests.add(digest);
                    // 传输时就已经算完的：直接算进进度，别让进度条停在 0% 等电脑端
                    VERIFY_DONE.addAndGet(digest.size);
                }
            } catch (Throwable e) {
                // 后台线程算失败（文件被删/线程池被关）：退回同步算一遍，别静默少一个文件
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("digest prefetch failed: " + path, e);
                FileDigest digest = digestOne(path);
                if (digest != null) {
                    digests.add(digest);
                }
            }
        }
        int threads = Math.min(4, todo.size());
        if (threads <= 1) {
            for (String path : todo) {
                FileDigest digest = digestOne(path);
                if (digest != null) {
                    digests.add(digest);
                }
            }
        } else {
            java.util.concurrent.ExecutorService pool =
                    java.util.concurrent.Executors.newFixedThreadPool(threads, r -> {
                        Thread t = new Thread(r, "hfx-digest");
                        t.setDaemon(true);
                        return t;
                    });
            try {
                for (String path : todo) {
                    pool.execute(() -> {
                        FileDigest digest = digestOne(path);
                        if (digest != null) {
                            digests.add(digest);
                        }
                    });
                }
                pool.shutdown();
                pool.awaitTermination(30, java.util.concurrent.TimeUnit.MINUTES);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            } finally {
                pool.shutdownNow();
            }
        }
        Collections.sort(digests);
        return digests;
    }

    /** 比对两端指纹表；一致返回 null，否则返回一句原因 */
    protected static String compareDigests(List<FileDigest> mine, List<FileDigest> theirs) {
        if (mine.size() != theirs.size()) {
            return "文件数量不一致：本端 " + mine.size() + " 个 / 电脑端 " + theirs.size() + " 个";
        }
        for (int i = 0; i < mine.size(); i++) {
            FileDigest a = mine.get(i);
            FileDigest b = theirs.get(i);
            if (a.compareTo(b) != 0) {
                return "第 " + (i + 1) + " 个文件的 SHA-256 不一致（本端 " + a.size + " 字节 "
                        + hex(a.sha256) + " / 电脑端 " + b.size + " 字节 " + hex(b.sha256) + "）";
            }
        }
        return null;
    }

    private static String hex(byte[] bytes) {
        StringBuilder builder = new StringBuilder(bytes.length * 2);
        for (byte b : bytes) {
            builder.append(Character.forDigit((b >> 4) & 0xF, 16));
            builder.append(Character.forDigit(b & 0xF, 16));
        }
        return builder.toString();
    }

    // ------------------------------------------------------------------
    // 控制通道
    // ------------------------------------------------------------------

    /** 等控制帧头超时（{@link #readControlHeaderPolled} 的返回值） */
    public static final short CONTROL_TIMEOUT = -1;

    /** 收尾结论：成功 */
    private static final Object RESULT_OK = new Object();
    /** 控制通道断了，等不到结论 */
    private static final Object RESULT_EOF = new Object();
    /** 专职读者线程读到的结论（RESULT_OK / 错误信息字符串 / RESULT_EOF） */
    private final LinkedBlockingQueue<Object> controlResults = new LinkedBlockingQueue<>();

    /**
     * 启动控制通道的唯一读者线程。
     *
     * <p>以前控制通道的读权在「会话线程」和「传输线程」之间来回交接：会话线程空闲时
     * 每 250ms 发 PAUSE_STATE 并同步读回 PAUSE_REPLY，传输期间改由传输线程读。
     * 交接窗口里两个读者会各吃半帧，帧流整体错位（电脑端读到 opcode 0，
     * 随后会话线程报错直接 disconnect，把正在传的六条通道一起关掉）。
     *
     * <p>现在读权永久归这个线程：会话线程和传输线程都只写，
     * 结论（RESULT）通过队列交给需要的线程。
     */
    protected void startControlReader() {
        Thread t = new Thread(this::controlReadLoop, "HFX-ControlReader-" + hashCode());
        t.setDaemon(true);
        t.start();
    }

    private void controlReadLoop() {
        try {
            while (true) {
                short opcode = ctChannel.readShort();
                if (opcode == ControllerIdentifiers.PAUSE_REPLY
                        || opcode == ControllerIdentifiers.PAUSE_STATE) {
                    // 302 之前电脑回的是裸 bool，302 起两边都是带 opcode 的帧
                    applyPcPause(ctChannel.readBoolean());
                } else if (opcode == ControllerIdentifiers.SPEED_LIMIT) {
                    // 308：电脑改了限速推过来的 {i32 MB/s}。载荷必须在这里读干净，
                    // 漏在这里会让这条读线程直接 break（整条控制通道的读者就没了）。
                    onPeerSpeedLimit(ctChannel.readInt());
                } else if (opcode == ControllerIdentifiers.PING) {
                    // 心跳，读到就行
                } else if (opcode == ControllerIdentifiers.RESULT) {
                    boolean ok = ctChannel.readBoolean();
                    controlResults.add(ok ? RESULT_OK : ctChannel.readUTF());
                } else {
                    top.weixiansen574.hybridfilexfer.core.HFXLog.d(
                            "controlReader: unexpected opcode " + opcode);
                    break;
                }
            }
        } catch (Throwable e) {
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("controlReader: stopped", e);
        } finally {
            controlResults.add(RESULT_EOF);
        }
    }

    /**
     * 等电脑的收尾结论：null = 成功，否则是错误信息；控制通道先断了就抛 IOException。
     */
    protected String awaitResult() throws IOException {
        Object r;
        try {
            // 电脑收尾要等它把最后一块落盘，给足时间
            r = controlResults.poll(120, TimeUnit.SECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new IOException("interrupted while waiting for RESULT");
        }
        if (r == null) {
            throw new IOException("timed out waiting for RESULT");
        }
        if (r == RESULT_EOF) {
            throw new IOException("control channel closed while waiting for RESULT");
        }
        return r == RESULT_OK ? null : (String) r;
    }

    /**
     * 302：写一帧结论 {@code [i16 RESULT][bool ok][UTF 错误信息?]}（成功时没有错误信息）。
     * 两个方向共用：接收方写"落盘结论"，发送方写"读文件结论"。
     */
    protected void writeResultFrame(boolean ok, String error, List<FileDigest> digests) throws IOException {
        int count = digests == null ? 0 : digests.size();
        // [i16 RESULT][bool ok][i32 n][(i64 size, 32B sha256) * n]
        byte[] frame = new byte[3 + 4 + count * 40];
        frame[0] = (byte) (ControllerIdentifiers.RESULT >> 8);
        frame[1] = (byte) ControllerIdentifiers.RESULT;
        frame[2] = (byte) (ok ? 1 : 0);
        putInt(frame, 3, count);
        int at = 7;
        for (int i = 0; i < count; i++) {
            FileDigest digest = digests.get(i);
            putLong(frame, at, digest.size);
            at += 8;
            System.arraycopy(digest.sha256, 0, frame, at, Math.min(32, digest.sha256.length));
            at += 32;
        }
        ctChannel.writeFully(frame);
        if (!ok) {
            ctChannel.writeUTF(error == null ? "" : error);
        }
    }

    private static void putInt(byte[] buffer, int offset, int value) {
        buffer[offset] = (byte) (value >>> 24);
        buffer[offset + 1] = (byte) (value >>> 16);
        buffer[offset + 2] = (byte) (value >>> 8);
        buffer[offset + 3] = (byte) value;
    }

    private static void putLong(byte[] buffer, int offset, long value) {
        for (int i = 7; i >= 0; i--) {
            buffer[offset + i] = (byte) value;
            value >>>= 8;
        }
    }

    /**
     * 304：等控制通道上的下一个帧头，但**可被本端取消打断**。
     *
     * <p>以前这里直接 {@code readShort()} 死等：发送方向上本线程是控制通道唯一的读者，
     * 用户点取消后它还在等电脑的结论帧，而电脑那边可能正停在"暂停"里（暂停时它只 peek
     * 传输通道，读不到我们的 CANCEL），于是两边谁也走不了 —— 实测卡满 15 秒后只能断线重连。
     * 现在按"至少 2 字节到齐"轮询，取消标志一置就返回 -1，会话线程立刻进入重建。
     *
     * <p>先确认 2 字节到齐再读：半个帧头被吃掉的话，剩下的字节会和下一帧的高位拼成
     * 假 opcode（实测读出 0），整条控制通道会永久错位。
     *
     * @return 帧头；本端已取消且通道上没有待处理帧时返回 -1
     */
    protected short pollControlOpcode() throws IOException {
        // 取消标志一置就退出等待（会话线程才好进重建），否则一直等帧头。
        return readControlHeaderPolled(-1, true);
    }

    /**
     * 从控制通道读一个帧头（两字节），**不用 Socket 的 available()**。
     *
     * <p>以前判断"有没有数据"用的是 {@code DataByteChannel.available()}，它走的是
     * Socket 的流式 InputStream；和 SocketChannel 直接读写混用时在真机上会永远返回 0。
     * 实测电脑端已经把结论帧写出来了（手机内核队列里 860 字节排着），手机这边
     * available() 一直是 0 → 收尾永久卡在「等待电脑校验」，两边互相等。
     * 这里直接对 SocketChannel 做非阻塞读，和重建握手用的 readControlOpcode 一致。
     *
     * <p>只在一个字节都还没读走时才认取消/超时：读到半个帧头就放弃的话，
     * 剩下的半个字节会和下一帧拼成假 opcode，整条控制通道永久错位。
     *
     * @param timeoutMs  <=0 表示不设超时
     * @param abortOnCancel 取消/待重建标志置位时是否提前返回 -1
     * @return 帧头；超时或取消提前退出时返回 {@link #CONTROL_TIMEOUT}
     */
    protected short readControlHeaderPolled(long timeoutMs, boolean abortOnCancel) throws IOException {
        DataByteChannel channel = ctChannel;
        if (channel == null) {
            // 会话线程已经 disconnect()（电脑关机/断线）：以前这里直接 NPE。
            throw new IOException("control channel closed while waiting for control frame");
        }
        java.nio.channels.SocketChannel socketChannel = channel.getSocketChannel();
        if (socketChannel == null) {
            return channel.readShort();
        }
        java.nio.ByteBuffer buffer = java.nio.ByteBuffer.allocate(2);
        long deadline = timeoutMs <= 0 ? Long.MAX_VALUE : System.currentTimeMillis() + timeoutMs;
        socketChannel.configureBlocking(false);
        try {
            while (true) {
                int read = socketChannel.read(buffer);
                if (read < 0) {
                    throw new java.io.EOFException("control channel closed by peer");
                }
                if (buffer.position() >= 2) {
                    break;
                }
                if (buffer.position() == 0) {
                    if (abortOnCancel && (transferCancelled || needsRebuild)) {
                        return CONTROL_TIMEOUT;
                    }
                    if (System.currentTimeMillis() >= deadline) {
                        return CONTROL_TIMEOUT;
                    }
                }
                try {
                    Thread.sleep(15);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    throw new IOException("interrupted while waiting for control frame", e);
                }
            }
        } finally {
            try {
                socketChannel.configureBlocking(true);
            } catch (IOException ignored) {
                // 通道已经断了，下一轮读会报出来
            }
        }
        buffer.flip();
        return buffer.getShort();
    }

    /**
     * 发请求前先把控制通道里"上一轮留下的"帧吃掉。
     *
     * <p>传输期间会话线程只写 PAUSE_STATE、不读回报（{@code writePauseStateOnly}），
     * 电脑回的那些 PAUSE_REPLY 会排在通道里。收尾后界面刷新目录，直接在 controlLock
     * 里发 LIST_FILES 并把残留的 PAUSE_REPLY 当成"列表长度"读走 —— 帧流一错位，
     * 读线程就卡在 readUTF 上，还握着 controlLock，会话线程连心跳都发不出去，
     * 电脑端 100 秒后判超时断线（真机实测）。
     *
     * <p>调用方必须已经持有 {@link #controlLock}。遇到重建/取消帧就地处理，
     * 遇到不认识的帧记进 pendingOpcode 后返回。
     */
    protected void drainStaleControlFrames() {
        while (true) {
            short opcode;
            try {
                opcode = readControlHeaderPolled(80, false);
            } catch (Throwable e) {
                return;
            }
            if (opcode == CONTROL_TIMEOUT) {
                return;
            }
            try {
                switch (opcode) {
                    case ControllerIdentifiers.PAUSE_REPLY:
                        applyPcPause(ctChannel.readBoolean());
                        break;
                    case ControllerIdentifiers.SPEED_LIMIT:
                        // 308：残留的限速帧（电脑在我们收尾时改的设置）。整帧 6 字节，
                        // 这里必须把 i32 读掉：只丢帧头会把 4 字节载荷留在通道里，
                        // 下一次读就成了假 opcode。
                        onPeerSpeedLimit(ctChannel.readInt());
                        break;
                    case ControllerIdentifiers.PING:
                        break;
                    case ControllerIdentifiers.RESULT:
                        // 上一次传输的收尾结论：没人要了，读干净丢掉
                        discardResultPayload(ctChannel);
                        break;
                    case ControllerIdentifiers.REBUILD_REQ:
                        stashRebuildName(ctChannel.readUTF());
                        // 电脑端已经在重建传输通道：这就是「对端取消」的另一种形态
                        peerCancelled = true;
                        needsRebuild = true;
                        return;
                    case ControllerIdentifiers.CANCEL:
                        peerCancelled = true;
                        transferCancelled = true;
                        needsRebuild = true;
                        return;
                    case ControllerIdentifiers.CANCEL_DISCARD:
                        // 和 CANCEL 一样是对端取消，另外记下「这轮收到的半截要删掉」。
                        // 走到这里说明收尾已经过去了（残留帧），没有 WriteFileCall 可删 ——
                        // 但绝不能当成不认识的帧塞进 pendingOpcode，那样下一个读循环
                        // （readResultFrameOrNull）会抛 "BAD opcode=31" 把会话打掉。
                        discardOnCancel = true;
                        peerCancelled = true;
                        transferCancelled = true;
                        needsRebuild = true;
                        return;
                    default:
                        pendingOpcode = opcode;
                        return;
                }
            } catch (Throwable e) {
                return;
            }
        }
    }

    /**
     * 305：解析"不带 opcode 的应答"之前，先把混在应答前面的控制帧吃掉。
     *
     * <p>典型场景：电脑刚在收尾/重建（它的事件循环可能卡在 connect 上几秒），
     * 它对我们早先那帧 PAUSE_STATE 的 PAUSE_REPLY 会晚到 —— 正好落在"发了 LIST_FILES、
     * 还没读到应答"之间。应答本身不带 opcode，直接 readInt 就把 PAUSE_REPLY 的两个字节
     * 当成数量，帧流一错位，界面线程就卡在 readUTF 上、还握着 controlLock。
     *
     * <p>做法：非阻塞试探两个字节，是已知帧头就吃掉（该读载荷的读掉），不是就退回
     * {@link DataByteChannel} 的退字缓冲，交给调用方按应答解析。
     *
     * <p>调用方必须已经持有 {@link #controlLock}。
     */
    protected void skipStaleFramesBeforeReply() {
        if (ctChannel == null) {
            return;
        }
        try {
            while (true) {
                int have = ctChannel.pollPushback(2, 150);
                if (have < 2) {
                    // 没有帧头，或者只有 1 个字节（DELETE_FILE / MKDIR 的 bool 应答）
                    return;
                }
                int hi = ctChannel.peekPushback(0) & 0xFF;
                int lo = ctChannel.peekPushback(1) & 0xFF;
                short opcode = (short) ((hi << 8) | lo);
                switch (opcode) {
                    case ControllerIdentifiers.PAUSE_REPLY:
                        ctChannel.consumePushback(2);
                        applyPcPause(ctChannel.readBoolean());
                        break;
                    case ControllerIdentifiers.SPEED_LIMIT:
                        // 308：整帧 6 字节（opcode 2 + i32 4）。这里和 PAUSE_REPLY /
                        // REBUILD_REQ 一样，先吃掉帧头再读载荷 —— 读载荷走 DataByteChannel
                        // 的读锁，退字缓冲里剩下的字节会先被喂出来，不会把帧读串。
                        ctChannel.consumePushback(2);
                        onPeerSpeedLimit(ctChannel.readInt());
                        break;
                    case ControllerIdentifiers.PING:
                        ctChannel.consumePushback(2);
                        break;
                    case ControllerIdentifiers.RESULT:
                        ctChannel.consumePushback(2);
                        discardResultPayload(ctChannel);
                        break;
                    case ControllerIdentifiers.REBUILD_REQ:
                        ctChannel.consumePushback(2);
                        stashRebuildName(ctChannel.readUTF());
                        peerCancelled = true;
                        needsRebuild = true;
                        break;
                    case ControllerIdentifiers.CANCEL:
                        ctChannel.consumePushback(2);
                        peerCancelled = true;
                        transferCancelled = true;
                        needsRebuild = true;
                        break;
                    case ControllerIdentifiers.CANCEL_DISCARD:
                        // 同 CANCEL，只多一条「这轮收到的半截要删」（提示词见 discardOnCancel）
                        ctChannel.consumePushback(2);
                        discardOnCancel = true;
                        peerCancelled = true;
                        transferCancelled = true;
                        needsRebuild = true;
                        break;
                    default:
                        // 这就是应答的开头：留着让调用方读
                        return;
                }
            }
        } catch (Throwable e) {
            // 探不动就按原路径继续（下面的读会带着超时报错）
        }
    }

    /**
     * 304：读掉一帧 RESULT 的载荷（这次传输已经作废，只求帧流不错位）。
     *
     * <p>格式与 {@link #writeResultFrame} 一致：
     * {@code [i16 RESULT][bool ok][i32 n][(i64 size, 32B sha256) * n][UTF 错误信息?]}。
     *
     * <p>用它的场合都是"结论帧已经没人要了"：电脑端的收尾结论可能在我们按取消的
     * 前一刻就写了出来。只丢帧头不读载荷，载荷会和下一个帧头拼成假 opcode。
     */
    protected void discardResultPayload(DataByteChannel channel) throws IOException {
        boolean ok = channel.readBoolean();
        int count = channel.readInt();
        if (count < 0) {
            count = 0;
        }
        byte[] sha256 = new byte[32];
        for (int i = 0; i < count; i++) {
            channel.readLong();
            channel.readFully(sha256);
        }
        if (!ok) {
            channel.readUTF();
        }
        top.weixiansen574.hybridfilexfer.core.HFXLog.d("discarded stale RESULT frame");
    }

    /** 305：按续传位图算出"接收端已经有的字节数"（进度基线） */
    private long resumeSkippedBytes(List<RemoteFile> fileList, Directory localDir, Directory remoteDir,
                                    java.util.Map<String, java.util.BitSet> resumeBits) {
        if (resumeBits == null || resumeBits.isEmpty()) {
            return 0;
        }
        long skipped = 0;
        for (RemoteFile file : fileList) {
            if (file.isDirectory()) {
                continue;
            }
            java.util.BitSet set = resumeBits.get(localDir.generateTransferPath(file.getPath(), remoteDir));
            if (set == null) {
                continue;
            }
            long size = file.getSize();
            for (int block = set.nextSetBit(0); block >= 0; block = set.nextSetBit(block + 1)) {
                long start = (long) block * FileBlock.BLOCK_SIZE;
                if (start >= size) {
                    break;
                }
                skipped += Math.min(FileBlock.BLOCK_SIZE, size - start);
            }
        }
        return skipped;
    }

    /**
     * 304：等这次传输的线程真正退出（有界等待）。
     *
     * <p>读文件线程和发块线程共用这条连接缓冲池里的 {@link java.nio.ByteBuffer}。
     * 取消 / 出错后不等它们退出就返回，下一次传输就会和它们抢同一个缓冲区
     * （实测 {@code IllegalArgumentException: newPosition > limit}，一次取消把后续
     * 传输全废掉）。正常情况几毫秒就退完；退不完也不能把界面卡死，所以给上限。
     */
    private void awaitTransferThreads(Thread readThread, FutureTask<Void> readFileTask,
                                     List<FutureTask<Void>> transferTasks, long timeoutMs) {
        if (readThread != null) {
            // 读线程可能正阻塞在 buffers.take() 上（池子空了）——打断它让它自己退出
            try {
                readThread.interrupt();
            } catch (Throwable ignored) {
            }
        }
        long deadline = System.currentTimeMillis() + Math.max(0, timeoutMs);
        if (transferTasks != null) {
            for (FutureTask<Void> task : transferTasks) {
                long left = deadline - System.currentTimeMillis();
                if (left <= 0) {
                    return;
                }
                try {
                    task.get(left, TimeUnit.MILLISECONDS);
                } catch (Throwable ignored) {
                }
            }
        }
        long left = deadline - System.currentTimeMillis();
        if (readFileTask != null && left > 0) {
            try {
                readFileTask.get(left, TimeUnit.MILLISECONDS);
            } catch (Throwable ignored) {
            }
        }
    }

    /**
     * 302：读一帧结论，返回 null 表示对端成功，否则是对端的错误信息。
     *
     * <p>读的过程中遇到 PAUSE_REPLY（电脑回答会话线程刚才那条 PAUSE_STATE）就地应用，
     * 这样收尾时还在飞的暂停同步不会把结论读串。
     */
    protected PeerResult readResultFrameOrNull() throws IOException {
        while (true) {
            // 304：帧头可能是别的读循环提前读到的（取消/重建期间帧序不固定），优先消费它
            short opcode = pendingOpcode >= 0 ? pendingOpcode : pollControlOpcode();
            pendingOpcode = -1;
            // 本端已经取消、且对端一个字节都没发来：不能再死等结论帧。
            // 电脑端这时可能还停在"暂停"里没读到我们的 CANCEL（它暂停时只 peek 传输通道），
            // 死等的话控制通道一直被这个线程占着，会话线程进不了重建，最后只能整条断掉。
            if (opcode < 0) {
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("readResultFrame: local cancel, stop waiting");
                return PeerResult.cancelled();
            }
            if (opcode == ControllerIdentifiers.PAUSE_REPLY) {
                applyPcPause(ctChannel.readBoolean());
                continue;
            }
            // 308：收尾期间电脑也可能改限速（它会推一帧过来）。就地采纳、继续等结论 ——
            // 不当成"意外的帧"抛出去，否则一次擦肩而过就把会话打掉了。
            if (opcode == ControllerIdentifiers.SPEED_LIMIT) {
                onPeerSpeedLimit(ctChannel.readInt());
                continue;
            }
            // 304：电脑端点「中断发送」= 先来一帧 CANCEL。
            // 这次传输就地作废：置取消标志（界面显示"已取消传输"），并且等它重建通道。
            if (opcode == ControllerIdentifiers.CANCEL) {
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("readResultFrame: peer CANCEL");
                // 3.0.0：除了置标志，还要**立刻关掉传输通道 + 放开暂停闸门** ——
                // 发块线程正阻塞在 write 里（电脑端不读了），只置标志它停不下来，
                // 用户看到的就是"电脑端已经取消了、手机还在继续发"。
                cancelByPeer("readResultFrame: peer CANCEL");
                return PeerResult.cancelled();
            }
            // 同上，另外要求收件方（本端）把已经收到的那半截也删掉。
            // 接收方向的收尾交换里也可能撞上它（电脑端在最后一块之后才发现校验不过、
            // 或者用户正好在这一刻点了「取消 → 不保留」），当成 CANCEL 处理，
            // 否则整条控制通道会因为 "unexpected control frame 31" 报错收场。
            if (opcode == ControllerIdentifiers.CANCEL_DISCARD) {
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("readResultFrame: peer CANCEL_DISCARD");
                cancelByPeerDiscard("readResultFrame: peer CANCEL_DISCARD");
                return PeerResult.cancelled();
            }
            // 304：电脑已经开始重建了（理论上 CANCEL 在它前面，这里只是兜底）
            if (opcode == ControllerIdentifiers.REBUILD_REQ) {
                String name = ctChannel.readUTF();
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("readResultFrame: REBUILD_REQ " + name);
                stashRebuildName(name);
                cancelByPeer("readResultFrame: REBUILD_REQ");
                return PeerResult.cancelled();
            }
            if (opcode == ControllerIdentifiers.RESULT) {
                boolean ok = ctChannel.readBoolean();
                int count = ctChannel.readInt();
                List<FileDigest> digests = new ArrayList<>(Math.max(0, Math.min(count, 4096)));
                for (int i = 0; i < count; i++) {
                    long size = ctChannel.readLong();
                    byte[] sha256 = new byte[32];
                    ctChannel.readFully(sha256);
                    digests.add(new FileDigest(size, sha256));
                }
                return new PeerResult(ok, ok ? null : ctChannel.readUTF(), digests);
            }
            if (opcode == ControllerIdentifiers.PING) {
                continue;
            }
            top.weixiansen574.hybridfilexfer.core.HFXLog.d(
                    "readResultFrame: BAD opcode=" + opcode);
            throw new IOException("unexpected control frame while waiting for RESULT: " + opcode);
        }
    }

    // ------------------------------------------------------------------
    // 304：取消 / 重建
    // ------------------------------------------------------------------

    /**
     * 收尾时看一眼控制通道有没有 CANCEL（电脑端点「中断发送」）。
     *
     * <p>接收方向传输期间控制通道的读取权在电脑那边（它 poll 暂停同步），
     * 手机这边只写不读；所以电脑点了「中断发送」之后，这一帧没人接。
     * 传输线程收尾时在这里探一下：有 CANCEL 就当"取消"收尾，别报成断线。
     *
     * <p>用 {@link DataByteChannel#available()} 先确认帧头到齐了再读，
     * 绝不把半个帧头吃掉（吃掉就会和下一个帧头拼成假 opcode）。
     *
     * @return true 表示对端取消了这次传输
     */
    protected boolean probePeerCancel(long timeoutMs) {
        DataByteChannel channel = ctChannel;
        if (channel == null) {
            return false;
        }
        long deadline = System.currentTimeMillis() + Math.max(0, timeoutMs);
        while (true) {
            long left = deadline - System.currentTimeMillis();
            if (left <= 0) {
                return false;
            }
            try {
                // 用直接读 SocketChannel 的方式等帧头：Socket 的 available() 在混用
                // SocketChannel 时会一直返回 0（真机实测），拿它当"有没有数据"会永远等不到。
                short opcode = readControlHeaderPolled(left, false);
                if (opcode == CONTROL_TIMEOUT) {
                    return false;
                }
                // 帧头 + 载荷必须在同一把锁里读完：只读帧头就松手，载荷会留到
                // 下一个读循环，和下一帧的帧头拼成假 opcode（实测读出 0 / 28513）。
                synchronized (controlLock) {
                    switch (opcode) {
                        case ControllerIdentifiers.CANCEL:
                            cancelByPeer("probePeerCancel: CANCEL");
                            return true;
                        // 接收方向电脑当发件方、用户选「不保留」时发的就是这一帧：
                        // 取消的语义和 CANCEL 完全一样，只多记一个「本轮收到的半截要删」，
                        // 删的动作在 receiveFiles 的收尾里做（见 settleCancelledReceive）。
                        // 两帧都不带载荷，帧流不会因此错位。
                        case ControllerIdentifiers.CANCEL_DISCARD:
                            cancelByPeerDiscard("probePeerCancel: CANCEL_DISCARD");
                            return true;
                        case ControllerIdentifiers.REBUILD_REQ:
                            // 电脑已经开始重建传输通道：这次传输同样作废
                            stashRebuildName(channel.readUTF());
                            cancelByPeer("probePeerCancel: REBUILD_REQ");
                            return true;
                        case ControllerIdentifiers.PAUSE_REPLY:
                            applyPcPause(channel.readBoolean());
                            return false;
                        case ControllerIdentifiers.SPEED_LIMIT:
                            // 308：电脑推来的限速值。整帧 6 字节，必须在这里用同一把锁
                            // 把 i32 读完（只读帧头会让载荷和下一帧拼成假 opcode）。
                            onPeerSpeedLimit(channel.readInt());
                            return false;
                        case ControllerIdentifiers.RESULT:
                            // 电脑端的收尾结论：这次传输已经作废，读干净丢掉
                            discardResultPayload(channel);
                            return false;
                        case ControllerIdentifiers.PING:
                            return false;
                        default:
                            // 不认识的帧：帧头不能丢，记下来交给后续的读循环
                            pendingOpcode = opcode;
                            return false;
                    }
                }
            } catch (Throwable e) {
                // 通道断了 / 被打断：都当"没探到取消"，交给上层按原路径收尾
                return false;
            }
        }
    }

    protected abstract WriteFileCall createWriteFileCall(LinkedBlockingDeque<ByteBuffer> buffers, int dequeCount, java.util.Map<String, java.util.BitSet> resumeBits);

    protected abstract ReadFileCall createReadFileCall(LinkedBlockingDeque<ByteBuffer> buffers, List<RemoteFile> files, Directory localDir, Directory remoteDir, int operateThreadCount, String[] channelNames, java.util.Map<String, java.util.BitSet> resumeBits);

}