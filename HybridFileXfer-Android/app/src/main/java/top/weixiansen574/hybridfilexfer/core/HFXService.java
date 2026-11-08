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
     */
    public static final int VERSION_CODE = 303;
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

    /** 本次传输是否被用户主动取消 */
    public boolean isTransferCancelled() {
        return transferCancelled;
    }

    protected boolean sendFiles(List<RemoteFile> fileList,Directory localDir, Directory remoteDir, TransferFileCallback callback) throws IOException {
        pauseGate = new TransferPause();
        lastPcPaused = false;
        ReadFileCall readFileCall = createReadFileCall(buffers, fileList, localDir, remoteDir, connections.size());
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
        for (TransferConnection connection : connections) {
            FutureTask<Void> task = new FutureTask<>(new SendFileCall(readFileCall, connection, callback, pauseGate));
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
            writeError = pcResult.ok ? null : pcResult.message;
        } catch (Throwable e) {
            // 用 Throwable：以前只 catch IOException，别的异常（比如缓冲区被并发写坏
            // 引起的 BufferUnderflowException）会从这里抛出去，日志只留下
            // 一句 "result=null"，看不出是异常退出还是真正的成功。
            speedMonitorThread.cancel();
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("sendFiles: readResultFrame failed", e);
            callback.onIncomplete();
            return false;
        } finally {
            sendingFiles = false;
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("sendFiles: reader=none (sendingFiles=false), result=" + writeError);
        }
        speedMonitorThread.cancel();

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

        PeerResult(boolean ok, String message, List<FileDigest> digests) {
            this.ok = ok;
            this.message = message;
            this.digests = digests;
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
    // 控制通道：单一专职读者
    // ------------------------------------------------------------------

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
     * 302：读一帧结论，返回 null 表示对端成功，否则是对端的错误信息。
     *
     * <p>读的过程中遇到 PAUSE_REPLY（电脑回答会话线程刚才那条 PAUSE_STATE）就地应用，
     * 这样收尾时还在飞的暂停同步不会把结论读串。
     */
    protected PeerResult readResultFrameOrNull() throws IOException {
        while (true) {
            short opcode = ctChannel.readShort();
            if (opcode == ControllerIdentifiers.PAUSE_REPLY) {
                applyPcPause(ctChannel.readBoolean());
                continue;
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

    protected abstract WriteFileCall createWriteFileCall(LinkedBlockingDeque<ByteBuffer> buffers, int dequeCount);

    protected abstract ReadFileCall createReadFileCall(LinkedBlockingDeque<ByteBuffer> buffers, List<RemoteFile> files, Directory localDir, Directory remoteDir, int operateThreadCount);

}