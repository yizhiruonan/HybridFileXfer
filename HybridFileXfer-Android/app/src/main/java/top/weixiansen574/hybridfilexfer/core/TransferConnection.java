package top.weixiansen574.hybridfilexfer.core;

import java.io.IOException;

import top.weixiansen574.hybridfilexfer.core.bean.TrafficInfo;
import top.weixiansen574.nio.DataByteChannel;

public class TransferConnection {
    public final String iName;
    private TrafficInfo currentTraffic;
    private TrafficInfo totalTraffic;
    public final DataByteChannel channel;

    public TransferConnection(String iName, DataByteChannel channel) throws IOException {
        this.iName = iName;
        this.channel = channel;
        currentTraffic = new TrafficInfo();
        totalTraffic = new TrafficInfo();
    }

    public synchronized void addUploadedBytes(long byteCount) {
        currentTraffic.uploadTraffic += byteCount;
        totalTraffic.uploadTraffic += byteCount;
    }

    public synchronized void addDownloadedBytes(long byteCount) {
        currentTraffic.downloadTraffic += byteCount;
        totalTraffic.downloadTraffic += byteCount;
    }

    public synchronized TrafficInfo resetCurrentTrafficInfo() {
        TrafficInfo info = currentTraffic;
        // 第一秒的 currentTraffic 是 new TrafficInfo()，没有名字；补上，调用方按名字分组
        info.iName = iName;
        currentTraffic = new TrafficInfo(iName);
        return info;
    }

    public synchronized TrafficInfo resetTotalTrafficInfo() {
        TrafficInfo info = totalTraffic;
        totalTraffic = new TrafficInfo(iName);
        return info;
    }

    public synchronized TrafficInfo getTotalTraffic(){
        return totalTraffic;
    }

    /** 本次传输这条通道累计发出的字节（进度条用的真值） */
    public synchronized long getUploadedBytes() {
        return totalTraffic.uploadTraffic;
    }

    /** 本次传输这条通道累计收到的字节（进度条用的真值） */
    public synchronized long getDownloadedBytes() {
        return totalTraffic.downloadTraffic;
    }

    /**
     * 一次原子地取走两个方向的累计值。
     *
     * <p>进度条现在是 100ms 一次的高频轮询，分两次调用 {@link #getUploadedBytes()} /
     * {@link #getDownloadedBytes()} 之间可能被传输线程加过字节，界面会看到"上传涨了下载没涨"
     * 这种撕裂状态；拷一份出来给界面用，不影响内部的真值。
     */
    public synchronized TrafficInfo snapshotTotalTraffic() {
        return new TrafficInfo(iName, totalTraffic.uploadTraffic, totalTraffic.downloadTraffic);
    }

    /** 用户点了「取消」：这条通道是被我们自己关掉的，异常算正常收尾，不算错误 */
    private volatile boolean cancelled;

    public void markCancelled() {
        cancelled = true;
    }

    public boolean isCancelled() {
        return cancelled;
    }

    public void close() throws IOException {
        channel.close();
    }
}

