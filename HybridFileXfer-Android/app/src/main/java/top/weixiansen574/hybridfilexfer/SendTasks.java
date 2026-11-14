package top.weixiansen574.hybridfilexfer;

import android.content.Context;
import android.content.Intent;

import java.util.ArrayList;
import java.util.Collections;
import java.util.Comparator;
import java.util.List;
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
            }
        }
    }

    // ------------------------------------------------------------------
    // 连接闸门：同一台电脑上同一时刻只放一个发送任务
    // ------------------------------------------------------------------

    /**
     * 申请某台电脑的发送通道，拿到返回 true；任务被取消返回 false。
     *
     * <p>排队期间任务的状态是 WAITING（面板上写着「等待其他任务完成…」），不是失败。
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
