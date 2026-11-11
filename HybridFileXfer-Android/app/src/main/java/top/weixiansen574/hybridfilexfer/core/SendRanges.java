package top.weixiansen574.hybridfilexfer.core;

import java.util.ArrayList;
import java.util.List;

/**
 * 发送方向的分段计划（IDM / 迅雷 那种"一条连接一段"）。
 *
 * <p>以前手机发送用的是"一个共享队列，谁抢到下一块谁发"：块是轮流投递的，
 * 段边界几乎每块都变，界面上只能画成"按通道数等分"的假分段 —— 传输过程中
 * 不会出现新的切割线。
 *
 * <p>现在改成：文件的块区间按通道切成连续段，读文件仍然是顺序读（磁盘友好），
 * 只是按这份计划把每一块投递到"负责这段"的通道自己的队列里。某条通道先干完，
 * 就把别人还没产出的部分里最长的一段再切一半过来 —— 段数因此会变多，
 * 进度条上就多出新的切割线。
 */
public final class SendRanges {

    /** 进度条上的一段（百分比区间） */
    public static final class Segment {
        /** 负责这条连接的名字 */
        public final String owner;
        /** 这一段在整条进度条上的起止（0~100） */
        public final float start;
        public final float end;

        Segment(String owner, float start, float end) {
            this.owner = owner;
            this.start = start;
            this.end = end;
        }
    }

    private final int channels;
    private final long totalBlocks;
    private final String[] names;
    private final long[] starts;
    private final long[] ends;
    /** 已经投递出去的块数（小于它的块不会再改归属） */
    private long frontier;
    private boolean finished;

    public SendRanges(int channels, long totalBlocks, String[] names) {
        this.channels = Math.max(1, channels);
        this.totalBlocks = Math.max(1, totalBlocks);
        this.names = new String[this.channels];
        for (int i = 0; i < this.channels; i++) {
            this.names[i] = names != null && i < names.length && names[i] != null ? names[i] : ("ch" + i);
        }
        this.starts = new long[this.channels];
        this.ends = new long[this.channels];
        long base = this.totalBlocks / this.channels;
        long rem = this.totalBlocks % this.channels;
        long at = 0;
        for (int i = 0; i < this.channels; i++) {
            long len = base + (i < rem ? 1 : 0);
            starts[i] = at;
            ends[i] = at + len;
            at += len;
        }
        this.frontier = 0;
    }

    /** 第 block 块该由哪条通道发 */
    public synchronized int ownerOf(long block) {
        for (int i = 0; i < channels; i++) {
            if (block >= starts[i] && block < ends[i]) {
                return i;
            }
        }
        return (int) (block % channels);
    }

    /** 第 block 块已经投递出去了（之后不会再改归属） */
    public synchronized void markProduced(long block) {
        if (block + 1 > frontier) {
            frontier = block + 1;
        }
    }

    /**
     * 通道 channel 干完自己的段了：看看能不能从别人"还没产出"的部分里切一半过来。
     *
     * @return true 表示它还有活干（自己那段还没产完，或者抢到了新的一段）
     */
    public synchronized boolean trySteal(int channel) {
        if (channel < 0 || channel >= channels) {
            return false;
        }
        long from = Math.max(starts[channel], frontier);
        if (ends[channel] > from) {
            // 自己那段还有块没产出来，等它
            return true;
        }
        if (finished) {
            return false;
        }
        int best = -1;
        long bestLen = 1;
        for (int i = 0; i < channels; i++) {
            if (i == channel) {
                continue;
            }
            long donorFrom = Math.max(starts[i], frontier);
            long len = ends[i] - donorFrom;
            if (len > bestLen) {
                best = i;
                bestLen = len;
            }
        }
        if (best < 0) {
            return false;
        }
        long donorFrom = Math.max(starts[best], frontier);
        long donorEnd = ends[best];
        long mid = donorFrom + (donorEnd - donorFrom) / 2;
        if (mid <= donorFrom) {
            return false;
        }
        // 把捐献者剩下的后半段 [mid, donorEnd) 划给申请者
        ends[best] = mid;
        starts[channel] = mid;
        ends[channel] = donorEnd;
        return true;
    }

    /** 读取线程把整份文件都产完了：之后不再重新分段 */
    public synchronized void finish() {
        finished = true;
    }

    public synchronized boolean isFinished() {
        return finished;
    }

    /** 给进度条用的快照（按在文件里的先后顺序排好） */
    public synchronized List<Segment> snapshot() {
        List<Segment> out = new ArrayList<>(channels);
        for (int i = 0; i < channels; i++) {
            if (ends[i] <= starts[i]) {
                continue;
            }
            out.add(new Segment(
                    names[i],
                    (float) starts[i] * 100f / totalBlocks,
                    (float) ends[i] * 100f / totalBlocks));
        }
        out.sort((a, b) -> Float.compare(a.start, b.start));
        return out;
    }
}
