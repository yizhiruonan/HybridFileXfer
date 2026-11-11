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
     */
    public static final int VERSION_CODE = 304;
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

    /** 发送方向当前的分段（进度条切割线）；没有传输时返回空表 */
    public java.util.List<SendRanges.Segment> currentSendSegments() {
        ReadFileCall call = currentReadFileCall;
        return call == null ? java.util.Collections.emptyList() : call.segments();
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

    protected void stashRebuildName(String name) {
        if (name != null) {
            pendingRebuildNames.add(name);
        }
    }

    /** 取一个提前读到的重建通道名；没有返回 null */
    public String pollRebuildName() {
        return pendingRebuildNames.poll();
    }

    protected boolean sendFiles(List<RemoteFile> fileList,Directory localDir, Directory remoteDir, TransferFileCallback callback) throws IOException {
        // 限速计时窗随传输重新开始，避免上次 setLimit 之后的空闲时间变成免费流量额度
        SpeedLimiter.resetWindow();
        pauseGate = new TransferPause();
        lastPcPaused = false;
        // 通道名按 connections 的顺序给"分段计划"用（进度条上的 owner 就是它）
        String[] channelNames = new String[connections.size()];
        for (int i = 0; i < connections.size(); i++) {
            channelNames[i] = connections.get(i).iName;
        }
        ReadFileCall readFileCall = createReadFileCall(buffers, fileList, localDir, remoteDir, connections.size(), channelNames);
        // 界面画"动态分段进度条"要读它：当前文件被切成了哪几段、每段归哪条通道
        this.currentReadFileCall = readFileCall;
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
            FutureTask<Void> task = new FutureTask<>(new SendFileCall(readFileCall, connection, callback, pauseGate, i));
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
            // 304：取消后不能立刻返回 —— 读文件线程（FileRead）和发块线程可能还在用
            // 这条连接共享的 native 缓冲区池。等它们退完，会话线程才会进重建；
            // 否则"取消完立刻再传"时两批线程抢同一个 ByteBuffer，实测下一次传输
            // 直接 IllegalArgumentException: newPosition > limit（缓冲区被二次回收/复用）。
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
            String verifyError = pcResult == null || pcResult.digests.isEmpty()
                    ? null
                    : compareDigests(digestOf(readFileCall.sourcePaths()), pcResult.digests);
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
        // 对称：接收方向的限速计时窗也随传输重置（PC 端若实现限速时同样受益）
        SpeedLimiter.resetWindow();
        pauseGate = new TransferPause();
        lastPcPaused = false;
        WriteFileCall writeFileCall = createWriteFileCall(buffers, connections.size());
        long startTime = System.currentTimeMillis();

        SpeedMonitorThread speedMonitorThread = new SpeedMonitorThread(connections, callback);
        speedMonitorThread.setName("SpeedMonitor");
        speedMonitorThread.start();

        List<FutureTask<Void>> transferTasks = new ArrayList<>(connections.size());
        for (int i = 0; i < connections.size(); i++) {
            TransferConnection connection = connections.get(i);
            FutureTask<Void> task = new FutureTask<>(new ReceiveFileCall(i, connection, writeFileCall, callback, pauseGate));
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
            // 写盘线程挂了：文件可能只写了一半，删掉（见 removeIncompleteFiles 注释）
            removeIncompleteFiles(writeFileCall.writtenPaths());
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
                    removeIncompleteFiles(writeFileCall.writtenPaths());
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
                // 落盘到一半的文件必须删掉才能走：createAndOpenFile() 一开始就把文件
                // 预设成最终长度，留着就是一个"大小正常、内容有洞"的假文件 ——
                // 用户看不出坏在哪，还会拿去转发/播放。和电脑端 finish_with(complete=false) 对齐。
                removeIncompleteFiles(writeFileCall.writtenPaths());
                callback.onIncomplete();
                return false;
            }
        }
        speedMonitorThread.cancel();
        // 303 收尾：先报"落盘结论 + 落盘文件的指纹"，再读电脑端的读文件结论
        List<FileDigest> digests = digestOf(writeFileCall.writtenPaths());
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

    /** 把一组本地文件算成指纹表（排序后两端可直接比对）；读不出来的文件跳过 */
    protected static List<FileDigest> digestOf(List<String> paths) {
        List<FileDigest> digests = new ArrayList<>();
        if (paths == null) {
            return digests;
        }
        byte[] buffer = new byte[1024 * 1024];
        for (String path : paths) {
            try {
                File file = new File(path);
                if (!file.isFile()) {
                    continue;
                }
                MessageDigest digest = MessageDigest.getInstance("SHA-256");
                try (FileInputStream in = new FileInputStream(file)) {
                    int read;
                    while ((read = in.read(buffer)) > 0) {
                        digest.update(buffer, 0, read);
                    }
                }
                digests.add(new FileDigest(file.length(), digest.digest()));
            } catch (Throwable e) {
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("digest failed: " + path, e);
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
                    case ControllerIdentifiers.PING:
                        break;
                    case ControllerIdentifiers.RESULT:
                        // 上一次传输的收尾结论：没人要了，读干净丢掉
                        discardResultPayload(ctChannel);
                        break;
                    case ControllerIdentifiers.REBUILD_REQ:
                        stashRebuildName(ctChannel.readUTF());
                        needsRebuild = true;
                        return;
                    case ControllerIdentifiers.CANCEL:
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
            // 304：电脑端点「中断发送」= 先来一帧 CANCEL。
            // 这次传输就地作废：置取消标志（界面显示"已取消传输"），并且等它重建通道。
            if (opcode == ControllerIdentifiers.CANCEL) {
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("readResultFrame: peer CANCEL");
                transferCancelled = true;
                needsRebuild = true;
                return PeerResult.cancelled();
            }
            // 304：电脑已经开始重建了（理论上 CANCEL 在它前面，这里只是兜底）
            if (opcode == ControllerIdentifiers.REBUILD_REQ) {
                String name = ctChannel.readUTF();
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("readResultFrame: REBUILD_REQ " + name);
                stashRebuildName(name);
                needsRebuild = true;
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
                            transferCancelled = true;
                            needsRebuild = true;
                            return true;
                        case ControllerIdentifiers.REBUILD_REQ:
                            // 电脑已经开始重建传输通道：这次传输同样作废
                            stashRebuildName(channel.readUTF());
                            transferCancelled = true;
                            needsRebuild = true;
                            return true;
                        case ControllerIdentifiers.PAUSE_REPLY:
                            applyPcPause(channel.readBoolean());
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

    protected abstract WriteFileCall createWriteFileCall(LinkedBlockingDeque<ByteBuffer> buffers, int dequeCount);

    protected abstract ReadFileCall createReadFileCall(LinkedBlockingDeque<ByteBuffer> buffers, List<RemoteFile> files, Directory localDir, Directory remoteDir, int operateThreadCount, String[] channelNames);

}