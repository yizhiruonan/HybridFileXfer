package top.weixiansen574.hybridfilexfer.core;

import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.util.BitSet;

/**
 * 断点续传日志（手机作接收端时用）：记录"这个文件已经收到哪些 1MiB 块"。
 *
 * <p>格式与电脑端 {@code src/journal.rs} 完全一致，两边都能读写同一份 sidecar：
 * <pre>
 * magic "HFXP"(4) | version(1) | reserved(3) | blockSize(u32) | totalSize(u64) |
 * mtime(i64) | bitmapLen(u32) | bitmap[]
 * </pre>
 * 位图第 i 位（LSB 先） = 第 i 块已经落盘。
 *
 * <p>为什么需要它：落盘是"先 setLength(总大小) 再按块随机写"，光看文件大小分不出
 * 哪些块有洞。有位图才能放心保留半截文件，下次只补缺的块。
 */
public final class ResumeJournal {
    public static final String SUFFIX = ".hfxpart";
    private static final byte[] MAGIC = {'H', 'F', 'X', 'P'};
    private static final int VERSION = 1;

    public final long blockSize;
    public final long totalSize;
    public final long mtimeMs;
    public final byte[] bits;

    private ResumeJournal(long blockSize, long totalSize, long mtimeMs, byte[] bits) {
        this.blockSize = blockSize;
        this.totalSize = totalSize;
        this.mtimeMs = mtimeMs;
        this.bits = bits;
    }

    public static String sidecarPath(String target) {
        return target + SUFFIX;
    }

    public static ResumeJournal create(long totalSize, long mtimeMs, long blockSize) {
        long size = Math.max(1, blockSize);
        long blocks = (totalSize + size - 1) / size;
        return new ResumeJournal(size, totalSize, mtimeMs, new byte[(int) ((blocks + 7) / 8)]);
    }

    /** 读旁边那份日志；不存在 / 格式不对 / 大小或修改时间对不上都返回 null（当新文件重传） */
    public static ResumeJournal load(String target, long totalSize, long mtimeMs, long blockSize) {
        File file = new File(sidecarPath(target));
        if (!file.isFile() || file.length() < 32) {
            return null;
        }
        try (FileInputStream in = new FileInputStream(file)) {
            byte[] header = new byte[32];
            if (in.read(header) != 32) {
                return null;
            }
            for (int i = 0; i < 4; i++) {
                if (header[i] != MAGIC[i]) {
                    return null;
                }
            }
            if ((header[4] & 0xFF) != VERSION) {
                return null;
            }
            long storedBlock = readInt(header, 8) & 0xFFFFFFFFL;
            long storedTotal = readLong(header, 12);
            long storedMtime = readLong(header, 20);
            int bitmapLen = readInt(header, 28);
            if (storedBlock != Math.max(1, blockSize) || storedTotal != totalSize || storedMtime != mtimeMs) {
                return null;
            }
            if (bitmapLen <= 0 || bitmapLen > 4 * 1024 * 1024) {
                return null;
            }
            byte[] bits = new byte[bitmapLen];
            int read = 0;
            while (read < bitmapLen) {
                int n = in.read(bits, read, bitmapLen - read);
                if (n <= 0) {
                    return null;
                }
                read += n;
            }
            return new ResumeJournal(storedBlock, storedTotal, storedMtime, bits);
        } catch (IOException e) {
            return null;
        }
    }

    public void save(String target) {
        byte[] out = new byte[32 + bits.length];
        System.arraycopy(MAGIC, 0, out, 0, 4);
        out[4] = (byte) VERSION;
        putInt(out, 8, (int) blockSize);
        putLong(out, 12, totalSize);
        putLong(out, 20, mtimeMs);
        putInt(out, 28, bits.length);
        System.arraycopy(bits, 0, out, 32, bits.length);
        File file = new File(sidecarPath(target));
        File parent = file.getParentFile();
        if (parent != null && !parent.exists()) {
            //noinspection ResultOfMethodCallIgnored
            parent.mkdirs();
        }
        try (FileOutputStream os = new FileOutputStream(file)) {
            os.write(out);
            os.flush();
        } catch (IOException ignored) {
        }
    }

    public static void remove(String target) {
        //noinspection ResultOfMethodCallIgnored
        new File(sidecarPath(target)).delete();
    }

    public long blockCount() {
        return (totalSize + blockSize - 1) / blockSize;
    }

    public boolean isDone(long index) {
        int byteIndex = (int) (index / 8);
        return byteIndex >= 0 && byteIndex < bits.length && (bits[byteIndex] & (1 << (index % 8))) != 0;
    }

    public void mark(long index) {
        int byteIndex = (int) (index / 8);
        if (byteIndex >= 0 && byteIndex < bits.length) {
            bits[byteIndex] |= (byte) (1 << (index % 8));
        }
    }

    public boolean allDone() {
        long blocks = blockCount();
        for (long i = 0; i < blocks; i++) {
            if (!isDone(i)) {
                return false;
            }
        }
        return true;
    }

    public long blocksDone() {
        long count = 0;
        for (long i = 0; i < blockCount(); i++) {
            if (isDone(i)) {
                count++;
            }
        }
        return count;
    }

    public BitSet bitset() {
        return BitSet.valueOf(bits);
    }

    private static int readInt(byte[] b, int at) {
        return ((b[at] & 0xFF) << 24) | ((b[at + 1] & 0xFF) << 16) | ((b[at + 2] & 0xFF) << 8) | (b[at + 3] & 0xFF);
    }

    private static long readLong(byte[] b, int at) {
        long value = 0;
        for (int i = 0; i < 8; i++) {
            value = (value << 8) | (b[at + i] & 0xFFL);
        }
        return value;
    }

    private static void putInt(byte[] b, int at, int v) {
        b[at] = (byte) (v >>> 24);
        b[at + 1] = (byte) (v >>> 16);
        b[at + 2] = (byte) (v >>> 8);
        b[at + 3] = (byte) v;
    }

    private static void putLong(byte[] b, int at, long v) {
        for (int i = 7; i >= 0; i--) {
            b[at + i] = (byte) v;
            v >>>= 8;
        }
    }
}
