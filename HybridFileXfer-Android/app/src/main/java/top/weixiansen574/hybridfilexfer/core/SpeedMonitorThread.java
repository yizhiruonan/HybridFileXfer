package top.weixiansen574.hybridfilexfer.core;

import java.util.ArrayList;
import java.util.List;

import top.weixiansen574.hybridfilexfer.core.TransferConnection;
import top.weixiansen574.hybridfilexfer.core.bean.TrafficInfo;
import top.weixiansen574.hybridfilexfer.core.callback.TransferFileCallback;

public class SpeedMonitorThread extends Thread {
    private boolean isRun = true;
    private final List<TransferConnection> connections;
    private final TransferFileCallback callback;

    public SpeedMonitorThread(List<TransferConnection> connections, TransferFileCallback callback) {
        this.connections = connections;
        this.callback = callback;
    }

    @Override
    @SuppressWarnings("BusyWait")
    public void run() {
        while (isRun){
            long windowStart = System.currentTimeMillis();
            try {
                Thread.sleep(1000);
            } catch (InterruptedException e) {
                break;
            }
            // 这一窗口的**真实**长度。sleep(1000) 只是「至少 1 秒」：满负荷传 12 条通道时
            // 这个线程会被调度延迟，实测窗口能到 1.6 秒左右。
            //
            // 下游（onSpeedInfo 的所有实现：ClientActivity 的每通道/合计速率、
            // TransferActivity 的弹窗、ServerService 的通知栏、SendTask 的完成峰值）
            // 都把这个字段当「每秒字节数」直接显示，所以必须按实际毫秒数换算。
            // 以前直接把「窗口内字节数」当「每秒字节数」用：窗口多长，数字就虚高多少倍 ——
            // 真机实测真实峰值约 120 MB/s，完成提示的「最高」却写 198（≈×1.65，
            // 正好等于那一段的平均窗口 1.65 秒）。
            long elapsedMs = Math.max(1L, System.currentTimeMillis() - windowStart);
            List<TrafficInfo> trafficInfoList = new ArrayList<>();
            for (TransferConnection channel : connections) {
                TrafficInfo info = channel.resetCurrentTrafficInfo();
                info.uploadTraffic = info.uploadTraffic * 1000L / elapsedMs;
                info.downloadTraffic = info.downloadTraffic * 1000L / elapsedMs;
                trafficInfoList.add(info);
            }
            callback.onSpeedInfo(trafficInfoList);
        }
    }

    public void cancel(){
        isRun = false;
        interrupt();
    }
}
