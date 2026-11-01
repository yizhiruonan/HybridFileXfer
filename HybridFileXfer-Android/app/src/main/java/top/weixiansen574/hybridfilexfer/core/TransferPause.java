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
