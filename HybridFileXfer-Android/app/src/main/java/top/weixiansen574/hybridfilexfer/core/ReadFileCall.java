package top.weixiansen574.hybridfilexfer.core;

import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.LinkedBlockingDeque;

import top.weixiansen574.hybridfilexfer.core.bean.Directory;
import top.weixiansen574.hybridfilexfer.core.bean.RemoteFile;

public abstract class ReadFileCall implements Callable<Void> {
    public static final FileBlock END_POINT = new FileBlock(true, -1, "END_POINT", 0, 0, -1, null);
    public static final FileBlock INTERRUPT = new FileBlock(true, -1, "INTERRUPT", 0, 0, -1, null);
    public static final FileBlock READ_ERROR = new FileBlock(true, -1, "READ_ERROR", 0, 0, -1, null);
    public static final FileBlock WRITE_ERROR = new FileBlock(true, -1, "WRITE_ERROR", 0, 0, -1, null);

    private final LinkedBlockingDeque<FileBlock> deque = new LinkedBlockingDeque<>();
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

    public ReadFileCall(LinkedBlockingDeque<ByteBuffer> buffers, List<RemoteFile> files, Directory localDir, Directory remoteDir, int operateThreadCount) {
        this.buffers = buffers;
        this.files = files;
        this.localDir = localDir;
        this.remoteDir = remoteDir;
        this.operateThreadCount = operateThreadCount;
    }

    public void setPause(TransferPause pause) {
        this.pause = pause;
    }

    private TransferPause pause() {
        return pause;
    }

    /**
     * 这次传输已经被取消 / 通道出错：读文件的线程必须自己停下来。
     *
     * <p>以前只把队列里的块"回收"进缓冲池就完事，读线程照旧往下读文件、占着同一批
     * {@link ByteBuffer}。取消后再传一次时，两个读线程抢同一个缓冲区 —— 实测第二次
     * 传输直接 {@code IllegalArgumentException: newPosition > limit}（缓冲区被另一条
     * 线程 flip 过），一次取消把后续传输全废掉。
     */
    private volatile boolean shutdown;

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
            for (int i = 0; i < operateThreadCount; i++) {
                deque.add(END_POINT);
            }
        } catch (Exception e) {
            //当发生读取错误时
            for (int i = 0; i < operateThreadCount; i++) {
                deque.add(READ_ERROR);
            }
            throw e;
        }
        return null;
    }

    private void listFilesAndRead(RemoteFile folder) throws Exception {
        List<RemoteFile> files = listFiles(folder.getPath());
        if (files != null) {
            for (RemoteFile file : files) {
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
            deque.add(new FileBlock(false,
                    fileIndex, localDir.generateTransferPath(file.getPath(), remoteDir),
                    file.lastModified(), 0, 0, null));
            return;
        }
        //RandomAccessFile randomAccessFile = new RandomAccessFile(file, "r");
        // 303：记下这次真正发出去的文件（收尾复算 SHA-256 用）
        sourcePaths.add(file.getPath());
        FileChannel channel = openFile(file.getPath());
        long length = channel.size();
        long lastModified = file.lastModified();
        long remaining = length;
        if (length == 0){
            ByteBuffer buffer = buffers.take();
            buffer.clear();
            buffer.limit(0);
            deque.add(new FileBlock(true,
                    fileIndex, localDir.generateTransferPath(file.getPath(), remoteDir),
                    lastModified, length, 0, buffer));
            closeFile();
            return;
        }
        int i = 0;
        while (remaining > 0){
            if (shutdown) {
                closeFile();
                return;
            }
            TransferPause gate = pause();
            if (gate != null) {
                gate.awaitResume();
            }
            int blkSize = (int) Math.min(remaining,FileBlock.BLOCK_SIZE);
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
            deque.add(new FileBlock(true,
                    fileIndex, localDir.generateTransferPath(file.getPath(), remoteDir),
                    lastModified, length, i, buffer));
            remaining -= blkSize;
            i++;
        }
        closeFile();
    }

    /** 303：本次发送涉及的文件（手机本地绝对路径） */
    public List<String> sourcePaths() {
        return sourcePaths;
    }

    public void recycleBuffer(ByteBuffer buffer) {
        buffers.add(buffer);
    }

    public FileBlock takeBlock() throws InterruptedException {
        return deque.take();
    }

    //当对方写入时发生错误时
    public void shutdownByWriteError() {
        shutdown = true;
        recycleAllBuffer();
        for (int i = 0; i < operateThreadCount; i++) {
            deque.addFirst(WRITE_ERROR);
        }
    }

    //当其中任意一条通道断开时
    public void shutdownByConnectionBreak() {
        shutdown = true;
        recycleAllBuffer();
        for (int i = 0; i < operateThreadCount - 1; i++) {
            deque.addFirst(INTERRUPT);
        }
    }

    /**
     * 把队列里还没人领的块全部取出来、把缓冲区还回池子。
     *
     * <p>必须"取出来"而不是"遍历"：只遍历不摘除的话，这些块还留在队列里，
     * 另一条发块线程照样能 take 到它们 —— 可它们的缓冲区已经回到池子里、
     * 可能正被别的线程写入（cancel 后尤其明显），写出去的就是垃圾，
     * 甚至直接 {@code IllegalArgumentException: newPosition > limit}。
     */
    private void recycleAllBuffer() {
        FileBlock fileBlock;
        while ((fileBlock = deque.pollFirst()) != null) {
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
