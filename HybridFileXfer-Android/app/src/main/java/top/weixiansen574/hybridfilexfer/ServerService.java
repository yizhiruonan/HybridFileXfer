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
import java.nio.channels.SocketChannel;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

import rikka.shizuku.Shizuku;
import top.weixiansen574.hybridfilexfer.aidl.IIOService;
import top.weixiansen574.hybridfilexfer.core.ControllerIdentifiers;
import top.weixiansen574.hybridfilexfer.core.bean.ServerNetInterface;
import top.weixiansen574.hybridfilexfer.core.bean.TrafficInfo;
import top.weixiansen574.hybridfilexfer.droidcore.HFXServer;
import top.weixiansen574.hybridfilexfer.droidcore.ServerListener;
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
    /** 限速 MB/s，随启动 intent 传给 :IOService 进程（静态字段跨进程不共享） */ public static final String EXTRA_SPEED_LIMIT = "speed_limit_mbps";

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
    private GlobalProgress globalProgress;

    /** 通知栏点回来时要求重新弹出接收进度窗口（Activity 在 onNewIntent/onCreate 里置位） */
    private static volatile boolean pendingShowTransfer;

    public static void requestShowTransfer() {
        pendingShowTransfer = true;
    }

    public static boolean consumeShowTransfer() {
        boolean value = pendingShowTransfer;
        pendingShowTransfer = false;
        return value;
    }
    /** 应用是否在前台（在前台时用应用内浮层，不再叠全局悬浮窗） */
    public static volatile boolean appVisible = false;

    private IIOService ioService;
    private Thread loopThread;
    private volatile boolean active;
    private volatile boolean bound;
    /** 共享监听套接字的分类器；只有 accept 循环线程会用它 */
    private volatile ServerListener listener;
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
        return HFXServer.count() > 0;
    }

    /** 只看有没有连接的地方用（分享面板等旧界面）；多台电脑时取第一台。 */
    public static HFXServer getServer() {
        return HFXServer.getAny();
    }

    /**
     * 取消某台电脑上正在进行的传输。
     *
     * <p>304：取消<b>不再拆会话</b>。以前这里是"取消 = 断开这条连接"，电脑端看到断线、
     * 报「连接已断开」再重连，手机端传输页也随连接一起卸载回主页 —— 取消完还得等一个
     * 来回才能再传。现在只关传输通道、控制通道留着，电脑端收尾完把通道重建回来，
     * 会话不断，取消后立刻能再传。
     */
    public static void cancelTransfer(int connectionId) {
        HFXServer target = HFXServer.getById(connectionId);
        if (target == null) {
            return;
        }
        try {
            // 先叫醒可能正停在暂停里的线程，再关传输通道
            target.resumeTransfer();
            // 标记"用户取消"：收尾时不要自动删半成品，交给界面问用户保不保留
            liveFor(connectionId).userCancelled = true;
            // 只关传输通道，让传输线程抛异常退出；控制通道和缓冲区都留给会话线程收尾，
            // 这里绝不能调 disconnect()（会 free 掉正在用的 native 内存）
            target.abortTransfer();
        } catch (Throwable ignored) {
        }
    }

    public static void pauseTransfer(int connectionId) {
        HFXServer target = HFXServer.getById(connectionId);
        if (target != null) {
            target.pauseTransfer();
        }
    }

    public static void resumeTransfer(int connectionId) {
        HFXServer target = HFXServer.getById(connectionId);
        if (target != null) {
            target.resumeTransfer();
        }
    }

    public static boolean isTransferPaused(int connectionId) {
        HFXServer target = HFXServer.getById(connectionId);
        return target != null && target.isTransferPaused();
    }

    /** 按连接号取指定的那台电脑；那台已经断开时退回任意一台（-1 也表示任意一台）。 */
    public static HFXServer getServer(int connectionId) {
        HFXServer server = HFXServer.getById(connectionId);
        return server != null ? server : HFXServer.getAny();
    }

    /** 当前已连接的所有电脑，按连接顺序排列。 */
    public static List<HFXServer> getConnections() {
        return HFXServer.snapshot();
    }

    public static int getConnectionCount() {
        return HFXServer.count();
    }

    /** 所有电脑的通道总数 */
    public static int getConnectedChannelCount() {
        int count = 0;
        for (HFXServer server : HFXServer.snapshot()) {
            try {
                count += server.getConnectionListINames().size();
            } catch (Throwable ignored) {
            }
        }
        return count;
    }

    public static List<String> getInterfaceNames() {
        return interfaceNames;
    }

    /** 所有电脑的通道明细，按"电脑 A · 通道名\t地址"聚合（不同电脑的同名通道靠前缀区分）。 */
    public static List<String> getChannelDetails() {
        List<String> details = new ArrayList<>();
        for (HFXServer server : HFXServer.snapshot()) {
            try {
                for (String detail : server.getChannelDetails()) {
                    int tab = detail.indexOf('\t');
                    String channel = tab >= 0 ? detail.substring(0, tab) : detail;
                    String rest = tab >= 0 ? detail.substring(tab) : "";
                    details.add(server.getDisplayName() + " · " + channel + rest);
                }
            } catch (Throwable ignored) {
            }
        }
        return details;
    }

    /**
     * 每台电脑一份的推送进度（界面轮询读取）。
     * 以前是全局唯一的一份，多台电脑会互相覆盖；现在按连接号隔离。
     */
    public static final class Live {
        public volatile int connectionId;
        public volatile boolean active = false;
        public volatile String name = "";
        public volatile long done = 0;
        public volatile long total = 0;
        /** 是用户主动点的取消（要问"保不保留已接收的部分"），不是掉线 */
        public volatile boolean userCancelled = false;
        private volatile long lastDone = 0;
        /** 每一条传输通道的累计字节 / 当前速度，进度浮层的明细表用 */
        private final java.util.Map<String, ChannelLive> channels = new java.util.LinkedHashMap<>();

        void reset() {
            active = false;
            name = "";
            done = 0;
            total = 0;
            lastDone = 0;
            userCancelled = false;
            synchronized (this) {
                channels.clear();
            }
        }

        synchronized void updateSpeed(java.util.List<TrafficInfo> list) {
            for (TrafficInfo info : list) {
                if (info == null || info.iName == null) {
                    continue;
                }
                ChannelLive channel = channels.get(info.iName);
                if (channel == null) {
                    channel = new ChannelLive(info.iName);
                    channels.put(info.iName, channel);
                }
                channel.speed = info.downloadTraffic;
                channel.bytes += info.downloadTraffic;
            }
        }

        synchronized void finishChannel(String name) {
            ChannelLive channel = channels.get(name);
            if (channel != null) {
                channel.speed = 0;
                channel.finished = true;
            }
        }

        public synchronized java.util.List<ChannelLive> channelSnapshot() {
            return new ArrayList<>(channels.values());
        }
    }

    /** 一条传输通道的进度（浮层表格的一行） */
    public static final class ChannelLive {
        public final String name;
        public volatile long bytes;
        public volatile long speed;
        public volatile boolean finished;

        ChannelLive(String name) {
            this.name = name;
        }
    }

    private static final ConcurrentHashMap<Integer, Live> LIVES = new ConcurrentHashMap<>();
    /** 所有连接累计接收的字节数（只增不减），界面据此算"合计速率" */
    private static final AtomicLong RX_TOTAL = new AtomicLong();

    public static Live liveFor(int connectionId) {
        Live live = LIVES.get(connectionId);
        if (live == null) {
            Live created = new Live();
            created.connectionId = connectionId;
            Live existing = LIVES.putIfAbsent(connectionId, created);
            live = existing != null ? existing : created;
        }
        return live;
    }

    /** 当前正在接收的电脑数量 */
    public static int transferringCount() {
        int count = 0;
        for (Live live : LIVES.values()) {
            if (live.active) {
                count++;
            }
        }
        return count;
    }

    /** 最活跃的一台（还没传完的优先），没有则返回 null */
    public static Live currentTransfer() {
        Live best = null;
        for (Live live : LIVES.values()) {
            if (!live.active) {
                continue;
            }
            boolean unfinished = live.total <= 0 || live.done < live.total;
            boolean bestUnfinished = best != null && (best.total <= 0 || best.done < best.total);
            if (best == null
                    || (unfinished && !bestUnfinished)
                    || (unfinished == bestUnfinished && live.done > best.done)) {
                best = live;
            }
        }
        return best;
    }

    public static long receivedTotalBytes() {
        return RX_TOTAL.get();
    }

    /** MIUI 会吞应用日志，调试信息直接写文件（仅 debug 构建） */
    static void debugFile(String line) {
        if (!BuildConfig.DEBUG) {
            return;
        }
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
        final java.io.File dir = ensurePushDir();
        final Live live = liveFor(server.getId());
        try {
            debugFile("pull start, files=" + files.size());
            live.reset();
            live.active = true;
            server.blockProgress.reset();
            for (top.weixiansen574.hybridfilexfer.core.bean.RemoteFile f : files) {
                debugFile("  pending: " + f.getPath() + " (" + f.getSize() + ")");
            }
            if (dir == null) {
                Log.w(TAG, "cannot create push dir");
                live.active = false;
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
                            // 记录"哪个通道送了第几块"，进度浮层按它画 IDM 那种动态分段
                            if (targetSize > 0) {
                                int index = (int) ((targetSize - 1) / top.weixiansen574.hybridfilexfer.core.FileBlock.BLOCK_SIZE);
                                server.blockProgress.onBlock(iName, path, index, totalSize);
                            } else {
                                server.blockProgress.onBlock(iName, path, 0, totalSize);
                            }
                            if (!path.equals(live.name)) {
                                // 换文件了：进度从这一块重新算
                                live.name = path;
                                live.lastDone = targetSize;
                            } else if (targetSize > live.lastDone) {
                                RX_TOTAL.addAndGet(targetSize - live.lastDone);
                            }
                            live.lastDone = Math.max(live.lastDone, targetSize);
                            live.done = targetSize;
                            live.total = totalSize;
                        }
                        public void onSpeedInfo(java.util.List<top.weixiansen574.hybridfilexfer.core.bean.TrafficInfo> list) {
                            live.updateSpeed(list);
                        }
                        public void onChannelComplete(String iName, long traffic, long time) {
                            live.finishChannel(iName);
                        }
                        public void onChannelError(String iName, int errorType, String message) {
                            live.finishChannel(iName);
                            debugFile("channel error " + iName + " type=" + errorType + " " + message);
                        }
                        public void onReadFileError(String message) {
                            debugFile("read file error: " + message);
                        }
                        public void onWriteFileError(String message) {
                            debugFile("WRITE FILE ERROR: " + message);
                            live.active = false;
                        }
                        public void onComplete(boolean isUpload, long traffic, long time) {
                            debugFile("complete: " + traffic + " bytes -> " + dir);
                            live.active = false;
                        }
                        public void onIncomplete() {
                            debugFile("incomplete");
                            live.active = false;
                            if (live.userCancelled) {
                                collectPartials(dir, files);
                            } else {
                                deletePartial(dir, files);
                            }
                        }
                    });
            debugFile("pulled " + files.size() + " file(s) from PC");
            live.active = false;
        } catch (Throwable e) {
            debugFile("pull failed: " + e);
            if (live.userCancelled) {
                // 用户主动取消：先别删，等界面问"保留还是删除"
                collectPartials(dir, files);
            } else {
                // 掉线/出错：预分配到完整大小的半成品没有用，删掉
                deletePartial(dir, files);
            }
        }
    }

    /** 用户取消接收后留下的半成品路径（界面弹"保留/删除"用） */
    private static final java.util.List<String> CANCELLED_PARTIALS =
            java.util.Collections.synchronizedList(new java.util.ArrayList<>());

    public static java.util.List<String> takeCancelledPartials() {
        synchronized (CANCELLED_PARTIALS) {
            if (CANCELLED_PARTIALS.isEmpty()) {
                return java.util.Collections.emptyList();
            }
            java.util.List<String> copy = new java.util.ArrayList<>(CANCELLED_PARTIALS);
            CANCELLED_PARTIALS.clear();
            return copy;
        }
    }

    private static void collectPartials(java.io.File dir,
            java.util.List<top.weixiansen574.hybridfilexfer.core.bean.RemoteFile> files) {
        if (dir == null || files == null) {
            return;
        }
        for (top.weixiansen574.hybridfilexfer.core.bean.RemoteFile file : files) {
            String path = file.getPath();
            int cut = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'));
            String name = cut >= 0 ? path.substring(cut + 1) : path;
            if (name.isEmpty()) {
                continue;
            }
            java.io.File target = new java.io.File(dir, name);
            if (target.isFile()) {
                CANCELLED_PARTIALS.add(target.getAbsolutePath());
            }
        }
    }

    /** 电脑推送的保存目录（不存在就建）；建不出来返回 null */
    private java.io.File ensurePushDir() {
        String configured = Config.getInstance(this).getPushDir();
        java.io.File dir = configured != null && !configured.isEmpty()
                ? new java.io.File(configured)
                : new java.io.File(
                        android.os.Environment.getExternalStoragePublicDirectory(
                                android.os.Environment.DIRECTORY_DOWNLOADS),
                        "HybridFileXfer");
        if (!dir.exists() && !dir.mkdirs()) {
            return null;
        }
        return dir;
    }

    /** 删掉这次推送里已经落了一半的文件（取消/断线时用） */
    private static void deletePartial(java.io.File dir,
            java.util.List<top.weixiansen574.hybridfilexfer.core.bean.RemoteFile> files) {
        if (dir == null || files == null) {
            return;
        }
        for (top.weixiansen574.hybridfilexfer.core.bean.RemoteFile file : files) {
            String path = file.getPath();
            int cut = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'));
            String name = cut >= 0 ? path.substring(cut + 1) : path;
            if (name.isEmpty()) {
                continue;
            }
            java.io.File target = new java.io.File(dir, name);
            if (target.isFile()) {
                if (!target.delete()) {
                    debugFile("cannot delete partial " + target);
                } else {
                    debugFile("deleted partial " + target);
                }
            }
        }
    }

    /**
     * 所有已连接电脑的通道名（去重、保持顺序）。
     * 旧界面（网卡列表、通知栏）只关心"哪些线路连上了"，不需要归属；
     * 需要区分电脑时用 {@link #getChannelDetails()}。
     */
    public static List<String> getConnectedInterfaceNames() {
        java.util.LinkedHashSet<String> names = new java.util.LinkedHashSet<>();
        for (HFXServer server : HFXServer.snapshot()) {
            try {
                names.addAll(server.getConnectionListINames());
            } catch (Throwable ignored) {
            }
        }
        return new ArrayList<>(names);
    }

    /** 通知栏/状态卡用的一句话摘要 */
    public static String getConnectionSummary() {
        int computers = HFXServer.count();
        if (computers <= 0) {
            return "";
        }
        if (computers == 1) {
            return TextUtils.join(", ", getConnectedInterfaceNames());
        }
        return computers + " 台电脑 · " + getConnectedChannelCount() + " 条通道";
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
        intent.putExtra(EXTRA_SPEED_LIMIT, Config.getInstance(context).getSpeedLimitMbps());
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
     * 电脑重新接上时调用。
     *
     * <p>以前这里会断开唯一的连接、重新监听；多台电脑时那样做会把别的电脑一起踢掉。
     * 现在只清理已经失效的会话，正常的连接交给心跳自然判断。
     */
    private void requestFresh() {
        for (HFXServer server : HFXServer.snapshot()) {
            if (!server.isAlive()) {
                try {
                    server.disconnect();
                } catch (Throwable ignored) {
                }
            }
        }
        if (HFXServer.count() == 0 && state == STATE_CONNECTED) {
            setState(STATE_WAITING);
        }
    }

    // ------------------------------------------------------------------
    // 生命周期
    // ------------------------------------------------------------------

    @Override
    public void onCreate() {
        super.onCreate();
        config = Config.getInstance(this);
        // 限速是 SpeedLimiter 的静态字段，按进程隔离：本服务跑在独立进程 `:IOService`，
        // 界面进程里的 applySpeedLimit()/静态 startServer() 设的值传不过来。
        // 所以必须在这里（服务自己的进程）按保存的配置再设一次，否则限速永远是 0。
        top.weixiansen574.hybridfilexfer.core.SpeedLimiter.setLimitBytesPerSecond(
                config.getSpeedLimitMbps() * 1024L * 1024L);
        createNotificationChannel();
        goForeground();
        // 局域网广播：让电脑不用 adb、不用手动输 IP 也能发现手机
        discoveryBeacon = new DiscoveryBeacon(this, config.getServerPort());
        discoveryBeacon.start();
        // 全局接收进度（悬浮窗 + 通知栏）
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        // 系统要求 startForegroundService() 之后 5 秒内必须 startForeground()；
        // onCreate 只在第一次调用，服务已经在跑时再被 start（电脑每次连都会用 adb 拉起）
        // 就必须再喊一次，否则进程会被系统以 "did not then call startForeground" 杀掉，
        // 正在传的连接也会跟着断（表现就是"会话不稳定、老是断开重连"）
        goForeground();
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
            // 限速值随 intent 进服务进程（静态字段跨进程不共享）；没带就用存好的配置
            int speedLimitMbps = intent.getIntExtra(EXTRA_SPEED_LIMIT, -1);
            top.weixiansen574.hybridfilexfer.core.SpeedLimiter.setLimitBytesPerSecond(
                    (speedLimitMbps >= 0 ? speedLimitMbps : config.getSpeedLimitMbps()) * 1024L * 1024L);
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
        // 全局悬浮窗 / 通知栏进度的 tick 从这里起（之前只写在 onBindingDied 里，
        // 正常启动时根本不会跑，所以应用外的进度弹窗一直不出现）
        ensureGlobalProgress();
        bindIoService(mode);
    }

    /** 服务开始工作就起全局进度 tick（已存在就不重建） */
    private void ensureGlobalProgress() {
        if (globalProgress != null) {
            return;
        }
        try {
            globalProgress = new GlobalProgress(this, mainHandler);
            globalProgress.start();
        } catch (Throwable e) {
            Log.w(TAG, "global progress start failed", e);
            globalProgress = null;
        }
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

    /**
     * 监听循环：绑定一次端口，然后不断接连接、跑握手，给每台电脑开一条会话线程。
     *
     * <p>握手是串行的：所有电脑的控制通道和传输通道都连同一个端口，只有同一个线程
     * 在 accept，才能保证不会把别台电脑的控制通道错当成当前连接的传输通道
     * （分类和暂存由 {@link ServerListener} 负责）。
     */
    private void serverLoop(int gen) {
        int port = config.getServerPort();
        ServerListener listener = null;
        while (active && gen == generation) {
            try {
                listener = new ServerListener(HFXServer.bindShared(port));
                break;
            } catch (IOException e) {
                Log.w(TAG, "bind failed on port " + port, e);
                notifyBindFailed(port);
                SystemClock.sleep(1000);
            }
        }
        if (listener == null || gen != generation) {
            if (gen == generation) {
                setState(STATE_STOPPED);
            }
            return;
        }
        this.listener = listener;
        setState(STATE_WAITING);
        try {
            while (active && gen == generation) {
                ServerListener.PendingControl control;
                try {
                    control = listener.nextControl(500);
                } catch (IOException e) {
                    if (!active || gen != generation) {
                        break;
                    }
                    Log.w(TAG, "accept failed", e);
                    SystemClock.sleep(300);
                    continue;
                }
                if (control == null) {
                    continue;
                }
                int limit = maxConnections();
                if (HFXServer.count() >= limit) {
                    // 先清掉已经死掉但还没被心跳回收的连接（电脑重启后常见）
                    dropDeadSessions();
                }
                if (HFXServer.count() >= limit) {
                    debugFile("refuse connection: already " + HFXServer.count() + " of " + limit);
                    closeQuietly(control.socket);
                    continue;
                }
                HFXServer server = new HFXServer(ioService);
                HandshakeGuard guard = new HandshakeGuard(server, control.socket);
                boolean connected;
                try {
                    connected = server.startServer(listener, control, interfaces, localBufferCount,
                            remoteBufferCount, newSessionCallback(server));
                } catch (Throwable e) {
                    Log.w(TAG, "handshake failed", e);
                    debugFile("handshake failed: " + e);
                    connected = false;
                } finally {
                    guard.cancel();
                }
                if (connected && gen == generation) {
                    setState(STATE_CONNECTED);
                    Thread session = new Thread(() -> sessionLoop(server, gen), "HFX-Session-" + server.getId());
                    session.start();
                } else {
                    try {
                        server.disconnect();
                    } catch (Throwable ignored) {
                    }
                }
            }
        } finally {
            listener.close();
            if (this.listener == listener) {
                this.listener = null;
            }
        }
        if (gen == generation) {
            setState(STATE_STOPPED);
        }
    }

    /** 一台电脑一条会话线程：心跳 + 暂停同步 + 拉取电脑推送的文件。断开只影响这一台。 */
    private void sessionLoop(HFXServer server, int gen) {
        int id = server.getId();
        // 重建传输通道要 accept（只能由 accept 线程做），这里拿的是"挂号"用的监听器
        ServerListener acceptListener = this.listener;
        try {
            long lastPing = SystemClock.elapsedRealtime();
            while (active && gen == generation && server.isAlive()) {
                SystemClock.sleep(250);
                if (!active || gen != generation || !server.isAlive()) {
                    break;
                }
                // 304：用户取消后电脑会把传输通道重建回来。这一步必须排在暂停同步前面 ——
                // 重建期间控制通道归这条线程读，再去跑 exchangePause 就会把
                // REBUILD_REQ 读成"意外的暂停回报"，一次擦肩而过就把会话打掉了。
                if (server.isNeedsRebuild()) {
                    if (!rebuildTransfers(server, acceptListener)) {
                        Log.i(TAG, "rebuild failed (" + server.getDisplayName() + "), disconnecting");
                        break;
                    }
                    // 重建刚花掉几秒，心跳重新计时，别立刻又发一条
                    lastPing = SystemClock.elapsedRealtime();
                    continue;
                }
                // 暂停状态同步：每轮（约 250ms）都做一次，传输中也要同步 ——
                // 电脑端点暂停，这边 250ms 内就跟着变；这边点暂停，电脑也会立刻收到。
                // 电脑端在传输过程中也会继续读控制通道（receiver 的 poll），不会等传完。
                //
                // 302：手机正在发送文件时，控制通道的读取权归发送线程（它要读收尾的 RESULT 帧），
                // 会话线程只能写 —— 否则两个读者会互相吃掉对方的帧，两边一起卡死。
                try {
                    // 传输期间控制通道的读权在传输线程手里，会话线程只写；
                    // 否则两个读者会把帧流读串（表现为电脑端读到 opcode 0、传输莫名中断）。
                    if (server.isSendingFiles()) {
                        server.writePauseStateOnly(server.isTransferPaused());
                    } else {
                        server.exchangePause(server.isTransferPaused());
                    }
                } catch (IOException e) {
                    Log.i(TAG, "pause sync failed (" + server.getDisplayName() + "): " + e);
                    top.weixiansen574.hybridfilexfer.core.HFXLog.d("sessionLoop: pause sync FAILED", e);
                    break;
                }
                long now = SystemClock.elapsedRealtime();
                if (now - lastPing < PING_INTERVAL_MS) {
                    continue;
                }
                lastPing = now;
                try {
                    server.writePing();
                } catch (IOException e) {
                    Log.i(TAG, "heartbeat failed (" + server.getDisplayName() + "): " + e);
                    break;
                }
                // 传输中 / 待重建时，传输通道的状态由传输线程和重建流程负责，
                // 这里既不能探死通道（取消时通道本来就是我们自己关的），
                // 也不能去拉"电脑推送的文件"（会和正在进行的传输打架）。
                if (server.isTransferring() || server.isNeedsRebuild()) {
                    continue;
                }
                // 拔数据线/掉 WiFi 时控制通道可能还活着（走的是别的链路），
                // 但某些传输通道已经被对端关掉了——探到就断开这条连接，让电脑重连，
                // 否则界面上会一直挂着已经死掉的通道。
                try {
                    if (server.hasDeadChannel()) {
                        Log.i(TAG, "dead transfer channel (" + server.getDisplayName() + "), reconnecting");
                        break;
                    }
                } catch (Throwable e) {
                    debugFile("channel probe failed: " + e);
                }
                // 问一下电脑有没有"发送到手机"推过来的文件，有就直接拉下来
                try {
                    java.util.List<top.weixiansen574.hybridfilexfer.core.bean.RemoteFile> pending =
                            server.checkPending();
                    if (!pending.isEmpty() && !server.isTransferring()) {
                        pullPendingFiles(server, pending);
                    }
                } catch (Throwable e) {
                    debugFile("checkPending failed (" + server.getDisplayName() + "): " + e);
                }
            }
        } catch (Throwable e) {
            debugFile("session failed (" + server.getDisplayName() + "): " + e);
        } finally {
            HFXServer.unregister(server);
            LIVES.remove(id);
            try {
                server.disconnect();
            } catch (Throwable ignored) {
            }
            if (gen == generation) {
                setState(HFXServer.count() > 0 ? STATE_CONNECTED : STATE_WAITING);
            }
        }
    }

    /** 304：取消后等电脑把通道重建回来的总窗口 */
    private static final long REBUILD_WINDOW_MS = 30000;
    /** 已经恢复出通道之后，多久没有新的 REBUILD_REQ 就认为电脑发完了 */
    private static final long REBUILD_IDLE_MS = 5000;

    /**
     * 304：取消之后等电脑把传输通道重建回来（会话不断）。
     *
     * <p>流程：等传输线程彻底退出（它可能还停在控制通道的结论帧上，两个读者会各吃半帧）
     * → 在监听器上挂号（这之后 accept 线程收到的安静连接不再丢，是电脑刚发起的传输通道）
     * → 一帧一帧读 REBUILD_REQ，每收到一条就向 accept 线程要一条通道，
     * 收下就回 REBUILD_ACK{true}，要不到就回 {false} 让电脑跳过这条。
     *
     * <p>帧必须带 opcode：手机随时可能在飞 PAUSE_STATE，裸 bool 握手会和它的帧头撞车，
     * 帧流一错位整条会话就报废（这就是 304 不用"裸 bool 重建握手"的原因）。
     *
     * @return true 表示至少恢复了一条通道（会话继续）；false 表示该断线重连了
     */
    private boolean rebuildTransfers(HFXServer server, ServerListener listener) {
        if (listener == null) {
            debugFile("rebuild: no listener (" + server.getDisplayName() + ")");
            return false;
        }
        // 1) 等传输线程交出控制通道的读权
        long waitDeadline = System.currentTimeMillis() + 15000;
        while ((server.isTransferring() || server.isSendingFiles())
                && System.currentTimeMillis() < waitDeadline) {
            SystemClock.sleep(50);
        }
        if (server.isTransferring() || server.isSendingFiles()) {
            // 传输线程还没退出就抢控制通道 = 两个读者各吃半帧，整条会话必废。
            // 宁可断线重连，也不要把帧流搞错位。
            debugFile("rebuild: transfer thread still owns the control channel");
            return false;
        }
        int want = server.beginRebuild();
        if (want <= 0) {
            debugFile("rebuild: no channel to restore (" + server.getDisplayName() + ")");
            return false;
        }
        debugFile("rebuild: waiting for " + want + " channel(s) from " + server.getDisplayName());
        listener.wantTransfers(want);
        int got = 0;
        long deadline = System.currentTimeMillis() + REBUILD_WINDOW_MS;
        long idleUntil = Long.MAX_VALUE;
        try {
            while (got < want) {
                long now = System.currentTimeMillis();
                if (now > deadline || now > idleUntil) {
                    break;
                }
                // 别的读循环提前读到的重建请求（exchangePause 兜底里收的）
                String name = server.pollRebuildName();
                if (name == null) {
                    short opcode;
                    try {
                        opcode = server.readControlOpcode(1000);
                    } catch (IOException e) {
                        debugFile("rebuild: control channel gone: " + e);
                        return false;
                    }
                    if (opcode == HFXServer.CONTROL_TIMEOUT) {
                        continue;
                    }
                    if (opcode == ControllerIdentifiers.PING
                            || opcode == ControllerIdentifiers.CANCEL
                            || opcode == ControllerIdentifiers.REBUILD_ACK
                            || opcode == ControllerIdentifiers.RESULT) {
                        // 心跳 / 重复的取消帧：读到就行。
                        // RESULT 是电脑在取消前一刻已经写出来的收尾结论（载荷已在
                        // readControlOpcode 的锁里读掉丢掉），不能让它打断重建。
                        continue;
                    }
                    if (opcode == ControllerIdentifiers.PAUSE_REPLY) {
                        // 载荷（电脑是否暂停）已经在 readControlOpcode 的锁里读掉并应用了
                        continue;
                    }
                    if (opcode != ControllerIdentifiers.REBUILD_REQ) {
                        // 不是重建握手：帧序已经乱了，别硬撑，交给会话重连
                        debugFile("rebuild: unexpected opcode " + opcode);
                        return false;
                    }
                    // 通道名已经在 readControlOpcode 的锁里读出来存好了
                    name = server.pollRebuildName();
                    if (name == null) {
                        debugFile("rebuild: REBUILD_REQ without channel name");
                        continue;
                    }
                }
                SocketChannel socket = listener.takeTransferChannel(12000);
                boolean accepted = socket != null;
                try {
                    if (accepted) {
                        server.addRebuiltConnection(name, socket);
                    }
                    server.writeRebuildAck(accepted);
                } catch (IOException e) {
                    debugFile("rebuild: ack failed: " + e);
                    return got > 0;
                }
                if (accepted) {
                    got++;
                    idleUntil = System.currentTimeMillis() + REBUILD_IDLE_MS;
                    debugFile("rebuild: channel " + name + " restored (" + got + "/" + want + ")");
                } else {
                    debugFile("rebuild: channel " + name + " refused (no socket)");
                }
            }
        } finally {
            listener.endWantTransfers();
            server.setNeedsRebuild(false);
        }
        if (got > 0) {
            Log.i(TAG, "rebuilt " + got + "/" + want + " channel(s) with " + server.getDisplayName());
            debugFile("rebuild: done " + got + "/" + want);
        }
        return got > 0;
    }

    private StartServerCallback newSessionCallback(HFXServer server) {
        return new StartServerCallback() {
            @Override
            public void onBindFailed(int port) {
            }

            @Override
            public void onStatedServer() {
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
            }

            @Override
            public void onError(Throwable th) {
                debugFile("session error (" + server.getDisplayName() + "): " + th);
            }
        };
    }

    private void dropDeadSessions() {
        for (HFXServer server : HFXServer.snapshot()) {
            if (!server.isAlive()) {
                try {
                    server.disconnect();
                } catch (Throwable ignored) {
                }
            }
        }
    }

    private int maxConnections() {
        try {
            return Math.max(1, config.getMaxConnections());
        } catch (Throwable e) {
            return 1;
        }
    }

    private static void closeQuietly(SocketChannel socket) {
        if (socket == null) {
            return;
        }
        try {
            socket.close();
        } catch (IOException ignored) {
        }
    }

    /** 握手卡死时的兜底：超时后关掉这条控制通道，免得把 accept 循环堵住。 */
    private final class HandshakeGuard implements Runnable {
        private final HFXServer server;
        private final SocketChannel control;
        private final AtomicBoolean done = new AtomicBoolean(false);

        HandshakeGuard(HFXServer server, SocketChannel control) {
            this.server = server;
            this.control = control;
            mainHandler.postDelayed(this, 20000);
        }

        @Override
        public void run() {
            if (done.get()) {
                return;
            }
            Log.w(TAG, "handshake timed out, dropping " + server.getDisplayName());
            debugFile("handshake timed out");
            closeQuietly(control);
        }

        void cancel() {
            done.set(true);
            mainHandler.removeCallbacks(this);
        }
    }

    private void stopLoop(boolean unbind) {
        generation++;
        active = false;
        ServerListener current = listener;
        listener = null;
        if (current != null) {
            current.close();
        }
        // 关闭共享监听套接字：accept 循环会被唤醒并退出
        HFXServer.closeShared();
        // 停止/改配置时所有电脑的会话都结束，让电脑端重连
        HFXServer.disconnectAll();
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
    }

    private void stopEverything() {
        active = false;
        stopLoop(true);
        setState(STATE_STOPPED);
        stopForegroundCompat();
    }

    @Override
    public void onDestroy() {
        if (globalProgress != null) {
            globalProgress.destroy();
            globalProgress = null;
        }
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
        Intent openIntent = new Intent(this, top.weixiansen574.hybridfilexfer.compose.MiuixMainActivity.class);
        openIntent.setFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TOP);
        // 点通知 → 回到应用并重新弹出接收进度窗口
        openIntent.putExtra("hfx_show_transfer", true);
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
            title = getString(R.string.notification_connected_title);
            if (HFXServer.count() > 1) {
                text = getString(R.string.notification_connected_multi,
                        HFXServer.count(), getConnectedChannelCount());
            } else {
                List<String> names = getConnectedInterfaceNames();
                text = getString(R.string.notification_connected_text, TextUtils.join(", ", names));
            }
        } else if (state == STATE_STARTING) {
            title = getString(R.string.notification_waiting_title);
            text = getString(R.string.notification_starting_text);
        } else {
            title = getString(R.string.notification_waiting_title);
            text = getString(R.string.notification_waiting_text);
        }

        NotificationCompat.Builder builder = new NotificationCompat.Builder(this, CHANNEL_ID)
                .setSmallIcon(R.drawable.baseline_upload_24)
                .setContentTitle(title)
                .setContentText(text)
                .setStyle(new NotificationCompat.BigTextStyle().bigText(text))
                .setContentIntent(contentIntent)
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .setPriority(NotificationCompat.PRIORITY_LOW)
                .addAction(0, getString(R.string.notification_stop), stopPendingIntent);
        // 正在接收时通知栏也显示进度（全局可见，不依赖悬浮窗权限）
        Live transfer = currentTransfer();
        if (transfer != null && transfer.total > 0) {
            long done = transfer.done;
            HFXServer server = HFXServer.getById(transfer.connectionId);
            if (server != null) {
                done = Math.max(done, server.blockProgress.getDoneBytes());
            }
            int pct = (int) Math.max(0, Math.min(100, done * 100 / transfer.total));
            String name = transfer.name;
            int cut = Math.max(name.lastIndexOf('/'), name.lastIndexOf('\\'));
            if (cut >= 0) {
                name = name.substring(cut + 1);
            }
            long speedSum = 0;
            for (ChannelLive channel : transfer.channelSnapshot()) {
                speedSum += channel.speed;
            }
            // 通知栏内容以「接收速率」为主
            builder.setContentTitle(getString(R.string.notification_receiving) + " · " + pct + "%")
                    .setContentText("接收速率 " + GlobalProgress.formatSpeed(speedSum)
                            + "  ·  " + GlobalProgress.formatSize(done) + " / "
                            + GlobalProgress.formatSize(transfer.total))
                    .setProgress(100, pct, false);
        }
        return builder.build();
    }

    /** 全局进度每秒钟刷新一次通知栏 */
    void refreshNotification() {
        updateNotification();
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