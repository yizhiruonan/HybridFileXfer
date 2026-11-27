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
import android.net.ConnectivityManager;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.NetworkRequest;
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

    /** 网卡变化的压抖窗口：一次 WiFi 切换会连来好几条回调，等系统状态稳下来再处理 */
    private static final long NETWORK_REFRESH_DELAY_MS = 800;

    public static final String ACTION_START = "top.weixiansen574.hybridfilexfer.action.START_SERVER";
    public static final String ACTION_STOP = "top.weixiansen574.hybridfilexfer.action.STOP_SERVER";
    /**
     * 308：界面改了限速，把新值带给服务。走的是和 {@link #ACTION_START} 同一条路
     * （startForegroundService + intent extra），不另造一套 IPC —— 限速是 SpeedLimiter
     * 的静态字段，按进程隔离，界面进程里设的值传不过服务进程。
     */
    public static final String ACTION_SPEED_LIMIT = "top.weixiansen574.hybridfilexfer.action.SPEED_LIMIT";
    /**
     * 308：共用限速值定下来了（握手定下来的 / 电脑推来的），服务广播给界面。
     *
     * <p>为什么要广播：界面进程有它自己的 Config 单例和 SpeedLimiter，服务这边改了
     * 它看不见，「连接」页的限速一栏会一直显示旧值。只在本应用内发（setPackage），
     * 不外泄。
     */
    public static final String ACTION_SPEED_LIMIT_SYNCED =
            "top.weixiansen574.hybridfilexfer.SPEED_LIMIT_SYNCED";
    /** 广播里带的共用限速值（MB/s，0 = 不限速） */
    public static final String EXTRA_SPEED_LIMIT_SYNCED = "mbps";

    // ---- 通知卡片上的任务级按钮 ----
    //
    // 为什么是「通知卡片上的按钮」而不是长按菜单里的项：Android 13+ 长按通知弹出的那个菜单
    // 完全由系统提供（设置 / 关闭通知 / 静音…），第三方应用**没有任何接口能往里加自己的项**。
    // 应用能加的只有 Notification.Builder.addAction() 这几颗按钮：卡片收起时只显示前 3 个
    // （展开后全部显示），所以下面最多挂 3 颗，把「停止服务」那颗让给任务按钮。
    /** 暂停当前正在传的那个任务 */
    public static final String ACTION_TASK_PAUSE = "top.weixiansen574.hybridfilexfer.action.TASK_PAUSE";
    /** 继续（暂停中 / 让出通道的那个任务） */
    public static final String ACTION_TASK_RESUME = "top.weixiansen574.hybridfilexfer.action.TASK_RESUME";
    /** 取消，但保留电脑上已经收到的部分（下次同一个文件断点续传） */
    public static final String ACTION_TASK_CANCEL_KEEP = "top.weixiansen574.hybridfilexfer.action.TASK_CANCEL_KEEP";
    /** 取消，并让电脑端把这次的不完整文件和 .hfxpart 一起删掉 */
    public static final String ACTION_TASK_CANCEL_DELETE = "top.weixiansen574.hybridfilexfer.action.TASK_CANCEL_DELETE";
    /** 任务级 action 带的任务号 */
    public static final String EXTRA_TASK_ID = "task_id";
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
    /**
     * 通知 contentIntent 的 requestCode 起点（+ taskId）。
     *
     * <p>每个发送任务一条自己的点击目标：几个任务的 PendingIntent 不能共用 requestCode，
     * 否则「点任务2的通知」会被 FLAG_UPDATE_CURRENT 换成任务1的面板。
     */
    private static final int NOTIFICATION_TASK_REQUEST_BASE = 6400;
    /**
     * 任务级按钮（暂停 / 继续 / 取消保留 / 取消删除）的 requestCode 段。
     *
     * <p>四段彼此错开、也和 6400+taskId（contentIntent）、1（停止服务）错开：
     * PendingIntent 是按「requestCode + Intent 是否相等」认人的，撞了就会互相顶掉 ——
     * 点了「取消」结果开了面板，或者点了任务2的动作落到任务1上。
     */
    private static final int NOTIFICATION_PAUSE_REQUEST_BASE = 7000;
    private static final int NOTIFICATION_RESUME_REQUEST_BASE = 7500;
    private static final int NOTIFICATION_CANCEL_KEEP_REQUEST_BASE = 8000;
    private static final int NOTIFICATION_CANCEL_DELETE_REQUEST_BASE = 8500;
    /**
     * 「每个任务一条通知」的 id 段（总通知就是 {@link #NOTIFICATION_ID}，这里 5800 + taskId）。
     *
     * <p>用户要的是：<b>每个传输任务各占一条通知，都归到总通知下面</b>，下拉展开后每条自带
     * 自己的暂停 / 取消按钮，点哪条就开哪个任务的进度面板。
     */
    private static final int NOTIFY_ID_TASK_BASE = 5800;
    /** 通知分组名：子通知（每个任务一条）挂在 {@link #NOTIFICATION_ID} 这个 summary 下面 */
    private static final String GROUP_TASKS = "hfx.transfer.tasks";
    private static final String CHANNEL_ID = "server";
    private static final long PING_INTERVAL_MS = 3000;
    /**
     * 309：传输进行中，多久补发一帧 QUEUE_STATE。
     *
     * <p>队列变化时会立刻推一帧（{@code SendTasks#notifyQueueChanged}），这一秒一次是<b>兜底</b>：
     * 传输期间任务状态也会在别的地方变（让出通道、收尾的边角、电脑端取消），
     * 漏一帧不影响大局，但一直漏就会让电脑上的队列和手机对不上。
     */
    private static final long QUEUE_STATE_INTERVAL_MS = 1000;
    /** 「进度帧」的最小间隔：前台服务通知是同一个 id 反复 notify，20Hz 地刷会掉帧、发烫，500ms 一帧够了 */
    private static final long PROGRESS_NOTIFY_INTERVAL_MS = 500;

    private static final CopyOnWriteArrayList<Listener> LISTENERS = new CopyOnWriteArrayList<>();
    private static volatile int state = STATE_STOPPED;
    private static volatile List<String> interfaceNames = Collections.emptyList();

    private Config config;
    private final Handler mainHandler = new Handler(Looper.getMainLooper());

    // ---- 网卡集合变化监听（见 registerNetworkWatcher） ----
    /** 系统网卡变化回调；null = 没注册成功（老系统 / 权限异常），退回只靠界面轮询 */
    private ConnectivityManager.NetworkCallback networkCallback;
    /** 传输中撞上网卡变化：先记下来，等这一轮传完再按新网卡列表重开监听 */
    private volatile boolean interfaceRefreshPending;
    /** 上一次「想重开监听但被传输挡住」的时刻，避免每 2 秒空转刷日志 */
    private volatile long lastInterfaceRefreshTry;
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
    /**
     * 「服务正在/已经停止」。
     *
     * <p>停止流程里 setState(STATE_STOPPED) 会顺手 updateNotification()；但 onDestroy 跑的时候
     * 服务记录已经从 AMS 摘掉了，那时的 stopForeground(STOP_FOREGROUND_REMOVE) 是空操作
     * （ActiveServices.setServiceForeground 找不到 ServiceRecord 直接返回），
     * 于是这条 notify() 就把刚被系统撤掉的前台通知又贴回通知栏 —— 而且它 setOngoing(true)、
     * 没有前台服务再管它，用户就永远清不掉（报的「点停止没生效、通知没消失」）。
     * 停止流程一开始就立这面旗，之后任何 updateNotification() 一律不 post。
     */
    private volatile boolean stopping;
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

    /**
     * 最近一次「电脑的控制通道到了、握手却没走完」的时刻（{@link android.os.SystemClock#elapsedRealtime()}，0 = 从没有过）。
     *
     * <p>为什么要有它：分享面板等不到连接时只会说「请先在电脑上运行接收器」，而实测最常见的失败
     * 根本不是电脑没运行 —— 是控制通道连上了、传输通道一条都没建起来（USB_ADB 通道被占用、
     * 防火墙拦了局域网通道，都会这样）。那时用户被指去检查一件已经做对的事，日志却在另一个地方。
     * 这里只记时刻，文案由面板自己按「多久以前」判断，免得两边各存一份状态。
     */
    private static volatile long lastHandshakeAbortAt;

    public static long getLastHandshakeAbortAt() {
        return lastHandshakeAbortAt;
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
        cancelTransfer(connectionId, false);
    }

    /**
     * 取消某台电脑上正在进行的传输，并告诉电脑端「已传的部分要不要保留」。
     *
     * @param discardPartial true = 用户在取消确认框里选了「不保留进度」：
     *        多发一帧 {@code CANCEL_DISCARD}，电脑端收尾时把这次写到一半的文件和
     *        {@code <文件名>.hfxpart} 续传记录一起删掉，下次同一个文件从头传；
     *        false = 保留（默认），下次同一个文件从断点续传。
     */
    public static void cancelTransfer(int connectionId, boolean discardPartial) {
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
            target.abortTransfer(discardPartial);
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

    // ------------------------------------------------------------------
    // 分享发送任务（SendTasks）：暂停 / 继续 / 取消一律按 taskId 作用
    // ------------------------------------------------------------------

    public static void pauseTask(int taskId) {
        SendTasks.pauseTask(taskId);
    }

    public static void resumeTask(int taskId) {
        SendTasks.resumeTask(taskId);
    }

    /**
     * 取消一个分享发送任务。
     *
     * @param discardPartial true = 连电脑上那份不完整文件一起删掉（下次从头传）；
     *                       false = 保留，下次同一个文件断点续传
     */
    public static void cancelTask(int taskId, boolean discardPartial) {
        SendTasks.cancelTask(taskId, discardPartial);
    }

    public static boolean isTaskTransferring(int taskId) {
        return SendTasks.isTaskTransferring(taskId);
    }

    /** 有会话正在传文件吗（收尾也算）：这时候不要改通道列表，会把传输打断 */
    public static boolean isBusy() {
        for (HFXServer server : HFXServer.snapshot()) {
            if (server.isTransferring()) {
                return true;
            }
        }
        return false;
    }

    /**
     * 正在监听的通道是否已经覆盖了这些网卡（每个网卡至少有第一通道在跑）。
     *
     * <p>给「网卡是后来才出现的」这种场景用：手机开「蓝牙网络共享」、电脑用蓝牙连上之后
     * 才会冒出 bt-pan，后台服务已经在跑，得按新列表重启一次监听才能把蓝牙通道加进来，
     * 否则用户必须杀掉应用重开（用户要的「加上蓝牙通道」就是这个）。
     * 用户自己在设置里关掉的通道（disabled_channels）不算缺，避免反复重启。
     */
    public static boolean coversAll(Context context, List<ServerNetInterface> wanted) {
        java.util.Set<String> disabled = Config.getInstance(context).getDisabledChannels();
        List<String> running = interfaceNames;
        for (ServerNetInterface item : wanted) {
            if (item.name == null || disabled.contains(item.name)) {
                continue;
            }
            boolean found = false;
            for (String name : running) {
                if (name.equals(item.name) || name.startsWith(item.name + "#")) {
                    found = true;
                    break;
                }
            }
            if (!found) {
                return false;
            }
        }
        return true;
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

        /** 整次传输结束：所有通道的速率都要归零，否则界面上会停在最后一秒的数字上 */
        synchronized void finishAllChannels() {
            for (ChannelLive channel : channels.values()) {
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
                            // 收尾时把每通道速率清零：速率监视线程停了以后没人再更新，
                            // 不清就是"传输结束各通道还挂着 MB/s"（用户报的）
                            live.finishAllChannels();
                            live.active = false;
                        }
                        public void onIncomplete() {
                            debugFile("incomplete");
                            live.active = false;
                            if (live.userCancelled) {
                                collectPartials(dir, files);
                            } else if (server.isPeerCancelled()) {
                                // 电脑端取消：留还是删，receiveFiles 的收尾
                                // (settleCancelledReceive) 已经按电脑上用户选的
                                // 「保留 / 不保留」处理完了 —— 这里再 deletePartial
                                // 会把「保留」也删成残骸：文件没了、<文件名>.hfxpart
                                // 还在，日志说"这些块已有"、文件却不存在，
                                // 下次同名文件续传会跳过这些块，落盘是带空洞的假文件。
                                // （真机 2026-11-21 实测：选「保留进度」后只剩 288 字节的日志）
                                debugFile("peer cancelled: partials already settled by receiveFiles");
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
            } else if (server.isPeerCancelled()) {
                // 电脑端取消：和 onIncomplete 里同一个道理 —— 留还是删已经由
                // receiveFiles 的收尾决定了（而且取消时到这里是预期内的异常：
                // 传输通道被对方关掉，本端的读写会以异常收场）
                debugFile("peer cancelled: partials already settled by receiveFiles");
            } else {
                // 掉线/出错：预分配到完整大小的半成品没有用，删掉（连续传日志一起，
                // 见 deletePartial 里的说明：留孤儿日志会让下次续传出空洞）
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
            // 续传日志必须跟着一起删。只删文件会留下一个"说这些块已经有了"的孤儿日志，
            // 下次同名文件续传时按它跳过这些块 —— 文件是预分配到完整长度的，
            // 那些位置就成了空洞（大小对、内容是 0），比一份没传完的文件更害人。
            java.io.File journal = new java.io.File(target.getPath() + ".hfxpart");
            if (journal.isFile()) {
                if (!journal.delete()) {
                    debugFile("cannot delete journal " + journal);
                } else {
                    debugFile("deleted journal " + journal);
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
     * 一张网卡开几条传输通道。
     *
     * <p>以前这里是写死的「wlan/wifi 4 条、其它 2 条」，用户设置里的
     * {@link Config#getChannelsPerInterface()}（1~4）<b>一个地方都没读</b>，于是设置怎么改都不生效。
     * 现在：设过（1~4）就按设置来，没设过（0）走自动规则：<b>wlan / wifi / eth → 4 条，其它 → 2 条</b>
     * （USB 数据线、USB 网络共享 rndis0、蓝牙共享网络 bt-pan 都落在「其它」这一支）。
     *
     * <p>和改动前相比<b>唯一的差别</b>是真以太网 {@code eth0}：老代码只认 wlan/wifi，eth0 走的是
     * 「其它 → 2」；现在按有线网卡该有的样子给 4 条。{@code "rndis0".contains("eth")} 是 false，
     * 所以 USB 网络共享仍然是 2 条，手机上的 12 条通道构成（USB_ADB×2 + wlan0×4 + rndis0×2 +
     * wlan2×4）不受影响；手机没有以太网口，绝大多数用户看不到变化。
     * 界面上「预计通道数」（主页 / 连接页）也走这个函数，免得两边口径不一致。
     *
     * <p><b>故意不做设置界面</b>（评审已确认）：这个旋钮是给调试 / 特殊网络环境用的，
     * 默认的 4 / 2 已经能把链路榨满；摆进设置页只会让普通用户困惑，而对外讲的就是
     * 「自动榨干所有通道」。所以它只能由预置值或代码调
     * {@link Config#setChannelsPerInterface(int)} 写进去 —— 这不是漏了 UI。
     */
    public static int channelsPerInterface(Context context, String interfaceName) {
        int configured = Config.getInstance(context).getChannelsPerInterface();
        if (configured >= 1 && configured <= 4) {
            return configured;
        }
        String lower = interfaceName == null ? "" : interfaceName.toLowerCase(java.util.Locale.ROOT);
        return (lower.contains("wlan") || lower.contains("wifi") || lower.contains("eth")) ? 4 : 2;
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
            // 每张网卡开几条通道：用户在设置里选过（1~4）就按设置来，没设过按自动规则
            // （WIFI/以太网 4 条，USB/蓝牙 2 条）。蓝牙共享网络（bt-pan）没有特殊处理，
            // 和别的网卡一样按保存的勾选状态走（这里原来写着「默认不勾选」，其实没有任何过滤）。
            int channelsPerInterface = channelsPerInterface(context, netInterface.name);
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
        // 308：这里取的是"两端共用的那个值"（持久化里存的就是共用值），
        // 按"采纳"语义设 —— 不是用户此刻的改动，不该提代次去推给电脑。
        top.weixiansen574.hybridfilexfer.core.SpeedLimiter.applySharedBytesPerSecond(
                top.weixiansen574.hybridfilexfer.core.SpeedLimiter
                        .mbpsToBytesPerSecond(Config.getInstance(context).getSpeedLimitMbps()));
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

    /** 应用上下文（onCreate 里存下的），后台线程崩溃后重开会话要用 */
    private static volatile Context appContext;
    /** 上次因为后台线程崩溃重开会话的时间，防止一次拔线连着重启好几次 */
    private static volatile long lastCrashRestart;

    /**
     * 后台传输/会话线程里没兜住的异常（拔线时最常见的那类空指针）。
     *
     * <p>线程已经没了，但它负责的那条通道和这一轮会话都还要重来：这里重开一次会话，
     * 顺便把异常写进日志。限流 3 秒，免得一次拔线打死好几条线程时反复重启。
     */
    public static void onBackgroundThreadCrash(String threadName) {
        Log.w(TAG, "后台线程异常退出：" + threadName);
        Context context = appContext;
        if (context == null || !isRunning()) {
            return;
        }
        long now = SystemClock.elapsedRealtime();
        if (now - lastCrashRestart < 3000) {
            return;
        }
        lastCrashRestart = now;
        restartSession(context);
    }

    /**
     * 308：界面改了限速 —— 把新值交给服务进程。
     *
     * <p>为什么不直接调 SpeedLimiter：那是静态字段，按进程隔离，界面进程里设的值
     * 服务那边根本看不见（见 {@link #EXTRA_SPEED_LIMIT} 的注释）。走 intent 才是
     * 唯一能跨进程把值带过去的路，和 {@link #startServer} 用的是同一条。
     *
     * <p>服务没在跑就什么也不做：值已经写进 Config 了，下次启动照样读得到，
     * 没必要为了改个限速把一个前台服务拉起来。
     */
    public static void pushSpeedLimit(Context context, int mbps) {
        if (!isRunning()) {
            return;
        }
        Intent intent = new Intent(context, ServerService.class);
        intent.setAction(ACTION_SPEED_LIMIT);
        intent.putExtra(EXTRA_SPEED_LIMIT, Math.max(0, mbps));
        try {
            ContextCompat.startForegroundService(context, intent);
        } catch (Throwable e) {
            Log.w(TAG, "pushSpeedLimit failed", e);
        }
    }

    /**
     * 308：手机用户自己改了限速（只会由 {@link #ACTION_SPEED_LIMIT} 这条路径调进来）。
     *
     * <p>设值 + 提升代次：会话线程发现代次变了就写一帧 SPEED_LIMIT 给电脑，
     * 两端从此显示同一个数。和采纳电脑的值（{@link #onSyncedSpeedLimit}）必须分开 ——
     * 采纳也提代次的话，两端会把同一个数来回推个没完。
     */
    public static void onLocalSpeedLimitChanged(int mbps) {
        int value = Math.max(0, mbps);
        top.weixiansen574.hybridfilexfer.core.SpeedLimiter.applyLocalBytesPerSecond(
                top.weixiansen574.hybridfilexfer.core.SpeedLimiter.mbpsToBytesPerSecond(value));
        Context context = appContext;
        if (context != null) {
            // 本机这一侧也要落盘：界面写的是界面进程的 Config，这里写的是服务这一侧的
            Config.getInstance(context).setSpeedLimitMbps(value);
            HFXServer.setConfiguredLimitFallback(value);
        }
    }

    /**
     * 308：两端共用的限速值定下来了（握手定下来的 / 电脑推来的）——落盘 + 通知界面。
     *
     * <p>SpeedLimiter 的采纳（配置值 + 生效值 + reset 计时窗）由调用方在自己那一侧做，
     * 这里只管"存"和"告诉界面"：界面的「连接」页读的是界面进程的 Config。
     */
    public static void onSyncedSpeedLimit(int mbps) {
        int value = Math.max(0, mbps);
        Context context = appContext;
        if (context == null) {
            // 服务还没 onCreate 完就要落盘：没有 Context 什么也写不了，等下次启动读配置
            return;
        }
        Config.getInstance(context).setSpeedLimitMbps(value);
        // 握手兜底值也跟着换成共用值：下次握手报出去的就是它（报旧值会把它又改回去）
        HFXServer.setConfiguredLimitFallback(value);
        try {
            Intent intent = new Intent(ACTION_SPEED_LIMIT_SYNCED)
                    .setPackage(context.getPackageName())
                    .putExtra(EXTRA_SPEED_LIMIT_SYNCED, value);
            context.sendBroadcast(intent);
        } catch (Throwable e) {
            // 广播发不出去最多是界面晚一帧刷新，不影响限速本身
            Log.w(TAG, "broadcast speed limit failed", e);
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
        // 传输线程里没兜住的异常（拔线最常见）只记日志、不带走进程，见 CrashGuard
        CrashGuard.install();
        appContext = getApplicationContext();
        config = Config.getInstance(this);
        // 限速是 SpeedLimiter 的静态字段，按进程隔离：界面里 applySpeedLimit() 设的值
        // 靠静态字段是传不过来的（界面改设置时走 ACTION_SPEED_LIMIT 这个 intent，见下）。
        // 所以必须在这里（服务自己这一侧）按保存的配置再设一次，否则限速永远是 0。
        // 308：持久化里存的就是"两端共用的那个值"，按采纳语义设（不提代次）。
        top.weixiansen574.hybridfilexfer.core.SpeedLimiter.applySharedBytesPerSecond(
                top.weixiansen574.hybridfilexfer.core.SpeedLimiter
                        .mbpsToBytesPerSecond(config.getSpeedLimitMbps()));
        // 307：同一份值再交给 HFXServer 当兜底。服务刚 rebuilt、SpeedLimiter 还没就绪的
        // 那几百毫秒里如果正好握手，用它代替报 0 —— 报 0 等于告诉电脑「我不限速」，
        // 电脑那边就会把自己那条发送路径放开（协商最怕的就是这种单边放宽）。
        top.weixiansen574.hybridfilexfer.droidcore.HFXServer.setConfiguredLimitFallback(
                config.getSpeedLimitMbps());
        createNotificationChannel();
        goForeground();
        // 局域网广播：让电脑不用 adb、不用手动输 IP 也能发现手机
        discoveryBeacon = new DiscoveryBeacon(this, config.getServerPort());
        discoveryBeacon.start();
        // 盯住网卡集合变化（换 WiFi / 开关蓝牙 / 开关 USB 共享之后要重算通道）
        registerNetworkWatcher();
        // 全局接收进度（悬浮窗 + 通知栏）
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        // 收到的是"启动"而不是"停止"：把停止标记复位（停止流程刚走完、实例还没销毁时
        // 又被电脑/adb 拉起，这条实例必须能继续更新通知，不然它以后再也不刷新通知栏）
        if (intent == null || !ACTION_STOP.equals(intent.getAction())) {
            stopping = false;
        }
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
        // 通知卡片上的任务按钮（暂停 / 继续 / 取消保留 / 取消删除）。
        //
        // 直接作用在任务上，不用先回 App：这几颗按钮就是用户说的「通知里也要能暂停、取消，
        // 并能选保留 / 不保留」。处理完立刻把通知刷一帧，暂停后按钮马上变成「继续」，
        // 取消后这条任务从通知里消失（队列里还有别的任务就换成那个任务的进度）。
        if (ACTION_TASK_PAUSE.equals(action) || ACTION_TASK_RESUME.equals(action)
                || ACTION_TASK_CANCEL_KEEP.equals(action) || ACTION_TASK_CANCEL_DELETE.equals(action)) {
            int taskId = intent.getIntExtra(EXTRA_TASK_ID, -1);
            if (taskId > 0) {
                if (ACTION_TASK_PAUSE.equals(action)) {
                    SendTasks.pauseTask(taskId);
                } else if (ACTION_TASK_RESUME.equals(action)) {
                    SendTasks.resumeTask(taskId);
                } else {
                    // 「取消（保留）」= 电脑上那份半截文件和 .hfxpart 都留着，下次同一个文件续传；
                    // 「取消（删除）」= 让电脑端把它们一起删掉
                    SendTasks.cancelTask(taskId, !ACTION_TASK_CANCEL_KEEP.equals(action));
                }
                Log.i(TAG, "通知按钮 " + action + " taskId=" + taskId);
            }
            // 500ms 的进度门闩不能把这一帧吃掉：暂停 / 继续 / 取消必须在同一条通知上立刻看到
            progressNotifyAt = 0;
            updateNotification();
            return START_STICKY;
        }
        // 308：界面改了限速。新值随 intent 进到服务这一侧，由**本进程**设值 + 提代次，
        // 会话线程在下一轮轮询里发现代次变了就把新值推给电脑。
        // 处理完直接返回：这不是"启动服务"，别落到下面的监听逻辑里去。
        if (ACTION_SPEED_LIMIT.equals(action)) {
            onLocalSpeedLimitChanged(intent.getIntExtra(EXTRA_SPEED_LIMIT, config.getSpeedLimitMbps()));
            return START_STICKY;
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
            // 限速值随 intent 进服务进程（静态字段跨进程不共享）；没带就用存好的配置。
            // 308：这一份是"启动时对齐用的共用值"（等同采纳），不是用户刚改的那一版 ——
            // 用户改设置走的是 ACTION_SPEED_LIMIT 分支（会提代次、会推给电脑）。
            int speedLimitMbps = intent.getIntExtra(EXTRA_SPEED_LIMIT, -1);
            top.weixiansen574.hybridfilexfer.core.SpeedLimiter.applySharedBytesPerSecond(
                    top.weixiansen574.hybridfilexfer.core.SpeedLimiter.mbpsToBytesPerSecond(
                            speedLimitMbps >= 0 ? speedLimitMbps : config.getSpeedLimitMbps()));
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
        // 刚按新列表开过监听，之前那条「等传输结束再重开」的待办已经兑现
        interfaceRefreshPending = false;
        mainHandler.removeCallbacks(networkRefresher);
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
        top.weixiansen574.hybridfilexfer.core.HFXLog.d("监听就绪，端口 " + port);
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
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("收到控制通道，开始握手");
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
                    top.weixiansen574.hybridfilexfer.core.HFXLog.d("握手完成（" + HFXServer.count() + " 台在线）");
                    setState(STATE_CONNECTED);
                    Thread session = new Thread(() -> sessionLoop(server, gen), "HFX-Session-" + server.getId());
                    session.start();
                } else {
                    // 走到这里说明「电脑的控制通道已经到了，但握手没走完」—— 记下时刻，
                    // 分享面板据此把「没连电脑」改成「连上了、通道没建起来」，别再让用户
                    // 去检查一个他明明做对了的动作（详见 getLastHandshakeAbortAt 的注释）。
                    lastHandshakeAbortAt = SystemClock.elapsedRealtime();
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
            // 309：上一次补发 QUEUE_STATE 的时刻（传输期间每秒一帧兜底）
            long lastQueueState = SystemClock.elapsedRealtime();
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
                // 309：传输进行中每 1 秒补发一帧 QUEUE_STATE 兜底。
                // 只写不读 —— 这段时间控制通道的读权在传输线程手里（见上面那段注释），
                // 会话线程去读就会把结论帧吃掉。
                if (server.isTransferring() && now - lastQueueState >= QUEUE_STATE_INTERVAL_MS) {
                    lastQueueState = now;
                    try {
                        server.writeQueueState();
                    } catch (IOException e) {
                        Log.i(TAG, "queue state push failed (" + server.getDisplayName() + "): " + e);
                        break;
                    }
                }
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
    /**
     * 已经恢复出通道之后，多久没有新的 REBUILD_REQ 就认为电脑发完了。
     *
     * <p>电脑端是一条一条来（connect → REQ → 等 ACK → 下一条），相邻两条之间的间隔
     * 真机实测 300~400ms；最坏的一条要先白等 {@link #REBUILD_TAKE_MS} 再重连一次，也就
     * 2 秒出头。所以这个窗口给 2.5 秒既够用、又不会像以前的 5 秒那样，在这之后整整
     * 白坐 5 秒才去开下一单（用户看到的「点了取消，半天不开始下一个」就是这个尾巴）。
     * 提前收窗口也不算错：通道少几条只是慢一点，电脑端知道这一轮只要回收到了的那几条。
     */
    private static final long REBUILD_IDLE_MS = 2500;
    /**
     * 等一条传输通道接入的上限。
     *
     * <p>电脑端是「先 connect 再发 REBUILD_REQ」，读到请求时 socket 早已在 accept 队列里，
     * 正常路径只要一个分类窗口（100ms）。所以这条上限定得短：真到了 800ms 还没取到，
     * 说明这条是电脑连得太早、被 accept 线程当成上一轮残留关掉的那种 —— 早点回
     * REBUILD_ACK false，电脑端就能早点重连，而不是陪着这里白等（以前给 1500ms，
     * 加上电脑重连，那一条通道要 2.3 秒才回来，用户看到的就是「取消后要等四五秒」）。
     */
    private static final long REBUILD_TAKE_MS = 800;

    /**
     * 304：取消之后等电脑把传输通道重建回来（会话不断）。
     *
     * <p>流程：等传输线程彻底退出（它可能还停在控制通道的结论帧上，两个读者会各吃半帧）
     * → 在监听器上挂号（这之后 accept 线程收到的安静连接不再丢，是电脑刚发起的传输通道）
     * → 一帧一帧读 REBUILD_REQ，每收到一条就向 accept 线程要一条通道，
     * 收下就回 REBUILD_ACK{true}，要不到就回 {false} 让电脑跳过这条。
     *
     * <p><b>有的 REBUILD_REQ 的 ACK 已经被别人回过了</b>：drainStaleControlFrames /
     * queryResume 在等自己的回复时撞上 REBUILD_REQ，必须当场回一帧（电脑就停在等 ACK，
     * 它不读别的东西，谁也不会来补这一帧 —— 那就是两端互等）。那几条进队列时带着
     * {@code ackAlreadySent=true}，这里只把它们对应的通道领回来加进 connections，
     * <b>不再回第二帧 ACK</b>：电脑端一帧 REQ 只等一帧 ACK，多出来的一帧会被它当成
     * 下一条通道的答复，名字和 socket 从此整体错位。
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
        // 0) 先挂号，再等传输线程交权。
        //
        // 电脑端一收到 CANCEL 就开始 connect，比我们这里进到重建握手早一秒多。
        // 以前挂号排在「等传输线程退出」之后，那一秒多里 accept 线程把电脑连过来的
        // 安静连接当成上一轮残留全部关掉，于是第一条通道必然等不到、整轮重建要
        // 多花一次 1.5 秒的重连 —— 用户看到的就是「点了取消还要等五六秒」。
        // 挂号只是给 accept 线程一个计数器，不碰控制通道，提前做完全安全：
        // 这段时间攒下来的连接本来就该交给接下来的重建。
        int wantEarly = server.getTransferCount();
        if (wantEarly > 0) {
            listener.wantTransfers(wantEarly);
        }
        // 1) 等传输线程交出控制通道的读权
        long waitStart = System.currentTimeMillis();
        long waitDeadline = waitStart + 15000;
        while ((server.isTransferring() || server.isSendingFiles())
                && System.currentTimeMillis() < waitDeadline) {
            SystemClock.sleep(50);
        }
        // 这一段在真机上能占到十几秒（取消之后到重建握手开始之间的空转），
        // 用户看到的就是「点了取消，半天才开始下一单」。留一行数，别靠猜。
        top.weixiansen574.hybridfilexfer.core.HFXLog.d("rebuild: waitTransferExit "
                + (System.currentTimeMillis() - waitStart) + "ms transferring="
                + server.isTransferring() + " sendingFiles=" + server.isSendingFiles());
        if (server.isTransferring() || server.isSendingFiles()) {
            // 传输线程还没退出就抢控制通道 = 两个读者各吃半帧，整条会话必废。
            // 宁可断线重连，也不要把帧流搞错位。
            debugFile("rebuild: transfer thread still owns the control channel");
            listener.endWantTransfers();
            return false;
        }
        int want = server.beginRebuild();
        if (want <= 0) {
            debugFile("rebuild: no channel to restore (" + server.getDisplayName() + ")");
            listener.endWantTransfers();
            return false;
        }
        debugFile("rebuild: waiting for " + want + " channel(s) from " + server.getDisplayName());
        int got = 0;
        long deadline = System.currentTimeMillis() + REBUILD_WINDOW_MS;
        long idleUntil = Long.MAX_VALUE;
        try {
            while (got < want) {
                long now = System.currentTimeMillis();
                if (now > deadline || now > idleUntil) {
                    break;
                }
                // 别的读循环提前读到的重建请求（exchangePause 兜底 / drainStaleControlFrames /
                // queryResume 收的）。ackAlreadySent=true 的那几条，ACK 已经由读帧的人当场
                // 回了 —— 这里只领通道，别再回第二帧（一帧 REQ 只配一帧 ACK）。
                top.weixiansen574.hybridfilexfer.core.HFXService.RebuildRequest pending =
                        server.pollRebuildRequest();
                String name = pending == null ? null : pending.name;
                boolean ackAlreadySent = pending != null && pending.ackAlreadySent;
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
                            || opcode == ControllerIdentifiers.CANCEL_DISCARD
                            || opcode == ControllerIdentifiers.REBUILD_ACK
                            || opcode == ControllerIdentifiers.SPEED_LIMIT
                            || opcode == ControllerIdentifiers.RESULT) {
                        // 心跳 / 重复的取消帧（CANCEL_DISCARD 也当取消帧读掉：要删的文件
                        // 已经由接收收尾处理过了，这里再删一次既没有 WriteFileCall 可删，
                        // 也会和重建握手抢帧序）：读到就行。
                        // 308：SPEED_LIMIT 的 i32 载荷也已经在 readControlOpcode 的锁里
                        // 读掉并采纳了，这里同样只是"读掉不打断重建"。
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
                    // 通道名已经在 readControlOpcode 的锁里读出来存好了。
                    // 这里必须按 FIFO 取"最早那条还没处理的"：队列里可能还排着前几轮
                    // 被 drain / queryResume 就地回过 ACK 的那几条（它们只欠领通道），
                    // 取头一条才不会让"名字 ↔ socket ↔ ACK"三者错位。
                    pending = server.pollRebuildRequest();
                    if (pending == null) {
                        debugFile("rebuild: REBUILD_REQ without channel name");
                        continue;
                    }
                    name = pending.name;
                    ackAlreadySent = pending.ackAlreadySent;
                }
                // 等通道要短：电脑端是「先 connect 再发 REBUILD_REQ」，读到这个请求时
                // socket 早就躺在 accept 队列里了，正常路径 100~200ms（分类窗口）就取到。
                // 以前给 12 秒，一旦这一条真的没到（电脑连得太早、被 accept 线程当成
                // 上一轮残留关掉），整条队列就要白等 12 秒才继续 —— 用户看到的就是
                // 「点了取消，二十秒后才开始下一单」。电脑端现在也会重连一次，
                // 800ms 加上它自己的一次 connect 足够把这条捡回来。
                SocketChannel socket = listener.takeTransferChannel(REBUILD_TAKE_MS);
                boolean accepted = socket != null;
                try {
                    if (accepted) {
                        server.addRebuiltConnection(name, socket);
                    }
                    // 提前回过 ACK 的那批（drain / queryResume 就地回的）这里只补"领通道"，
                    // 绝不再写第二帧：电脑端已经在等下一条通道的答复了，多一帧会让
                    // 通道名和 socket 整体错位（它会把这一帧当成下一条通道的 ACK）。
                    if (!ackAlreadySent) {
                        server.writeRebuildAck(accepted);
                    } else if (!accepted) {
                        // ACK 已经说了"收下"，手里却没通道：电脑那边会一直往这条死
                        // socket 上写，只能在日志里留证据（已经没法撤回那一帧了）。
                        debugFile("rebuild: channel " + name
                                + " was acked early but no socket arrived");
                    }
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
        // 「先立旗再 setState：setState 会调 updateNotification()，停止期间绝不能再 post，
        //  否则 onDestroy 那一次会把系统刚撤掉的前台通知重新贴回来」
        stopping = true;
        active = false;
        stopLoop(true);
        setState(STATE_STOPPED);
        stopForegroundCompat();
        // 「onDestroy 里服务记录已经没了，stopForeground 是空操作，必须自己 cancel 一次，
        //  否则前面任何一次 updateNotification() 留下的通知会一直挂在通知栏（ongoing，滑不掉）」
        cancelNotification();
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
        unregisterNetworkWatcher();
        mainHandler.removeCallbacks(networkRefresher);
        stopEverything();
        super.onDestroy();
    }

    // ------------------------------------------------------------------
    // 网卡集合变化 → 重开监听（僵尸通道的手机端修法）
    // ------------------------------------------------------------------

    /**
     * 盯住系统网卡集合的变化。
     *
     * <p>为什么必须有这一步：{@link #interfaces} 只在 {@link #begin} 里赋值一次，之后
     * 谁都不再动它。用 `svc wifi disable` / 开关蓝牙 / 开关 USB 网络共享换过网卡之后，
     * 电脑端看到的还是**开机时那批通道**，其中已经不存在的那些是哑的（写进去没反应、
     * 又不会报错），界面却还照旧显示「6 条通道」。用户实测到的现象就是：速度只剩一两条
     * 通道的水平，非要 `am force-stop` 杀掉应用、让电脑端重新拉一次才恢复。
     *
     * <p>以前唯一的兜底是 Compose 界面里那圈轮询（HfxApp.kt 的 `interfaces` 比较），
     * 它有两个致命缺口：一是**只在「服务在跑但没连上电脑」时才重启**（已经连上就不动），
     * 二是它调的是 {@link #restartSession}，那条路只带 EXTRA_FRESH、**不带网卡列表**，
     * `onStartCommand` 里 requested 为空时什么都不做 —— 通道集合根本不会重算。
     * 所以这里直接走「带完整网卡列表的 startServer」（即 {@link #startAuto}），
     * 那条路才会真的重新规划通道。
     *
     * <p>传输中不改通道列表（会把正在跑的传输打断），只记一个待办，等这一轮结束再办。
     */
    private void registerNetworkWatcher() {
        if (networkCallback != null) {
            return;
        }
        try {
            ConnectivityManager cm =
                    (ConnectivityManager) getSystemService(Context.CONNECTIVITY_SERVICE);
            if (cm == null) {
                return;
            }
            // 空 Request = 所有网络都要（WiFi / 双 WiFi / 以太网 / USB 共享 / 蓝牙 PAN 都会报），
            // 不能加 NET_CAPABILITY_INTERNET 过滤：蓝牙 PAN、刚起来的 rndis 在拿到 IP 之前
            // 没有 INTERNET 能力，加了就完全收不到它们的回调。
            NetworkRequest request = new NetworkRequest.Builder().build();
            ConnectivityManager.NetworkCallback callback =
                    new ConnectivityManager.NetworkCallback() {
                        @Override
                        public void onAvailable(Network network) {
                            noteInterfacesMayHaveChanged("network available");
                        }

                        @Override
                        public void onLost(Network network) {
                            noteInterfacesMayHaveChanged("network lost");
                        }

                        @Override
                        public void onLinkPropertiesChanged(Network network, LinkProperties lp) {
                            // 同一张网卡换了 IP（WiFi 重连拿到新地址）只走这条，不报 onAvailable
                            noteInterfacesMayHaveChanged("link properties changed");
                        }
                    };
            cm.registerNetworkCallback(request, callback);
            networkCallback = callback;
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("已开始监听网卡变化");
        } catch (Throwable e) {
            // 注册不上不影响原有功能（界面轮询仍在），只是响应慢一点
            networkCallback = null;
            Log.w(TAG, "registerNetworkCallback failed", e);
        }
    }

    private void unregisterNetworkWatcher() {
        ConnectivityManager.NetworkCallback callback = networkCallback;
        networkCallback = null;
        if (callback == null) {
            return;
        }
        try {
            ConnectivityManager cm =
                    (ConnectivityManager) getSystemService(Context.CONNECTIVITY_SERVICE);
            if (cm != null) {
                cm.unregisterNetworkCallback(callback);
            }
        } catch (Throwable e) {
            Log.w(TAG, "unregisterNetworkCallback failed", e);
        }
    }

    /**
     * 系统说网卡变了。
     *
     * <p>一次 WiFi 切换会连着来好几条回调（lost → available → linkProperties），
     * 所以先压 800ms 再统一处理；那时 {@code NetInterfaces.enumerate} 才能读到稳定的状态。
     */
    private void noteInterfacesMayHaveChanged(String reason) {
        if (!active) {
            return;
        }
        top.weixiansen574.hybridfilexfer.core.HFXLog.d("网卡变化（" + reason + "），稍后重算通道");
        mainHandler.postDelayed(networkRefresher, NETWORK_REFRESH_DELAY_MS);
    }

    /** 网卡变化后的统一处理：集合/地址真变了就按新列表重开监听 */
    private final Runnable networkRefresher = new Runnable() {
        @Override
        public void run() {
            if (!active) {
                interfaceRefreshPending = false;
                return;
            }
            List<ServerNetInterface> wanted;
            try {
                wanted = NetInterfaces.forAutoConnect(appContext != null ? appContext : ServerService.this);
            } catch (Throwable e) {
                Log.w(TAG, "enumerate interfaces failed", e);
                return;
            }
            if (wanted.isEmpty()) {
                // 用户一张网卡都没勾：保持现状，别把监听停掉
                interfaceRefreshPending = false;
                return;
            }
            if (sameInterfaceSet(wanted)) {
                interfaceRefreshPending = false;
                return;
            }
            if (isBusy()) {
                // 传输中：记下来，等这一轮结束（下面 2 秒后自己再看一眼）
                if (!interfaceRefreshPending) {
                    interfaceRefreshPending = true;
                    lastInterfaceRefreshTry = SystemClock.elapsedRealtime();
                    top.weixiansen574.hybridfilexfer.core.HFXLog.d(
                            "网卡变了但正在传输，等这一轮结束再重开通道");
                }
                mainHandler.removeCallbacks(this);
                mainHandler.postDelayed(this, 2000);
                return;
            }
            interfaceRefreshPending = false;
            lastInterfaceRefreshTry = SystemClock.elapsedRealtime();
            top.weixiansen574.hybridfilexfer.core.HFXLog.d(
                    "网卡集合变了，按新列表重开监听（" + wanted.size() + " 张网卡）");
            Log.i(TAG, "interfaces changed, restarting listener with " + wanted.size() + " interfaces");
            debugFile("interfaces changed -> restart listener, " + wanted.size());
            // 走「带完整网卡列表的 startServer」这条路（= startAuto 内部做的事）：
            // restartSession 只带 EXTRA_FRESH，不会重算通道集合。
            startAuto(ServerService.this);
        }
    };

    /** 当前正在用的网卡集合（含 IP）是否等于 wanted —— 名字和地址都得对上 */
    private boolean sameInterfaceSet(List<ServerNetInterface> wanted) {
        return interfaceKeys(interfaces).equals(interfaceKeys(wanted));
    }

    /**
     * 把网卡列表压成可比较的 key 集合。
     *
     * <p>{@link #interfaces} 是**展开过**的（wlan0 / wlan0#2 / wlan0#3 / wlan0#4），
     * wanted 是每张网卡一条，所以先把 "#n" 后缀去掉再比；地址也带上 ——
     * WiFi 重连后名字可能还是 wlan0，但 IP 从 .117 变成 .120，那条也得重开。
     */
    private static java.util.Set<String> interfaceKeys(List<ServerNetInterface> list) {
        java.util.Set<String> keys = new java.util.TreeSet<>();
        for (ServerNetInterface item : list) {
            if (item == null || item.name == null) {
                continue;
            }
            String name = item.name;
            int hash = name.indexOf('#');
            if (hash > 0) {
                name = name.substring(0, hash);
            }
            String address = item.address == null ? "" : item.address.getHostAddress();
            keys.add(name + "@" + address);
        }
        return keys;
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
        // 通知上现在挂着哪一条传输：有分享发送任务就是那个任务（多任务时是聚合视图）
        TransferView view = currentTransferView();
        // 点通知要落到哪个任务的面板：有进度条就是进度条上那个任务（队列头），
        // 没有进度条（准备中 / 排队中 / 刚收尾）也照样落到任务面板上。
        //
        // 以前这里只认 view.taskId：没有进度可显示的那几段（准备、排队、传完的那 1.5 秒）
        // contentIntent 退回 MiuixMainActivity，用户点了通知看到的就是「App 主页」——
        // 报的「点通知仍打开主页」正是这条路。现在只要表里还有任务，就一定落到它的面板。
        int targetTaskId = (view != null && view.taskId > 0)
                ? view.taskId : SendTasks.panelTargetTaskId();
        Intent openIntent;
        int requestCode = 0;
        if (targetTaskId != lastNotifiedTaskId) {
            // 只在目标变化时打一行（500ms 一帧全打会把日志刷满）：排查「点通知落到哪」时
            // 直接看这行 —— taskId=N（任务面板）或者 （主界面）
            Log.i(TAG, "通知点击目标 taskId=" + targetTaskId
                    + (targetTaskId > 0 ? "（任务面板）" : "（主界面）"));
        }
        lastNotifiedTaskId = targetTaskId;
        if (targetTaskId > 0) {
            // 点通知 → 回到「这个任务」的进度面板。
            // 面板还在就是它自己（同 taskId 只会有一个窗口），被划掉了就重建一个绑到同一个 taskId，
            // 进度接着显示。requestCode 按 taskId 区分，几个任务的 PendingIntent 不会互相顶掉。
            openIntent = SendTasks.panelIntent(this, targetTaskId);
            requestCode = NOTIFICATION_TASK_REQUEST_BASE + targetTaskId;
        } else {
            openIntent = new Intent(this, top.weixiansen574.hybridfilexfer.compose.MiuixMainActivity.class);
            openIntent.setFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TOP);
            // 点通知 → 回到应用并重新弹出接收进度窗口
            openIntent.putExtra("hfx_show_transfer", true);
        }
        PendingIntent contentIntent = PendingIntent.getActivity(this, requestCode, openIntent,
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
                // 这条是「总通知」（summary）：每个任务另有一条自己的子通知
                // （见 syncTaskNotifications）。收起时通知栏只显示这一条合计，
                // 下拉展开才逐条列出各任务、各带自己的按钮。
                .setGroup(GROUP_TASKS)
                .setGroupSummary(true);
        // 卡片上的按钮（现代 Android 卡片收起时只放前 3 颗，所以有任务时不再挂「停止服务」）：
        //   正在传 / 暂停着：[暂停 或 继续] [取消（保留）] [取消（删除）]
        //   只是在准备 / 排队：[取消（保留）] [取消（删除）]
        //   完全没有任务：      [停止]（和以前一样，停止整个后台服务）
        if (targetTaskId > 0) {
            boolean thisTaskLive = view != null && view.taskId == targetTaskId;
            if (thisTaskLive) {
                boolean paused = view.paused;
                builder.addAction(0, paused ? "继续" : "暂停", taskActionIntent(
                        paused ? ACTION_TASK_RESUME : ACTION_TASK_PAUSE,
                        paused ? NOTIFICATION_RESUME_REQUEST_BASE : NOTIFICATION_PAUSE_REQUEST_BASE,
                        targetTaskId));
            }
            builder.addAction(0, "取消（保留）", taskActionIntent(
                    ACTION_TASK_CANCEL_KEEP, NOTIFICATION_CANCEL_KEEP_REQUEST_BASE, targetTaskId));
            builder.addAction(0, "取消（删除）", taskActionIntent(
                    ACTION_TASK_CANCEL_DELETE, NOTIFICATION_CANCEL_DELETE_REQUEST_BASE, targetTaskId));
        } else {
            builder.addAction(0, getString(R.string.notification_stop), stopPendingIntent);
        }
        // 传输中：通知栏直接显示进度（发送/接收挑当前有活动的那一路，全局可见、不依赖悬浮窗权限）
        applyTransferProgress(builder, view);
        return builder.build();
    }

    /**
     * 任务级按钮的 PendingIntent（暂停 / 继续 / 取消保留 / 取消删除）。
     *
     * <p>用 getForegroundService（O+）：这几颗按钮和「停止」那颗是同一套机制 —— 服务本身
     * 已经在前台，onStartCommand 一进来又会 goForeground()，从通知点进来的那次允许名单
     * 也由系统给（NOTIFICATION_SERVICE 的临时放行），不会撞 Android 12+ 的后台启动限制。
     */
    private PendingIntent taskActionIntent(String action, int requestCodeBase, int taskId) {
        Intent intent = new Intent(this, ServerService.class);
        intent.setAction(action);
        intent.putExtra(EXTRA_TASK_ID, taskId);
        int requestCode = requestCodeBase + taskId;
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            return PendingIntent.getForegroundService(this, requestCode, intent,
                    PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        }
        return PendingIntent.getService(this, requestCode, intent,
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
    }

    // ------------------------------------------------------------------
    // 通知栏进度（正在发送/接收 xx%）
    // ------------------------------------------------------------------

    /** 上次进度帧 post 的时刻（elapsedRealtime；0 表示当前没有在跟进度） */
    private long progressNotifyAt;
    /** 速率采样：上次采样的累计已传字节 / 时刻 / 一阶平滑后的速率 */
    private long sampleBytes;
    private long sampleAt;
    private long sampleSpeed;
    /** 通知栏现在贴的是不是「正在发送/接收 xx%」：传完时靠它触发一次回落到常驻文案 */
    private volatile boolean progressShown;
    /**
     * 通知上一次贴出去时，contentIntent 落在哪个任务（-1 = 落在主界面）。
     *
     * <p>没有进度可显示的时候（准备中 / 排队中 / 等电脑连接）通知本来是<b>不刷</b>的 —— 这是为省电，
     * 每 50ms 无脑 notify 会掉帧、发烫。但「点通知该落到哪个任务的面板」是会变的：
     * 新任务刚建好的那一瞬间，通知还挂着服务启动时那条「点开主页」的 contentIntent，
     * 用户点它看到的就是 App 主页（报的「点通知仍打开主页」另一条路）。
     * 所以记下最后一次贴出去的目标，目标一变就补刷一帧。
     */
    private volatile int lastNotifiedTaskId = Integer.MIN_VALUE;

    /**
     * 通知栏要显示的一条传输：哪个方向、当前文件、已传多少、速率。
     *
     * <p>发送和接收不会同时进行（一次会话只有一个传输线程），{@link #currentTransferView()}
     * 挑"当下有活动的那一路"，两者都空就是没有传输。
     */
    private static final class TransferView {
        /** 分享发送任务的 taskId；-1 = 不是任务（电脑推送 / 双窗口「← 复制」那两路） */
        final int taskId;
        final boolean sending;
        final String name;
        final long done;
        final long total;
        final long speed;
        /** 参与聚合的任务数（1 = 单个任务或者不是任务） */
        final int taskCount;
        /** 多任务时的展开明细（每行一个任务：文件名 · 百分比 · 各自的速率） */
        final String multiDetail;
        /** 这个任务现在是暂停着（用户按的暂停 / 为了给别的任务让路而停住） */
        final boolean paused;

        TransferView(boolean sending, String name, long done, long total, long speed) {
            this(-1, sending, name, done, total, speed, 1, "", false);
        }

        TransferView(int taskId, boolean sending, String name, long done, long total, long speed,
                     int taskCount, String multiDetail) {
            this(taskId, sending, name, done, total, speed, taskCount, multiDetail, false);
        }

        TransferView(int taskId, boolean sending, String name, long done, long total, long speed,
                     int taskCount, String multiDetail, boolean paused) {
            this.taskId = taskId;
            this.sending = sending;
            this.name = name == null ? "" : name;
            this.done = done;
            this.total = total;
            this.speed = speed;
            this.taskCount = Math.max(1, taskCount);
            this.multiDetail = multiDetail == null ? "" : multiDetail;
            this.paused = paused;
        }

        int percent() {
            if (total <= 0) {
                return 0;
            }
            return (int) Math.max(0, Math.min(100, done * 100 / total));
        }
    }

    /** 每个发送任务自己的速率采样（多任务并发时各算各的，不能共用一个） */
    private final java.util.Map<Integer, TaskSpeed> taskSpeeds = new java.util.HashMap<>();

    private static final class TaskSpeed {
        long bytes;
        long at;
        long speed;
    }

    /**
     * 采一次某个发送任务的速率。
     *
     * <p>和 {@link #sampleTransferSpeed} 同一套算法（两次采样的增量 ÷ 时间，再做一阶平滑），
     * 只是<b>按 taskId 各存一份</b> —— 两个任务同时在传时，通知栏上的合计和展开里的
     * 每一条都必须是各自的速度，不能互相串。
     */
    private long sampleTaskSpeed(int taskId, long done, long now) {
        TaskSpeed sample = taskSpeeds.get(taskId);
        if (sample == null) {
            sample = new TaskSpeed();
            sample.bytes = done;
            sample.at = now;
            taskSpeeds.put(taskId, sample);
            return 0;
        }
        // 换文件 / 让出通道后重新接着传：账本会重来，done 变小就重新起算
        if (done < sample.bytes) {
            sample.bytes = done;
            sample.at = now;
            sample.speed = 0;
            return 0;
        }
        long elapsed = now - sample.at;
        if (elapsed < PROGRESS_NOTIFY_INTERVAL_MS / 2) {
            return sample.speed;
        }
        long instant = (done - sample.bytes) * 1000L / Math.max(1L, elapsed);
        sample.speed = sample.speed <= 0 ? instant : (sample.speed * 2 + instant) / 3;
        sample.bytes = done;
        sample.at = now;
        return sample.speed;
    }

    /**
     * 当前该显示在通知栏上的那一路传输，没有就返回 null。
     *
     * <p>真值来源（两路都可能活动，谁有数就是谁）：
     * <ul>
     *   <li>接收：{@link #currentTransfer()} 的 Live（电脑推送那条路）＋ {@code blockProgress}；</li>
     *   <li>发送：「复制 →」/分享面板发送，读 {@code server.sendProgress}
     *       （块真正写出去以后才记，不会跑到真值前面）。</li>
     * </ul>
     * {@code sendProgress} 和 {@code blockProgress} 在每次开始传输时都会被 reset，
     * 所以"有数的那一路"就是正在传的那一路，不会把上一次传输的数字贴上来。
     */
    private TransferView currentTransferView() {
        // 0) 分享发送任务（多任务会话表 SendTasks）：有任务在传就以任务为准 ——
        //    这条通知的 contentIntent 要落到「这个任务」的面板（用户报的「点通知回不到任务1」）。
        //    读的是队列快照（只读）：顺序就是队列顺序 —— 正在传的在最前，排队中的跟在后面。
        TransferView queueView = sendTaskView(SendTasks.queueSnapshot());
        if (queueView != null) {
            return queueView;
        }
        Live live = currentTransfer();
        if (live != null && live.total > 0) {
            long done = live.done;
            HFXServer server = HFXServer.getById(live.connectionId);
            if (server != null) {
                // 按块记的真值比回调里的 targetSize 更贴近"已经落盘"，取大的那个
                done = Math.max(done, server.blockProgress.getDoneBytes());
            }
            long speed = 0;
            for (ChannelLive channel : live.channelSnapshot()) {
                speed += channel.speed;
            }
            return new TransferView(false, shortName(live.name), done, live.total, speed);
        }
        for (HFXServer server : HFXServer.snapshot()) {
            try {
                // 只在真传输时读账本：空闲时 snapshot() 没必要（大文件要遍历不少块）
                if (!server.isTransferring()) {
                    continue;
                }
                top.weixiansen574.hybridfilexfer.core.BlockProgress.Snapshot sent =
                        server.sendProgress.snapshot();
                if (sent.totalSize > 0 && sent.doneBytes > 0) {
                    return new TransferView(true, shortName(server.sendProgress.currentPath()),
                            sent.doneBytes, sent.totalSize, sampleSpeed);
                }
                // 应用内「← 复制」拉取：接收进度记在 blockProgress 上（电脑推送那条路走上面的 Live）
                top.weixiansen574.hybridfilexfer.core.BlockProgress.Snapshot got =
                        server.blockProgress.snapshot();
                if (got.totalSize > 0 && got.doneBytes > 0) {
                    return new TransferView(false, shortName(server.blockProgress.currentPath()),
                            got.doneBytes, got.totalSize, sampleSpeed);
                }
            } catch (Throwable ignored) {
            }
        }
        return null;
    }

    /**
     * 把「分享发送的这几个任务」折成一条通知要的视图（**队列顺序**：正在传的在最前）。
     *
     * <p>一个任务：就是它自己的进度 + 它自己的速率，点通知回到它。
     * 多个任务：标题给「N 个任务 · 合计」，展开明细里一行一个任务、按队列顺序排，
     * 排队中的标「等待中（前面还有 N 个）」，暂停的标「已暂停」—— 各算各的速率。
     *
     * @return null = 眼下没有任务真的占着传输通道（只有准备中 / 只在排队的），
     *         通知该走原来那套「已连接 / 等待连接」的文案（和以前 transferringTasks() 的口径一致）
     */
    private TransferView sendTaskView(java.util.List<SendTasks.TaskInfo> queue) {
        if (queue == null || queue.isEmpty()) {
            return null;
        }
        java.util.List<SendTasks.TaskInfo> shown = new java.util.ArrayList<>();
        boolean anyActive = false;
        for (SendTasks.TaskInfo info : queue) {
            // 收尾的、还在准备的不进通知：它们还没有进度可言（和以前一样）
            if (info.isFinished() || info.state == SendTask.STATE_PREPARING) {
                continue;
            }
            if (info.isActive()) {
                anyActive = true;
            }
            shown.add(info);
        }
        if (shown.isEmpty() || !anyActive) {
            return null;
        }
        long now = SystemClock.elapsedRealtime();
        if (shown.size() == 1) {
            SendTasks.TaskInfo info = shown.get(0);
            boolean paused = info.state == SendTask.STATE_PAUSED
                    || info.state == SendTask.STATE_SUSPENDED;
            return new TransferView(info.taskId, true, shortName(info.fileName), info.doneBytes,
                    info.totalBytes, paused ? 0L : taskSpeed(info, now), 1, "", paused);
        }
        long doneSum = 0L;
        long totalSum = 0L;
        long speedSum = 0L;
        StringBuilder detail = new StringBuilder();
        for (int i = 0; i < shown.size(); i++) {
            SendTasks.TaskInfo info = shown.get(i);
            long speed = taskSpeed(info, now);
            doneSum += info.doneBytes;
            totalSum += info.totalBytes;
            speedSum += speed;
            if (detail.length() > 0) {
                detail.append('\n');
            }
            // 序号 = 队列位次（1 开始）：展开明细和面板上那个列表是同一个顺序
            detail.append(i + 1).append(". ").append(shortName(info.fileName))
                    .append(" · ");
            if (info.state == SendTask.STATE_WAITING) {
                // 排队中：没有进度可言，就说清「前面还有几个」（0 = 它自己就是队头）
                if (info.aheadCount <= 0) {
                    detail.append("等待中");
                } else {
                    detail.append("等待中（前面还有 ").append(info.aheadCount).append(" 个）");
                }
            } else if (speed > 0L) {
                detail.append(info.percent()).append('%')
                        .append(" · ").append(GlobalProgress.formatSpeed(speed));
            } else {
                detail.append(info.percent()).append('%');
                if (info.state != SendTask.STATE_SENDING) {
                    detail.append(" · 已暂停");
                }
            }
        }
        // 点通知落到队列头的那个任务（正在传的那个，队列快照已经把它排在最前）
        return new TransferView(shown.get(0).taskId, true, "", doneSum, totalSum, speedSum,
                shown.size(), detail.toString());
    }

    /**
     * 通知明细里那一行的速率。
     *
     * <p>队列快照里已经采好了（{@link SendTasks#queueSnapshot()} 按 taskId 各采各的）；
     * 拿不到就退回本类原来那套 {@link #sampleTaskSpeed}，别让明细里出现一个空缺。
     * 暂停 / 让出通道 / 排队中的一律 0：它们本来就没有数据在流动。
     */
    private long taskSpeed(SendTasks.TaskInfo info, long now) {
        if (info.state != SendTask.STATE_SENDING) {
            return 0L;
        }
        if (info.speed > 0L) {
            return info.speed;
        }
        return sampleTaskSpeed(info.taskId, info.doneBytes, now);
    }

    /**
     * 采样一次速率。
     *
     * <p>{@code BlockProgress} 只有"已经传了多少字节"的累计真值，没有速率；通知栏要的 MB/s
     * 就用两次采样的差算，再做一阶平滑（不平滑的话每帧抖 ±30%，数字根本读不了）。
     * 换文件 / 续传重算时 done 会变小，这时重新起算，别把上一段的字节算进这一秒。
     */
    private void sampleTransferSpeed(long done, long now) {
        if (sampleAt == 0 || done < sampleBytes) {
            sampleBytes = done;
            sampleAt = now;
            sampleSpeed = 0;
            return;
        }
        long elapsed = now - sampleAt;
        if (elapsed < PROGRESS_NOTIFY_INTERVAL_MS / 2) {
            return;
        }
        long instant = (done - sampleBytes) * 1000L / elapsed;
        sampleSpeed = sampleSpeed <= 0 ? instant : (sampleSpeed * 2 + instant) / 3;
        sampleBytes = done;
        sampleAt = now;
    }

    /**
     * 全局进度每 50ms tick 一次，这里再限速成 500ms 一帧贴到通知栏。
     *
     * <p><b>停止流程里一律不 post</b>（和 {@link #updateNotification()} 同一条规矩）：
     * 否则 stopEverything() 刚 cancel 掉的前台通知会被这一帧重新贴回通知栏，
     * 用户又得面对一条滑不掉的 ongoing 通知（刚修过的那个 bug）。
     */
    void refreshProgressNotification() {
        if (stopping) {
            return;
        }
        long now = SystemClock.elapsedRealtime();
        if (progressNotifyAt != 0 && now - progressNotifyAt < PROGRESS_NOTIFY_INTERVAL_MS) {
            return;
        }
        TransferView view = currentTransferView();
        if (view == null) {
            if (!progressShown) {
                // 没有进度可显示：只在「点通知落到哪个任务」变了的时候补刷一帧
                // （任务刚建好 / 排队中 → 通知的 contentIntent 必须换到那个任务的面板）
                if (SendTasks.panelTargetTaskId() != lastNotifiedTaskId) {
                    updateNotification();
                }
                return;
            }
            // 传输结束 / 取消 / 停止：换回原来的常驻文案（只刷这一次，之后回到"状态变了才刷"）
            progressShown = false;
            progressNotifyAt = 0;
            sampleAt = 0;
            sampleBytes = 0;
            sampleSpeed = 0;
            taskSpeeds.clear();
            updateNotification();
            return;
        }
        progressNotifyAt = now;
        // 任务那一路的速率是 sendTaskView 里按 taskId 各自采的，这里只兜「不是任务」的老路径
        if (view.speed <= 0 && view.taskId <= 0) {
            sampleTransferSpeed(view.done, now);
        }
        updateNotification();
    }

    /**
     * 把进度写进通知栏（仍然用同一个常驻通知、同一个渠道，只换标题/正文和进度条）。
     *
     * <p>标题：「正在发送文件 · 45%」；正文：「file.mp4 · 12.3 MB / 600 MB · 24.1 MB/s」。
     * 到 100% 时不写「正在发送 99%」那种残留字面，改成「收尾中」，下一次 tick 就回到常驻文案。
     * setOngoing(true) / setOnlyAlertOnce(true) 在外面已经设好，这些帧只改内容、不响不弹。
     */
    private void applyTransferProgress(NotificationCompat.Builder builder, TransferView view) {
        if (view == null || view.total <= 0) {
            return;
        }
        int pct = view.percent();
        String title;
        if (view.taskCount > 1) {
            // 多任务并发：一条通知给合计（前台服务只能有一条常驻通知，拆成多条会和
            // 「等电脑连接」那条同时挂着，用户更分不清）。展开明细里每个任务一行、
            // 各算各的速率；点它落到最近那个任务的面板，其它任务的面板各自的窗口都在跑。
            title = view.taskCount + " 个任务 · " + pct + "%"
                    + (pct < 100 && view.speed > 0 ? " · " + GlobalProgress.formatSpeed(view.speed) : "");
        } else {
            String base = view.sending ? "正在发送文件" : getString(R.string.notification_receiving);
            if (pct >= 100) {
                title = base + " · 收尾中";
            } else if (view.paused) {
                title = "已暂停 · " + pct + "%";
            } else {
                title = base + " · " + pct + "%";
            }
        }
        StringBuilder detail = new StringBuilder();
        if (!view.name.isEmpty()) {
            detail.append(view.name).append(" · ");
        }
        detail.append(GlobalProgress.formatSize(Math.min(view.done, view.total)))
                .append(" / ")
                .append(GlobalProgress.formatSize(view.total));
        if (pct < 100 && view.speed > 0) {
            detail.append(" · ").append(GlobalProgress.formatSpeed(view.speed));
        }
        // 多任务时正文保持合计，展开（BigText）里给每个任务各自一行
        String bigText = view.multiDetail.isEmpty() ? detail.toString() : view.multiDetail;
        builder.setContentTitle(title)
                .setContentText(detail.toString())
                .setStyle(new NotificationCompat.BigTextStyle().bigText(bigText))
                .setProgress(100, pct, false);
        progressShown = true;
    }

    /** 文件名部分（通知栏那一条横排放不下整条路径） */
    private static String shortName(String path) {
        if (path == null) {
            return "";
        }
        int cut = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'));
        return cut >= 0 ? path.substring(cut + 1) : path;
    }

    // ------------------------------------------------------------------
    // 「每个任务一条通知」：都归到总通知（summary）的分组里
    // ------------------------------------------------------------------

    /**
     * 已经贴出去的任务通知 id。
     *
     * <p>任务收尾 / 从队列表里消失时要按这个集合把多出来的那几条撤掉 ——
     * 不撤的话通知栏会一直挂着一堆「已完成」的条目，还得用户手动划。
     */
    private volatile java.util.Set<Integer> taskNotifyIds = java.util.Collections.emptySet();

    /**
     * 把「每个任务一条通知」和任务队列对齐：还在的更新内容、新出现的贴出去、收尾的撤掉。
     *
     * <p>调用点就是总通知刷新的那几处（进度帧 500ms 一帧 / 通知按钮 / 服务进前台），
     * 对同一个 id 反复 notify 是幂等的，{@code setOnlyAlertOnce(true)} 保证不响不弹。
     *
     * <p>只有「还在队列里、且没收尾」的任务才占一条子通知：收尾的结果在面板里能看到，
     * 通知栏留着只会越堆越多（用户的诉求是「每个<b>传输任务</b>占一条」）。
     */
    private void syncTaskNotifications() {
        if (stopping) {
            return;
        }
        NotificationManager manager = (NotificationManager) getSystemService(NOTIFICATION_SERVICE);
        if (manager == null) {
            return;
        }
        java.util.Set<Integer> alive = new java.util.HashSet<>();
        int order = 0;
        for (SendTasks.TaskInfo info : SendTasks.queueSnapshot()) {
            if (info.isFinished()) {
                continue;
            }
            int id = NOTIFY_ID_TASK_BASE + info.taskId;
            alive.add(id);
            try {
                manager.notify(id, buildTaskNotification(info, order));
            } catch (Throwable e) {
                Log.w(TAG, "贴任务通知失败 taskId=" + info.taskId, e);
            }
            order++;
        }
        java.util.Set<Integer> old = taskNotifyIds;
        if (!old.equals(alive)) {
            for (Integer id : old) {
                if (!alive.contains(id)) {
                    try {
                        manager.cancel(id);
                    } catch (Throwable e) {
                        Log.w(TAG, "撤任务通知失败 id=" + id, e);
                    }
                }
            }
            taskNotifyIds = alive;
        }
    }

    /** 撤掉所有任务子通知（服务停止时用；stopForeground 只管总通知那一条） */
    private void cancelTaskNotifications() {
        NotificationManager manager = (NotificationManager) getSystemService(NOTIFICATION_SERVICE);
        if (manager == null) {
            return;
        }
        for (Integer id : taskNotifyIds) {
            try {
                manager.cancel(id);
            } catch (Throwable ignored) {
            }
        }
        taskNotifyIds = java.util.Collections.emptySet();
    }

    /**
     * 某一个任务自己的通知（分组里的子通知）。
     *
     * <p>标题就是文件名，正文是「状态 · 百分比 · 已传/总量 · 速率」；
     * 点它落到这个任务的进度面板（和总通知撞同一个 PendingIntent 也没关系，落点一样）；
     * 按钮和总通知那三颗一致，只是 taskId 固定成它自己 —— 两个任务同时挂着时，
     * 想暂停第二个不用先进面板找。
     */
    private Notification buildTaskNotification(SendTasks.TaskInfo info, int order) {
        boolean sending = info.state == SendTask.STATE_SENDING;
        boolean paused = info.state == SendTask.STATE_PAUSED;
        boolean suspended = info.state == SendTask.STATE_SUSPENDED;
        boolean preparing = info.state == SendTask.STATE_PREPARING;
        boolean queued = info.isQueued();

        String name = shortName(info.fileName);
        StringBuilder text = new StringBuilder();
        if (preparing) {
            text.append("准备中…");
        } else if (queued) {
            text.append("排队中");
            if (info.aheadCount > 0) {
                text.append("（前面还有 ").append(info.aheadCount).append(" 个任务）");
            }
        } else if (paused) {
            text.append("已暂停");
        } else if (suspended) {
            text.append("已让出通道（暂停着）");
        } else {
            text.append(sending ? "正在发送" : "正在接收");
        }
        if (!queued) {
            text.append(" · ").append(info.percent()).append('%');
            if (info.totalBytes > 0) {
                text.append(" · ").append(GlobalProgress.formatSize(Math.min(info.doneBytes, info.totalBytes)))
                        .append(" / ").append(GlobalProgress.formatSize(info.totalBytes));
            }
            if (sending && info.speed > 0) {
                text.append(" · ").append(GlobalProgress.formatSpeed(info.speed));
            }
        }

        Intent openIntent = SendTasks.panelIntent(this, info.taskId);
        PendingIntent contentIntent = PendingIntent.getActivity(this,
                NOTIFICATION_TASK_REQUEST_BASE + info.taskId, openIntent,
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);

        NotificationCompat.Builder builder = new NotificationCompat.Builder(this, CHANNEL_ID)
                .setSmallIcon(R.drawable.baseline_upload_24)
                .setContentTitle(name.isEmpty() ? ("传输任务 " + info.taskId) : name)
                .setContentText(text.toString())
                .setStyle(new NotificationCompat.BigTextStyle().bigText(text.toString()))
                .setContentIntent(contentIntent)
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .setShowWhen(false)
                .setSilent(true)
                .setPriority(NotificationCompat.PRIORITY_LOW)
                .setGroup(GROUP_TASKS)
                .setGroupSummary(false)
                // 展开后按队列顺序排：位次补零，字符串排序才和队列一致
                .setSortKey(String.format(java.util.Locale.US, "%04d", Math.max(0, order)));
        if (info.totalBytes > 0) {
            builder.setProgress(100, info.percent(), false);
        } else {
            builder.setProgress(0, 0, queued);
        }
        boolean live = sending || paused || suspended;
        if (live) {
            builder.addAction(0, paused || suspended ? "继续" : "暂停", taskActionIntent(
                    paused || suspended ? ACTION_TASK_RESUME : ACTION_TASK_PAUSE,
                    paused || suspended ? NOTIFICATION_RESUME_REQUEST_BASE : NOTIFICATION_PAUSE_REQUEST_BASE,
                    info.taskId));
        }
        builder.addAction(0, "取消（保留）", taskActionIntent(
                ACTION_TASK_CANCEL_KEEP, NOTIFICATION_CANCEL_KEEP_REQUEST_BASE, info.taskId));
        builder.addAction(0, "取消（删除）", taskActionIntent(
                ACTION_TASK_CANCEL_DELETE, NOTIFICATION_CANCEL_DELETE_REQUEST_BASE, info.taskId));
        return builder.build();
    }

    private void updateNotification() {
        // 「停止流程开始后一律不 post：onDestroy 阶段 post 出去的通知没人再撤」
        if (stopping) {
            return;
        }
        try {
            NotificationManager manager = (NotificationManager) getSystemService(NOTIFICATION_SERVICE);
            if (manager != null) {
                manager.notify(NOTIFICATION_ID, buildNotification());
            }
            // 总通知刷新的同时把「每任务一条」的子通知也对齐（同一帧、同一份快照）
            syncTaskNotifications();
        } catch (Throwable e) {
            Log.w(TAG, "updateNotification failed", e);
        }
    }

    /** 「撤掉这条前台/常驻通知（停止后用；stopForeground 在服务已销毁时是空操作）」 */
    private void cancelNotification() {
        try {
            NotificationManager manager = (NotificationManager) getSystemService(NOTIFICATION_SERVICE);
            if (manager != null) {
                manager.cancel(NOTIFICATION_ID);
            }
            // 总通知撤了，子通知也得一起撤：stopForeground 只认前台那一条
            cancelTaskNotifications();
        } catch (Throwable e) {
            Log.w(TAG, "cancelNotification failed", e);
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
            // 服务刚起 / 重启时把还活着的任务的下一条通知补回来
            // （进程被系统杀掉重启后，之前的子通知已经没了，队列里的任务还在）
            syncTaskNotifications();
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