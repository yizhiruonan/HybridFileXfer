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
    /** 文件夹这类"谁发都行"的块 */
    private final LinkedBlockingDeque<FileBlock> specialQueue = new LinkedBlockingDeque<>();
    /** 当前文件的分段计划（界面画进度条读它） */
    private volatile SendRanges ranges;
    private final String[] channelNames;

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
        this.buffers = buffers;
        this.files = files;
        this.localDir = localDir;
        this.remoteDir = remoteDir;
        this.operateThreadCount = Math.max(1, operateThreadCount);
        this.channelNames = channelNames;
        this.queues = new LinkedBlockingDeque[this.operateThreadCount];
        for (int i = 0; i < this.operateThreadCount; i++) {
            this.queues[i] = new LinkedBlockingDeque<>();
        }
    }

    public void setPause(TransferPause pause) {
        this.pause = pause;
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
        // 303：记下这次真正发出去的文件（收尾复算 SHA-256 用）
        sourcePaths.add(file.getPath());
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
        long blocks = (length + FileBlock.BLOCK_SIZE - 1) / FileBlock.BLOCK_SIZE;
        SendRanges plan = new SendRanges(operateThreadCount, blocks, channelNames);
        ranges = plan;
        long remaining = length;
        int i = 0;
        while (remaining > 0) {
            if (shutdown) {
                closeFile();
                return;
            }
            TransferPause gate = pause();
            if (gate != null) {
                gate.awaitResume();
            }
            int blkSize = (int) Math.min(remaining, FileBlock.BLOCK_SIZE);
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
                channel.read(buffer);
            }
            FileBlock block = new FileBlock(true, fileIndex, transferPath,
                    lastModified, length, i, buffer);
            int owner = plan.ownerOf(i);
            plan.markProduced(i);
            queues[owner].add(block);
            remaining -= blkSize;
            i++;
        }
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

    /** 取一个"谁发都行"的块（文件夹 / 空文件）；没有就返回 null */
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
