package top.weixiansen574.hybridfilexfer.core;

import java.util.BitSet;

/**
 * 单个文件的"块完成度"位图，给进度浮层画分块进度条用（IDM / 迅雷 那种）。
 *
 * <p>写盘线程每写完一块调用 {@link #onBlockWritten(String, int, long)}，
 * 界面线程随时调用 {@link #segments(int)} 取各段的完成比例：
 * 0 = 还没到，1 = 已完成，0~1 = 这一段只传了一部分。
 *
 * <p>线程安全：写盘线程和 UI 线程会同时访问。
 */
public class BlockProgress {
    /** 位图上限，防止异常文件大小把内存吃光（1M 块 = 1TB） */
    public static final int MAX_BLOCKS = 1 << 20;

    private final Object lock = new Object();
    private String path = "";
    private long totalSize;
    private int totalBlocks;
    private final BitSet done = new BitSet();
    private int doneCount;

    /** 换一次传输（或断开）时清空 */
    public void reset() {
        synchronized (lock) {
            path = "";
            totalSize = 0;
            totalBlocks = 0;
            done.clear();
            doneCount = 0;
        }
    }

    /**
     * 读到文件头时先告诉它文件多大，这样进度条一开始就是完整的一条，
     * 而不是"先有一条缝、传完才变长"。
     */
    public void beginFile(String filePath, long size) {
        if (filePath == null) {
            return;
        }
        synchronized (lock) {
            if (!filePath.equals(path)) {
                path = filePath;
                done.clear();
                doneCount = 0;
                totalSize = 0;
                totalBlocks = 0;
            }
            if (size > 0) {
                totalSize = size;
                totalBlocks = blocksOf(size);
            }
        }
    }

    /** 写盘线程每写完一块调用一次 */
    public void onBlockWritten(String filePath, int index, long size) {
        synchronized (lock) {
            if (filePath != null && !filePath.equals(path)) {
                path = filePath;
                done.clear();
                doneCount = 0;
                totalSize = 0;
                totalBlocks = 0;
            }
            if (size > 0) {
                totalSize = size;
                int blocks = blocksOf(size);
                if (blocks > totalBlocks) {
                    totalBlocks = blocks;
                }
            }
            if (index >= 0 && index < MAX_BLOCKS && !done.get(index)) {
                done.set(index);
                doneCount++;
            }
        }
    }

    /** 各段完成比例（0~1），段数为界面要画的格子数 */
    public float[] segments(int count) {
        int segmentCount = Math.max(1, count);
        float[] out = new float[segmentCount];
        synchronized (lock) {
            int blocks = totalBlocks > 0 ? totalBlocks : Math.max(doneCount, 1);
            for (int i = 0; i < segmentCount; i++) {
                int from = (int) ((long) i * blocks / segmentCount);
                int to = (int) ((long) (i + 1) * blocks / segmentCount);
                if (to <= from) {
                    to = from + 1;
                }
                int filled = 0;
                for (int b = from; b < to && b < blocks; b++) {
                    if (done.get(b)) {
                        filled++;
                    }
                }
                out[i] = Math.min(1f, (float) filled / (to - from));
            }
        }
        return out;
    }

    /** 已经完成的块数（调试/备用） */
    public int getDoneBlocks() {
        synchronized (lock) {
            return doneCount;
        }
    }

    private static int blocksOf(long size) {
        long blocks = (size + FileBlock.BLOCK_SIZE - 1) / FileBlock.BLOCK_SIZE;
        return (int) Math.min(MAX_BLOCKS, Math.max(1, blocks));
    }
}
