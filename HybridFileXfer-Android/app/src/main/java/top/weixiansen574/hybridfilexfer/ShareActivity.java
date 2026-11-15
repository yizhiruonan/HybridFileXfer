package top.weixiansen574.hybridfilexfer;

import android.content.Intent;
import android.graphics.Color;
import android.graphics.drawable.ColorDrawable;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.util.Log;
import android.view.MotionEvent;
import android.view.ViewGroup;
import android.view.WindowManager;

import androidx.appcompat.app.AppCompatActivity;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

import top.weixiansen574.hybridfilexfer.compose.ChannelStat;
import top.weixiansen574.hybridfilexfer.compose.QueueRow;
import top.weixiansen574.hybridfilexfer.compose.SegInfo;
import top.weixiansen574.hybridfilexfer.compose.ShareActions;
import top.weixiansen574.hybridfilexfer.compose.SharePanel;
import top.weixiansen574.hybridfilexfer.core.BlockProgress;
import top.weixiansen574.hybridfilexfer.core.Utils;
import top.weixiansen574.hybridfilexfer.core.HFXLog;
import top.weixiansen574.hybridfilexfer.core.HFXService;
import top.weixiansen574.hybridfilexfer.droidcore.HFXServer;

/**
 * 分享 / 打开方式里的「发送到电脑」：<b>某个发送任务的面板</b>。
 *
 * <p>这个类现在只做视图：任务（文件、连接、进度、暂停、取消、生命周期）全在
 * {@link SendTask} 里，由 {@link SendTasks} 这张会话表托着。所以：
 * <ul>
 *   <li><b>一个任务一个实例</b>：{@code launchMode} 从 singleTop 改成 standard ——
 *       第二次分享是「新任务 + 新弹窗」，不会再走 {@code onNewIntent} 把上一轮顶掉
 *       （用户报的「暂停任务1再发任务2，新弹窗只剩一行灰字」就是单实例 + 全局面板状态
 *       互相覆盖造成的）；</li>
 *   <li>intent 里带 {@link #EXTRA_TASK_ID}：通知栏点回来按它落到<b>对应那个任务</b>的面板；
 *       面板已经被划掉了就新建一个实例绑到同一个 taskId，进度接着显示；</li>
 *   <li>面板收起（点遮罩 / 返回键）只 {@code moveTaskToBack}，任务线程不受影响。</li>
 * </ul>
 *
 * <p>渲染仍然是那一套：每 100ms（vsync 节拍）从任务取一次按块真值，交给
 * {@link SharePanel}（内部做 200ms 补间）。补间、速率、分段条都还是原来的口径。
 */
public class ShareActivity extends AppCompatActivity {

    private static final String TAG = "ShareActivity";
    /**
     * 要显示哪个发送任务的面板。
     *
     * <p>没有这个 extra = 这是一次新的分享（新建任务）；有 = 显示 / 重建某个已有任务的面板
     * （通知栏点回来、面板被系统回收后重建）。
     */
    public static final String EXTRA_TASK_ID = "top.weixiansen574.hybridfilexfer.extra.SEND_TASK_ID";
    /** 多台电脑时指定发给哪一台（连接号；不传或 -1 表示随便一台） */
    public static final String EXTRA_CONNECTION_ID = "top.weixiansen574.hybridfilexfer.extra.CONNECTION_ID";

    private static final String STATE_TASK_ID = "hfx_share_task_id";
    /** 真值采样节拍：100ms 采一次，只取数、只做算术，不碰界面（补间交给面板） */
    private static final long TICK_MS = 100L;
    /** 速率采样窗口：最近 600ms 内的字节差 ÷ 时间就是一个「平均值」 */
    private static final long SPEED_WINDOW_MS = 600L;
    /** 真值多久没涨就认为「确实没有数据在流」 */
    private static final long SPEED_IDLE_MS = 700L;
    private static final long SPEED_IDLE_MAX_MS = 15_000L;
    /** 速率上限：刚连上/重连那一拍会算出十几 GB/s 的离谱值，这种不采信 */
    private static final long SPEED_MAX_BYTES_PER_SEC = 2000L * 1024 * 1024;

    private final Handler handler = new Handler(Looper.getMainLooper());
    /** 逐帧节拍（vsync）：每帧看一眼有没有到 100ms 的采样点，见 FrameClock */
    private final FrameClock frameClock = new FrameClock(this::onFrame);

    private SharePanel panel;
    /** 这个面板显示的任务（任务本身活在 SendTasks 里，不跟着 Activity 生死） */
    private volatile SendTask task;
    private int taskId = -1;
    /** 已经「收起」到后台（传输中点了遮罩 / 返回键）：这时再点什么都不要再把 Activity 收掉 */
    private volatile boolean collapsed;
    /**
     * 面板是何时绑上任务的（elapsedRealtime 用 uptimeMillis 那套时钟）。
     *
     * <p>从通知栏点过来时，「按在通知上」那一下的 UP 会在面板刚出现时以 ACTION_OUTSIDE
     * 递进来（窗口是 WRAP_CONTENT 小窗）。这个时间戳用来把那一下放过，别让用户
     * 一点通知面板就把自己关掉。
     */
    private volatile long shownAt;
    /** 窗口刚起多久内的「点外面」不算用户操作（毫秒）：盖住通知栏那一下的 UP 抖动 */
    private static final long OUTSIDE_GRACE_MS = 600L;
    /** 已经排过「延时自动关窗口」 */
    private boolean finishingScheduled;

    // ---- 进度真值 / 速率状态：只在主线程里读写 ----
    private final ArrayDeque<long[]> speedSamples = new ArrayDeque<>();
    private long lastSampleBytes;
    private long lastGrowAt;
    private long lastGrowGapMs;
    private long smoothedSpeed;
    private long truthBytes;
    private long truthTotal;
    private boolean verifyPhase;
    private long shownBytes;
    private BlockProgress.Snapshot lastSnapshot;
    private long lastSampleAt;
    private boolean speedStateStarted;
    private boolean waitingPc;
    private final Map<String, Long> channelLastBytes = new LinkedHashMap<>();
    private final Map<String, Long> channelLastSpeed = new LinkedHashMap<>();
    private long channelSampleAt;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        // 3.0.0：这个窗口**没有标题栏**，卡片上方不许出现任何一行标题文字。
        //
        // 以前这里调的是 Activity.requestWindowFeature()，对 AppCompatActivity 来说
        // 只是个 framework 调用：它关掉的是 PhoneWindow 自己的 DecorView 标题，
        // AppCompat 那套 SubDecor 完全不知道。而本页面用的是 Dialogue 系主题
        // （windowIsFloating=true），AppCompat 认为"没标 windowNoTitle"时就会把
        // abc_dialog_title_material 里的那颗 TextView（id/title，文字 = activity 的
        // android:label「发送到电脑」）插在 SubDecor 最上面。这就是多任务重构之后
        // 卡片上方多出来的那条孤立文字（真机 dumpsys 里能看到：
        // MaterialTextView #id/title 0,0-1595,124，正好压在卡片上面 124px）。
        //
        // 正确的入口是 supportRequestWindowFeature()（走 AppCompatDelegate），
        // 而且必须在 super.onCreate() 之后、setContentView()（安装 SubDecor）之前调用。
        supportRequestWindowFeature(android.view.Window.FEATURE_NO_TITLE);
        CrashGuard.install();
        // 主题里已经关掉了 ActionBar（themes.xml 的 windowNoTitle/windowActionBar），
        // 这里再兜一次：某些 AppCompat 版本会先建出 ActionBar 再让你 hide()。
        if (getSupportActionBar() != null) {
            getSupportActionBar().hide();
        }
        setContentView(R.layout.activity_share);
        // 最后兜一道：万一还是被插了标题（老版本 AppCompat / 系统 Dialog 主题），
        // 直接把那颗 TextView 从窗口里摘掉，别让它把卡片往下挤 124px。
        dropStrayWindowTitle();
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP) {
            setFinishOnTouchOutside(false);
        }

        // 进度界面只有一套：把双窗口弹窗 / 应用外悬浮窗用的那个 Compose 面板塞进布局里的占位容器。
        // 每个 Activity 一份 SharePanel（Compose 状态也是各自一份）：多个任务的面板同时存在时互不干扰。
        panel = new SharePanel(this);
        ViewGroup panelHost = findViewById(R.id.share_panel);
        panelHost.addView(panel.view(this));

        // 窗口跟着卡片走：宽高都 WRAP_CONTENT，背景透明 —— 窗口自带的圆角背景会变成
        // 「一个白框套着卡片」，所以只留卡片本身。
        getWindow().setBackgroundDrawable(new ColorDrawable(Color.TRANSPARENT));
        // 背景压暗交给 Window 自己的 dim：只压后面那层，卡片保持原大小。
        getWindow().setDimAmount(0.35f);
        // 「点弹窗外面」= 退出这个界面（传输中只是收起，见 dismissPanel / collapsePanel）。
        // 那块遮罩是 Window 自己的 dim 层，在窗口外面 —— Compose 里既画不到也点不到，
        // 只能让系统把窗外那一下触摸当成 ACTION_OUTSIDE 递进来（见 dispatchTouchEvent）。
        getWindow().clearFlags(WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_WATCH_OUTSIDE_TOUCH);
        getWindow().setLayout(
                ViewGroup.LayoutParams.WRAP_CONTENT,
                ViewGroup.LayoutParams.WRAP_CONTENT);

        // taskId：进程被回收后重建 > intent 里带的（通知栏点回来）> 新建一个（第一次分享）
        if (savedInstanceState != null) {
            taskId = savedInstanceState.getInt(STATE_TASK_ID, -1);
        }
        if (taskId < 0) {
            taskId = getIntent().getIntExtra(EXTRA_TASK_ID, -1);
        }
        // 面板那几颗按钮直接作用在「当前任务」上（暂停 / 继续 / 取消都走 taskId，
        // 不会再出现「操作到别的任务」这种事）
        panel.setActions(new ShareActions(
                this::dismissPanel,
                this::cancelTransfer,
                this::pauseTransfer,
                this::resumeTransfer));
        // 取消确认框按「要取消哪个任务」收尾（面板自己的取消按钮、队列行上的取消，同一个框）
        panel.setCancelHandlers(this::cancelKeepingProgress, this::cancelDiscardingProgress);
        // 队列列表的两个落点：点一行切到那个任务的面板；点行上的「取消」取消那个任务
        panel.setQueueActions(this::switchToTask, this::cancelQueueTask);

        SendTask target = taskId < 0 ? createTaskFromIntent() : SendTasks.get(taskId);
        if (target == null) {
            // 任务已经不在了（面板关掉以后进程又被回收、或者通知点晚了一步）：
            // 明确关掉，绝不留一个只剩标题的空壳窗口
            Log.w(TAG, "没有 task " + taskId + "，关掉这个空面板");
            finish();
            return;
        }
        attachTask(target);
    }

    /**
     * 把窗口自带的标题那一行摘掉（卡片上方那条「发送到电脑」）。
     *
     * <p>正常情况下 themes.xml 里的 windowNoTitle + supportRequestWindowFeature 已经
     * 让它根本不会被创建；这里只是兜底：AppCompat 的行为按版本有过变化
     * （悬浮窗口 + 没读到 windowNoTitle 时会 inflate abc_dialog_title_material，
     * 里面那颗 TextView 的 id 就是 {@code app:id/title}）。留着它会把面板往下挤
     * 124px，屏幕上就是一条孤零零的「发送到电脑」。
     *
     * <p>只摘"窗口标题"那一个：父节点不能是卡片那一层，绝不碰面板内部的任何 view。
     */
    private void dropStrayWindowTitle() {
        try {
            // 窗口标题的文字来自 activity 的 android:label；先清空，就算摘不掉也不会显形
            setTitle("");
            int id = getResources().getIdentifier("title", "id", getPackageName());
            if (id == 0) {
                return;
            }
            android.view.View title = getWindow().getDecorView().findViewById(id);
            if (title == null) {
                return;
            }
            android.view.ViewParent parent = title.getParent();
            android.view.View card = findViewById(R.id.share_panel);
            if (parent instanceof ViewGroup && parent != card) {
                ((ViewGroup) parent).removeView(title);
                Log.i(TAG, "移除了窗口自带的标题 view（卡片上方不应有标题）");
            }
        } catch (Throwable e) {
            Log.w(TAG, "移除窗口标题失败（不影响使用）", e);
        }
    }

    /** 第一次进来（一次新的分享）：新建任务并开跑 */
    private SendTask createTaskFromIntent() {
        List<android.net.Uri> uris = SendTask.extractUris(getIntent());
        int connectionId = getIntent().getIntExtra(EXTRA_CONNECTION_ID, -1);
        SendTask created = SendTasks.create(this, connectionId);
        // intent 里补上 taskId：进程被回收后重建、通知栏路由都靠它
        Intent intent = new Intent(getIntent());
        intent.putExtra(EXTRA_TASK_ID, created.getTaskId());
        setIntent(intent);
        HFXLog.d("share: 新建任务 taskId=" + created.getTaskId() + " files=" + uris.size());
        created.start(uris);
        return created;
    }

    /** 把这个面板绑到一个任务上，并把当前状态画出来 */
    private void attachTask(SendTask target) {
        task = target;
        taskId = target.getTaskId();
        // 窗口从这一刻起算「新」：见 dispatchTouchEvent 里的 OUTSIDE_GRACE_MS
        shownAt = SystemClock.uptimeMillis();
        target.bind(this);
        renderNow();
        if (!target.isTerminal()) {
            frameClock.start();
        }
    }

    /**
     * 二次分享（只有「用 FLAG_ACTIVITY_SINGLE_TOP 起这个页面」或者别的应用复用了同一个
     * intent 时才会走到，{@code launchMode=standard} 下正常是新建实例）。
     *
     * <p>这里按「这次带的是新分享就再开一个面板」处理：绝不再像以前那样把上一轮取消掉。
     */
    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        int incoming = intent.getIntExtra(EXTRA_TASK_ID, -1);
        if (incoming == taskId) {
            // 同一条任务的通知又点了一次：只是把面板刷一下
            renderNow();
            return;
        }
        // 这个面板被复用去显示另一个任务（或者又一次新的分享）：先解开旧任务
        // —— 旧任务如果是传输中，unbind 不会取消它，它照跑
        SendTask old = task;
        if (old != null) {
            old.unbind(this);
        }
        task = null;
        finishingScheduled = false;
        SendTask target = incoming >= 0 ? SendTasks.get(incoming) : createTaskFromIntent();
        if (target == null) {
            Log.w(TAG, "onNewIntent 指向的任务不在了（taskId=" + incoming + "）");
            finish();
            return;
        }
        HFXLog.d("share: 面板复用，改绑任务 taskId=" + target.getTaskId());
        attachTask(target);
    }

    // ------------------------------------------------------------------
    // 面板上的按钮（由 SharePanel.setActions 接进来）
    // ------------------------------------------------------------------

    /**
     * 点弹窗外面（那块 dim 遮罩上）：
     * 窗口是 WRAP_CONTENT 的小窗，窗外那一下触摸系统只给一条 ACTION_OUTSIDE
     * （见 onCreate 里那两个 FLAG_*），在这里接住，交给和「关闭」按钮同一个出口。
     */
    @Override
    public boolean dispatchTouchEvent(MotionEvent ev) {
        if (ev.getActionMasked() == MotionEvent.ACTION_OUTSIDE) {
            // 刚起窗口的那一下要放过：从通知栏点过来时，手指是「先按在通知上、面板才起来、
            // 然后才松开」，那条 UP 会被系统当成落在窗口外面（ACTION_OUTSIDE）。
            // 以前这里无条件 dismissPanel()：传输中 → 收起面板（用户看到的是「点了通知又回到 App/桌面」），
            // 排队 / 准备中 → 直接取消任务。两种都像「点通知没反应」，报的「传输时点通知还是跳回主页」就是这条。
            long age = SystemClock.uptimeMillis() - shownAt;
            if (age < OUTSIDE_GRACE_MS) {
                HFXLog.d("share: 忽略窗口刚起时的窗外触摸（" + age + "ms，来自通知栏那一下）");
                return true;
            }
            HFXLog.d("share: 点面板外面 → dismissPanel");
            dismissPanel();
            return true;
        }
        return super.dispatchTouchEvent(ev);
    }

    /**
     * 唯一出口：面板上的「关闭」按钮、点弹窗外面、返回键，三个都走这里。
     *
     * <ul>
     *   <li>已经收尾（完成 / 失败 / 取消）：直接关窗口；</li>
     *   <li>传输中（含暂停、让出通道）：<b>收起</b>界面，任务照跑；</li>
     *   <li>还没开始传（准备 / 排队）：立取消让准备线程停下，再关窗口。</li>
     * </ul>
     */
    private void dismissPanel() {
        SendTask current = task;
        if (current == null || current.isTerminal()) {
            finish();
            return;
        }
        if (current.isTransferActive()) {
            collapsePanel();
            return;
        }
        current.cancel(true);
        finish();
    }

    /**
     * 传输中「收起」界面：不走 finish()，任务线程和面板状态原样留着。
     *
     * <p>为什么用 moveTaskToBack 而不是 finish()：finish() 会把这个面板销毁
     * （虽然任务照跑，但面板就回不来了 ——「回来还能看到进度」这条就做不到）。
     * moveTaskToBack 只是把任务挪到后台：Activity 留在栈里（只走 onPause/onStop），
     * 任务线程接着发；用户从通知栏点回来时 {@link #onResume()} 会把采样重新起拍并立刻补一帧。
     */
    private void collapsePanel() {
        if (collapsed) {
            return;
        }
        collapsed = true;
        frameClock.stop();
        if (!moveTaskToBack(true)) {
            finish();
        }
    }

    /** 面板上的「取消发送」：先问一句「已经传过去的那部分要不要保留」，再真的取消 */
    private void cancelTransfer() {
        SendTask current = task;
        if (current == null) {
            finish();
            return;
        }
        if (current.isTerminal()) {
            finish();
            return;
        }
        if (!current.isTransferActive()) {
            doCancelTransfer(true);
            return;
        }
        if (panel != null) {
            panel.showCancelConfirm(taskId);
        }
    }

    /**
     * 队列行上的「取消」。
     *
     * <p>和面板上「取消发送」同一个出口，只是作用在<b>那一行的任务</b>上：
     * <ul>
     *   <li>已经收尾：什么都不做（那一行本来也不给取消入口）；</li>
     *   <li>排队中 / 准备中（还没开传）：直接取消并保留进度 —— 和面板对「还没开传」的处理一致
     *       （电脑上还没有半截文件，没什么可问的）；</li>
     *   <li>正在传 / 暂停着：弹同一个「保留进度 / 不保留」确认框，只是记的是那个任务的 taskId。</li>
     * </ul>
     */
    private void cancelQueueTask(int targetTaskId) {
        if (targetTaskId == taskId) {
            cancelTransfer();
            return;
        }
        SendTask target = SendTasks.get(targetTaskId);
        if (target == null || target.isTerminal()) {
            return;
        }
        if (!target.isTransferActive()) {
            target.cancel(true);
            HFXLog.d("share: 队列里取消排队任务 " + targetTaskId + "（保留进度）");
            return;
        }
        if (panel != null) {
            panel.showCancelConfirm(targetTaskId);
        }
    }

    /**
     * 队列里点了别的任务：打开那个任务的面板。
     *
     * <p>走现成的 {@link SendTasks#openPanel}：它按 taskId 路由（{@code ShareActivity} 里
     * {@code EXTRA_TASK_ID} → {@code SendTasks.get}），排队中的任务也有同样的记录，
     * 所以「点队列里的排队任务 → 落到那个任务的面板」和点通知回来是同一条路。
     *
     * <p>这里<b>不动</b>当前这个面板：它是当前任务的视图，关掉它等于
     * {@code SendTask.unbind} —— 排队中的任务会在那里被取消，用户只是「看一眼别人」不该这样。
     */
    private void switchToTask(int targetTaskId) {
        if (targetTaskId == taskId) {
            return;
        }
        HFXLog.d("share: 从队列切到 taskId=" + targetTaskId);
        SendTasks.openPanel(this, targetTaskId);
    }

    /** 确认框里选了「保留进度」：电脑上的半截文件留着下次续传 */
    private void cancelKeepingProgress(int targetTaskId) {
        doCancelTask(targetTaskId, true);
    }

    /** 确认框里选了「不保留」：让电脑端把这次的不完整文件和续传记录删掉 */
    private void cancelDiscardingProgress(int targetTaskId) {
        doCancelTask(targetTaskId, false);
    }

    /** 确认框收尾：取消「框里记的那个任务」（可能是本面板的，也可能是队列里别的） */
    private void doCancelTask(int targetTaskId, boolean keepProgress) {
        if (targetTaskId == taskId || targetTaskId < 0) {
            doCancelTransfer(keepProgress);
            return;
        }
        SendTask target = SendTasks.get(targetTaskId);
        if (target == null) {
            return;
        }
        target.cancel(keepProgress);
        HFXLog.d("share: 队列里取消任务 " + targetTaskId + "，keep=" + keepProgress);
    }

    private void doCancelTransfer(boolean keepProgress) {
        SendTask current = task;
        if (current == null) {
            finish();
            return;
        }
        frameClock.stop();
        current.cancel(keepProgress);
        HFXLog.d("share: 任务 " + taskId + " 取消，keep=" + keepProgress);
        renderTerminal(current);
    }

    /** 面板上的「暂停发送」 */
    private void pauseTransfer() {
        SendTask current = task;
        if (current != null) {
            current.pause();
        }
    }

    /** 面板上的「继续」（暂停中 / 已让出通道两种情况都在任务里处理） */
    private void resumeTransfer() {
        SendTask current = task;
        if (current != null) {
            current.resume();
            // 让出过通道的任务会重新起一条运行线程：采样重新起拍，面板立刻活过来
            frameClock.start();
            renderNow();
        }
    }

    // ------------------------------------------------------------------
    // 采样 / 渲染
    // ------------------------------------------------------------------

    /** 立刻把任务当前状态画一帧（进来时、onResume 时） */
    private void renderNow() {
        SendTask current = task;
        if (current == null || panel == null) {
            return;
        }
        truthTotal = current.getTotalBytes();
        if (current.isTerminal()) {
            renderTerminal(current);
            return;
        }
        if (current.isTransferActive()) {
            long now = System.nanoTime() / 1_000_000L;
            lastSampleAt = now;
            sampleTruth(now);
            applyToPanel(current);
        } else {
            pushPanel(current, false, Collections.emptyList(), buildChannels(current, true));
        }
    }

    /** 每帧回调（vsync）：够 100ms 就采一次真值推给面板 */
    private void onFrame(long frameTimeNanos) {
        SendTask current = task;
        if (current == null) {
            frameClock.stop();
            return;
        }
        long now = frameTimeNanos / 1_000_000L;
        if (lastSampleAt != 0L && now - lastSampleAt < TICK_MS) {
            return;
        }
        lastSampleAt = now;
        if (current.isTerminal()) {
            renderTerminal(current);
            frameClock.stop();
            return;
        }
        if (current.isTransferActive()) {
            sampleTruth(now);
            applyToPanel(current);
        } else {
            // 准备 / 排队 / 等电脑连接：没有块在动，速率那一列收起
            pushPanel(current, false, Collections.emptyList(), buildChannels(current, true));
        }
    }

    /**
     * 收尾（完成 / 失败 / 取消）时把面板最后刷一次，并按状态决定要不要自动关窗口。
     *
     * <p>注意这里<b>只认任务自己的状态</b>：任务不跟着界面生死，所以「这一帧该不该画」
     * 不需要再像以前那样靠 round / activeRound 对号码。
     */
    private void renderTerminal(SendTask current) {
        if (current == null || panel == null) {
            return;
        }
        int state = current.getState();
        if (state == SendTask.STATE_DONE) {
            long total = current.getTotalBytes();
            if (total > 0L) {
                shownBytes = total;
                truthTotal = total;
            }
        }
        pushPanel(current, false, Collections.emptyList(), buildChannels(current, true));
        if (finishingScheduled) {
            return;
        }
        finishingScheduled = true;
        long delay;
        if (state == SendTask.STATE_DONE) {
            // 传完了：看一眼结果自己关
            delay = 1500L;
        } else if (state == SendTask.STATE_CANCELLED) {
            delay = 800L;
        } else {
            // 失败：窗口留着让人看清原因（点外面 / 返回键退出）
            enableDismissOutside();
            return;
        }
        handler.postDelayed(this::finishIfAlive, delay);
    }

    private void finishIfAlive() {
        if (!isFinishing() && !isDestroyed()) {
            finish();
        }
    }

    /**
     * 收尾之后放开「点窗口外面关闭」（外面那一下现在统一由 dispatchTouchEvent 接，
     * 这里保留只是为了兼容系统 DecorView 自己的那套）。
     */
    private void enableDismissOutside() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP && !isFinishing() && !isDestroyed()) {
            setFinishOnTouchOutside(true);
        }
    }

    /**
     * 每 100ms 采一次真值。口径没变，仍然是那两个「按块统计」的数：
     * <ul>
     *   <li>数据段：任务给的 {@link SendTask#getDoneBytes()}（已完成组 + 续传跳过 + 当前组按块）；</li>
     *   <li>校验段（verifyTotal &gt; 0）：{@link HFXService#verifyDoneBytes()}。</li>
     * </ul>
     * 这里只取数、只做算术，不碰界面。
     */
    private void sampleTruth(long now) {
        SendTask current = task;
        if (current == null) {
            return;
        }
        long verifyTotal = HFXService.verifyTotalBytes();
        long verifyDone = HFXService.verifyDoneBytes();
        boolean verify = verifyTotal > 0L;
        long truth;
        long total;
        if (verify) {
            // 校验阶段：数据已经发完，两端在算整文件 SHA-256
            truth = verifyDone;
            total = verifyTotal;
            lastSnapshot = null;
        } else {
            truth = current.getDoneBytes();
            total = current.getTotalBytes();
            lastSnapshot = current.sendSnapshot();
        }
        boolean phaseChanged = verify != verifyPhase;
        boolean rewound = truth < truthBytes;
        verifyPhase = verify;
        truthBytes = truth;
        truthTotal = total;
        if (phaseChanged || rewound) {
            // 换阶段（数据 → 校验）或者账本被重置（真值变小，例如让出通道后重新接着传）：
            // 基线直接挪到新真值上。这是整条进度上唯一允许往回退的时刻，
            // 因为它不是「进度倒退」，而是「上一拍那个数是错的」
            resetBaseline(truth);
        } else if (truth > shownBytes) {
            // 只增不减：块是 1MiB 一颗地记账的，显示值永远不超过真值
            shownBytes = truth;
        }
        updateSpeed(truth, now);
    }

    /** 换阶段/换账本时的基线重置：显示值挪到真值，速率重新量 */
    private void resetBaseline(long truth) {
        shownBytes = truth;
        speedSamples.clear();
        speedStateStarted = false;
        smoothedSpeed = 0L;
        lastSampleBytes = truth;
        lastGrowAt = 0L;
        lastGrowGapMs = 0L;
    }

    /** 把这一拍的真值组装成面板要的状态，推给 SharePanel（补间由面板自己做） */
    private void applyToPanel(SendTask current) {
        List<ChannelStat> channels = buildChannels(current, false);
        boolean allChannelsFinished = true;
        for (ChannelStat channel : channels) {
            if (!channel.getFinished()) {
                allChannelsFinished = false;
                break;
            }
        }
        // 数据已经发完、还在等电脑端复算整文件 SHA-256：这段时间没有字节在流动，
        // 必须给个说法，否则界面就是「79x/800 MB · 0 B/s」一动不动，看着像卡死
        waitingPc = !verifyPhase && truthTotal > 0L && truthBytes >= truthTotal && allChannelsFinished;
        // 多文件/文件夹没有跨文件的块空间，分段画出来会和整条进度对不上，退回整条连续进度
        List<SegInfo> segments;
        BlockProgress.Snapshot snapshot = lastSnapshot;
        if (!verifyPhase && current.getSentFileCount() <= 1 && truthTotal > 0L && snapshot != null) {
            segments = new ArrayList<>(snapshot.segments.size());
            for (BlockProgress.Segment segment : snapshot.segments) {
                segments.add(new SegInfo(
                        segment.owner == null ? "" : segment.owner,
                        segment.start,
                        segment.end,
                        segment.filled));
            }
        } else {
            segments = Collections.emptyList();
        }
        pushPanel(current, true, segments, channels);
    }

    /**
     * 各通道明细：名字 / 已发字节 / 实时速率 / 是否已收尾。
     *
     * <p>字节取 {@code trafficSnapshot()} 的累计发出量（真值），速率按相邻两拍的增量做 1 秒低通，
     * 和双窗口传输页同一套口径。{@code idle=true} 时速率一律给 0。
     */
    private List<ChannelStat> buildChannels(SendTask current, boolean idle) {
        HFXServer srv = current.getServer();
        if (srv == null) {
            return Collections.emptyList();
        }
        List<HFXService.ChannelTrafficSnapshot> snapshot;
        try {
            snapshot = srv.trafficSnapshot();
        } catch (Throwable e) {
            // 取消后重建会把 connections 整体换掉，读到一半抛异常就当这一拍没取到
            return Collections.emptyList();
        }
        long now = SystemClock.elapsedRealtime();
        long dt = channelSampleAt == 0L ? TICK_MS : Math.max(1L, Math.min(1000L, now - channelSampleAt));
        channelSampleAt = now;
        List<ChannelStat> out = new ArrayList<>(snapshot.size());
        for (HFXService.ChannelTrafficSnapshot channel : snapshot) {
            String name = channel.name == null ? "" : channel.name;
            long bytes = channel.uploaded;
            Long before = channelLastBytes.get(name);
            channelLastBytes.put(name, bytes);
            long instant = before != null && bytes >= before ? (bytes - before) * 1000L / dt : 0L;
            Long lastBoxed = channelLastSpeed.get(name);
            long last = lastBoxed == null ? 0L : lastBoxed;
            boolean finished = current.isChannelDone(name);
            long speed = idle || finished ? 0L : last + (instant - last) * dt / 1000L;
            if (speed < 0L) {
                speed = 0L;
            }
            channelLastSpeed.put(name, speed);
            out.add(new ChannelStat(name, bytes, speed, finished));
        }
        return out;
    }

    /**
     * 用按块真值的增量算速率：600ms 滑动窗口平均 + 和上一拍 0.75/0.25 混合。
     *
     * <p>为什么不用 {@code onSpeedInfo}：那是「每秒一次的流量回调」，一秒里一个字节都没有的
     * 收尾阶段根本不会触发，界面就常驻在 0 B/s。按块真值的增量算，任何速度下都有数。
     */
    private void updateSpeed(long sent, long now) {
        if (!speedStateStarted) {
            speedStateStarted = true;
            speedSamples.clear();
            speedSamples.addLast(new long[]{now, sent});
            lastSampleBytes = sent;
            lastGrowAt = now;
            smoothedSpeed = 0;
            return;
        }
        if (sent < lastSampleBytes) {
            speedSamples.clear();
            speedSamples.addLast(new long[]{now, sent});
            lastSampleBytes = sent;
            lastGrowAt = now;
            return;
        }
        boolean grew = sent > lastSampleBytes;
        long previousBytes = lastSampleBytes;
        long previousGrowAt = lastGrowAt;
        if (grew) {
            if (previousGrowAt > 0) {
                lastGrowGapMs = Math.min(SPEED_IDLE_MAX_MS, now - previousGrowAt);
            }
            lastGrowAt = now;
        }
        lastSampleBytes = sent;
        speedSamples.addLast(new long[]{now, sent});
        while (speedSamples.size() > 2 && now - speedSamples.peekFirst()[0] > SPEED_WINDOW_MS) {
            speedSamples.removeFirst();
        }
        long idleLimitMs = Math.min(SPEED_IDLE_MAX_MS,
                Math.max(SPEED_IDLE_MS, lastGrowGapMs + lastGrowGapMs / 2));
        if (now - lastGrowAt >= idleLimitMs) {
            smoothedSpeed = 0;
            return;
        }
        if (!grew) {
            return;
        }
        long[] oldest = speedSamples.peekFirst();
        long spanMs = now - oldest[0];
        long deltaBytes = sent - oldest[1];
        // 慢通道是「好几秒才到一块」：改用「这次涨了多少 ÷ 隔了多久」
        if (lastGrowGapMs > SPEED_WINDOW_MS) {
            spanMs = lastGrowGapMs;
            deltaBytes = sent - previousBytes;
        }
        if (spanMs < 100L || deltaBytes <= 0) {
            return;
        }
        long instant = deltaBytes * 1000L / spanMs;
        if (instant > SPEED_MAX_BYTES_PER_SEC) {
            return;
        }
        smoothedSpeed = smoothedSpeed <= 0 ? instant : (smoothedSpeed * 3 + instant) / 4;
    }

    /**
     * 面板那一行说明（xferNote）的取值，优先级从高到低：
     * <ol>
     *   <li>完成 / 失败 / 取消这类<b>最终结果</b>；</li>
     *   <li>让出通道（为了给别的任务传）；</li>
     *   <li>校验中 N% / 等待电脑校验…；</li>
     *   <li>准备 / 等待电脑连接 / 正在复制 这类过程状态 —— <b>只在还没有数据在动时</b>占位；</li>
     *   <li>保存到：电脑名 目标目录 —— 最低优先级。</li>
     * </ol>
     * 有字节在流动时必须让 note 为空（那一行要显示合计速率）。
     * 「已暂停」<b>不在这条链里</b>：面板自己有 paused 状态（按钮变「继续」+ 进度条样式）。
     */
    private String currentNote(SendTask current) {
        String result = current.getResultText();
        if (result != null && !result.isEmpty()) {
            return result;
        }
        if (current.isSuspended()) {
            // 通道让给别的任务了：进度留在电脑上，点「继续」接着传（只补缺的块）
            return "已让出通道，等待其他任务结束";
        }
        if (verifyPhase) {
            return "校验中 " + (truthBytes * 100L / Math.max(1L, truthTotal)) + "%";
        }
        if (waitingPc) {
            return "等待电脑校验…";
        }
        if (dataFlowing()) {
            return "";
        }
        String status = current.getStatusText();
        if (status != null && !status.isEmpty()) {
            return status;
        }
        if (current.isTransferActive()) {
            return "";
        }
        return targetNote(current);
    }

    /** 有没有数据在流动：显示值涨过，或者平滑速率不是 0 */
    private boolean dataFlowing() {
        return shownBytes > 0L || smoothedSpeed > 0L;
    }

    /** 「保存到：电脑A /storage/emulated/0/Download」：和任务真正发往的是同一份计算 */
    private String targetNote(SendTask current) {
        String dir = current.getRemoteTarget();
        HFXServer srv = current.getServer();
        if (dir == null || dir.isEmpty() || srv == null) {
            return "";
        }
        return middleEllipsis("保存到：" + srv.getDisplayName() + " " + dir.replace('\\', '/'), 42);
    }

    /**
     * 面板标题那一行要显示的文件名。
     *
     * <p>正在传的时候 {@code currentFileName} 是回调给的文件名；还没开传（准备中 / 排队中）
     * 它是空的，那时用 {@link SendTask#getDisplayFileName()}（准备阶段记下的第一个文件名）——
     * 以前这里直接用 {@code getCurrentFileName()}，排队中的面板标题就只有一个「…」。
     */
    private String panelFileName(SendTask current) {
        String name = current.getCurrentFileName();
        if (name != null && !name.isEmpty()) {
            return name;
        }
        return current.getDisplayFileName();
    }

    /** 只取文件名部分（远端路径 {@code C:\Users\x\a.bin} → {@code a.bin}） */
    private static String shortName(String path) {
        if (path == null) {
            return "";
        }
        int cut = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'));
        return cut >= 0 ? path.substring(cut + 1) : path;
    }

    /** 太长就从中间省略（面板那行放不下） */
    private static String middleEllipsis(String text, int max) {
        if (text == null) {
            return "";
        }
        if (max <= 1 || text.length() <= max) {
            return text;
        }
        int keep = max - 1;
        int head = keep * 3 / 5;
        int tail = keep - head;
        return text.substring(0, head) + "…" + text.substring(text.length() - tail);
    }

    /**
     * 把当前状态推给面板。
     *
     * @param active 面板底部要不要画「暂停 / 继续 / 取消」这一组（传输中才画）
     */
    private void pushPanel(SendTask current, boolean active, List<SegInfo> segments,
                           List<ChannelStat> channels) {
        SharePanel target = panel;
        if (target == null || current == null) {
            return;
        }
        boolean paused = current.isPausedByUser() || current.isSuspended();
        target.update(
                active,
                paused,
                panelFileName(current),
                shownBytes,
                truthTotal,
                shownBytes,
                segments,
                channels,
                currentNote(current),
                current.getLabel(),
                buildQueueRows(current));
    }

    // ------------------------------------------------------------------
    // 任务队列列表（面板里「任务队列」那一段）
    // ------------------------------------------------------------------

    /**
     * 队列列表要的行，每帧从 {@link SendTasks#queueSnapshot()} 取一次。
     *
     * <p>快照是只读的（字段都拷好了），顺序就是队列顺序：正在传的在最前，然后按创建顺序排队，
     * 已经收尾但面板还开着的垫底。
     *
     * <p><b>「画不画这一段」只在一个地方判断</b>：Dialogs.kt 的 {@code if (queue.size > 1)}。
     * 这里把所有还活着的任务都交出去（不再自己先判一次 {@code size < 2}）—— 两处门槛
     * 各判一次的写法，改了一边忘了另一边就是「有 2 个任务却没有队列段」这种查不出原因的怪现象。
     */
    private List<QueueRow> buildQueueRows(SendTask current) {
        if (current == null) {
            return Collections.emptyList();
        }
        List<SendTasks.TaskInfo> infos = SendTasks.queueSnapshot();
        if (infos.isEmpty()) {
            return Collections.emptyList();
        }
        List<QueueRow> rows = new ArrayList<>(infos.size());
        for (int i = 0; i < infos.size(); i++) {
            SendTasks.TaskInfo info = infos.get(i);
            rows.add(new QueueRow(
                    info.taskId,
                    i + 1,
                    // 只留文件名：task.currentFileName 是回调里的**远端完整路径**
                    // （C:\Users\nil\Downloads\...），直接摆进队列会占满一行还看不清是哪个文件
                    shortName(info.fileName),
                    queueSubtitle(info),
                    queueStatus(info),
                    info.taskId == current.getTaskId(),
                    !info.isFinished(),
                    info.isQueued()));
        }
        return rows;
    }

    /** 队列行第二行小字：目标电脑 · 已传/总量（正在传的那行再带一个速率） */
    private String queueSubtitle(SendTasks.TaskInfo info) {
        // 还没连上电脑时 label 是空的：排队中显示「等待电脑」，已经失败 / 取消的显示「未送达」
        // （以前一律写「等待电脑」，收尾的失败任务看着像还在等）
        String fallback = info.isFinished() ? "未送达" : "等待电脑";
        String label = info.label == null || info.label.isEmpty() ? fallback : info.label;
        if (info.totalBytes <= 0L) {
            return label;
        }
        StringBuilder text = new StringBuilder(label);
        text.append(" · ").append(Utils.formatFileSize(info.doneBytes))
                .append(" / ").append(Utils.formatFileSize(info.totalBytes));
        if (info.state == SendTask.STATE_SENDING && info.speed > 0L) {
            text.append(" · ").append(Utils.formatSpeed(info.speed));
        }
        return text.toString();
    }

    /** 队列行右边那句状态 */
    private String queueStatus(SendTasks.TaskInfo info) {
        switch (info.state) {
            case SendTask.STATE_SENDING:
                return "正在发送 " + info.percent() + "%";
            case SendTask.STATE_PAUSED:
            case SendTask.STATE_SUSPENDED:
                return info.percent() > 0 ? "已暂停 " + info.percent() + "%" : "已暂停";
            case SendTask.STATE_WAITING:
                // N = 同一条连接上排在它前面的任务数（含正在传的那个）；
                // N = 0 说明它自己就是队头，那句「还有 0 个」就不写了
                return info.aheadCount <= 0
                        ? "等待中"
                        : "等待中（前面还有 " + info.aheadCount + " 个）";
            case SendTask.STATE_PREPARING:
                return "准备中";
            case SendTask.STATE_DONE:
                return "已完成";
            case SendTask.STATE_FAILED:
                return "发送失败";
            case SendTask.STATE_CANCELLED:
                return "已取消";
            default:
                return "";
        }
    }

    // ------------------------------------------------------------------
    // 生命周期
    // ------------------------------------------------------------------

    /** 返回键和点遮罩同一套出口（dismissPanel）：传输中收起后台继续传，其它情况直接退出 */
    @Override
    public void onBackPressed() {
        dismissPanel();
    }

    @Override
    protected void onPause() {
        super.onPause();
        // 界面不可见了就别再占着 vsync（任务线程不受影响，通知栏那条进度由 ServerService 出）
        frameClock.stop();
    }

    @Override
    protected void onResume() {
        super.onResume();
        collapsed = false;
        // 从后台回来（收起的界面被重新打开，或切走又切回）：采样重新起拍 + 立刻补一帧，
        // 否则面板会先停在「离开那一刻」的进度上，等下一拍（100ms）才动
        renderNow();
        SendTask current = task;
        if (current != null && !current.isTerminal()) {
            frameClock.start();
        }
    }

    @Override
    protected void onSaveInstanceState(Bundle outState) {
        super.onSaveInstanceState(outState);
        // 进程被回收后重建：至少要认得出自己是哪个任务的面板
        outState.putInt(STATE_TASK_ID, taskId);
    }

    @Override
    protected void onDestroy() {
        // 撤掉「延时自动关窗口」这类排队的收尾动作
        handler.removeCallbacksAndMessages(null);
        frameClock.stop();
        SendTask current = task;
        task = null;
        if (current != null) {
            // 任务不跟着面板死：传输中（含暂停、让出通道）只是把这个面板解开，
            // 准备 / 排队阶段才真的取消（和以前 onDestroy 的规矩一样）
            current.unbind(this);
        }
        if (panel != null) {
            panel.release();
            panel = null;
        }
        super.onDestroy();
    }
}
