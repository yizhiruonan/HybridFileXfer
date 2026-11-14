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
import top.weixiansen574.hybridfilexfer.compose.SegInfo;
import top.weixiansen574.hybridfilexfer.compose.ShareActions;
import top.weixiansen574.hybridfilexfer.compose.SharePanel;
import top.weixiansen574.hybridfilexfer.core.BlockProgress;
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
        requestWindowFeature(android.view.Window.FEATURE_NO_TITLE);
        super.onCreate(savedInstanceState);
        CrashGuard.install();
        if (getSupportActionBar() != null) {
            getSupportActionBar().hide();
        }
        setContentView(R.layout.activity_share);
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
        panel.setCancelHandlers(this::cancelKeepingProgress, this::cancelDiscardingProgress);

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
            panel.showCancelConfirm();
        }
    }

    /** 确认框里选了「保留进度」：电脑上的半截文件留着下次续传 */
    private void cancelKeepingProgress() {
        doCancelTransfer(true);
    }

    /** 确认框里选了「不保留」：让电脑端把这次的不完整文件和续传记录删掉 */
    private void cancelDiscardingProgress() {
        doCancelTransfer(false);
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
                current.getCurrentFileName(),
                shownBytes,
                truthTotal,
                shownBytes,
                segments,
                channels,
                currentNote(current),
                current.getLabel());
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
