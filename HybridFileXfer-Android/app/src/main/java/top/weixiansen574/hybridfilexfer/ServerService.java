package top.weixiansen574.hybridfilexfer;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.ServiceConnection;
import android.content.pm.PackageManager;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.SystemClock;
import android.text.TextUtils;
import android.util.Log;

import androidx.core.app.NotificationCompat;
import androidx.core.content.ContextCompat;

import java.io.IOException;
import java.net.InetAddress;
import java.net.SocketException;
import java.net.UnknownHostException;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;

import rikka.shizuku.Shizuku;
import top.weixiansen574.hybridfilexfer.aidl.IIOService;
import top.weixiansen574.hybridfilexfer.core.bean.ServerNetInterface;
import top.weixiansen574.hybridfilexfer.droidcore.HFXServer;
import top.weixiansen574.hybridfilexfer.droidcore.callback.StartServerCallback;

/**
 * 后台服务：保持端口监听，等待电脑连接。
 * <p>
 * 有了它，手机插上电脑（电脑上运行接收器）就会自动建立连接，不需要先打开应用点按钮；
 * 分享面板里的"发送到电脑"也能在应用处于后台时使用。
 */
public class ServerService extends Service implements ServiceConnection {

    private static final String TAG = "ServerService";

    public static final String ACTION_START = "top.weixiansen574.hybridfilexfer.action.START_SERVER";
    public static final String ACTION_STOP = "top.weixiansen574.hybridfilexfer.action.STOP_SERVER";
    public static final String EXTRA_INTERFACES = "interfaces";
    public static final String EXTRA_MODE = "io_mode";
    public static final String EXTRA_FRESH = "fresh";
    public static final String EXTRA_LOCAL_BUFFER = "local_buffer_count";
    public static final String EXTRA_REMOTE_BUFFER = "remote_buffer_count";

    public static final int STATE_STOPPED = 0;
    public static final int STATE_STARTING = 1;
    public static final int STATE_WAITING = 2;
    public static final int STATE_CONNECTED = 3;

    private static final int NOTIFICATION_ID = 5740;
    private static final String CHANNEL_ID = "server";
    private static final long PING_INTERVAL_MS = 3000;

    private static final CopyOnWriteArrayList<Listener> LISTENERS = new CopyOnWriteArrayList<>();
    private static volatile int state = STATE_STOPPED;
    private static volatile List<String> interfaceNames = Collections.emptyList();

    private Config config;
    private final Handler mainHandler = new Handler(Looper.getMainLooper());

    private IIOService ioService;
    private Thread loopThread;
    private volatile boolean active;
    private volatile boolean freshRequested;
    private volatile boolean sessionConnected;
    private volatile boolean bound;
    private volatile boolean shizukuBound;
    private volatile int ioMode = Config.MODE_NORMAL;
    private volatile List<ServerNetInterface> interfaces = Collections.emptyList();
    private volatile int localBufferCount = 256;
    private volatile int remoteBufferCount = 512;
    /** 每次重启监听都会自增，旧的循环线程据此退出，避免两个监听线程并存 */
    private volatile int generation = 0;
    private DiscoveryBeacon discoveryBeacon;

    /**
     * 监听服务状态变化（回调在主线程）
     */
    public interface Listener {
        default void onServerStateChanged(int state) {
        }

        default void onInterfaceAccepted(String name) {
        }

        default void onInterfaceAcceptFailed(String name) {
        }

        default void onBindFailed(int port) {
        }

        default void onOutOfMemory(int created, int needed) {
        }

        default void onRemoteOutOfMemory() {
        }

        default void onServerError(String message) {
        }
    }

    // ------------------------------------------------------------------
    // 供 Activity / 分享界面使用的静态入口
    // ------------------------------------------------------------------

    public static void addListener(Listener listener) {
        if (listener != null && !LISTENERS.contains(listener)) {
            LISTENERS.add(listener);
        }
    }

    public static void removeListener(Listener listener) {
        LISTENERS.remove(listener);
    }

    public static int getState() {
        return state;
    }

    public static boolean isRunning() {
        return state != STATE_STOPPED;
    }

    public static boolean isConnected() {
        return state == STATE_CONNECTED && HFXServer.instance != null;
    }

    public static HFXServer getServer() {
        return HFXServer.instance;
    }

    public static List<String> getInterfaceNames() {
        return interfaceNames;
    }

    public static List<String> getChannelDetails() {
        HFXServer server = getServer();
        return server != null ? server.getChannelDetails() : java.util.Collections.emptyList();
    }

    /** MIUI 会吞应用日志，调试信息直接写文件 */
    private static void debugFile(String line) {
        try {
            java.io.File f = new java.io.File(
                    android.os.Environment.getExternalStoragePublicDirectory(
                            android.os.Environment.DIRECTORY_DOWNLOADS),
                    "hfx_pull.log");
            java.io.FileWriter w = new java.io.FileWriter(f, true);
            w.write(new java.text.SimpleDateFormat("HH:mm:ss", java.util.Locale.US)
                    .format(new java.util.Date()) + "  " + line + "\n");
            w.close();
        } catch (Throwable ignored) {
        }
    }

    /**
     * 电脑通过「发送到手机」推送过来的文件：拉到手机的 Downloads/HybridFileXfer。
     */
    private void pullPendingFiles(HFXServer server,
            java.util.List<top.weixiansen574.hybridfilexfer.core.bean.RemoteFile> files) {
        final String TAG2 = "HFXPULL";
        try {
            debugFile("pull start, files=" + files.size());
            for (top.weixiansen574.hybridfilexfer.core.bean.RemoteFile f : files) {
                debugFile("  pending: " + f.getPath() + " (" + f.getSize() + ")");
            }
            java.io.File dir = new java.io.File(
                    android.os.Environment.getExternalStoragePublicDirectory(
                            android.os.Environment.DIRECTORY_DOWNLOADS),
                    "HybridFileXfer");
            if (!dir.exists() && !dir.mkdirs()) {
                Log.w(TAG, "cannot create " + dir);
                return;
            }
            top.weixiansen574.hybridfilexfer.core.bean.Directory local =
                    new top.weixiansen574.hybridfilexfer.core.bean.Directory(
                            dir.getAbsolutePath(),
                            top.weixiansen574.hybridfilexfer.core.bean.Directory.getCurrentFileSystem());
            top.weixiansen574.hybridfilexfer.core.bean.Directory remote =
                    new top.weixiansen574.hybridfilexfer.core.bean.Directory(
                            "/",
                            top.weixiansen574.hybridfilexfer.core.bean.Directory.getCurrentFileSystem());
            debugFile("local=" + local.path + " remote=" + remote.path);
            server.sendFilesToShelf(files, local, remote,
                    new top.weixiansen574.hybridfilexfer.core.callback.TransferFileCallback() {
                        public void onFileUploading(String iName, String path, long targetSize, long totalSize) {
                        }
                        public void onFileDownloading(String iName, String path, long targetSize, long totalSize) {
                        }
                        public void onSpeedInfo(java.util.List<top.weixiansen574.hybridfilexfer.core.bean.TrafficInfo> list) {
                        }
                        public void onChannelComplete(String iName, long traffic, long time) {
                        }
                        public void onChannelError(String iName, int errorType, String message) {
                            debugFile("channel error " + iName + " type=" + errorType + " " + message);
                        }
                        public void onReadFileError(String message) {
                            debugFile("read file error: " + message);
                        }
                        public void onWriteFileError(String message) {
                            debugFile("WRITE FILE ERROR: " + message);
                        }
                        public void onComplete(boolean isUpload, long traffic, long time) {
                            debugFile("complete: " + traffic + " bytes -> " + dir);
                        }
                        public void onIncomplete() {
                            debugFile("incomplete");
                        }
                    });
            debugFile("pulled " + files.size() + " file(s) from PC");
        } catch (Throwable e) {
            debugFile("pull failed: " + e);
        }
    }

    public static List<String> getConnectedInterfaceNames() {
        HFXServer server = HFXServer.instance;
        if (server == null || state != STATE_CONNECTED) {
            return Collections.emptyList();
        }
        try {
            return server.getConnectionListINames();
        } catch (Throwable e) {
            return Collections.emptyList();
        }
    }

    /**
     * 启动（或用新的网卡列表重启）后台接收服务。
     */
    public static void startServer(Context context, int mode, List<ServerNetInterface> serverInterfaces,
                                   int localBuffer, int remoteBuffer) {
        Intent intent = new Intent(context, ServerService.class);
        intent.setAction(ACTION_START);
        intent.putExtra(EXTRA_MODE, mode);
        java.util.ArrayList<ServerNetInterface> expanded = new java.util.ArrayList<>();
        for (ServerNetInterface netInterface : serverInterfaces) {
            // 自动配置：WIFI 链路带宽高，多开几条通道；USB 通道受 adb 限制，开 2 条足够
            String lower = netInterface.name == null ? "" : netInterface.name.toLowerCase(java.util.Locale.ROOT);
            int channelsPerInterface = (lower.contains("wlan") || lower.contains("wifi") || lower.contains("wlan0")) ? 4 : 2;
            java.util.Set<String> disabled = Config.getInstance(context).getDisabledChannels();
            for (int i = 0; i < channelsPerInterface; i++) {
                String name = i == 0 ? netInterface.name : netInterface.name + "#" + (i + 1);
                if (disabled.contains(name)) {
                    continue;
                }
                expanded.add(new ServerNetInterface(name, netInterface.address, netInterface.clientBindAddress));
            }
        }
        intent.putStringArrayListExtra(EXTRA_INTERFACES, serialize(expanded));
        top.weixiansen574.hybridfilexfer.core.SpeedLimiter.setLimitBytesPerSecond(
                Config.getInstance(context).getSpeedLimitMbps() * 1024L * 1024L);
        intent.putExtra(EXTRA_LOCAL_BUFFER, localBuffer);
        intent.putExtra(EXTRA_REMOTE_BUFFER, remoteBuffer);
        ContextCompat.startForegroundService(context, intent);
    }

    /**
     * 按保存的配置启动（开机自启、电脑上的 adb 拉起服务时使用）。
     */
    public static void startAuto(Context context) {
        try {
            Config config = Config.getInstance(context);
            List<ServerNetInterface> list = NetInterfaces.forAutoConnect(context);
            if (list.isEmpty()) {
                Log.w(TAG, "startAuto: no interface selected");
                return;
            }
            startServer(context, config.getMode(), list, config.getLocalBufferCount(),
                    config.getRemoteBufferCount());
        } catch (SocketException | UnknownHostException e) {
            Log.w(TAG, "startAuto failed", e);
        }
    }

    /**
     * 传输出错后重新开始监听（不断开自动连接服务本身）。
     */
    public static void restartSession(Context context) {
        Intent intent = new Intent(context, ServerService.class);
        intent.setAction(ACTION_START);
        intent.putExtra(EXTRA_FRESH, true);
        try {
            ContextCompat.startForegroundService(context, intent);
        } catch (Throwable e) {
            Log.w(TAG, "restartSession failed", e);
        }
    }

    public static void stopServer(Context context) {
        Intent intent = new Intent(context, ServerService.class);
        intent.setAction(ACTION_STOP);
        try {
            ContextCompat.startForegroundService(context, intent);
        } catch (Throwable e) {
            Log.w(TAG, "stopServer failed", e);
        }
    }

    /**
     * 电脑重新接上时调用：断开可能已经失效的旧连接，重新开始监听。
     */
    private void requestFresh() {
        freshRequested = true;
    }

    // ------------------------------------------------------------------
    // 生命周期
    // ------------------------------------------------------------------

    @Override
    public void onCreate() {
        super.onCreate();
        config = Config.getInstance(this);
        createNotificationChannel();
        goForeground();
        // 局域网广播：让电脑不用 adb、不用手动输 IP 也能发现手机
        discoveryBeacon = new DiscoveryBeacon(this, config.getServerPort());
        discoveryBeacon.start();
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent == null) {
            // 被系统重启，按保存的配置恢复
            startAuto(this);
            return START_STICKY;
        }
        String action = intent.getAction();
        if (ACTION_STOP.equals(action)) {
            stopEverything();
            stopSelf();
            return START_NOT_STICKY;
        }
        // 电脑上的接收器用 adb 拉起服务时不会带 action，这里也当成"开始监听"
        if (ACTION_START.equals(action) || action == null) {
            if (intent.getBooleanExtra(EXTRA_FRESH, false)) {
                requestFresh();
            }
            List<ServerNetInterface> requested = deserialize(intent.getStringArrayListExtra(EXTRA_INTERFACES));
            int mode = intent.getIntExtra(EXTRA_MODE, config.getMode());
            int local = intent.getIntExtra(EXTRA_LOCAL_BUFFER, config.getLocalBufferCount());
            int remote = intent.getIntExtra(EXTRA_REMOTE_BUFFER, config.getRemoteBufferCount());
            if (requested.isEmpty()) {
                // 只带了"重新开始监听"的请求：保持当前网卡配置，若服务未运行则按配置启动
                if (!active) {
                    startAuto(this);
                }
            } else if (isSameConfiguration(requested, mode)) {
                // 已经在用同样的配置运行：只需要保持监听即可
                if (state == STATE_STOPPED) {
                    begin(requested, mode, local, remote);
                }
            } else {
                begin(requested, mode, local, remote);
            }
        }
        return START_STICKY;
    }

    private boolean isSameConfiguration(List<ServerNetInterface> requested, int mode) {
        if (!active || ioMode != mode || !bound) {
            return false;
        }
        List<String> current = serialize(interfaces);
        List<String> wanted = serialize(requested);
        return current.equals(wanted);
    }

    private void begin(List<ServerNetInterface> requested, int mode, int local, int remote) {
        stopLoop(true);
        generation++;
        loopThread = null;
        // 旧循环用 freshRequested 退出，新循环一开始应该是"正常监听"状态
        freshRequested = false;
        this.interfaces = new ArrayList<>(requested);
        this.localBufferCount = local;
        this.remoteBufferCount = remote;
        this.ioMode = mode;
        List<String> names = new ArrayList<>(requested.size());
        for (ServerNetInterface item : requested) {
            names.add(item.name);
        }
        interfaceNames = names;
        active = true;
        setState(STATE_STARTING);
        bindIoService(mode);
    }

    private void bindIoService(int mode) {
        if (mode != Config.MODE_NORMAL) {
            if (shizukuUsable()) {
                shizukuBound = true;
                bound = true;
                try {
                    Shizuku.bindUserService(IOService.getUserServiceArgs(this), this);
                    return;
                } catch (Throwable e) {
                    Log.w(TAG, "Shizuku.bindUserService failed", e);
                    shizukuBound = false;
                    bound = false;
                }
            }
            notifyError(getString(R.string.fallback_to_normal_mode));
            ioMode = Config.MODE_NORMAL;
        }
        try {
            bound = bindService(new Intent(this, IOService.class), this, Context.BIND_AUTO_CREATE);
        } catch (Throwable e) {
            notifyError(e.toString());
        }
    }

    private boolean shizukuUsable() {
        try {
            return Shizuku.pingBinder() && Shizuku.checkSelfPermission() == PackageManager.PERMISSION_GRANTED;
        } catch (Throwable e) {
            return false;
        }
    }

    @Override
    public void onServiceConnected(ComponentName name, IBinder service) {
        ioService = IIOService.Stub.asInterface(service);
        bound = true;
        if (active && loopThread == null) {
            int gen = generation;
            loopThread = new Thread(() -> serverLoop(gen), "HFX-Server-Loop");
            loopThread.start();
        }
    }

    @Override
    public void onServiceDisconnected(ComponentName name) {
        bound = false;
        ioService = null;
        notifyError(getString(R.string.io_service_disconnected));
    }

    @Override
    public void onBindingDied(ComponentName name) {
        bound = false;
        ioService = null;
        if (active) {
            mainHandler.postDelayed(() -> bindIoService(ioMode), 1000);
        }
    }

    private void serverLoop(int gen) {
        while (active && gen == generation) {
            HFXServer server = null;
            sessionConnected = false;
            try {
                server = new HFXServer(ioService);
                final HFXServer thisServer = server;
                HFXServer.instance = thisServer;
                thisServer.startServer(config.getServerPort(), interfaces, localBufferCount, remoteBufferCount,
                        new StartServerCallback() {
                            @Override
                            public void onBindFailed(int port) {
                                notifyBindFailed(port);
                            }

                            @Override
                            public void onStatedServer() {
                                setState(STATE_WAITING);
                            }

                            @Override
                            public void onAccepted(String name) {
                                notifyAccepted(name);
                            }

                            @Override
                            public void onAcceptFailed(String name) {
                                notifyAcceptFailed(name);
                            }

                            @Override
                            public void onPcOOM() {
                                notifyRemoteOOM();
                            }

                            @Override
                            public void onMeOOM(int created, int localBufferCount) {
                                notifyLocalOOM(created, localBufferCount);
                            }

                            @Override
                            public void onConnectSuccess() {
                                sessionConnected = true;
                                HFXServer.instance = thisServer;
                                setState(STATE_CONNECTED);
                            }

                            @Override
                            public void onError(Throwable th) {
                                notifyError(String.valueOf(th));
                            }
                        });
            } catch (Throwable e) {
                Log.w(TAG, "startServer failed", e);
                notifyError(String.valueOf(e));
            }

            if (server != null && sessionConnected) {
                long lastPing = SystemClock.elapsedRealtime();
                while (active && sessionConnected && !freshRequested && gen == generation) {
                    SystemClock.sleep(250);
                    if (SystemClock.elapsedRealtime() - lastPing < PING_INTERVAL_MS) {
                        continue;
                    }
                    lastPing = SystemClock.elapsedRealtime();
                    try {
                        server.writePing();
                    } catch (IOException e) {
                        Log.i(TAG, "heartbeat failed: " + e);
                        break;
                    }
                    // 问一下电脑有没有"发送到手机"推过来的文件，有就直接拉下来
                    try {
                        java.util.List<top.weixiansen574.hybridfilexfer.core.bean.RemoteFile> pending =
                                server.checkPending();
                        if (!pending.isEmpty()) {
                            pullPendingFiles(server, pending);
                        }
                    } catch (Throwable e) {
                        debugFile("checkPending failed: " + e);
                    }
                }
            }

            sessionConnected = false;
            freshRequested = false;
            if (gen == generation) {
                HFXServer.instance = null;
            }
            if (server != null) {
                try {
                    server.disconnect();
                } catch (Throwable ignored) {
                }
            }
            if (active && gen == generation) {
                setState(STATE_WAITING);
                SystemClock.sleep(600);
            }
        }
        if (gen == generation) {
            setState(STATE_STOPPED);
        }
    }

    private void stopLoop(boolean unbind) {
        generation++;
        active = false;
        freshRequested = true;
        sessionConnected = false;
        HFXServer server = HFXServer.instance;
        HFXServer.instance = null;
        if (server != null) {
            try {
                server.closeServerSocket();
            } catch (Throwable ignored) {
            }
        }
        Thread thread = loopThread;
        loopThread = null;
        if (thread != null) {
            thread.interrupt();
        }
        if (unbind) {
            unbindIoService();
        }
    }

    private void unbindIoService() {
        try {
            if (shizukuBound) {
                Shizuku.unbindUserService(IOService.getUserServiceArgs(this), this, true);
            } else if (bound) {
                unbindService(this);
            }
        } catch (Throwable e) {
            Log.w(TAG, "unbind failed", e);
        }
        shizukuBound = false;
        bound = false;
        ioService = null;
        HFXServer.instance = null;
    }

    private void stopEverything() {
        active = false;
        stopLoop(true);
        setState(STATE_STOPPED);
        stopForegroundCompat();
    }

    @Override
    public void onDestroy() {
        if (discoveryBeacon != null) {
            discoveryBeacon.stop();
            discoveryBeacon = null;
        }
        stopEverything();
        super.onDestroy();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    // ------------------------------------------------------------------
    // 状态与通知
    // ------------------------------------------------------------------

    private void setState(int newState) {
        state = newState;
        updateNotification();
        mainHandler.post(() -> {
            for (Listener listener : LISTENERS) {
                try {
                    listener.onServerStateChanged(newState);
                } catch (Throwable e) {
                    Log.w(TAG, "listener failed", e);
                }
            }
        });
    }

    private void notifyAccepted(String name) {
        mainHandler.post(() -> {
            for (Listener listener : LISTENERS) {
                listener.onInterfaceAccepted(name);
            }
            updateNotification();
        });
    }

    private void notifyAcceptFailed(String name) {
        mainHandler.post(() -> {
            for (Listener listener : LISTENERS) {
                listener.onInterfaceAcceptFailed(name);
            }
        });
    }

    private void notifyBindFailed(int port) {
        mainHandler.post(() -> {
            for (Listener listener : LISTENERS) {
                listener.onBindFailed(port);
            }
        });
    }

    private void notifyLocalOOM(int created, int needed) {
        mainHandler.post(() -> {
            for (Listener listener : LISTENERS) {
                listener.onOutOfMemory(created, needed);
            }
        });
    }

    private void notifyRemoteOOM() {
        mainHandler.post(() -> {
            for (Listener listener : LISTENERS) {
                listener.onRemoteOutOfMemory();
            }
        });
    }

    private void notifyError(String message) {
        mainHandler.post(() -> {
            for (Listener listener : LISTENERS) {
                listener.onServerError(message);
            }
        });
    }

    private void createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            NotificationChannel channel = new NotificationChannel(CHANNEL_ID,
                    getString(R.string.notification_channel_name), NotificationManager.IMPORTANCE_LOW);
            channel.setDescription(getString(R.string.notification_channel_description));
            channel.setShowBadge(false);
            NotificationManager manager = getSystemService(NotificationManager.class);
            if (manager != null) {
                manager.createNotificationChannel(channel);
            }
        }
    }

    private Notification buildNotification() {
        Intent openIntent = new Intent(this, MainActivity.class);
        openIntent.setFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TOP);
        PendingIntent contentIntent = PendingIntent.getActivity(this, 0, openIntent,
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);

        Intent stopIntent = new Intent(this, ServerService.class);
        stopIntent.setAction(ACTION_STOP);
        PendingIntent stopPendingIntent;
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            stopPendingIntent = PendingIntent.getForegroundService(this, 1, stopIntent,
                    PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        } else {
            stopPendingIntent = PendingIntent.getService(this, 1, stopIntent,
                    PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        }

        String title;
        String text;
        if (state == STATE_CONNECTED) {
            List<String> names = getConnectedInterfaceNames();
            title = getString(R.string.notification_connected_title);
            text = getString(R.string.notification_connected_text, TextUtils.join(", ", names));
        } else if (state == STATE_STARTING) {
            title = getString(R.string.notification_waiting_title);
            text = getString(R.string.notification_starting_text);
        } else {
            title = getString(R.string.notification_waiting_title);
            text = getString(R.string.notification_waiting_text);
        }

        return new NotificationCompat.Builder(this, CHANNEL_ID)
                .setSmallIcon(R.drawable.baseline_upload_24)
                .setContentTitle(title)
                .setContentText(text)
                .setStyle(new NotificationCompat.BigTextStyle().bigText(text))
                .setContentIntent(contentIntent)
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .setPriority(NotificationCompat.PRIORITY_LOW)
                .addAction(0, getString(R.string.notification_stop), stopPendingIntent)
                .build();
    }

    private void updateNotification() {
        try {
            NotificationManager manager = (NotificationManager) getSystemService(NOTIFICATION_SERVICE);
            if (manager != null) {
                manager.notify(NOTIFICATION_ID, buildNotification());
            }
        } catch (Throwable e) {
            Log.w(TAG, "updateNotification failed", e);
        }
    }

    private void goForeground() {
        Notification notification = buildNotification();
        try {
            if (Build.VERSION.SDK_INT >= 34) {
                startForeground(NOTIFICATION_ID, notification,
                        ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
            } else {
                startForeground(NOTIFICATION_ID, notification);
            }
        } catch (Throwable e) {
            Log.e(TAG, "startForeground failed", e);
        }
    }

    private void stopForegroundCompat() {
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
                stopForeground(STOP_FOREGROUND_REMOVE);
            } else {
                stopForeground(true);
            }
        } catch (Throwable ignored) {
        }
    }

    // ------------------------------------------------------------------
    // 工具方法
    // ------------------------------------------------------------------

    private static ArrayList<String> serialize(List<ServerNetInterface> list) {
        ArrayList<String> result = new ArrayList<>();
        if (list == null) {
            return result;
        }
        for (ServerNetInterface item : list) {
            String bind = item.clientBindAddress == null ? "" : item.clientBindAddress.getHostAddress();
            result.add(item.name + "|" + item.address.getHostAddress() + "|" + bind);
        }
        return result;
    }

    private static List<ServerNetInterface> deserialize(ArrayList<String> list) {
        List<ServerNetInterface> result = new ArrayList<>();
        if (list == null) {
            return result;
        }
        for (String entry : list) {
            String[] parts = entry.split("\\|", -1);
            if (parts.length < 3) {
                continue;
            }
            try {
                InetAddress address = InetAddress.getByName(parts[1]);
                InetAddress bind = TextUtils.isEmpty(parts[2]) ? null : InetAddress.getByName(parts[2]);
                result.add(new ServerNetInterface(parts[0], address, bind));
            } catch (UnknownHostException e) {
                Log.w(TAG, "invalid interface entry: " + entry, e);
            }
        }
        return result;
    }
}