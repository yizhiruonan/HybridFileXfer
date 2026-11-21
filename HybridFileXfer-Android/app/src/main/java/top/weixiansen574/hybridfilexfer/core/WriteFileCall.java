package top.weixiansen574.hybridfilexfer.core;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.util.*;
import java.util.concurrent.Callable;
import java.util.concurrent.LinkedBlockingDeque;

import top.weixiansen574.hybridfilexfer.core.callback.TransferFileCallback;

public abstract class WriteFileCall implements Callable<Void> {
    private final LinkedBlockingDeque<ByteBuffer> buffers;
    private final boolean[] channelFinished;
    private final ArrayList<LinkedList<FileBlock>> dequeArray;
    /**
     * 303：本次落盘的文件（手机本地绝对路径）。
     *
     * <p>同步包装：写盘线程逐个 add，收尾线程要整表复制（「不保留」的删除路径会在写盘
     * 线程还没退干净时就来取）。
     */
    private final java.util.Set<String> writtenPaths =
            java.util.Collections.synchronizedSet(new java.util.LinkedHashSet<>());
    private boolean canceled = false;

    /** 306：目标路径 → 接收端已有的块（续传时电脑只补缺的块） */
    private final java.util.Map<String, java.util.BitSet> resumeBits;
    /** 每个落盘文件的续传日志（收尾时按"传完删日志 / 没传完留日志"处理） */
    private final java.util.HashMap<String, ResumeJournal> journals = new java.util.HashMap<>();
    /** 收尾要复算 SHA-256 的文件：本次落盘的 + 续传时"已经全在"的 */
    private final java.util.LinkedHashSet<String> digestPaths = new java.util.LinkedHashSet<>();

    public WriteFileCall(LinkedBlockingDeque<ByteBuffer> buffers, int dequeCount) {
        this(buffers, dequeCount, null);
    }

    public WriteFileCall(LinkedBlockingDeque<ByteBuffer> buffers, int dequeCount,
                         java.util.Map<String, java.util.BitSet> resumeBits) {
        this.buffers = buffers;
        this.resumeBits = resumeBits;
        dequeArray = new ArrayList<>(dequeCount);
        channelFinished = new boolean[dequeCount];  // 初始化通道结束状态
        for (int i = 0; i < dequeCount; i++) {
            dequeArray.add(new LinkedList<>());
        }
        if (resumeBits != null) {
            // 已经在手机上的文件（这次一块都不用收）也要参与收尾的 SHA-256 比对
            digestPaths.addAll(resumeBits.keySet());
        }
    }

    @Override
    public Void call() throws Exception {
        try {
            FileBlock block = takeBlock();
            FileBlock lastBlock = null;
            /*File lastFile = null;
            RandomAccessFile lastRaf = null;*/
            FileChannel lastChannel = null;
            long cursor = 0;

            while (block != null) {
                if (block.isDirectory()) {
                    //File file = new File(block.path);
                    String file = block.path;
                    tryMkdirs(file);
                    setLastModified(file, block.lastModified);
                    block = takeBlock();
                    continue;
                }
                //创建文件的父目录，如果不存在，保证后续文件能够创建
                createParentDirIfNotExists(block.path);
                //RandomAccessFile raf;
                FileChannel channel;
                //如果上个文件与当前
                if (lastBlock == null || !lastBlock.path.equals(block.path)) {
                    if (lastChannel != null) {
                        closeFile();
                        setLastModified(lastBlock.path, lastBlock.lastModified);
                        closeJournal(lastBlock.path);
                    }
                    /*raf = new RandomAccessFile(file, "rw");
                    raf.setLength(block.totalSize);
                    channel = raf.getChannel();*/
                    channel = createAndOpenFile(block.path, block.totalSize);
                    // 303：记下落盘的文件（收尾复算 SHA-256 用）
                    writtenPaths.add(block.path);
                    digestPaths.add(block.path);
                    // 306：能续传就接着用旧日志（同路径、同大小、同修改时间），否则新建
                    ResumeJournal journal = ResumeJournal.load(block.path, block.totalSize,
                            block.lastModified, FileBlock.BLOCK_SIZE);
                    if (journal == null) {
                        journal = ResumeJournal.create(block.totalSize, block.lastModified, FileBlock.BLOCK_SIZE);
                    }
                    journals.put(block.path, journal);
                    cursor = 0;
                } else {
                    //raf = lastRaf;
                    channel = lastChannel;
                }
                //如果上个指针与当前指针不不一致就进行seek操作
                if (cursor != block.getStartPosition()) {
                    cursor = block.getStartPosition();
                    channel.position(cursor);
                }
                 /*   logSeek(block);
                } else {
                    logBlock(block);
                }*/

                ByteBuffer data = block.data;
                data.flip();
                // 一整个块要写满：SocketChannel 的风格是"能写多少写多少"，
                // 单次 write 少写了就等于文件里少了这一段（校验和会报错，但何必让它错）
                while (data.hasRemaining()) {
                    channel.write(data);
                }
                // 306：这一块落盘了，记进续传日志
                ResumeJournal journal = journals.get(block.path);
                if (journal != null) {
                    journal.mark(block.index);
                }
                cursor += block.getLength();
                //回收缓冲区块
                buffers.add(block.data);
                lastBlock = block;
                /*lastFile = file;
                lastRaf = raf;*/
                lastChannel = channel;
                block = takeBlock();
            }
            if (lastBlock != null) {
                closeFile();
                setLastModified(lastBlock.path, lastBlock.lastModified);
                closeJournal(lastBlock.path);
            }
        } catch (IOException e){
            cancel();
            throw e;
        }
        return null;
    }

    /** 一个文件写完了：日志里所有块都齐了就把 sidecar 删掉 */
    private void closeJournal(String path) {
        ResumeJournal journal = journals.get(path);
        if (journal == null) {
            return;
        }
        if (journal.allDone()) {
            ResumeJournal.remove(path);
            journals.remove(path);
        }
    }

    /**
     * 306：把"已经收到哪些块"的日志落盘（取消 / 通道出错时调用）。
     *
     * <p>写盘线程正常退出时不会走 {@link #cancel()}，而取消时它往往正好是"所有通道结束 + 队列空"
     * 的正常退出 —— 不显式保存的话，半截文件的位图就丢了，下次只能整份重传。
     * 只保存没传完的；已经传完的日志由 {@link #finish()} 删除。
     */
    public synchronized void saveJournals() {
        for (java.util.Map.Entry<String, ResumeJournal> entry : journals.entrySet()) {
            if (!entry.getValue().allDone()) {
                entry.getValue().save(entry.getKey());
            }
        }
    }

    /**
     * 对端取消时明确要求「已收到的那半截也别留」（{@code CANCEL_DISCARD}）的收尾：
     * 摘掉本轮的续传日志，并把"本轮创建过的文件"交给调用方删。
     *
     * <p>为什么日志要在这里删而不是留给 {@link #finish()} / {@link #cancel()}：这两个方法
     * 的语义都是"没传完就把位图存下来，下次续传"，正好和「不保留」相反。
     *
     * <p>为什么只返回路径、不自己删文件：删文件的口径（只删存在的、失败只打日志）在
     * {@code HFXService.removeIncompleteFiles} 里已经有一份，这里不复制第二份。
     * 顺序也是先日志后文件——反过来的话，文件没了日志还在，下次同名文件会被当成
     * "这些块已经有了"。
     *
     * <p>只认 {@link #writtenPaths}（真正创建过的文件）：本轮连文件都没建、只建了日志的
     * 情况极少，不为它加额外的状态。
     */
    public synchronized List<String> discardJournalsAndListWritten() {
        for (String path : journals.keySet()) {
            ResumeJournal.remove(path);
        }
        journals.clear();
        return writtenPaths();
    }

    /**
     * 收尾：成功传完的文件删日志；没传完的（理论上不该走到这里）留着下次续传。
     */
    public synchronized void finish() {
        for (java.util.Map.Entry<String, ResumeJournal> entry : journals.entrySet()) {
            if (entry.getValue().allDone()) {
                ResumeJournal.remove(entry.getKey());
            } else {
                entry.getValue().save(entry.getKey());
            }
        }
        journals.clear();
    }

    private void logSeek(FileBlock block) {
        System.out.printf("seek: %d %s %d %d %d%n",
                block.getStartPosition(), block.path, block.totalSize, block.index, block.getLength());
    }

    private void logBlock(FileBlock block) {
        System.out.printf("%s %d %d %d%n",
                block.path, block.totalSize, block.index, block.getLength());
    }

    public ByteBuffer getBuffer() throws InterruptedException {
        return buffers.take();
    }

    // 新增方法：标记通道结束
    public synchronized void finishChannel(int tIndex) {
        channelFinished[tIndex] = true;
        notify();  // 唤醒可能阻塞的写线程
    }

    public synchronized void cancel(){
        canceled = true;
        // 306：取消/出错时把"已经收到哪些块"存下来，半截文件留着下次续传
        for (java.util.Map.Entry<String, ResumeJournal> entry : journals.entrySet()) {
            if (!entry.getValue().allDone()) {
                entry.getValue().save(entry.getKey());
            }
        }
        //回收未写入硬盘的块的ByteBuffer
        for (LinkedList<FileBlock> deque : dequeArray) {
            for (FileBlock fileBlock : deque) {
                if (fileBlock.data != null){
                    buffers.add(fileBlock.data);
                }
            }
        }
        notify();
    }

    // 修改后的putBlock（保持原有逻辑）
    public synchronized void putBlock(FileBlock block, int tIndex) {
        //System.out.println("put:"+block.index+" "+tIndex);
        dequeArray.get(tIndex).add(block);
        notify();  // 唤醒可能阻塞的写线程
    }


    // 重构后的takeBlock（实现阻塞等待）
    private synchronized FileBlock takeBlock() throws InterruptedException {
        while (true) {
            FileBlock block = tryTakeBlockInternal();

            if (block != null) return block;

            //检查终止条件：所有通道结束 + 所有队列为空
            if (canceled || (allChannelsFinished() && allQueuesEmpty())) {
                return null;
            }

            wait();  // 阻塞等待直到被唤醒
        }
    }

    public synchronized FileBlock tryTakeBlockInternal() {
        FileBlock minHead = null;
        int mdqIndex = -1;

        for (int i = 0; i < dequeArray.size(); i++) {
            LinkedList<FileBlock> deque = dequeArray.get(i);
            if (!deque.isEmpty()) {
                FileBlock head = deque.getFirst();
                if (minHead == null || head.compareTo(minHead) < 0) {
                    minHead = head;
                    mdqIndex = i;
                }
            }
        }
        if (minHead != null) {
            dequeArray.get(mdqIndex).removeFirst();
        }
        return minHead;
    }

    private boolean allChannelsFinished() {
        for (boolean finished : channelFinished) {
            if (!finished) return false;
        }
        return true;
    }

    // 辅助方法：检查所有队列是否为空
    private boolean allQueuesEmpty() {
        for (LinkedList<FileBlock> deque : dequeArray) {
            if (!deque.isEmpty()) return false;
        }
        return true;
    }

    private void setLastModified(String file, long time) throws Exception {
        if (!setFileLastModified(file,time)) {
            System.out.println("Warning! file cannot set last modified:" + file);
        }
    }

    /** 303：本次落盘的文件（手机本地绝对路径） */
    public List<String> writtenPaths() {
        return new ArrayList<>(writtenPaths);
    }

    /** 306：收尾复算 SHA-256 的文件（本次落盘的 + 续传时已经全在的） */
    public List<String> digestPaths() {
        return new ArrayList<>(digestPaths);
    }

    protected abstract void createParentDirIfNotExists(String path) throws Exception;
    protected abstract void tryMkdirs(String path) throws Exception;
    protected abstract FileChannel createAndOpenFile(String path,long length) throws Exception;
    protected abstract void closeFile() throws Exception;
    protected abstract boolean setFileLastModified(String path,long time) throws Exception;
}
