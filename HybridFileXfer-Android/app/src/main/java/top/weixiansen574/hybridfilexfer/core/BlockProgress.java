package top.weixiansen574.hybridfilexfer.core;

import java.util.ArrayList;
import java.util.BitSet;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * 接收进度：按"谁负责哪一段"来记，给进度浮层画 IDM 那种动态分块进度条。
 *
 * <p>IDM 的进度条不是固定段数：文件按连接切成若干段，一条连接一段；
 * 某条连接传完自己的段后，它会去把别人还没传完的段再切一半过来。
 * 所以段数会随传输过程变化（一开始 = 连接数，之后可能变多）。
 *
 * <p>手机是接收方，块由电脑分配；这里只需要记录"哪个通道送了第几块"，
 * 相邻块归属变化的地方就是一段的边界（{@link #segments()}）。
 * 电脑端按连续区间调度时，这里得到的就是 IDM 的段；旧电脑端按共享队列
 * 轮询发送时，几乎每块都会换通道，段数会爆炸——{@link #segments(int)}
 * 会检测到并退回成整条进度。
 *
 * <p>线程安全：接收线程写、UI 线程读。（块到达时记录，不区分是否已写盘）
 */
public class BlockProgress {
    public static final int MAX_BLOCKS = 1 << 20;
    private static final int MAX_SEGMENTS = 24;

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
     * 当前的分段：相邻块归属一变就是一段。
     * 段数超过上限（旧电脑端的轮询调度会让它爆炸）时退化成一条整进度。
     */
    public List<Segment> segments() {
        synchronized (lock) {
            int total = Math.max(totalBlocks, done.length());
            if (total <= 0) {
                return new ArrayList<>();
            }
            // 按索引扫描已完成块，记录归属发生变化的位置
            List<int[]> bounds = new ArrayList<>(); // [startBlock, ownerId]
            int previous = -2;
            int segmentStart = 0;
            for (int i = done.nextSetBit(0); i >= 0 && i < total; i = done.nextSetBit(i + 1)) {
                int owner = i < owners.length ? owners[i] : -1;
                if (owner < 0) {
                    continue;
                }
                if (previous == -2) {
                    previous = owner;
                    segmentStart = 0;
                } else if (owner != previous) {
                    bounds.add(new int[]{segmentStart, previous});
                    segmentStart = i;
                    previous = owner;
                }
            }
            if (previous == -2) {
                return new ArrayList<>();
            }
            bounds.add(new int[]{segmentStart, previous});
            if (bounds.size() > MAX_SEGMENTS) {
                // 旧电脑端的轮询调度：每块都换通道，画出来全是碎条，退回整条
                List<Segment> single = new ArrayList<>(1);
                single.add(new Segment("", 0, total, (float) done.cardinality() / total));
                return single;
            }
            List<Segment> out = new ArrayList<>(bounds.size());
            for (int b = 0; b < bounds.size(); b++) {
                int start = bounds.get(b)[0];
                int end = b + 1 < bounds.size() ? bounds.get(b + 1)[0] : total;
                if (end <= start) {
                    end = Math.min(total, start + 1);
                }
                int span = Math.max(1, end - start);
                int filled = 0;
                for (int i = done.nextSetBit(start); i >= 0 && i < end; i = done.nextSetBit(i + 1)) {
                    filled++;
                }
                String ownerName = names.get(bounds.get(b)[1]);
                out.add(new Segment(ownerName, start, end, Math.min(1f, (float) filled / span)));
            }
            return out;
        }
    }

    /** 已完成字节数（按块估，末块可能略多一点点） */
    public long getDoneBytes() {
        synchronized (lock) {
            long bytes = (long) done.cardinality() * FileBlock.BLOCK_SIZE;
            return totalSize > 0 ? Math.min(bytes, totalSize) : bytes;
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
