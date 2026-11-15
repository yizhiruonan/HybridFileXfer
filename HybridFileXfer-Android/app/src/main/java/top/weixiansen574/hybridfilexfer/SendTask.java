package top.weixiansen574.hybridfilexfer;

import android.content.ClipData;
import android.content.Context;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Build;
import android.os.Environment;
import android.os.Handler;
import android.os.Looper;
import android.provider.DocumentsContract;
import android.provider.MediaStore;
import android.provider.OpenableColumns;
import android.text.TextUtils;
import android.util.Log;
import android.widget.Toast;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.LinkedList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.atomic.AtomicBoolean;

import top.weixiansen574.hybridfilexfer.core.BlockProgress;
import top.weixiansen574.hybridfilexfer.core.HFXLog;
import top.weixiansen574.hybridfilexfer.core.HFXService;
import top.weixiansen574.hybridfilexfer.core.Utils;
import top.weixiansen574.hybridfilexfer.core.bean.Directory;
import top.weixiansen574.hybridfilexfer.core.bean.RemoteFile;
import top.weixiansen574.hybridfilexfer.core.bean.ServerNetInterface;
import top.weixiansen574.hybridfilexfer.core.bean.TrafficInfo;
import top.weixiansen574.hybridfilexfer.core.callback.TransferFileCallback;
import top.weixiansen574.hybridfilexfer.droidcore.HFXServer;

/**
 * 一个「分享发送」任务：状态机 + 发送线程，<b>不依赖任何 Activity</b>。
 *
 * <p>改造的原因（用户报的两个问题）：以前这些状态全在 {@code ShareActivity} 的实例字段里，
 * 而那个 Activity 是 {@code singleTop} 的单实例 —— 一个实例承载所有分享任务，第二次分享就把
 * 上一轮取消掉重开，两个任务的面板状态互相覆盖；Activity 一旦被销毁，任务和它的面板就都没了。
 * 现在：
 * <ul>
 *   <li>任务活在 {@link SendTasks} 的表里，Activity 只是「某个 taskId 的视图」，可以同时存在多个；</li>
 *   <li>面板（{@code ShareActivity}）被收起 / 销毁，传输照跑（{@link #run()} 这条线程不认识界面）；</li>
 *   <li>暂停 / 继续 / 取消都按任务作用在本对象上，不会误伤别的任务。</li>
 * </ul>
 *
 * <p><b>并发</b>：{@code core/} 的发送调度不能动，一条连接上同一时刻只能有一次传输。
 * 所以打到<b>同一台电脑</b>上的多个任务靠 {@link SendTasks#acquire} 排队；
 * 打到不同电脑上的任务是真正并发的。被用户暂停着的任务占着通道不放的话，
 * 排队者会请它让出（{@link #requestHandover()}：保留进度的取消），
 * 所以「暂停任务1 → 发任务2 → 回头继续任务1」这条路走得通。
 *
 * <p><b>状态机</b>：
 * <pre>
 *   PREPARING ──► WAITING ──► SENDING ⇄ PAUSED
 *       │            │            │
 *       │            │            └─► SUSPENDED（把通道让给别的任务，保留进度）
 *       │            │                     │
 *       │            │                     └─（继续）─► WAITING ──► SENDING …
 *       └────────────┴──────────────────────────────► DONE / FAILED / CANCELLED
 * </pre>
 */
public final class SendTask {

    /** 解析 / 复制文件（还没连电脑） */
    public static final int STATE_PREPARING = 0;
    /** 等电脑连接 / 等别的任务让出这条连接的传输通道 */
    public static final int STATE_WAITING = 1;
    /** 正在传 */
    public static final int STATE_SENDING = 2;
    /** 用户按了暂停（传输线程卡在闸门上，通道还占着） */
    public static final int STATE_PAUSED = 3;
    /** 已让出通道（另一个任务要用），进度留在电脑上，点继续接着传 */
    public static final int STATE_SUSPENDED = 4;
    public static final int STATE_DONE = 5;
    public static final int STATE_FAILED = 6;
    public static final int STATE_CANCELLED = 7;

    private static final String TAG = "SendTask";
    /** 等电脑连上最多等这么久 */
    private static final long CONNECT_TIMEOUT_MS = 30_000L;
    /** 等这条连接上一次传输真的退出（它现在正被取消）最多等这么久 */
    private static final long PREV_IDLE_TIMEOUT_MS = 10_000L;

    /** 「这一轮被取消 / 被让出 / 被电脑端取消」的哨兵值：和真正的错误文案区分开 */
    private static final String ERR_ABANDONED = "\u0000abandoned";
    private static final String ERR_SUSPENDED = "\u0000suspended";
    /** 电脑端点「取消 → 保留进度」：传输作废，但不是错误，也不该提示"请返回重连" */
    private static final String ERR_PEER_CANCELLED = "\u0000peercancelled";

    public final int taskId;
    private final long createdAt = System.currentTimeMillis();
    private final Context appContext;
    private final Handler main = new Handler(Looper.getMainLooper());

    /** 要发的文件（Uri 形式，准备线程消费一次就转换成 File） */
    private final List<Uri> uris = new ArrayList<>();
    /** 多台电脑时指定发给哪台（-1 = 随便一台） */
    private volatile int targetConnectionId;

    private final Object stateLock = new Object();
    private final Object pauseLock = new Object();
    /** 运行线程的互斥锁（同一个任务同一时刻只有一条运行线程，见 {@link #run()}） */
    private final Object runLock = new Object();

    private volatile int state = STATE_PREPARING;
    private volatile boolean pausedByUser;
    private volatile boolean cancelled;
    private volatile boolean terminal;
    private volatile boolean prepared;
    private volatile HFXServer server;
    /** 这条任务现在有没有占着连接的发送通道（{@link SendTasks.Gate} 里的 holder 就是它） */
    private volatile boolean holdsSlot;
    /** 占着哪条连接的通道（-1 = 没占着） */
    private volatile int slotConnectionId = -1;
    private final AtomicBoolean suspendRequested = new AtomicBoolean();
    /** 正在让出通道（另一个任务要用）：传输线程看到它就别把 abort 当成真错误 */
    private volatile boolean handoverActive;
    /** 让出通道那条线程：新的运行线程要等它把 abort 收干净再开跑 */
    private volatile Thread handoverThread;

    // ---- 准备结果：文件、分组、总量 ----
    private final List<File> files = new ArrayList<>();
    private final LinkedList<Group> groups = new LinkedList<>();
    private volatile long totalBytes;
    /** 已经整组发完的字节（组与组之间记账，当前组按块的真值另外取） */
    private volatile long completedBytes;
    /** 这一轮由「断点续传」跳过的字节（电脑上已经有了，不在 sendProgress 的账本里） */
    private volatile long resumeSkipped;
    private volatile long currentGroupTotal;
    /** 已经并进 completedBytes 的那个账本属于哪个文件：识别「陈旧快照」，别把一组算两遍 */
    private volatile String accountedLedgerPath;

    // ---- 面板 / 通知要的文字 ----
    private volatile String statusText = "";
    private volatile String resultText = "";
    private volatile String remoteTarget = "";
    private volatile String currentFileName = "";
    private volatile long startTime;
    private volatile long peakSpeedBytes;

    private volatile int sentFileCount;
    private final List<String> sentFileNames = new ArrayList<>();
    private final Map<String, Double> channelSpeeds = new LinkedHashMap<>();
    private final java.util.Set<String> doneChannels =
            Collections.newSetFromMap(new java.util.concurrent.ConcurrentHashMap<String, Boolean>());

    /** 绑在这个任务上的面板（同一时刻应该只有一个；多出来的自己 finish 掉） */
    private final List<WeakReference<ShareActivity>> views = new CopyOnWriteArrayList<>();

    SendTask(int taskId, Context appContext, int targetConnectionId) {
        this.taskId = taskId;
        this.appContext = appContext;
        this.targetConnectionId = targetConnectionId;
    }

    // ------------------------------------------------------------------
    // 生命周期 / 控制
    // ------------------------------------------------------------------

    /** 收下要发的文件并开始跑（准备线程）。一个任务只调用一次。 */
    public void start(List<Uri> sources) {
        if (sources != null) {
            uris.addAll(sources);
        }
        statusText = appContext.getString(R.string.share_preparing);
        startRun();
    }

    private void startRun() {
        Thread thread = new Thread(this::run, "SendTask-" + taskId);
        thread.start();
    }

    /** 面板上的「暂停发送」：关掉暂停闸门（传输线程每处理一块检查一次） */
    public void pause() {
        HFXServer srv = server;
        synchronized (pauseLock) {
            pausedByUser = true;
        }
        synchronized (stateLock) {
            if (terminal) {
                return;
            }
            if (state == STATE_SENDING) {
                state = STATE_PAUSED;
                if (srv != null) {
                    srv.pauseTransfer();
                }
            }
        }
    }

    /**
     * 面板上的「继续」。
     *
     * <ul>
     *   <li>PAUSED：放开闸门，原来那条发送线程接着跑（它一直没退出）；</li>
     *   <li>SUSPENDED（为了给别的任务让路，通道已经被关掉）：重新起一条运行线程，
     *       从没发完的那一组接着发 —— 电脑端留着 {@code .hfxpart}，
     *       {@code sendFilesToRemote} 会先问「已经收到哪些块」，只补缺的；</li>
     *   <li>WAITING / PREPARING：本来就在排队，什么都不用做。</li>
     * </ul>
     */
    public void resume() {
        HFXServer srv = server;
        synchronized (pauseLock) {
            pausedByUser = false;
            pauseLock.notifyAll();
        }
        boolean restart = false;
        synchronized (stateLock) {
            if (terminal) {
                return;
            }
            if (state == STATE_PAUSED) {
                state = STATE_SENDING;
                if (srv != null) {
                    srv.resumeTransfer();
                }
                return;
            }
            if (state == STATE_SUSPENDED) {
                state = STATE_WAITING;
                statusText = queueNote();
                restart = true;
            }
        }
        if (restart) {
            startRun();
        }
    }

    /**
     * 取消任务。
     *
     * @param keepProgress true = 保留电脑上已经收到的部分（下次同一个文件续传）；
     *                     false = 让电脑端把不完整文件和 {@code .hfxpart} 一起删掉
     */
    public void cancel(boolean keepProgress) {
        HFXServer srv;
        synchronized (stateLock) {
            if (terminal) {
                return;
            }
            terminal = true;
            cancelled = true;
            pausedByUser = false;
            state = STATE_CANCELLED;
            statusText = "";
            srv = server;
        }
        resultText = keepProgress
                ? appContext.getString(R.string.share_cancelled) + "（保留进度，下次接着传）"
                : appContext.getString(R.string.share_cancelled) + "（已让电脑删除未完成的文件）";
        synchronized (pauseLock) {
            pauseLock.notifyAll();
        }
        // 只有「这条任务自己占着通道」时才去关传输通道：
        // 同一个 taskId 之外的任务可能正在这台电脑上传，乱 abort 会把它一起打死
        if (srv != null && holdsSlot && srv.isTransferring()) {
            try {
                ServerService.cancelTransfer(srv.getId(), !keepProgress);
            } catch (Throwable e) {
                HFXLog.d("task " + taskId + ": cancelTransfer 失败", e);
            }
        }
        SendTasks.onSettled(this);
    }

    // ------------------------------------------------------------------
    // 面板绑定
    // ------------------------------------------------------------------

    /**
     * 把一个面板绑到这个任务上。同一个任务只留一个面板：老的那个自己 finish 掉
     * （用户点通知回来、或者面板被系统回收后重建，都不会出现两个窗口叠着）。
     */
    public void bind(ShareActivity activity) {
        ShareActivity stale = null;
        for (WeakReference<ShareActivity> ref : views) {
            ShareActivity other = ref.get();
            if (other != null && other != activity) {
                stale = other;
            }
        }
        views.add(new WeakReference<>(activity));
        if (stale != null) {
            HFXLog.d("task " + taskId + ": 旧面板被新面板顶掉");
            try {
                stale.finish();
            } catch (Throwable ignored) {
            }
        }
    }

    /**
     * 面板销毁。
     *
     * <p>和以前 {@code ShareActivity#onDestroy} 同一条规矩：<b>传输中不取消</b>
     * （任务线程不认识界面，文件照发），还没开传（准备 / 排队）就取消掉。
     * 让出通道的 SUSPENDED 不在这儿取消 —— 那是「收起后台」，用户随时可以从通知回来继续。
     */
    public void unbind(ShareActivity activity) {
        boolean removed = false;
        for (WeakReference<ShareActivity> ref : views) {
            if (ref.get() == activity) {
                views.remove(ref);
                removed = true;
                break;
            }
        }
        if (!removed || hasViews()) {
            return;
        }
        int current = state;
        if (!terminal && (current == STATE_PREPARING || current == STATE_WAITING)) {
            cancel(true);
            return;
        }
        // 还在传（SENDING / PAUSED / SUSPENDED）：面板关掉只是「界面没了」，任务照跑。
        // 以前这里直接走 onSettled，把正在传的任务从队列表里删掉 —— 表现就是：
        // 分享第 1 个文件、把面板收起来、再分享第 2 个文件，第 2 个的面板里**看不到第 1 个**
        // （任务其实还在后台传，通知栏那条进度也一起没了）。
        if (!terminal) {
            HFXLog.d("task " + taskId + ": 面板关掉，任务继续（留在队列里）");
            return;
        }
        SendTasks.onSettled(this);
    }

    public boolean hasViews() {
        for (WeakReference<ShareActivity> ref : views) {
            if (ref.get() != null) {
                return true;
            }
        }
        return false;
    }

    // ------------------------------------------------------------------
    // 闸门回调（SendTasks 调的）
    // ------------------------------------------------------------------

    void onSlotAcquired(int connectionId) {
        holdsSlot = true;
        slotConnectionId = connectionId;
        // 「排队的时候就被暂停了」：现在拿到通道了，把它认成 PAUSED，
        // 这样别的任务要通道时能请它让出去（不会一直卡着队列）
        synchronized (stateLock) {
            if (!terminal && pausedByUser && state == STATE_WAITING) {
                state = STATE_PAUSED;
            }
        }
    }

    int getSlotConnectionId() {
        return slotConnectionId;
    }

    void onSlotWaiting() {
        HFXServer srv = server;
        synchronized (stateLock) {
            if (terminal || state == STATE_PAUSED || state == STATE_SUSPENDED) {
                return;
            }
            state = STATE_WAITING;
            // 这里在闸门锁里（SendTasks#acquire 的 synchronized(gate)）：不去算队列位次，
            // 文案由 getStatusText() 按当前位次现取
            statusText = "";
        }
        if (srv != null) {
            HFXLog.d("task " + taskId + ": 排队等 " + srv.getDisplayName() + " 的发送通道");
        }
    }

    void onSlotReleased() {
        holdsSlot = false;
        slotConnectionId = -1;
    }

    /**
     * 请这条任务让出传输通道（另一个任务要在这台电脑上发）。
     *
     * <p>只在「用户暂停着」这种占着通道却不发数据的状态下才会被叫到。
     * 让出 = 保留进度的取消（{@code abortTransfer(false)}）：电脑上那份半截文件和
     * {@code .hfxpart} 都留着，点继续时只补缺的块。
     */
    void requestHandover() {
        if (!suspendRequested.compareAndSet(false, true)) {
            return;
        }
        Thread thread = new Thread(() -> {
            try {
                suspendForHandover();
            } catch (Throwable e) {
                HFXLog.d("task " + taskId + ": 让出通道失败", e);
                suspendRequested.set(false);
            }
        }, "SendHandover-" + taskId);
        handoverThread = thread;
        thread.start();
    }

    private void suspendForHandover() {
        HFXServer srv = server;
        synchronized (stateLock) {
            if (terminal || (state != STATE_SENDING && state != STATE_PAUSED)) {
                suspendRequested.set(false);
                return;
            }
            state = STATE_SUSPENDED;
            handoverActive = true;
            statusText = "";
        }
        // 把「暂停」标记收掉并叫醒传输线程：
        // 它可能正停在 awaitUnpaused 里等暂停结束，不叫醒它就会一直占着通道不放
        synchronized (pauseLock) {
            pausedByUser = false;
            pauseLock.notifyAll();
        }
        HFXLog.d("task " + taskId + ": 让出传输通道（保留进度）");
        // 只有真的在传才需要关传输通道；只是「暂停着还没开跑」的话，
        // 上面把 pausedByUser 收掉就已经让它退出去了（abort 会顺带让电脑端重建通道）
        if (srv != null && holdsSlot && srv.isTransferring()) {
            srv.abortTransfer(false);
        }
    }

    // ------------------------------------------------------------------
    // 运行
    // ------------------------------------------------------------------

    /**
     * 运行线程入口：<b>同一个任务同一时刻只允许一条运行线程</b>。
     *
     * <p>为什么要这把锁：让出通道（SUSPENDED）之后用户马上点「继续」时，上一条运行线程
     * 可能还在收尾（abortTransfer 抛异常 → transferGroup 返回 → finally 里 release 通道）。
     * 新线程如果这时候直接去 acquire，拿到的还是同一个 task 占着的闸门，
     * 而旧线程的 release 会把它清掉 —— 两个任务就会同时往一条连接上发。
     * 锁住之后新线程会等旧线程彻底退出（包括 release）再开始，代价只是多停几毫秒。
     */
    private void run() {
        synchronized (runLock) {
            runInner();
        }
    }

    private void runInner() {
        try {
            // 上一条「让出通道」的线程可能还在 abortTransfer 里收尾：等它收干净再开跑，
            // 否则新传输和那次 abort 会同时动同一批传输通道
            Thread pendingHandover = handoverThread;
            handoverThread = null;
            if (pendingHandover != null && pendingHandover != Thread.currentThread()) {
                try {
                    pendingHandover.join(3000L);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            }
            handoverActive = false;
            suspendRequested.set(false);
            if (isAbandoned() || state == STATE_SUSPENDED) {
                return;
            }
            if (!prepared) {
                prepareFiles();
                prepared = true;
            }
            if (isAbandoned()) {
                return;
            }
            if (groups.isEmpty()) {
                fail(appContext.getString(R.string.share_no_file));
                return;
            }
            if (!ensureConnected()) {
                if (!isAbandoned()) {
                    fail(appContext.getString(R.string.share_pc_not_connected));
                }
                return;
            }
            HFXServer srv = server;
            if (srv == null) {
                fail(appContext.getString(R.string.share_pc_not_connected));
                return;
            }
            // 这条连接的发送通道：同一时刻只放一个任务进去（另一个任务正在传就在这里排队）
            if (!SendTasks.acquire(this, srv)) {
                return;
            }
            try {
                // 通道拿到了，但上一条传输线程可能还没退干净（transferring 是「这次传输」的标志，
                // 上一轮退出时会无条件清掉 —— 新的已经开跑的话它清的就是新的，帧流会读串）
                if (!awaitServerIdle(srv)) {
                    if (!isAbandoned()) {
                        fail(appContext.getString(R.string.share_busy));
                    }
                    return;
                }
                runGroups(srv);
            } finally {
                SendTasks.release(this);
            }
        } catch (Throwable e) {
            HFXLog.d("task " + taskId + ": 运行异常", e);
            if (!isAbandoned()) {
                fail(String.valueOf(e));
            }
        }
    }

    private void runGroups(HFXServer srv) {
        while (true) {
            if (isAbandoned() || state == STATE_SUSPENDED || handoverActive) {
                return;
            }
            Group group = groups.peek();
            if (group == null) {
                complete();
                return;
            }
            String error = transferGroup(srv, group);
            if (ERR_ABANDONED.equals(error) || ERR_SUSPENDED.equals(error)) {
                return;
            }
            if (ERR_PEER_CANCELLED.equals(error)) {
                // 电脑端点「取消 → 保留进度」：收成「已取消（未完成）」，不是「发送失败」
                finishPeerCancelled();
                return;
            }
            if (error != null) {
                fail(error);
                return;
            }
            groups.poll();
            completedBytes += group.totalBytes;
            // 续传跳过的字节已经算在 group.totalBytes 里了（那是整组的字节数），
            // 不清掉它就等于把这一组算两遍 —— 真机实测「300 MB 的文件显示 589 MB / 300 MB」，
            // 队列行、通知栏的数字跟着一起错。
            resumeSkipped = 0L;
            // 这一组的字节已经并进 completedBytes，账本却还停在它上面：
            // 记下这个路径，采样时按它识别「陈旧快照」，别把同一组算两遍
            accountedLedgerPath = srv.sendProgress.currentPath();
            if (groups.isEmpty()) {
                complete();
                return;
            }
        }
    }

    /**
     * 发一组（同一个目录下的文件）。
     *
     * @return null = 成功；{@link #ERR_ABANDONED} / {@link #ERR_SUSPENDED} /
     *         {@link #ERR_PEER_CANCELLED} 三个哨兵（都不是失败）；
     *         其它字符串 = 要写进面板的失败原因
     */
    private String transferGroup(HFXServer srv, Group group) {
        currentGroupTotal = group.totalBytes;
        resumeSkipped = 0L;
        currentFileName = group.firstFileName();
        doneChannels.clear();
        if (startTime == 0L) {
            startTime = System.currentTimeMillis();
        }
        awaitUnpaused();
        if (isAbandoned()) {
            return ERR_ABANDONED;
        }
        synchronized (stateLock) {
            if (terminal) {
                return ERR_ABANDONED;
            }
            if (state == STATE_SUSPENDED) {
                return ERR_SUSPENDED;
            }
            state = STATE_SENDING;
            statusText = "";
        }
        Outcome outcome = new Outcome();
        try {
            Directory localDir = new Directory(group.dir, Directory.getCurrentFileSystem());
            // 目标目录：面板那行「保存到：…」显示的就是它（和真正发往的是同一份计算）
            String remoteBase = resolveRemoteBase();
            if (remoteBase.isEmpty()) {
                remoteBase = srv.getRemoteHomeDir();
            }
            remoteTarget = remoteBase;
            Directory remoteDir = new Directory(remoteBase, srv.getRemoteFileSystem());
            HFXLog.d("task " + taskId + ": 开始发送 dir=" + group.dir
                    + " files=" + group.files.size() + " total=" + group.totalBytes);
            // 同步发完这一组（以前走 BackstageTask，本质上也是另起一条线程，这里直接用来当运行线程：
            // 组与组之间、任务与任务之间的先后关系才是要管的东西）
            srv.sendFilesToRemote(group.files, localDir, remoteDir, new Callback(outcome));
        } catch (Throwable e) {
            if (isAbandoned()) {
                return ERR_ABANDONED;
            }
            // 让出通道时传输是被我们自己 abort 掉的：那不是「传输出错」，不要写成失败
            if (handoverActive || state == STATE_SUSPENDED) {
                return ERR_SUSPENDED;
            }
            // 电脑端点了「取消 → 保留进度」：控制通道/传输通道的异常都是这次取消的连带结果，
            // 不是本端的错误（HFXService 已经把这次传输标成 peerCancelled）
            if (isPeerCancelledRound(srv)) {
                HFXLog.d("task " + taskId + ": 电脑端已取消，异常按取消收尾：" + e);
                return ERR_PEER_CANCELLED;
            }
            HFXLog.d("task " + taskId + ": 发送异常", e);
            return String.valueOf(e);
        }
        // 电脑端取消优先于其它结论：它可能同时留下一条"读文件中断/写失败"的回调，
        // 那些都是取消的连带结果，别拿来当失败原因
        if (isPeerCancelledRound(srv)) {
            return ERR_PEER_CANCELLED;
        }
        if (outcome.error != null) {
            if (handoverActive || state == STATE_SUSPENDED) {
                return ERR_SUSPENDED;
            }
            return outcome.error;
        }
        if (!outcome.completed) {
            if (isAbandoned()) {
                return ERR_ABANDONED;
            }
            if (handoverActive || state == STATE_SUSPENDED) {
                return ERR_SUSPENDED;
            }
            // 原来这里直接返回「传输时发生异常，请返回重连！」—— 电脑端点「取消 → 保留进度」
            // 走的正是这条路（取消 → 本次传输作废 → onIncomplete → 没有 error 文案），
            // 于是手机上弹出一句「发送失败：传输时发生异常，请返回重连！」，
            // 而实际上通道好好的（取消后电脑会把传输通道重建回来）。
            if (isPeerCancelledRound(srv)) {
                return ERR_PEER_CANCELLED;
            }
            return appContext.getString(R.string.chuan_shu_shi_fa_sheng_yi_chang);
        }
        return null;
    }

    /**
     * 这一轮是不是「电脑端点了取消」造成的作废。
     *
     * <p>只看 {@link HFXServer#isPeerCancelled()}（对端取消专用标志），<b>不能</b>看
     * {@code isTransferCancelled()}：那个是「本端也取消」的宽泛标志，让出通道
     * （{@code abortTransfer}）和用户点取消都会置它，会把「让出通道」误判成电脑端取消。
     */
    private boolean isPeerCancelledRound(HFXServer srv) {
        if (srv == null) {
            return false;
        }
        try {
            return srv.isPeerCancelled();
        } catch (Throwable e) {
            return false;
        }
    }

    private void awaitUnpaused() {
        synchronized (pauseLock) {
            while (pausedByUser && !isAbandoned()) {
                try {
                    pauseLock.wait(300L);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    return;
                }
            }
        }
    }

    /**
     * 等这条连接上一次的传输真的退出（两个标志都要等）。
     *
     * <p>为什么非等不可：{@code HFXService} 的 {@code transferring} / {@code sendingFiles} 是
     * 「这一次传输」的标志，上一轮退出时会在 {@code finally} 里无条件清掉 —— 如果新的这一轮已经
     * 开始，它清掉的就是新那一轮的标志，会话线程于是又去读控制通道，两个读者一起把帧流读串；
     * 传输通道本身也是同一批，不能两轮同时用。
     *
     * <p>等不到就不能硬上：越过它开跑等于主动制造「控制通道两个读者」。
     * 所以超时以后这一轮直接以一条明确的失败收场（用户再分享一次即可）。
     */
    private boolean awaitServerIdle(HFXServer srv) {
        long deadline = System.currentTimeMillis() + PREV_IDLE_TIMEOUT_MS;
        while ((srv.isTransferring() || srv.isSendingFiles())
                && System.currentTimeMillis() < deadline && !isAbandoned()) {
            try {
                Thread.sleep(50L);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return false;
            }
        }
        return !srv.isTransferring() && !srv.isSendingFiles();
    }

    private void complete() {
        long displayPeak;
        synchronized (stateLock) {
            if (terminal) {
                return;
            }
            terminal = true;
            state = STATE_DONE;
            statusText = "";
            pausedByUser = false;
            displayPeak = peakSpeedBytes;
        }
        String summary = buildFileSummary();
        if (displayPeak <= 0 && totalBytes > 0 && startTime > 0) {
            long elapsedMs = Math.max(1L, System.currentTimeMillis() - startTime);
            displayPeak = totalBytes * 1000L / elapsedMs;
        }
        final long peakForToast = displayPeak;
        try {
            Config config = Config.getInstance(appContext);
            config.setLastPeakBytesPerSecond(displayPeak);
            StringBuilder speedText = new StringBuilder();
            synchronized (channelSpeeds) {
                for (Map.Entry<String, Double> entry : channelSpeeds.entrySet()) {
                    if (speedText.length() > 0) {
                        speedText.append(",");
                    }
                    speedText.append(entry.getKey()).append("=")
                            .append(String.format(java.util.Locale.US, "%.1f", entry.getValue()));
                }
            }
            config.setChannelSpeeds(speedText.toString());
        } catch (Throwable e) {
            HFXLog.d("task " + taskId + ": 记录峰值速率失败", e);
        }
        resultText = appContext.getString(R.string.share_complete_files, summary,
                Utils.formatSpeed(peakForToast));
        HFXLog.d("task " + taskId + ": 完成 " + resultText);
        main.post(() -> {
            try {
                Toast.makeText(appContext, appContext.getString(R.string.share_complete_files,
                        summary, Utils.formatSpeed(peakForToast)), Toast.LENGTH_SHORT).show();
            } catch (Throwable ignored) {
            }
        });
        SendTasks.onSettled(this);
    }

    /**
     * 电脑端点「取消 → 保留进度」后的收尾：状态记「已取消」，结果文案写「未完成，可续传」。
     *
     * <p>和用户在本机点取消一样，<b>不是失败</b>：不弹「发送失败」、不出现「请返回重连」
     * （电脑端取消后会把传输通道重建回来，会话根本不断）。电脑上已经收到的块和
     * {@code .hfxpart} 都留着，用户重新分享同一个文件就是从断点接着传。
     */
    private void finishPeerCancelled() {
        synchronized (stateLock) {
            if (terminal) {
                return;
            }
            terminal = true;
            state = STATE_CANCELLED;
            statusText = "";
            pausedByUser = false;
        }
        String text = appContext.getString(R.string.share_peer_cancelled);
        resultText = text;
        HFXLog.d("task " + taskId + ": " + text);
        try {
            main.post(() -> {
                try {
                    // 电脑端取消不是用户在本机点的，面板 800ms 就自己收起来了，
                    // 用长一点的 toast 保证用户能看清「已取消 / 可续传」
                    Toast.makeText(appContext, text, Toast.LENGTH_LONG).show();
                } catch (Throwable ignored) {
                }
            });
        } catch (Throwable ignored) {
        }
        SendTasks.onSettled(this);
    }

    private void fail(String message) {
        synchronized (stateLock) {
            if (terminal) {
                return;
            }
            terminal = true;
            state = STATE_FAILED;
            statusText = "";
            pausedByUser = false;
        }
        Log.w(TAG, "task " + taskId + " 失败: " + message);
        HFXLog.d("task " + taskId + ": 失败 " + message);
        resultText = appContext.getString(R.string.share_failed, message);
        SendTasks.onSettled(this);
    }

    /** 这一轮已经作废（取消 / 收尾）：准备线程、发送线程看到它就该收工 */
    boolean isAbandoned() {
        return cancelled || terminal;
    }

    // ------------------------------------------------------------------
    // 准备：Uri → File（可能需要复制到缓存）、连电脑、算目标目录
    // ------------------------------------------------------------------

    private void prepareFiles() {
        List<File> resolved = new ArrayList<>();
        long total = 0L;
        for (int i = 0; i < uris.size(); i++) {
            if (isAbandoned()) {
                return;
            }
            int index = i + 1;
            statusText = appContext.getString(R.string.share_resolving_file, index, uris.size());
            File file = resolve(uris.get(i));
            if (file != null && file.exists()) {
                resolved.add(file);
                if (!file.isDirectory()) {
                    total += file.length();
                }
            }
        }
        if (isAbandoned()) {
            return;
        }
        if (resolved.isEmpty()) {
            return;
        }
        files.clear();
        files.addAll(resolved);
        totalBytes = total;
        // 同一个文件夹里的文件一起发，这样电脑上的文件名保持原样
        Map<String, List<RemoteFile>> byParent = new LinkedHashMap<>();
        for (File file : files) {
            String parent = file.getParent();
            if (TextUtils.isEmpty(parent)) {
                parent = "/";
            }
            List<RemoteFile> list = byParent.get(parent);
            if (list == null) {
                list = new ArrayList<>();
                byParent.put(parent, list);
            }
            list.add(new RemoteFile(file));
        }
        groups.clear();
        synchronized (sentFileNames) {
            sentFileNames.clear();
        }
        sentFileCount = 0;
        for (Map.Entry<String, List<RemoteFile>> entry : byParent.entrySet()) {
            long groupTotal = 0L;
            List<String> names = new ArrayList<>();
            for (RemoteFile file : entry.getValue()) {
                if (!file.isDirectory()) {
                    groupTotal += file.getSize();
                    names.add(file.getName());
                }
            }
            sentFileCount += names.size();
            synchronized (sentFileNames) {
                for (String name : names) {
                    if (sentFileNames.size() < 3) {
                        sentFileNames.add(name);
                    }
                }
            }
            groups.add(new Group(entry.getKey(), groupTotal, entry.getValue(), names));
        }
        HFXLog.d("task " + taskId + ": 准备完成 files=" + files.size()
                + " groups=" + groups.size() + " total=" + totalBytes);
    }

    private boolean ensureConnected() {
        if (ServerService.isConnected()) {
            server = ServerService.getServer(targetConnectionId);
            if (server != null) {
                return true;
            }
        }
        statusText = appContext.getString(R.string.share_waiting_pc);
        if (!ServerService.isRunning()) {
            try {
                Config config = Config.getInstance(appContext);
                List<ServerNetInterface> list = NetInterfaces.forAutoConnect(appContext);
                if (!list.isEmpty()) {
                    ServerService.startServer(appContext, config.getMode(), list,
                            config.getLocalBufferCount(), config.getRemoteBufferCount());
                }
            } catch (Throwable e) {
                Log.w(TAG, "start server failed", e);
            }
        }
        long deadline = System.currentTimeMillis() + CONNECT_TIMEOUT_MS;
        while (System.currentTimeMillis() < deadline && !isAbandoned()) {
            if (ServerService.isConnected()) {
                server = ServerService.getServer(targetConnectionId);
                if (server != null) {
                    return true;
                }
            }
            try {
                Thread.sleep(250L);
            } catch (InterruptedException e) {
                return false;
            }
        }
        return false;
    }

    /** 电脑上的目标目录：{@code AbsDir}（为空时用接收器的 home）+ {@code SubDir} */
    private String resolveRemoteBase() {
        HFXServer srv = server;
        if (srv == null) {
            return "";
        }
        String remoteBase = Config.getInstance(appContext).getRemoteAbsDir();
        if (remoteBase == null || remoteBase.trim().isEmpty()) {
            remoteBase = srv.getRemoteHomeDir();
        }
        if (remoteBase == null) {
            return "";
        }
        String subDir = Config.getInstance(appContext).getRemoteSubDir();
        if (subDir != null && !subDir.trim().isEmpty()) {
            String sub = subDir.replace('\\', '/');
            while (sub.startsWith("/")) sub = sub.substring(1);
            while (sub.endsWith("/")) sub = sub.substring(0, sub.length() - 1);
            if (!sub.isEmpty()) {
                if (remoteBase.endsWith("/") || remoteBase.endsWith("\\")) {
                    remoteBase = remoteBase + sub;
                } else {
                    remoteBase = remoteBase + "/" + sub;
                }
            }
        }
        return remoteBase;
    }

    private String buildFileSummary() {
        List<String> names;
        synchronized (sentFileNames) {
            names = new ArrayList<>(sentFileNames);
        }
        if (names.isEmpty()) {
            return appContext.getString(R.string.share_files_fallback);
        }
        StringBuilder builder = new StringBuilder();
        for (int i = 0; i < names.size(); i++) {
            if (i > 0) {
                builder.append("、");
            }
            builder.append(names.get(i));
        }
        if (sentFileCount > names.size()) {
            builder.append(" 等 ").append(sentFileCount).append(" 个文件");
        }
        return builder.toString();
    }

    // ------------------------------------------------------------------
    // 传输回调（发送线程 / 通道线程都会回调，这里只写几个 volatile 字段）
    // ------------------------------------------------------------------

    private static final class Outcome {
        volatile boolean completed;
        volatile String error;
    }

    private final class Callback implements TransferFileCallback {
        private final Outcome outcome;

        Callback(Outcome outcome) {
            this.outcome = outcome;
        }

        @Override
        public void onFileUploading(String iName, String path, long targetSize, long totalSize) {
            // 只记文件名。进度不在回调里算：分段发送时 targetSize 是「这一块的末尾位置」，
            // 块是乱序并行到达的，拿它当进度会跳来跳去
            if (path != null && !path.isEmpty()) {
                currentFileName = new File(path).getName();
            }
        }

        @Override
        public void onFileDownloading(String iName, String path, long targetSize, long totalSize) {
        }

        @Override
        public void onSpeedInfo(List<TrafficInfo> trafficInfoList) {
            long bytes = 0L;
            for (TrafficInfo info : trafficInfoList) {
                bytes += info.uploadTraffic;
            }
            // 只留峰值：屏幕上的速率由面板按块真值的增量自己算
            if (bytes > peakSpeedBytes) {
                peakSpeedBytes = bytes;
            }
        }

        @Override
        public void onChannelComplete(String iName, long traffic, long time) {
            if (iName != null) {
                doneChannels.add(iName);
            }
            if (time > 0) {
                double mbps = traffic / 1024.0 / 1024.0 / (time / 1000.0);
                synchronized (channelSpeeds) {
                    channelSpeeds.put(iName, mbps);
                }
            }
        }

        @Override
        public void onChannelError(String iName, int errorType, String message) {
            if (errorType == TransferFileCallback.ERROR_TYPE_EXCEPTION && !TextUtils.isEmpty(message)) {
                Log.w(TAG, "task " + taskId + " 通道 " + iName + ": " + message);
            }
        }

        @Override
        public void onReadFileError(String message) {
            outcome.completed = false;
            outcome.error = message;
        }

        @Override
        public void onWriteFileError(String message) {
            outcome.completed = false;
            outcome.error = message;
        }

        @Override
        public void onComplete(boolean isUpload, long traffic, long time) {
            if (isUpload) {
                outcome.completed = true;
            }
        }

        @Override
        public void onResumeSkipped(long bytes) {
            // 续传：电脑上已经有这么多字节了，进度基线要接上（不然进度条从 0 开始涨）
            if (bytes > resumeSkipped) {
                resumeSkipped = bytes;
            }
        }

        @Override
        public void onIncomplete() {
            outcome.completed = false;
        }
    }

    // ------------------------------------------------------------------
    // 面板 / 通知读的快照
    // ------------------------------------------------------------------

    public int getTaskId() {
        return taskId;
    }

    public long getCreatedAt() {
        return createdAt;
    }

    public int getState() {
        return state;
    }

    public boolean isTerminal() {
        int current = state;
        return terminal || current == STATE_DONE || current == STATE_FAILED || current == STATE_CANCELLED;
    }

    /** 「正在传」的三个状态（面板要画暂停 / 继续 / 取消那颗按钮） */
    public boolean isTransferActive() {
        int current = state;
        return current == STATE_SENDING || current == STATE_PAUSED || current == STATE_SUSPENDED;
    }

    public boolean isSending() {
        return state == STATE_SENDING;
    }

    public boolean isPausedByUser() {
        return pausedByUser;
    }

    public boolean isSuspended() {
        return state == STATE_SUSPENDED;
    }

    public boolean isWaiting() {
        int current = state;
        return current == STATE_PREPARING || current == STATE_WAITING;
    }

    public HFXServer getServer() {
        return server;
    }

    /** 面板 / 通知上那台电脑的名字 */
    public String getLabel() {
        HFXServer srv = server;
        return srv == null ? "" : srv.getDisplayName();
    }

    public String getStatusText() {
        // 排队中那句「排队中（前面还有 N 个任务）」里的 N 是会变的：
        // 位次当场从队列快照里取，别用开始排队那一刻记下的旧数字
        if (state == STATE_WAITING) {
            return queueNote();
        }
        return statusText;
    }

    /** 排队中面板上那一行：「排队中（前面还有 N 个任务）」，N = 同一条连接上排在它前面的任务数 */
    private String queueNote() {
        int ahead = SendTasks.aheadCount(taskId);
        // N = 0 说明它自己就是队头（在等的是电脑上的别的传输，不是本队列里的任务）：
        // 「前面还有 0 个任务」读着别扭，就说「排队中」
        return ahead <= 0 ? "排队中" : "排队中（前面还有 " + ahead + " 个任务）";
    }

    public String getResultText() {
        return resultText;
    }

    public String getCurrentFileName() {
        return currentFileName;
    }

    public String getRemoteTarget() {
        return remoteTarget;
    }

    public long getTotalBytes() {
        return totalBytes;
    }

    public int getSentFileCount() {
        return sentFileCount;
    }

    public boolean isChannelDone(String name) {
        return name != null && doneChannels.contains(name);
    }

    /**
     * 按块真值已经发出去多少字节：已完成的组 + 断点续传跳过的 + 当前组 {@code sendProgress} 的账本。
     *
     * <p>和面板上那个数同一口径（面板在这个数上再加「只增不减」和补间）。
     */
    public long getDoneBytes() {
        long done = completedBytes + resumeSkipped;
        HFXServer srv = server;
        if (srv != null) {
            try {
                BlockProgress.Snapshot snapshot = srv.sendProgress.snapshot();
                String path = srv.sendProgress.currentPath();
                String accounted = accountedLedgerPath;
                if (snapshot != null && (accounted == null || !accounted.equals(path))) {
                    done += snapshot.doneBytes;
                }
            } catch (Throwable ignored) {
                // 传输刚结束 / 账本正在被清空：这一拍就用已完成组的字节
            }
        }
        return done;
    }

    /**
     * 队列 / 通知用的已传字节。
     *
     * <p>和 {@link #getDoneBytes()} 的区别：{@code sendProgress} 是<b>这条连接上正在传的那个任务</b>
     * 的账本，排队的任务读它会把别人的进度算成自己的。所以只有「正在用这本账」的任务
     * （在传 / 暂停着占着通道）才加上它，其余只报自己已经整组发完的字节。
     */
    public long getQueueDoneBytes() {
        int current = state;
        if (current == STATE_SENDING || current == STATE_PAUSED) {
            return getDoneBytes();
        }
        return completedBytes + resumeSkipped;
    }

    /** 队列 / 通知要的文件名：正在传的那个，还没开传就用准备阶段记下的第一个 */
    public String getDisplayFileName() {
        String current = currentFileName;
        if (current != null && !current.isEmpty()) {
            return current;
        }
        synchronized (sentFileNames) {
            return sentFileNames.isEmpty() ? "" : sentFileNames.get(0);
        }
    }

    /** 当前拍的块快照（分段进度条用），取不到返回 null */
    public BlockProgress.Snapshot sendSnapshot() {
        HFXServer srv = server;
        if (srv == null) {
            return null;
        }
        try {
            return srv.sendProgress.snapshot();
        } catch (Throwable ignored) {
            return null;
        }
    }

    private static final class Group {
        final String dir;
        final long totalBytes;
        final List<RemoteFile> files;
        final List<String> names;

        Group(String dir, long totalBytes, List<RemoteFile> files, List<String> names) {
            this.dir = dir;
            this.totalBytes = totalBytes;
            this.files = files;
            this.names = names;
        }

        String firstFileName() {
            return names.isEmpty() ? "" : names.get(0);
        }
    }

    // ------------------------------------------------------------------
    // Uri -> File
    // ------------------------------------------------------------------

    private File resolve(Uri uri) {
        try {
            String scheme = uri.getScheme();
            if ("file".equalsIgnoreCase(scheme)) {
                String path = uri.getPath();
                if (!TextUtils.isEmpty(path)) {
                    File file = new File(path);
                    if (file.exists()) {
                        return file;
                    }
                }
                return null;
            }
            if ("content".equalsIgnoreCase(scheme)) {
                String path = resolveContentPath(uri);
                if (!TextUtils.isEmpty(path)) {
                    File file = new File(path);
                    if (file.exists()) {
                        return file;
                    }
                }
                return copyToCache(uri);
            }
        } catch (Throwable e) {
            Log.w(TAG, "cannot resolve " + uri, e);
        }
        return null;
    }

    private String resolveContentPath(Uri uri) {
        try {
            if (DocumentsContract.isDocumentUri(appContext, uri)) {
                String authority = uri.getAuthority();
                String documentId = DocumentsContract.getDocumentId(uri);
                if ("com.android.externalstorage.documents".equals(authority)) {
                    String[] parts = documentId.split(":");
                    if (parts.length >= 2) {
                        if ("primary".equalsIgnoreCase(parts[0])) {
                            return Environment.getExternalStorageDirectory().getAbsolutePath()
                                    + "/" + parts[1];
                        }
                        return "/storage/" + parts[0] + "/" + parts[1];
                    }
                } else if ("com.android.providers.downloads.documents".equals(authority)) {
                    if (documentId.startsWith("raw:")) {
                        return documentId.substring(4);
                    }
                }
                return null;
            }
            try (Cursor cursor = appContext.getContentResolver().query(uri,
                    new String[]{MediaStore.MediaColumns.DATA}, null, null, null)) {
                if (cursor != null && cursor.moveToFirst()) {
                    int index = cursor.getColumnIndex(MediaStore.MediaColumns.DATA);
                    if (index >= 0) {
                        return cursor.getString(index);
                    }
                }
            }
        } catch (Throwable e) {
            Log.d(TAG, "resolveContentPath: " + e);
        }
        return null;
    }

    private File copyToCache(Uri uri) throws IOException {
        String name = queryDisplayName(uri);
        File dir = new File(appContext.getCacheDir(), "share");
        if (!dir.exists() && !dir.mkdirs()) {
            throw new IOException("cannot create " + dir);
        }
        File out = uniqueFile(dir, name);
        long copied = 0L;
        long lastUpdate = 0L;
        try (InputStream in = appContext.getContentResolver().openInputStream(uri)) {
            if (in == null) {
                throw new IOException("cannot open " + uri);
            }
            try (FileOutputStream fos = new FileOutputStream(out)) {
                byte[] buffer = new byte[256 * 1024];
                int read;
                while ((read = in.read(buffer)) > 0) {
                    if (isAbandoned()) {
                        break;
                    }
                    fos.write(buffer, 0, read);
                    copied += read;
                    if (copied - lastUpdate > 8L * 1024 * 1024) {
                        lastUpdate = copied;
                        statusText = appContext.getString(R.string.share_copying_file,
                                Utils.formatFileSize(copied));
                    }
                }
            }
        }
        return out;
    }

    private String queryDisplayName(Uri uri) {
        String name = null;
        try (Cursor cursor = appContext.getContentResolver().query(uri,
                new String[]{OpenableColumns.DISPLAY_NAME}, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                int index = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                if (index >= 0) {
                    name = cursor.getString(index);
                }
            }
        } catch (Throwable ignored) {
        }
        if (TextUtils.isEmpty(name)) {
            name = uri.getLastPathSegment();
        }
        if (TextUtils.isEmpty(name)) {
            name = "shared_file";
        }
        name = name.replaceAll("[\\\\/:*?\"<>|]", "_");
        if (name.length() > 120) {
            name = name.substring(name.length() - 120);
        }
        return name;
    }

    private File uniqueFile(File dir, String name) {
        File file = new File(dir, name);
        int i = 1;
        while (file.exists()) {
            file = new File(dir, i + "_" + name);
            i++;
        }
        return file;
    }

    /** 把一段「Intent」里的文件收下来（分享 / 打开方式） */
    public static List<Uri> extractUris(Intent intent) {
        List<Uri> list = new ArrayList<>();
        if (intent == null) {
            return list;
        }
        String action = intent.getAction();
        if (Intent.ACTION_SEND.equals(action)) {
            Uri uri = getParcelableUri(intent);
            if (uri != null) {
                list.add(uri);
            }
        } else if (Intent.ACTION_SEND_MULTIPLE.equals(action)) {
            ArrayList<Uri> uris = getParcelableUriList(intent);
            if (uris != null) {
                list.addAll(uris);
            }
        } else if (Intent.ACTION_VIEW.equals(action)) {
            Uri data = intent.getData();
            if (data != null) {
                list.add(data);
            }
        }
        if (list.isEmpty() && intent.getClipData() != null) {
            ClipData clipData = intent.getClipData();
            for (int i = 0; i < clipData.getItemCount(); i++) {
                Uri uri = clipData.getItemAt(i).getUri();
                if (uri != null) {
                    list.add(uri);
                }
            }
        }
        return list;
    }

    @SuppressWarnings("deprecation")
    private static Uri getParcelableUri(Intent intent) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            return intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri.class);
        }
        return intent.getParcelableExtra(Intent.EXTRA_STREAM);
    }

    @SuppressWarnings("deprecation")
    private static ArrayList<Uri> getParcelableUriList(Intent intent) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            return intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri.class);
        }
        return intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM);
    }
}
