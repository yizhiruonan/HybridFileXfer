package top.weixiansen574.hybridfilexfer;

import android.view.Choreographer;

/**
 * 逐帧节拍器：把 Choreographer 的 vsync 回调包成「每帧回调一次」的循环。
 *
 * <p><b>为什么不用 {@code handler.postDelayed(16)}</b>：handler 的延时不跟屏幕刷新对齐，
 * 排队一抖就被裁到下一个 vsync 边界，帧间隔在 16~33ms 之间乱跳；上一版更狠 ——
 * 100ms 一拍，一秒只有 10 帧，屏幕上的进度就是「一格一格」地蹦。
 * Choreographer 的回调直接挂在 vsync 上（60Hz → 16.7ms，120Hz → 8.3ms），
 * 每帧算一次、每帧画一次，才可能连续。
 *
 * <p><b>为什么进度条不套 ObjectAnimator</b>：这里要画的值不是「从 A 过渡到 B」的一次性动画，
 * 而是一条随时会变、还会突然跳一下的曲线（块是 1MiB 一颗）。animator 每次目标变了都得重开一个
 * 过渡，目标每 100ms 换一次就不断地重开，反而更抖，而且不好保证「永远不超过真值」。
 * 逐帧积分（dead reckoning）才能同时做到「连续」和「不超过真值」。
 *
 * <p><b>线程约束</b>：只在主线程 start / stop；回调也在主线程。
 * 回调里不要做重活（不要读文件、不要加长锁），只做算术。
 */
final class FrameClock implements Choreographer.FrameCallback {

    interface OnFrame {
        /**
         * @param frameTimeNanos 这一帧的 vsync 时刻，和 {@code System.nanoTime()}、
         *                       {@code SystemClock.elapsedRealtime()} 是同一个时间基准
         */
        void onFrame(long frameTimeNanos);
    }

    private final OnFrame target;
    private boolean running;

    FrameClock(OnFrame target) {
        this.target = target;
    }

    /** 开始（或在已经在跑时忽略）。重复调用是安全的：重复投递会让每帧回调两次 */
    void start() {
        if (running) {
            return;
        }
        running = true;
        Choreographer.getInstance().postFrameCallback(this);
    }

    /** 停止。回调里调用 stop() 也安全（本帧之后不再排下一帧） */
    void stop() {
        if (!running) {
            return;
        }
        running = false;
        Choreographer.getInstance().removeFrameCallback(this);
    }

    boolean isRunning() {
        return running;
    }

    @Override
    public void doFrame(long frameTimeNanos) {
        if (!running) {
            return;
        }
        try {
            target.onFrame(frameTimeNanos);
        } finally {
            // 回调里可能已经 stop()（传输结束/取消/失败）：那就不要再排下一帧，
            // 否则 Choreographer 会一直被这个已经结束的界面占着 vsync
            if (running) {
                Choreographer.getInstance().postFrameCallback(this);
            }
        }
    }
}
