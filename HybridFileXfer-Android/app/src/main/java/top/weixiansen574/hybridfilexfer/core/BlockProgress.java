package top.weixiansen574.hybridfilexfer.core;

import java.util.ArrayList;
import java.util.BitSet;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * 传输进度：按"谁负责哪一段"来记，给进度浮层画 IDM 那种动态分块进度条。
 *
 * <p>IDM 的进度条不是固定段数：文件按连接切成若干段，一条连接一段；
 * 某条连接传完自己的段后，它会去把别人还没传完的段再切一半过来。
 * 所以段数会随传输过程变化（一开始 = 连接数，之后可能变多）。
 *
 * <p><b>发送方向和接收方向共用这一个类</b>（HFXService.sendProgress / blockProgress）：
 * 只要记下"哪个通道传了第几块"，相邻块归属变化的地方就是一段的边界（{@link #segments()}），
 * 每段的填充 = 这一段里已完成的块 / 总块数。<b>不要</b>再用"通道累计字节 ÷ 段的份额"去猜
 * （通道累计字节是跨段的，一条通道抢了别人的段以后，它自己的累计值早就超过新段的份额，
 * 画出来整段瞬间填满、还会和主进度条对不上）。
 *
 * <p>电脑端按连续区间调度时，这里得到的就是 IDM 的段；旧电脑端按共享队列
 * 轮询发送时，几乎每块都会换通道，段数会爆炸——{@link #segments()}
 * 会检测到并退回成整条进度。
 *
 * <p>线程安全：传输线程写、UI 线程读。界面高频轮询请用 {@link #snapshot()}（一次性真值）。
 */
public class BlockProgress {
    public static final int MAX_BLOCKS = 1 << 20;
    private static final int MAX_SEGMENTS = 24;
    /** 归属区间的硬上限：超过就说明是「每块都换通道」的旧调度，画碎条没意义 */
    private static final int MAX_RUNS = 512;

    /** 进度条上的一段：一个连接负责的文件区间 */
    public static final class Segment {
        /** 负责这条连接的名字（旧电脑端退回整条时为空串） */
        public final String owner;
        public final int start;
        public final int end;
        /** 这一段已完成比例 0~1 */
        public final float filled;

        Segment(String owner, int start, int end, float filled) {
            this.owner = owner;
            this.start = start;
            this.end = end;
            this.filled = filled;
        }
    }

    private final Object lock = new Object();
    private String path = "";
    private int totalBlocks;
    private long totalSize;
    private int[] owners = new int[0];
    private final BitSet done = new BitSet();
    private final List<String> names = new ArrayList<>();
    private final Map<String, Integer> nameIds = new HashMap<>();

    public void reset() {
        synchronized (lock) {
            path = "";
            totalBlocks = 0;
            totalSize = 0;
            owners = new int[0];
            done.clear();
        }
    }

    /**
     * 收到某一块的文件头（还没收完数据也算，进度条会稍微提前一点点）。
     *
     * @param owner  哪个传输通道送来的
     * @param file   文件路径（换文件时自动重来）
     * @param index  第几块
     * @param size   文件总大小
     */
    public void onBlock(String owner, String file, int index, long size) {
        if (file == null || index < 0 || index >= MAX_BLOCKS) {
            return;
        }
        synchronized (lock) {
            if (!file.equals(path)) {
                path = file;
                done.clear();
                names.clear();
                nameIds.clear();
                owners = new int[0];
                totalBlocks = 0;
            }
            if (size > 0) {
                totalSize = size;
                int blocks = blocksOf(size);
                if (blocks > totalBlocks || owners.length < blocks) {
                    int[] next = new int[Math.max(blocks, index + 1)];
                    java.util.Arrays.fill(next, -1);
                    System.arraycopy(owners, 0, next, 0, Math.min(owners.length, next.length));
                    owners = next;
                    totalBlocks = Math.max(totalBlocks, blocks);
                }
            }
            if (index >= owners.length) {
                int[] next = new int[index + 1];
                java.util.Arrays.fill(next, -1);
                System.arraycopy(owners, 0, next, 0, owners.length);
                owners = next;
            }
            owners[index] = ownerId(owner);
            done.set(index);
            if (index + 1 > totalBlocks) {
                totalBlocks = index + 1;
            }
        }
    }

    private int ownerId(String owner) {
        if (owner == null) {
            owner = "";
        }
        Integer id = nameIds.get(owner);
        if (id == null) {
            id = names.size();
            names.add(owner);
            nameIds.put(owner, id);
        }
        return id;
    }

    /**
     * 一次取走画进度条要的全部真值（分段 + 已完成字节 + 总大小）。
     *
     * <p>界面每 50~60ms 调一次，必须"同一瞬间"的数据：分三次读（segments / doneBytes / totalSize）
     * 会有撕裂——比如拿到 800 块的分段，却配上 801 块的 done，进度条和数字就对不上。
     */
    public static final class Snapshot {
        public final List<Segment> segments;
        /** 已完成字节（按块算，末块可能略多，已按文件大小截断） */
        public final long doneBytes;
        public final long totalSize;
        public final int doneBlocks;
        public final int totalBlocks;

        Snapshot(List<Segment> segments, long doneBytes, long totalSize, int doneBlocks, int totalBlocks) {
            this.segments = segments;
            this.doneBytes = doneBytes;
            this.totalSize = totalSize;
            this.doneBlocks = doneBlocks;
            this.totalBlocks = totalBlocks;
        }
    }

    /** 当前进度的一次性快照（线程安全） */
    public Snapshot snapshot() {
        synchronized (lock) {
            return new Snapshot(segmentsLocked(), doneBytesLocked(), totalSize, done.cardinality(),
                    Math.max(totalBlocks, done.length()));
        }
    }

    /**
     * 当前的分段：相邻块归属一变就是一段。
     *
     * <p>两条必须守住的：
     * <ul>
     *   <li>只有<b>相邻两段都填满</b>才拼成一条 —— 左边满了就把右边正在传的块吞进去，
     *       会出现「还剩几条通道在传，切分线却全没了」；</li>
     *   <li>段数超上限时合并<b>最短</b>的那段，不是整条并成一根。</li>
     * </ul>
     */
    public List<Segment> segments() {
        synchronized (lock) {
            return segmentsLocked();
        }
    }

    /** 必须在 lock 里调用 */
    private List<Segment> segmentsLocked() {
        int total = Math.max(totalBlocks, done.length());
        if (total <= 0) {
            return new ArrayList<>();
        }
        // ① 整条块空间按属主切区间：块 i 的属主一变就是一条边界。
        //    以前只扫「已完成」的块，最后一格一路延伸到文件末尾，正在传的那几块没有自己的段；
        //    现在未分配/正在传的块也参与划分（owner = -1），它们各自成格，画出来就是还没填的那截。
        List<int[]> runs = new ArrayList<>(); // [start, end, ownerId]
        int previous = -2;
        int runStart = 0;
        for (int i = 0; i < total; i++) {
            int owner = i < owners.length ? owners[i] : -1;
            if (previous == -2) {
                previous = owner;
                runStart = 0;
            } else if (owner != previous) {
                runs.add(new int[]{runStart, i, previous});
                runStart = i;
                previous = owner;
            }
        }
        runs.add(new int[]{runStart, total, previous});
        if (runs.size() > MAX_RUNS) {
            // 旧电脑端按块轮询、每块都换通道：画出来全是碎条，退回整条
            List<Segment> single = new ArrayList<>(1);
            single.add(new Segment("", 0, total, (float) done.cardinality() / total));
            return single;
        }

        // ② 相邻两段**都填满**了才拼成一条（IDM 的「传完就拼成整条」）。
        //    以前只要左边那段满了就并，右边还在传的块被吞进去，于是「还剩 4 条通道在传，
        //    切分线全没了、正在传的块也并到满的那一截里」——用户报的就是这个。
        List<int[]> merged = new ArrayList<>(); // [start, end, ownerId；-2 = 混合属主]
        int[] current = null;
        boolean currentFull = false;
        for (int[] run : runs) {
            boolean full = filledLocked(run[0], run[1]) >= (run[1] - run[0]);
            if (current == null) {
                current = new int[]{run[0], run[1], run[2]};
                currentFull = full;
            } else if (currentFull && full) {
                current[1] = run[1];
                if (current[2] != run[2]) {
                    current[2] = -2;
                }
            } else {
                merged.add(current);
                current = new int[]{run[0], run[1], run[2]};
                currentFull = full;
            }
        }
        if (current != null) {
            merged.add(current);
        }

        // ③ 段数超上限时合并**最短的那一段**，别像以前那样整条并成一根：
        //    末尾正在传的那几段很短，但正是用户要看的「切分」。
        while (merged.size() > MAX_SEGMENTS) {
            int shortest = 0;
            for (int i = 1; i < merged.size(); i++) {
                if (merged.get(i)[1] - merged.get(i)[0] <
                        merged.get(shortest)[1] - merged.get(shortest)[0]) {
                    shortest = i;
                }
            }
            int into = shortest == 0 ? 1 : shortest - 1;
            int[] target = merged.get(into);
            int[] gone = merged.get(shortest);
            target[0] = Math.min(target[0], gone[0]);
            target[1] = Math.max(target[1], gone[1]);
            if (target[2] != gone[2]) {
                target[2] = -2;
            }
            merged.remove(shortest);
        }

        // ④ 出结果：填充 = 段内已完成块 / 段总块（只从左边填，和主进度条同一套口径）
        List<Segment> out = new ArrayList<>(merged.size());
        for (int[] seg : merged) {
            int span = Math.max(1, seg[1] - seg[0]);
            int filled = filledLocked(seg[0], seg[1]);
            String ownerName = seg[2] >= 0 && seg[2] < names.size() ? names.get(seg[2]) : "";
            out.add(new Segment(ownerName, seg[0], seg[1], Math.min(1f, (float) filled / span)));
        }
        return out;
    }

    /** 必须在 lock 里调用：统计 [start, end) 里已完成的块数 */
    private int filledLocked(int start, int end) {
        int count = 0;
        for (int i = done.nextSetBit(start); i >= 0 && i < end; i = done.nextSetBit(i + 1)) {
            count++;
        }
        return count;
    }

    /** 已完成字节数（按块估，末块可能略多一点点） */
    public long getDoneBytes() {
        synchronized (lock) {
            return doneBytesLocked();
        }
    }

    /** 必须在 lock 里调用 */
    private long doneBytesLocked() {
        long bytes = (long) done.cardinality() * FileBlock.BLOCK_SIZE;
        return totalSize > 0 ? Math.min(bytes, totalSize) : bytes;
    }

    /** 当前跟踪的是哪个文件（没在传时是空串） */
    public String currentPath() {
        synchronized (lock) {
            return path;
        }
    }

    /** 已完成块数（调试用） */
    public int getDoneBlocks() {
        synchronized (lock) {
            return done.cardinality();
        }
    }

    private static int blocksOf(long size) {
        long blocks = (size + FileBlock.BLOCK_SIZE - 1) / FileBlock.BLOCK_SIZE;
        return (int) Math.min(MAX_BLOCKS, Math.max(1, blocks));
    }
}