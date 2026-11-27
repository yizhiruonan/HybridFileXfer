package top.weixiansen574.hybridfilexfer.core;

import java.util.BitSet;
import java.util.HashMap;
import java.util.Map;

/**
 * 一次传输（一批文件）的<b>字节账本：按块去重</b>。界面进度条的 done 只认它。
 *
 * <p>为什么不能继续把"各通道 addUploadedBytes / addDownloadedBytes 的累计值"加起来当 done：
 * 那是<b>线上原始字节</b>，跟"这批文件传完了多少"不是一回事 ——
 * <ol>
 *   <li>同一个块有可能在两条通道上各写一遍（块被救回公共队列后重投、通道半路断掉以后
 *       由别的通道补发），原始字节会把它算两次；</li>
 *   <li>通道对象比"一笔传输"活得久：取消 / 电脑端重建传输通道之后，{@code connections} 里
 *       还能用的旧通道是<b>原对象</b>，它的累计值里带着上一笔传输的字节，界面一累加就凭空
 *       多出几百 MB —— 这正是「4 GB / 2 GB（完成数比总量还大）」的来源；</li>
 *   <li>续传时跳过的块一个字节都不上网，原始字节里压根没有它们。</li>
 * </ol>
 * 所以字节账本必须和 {@link BlockProgress} 一样，以「(文件, 块号) 已完成」为唯一真相：
 * 同一块记一百次也只算一次。区别只有一点 —— <b>它跨文件累计</b>（BlockProgress 换文件
 * 就清零，只能画当前这个文件的分段，当不了多文件批次的总数），这样单文件和多文件/
 * 文件夹走的是同一本账，界面不会出现"一边按单文件、一边按全局"的口径错位。
 *
 * <p>写入点和 {@link BlockProgress} 完全同源：发送方向是 {@code ReadFileCall.noteBlockSent}
 * （块真正写到通道上之后），接收方向是 {@code ReceiveFileCall} 里 CRC32 校验通过之后；
 * 接收方向续传时"本地已经有的块"由 {@code HFXService.receiveFiles} 预先灌进来。
 * <b>发送方向不灌"电脑上已有的块"</b>：那部分由界面的 {@code xferPopupResumeBase} 单独加，
 * 两边都加就是算两遍（老 bug「589 MB / 300 MB」）。
 *
 * <p>线程安全：传输线程（多条）写、界面 50ms 读一次，全部走 lock。
 */
public class TransferLedger {
    private final Object lock = new Object();
    /** 传输路径 → 这个文件已完成的块号 */
    private final Map<String, BitSet> blocks = new HashMap<>();
    /** 传输路径 → 文件大小；没有这个键表示大小还不知道（续传预灌的块拿不到大小） */
    private final Map<String, Long> sizes = new HashMap<>();

    /** 新一轮传输开始：账本清空。每个批次一本账，和上一批彻底无关（不能"忘了清零"）。 */
    public void reset() {
        synchronized (lock) {
            blocks.clear();
            sizes.clear();
        }
    }

    /**
     * 一块已经完成（发出去 / 收到并通过校验）。同一个 (path, index) 重复调用只算一次。
     *
     * @param path      传输路径（{@code Directory.generateTransferPath} 给的那个，同一批里唯一）
     * @param index     文件内块号
     * @param totalSize 文件总大小；0 = 还不知道，按块数 × 块大小估，最后界面还会夹到总量
     */
    public void onBlock(String path, int index, long totalSize) {
        if (path == null || index < 0 || index >= BlockProgress.MAX_BLOCKS) {
            return;
        }
        synchronized (lock) {
            BitSet set = blocks.get(path);
            if (set == null) {
                set = new BitSet();
                blocks.put(path, set);
            }
            set.set(index);
            if (totalSize > 0) {
                sizes.put(path, totalSize);
            }
        }
    }

    /**
     * 已完成字节数：每块只算一次；单个文件封顶在它自己的大小上
     * （末块是短块，块数 × 块大小会比文件大一点点）。
     */
    public long doneBytes() {
        synchronized (lock) {
            long sum = 0;
            for (Map.Entry<String, BitSet> entry : blocks.entrySet()) {
                long bytes = (long) entry.getValue().cardinality() * FileBlock.BLOCK_SIZE;
                Long size = sizes.get(entry.getKey());
                if (size != null && size > 0) {
                    bytes = Math.min(bytes, size);
                }
                sum += bytes;
            }
            return sum;
        }
    }

    /** 账本里出现过的文件数（排障日志用） */
    public int fileCount() {
        synchronized (lock) {
            return blocks.size();
        }
    }
}
