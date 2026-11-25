package top.weixiansen574.hybridfilexfer.core;

/**
 * 传输暂停闸门：传输线程（读文件 / 发块 / 收块）每处理一块检查一次。
 *
 * <p>暂停时不读也不写，对端（电脑）的写会自然被 TCP 缓冲挡住，等于两边都停了；
 * 继续时唤醒所有等待的线程。
 */
public class TransferPause {
    private volatile boolean paused;
    private final Object lock = new Object();

    public void pause() {
        synchronized (lock) {
            paused = true;
        }
    }

    public void resume() {
        synchronized (lock) {
            paused = false;
            lock.notifyAll();
        }
        // 恢复时把限速的计时窗作废。
        //
        // SpeedLimiter 是**累计式**账本：记下「这一轮从开始到现在发了多少字节」，
        // 再和「本该花掉多少时间」比。暂停期间一个字节都不发，但 elapsed 照样在涨，
        // 恢复那一刻手上就攒出一笔和暂停时长等额的"免费额度"。
        // 真机实测：限 5MB/s、200MB 传一半暂停 17.3 秒，恢复后整单只花 23 秒
        // （平均 8.7MB/s），多出来的正好是 17.3 × 5 ≈ 85MB。
        // 电脑端对称的位置是 control.rs 的 pause()。
        SpeedLimiter.resetWindow();
    }

    public boolean isPaused() {
        return paused;
    }

    /** 暂停中就一直等；被中断时抛 InterruptedException（取消/收尾会打断） */
    public void awaitResume() throws InterruptedException {
        synchronized (lock) {
            while (paused) {
                lock.wait(500);
            }
        }
    }
}
