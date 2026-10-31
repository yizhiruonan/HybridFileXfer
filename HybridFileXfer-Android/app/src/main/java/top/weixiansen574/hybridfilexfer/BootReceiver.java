package top.weixiansen574.hybridfilexfer;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.os.Build;
import android.os.Environment;
import android.util.Log;

/**
 * 开机后自动开始等待电脑连接，这样"插上数据线就能发文件"。
 */
public class BootReceiver extends BroadcastReceiver {

    private static final String TAG = "BootReceiver";

    @Override
    public void onReceive(Context context, Intent intent) {
        if (intent == null || intent.getAction() == null) {
            return;
        }
        String action = intent.getAction();
        if (!Intent.ACTION_BOOT_COMPLETED.equals(action)
                && !Intent.ACTION_MY_PACKAGE_REPLACED.equals(action)) {
            return;
        }
        Config config = Config.getInstance(context);
        if (!config.isAutoConnectEnabled()) {
            return;
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R && !Environment.isExternalStorageManager()) {
            Log.i(TAG, "storage permission not granted yet, skip auto start");
            return;
        }
        ServerService.startAuto(context);
    }
}
