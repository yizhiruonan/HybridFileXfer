package top.weixiansen574.hybridfilexfer;

import android.os.Looper;
import android.util.Log;

import top.weixiansen574.hybridfilexfer.core.HFXLog;

/**
 * 兜住未捕获异常：先记日志，再决定要不要把进程带走。
 *
 * <p>传输过程中拔线时，后台传输线程里任何一处没拦住的空指针都会直接让整个 App 闪退
 * （用户报的就是这个）。这些线程本来就该"尽力而为"：一条通道挂了，其它通道和界面
 * 都不该跟着死。所以这里对后台线程的异常只记录、不重抛，进程继续活着，下一轮自动重连；
 * 主线程的异常仍然交给系统（界面状态已经不可信，硬撑只会更乱）。
 */
public final class CrashGuard {

    private static final String TAG = "HFXCrash";

    private static volatile boolean installed;

    private CrashGuard() {
    }

    public static synchronized void install() {
        if (installed) {
            return;
        }
        installed = true;
        final Thread.UncaughtExceptionHandler previous = Thread.getDefaultUncaughtExceptionHandler();
        Thread.setDefaultUncaughtExceptionHandler(new Thread.UncaughtExceptionHandler() {
            @Override
            public void uncaughtException(Thread thread, Throwable throwable) {
                boolean main = Looper.getMainLooper() != null
                        && thread == Looper.getMainLooper().getThread();
                try {
                    HFXLog.d("未捕获异常 thread=" + thread.getName() + (main ? "（主线程）" : ""), throwable);
                    Log.e(TAG, "未捕获异常 thread=" + thread.getName(), throwable);
                } catch (Throwable ignored) {
                }
                if (main) {
                    if (previous != null) {
                        previous.uncaughtException(thread, throwable);
                    }
                    return;
                }
                // 后台线程（传输/会话）：只结束这一个线程，进程继续，
                // 服务下一轮会自动重连；不让拔线把整个 App 带走
                try {
                    ServerService.onBackgroundThreadCrash(thread.getName());
                } catch (Throwable ignored) {
                }
            }
        });
    }
}
