package top.weixiansen574.hybridfilexfer;

import android.content.Context;
import android.content.Intent;
import android.os.SystemClock;

import java.util.ArrayList;
import java.util.Collections;
import java.util.Comparator;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicInteger;

import top.weixiansen574.hybridfilexfer.droidcore.HFXServer;

/**
 * 「分享发送」任务会话表：一个发送任务一个 {@link SendTask}，全进程一份。
 *
 * <p>为什么要有这张表（以前没有）：{@code ShareActivity} 是 {@code singleTop} 的单实例，
 * 第二次分享走 {@code onNewIntent} 把上一轮<b>取消掉</b>再开新的 —— 「暂停任务1、再发任务2」
 * 就等于任务1被顶掉，两个任务的面板状态互相覆盖，屏幕上只剩一层空壳。
 * 现在任务状态活在服务侧（这个表里），Activity 只是「某个 taskId 的视图」：
 * <ul>
 *   <li>每个分享任务都有独立的 taskId，可以并存；</li>
 *   <li>Activity 消失（收起 / 被系统回收）任务照传；</li>
 *   <li>通知栏点回来按 taskId 落到对应任务的面板（面板没了就重新造一个绑上去）。</li>
 * </ul>
 *
 * <p><b>并发边界</b>：{@code core/} 的发送调度不能改，一条连接上同一时刻只能有一次传输
 * （控制通道只有一个读者、传输通道是同一批）。所以这里按「连接」加闸门 {@link Gate}：
 * 同一个 taskId 之外的任务打到同一台电脑上就排队（{@link #acquire}）；
 * 打到<b>不同</b>电脑上的任务是真正并发的（各自一条会话、各自的账本和速率）。
 */
public final class SendTasks {

    private static final AtomicInteger NEXT_ID = new AtomicInteger(1);
    private static final ConcurrentHashMap<Integer, SendTask> TASKS = new ConcurrentHashMap<>();
    /** 每条连接一把闸门：key = 连接号（{@code HFXServer#getId()}） */
    private static final ConcurrentHashMap<Integer, Gate> GATES = new ConcurrentHashMap<>();

    private SendTasks() {
    }

    /** 一条连接上的发送闸门：同一时刻只放一个任务进去 */
    static final class Gate {
        SendTask holder;
    }

    static Gate gateFor(int connectionId) {
        Gate gate = GATES.get(connectionId);
        if (gate == null) {
            Gate created = new Gate();
            Gate existing = GATES.putIfAbsent(connectionId, created);
            gate = existing != null ? existing : created;
        }
        return gate;
    }

    /**
     * 新建一个发送任务（还没开始跑，调用方紧接着要 {@link SendTask#start()}）。
     *
     * <p>顺手回收「没人能看见」的旧任务：面板已经关掉、状态也已经终结的任务留着没有意义；
     * 让出通道以后面板又被关掉的（SUSPENDED）也收掉 —— 它等的那个「继续」永远不会来了
     * （电脑上的半截文件 + .hfxpart 还在，用户重新分享同一个文件照样续传）。
     */
    public static SendTask create(Context context, int targetConnectionId) {
        prune();
        int taskId = NEXT_ID.getAndIncrement();
        SendTask task = new SendTask(taskId, context.getApplicationContext(), targetConnectionId);
        TASKS.put(taskId, task);
        return task;
    }

    public static SendTask get(int taskId) {
        return TASKS.get(taskId);
    }

    /** 所有还活着的任务，按 taskId 升序（= 创建顺序） */
    public static List<SendTask> snapshot() {
        List<SendTask> list = new ArrayList<>(TASKS.values());
        Collections.sort(list, Comparator.comparingInt(SendTask::getTaskId));
        return list;
    }

    /**
     * 通知栏要显示的那些任务：真正在传的、被用户暂停的、以及为了给别的任务让路而停住的。
     *
     * <p>排队的（WAITING）和准备中的不算 —— 它们还没有进度可言。
     * 让出通道的（SUSPENDED）<b>算</b>：它等着用户点「继续」，如果它的面板已经被收起到后台，
     * 通知栏就是唯一能回到它的入口（挂在这里会显示「已暂停」）。
     * 最近创建的排前面（聚合标题点进去落到的就是它）。
     */
    public static List<SendTask> transferringTasks() {
        List<SendTask> list = new ArrayList<>();
        for (SendTask task : TASKS.values()) {
            int state = task.getState();
            if (state == SendTask.STATE_SENDING || state == SendTask.STATE_PAUSED
                    || state == SendTask.STATE_SUSPENDED) {
                list.add(task);
            }
        }
        Collections.sort(list, (a, b) -> Integer.compare(b.getTaskId(), a.getTaskId()));
        return list;
    }

    static void onSettled(SendTask task) {
        if (task.hasViews()) {
            // 面板还开着：留着记录，用户还能从通知 / 重新进来看到结果文案
            return;
        }
        TASKS.remove(task.getTaskId());
        SPEEDS.remove(task.getTaskId());
    }

    private static void prune() {
        for (SendTask task : TASKS.values()) {
            if (task.hasViews()) {
                continue;
            }
            int state = task.getState();
            if (state == SendTask.STATE_DONE || state == SendTask.STATE_FAILED
                    || state == SendTask.STATE_CANCELLED
                    || state == SendTask.STATE_SUSPENDED
                    || state == SendTask.STATE_WAITING) {
                TASKS.remove(task.getTaskId());
                SPEEDS.remove(task.getTaskId());
            }
        }
    }

    // ------------------------------------------------------------------
    // 「任务队列」：面板和通知读的只读快照
    // ------------------------------------------------------------------

    /**
     * 队列里的一行，<b>只读</b>：字段全是 final，取的时候一次性拷好，
     * 界面线程读它不会读到任务状态改到一半的中间态（任务字段本身是跨线程读写）。
     */
    public static final class TaskInfo {
        public final int taskId;
        /** 文件名：正在传的那个，还没开传就用准备阶段记下的第一个 */
        public final String fileName;
        public final long totalBytes;
        /** 已传字节：只算自己的账本（排队的任务读不到正在传那个任务的账本） */
        public final long doneBytes;
        public final int state;
        /** 目标电脑名（还没连上是空串） */
        public final String label;
        /** 同一条连接上「前面还有几个任务」（含正在传的那个） */
        public final int aheadCount;
        /** 排队中的第几个（0 = 下一个就轮到它）；没在排队是 -1 */
        public final int queueIndex;
        /** 速率（字节/秒，只有正在传的那个才有值） */
        public final long speed;

        TaskInfo(int taskId, String fileName, long totalBytes, long doneBytes, int state,
                 String label, int aheadCount, int queueIndex, long speed) {
            this.taskId = taskId;
            this.fileName = fileName == null ? "" : fileName;
            this.totalBytes = totalBytes;
            this.doneBytes = doneBytes;
            this.state = state;
            this.label = label == null ? "" : label;
            this.aheadCount = aheadCount;
            this.queueIndex = queueIndex;
            this.speed = speed;
        }

        /** 正在用这条连接的发送通道：在传 / 暂停着占通道 / 让出通道 */
        public boolean isActive() {
            return state == SendTask.STATE_SENDING || state == SendTask.STATE_PAUSED
                    || state == SendTask.STATE_SUSPENDED;
        }

        /** 还在排队（准备中 / 等通道） */
        public boolean isQueued() {
            return state == SendTask.STATE_WAITING || state == SendTask.STATE_PREPARING;
        }

        public boolean isFinished() {
            return state == SendTask.STATE_DONE || state == SendTask.STATE_FAILED
                    || state == SendTask.STATE_CANCELLED;
        }

        public int percent() {
            if (totalBytes <= 0L) {
                return 0;
            }
            return (int) Math.max(0, Math.min(100, doneBytes * 100L / totalBytes));
        }
    }

    /**
     * 队列里的速率采样（每个任务一份）。
     *
     * <p>面板每 100ms 取一次快照、通知每 500ms 取一次，两边共用一个采样点：
     * 只有两次间隔够长时才重算，别让高频调用把间隔切碎、算出一串离谱值。
     */
    private static final Map<Integer, SpeedSample> SPEEDS = new ConcurrentHashMap<>();
    private static final long SPEED_MIN_INTERVAL_MS = 200L;

    private static final class SpeedSample {
        long bytes;
        long at;
        long speed;
    }

    private static long sampleSpeed(int taskId, long done, boolean active, long now) {
        if (!active) {
            // 没在传就不是「速率」，顺手把采样点扔掉（下次开传从 0 起算）
            SPEEDS.remove(taskId);
            return 0L;
        }
        SpeedSample sample = SPEEDS.get(taskId);
        if (sample == null) {
            sample = new SpeedSample();
            sample.bytes = done;
            sample.at = now;
            SPEEDS.put(taskId, sample);
            return 0L;
        }
        // 换文件 / 续传重算时 done 会变小：重新起算，别把上一段算进这一秒
        if (done < sample.bytes) {
            sample.bytes = done;
            sample.at = now;
            sample.speed = 0L;
            return 0L;
        }
        long elapsed = now - sample.at;
        if (elapsed < SPEED_MIN_INTERVAL_MS) {
            return sample.speed;
        }
        long instant = (done - sample.bytes) * 1000L / Math.max(1L, elapsed);
        sample.speed = sample.speed <= 0L ? instant : (sample.speed * 2L + instant) / 3L;
        sample.bytes = done;
        sample.at = now;
        return sample.speed;
    }

    /** 队列的展示顺序：正在传的最前，然后排队中的（按创建顺序），收尾了的垫底 */
    private static int rank(int state) {
        switch (state) {
            case SendTask.STATE_SENDING:
                return 0;
            case SendTask.STATE_PAUSED:
                return 1;
            case SendTask.STATE_SUSPENDED:
                return 2;
            case SendTask.STATE_WAITING:
                return 3;
            case SendTask.STATE_PREPARING:
                return 4;
            default:
                return 5;
        }
    }

    /** 这个任务占的是哪条连接的通道（还没连上是 -1，都归到「未知」那一组） */
    private static int connectionKey(SendTask task) {
        int slot = task.getSlotConnectionId();
        if (slot >= 0) {
            return slot;
        }
        HFXServer srv = task.getServer();
        return srv == null ? -1 : srv.getId();
    }

    /**
     * 任务队列快照：<b>只读</b>，面板（ShareActivity）和通知（ServerService）都读它。
     *
     * <p>顺序就是队列顺序：正在传的在最前，然后按创建顺序排队（= 「谁在前」），
     * 已经收尾但面板还开着的垫底。
     *
     * <p>「前面还有几个」按<b>同一条连接</b>算：闸门（{@link Gate}）是一条连接一把，
     * 打到不同电脑上的任务是并发的，彼此不算在对方的队列里。
     */
    public static List<TaskInfo> queueSnapshot() {
        List<SendTask> tasks = snapshot();
        if (tasks.isEmpty()) {
            return Collections.emptyList();
        }
        // 先按连接分组，组内排出「谁占着、谁第几个等」：位置下标就是「前面还有几个」
        Map<Integer, List<SendTask>> groups = new LinkedHashMap<>();
        for (SendTask task : tasks) {
            int key = connectionKey(task);
            List<SendTask> group = groups.get(key);
            if (group == null) {
                group = new ArrayList<>();
                groups.put(key, group);
            }
            group.add(task);
        }
        Map<Integer, int[]> positions = new HashMap<>();
        for (List<SendTask> group : groups.values()) {
            Collections.sort(group, (a, b) -> {
                int rankA = rank(a.getState());
                int rankB = rank(b.getState());
                if (rankA != rankB) {
                    return Integer.compare(rankA, rankB);
                }
                return Integer.compare(a.getTaskId(), b.getTaskId());
            });
            int queueIndex = 0;
            for (int i = 0; i < group.size(); i++) {
                SendTask task = group.get(i);
                int state = task.getState();
                boolean queued = state == SendTask.STATE_WAITING || state == SendTask.STATE_PREPARING;
                positions.put(task.getTaskId(), new int[]{queued ? i : 0, queued ? queueIndex : -1});
                if (queued) {
                    queueIndex++;
                }
            }
        }
        long now = SystemClock.elapsedRealtime();
        List<TaskInfo> out = new ArrayList<>(tasks.size());
        for (SendTask task : tasks) {
            int[] position = positions.get(task.getTaskId());
            int state = task.getState();
            long done = task.getQueueDoneBytes();
            out.add(new TaskInfo(
                    task.getTaskId(),
                    task.getDisplayFileName(),
                    task.getTotalBytes(),
                    done,
                    state,
                    task.getLabel(),
                    position == null ? 0 : position[0],
                    position == null ? -1 : position[1],
                    sampleSpeed(task.getTaskId(), done, state == SendTask.STATE_SENDING, now)));
        }
        Collections.sort(out, (a, b) -> {
            int rankA = rank(a.state);
            int rankB = rank(b.state);
            if (rankA != rankB) {
                return Integer.compare(rankA, rankB);
            }
            return Integer.compare(a.taskId, b.taskId);
        });
        return out;
    }

    /**
     * 通知栏点一下应该落到哪个任务的面板（没有任务就返回 -1）。
     *
     * <p>为什么要这个而不只是用「通知上正在显示的那个任务」：通知只有在<b>有任务占着传输通道</b>
     * 时才带进度（{@code ServerService#sendTaskView} 的 anyActive），准备中 / 排队中 /
     * 刚好收尾的那几个瞬间都没有进度可显示 —— 以前那种时候 contentIntent 会退回
     * 主界面（{@code hfx_show_transfer}），用户点了通知看到的就是「App 主页」。
     *
     * <p>顺序直接用队列顺序（{@link #queueSnapshot()}：正在传的在最前、排队的跟上、
     * 收尾的垫底）：取第一个还没收尾的，全都收尾了就取队头那一个 ——
     * 这样点通知落到的永远是队列里最"该看"的那个任务的面板。
     */
    public static int panelTargetTaskId() {
        List<TaskInfo> infos = queueSnapshot();
        if (infos.isEmpty()) {
            return -1;
        }
        for (TaskInfo info : infos) {
            if (!info.isFinished()) {
                return info.taskId;
            }
        }
        return infos.get(0).taskId;
    }

    /**
     * 某个任务前面还有几个任务（面板上「排队中（前面还有 N 个任务）」用它）。
     *
     * <p>任务已经不在表里（刚收尾）时返回 0：文案里少一个数字比抛异常好。
     */
    public static int aheadCount(int taskId) {
        for (TaskInfo info : queueSnapshot()) {
            if (info.taskId == taskId) {
                return info.aheadCount;
            }
        }
        return 0;
    }

    // ------------------------------------------------------------------
    // 连接闸门：同一台电脑上同一时刻只放一个发送任务
    // ------------------------------------------------------------------

    /**
     * 申请某台电脑的发送通道，拿到返回 true；任务被取消返回 false。
     *
     * <p>排队期间任务的状态是 WAITING（面板上写着「排队中（前面还有 N 个任务）」），不是失败。
     *
     * <p><b>让出</b>：如果此刻占着通道的那个任务正被用户暂停着（闸门关着、传输线程阻塞在读块上），
     * 它不主动退出就谁也进不来。这里发现这种情况就请它让出（{@link SendTask#requestHandover()}：
     * 保留进度的取消），它退出后本方法拿到通道。
     */
    static boolean acquire(SendTask task, HFXServer server) {
        Gate gate = gateFor(server.getId());
        synchronized (gate) {
            while (true) {
                if (task.isAbandoned()) {
                    return false;
                }
                SendTask holder = gate.holder;
                if (holder == null || holder == task) {
                    gate.holder = task;
                    task.onSlotAcquired(server.getId());
                    return true;
                }
                if (holder.isAbandoned()) {
                    // 占着通道那个任务已经取消了，它马上就会在 finally 里 release
                    gate.holder = null;
                    continue;
                }
                if (holder.isPausedByUser() || holder.isSuspended()) {
                    // 「暂停 A 再发 B」：请 A 让出通道（保留进度），A 退出后这里就能进去
                    holder.requestHandover();
                }
                task.onSlotWaiting();
                try {
                    gate.wait(300L);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    return false;
                }
            }
        }
    }

    /** 让出 / 结束：唤醒所有排队者 */
    static void release(SendTask task) {
        int connectionId = task.getSlotConnectionId();
        task.onSlotReleased();
        if (connectionId < 0) {
            return;
        }
        Gate gate = GATES.get(connectionId);
        if (gate == null) {
            return;
        }
        synchronized (gate) {
            if (gate.holder == task) {
                gate.holder = null;
            }
            gate.notifyAll();
        }
    }

    // ------------------------------------------------------------------
    // 面板 / 通知的入口
    // ------------------------------------------------------------------

    /** 打开（或重建）某个任务的进度面板 */
    public static Intent panelIntent(Context context, int taskId) {
        Intent intent = new Intent(context, ShareActivity.class);
        intent.putExtra(ShareActivity.EXTRA_TASK_ID, taskId);
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        return intent;
    }

    public static void openPanel(Context context, int taskId) {
        if (context == null) {
            return;
        }
        try {
            context.startActivity(panelIntent(context, taskId));
        } catch (Throwable ignored) {
            // 后台限制 / Activity 被禁用：通知点了没反应总比崩溃好
        }
    }

    // ------------------------------------------------------------------
    // 按 taskId 的传输控制（ServerService 里那几个静态入口的 taskId 版本）
    // ------------------------------------------------------------------

    public static void pauseTask(int taskId) {
        SendTask task = TASKS.get(taskId);
        if (task != null) {
            task.pause();
        }
    }

    public static void resumeTask(int taskId) {
        SendTask task = TASKS.get(taskId);
        if (task != null) {
            task.resume();
        }
    }

    public static void cancelTask(int taskId, boolean discardPartial) {
        SendTask task = TASKS.get(taskId);
        if (task != null) {
            task.cancel(!discardPartial);
        }
    }

    public static boolean isTaskTransferring(int taskId) {
        SendTask task = TASKS.get(taskId);
        return task != null && task.isTransferActive();
    }
}
