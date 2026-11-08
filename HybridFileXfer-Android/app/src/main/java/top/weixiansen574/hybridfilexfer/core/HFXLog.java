package top.weixiansen574.hybridfilexfer.core;

import java.io.File;
import java.io.FileWriter;
import java.text.SimpleDateFormat;
import java.util.Date;
import java.util.Locale;

import top.weixiansen574.hybridfilexfer.BuildConfig;

/**
 * 协议层时序插桩：把关键时序 append 到 /sdcard/Download/hfx_core.log（排查控制通道读串用）。
 *
 * 只在 DEBUG 构建里生效：release 包第一行就 return，零开销、不写文件。
 * 排障时用 debug 包即可复现完整时序（见 HANDOFF 第 5 节的时间对齐方法）。
 */
public final class HFXLog {
    /** SimpleDateFormat 非线程安全，而这里会被 6 条传输线程同时调用。 */
    private static final ThreadLocal<SimpleDateFormat> FMT = new ThreadLocal<SimpleDateFormat>() {
        @Override
        protected SimpleDateFormat initialValue() {
            return new SimpleDateFormat("HH:mm:ss.SSS", Locale.US);
        }
    };

    private HFXLog() {
    }

    public static void d(String line) {
        if (!BuildConfig.DEBUG) {
            return;
        }
        try {
            File f = new File(android.os.Environment.getExternalStoragePublicDirectory(
                    android.os.Environment.DIRECTORY_DOWNLOADS), "hfx_core.log");
            FileWriter w = new FileWriter(f, true);
            w.write(FMT.get().format(new Date()) + "  " + line + "\n");
            w.close();
        } catch (Throwable ignored) {
        }
    }

    public static void d(String line, Throwable e) {
        if (!BuildConfig.DEBUG) {
            return;
        }
        StringBuilder sb = new StringBuilder(line);
        if (e != null) {
            sb.append(" : ").append(e);
            StackTraceElement[] st = e.getStackTrace();
            for (int i = 0; i < Math.min(4, st.length); i++) {
                sb.append(" | ").append(st[i]);
            }
        }
        d(sb.toString());
    }
}
