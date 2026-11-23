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

import top.weixiansen574.hybridfilexfer.core.HFXLog;
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
        /**
         * 排队者的到达顺序（{@link SendTask} 对象身份去重）。
         *
         * <p>为什么要有它：{@code notifyAll()} 醒来之后谁抢到锁是随机的 —— 真机上出现过
         * 「任务 3 抢在任务 2 前面开跑」，同一个文件队列传完的顺序和面板上排的顺序不一致。
         * 现在只有队头能进：醒了看一眼自己是不是第一个，不是就接着等。
         */
        final java.util.LinkedHashSet<SendTask> waiters = new java.util.LinkedHashSet<>();
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
        // 排「队列里少一个任务」这类问题时，这一行能看到建新任务时表里还剩谁
        HFXLog.d("queue: 新建 task " + taskId + " → " + digest());
        return task;
    }

    /**
     * 诊断用：队列里都有谁、什么状态、面板还开着没。
     *
     * <p>「任务1在传、任务2的面板里却看不到任务1」这种报告，靠这行一眼能分清是
     * 「表里没有」还是「渲染没画」。
     */
    static String digest() {
        List<SendTask> list = snapshot();
        if (list.isEmpty()) {
            return "空";
        }
        StringBuilder sb = new StringBuilder();
        for (SendTask task : list) {
            if (sb.length() > 0) {
                sb.append(", ");
            }
            sb.append(task.getTaskId()).append(':').append(stateName(task.getState()))
                    .append(task.hasViews() ? "(有面板)" : "(无面板)");
        }
        return sb.toString();
    }

    private static String stateName(int state) {
        switch (state) {
            case SendTask.STATE_PREPARING:
                return "PREPARING";
            case SendTask.STATE_WAITING:
                return "WAITING";
            case SendTask.STATE_SENDING:
                return "SENDING";
            case SendTask.STATE_PAUSED:
                return "PAUSED";
            case SendTask.STATE_SUSPENDED:
                return "SUSPENDED";
            case SendTask.STATE_DONE:
                return "DONE";
            case SendTask.STATE_FAILED:
                return "FAILED";
            case SendTask.STATE_CANCELLED:
                return "CANCELLED";
            default:
                return String.valueOf(state);
        }
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

    /** 队列里还有没收尾的任务吗（在传的 / 排队的 / 准备中的都算） */
    private static boolean anyLive() {
        for (SendTask task : TASKS.values()) {
            if (!task.isTerminal()) {
                return true;
            }
        }
        return false;
    }

    static void onSettled(SendTask task) {
        if (task.hasViews()) {
            // 面板还开着：留着记录，用户还能从通知 / 重新进来看到结果文案
            return;
        }
        if (!task.isTerminal()) {
            // 兜底：还在传的任务不能因为「没有面板」就从表里删掉 ——
            // 别的任务的面板要靠这张表列出队列，通知栏也要靠它挂进度，删了就再也找不回来。
            HFXLog.d("queue: task " + task.getTaskId() + " 没有面板但还在传，保留");
            return;
        }
        if (anyLive()) {
            // 多选分享 = 每个文件一个任务：这一个传完了，队列里还有别的要传（或者正传着），
            // 就把它留在列表里（队列末尾显示「已完成」）。删掉的话队列越传越短，
            // 用户既看不到前面那些传得怎么样，也没法从队列里回看结果。
            HFXLog.d("queue: task " + task.getTaskId() + " 收尾，队列里还有别的任务，留在列表里");
            return;
        }
        TASKS.remove(task.getTaskId());
        SPEEDS.remove(task.getTaskId());
    }

    private static void prune() {
        if (anyLive()) {
            // 还有任务在传 / 排队：收尾的那些当一个「已完成」留在队列里，别清列表
            return;
        }
        for (SendTask task : TASKS.values()) {
            if (task.hasViews()) {
                continue;
            }
            int state = task.getState();
            // WAITING 不在回收之列：那是「排队等发送通道」，运行线程还活着（阻塞在
            // SendTasks.acquire 里），轮到自己就会开跑。多选分享 = 每个文件一个任务，
            // 排队的那些本来就没有面板 —— 以前按「没面板就回收」删掉，队里就只剩正在传的那个，
            // 后面的文件再也传不了。
            if (state == SendTask.STATE_DONE || state == SendTask.STATE_FAILED
                    || state == SendTask.STATE_CANCELLED
                    || state == SendTask.STATE_SUSPENDED) {
                TASKS.remove(task.getTaskId());
                SPEEDS.remove(task.getTaskId());
                HFXLog.d("queue: 回收 task " + task.getTaskId() + "（" + stateName(state) + "，没有面板）");
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
    // 309：QUEUE_STATE —— 发给电脑的那份队列快照
    // ------------------------------------------------------------------

    /**
     * QUEUE_STATE 的状态码（线格式上定死的数值，和 {@code ControllerIdentifiers.QUEUE_STATE} 的注释一致）。
     *
     * <p>手机内部有 8 个状态（准备中 / 排队 / 在传 / 暂停 / 让出 / 完成 / 失败 / 取消），
     * 线上只有 4 个格子。折叠规则见 {@link #queueStateFor(int)}。
     */
    public static final int QUEUE_STATE_WAITING = 0;
    public static final int QUEUE_STATE_RUNNING = 1;
    public static final int QUEUE_STATE_DONE = 2;
    public static final int QUEUE_STATE_FAILED = 3;

    /**
     * 队列里的一行，<b>协议用</b>：只有线上要的那三个字段，字段全是 final。
     *
     * <p>为什么不直接把 {@link TaskInfo} 那种富对象递出去：协议帧的字段越少越不容易
     * 跟着界面需求变，电脑那边按这个结构读就行。
     */
    public static final class QueueStateEntry {
        /** 文件名（还没解析出文件时是空串） */
        public final String fileName;
        /** 这个任务的文件总字节数（不是已传字节，见 {@code ControllerIdentifiers.QUEUE_STATE}） */
        public final long bytes;
        /** {@link #QUEUE_STATE_WAITING} / RUNNING / DONE / FAILED */
        public final int state;

        QueueStateEntry(String fileName, long bytes, int state) {
            this.fileName = fileName == null ? "" : fileName;
            this.bytes = bytes;
            this.state = state;
        }
    }

    /**
     * 某台电脑那一侧的队列快照，给 {@code HFXServer#writeQueueState()} 用。
     *
     * <p><b>按连接过滤</b>：这张表是全进程一份的，一台手机可能同时连着几台电脑，
     * 发给 A 的那一帧只能有 A 要收的那几行（不然 A 的界面上会出现别人的文件）。
     * 还没落到某条连接上的任务（准备中、还没握手）不在这条连接里，等它连上再出现。
     *
     * <p>顺序和面板 / 通知一致（正在传的在最前，排队的跟上，收尾的垫底），
     * 电脑画出来的列表就和手机上一个样。
     *
     * <p>折叠规则：
     * <ul>
     *   <li>SENDING / PAUSED → 进行中：暂停着的那一行还占着传输通道，它对队列来说是"当前这个"；</li>
     *   <li>PREPARING / WAITING / SUSPENDED → 等待中：准备和排队本来就没开跑，
     *       SUSPENDED（把通道让给别人、保留进度）等的是"再轮到我"，也归这一格；</li>
     *   <li>DONE → 已完成；</li>
     *   <li>FAILED <b>和</b> CANCELLED → 失败：线上没有"取消"这一格，而取消和失败对电脑
     *       来说都是"这个文件没传成"，宁可少一格，也不自己发明第 4 个状态码之外的值。</li>
     * </ul>
     */
    public static List<QueueStateEntry> queueStateFor(int connectionId) {
        List<SendTask> tasks = new ArrayList<>();
        for (SendTask task : snapshot()) {
            if (connectionKey(task) == connectionId) {
                tasks.add(task);
            }
        }
        Collections.sort(tasks, (a, b) -> {
            int rankA = rank(a.getState());
            int rankB = rank(b.getState());
            if (rankA != rankB) {
                return Integer.compare(rankA, rankB);
            }
            return Integer.compare(a.getTaskId(), b.getTaskId());
        });
        List<QueueStateEntry> out = new ArrayList<>(tasks.size());
        for (SendTask task : tasks) {
            out.add(new QueueStateEntry(
                    task.getDisplayFileName(),
                    task.getTotalBytes(),
                    queueStateOf(task.getState())));
        }
        return out;
    }

    /** 手机内部状态 → QUEUE_STATE 的 4 个状态码（规则见 {@link #queueStateFor(int)}） */
    private static int queueStateOf(int state) {
        switch (state) {
            case SendTask.STATE_SENDING:
            case SendTask.STATE_PAUSED:
                return QUEUE_STATE_RUNNING;
            case SendTask.STATE_DONE:
                return QUEUE_STATE_DONE;
            case SendTask.STATE_FAILED:
            case SendTask.STATE_CANCELLED:
                return QUEUE_STATE_FAILED;
            default:
                // PREPARING / WAITING / SUSPENDED
                return QUEUE_STATE_WAITING;
        }
    }

    /**
     * 队列变了（加入 / 开始 / 完成 / 失败 / 取消）：立刻把新快照推给每台在线的电脑。
     *
     * <p>推到哪几台是各推各的：每台电脑只拿 {@link #queueStateFor(int)} 里属于自己那几行，
     * 所以这里直接广播给所有会话，不需要"这个任务是发给谁的"这种判断。
     *
     * <p>这里<b>不等</b>写完才返回：真正的写由
     * {@code HFXServer#postQueueState()} 丢给后台那条线程，传输出错时也不会把
     * 发送线程堵在网络写上。
     */
    static void notifyQueueChanged() {
        for (HFXServer server : HFXServer.snapshot()) {
            server.postQueueState();
        }
    }

    // ------------------------------------------------------------------
    // 连接闸门：同一台电脑上同一时刻只放一个发送任务
    // ------------------------------------------------------------------

    /**
     * 申请某台电脑的发送通道，拿到返回 true；任务被取消返回 false。
     *
     * <p>排队期间任务的状态是 WAITING（面板上写着「排队中（前面还有 N 个任务）」），不是失败。
     *
     * <p><b>暂停优先于排队</b>（3.7.0）：这条连接被暂停着的时候，队列里的下一个任务就地等着 ——
     * 既不进去，也不像以前那样请暂停着的持锁者让出通道。用户报的现象是「暂停了还在一个接一个地传」：
     * 多选分享是<b>一个文件一个任务</b>（见 ShareActivity#createTaskPerFile），暂停正在传的那个之后，
     * 原来那句 {@code requestHandover()} 会请它让出通道、下一个文件立刻顶上来 ——
     * 于是"暂停"只停住了这一个文件，后面的照样一个接一个往外发。
     *
     * <p>代价（有意为之）：暂停着的任务占着通道，新分享的任务也要等它继续 / 取消才开跑。
     * 以前这条路径是"暂停 A → 发 B → B 立刻跑、A 让出进度"，现在改成"暂停 A → B 排队等"，
     * 因为用户要的是"暂停就真的都停下来"。
     */
    static boolean acquire(SendTask task, HFXServer server) {
        Gate gate = gateFor(server.getId());
        synchronized (gate) {
            // 排队登记（按到达顺序）：下面每次都检查自己是不是队头，见 Gate#waiters
            gate.waiters.add(task);
            boolean got = false;
            try {
                while (true) {
                    if (task.isAbandoned()) {
                        return false;
                    }
                    SendTask holder = gate.holder;
                    if (queuePaused(task, holder, server)) {
                        // 暂停期间不启动新任务：就地等，用户点继续 / 取消后下一轮(≤300ms)自己就过去了
                        task.onSlotWaiting();
                        try {
                            gate.wait(300L);
                        } catch (InterruptedException e) {
                            Thread.currentThread().interrupt();
                            return false;
                        }
                        continue;
                    }
                    if (holder == null || holder == task) {
                        // 空出来了也得先问队头：不然「刚取消、刚让出」那一下谁抢到锁谁先跑
                        if (isHeadOfQueue(gate, task)) {
                            gate.holder = task;
                            gate.waiters.remove(task);
                            got = true;
                            task.onSlotAcquired(server.getId());
                            return true;
                        }
                        task.onSlotWaiting();
                        try {
                            gate.wait(300L);
                        } catch (InterruptedException e) {
                            Thread.currentThread().interrupt();
                            return false;
                        }
                        continue;
                    }
                    if (holder.isAbandoned()) {
                        // 占着通道那个任务已经取消了，它马上就会在 finally 里 release
                        gate.holder = null;
                        continue;
                    }
                    // 到这里 holder 一定**不在暂停**（上面两道判据都过了）：它只是还在跑这一单。
                    // 以前这里还有一句 {@code holder.requestHandover()}（请它保留进度让出通道）——
                    // 3.7.0 已删除，理由：
                    //   * 用户要的是「队列一个一个来」，而不是「新的顶掉旧的」。让出通道等于
                    //     把正在传的文件半路掐掉（电脑端还得重建通道），然后下一单从头开始；
                    //   * 它和暂停闸门是两条互相打架的路：闸门判定「暂停就别开跑」，handover 却在
                    //     「holder 暂停着」的时候把它踹走。真机上表现为「暂停一个任务，下一个自己开始」。
                    // 现在只剩一条路：holder 干完 → finally 里 release → 这里 notifyAll 醒来。
                    task.onSlotWaiting();
                    try {
                        gate.wait(300L);
                    } catch (InterruptedException e) {
                        Thread.currentThread().interrupt();
                        return false;
                    }
                }
            } finally {
                // 拿不到（取消 / 中断）就别留在排队表里：留着会让后面的任务永远等一个
                // 不会再有人来接的位置。拿到了（got）当然也不用留 —— 它已经是 holder。
                if (!got) {
                    gate.waiters.remove(task);
                }
            }
        }
    }

    /** 这个任务是不是排在最前面的那一个（{@link Gate#waiters} 的队头） */
    private static boolean isHeadOfQueue(Gate gate, SendTask task) {
        java.util.Iterator<SendTask> it = gate.waiters.iterator();
        return it.hasNext() && it.next() == task;
    }

    /**
     * 现在能不能把队列里的下一个任务放进去（3.7.0）。
     *
     * <p>看的是<b>已有的暂停标志</b>，没有新造开关：
     * <ul>
     *   <li>{@link HFXServer#isTransferPaused()}：会话级的暂停闸门。手机上点暂停
     *       （{@code SendTask#pause()} → {@code HFXServer#pauseTransfer()}）、电脑上点暂停
     *       （PAUSE_REPLY → {@code applyPcPause}）都作用在它上面，界面显示的"已暂停"也是它 ——
     *       一个连接只有一个暂停状态，队列要认的就是这一个；</li>
     *   <li>任务自己的 {@link SendTask#isPausedByUser()}：任务在<b>排队时</b>就被暂停了，
     *       那一刻闸门还没关（{@code pause()} 只在"正在传"时才关闸门），不看它就会把
     *       一个用户已经暂停的任务放进去开跑。</li>
     * </ul>
     *
     * <p>持锁者是不是暂停着也要看：闸门是会话级的，理论上三者一致；但任务在排队时被暂停
     * 这种先后顺序会让闸门晚一步关上，多看一眼不会有副作用，却少一条"暂停了还在传"的路。
     */
    private static boolean queuePaused(SendTask task, SendTask holder, HFXServer server) {
        if (task.isPausedByUser()) {
            return true;
        }
        if (holder != null && holder != task && holder.isPausedByUser()) {
            return true;
        }
        return server != null && server.isTransferPaused();
    }

    /**
     * 叫醒这条连接上所有排队等闸门的任务（暂停放开之后用）。
     *
     * <p>排队者平时是靠 {@code gate.wait(300)} 自己醒的，但暂停那一段是"闸门开着却谁也不动"
     * 的状态：每次醒来看一眼 {@link #queuePaused} 就又睡回去。所以暂停一放开，必须有人
     * 立刻 notifyAll，不然队列要等满一拍（300ms）才开始，真机上还会因为没人唤醒而完全不动。
     *
     * <p>connectionId &lt; 0（任务还没落到某条连接上）时按"全部"处理：那种情况下
     * 排队者自己也分不清该等哪把闸门，一起叫一遍最省事。
     */
    public static void notifyQueueGate(int connectionId) {
        for (Map.Entry<Integer, Gate> e : GATES.entrySet()) {
            if (connectionId >= 0 && e.getKey() != connectionId) {
                continue;
            }
            synchronized (e.getValue()) {
                e.getValue().notifyAll();
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
            // 取消的任务可能还留在排队表里（acquire 里那个 finally 一般已经清掉了，
            // 这里补一刀：它永远不会再来开跑了，占着队头等于把后面的人锁死）
            gate.waiters.remove(task);
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
