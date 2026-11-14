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
    /** 已经投递进某条通道队列的块：抢段是随时发生的，投递顺序不再是一个前缀，所以按块记 */
    private final java.util.BitSet produced = new java.util.BitSet();
    /** 已投递块数（allProduced 要用 O(1) 判断，别每次扫位图） */
    private long producedCount;
    private boolean finished;
    /**
     * 别人的尾巴只剩这么多块时，一次全部接走，不再对半切。
     *
     * <p>再切下去每抢一次都要跨线程投递一轮（读线程重新算 takeNext、往队列里放块），
     * 剩下这几块的传输时间还没这一轮调度长。
     */
    private static final long TAIL_LATCH_BLOCKS = 8;

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
    }

    /**
     * 按需生产：取走这条通道当前段里最小的**还没投递**的块号，并标记为已投递。
     * 读取线程用它决定下一块该读哪——谁缺谁拿，抢到新段的通道立刻能拿到那块。
     *
     * @return 块号（在本次传输的 missing 空间里），本段已经产完则返回 -1
     */
    public synchronized long takeNext(int channel) {
        if (channel < 0 || channel >= channels) {
            return -1;
        }
        int at = produced.nextClearBit((int) Math.max(0, starts[channel]));
        if (at < 0 || at >= ends[channel] || at >= totalBlocks) {
            return -1;
        }
        produced.set(at);
        producedCount++;
        return at;
    }

    /** 整份文件都投递出去了吗 */
    public synchronized boolean allProduced() {
        return producedCount >= totalBlocks;
    }

    /** 已投递出去的块数（排障/日志用） */
    public synchronized long producedBlocks() {
        return producedCount;
    }

    /** 整份文件一共有多少块（排障/日志用） */
    public long totalBlocks() {
        return totalBlocks;
    }

    /** [from, to) 里还有没投递出去的块吗（给「自己这段还没产完」判断用） */
    private boolean hasUnproduced(long from, long to) {
        if (to <= from) {
            return false;
        }
        int at = produced.nextClearBit((int) Math.max(0, from));
        return at >= 0 && at < to;
    }

    /** 「这条通道慢到不值得再给新活」的判定，由 ReadFileCall 按实测速度提供 */
    public interface HopelessOracle {
        boolean isHopeless(int channel);
    }

    private volatile HopelessOracle hopelessOracle;

    /** 装上慢通道判定（读文件线程按 500ms 采样的速度算） */
    public void setHopelessOracle(HopelessOracle oracle) {
        this.hopelessOracle = oracle;
    }

    private boolean isHopeless(int channel) {
        HopelessOracle oracle = hopelessOracle;
        return oracle != null && oracle.isHopeless(channel);
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
        if (hasUnproduced(starts[channel], ends[channel])) {
            // 自己那段还有块没产出来，等它
            return true;
        }
        if (finished) {
            return false;
        }
        // 慢通道（蓝牙 0.2MB/s）不再抢新段：抢来也传不完，只会把收尾拖住。
        // 它自己那段没产出完时上面已经返回 true 了，所以这里不会把它饿死。
        if (isHopeless(channel)) {
            return false;
        }
        int best = -1;
        boolean bestHopeless = false;
        long bestFrom = 0;
        long bestLen = 1;
        for (int i = 0; i < channels; i++) {
            if (i == channel) {
                continue;
            }
            // 只抢别人「还没投递出去」的那条尾巴。已投递的块在每段里是一段前缀，
            // 所以尾巴从「第一个没投递的块」一直到最后：nextClearBit(starts[i])。
            // 注意别从 ends 往回走 —— 每段最后一个块本来就还没投递，那样走一位就停了，
            // 算出来的长度恒为 0，抢段就永远不生效（之前那个 bug 就是这么来的）。
            if (ends[i] <= starts[i]) {
                continue;
            }
            int firstClear = produced.nextClearBit((int) starts[i]);
            long from = firstClear < 0 ? ends[i] : Math.max(starts[i], firstClear);
            if (from >= ends[i]) {
                continue;
            }
            long len = ends[i] - from;
            if (len <= 0) {
                continue;
            }
            // 慢通道（蓝牙）的尾巴：闲下来的快通道**立刻**接手，不再等"所有快通道都干完自己那段"。
            //
            // 以前要等收尾阶段才抢，结果正好相反 —— 快通道全干完了，最后剩的就是蓝牙那一截，
            // 它 0.2MB/s，一截尾巴要吐一分多钟（用户的原话：「收尾用蓝牙比较慢」）。
            // 现在蓝牙的段在开头就被快通道分着接走，它只做完手上已经在传的那几块就收工，
            // 收尾阶段一条蓝牙都不剩。它也不是完全分不到活：抢是"对半切"，第一次抢它留一半，
            // 而且要等快通道自己那段产完了才轮得到（见上面 hasUnproduced 那一支）。
            boolean hopeless = isHopeless(i);
            // 优先抢慢通道的尾巴：它们本来也传不完，早一点把活接过来，收尾就不等它们
            boolean better = best < 0
                    || (hopeless && !bestHopeless)
                    || (hopeless == bestHopeless && len > bestLen);
            if (better) {
                best = i;
                bestHopeless = hopeless;
                bestFrom = from;
                bestLen = len;
            }
        }
        if (best < 0) {
            return false;
        }
        // 尾巴怎么切：从尾巴那头切一半过来（申请者拿后半，捐献者留前半继续往前推）。
        //
        // 慢通道（蓝牙 0.2MB/s）手里那一段以前是「整条被接走」：同时闲下来的快通道有好几条时，
        // 只有第一条能抢到，其余全空转等它慢慢吐 —— 用户看到的「最后只剩蓝牙在收尾」。
        // 现在一律对半切，效果是：
        //   · 几条快通道轮流来抢，几轮就把蓝牙那段尾巴对半分完，尾巴是并行传的；
        //   · 蓝牙自己只从前往后做（takeNext 永远拿最小块号），快通道从后往前啃，
        //     它那一小段从两头同时缩，很快就见底；
        //   · 尾巴只剩最后几块时一次全部接走，不在最后几块上来回切（切一次要跨线程投递一轮，
        //     比直接传这几块还慢）。
        long take = bestLen <= TAIL_LATCH_BLOCKS ? bestLen : (bestLen + 1) / 2;
        long donorEnd = ends[best];
        long mid = Math.max(bestFrom, donorEnd - take);
        if (mid >= donorEnd) {
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
