package top.weixiansen574.hybridfilexfer;

import android.content.ClipData;
import android.content.Intent;
import android.database.Cursor;
import android.graphics.Color;
import android.graphics.drawable.ColorDrawable;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Environment;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.provider.DocumentsContract;
import android.provider.MediaStore;
import android.provider.OpenableColumns;
import android.text.TextUtils;
import android.util.Log;
import android.view.MotionEvent;
import android.view.ViewGroup;
import android.view.WindowManager;
import android.widget.Toast;

import androidx.appcompat.app.AppCompatActivity;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Queue;
import java.util.concurrent.ConcurrentLinkedQueue;

import top.weixiansen574.hybridfilexfer.core.BlockProgress;
import top.weixiansen574.hybridfilexfer.core.HFXLog;
import top.weixiansen574.hybridfilexfer.core.HFXService;
import top.weixiansen574.hybridfilexfer.core.Utils;
import top.weixiansen574.hybridfilexfer.core.bean.Directory;
import top.weixiansen574.hybridfilexfer.core.bean.RemoteFile;
import top.weixiansen574.hybridfilexfer.core.bean.ServerNetInterface;
import top.weixiansen574.hybridfilexfer.core.bean.TrafficInfo;
import top.weixiansen574.hybridfilexfer.core.callback.TransferFileCallback;
import top.weixiansen574.hybridfilexfer.compose.ChannelStat;
import top.weixiansen574.hybridfilexfer.compose.SegInfo;
import top.weixiansen574.hybridfilexfer.compose.ShareActions;
import top.weixiansen574.hybridfilexfer.compose.ShareProgress;
import top.weixiansen574.hybridfilexfer.droidcore.HFXServer;
import top.weixiansen574.hybridfilexfer.tasks.BTransferFileCallback;
import top.weixiansen574.hybridfilexfer.tasks.SendFilesToRemoteTask;

/**
 * 分享/打开方式里的「发送到电脑」。
 * <p>
 * 收下文件（content:// 或 路径），确保后台服务已经和电脑连上，然后把文件直接
 * 发送到电脑接收器设置的默认文件夹。
 */
public class ShareActivity extends AppCompatActivity {

    private static final String TAG = "ShareActivity";
    private static final long CONNECT_TIMEOUT_MS = 30_000L;
    /**
     * 等上一轮的发送线程真的退出，最多等这么久。
     *
     * <p>为什么非等不可：{@code HFXService} 的 {@code transferring} / {@code sendingFiles} 是
     * 「这一次传输」的标志，上一轮退出时会在 {@code finally} 里无条件清掉 —— 如果新的这一轮已经
     * 开始，它清掉的就是新那一轮的标志，会话线程于是又去读控制通道，两个读者一起把帧流读串。
     * 传输通道本身也是同一批，不能两轮同时用。
     *
     * <p>等不到就不能硬上：越过它开跑等于主动制造「控制通道两个读者」。所以超时以后这一轮直接
     * 以一条明确的失败收场（用户再分享一次即可），而不是把帧流搞错位。
     */
    private static final long PREV_IDLE_TIMEOUT_MS = 10_000L;

    /**
     * 真值采样节拍：100ms 采一次，只取数、只做算术，不碰界面。
     *
     * <p>界面上的补间不在这里了：进度面板（{@code ReceiveProgressPanel}，和双窗口弹窗、
     * 应用外悬浮窗是同一个组件）内部自己做 200ms 的 tween，这里只要把真值按这个节拍推给它。
     * 真值是按块统计的（1MiB 一跳），100ms 看一眼足够跟上块的变化。
     *
     * <p>{@link FrameClock} 的逐帧节拍还留着：vsync 回调天然和屏幕对齐，采样时刻稳定，
     * 而且它就是「这一屏还活着」的心跳（收尾时停掉）。
     */
    private static final long TICK_MS = 100L;
    /**
     * 速率采样窗口：最近 600ms 内的字节差 ÷ 时间就是一个「平均值」。
     *
     * <p>100ms 的瞬时值太碎：块一到是一波尖峰、块没到就是 0，数字会一格一格地蹦。
     * 窗口拉长到 600ms 后尖峰被摊平，再叠一点点平滑（见 {@link #updateSpeed}）。
     */
    private static final long SPEED_WINDOW_MS = 600L;
    /** 真值多久没涨就认为「确实没有数据在流」。慢通道会按实际块间隔自适应放大，见 {@link #updateSpeed} */
    private static final long SPEED_IDLE_MS = 700L;
    /**
     * 上面那个自适应门限的上限：再慢也不超过 15 秒。
     *
     * <p>门限是靠「最近一次块间隔」放大的，如果碰到一次十几秒的停顿（暂停、电脑端卡了一下），
     * 不封顶的话之后每次归零都要等几十秒，屏幕会一直挂着旧的高速率。
     * 15 秒够覆盖 0.1MB/s 这种最慢的通道（1MB 一颗要 10 秒），再长就该当没数据在流了。
     */
    private static final long SPEED_IDLE_MAX_MS = 15_000L;
    /** 速率上限：刚连上/重连那一拍会算出十几 GB/s 的离谱值，这种不采信 */
    private static final long SPEED_MAX_BYTES_PER_SEC = 2000L * 1024 * 1024;
    /** 多台电脑时指定发给哪一台（连接号；不传或 -1 表示随便一台） */
    public static final String EXTRA_CONNECTION_ID = "top.weixiansen574.hybridfilexfer.extra.CONNECTION_ID";

    private final Handler handler = new Handler(Looper.getMainLooper());
    private final Queue<Group> groups = new ConcurrentLinkedQueue<>();

    private volatile boolean cancelled;
    private volatile boolean finished;
    private volatile boolean transferring;
    /**
     * 本次发送的「轮次」：二次分享（singleTop）时自增。
     *
     * <p>上一轮的准备线程 / 发送线程收尾时都会拿自己那一轮的号码和它对一下，
     * 号码变了就什么都不碰（不改状态、不刷面板、不自动关窗口）——
     * 否则上一轮的「失败」「完成」会把刚开始的这一轮直接掐掉。
     */
    private volatile int round;
    /**
     * 界面（帧循环 / 面板）现在归哪一轮，-1 表示没有。
     *
     * <p>{@link #handleIntent} / {@link #startNextGroup} 把它设成当轮号码，
     * {@link #onFrame} 与 {@link #applyToPanel} 每帧核对它 —— 上一轮残留的那次 vsync 回调、
     * 或者收起后再回来补的那一帧，都不会再把旧数据写进新一轮的面板。
     */
    private volatile int activeRound = -1;
    /**
     * 「取消上一轮」那条后台线程（{@link #restartWithNewShare} 里起）。
     *
     * <p>取消动作里有 {@code worker.join(1500)}，放主线程等就是一次 1.5 秒的界面冻结
     * （用户点完「分享」界面不动），所以丢给一条线程；新一轮的准备线程在真正开跑之前
     * join 它（见 {@link #waitPreviousTransferIdle}）。
     */
    private volatile Thread previousCancelThread;
    /** 已经「收起」到后台（传输中点了遮罩 / 返回键）：这时再点什么都不要再把 Activity 收掉 */
    private volatile boolean collapsed;
    private volatile HFXServer server;
    /** 多台电脑时指定发给哪台：onCreate / onNewIntent 在主线程写，准备线程要读 */
    private volatile int targetConnectionId = -1;

    /** 总量：准备线程写、主线程的采样拍读（long 不是原子写，必须 volatile 免得读到半截） */
    private volatile long totalBytes;
    private long completedBytes;
    private volatile long currentGroupTotal;
    /**
     * 当前文件「按块真值」已经发出去多少字节（{@code sendProgress} 的 doneBytes）。
     *
     * <p>以前进度用「已完成组的字节 + 当前文件大小」，分段发送时那个数一上来就是满的：
     * 用户看到的「600.00MB/600.00MB · 0B/s」而进度条还在 40%，就是这里来的。
     * 块计数和到达顺序无关，才和分段进度条对得上。
     */
    private volatile long currentGroupSent;
    private volatile String currentFileName = "";
    /** 峰值速率（完成提示里用）：高速段取 {@code onSpeedInfo} 的秒流量，慢速段取下面的平滑速率 */
    private volatile long peakSpeedBytes;

    // ---- 进度真值 / 速率状态：只在主线程（FrameClock 的采样拍）里读写，不需要 volatile ----
    /**
     * 逐帧节拍（vsync）：每帧看一眼有没有到 100ms 的采样点。
     * 为什么用它、不用 handler.postDelayed，见 {@link FrameClock} 的类注释。
     */
    private final FrameClock frameClock = new FrameClock(this::onFrame);
    /** (时刻, 已发字节) 采样点，只保留最近 {@link #SPEED_WINDOW_MS} 内的 */
    private final ArrayDeque<long[]> speedSamples = new ArrayDeque<>();
    /** 上一拍采到的真值，用来算增量 */
    private long lastSampleBytes;
    /** 真值最后一次变大的时刻 */
    private long lastGrowAt;
    /** 最近一次「变大」与上一次「变大」的间隔：慢通道（1MB 一颗要好几秒）靠它放宽归零门限 */
    private long lastGrowGapMs;
    /** 合计速率的低通值（B/s）：只用来记峰值，屏幕上的速率由面板自己按字节增量算 */
    private long smoothedSpeed;
    /** 这一拍的真值（数据段 = 已完成组 + 当前组按块已发；校验段 = verifyDone） */
    private long truthBytes;
    /**
     * 真值对应的总量（数据段 = totalBytes；校验段 = verifyTotal）；&lt;= 0 表示总量未知。
     *
     * <p>volatile：准备线程（{@link #prepareAndSend}）在等电脑连接之前就先把总量写进来，
     * 让面板那几秒显示「0 B / 12.3 MB」而不是「0 B / 0 B」；主线程的采样拍要读它。
     */
    private volatile long truthTotal;
    /** 当前这一拍是不是在校验阶段 */
    private boolean verifyPhase;
    /**
     * 交给面板的已发字节（B）。
     *
     * <p>插值整块删掉了 —— 面板内部自己做 200ms 的 tween，这里只保证<b>只增不减</b>：
     * 真值按块记账（1MiB 一跳），会往回退的情况只有「换阶段（数据 → 校验）」和
     * 「账本被重置」两处，那两处由 {@link #resetBaseline} 明确地把基线挪过去。
     */
    private long shownBytes;
    /** 上一次采信的那个账本属于哪个文件（{@code BlockProgress#currentPath()}） */
    private String ledgerPath = "";
    /** 已经把字节并进 completedBytes 的那个账本路径；非 null 时说明这个账本的数不能再采信 */
    private String accountedLedgerPath;
    /** 这一拍的分块快照：顺手给面板画分段条，不用再取第二次 */
    private BlockProgress.Snapshot lastSnapshot;
    /** 上一次真值采样的时刻（ms） */
    private long lastSampleAt;
    private boolean speedStateStarted;
    /** 已经 EOF 的通道名：各通道明细里的「已完成」就是它 */
    private final java.util.Set<String> doneChannels = new java.util.HashSet<>();
    /** 各通道上一拍的累计发出字节 / 低通后的速率 / 上一拍的时刻（算通道速率用） */
    private final java.util.Map<String, Long> channelLastBytes = new java.util.LinkedHashMap<>();
    private final java.util.Map<String, Long> channelLastSpeed = new java.util.LinkedHashMap<>();
    private long channelSampleAt;
    /** 名字表只由准备线程写一次（在 post startNextGroup 之前），读的都在这之后，靠 Handler 的可见性 */
    private final List<String> sentFileNames = new ArrayList<>();
    private volatile int sentFileCount;
    /** 每条通道的平均速率（MB/s，完成时记一次，写进配置给主页显示） */
    private final java.util.Map<String, Double> channelSpeeds = new java.util.LinkedHashMap<>();
    private long startTime;

    // ---- 面板那一行说明（xferNote）的取值来源，见 currentNote() ----
    /** 准备 / 等待电脑连接 / 正在复制这类过程状态（只在还没有数据在动时占这行） */
    private volatile String statusText = "";
    /** 完成 / 失败 / 取消这类最终结果：一出现就压住其它所有提示 */
    private volatile String resultText = "";
    /** 电脑上的目标目录（和实际发送用的是同一份计算，见 resolveRemoteBase）；准备线程也会写 */
    private volatile String remoteTarget = "";
    /** 数据已经发完、还在等电脑端复算整文件校验和 */
    private volatile boolean waitingPc;

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
        // 界面在多台电脑里选了哪一台，就只发给那一台
        targetConnectionId = getIntent().getIntExtra(EXTRA_CONNECTION_ID, -1);

        // 进度界面只有一套：把双窗口弹窗 / 应用外悬浮窗用的那个 Compose 面板塞进布局里的占位容器。
        // 面板自己会画标题、分段进度条、已发/总量、速率、各通道明细，底部的「暂停发送 / 继续 /
        // 取消发送 / 关闭」也由面板自己画 —— 这个页面里再没有别的控件了。
        ViewGroup panelHost = findViewById(R.id.share_panel);
        panelHost.addView(ShareProgress.host(this));
        // 面板那几颗按钮接到本页自己的逻辑上（取消 = 以前底部 share_action 按钮干的那一套）
        ShareProgress.setActions(new ShareActions(
                this::dismissPanel,
                this::cancelTransfer,
                this::pauseTransfer,
                this::resumeTransfer));
        // 「取消发送」先弹确认框（已传的部分要不要保留），两个选项各自落到这里
        ShareProgress.setCancelHandlers(this::cancelKeepingProgress, this::cancelDiscardingProgress);
        // 窗口跟着卡片走：宽高都 WRAP_CONTENT，背景透明 —— 窗口自带的圆角背景会变成
        // 「一个白框套着卡片」，所以只留卡片本身。
        getWindow().setBackgroundDrawable(new ColorDrawable(Color.TRANSPARENT));
        // 背景压暗交给 Window 自己的 dim：只压后面那层，卡片保持原大小。
        // （双窗口那层 35% 遮罩是 Compose 里 fillMaxSize 的黑 Box，搬到这种 WRAP_CONTENT
        // 小窗口里会把窗口直接撑成满屏，所以这里用 dim 而不是黑 Box。）
        getWindow().setDimAmount(0.35f);
        // 「点弹窗外面」= 退出这个界面（传输中只是收起，见 dismissPanel / collapsePanel）。
        // 那块遮罩是 Window 自己的 dim 层，在窗口外面 —— Compose 里既画不到也点不到，
        // 只能让系统把窗外那一下触摸当成 ACTION_OUTSIDE 递进来（见 dispatchTouchEvent）：
        //   FLAG_WATCH_OUTSIDE_TOUCH  系统才会送一条 ACTION_OUTSIDE 给我们（一次手势一条）；
        //   清掉 FLAG_NOT_TOUCH_MODAL 让窗口保持「模态」：遮罩上那一下只到我们这里，
        //   不会再穿到后面那个应用去（否则点遮罩的同时还会点到分享来源 App 的按钮）。
        // 这套配置就是系统 Dialog 的「点外面取消」：模态 + 看窗外触摸。
        // 别改成「Compose 里铺一层 fillMaxSize 的黑 Box」：窗口是 WRAP_CONTENT，
        // fillMaxSize 会把窗口直接撑成满屏 + 一层 35% 黑（面板这边就是为了这个才走 bare=true 的）。
        getWindow().clearFlags(WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_WATCH_OUTSIDE_TOUCH);
        getWindow().setLayout(
                ViewGroup.LayoutParams.WRAP_CONTENT,
                ViewGroup.LayoutParams.WRAP_CONTENT);

        handleIntent(getIntent());
    }

    /**
     * 二次分享（singleTop）：把上一轮那一整套状态整块丢掉，再当一次全新的发送。
     *
     * <p>用户报的「界面错位 / 上一次的发送还没完成」就在这儿：第一次分享的弹窗还开着
     * （传完了、失败了、或者被暂停在「0 B / 1.78 GB · 已暂停」），这时再从 MT 管理器分享一次，
     * 以前这里只调了 {@link #handleIntent}，而 {@code finished} 还是上一轮的 true ——
     * {@link #startNextGroup} 第一句 {@code if (finished) return;} 直接返回，
     * 新的传输一次都没跑，面板就停在上一轮那堆残留上
     * （14 条通道全是「0 B 正在发送数据…」、状态还是「已暂停」）。
     *
     * <p>以前还有个「传输中再来一次就 Toast share_busy」的分支：那个 Toast 一瞬就没了，
     * 用户看到的就是"什么都没发生、界面还是老样子"。现在改成把上一轮取消掉、
     * 直接开始发新的这一次（用户既然又分享了一次，要发的是新文件）。
     */
    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        // getIntent() 也要换掉：后面别的地方读的还是最初那一次
        setIntent(intent);
        restartWithNewShare();
        handleIntent(intent);
    }

    // ------------------------------------------------------------------
    // 面板上的按钮（由 ShareProgress.setActions 接进来）
    // ------------------------------------------------------------------

    /**
     * 点弹窗外面（那块 dim 遮罩上）：窗口是 WRAP_CONTENT 的小窗，窗外那一下触摸系统只给一条
     * {@link MotionEvent#ACTION_OUTSIDE}（见 onCreate 里那两个 FLAG_*），在这里接住，
     * 交给和「关闭」按钮同一个出口 {@link #dismissPanel()}。
     *
     * <p>为什么不用 Activity#setFinishOnTouchOutside(true)：那是让 DecorView 直接 finish()，
     * 绕不过「传输中只收起、不取消」这条逻辑。
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
     *   <li>传输中：<b>收起</b>界面（{@link #collapsePanel()}），传输不取消；</li>
     *   <li>还没开始传（准备 / 等待电脑连接 / 正在复制）：立 cancelled 让那条准备线程停下，再关窗口。</li>
     * </ul>
     *
     * <p>以前传输中这里是一颗 Toast（share_busy）把人拦回去，点遮罩等于没反应。
     * 现在发送任务跑在 {@code SendFilesToRemoteTask}（{@code BackstageTask}，TaskManger 的线程池）里，
     * 它只认 finished / cancelled 两个标志和 server，跟 Activity 是否存活无关 ——
     * 所以界面收起来以后文件照样发完。
     */
    private void dismissPanel() {
        if (finished) {
            finish();
            return;
        }
        if (transferring) {
            collapsePanel();
            return;
        }
        cancelled = true;
        finish();
    }

    /**
     * 传输中「收起」界面：不走 finish()，传输线程和这套状态机原样留着。
     *
     * <p>为什么用 {@link #moveTaskToBack(boolean)} 而不是 finish()：finish() 会把 Activity 连同
     * ComposeView、Handler、采样循环一起销毁，后台虽然还能把文件发完（任务线程不依赖 Activity），
     * 但面板就再也回不来了 ——「回来还能看到进度」这条就做不到。moveTaskToBack 只是把任务挪到后台：
     * Activity 留在栈里（只走 onPause/onStop，不 onDestroy），传输线程接着发；
     * 用户回到这个界面时 {@link #onResume()} 会把采样重新起拍并立刻补一帧，面板就是活的。
     */
    private void collapsePanel() {
        if (collapsed) {
            // 连点两下遮罩：第二下不要再往下走（moveTaskToBack 第二次会返回 false，
            // 再退到 finish() 就把面板弄没了）
            return;
        }
        collapsed = true;
        // 界面不可见了就别再占着 vsync：采样只是为了喂面板（onResume 会重新起拍）
        frameClock.stop();
        if (!moveTaskToBack(true)) {
            // 极少数情况下系统不让把这个任务挪到后台（这个 Activity 就是栈底且没有别的界面）：
            // 退一步直接把窗口收掉。传输照样继续 —— onDestroy 里传输中是不立 cancelled 的。
            finish();
        }
    }

    /**
     * 面板上的「取消发送」：先问一句「已经传过去的那部分要不要保留」，再真的取消。
     *
     * <ul>
     *   <li><b>保留进度</b>：什么都不用做 —— 电脑端本来就会把这次的不完整文件和
     *       {@code <文件名>.hfxpart} 留下来，下次发同一个文件只补缺的块（断点续传）；</li>
     *   <li><b>不保留</b>：取消时多发一帧 {@code CANCEL_DISCARD}，电脑端收尾时把半截文件和
     *       续传记录一起删掉，下次同一个文件从头传（1.78 GB 的文件预分配了整份大小，
     *       留着就是白占盘）。</li>
     * </ul>
     *
     * <p>还没开始传（准备 / 等电脑连接 / 正在复制）就没有进度可留，直接取消。
     */
    private void cancelTransfer() {
        if (finished) {
            finish();
            return;
        }
        if (!transferring) {
            doCancelTransfer(true);
            return;
        }
        ShareProgress.showCancelConfirm();
    }

    /** 确认框里选了「保留进度」：走原来的取消逻辑，电脑上的半截文件留着下次续传 */
    private void cancelKeepingProgress() {
        doCancelTransfer(true);
    }

    /** 确认框里选了「不保留」：让电脑端把这次的不完整文件和续传记录删掉 */
    private void cancelDiscardingProgress() {
        doCancelTransfer(false);
    }

    /**
     * 真正的取消动作（原来 share_action 那段逻辑）。
     *
     * <p>取消只关传输通道、控制通道留着（304），电脑端收尾完会把通道重建回来；
     * 传输线程是阻塞读写的，靠 {@code ServerService.cancelTransfer} 关通道让它退出。
     *
     * @param keepProgress true = 保留已传的部分（下次续传）；
     *                     false = 电脑端删掉不完整文件和续传记录（下次从头传）
     */
    private void doCancelTransfer(boolean keepProgress) {
        if (finished) {
            finish();
            return;
        }
        cancelled = true;
        finished = true;
        transferring = false;
        if (server != null) {
            ServerService.cancelTransfer(server.getId(), !keepProgress);
        }
        final int myRound = round;
        // 采样循环先停：后面那一次「清零」不能被下一拍的真值又盖回去
        frameClock.stop();
        // 面板最后刷一次：进度清零、速率收起、文案换成「已取消」
        flushPanel(keepProgress
                ? getString(R.string.share_cancelled) + "（保留进度，下次接着传）"
                : getString(R.string.share_cancelled) + "（已让电脑删除未完成的文件）", true, myRound);
        // 延时的自动关窗口也要认轮次：这 800ms 里又分享了一次的话，
        // 关掉的会是新一轮的窗口（而不是上一轮这个已经取消掉的）
        handler.postDelayed(() -> {
            if (isCurrentRound(myRound)) {
                finish();
            }
        }, 800);
    }

    /**
     * 二次分享：上一轮的状态整块清干净，再当一次全新的发送。
     *
     * <p>要清的是三样东西：
     * <ol>
     *   <li>本页自己的状态机（finished / cancelled / transferring、进度、账本基线、面板文案）；</li>
     *   <li>上一轮排着队的收尾动作（「延时 800ms 关窗口」「延时 1.5s 关窗口」）；</li>
     *   <li>HFXServer 上的账本（sendProgress / blockProgress / 校验进度）——
     *       面板每一拍都会把它们画出来，这就是「14 条通道 0 B 正在发送数据…」的残留来源。</li>
     * </ol>
     */
    private void restartWithNewShare() {
        // 这一轮作废：上一轮还在跑的准备线程 / 发送线程看到号码变了就不再碰界面
        int nextRound = round + 1;
        round = nextRound;
        // 新的界面归属：activeRound 由 handleIntent 设成新号码，在那之前 -1（上一轮的
        // 帧回调看到号码不对就自己收工，见 onFrame / applyToPanel）
        activeRound = -1;
        // 上一轮排着队的收尾动作（「延时 800ms / 1.5s 关窗口」）先撤掉。
        // 这一步必须排在下面任何「新一轮的投递」之前 —— 否则撤掉的就是新一轮自己的回调。
        handler.removeCallbacksAndMessages(null);
        // 新的一次分享可能指定了另一台电脑（多台在线时）：从新 intent 里重取一遍
        targetConnectionId = getIntent().getIntExtra(EXTRA_CONNECTION_ID, -1);
        HFXServer previous = server;
        boolean previousBusy = previous != null && previous.isTransferring();
        // 上一轮还在传：先在后台把它取消掉（这不是用户点了「不保留」，只是"换一份新的发送"，
        // 已传的部分照旧保留），否则两条发送线程会抢同一条连接上的控制通道。
        //
        // 为什么不在主线程直接调：ServerService.cancelTransfer → HFXServer.abortTransfer 里要
        // join 关通道的那条线程（最多 1.5 秒），在主线程上等就是用户点完「分享」界面定格 1.5 秒。
        // 新一轮的准备线程会在真正开跑之前 join 这条线程，再等上一轮真的退出
        // （见 waitPreviousTransferIdle）。
        previousCancelThread = null;
        if (previousBusy) {
            final HFXServer target = previous;
            Thread canceler = new Thread(() -> {
                try {
                    ServerService.cancelTransfer(target.getId());
                } catch (Throwable e) {
                    HFXLog.d("share: 取消上一轮失败", e);
                }
            }, "ShareCancelPrev");
            previousCancelThread = canceler;
            canceler.start();
        }
        cancelled = false;
        finished = false;
        transferring = false;
        collapsed = false;
        waitingPc = false;
        statusText = "";
        resultText = "";
        remoteTarget = "";
        totalBytes = 0L;
        completedBytes = 0L;
        currentGroupTotal = 0L;
        currentGroupSent = 0L;
        currentFileName = "";
        peakSpeedBytes = 0L;
        startTime = 0L;
        sentFileNames.clear();
        sentFileCount = 0;
        channelSpeeds.clear();
        groups.clear();
        frameClock.stop();
        lastSampleAt = 0L;
        // 速率窗口、显示字节、通道明细、doneChannels 一起归零（不然会接着上一轮的数跑）
        resetProgressSmoothing();
        if (previous != null) {
            try {
                // 上一轮的账本：新任务在 sendFiles 之前本来也会 reset，
                // 但二次分享这条路以前根本没走到那里，所以这里先清一遍
                previous.sendProgress.reset();
                previous.blockProgress.reset();
                HFXService.resetVerifyProgress();
                // 暂停闸门也放开：面板那行「已暂停」读的就是 isTransferPaused()。
                // cancelTransfer 里已经放开过一次，这里再兜一次底 ——
                // 上一轮如果是「失败 / 完成」结束的，根本没走到 cancelTransfer，
                // 闸门会一直停在 true，新一轮一开播面板就写着「已暂停」（用户报的那条）。
                previous.resumeTransfer();
            } catch (Throwable e) {
                HFXLog.d("share: 清理上一轮账本失败", e);
            }
        }
        // server 重新按"现在还在连的那台"拿（上一台可能已经断了，ensureConnected 会给新的）
        server = null;
        // 面板先整块刷成干净空态（ShareProgress.reset），再喂一次「未激活」：
        // 这样上一轮的任何字段（xferNote / xferPaused / 通道明细 / 已发字节）都不会留在屏幕上。
        ShareProgress.reset();
        ShareProgress.update(false, false, "", 0L, 0L, 0L,
                Collections.emptyList(), Collections.emptyList(), "",
                previous != null ? previous.getDisplayName() : "");
        HFXLog.d("share: 新一轮 round=" + nextRound
                + " previous=" + (previous != null ? previous.getDisplayName() : "none")
                + " previousTransferring=" + previousBusy);
    }

    /** 面板上的「暂停发送」：和双窗口弹窗一样，直接叫 HFXServer 关上暂停闸门 */
    private void pauseTransfer() {
        HFXServer srv = server;
        if (srv != null) {
            srv.pauseTransfer();
        }
    }

    /** 面板上的「继续」：放开暂停闸门 */
    private void resumeTransfer() {
        HFXServer srv = server;
        if (srv != null) {
            srv.resumeTransfer();
        }
    }

    /**
     * 收尾之后放开「点窗口外面关闭」。
     *
     * <p>外面那一下现在统一由 {@link #dispatchTouchEvent} 接（走 dismissPanel）；这里保留只是为了
     * 兼容系统 DecorView 自己的那套。Activity 可能已经销毁（传输中收起 / 收尾后自动关窗口），
     * 销毁之后不能再碰 Window，所以先判一下。
     */
    private void enableDismissOutside() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP && !isFinishing() && !isDestroyed()) {
            setFinishOnTouchOutside(true);
        }
    }

    private void handleIntent(Intent intent) {
        // 新的一次传输：速率的采样窗口、显示用的字节都从头开始（不然会接着上一次的数跑）
        resetProgressSmoothing();
        final int myRound = round;
        // 界面从这一刻起归这一轮：所有写面板的路径都要先核对它（见 isCurrentRound）
        activeRound = myRound;
        List<Uri> uris = extractUris(intent);
        if (uris.isEmpty()) {
            fail(getString(R.string.share_no_file), myRound);
            return;
        }
        showStatus(getString(R.string.share_preparing), myRound);
        new Thread(() -> prepareAndSend(uris, myRound), "SharePrepare").start();
    }

    /** 这一轮还算数吗（二次分享会把 round 顶掉，上一轮的线程看到号码变了就该收工） */
    private boolean isCurrentRound(int myRound) {
        return myRound == round;
    }

    private List<Uri> extractUris(Intent intent) {
        if (intent == null) {
            return Collections.emptyList();
        }
        List<Uri> uris = new ArrayList<>();
        String action = intent.getAction();
        if (Intent.ACTION_SEND.equals(action)) {
            Uri uri = getParcelableUri(intent);
            if (uri != null) {
                uris.add(uri);
            }
        } else if (Intent.ACTION_SEND_MULTIPLE.equals(action)) {
            ArrayList<Uri> list = getParcelableUriList(intent);
            if (list != null) {
                uris.addAll(list);
            }
        } else if (Intent.ACTION_VIEW.equals(action)) {
            Uri data = intent.getData();
            if (data != null) {
                uris.add(data);
            }
        }
        if (uris.isEmpty() && intent.getClipData() != null) {
            ClipData clipData = intent.getClipData();
            for (int i = 0; i < clipData.getItemCount(); i++) {
                Uri uri = clipData.getItemAt(i).getUri();
                if (uri != null) {
                    uris.add(uri);
                }
            }
        }
        return uris;
    }

    @SuppressWarnings("deprecation")
    private Uri getParcelableUri(Intent intent) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            return intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri.class);
        }
        return intent.getParcelableExtra(Intent.EXTRA_STREAM);
    }

    @SuppressWarnings("deprecation")
    private ArrayList<Uri> getParcelableUriList(Intent intent) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            return intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri.class);
        }
        return intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM);
    }

    // ------------------------------------------------------------------
    // 准备 + 发送（后台线程）
    // ------------------------------------------------------------------

    private void prepareAndSend(List<Uri> uris, int myRound) {
        List<File> files = new ArrayList<>();
        long total = 0;
        for (int i = 0; i < uris.size(); i++) {
            if (cancelled || !isCurrentRound(myRound)) {
                return;
            }
            final int index = i + 1;
            showStatus(getString(R.string.share_resolving_file, index, uris.size()), myRound);
            File file = resolve(uris.get(i), myRound);
            if (file != null && file.exists()) {
                files.add(file);
                if (!file.isDirectory()) {
                    total += file.length();
                }
            }
        }
        if (!isCurrentRound(myRound)) {
            // 复制文件期间又被分享了一次：这一轮作废，别碰界面
            return;
        }
        if (files.isEmpty()) {
            fail(getString(R.string.share_no_file), myRound);
            return;
        }
        totalBytes = total;
        // 总量先交给面板：连接电脑可能要等十几秒，这段时间面板上写的是
        // 「0 B / 12.3 MB · 正在等待电脑连接…」，比一条 0 B / 0 B 有信息量（下一拍采样会覆盖它）
        truthTotal = total;

        showStatus(getString(R.string.share_waiting_pc), myRound);
        if (!ensureConnected()) {
            if (isCurrentRound(myRound)) {
                fail(getString(R.string.share_pc_not_connected), myRound);
            }
            return;
        }
        if (!isCurrentRound(myRound)) {
            return;
        }
        HFXServer srv = server;
        if (srv == null) {
            fail(getString(R.string.share_pc_not_connected), myRound);
            return;
        }
        // 上一轮如果还在收尾（我们刚把它取消掉）：等它真的退出，否则两条发送线程会
        // 抢同一条连接上的控制通道（旧线程 endTransfer 还会把新线程的 transferring 清掉）。
        // 等不到就不硬上 —— 越过它开跑等于主动制造「控制通道两个读者」。
        if (!waitPreviousTransferIdle(srv, myRound)) {
            if (isCurrentRound(myRound) && !cancelled) {
                // 用现成的 share_busy（「传输中，请稍候」）：这条路径是「上一轮还没退出」，
                // 不硬上、也不吞掉，直接告诉用户再分享一次
                fail(getString(R.string.share_busy), myRound);
            }
            return;
        }
        if (cancelled || !isCurrentRound(myRound)) {
            return;
        }
        try {
            // 新一轮从「没暂停」开始：上一轮要是停在暂停里结束的，闸门会一直是 true，
            // 面板那行（读 isTransferPaused()）就永远写着「已暂停」。
            srv.resumeTransfer();
            HFXLog.d("share: 新一轮准备完成，暂停闸门已复位 round=" + myRound);
        } catch (Throwable e) {
            HFXLog.d("share: resumeTransfer 失败", e);
        }
        // 连上了：把「文件会存到电脑上的哪个目录」算出来（以前是 share_hint 那行），
        // 面板那行说明会以「保存到：电脑名 目录」的形式显示它（见 currentNote）。
        // 和 startNextGroup 里真正用来发的是同一个方法，不会显示一个目录、实际发到另一个。
        remoteTarget = resolveRemoteBase();

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
        for (Map.Entry<String, List<RemoteFile>> entry : byParent.entrySet()) {
            long groupTotal = 0;
            for (RemoteFile file : entry.getValue()) {
                if (!file.isDirectory()) {
                    groupTotal += file.getSize();
                }
            }
            for (RemoteFile file : entry.getValue()) {
                if (!file.isDirectory()) {
                    sentFileCount++;
                    if (sentFileNames.size() < 3) {
                        sentFileNames.add(file.getName());
                    }
                }
            }
            groups.add(new Group(entry.getKey(), groupTotal, entry.getValue()));
        }
        final int roundForGroup = myRound;
        handler.post(() -> startNextGroup(roundForGroup));
    }

    /**
     * 等上一轮那条发送线程真的退出（它现在正被取消，通道已经关了，正常几十毫秒就结束）。
     *
     * <p>为什么要等：{@code HFXService} 的 {@code transferring} / {@code sendingFiles}
     * 是"这一次传输"的标志，上一轮退出时会无条件清掉 —— 如果新的这一轮已经开始，
     * 它清掉的就是新那一轮的标志，会话线程于是又去读控制通道，两个读者一起把帧流读串。
     * 传输通道本身也是同一批，不能两轮同时用。
     */
    private boolean waitPreviousTransferIdle(HFXServer srv, int myRound) {
        // 先等「取消上一轮」那条线程收工：它内部要 join 关通道的线程（最多 1.5 秒）
        Thread canceler = previousCancelThread;
        if (canceler != null && canceler != Thread.currentThread()) {
            try {
                canceler.join(3000L);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return false;
            }
            // 只清掉自己等一下来的那一条：这期间又分享了一次的话，字段里已经是
            // 新一轮刚起的那条线程了，清掉会让它没人 join
            if (previousCancelThread == canceler) {
                previousCancelThread = null;
            }
        }
        if (srv == null) {
            return true;
        }
        long deadline = System.currentTimeMillis() + PREV_IDLE_TIMEOUT_MS;
        while (srv.isTransferring() && System.currentTimeMillis() < deadline
                && !cancelled && isCurrentRound(myRound)) {
            try {
                Thread.sleep(50L);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return false;
            }
        }
        boolean idle = !srv.isTransferring();
        if (!idle) {
            // 等不到就不硬上（见 PREV_IDLE_TIMEOUT_MS 的注释）：这一轮由调用方判失败
            HFXLog.d("share: 等上一轮退出超时 round=" + myRound
                    + " transferring=" + srv.isTransferring()
                    + " cancelled=" + cancelled + " current=" + isCurrentRound(myRound));
        }
        return idle;
    }

    private boolean ensureConnected() {
        if (ServerService.isConnected()) {
            server = ServerService.getServer(targetConnectionId);
            return server != null;
        }
        if (!ServerService.isRunning()) {
            try {
                Config config = Config.getInstance(this);
                List<ServerNetInterface> list = NetInterfaces.forAutoConnect(this);
                if (!list.isEmpty()) {
                    ServerService.startServer(this, config.getMode(), list,
                            config.getLocalBufferCount(), config.getRemoteBufferCount());
                }
            } catch (Exception e) {
                Log.w(TAG, "start server failed", e);
            }
        }
        long deadline = System.currentTimeMillis() + CONNECT_TIMEOUT_MS;
        while (System.currentTimeMillis() < deadline && !cancelled) {
            if (ServerService.isConnected()) {
                server = ServerService.getServer(targetConnectionId);
                return server != null;
            }
            try {
                Thread.sleep(250);
            } catch (InterruptedException e) {
                return false;
            }
        }
        return false;
    }

    /**
     * 电脑上的目标目录：{@code AbsDir}（为空时用接收器的 home）+ {@code SubDir}。
     *
     * <p>只有一个实现：{@link #prepareAndSend} 拿它给面板显示「保存到：…」，
     * {@link #startNextGroup} 拿它真正去建 {@link Directory}，两边绝不会不一致。
     * 取不到（还没连上）时返回空串。
     */
    private String resolveRemoteBase() {
        HFXServer srv = server;
        if (srv == null) {
            return "";
        }
        String remoteBase = Config.getInstance(this).getRemoteAbsDir();
        if (remoteBase == null || remoteBase.trim().isEmpty()) {
            remoteBase = srv.getRemoteHomeDir();
        }
        if (remoteBase == null) {
            return "";
        }
        String subDir = Config.getInstance(this).getRemoteSubDir();
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

    private void startNextGroup(int myRound) {
        if (!isCurrentRound(myRound)) {
            // 上一轮的收尾（新一轮已经开始了）：这一轮什么都不能碰
            return;
        }
        if (finished) {
            return;
        }
        if (cancelled) {
            finish();
            return;
        }
        Group group = groups.poll();
        if (group == null) {
            onAllComplete(myRound);
            return;
        }
        if (server == null || !ServerService.isConnected()) {
            fail(getString(R.string.share_pc_not_connected), myRound);
            return;
        }
        // 帧循环 / 面板从这一刻起归这一轮所有（onFrame 每帧核对）
        activeRound = myRound;
        // 传输真的开始了：「正在等待电脑连接 / 正在复制」这类过程文案从这里让位，
        // 那行要留给合计速率（见 currentNote：note 非空就会把速率顶掉）
        statusText = "";
        transferring = true;
        currentGroupTotal = group.totalBytes;
        currentGroupSent = 0;
        // 换文件了：每条通道的「已完成」重新算（面板上这一列是给当前文件看的）
        doneChannels.clear();
        if (startTime == 0) {
            startTime = System.currentTimeMillis();
        }
        // 真值、速率、校验进度全靠这个帧循环（不再信回调里的 position）：
        // vsync 每帧看一眼，到 100ms 就采一次真值推给面板（见 onFrame）
        frameClock.start();
        HFXLog.d("share: 开始发送 round=" + myRound + " dir=" + group.dir
                + " files=" + group.files.size() + " total=" + group.totalBytes);
        try {
            Directory localDir = new Directory(group.dir, Directory.getCurrentFileSystem());
            // 目标目录的计算抽成 resolveRemoteBase()：面板那行「保存到：…」显示的就是它
            String remoteBase = resolveRemoteBase();
            if (remoteBase.isEmpty()) {
                remoteBase = server.getRemoteHomeDir();
            }
            remoteTarget = remoteBase;
            Directory remoteDir = new Directory(remoteBase, server.getRemoteFileSystem());
            new SendFilesToRemoteTask(new TransferHandler(myRound), server, group.files, localDir, remoteDir).execute();
        } catch (Throwable e) {
            fail(String.valueOf(e), myRound);
        }
    }

    private void onAllComplete(int myRound) {
        if (!isCurrentRound(myRound)) {
            // 上一轮的收尾走到这里（新一轮已经开始了）：完成文案不能写到新一轮的面板上
            return;
        }
        if (finished) {
            return;
        }
        finished = true;
        transferring = false;
        String target = server != null ? server.getRemoteHomeDir() : "";
        String summary = buildFileSummary();
        long displayPeak = peakSpeedBytes;
        if (displayPeak <= 0 && totalBytes > 0 && startTime > 0) {
            long elapsedMs = Math.max(1L, System.currentTimeMillis() - startTime);
            displayPeak = totalBytes * 1000L / elapsedMs;
        }
        final long peakForToast = displayPeak;
        Config.getInstance(this).setLastPeakBytesPerSecond(displayPeak);
        StringBuilder speedText = new StringBuilder();
        for (java.util.Map.Entry<String, Double> entry : channelSpeeds.entrySet()) {
            if (speedText.length() > 0) speedText.append(",");
            speedText.append(entry.getKey()).append("=").append(String.format(java.util.Locale.US, "%.1f", entry.getValue()));
        }
        Config.getInstance(this).setChannelSpeeds(speedText.toString());
        handler.post(() -> {
            if (!isCurrentRound(myRound)) {
                // 这一拍才轮到这里，期间用户又分享了一次：这一轮的完成文案到此为止
                return;
            }
            // 采样循环先停，再把进度拉满、把完成文案交给面板（面板自己会把进度补间到底）
            frameClock.stop();
            if (truthTotal > 0L) {
                shownBytes = truthTotal;
            }
            flushPanel(getString(R.string.share_complete_files, summary, Utils.formatSpeed(peakForToast)),
                    false, myRound);
            // 传完了：面板只剩结果文案，点窗口外面或返回键都能退出（这里也会自动关）
            enableDismissOutside();
            handler.postDelayed(() -> {
                if (isCurrentRound(myRound)) {
                    finish();
                }
            }, 1500);
        });
        Toast.makeText(getApplicationContext(),
                getString(R.string.share_complete_files, summary, Utils.formatSpeed(peakForToast)),
                Toast.LENGTH_SHORT).show();
    }

    private void fail(String message, int myRound) {
        if (!isCurrentRound(myRound)) {
            // 上一轮（已经被新一轮顶掉）的失败：一个字都不能写进界面
            return;
        }
        if (finished) {
            return;
        }
        finished = true;
        transferring = false;
        Log.w(TAG, "share failed: " + message);
        HFXLog.d("share: 失败 round=" + myRound + " " + message);
        handler.post(() -> {
            if (!isCurrentRound(myRound)) {
                return;
            }
            statusText = "";
            frameClock.stop();
            // 失败：进度清零、速率收起、文案换成失败原因
            flushPanel(getString(R.string.share_failed, message), true, myRound);
            // 失败后窗口留着让人看清原因：点窗口外面或返回键都能退出
            enableDismissOutside();
        });
    }

    // ------------------------------------------------------------------
    // 传输回调（主线程）
    // ------------------------------------------------------------------

    private class TransferHandler implements BTransferFileCallback {

        /** 这一组属于哪一轮：二次分享把 round 顶掉以后，上一轮的回调一律不认 */
        private final int myRound;

        TransferHandler(int myRound) {
            this.myRound = myRound;
        }

        @Override
        public void onFileUploading(String iName, String path, long targetSize, long totalSize) {
            if (!isCurrentRound(myRound)) {
                // 上一轮的回调：主线程队列里可能还压着几条，别把旧文件名写进新一轮的面板
                return;
            }
            // 只记文件名。进度不在这里算：分段发送时 targetSize 是「这一块的末尾位置」，
            // 块是乱序并行到达的，拿它当进度就会跳来跳去、甚至一开始就显示满
            //（用户看到的「600.00MB/600.00MB · 0B/s」就是这个）。进度统一由帧循环
            // 每 100ms 采一次块计数，把真值推给面板（见 onFrame / applyToPanel）。
            currentFileName = new File(path).getName();
        }

        @Override
        public void onFileDownloading(String iName, String path, long targetSize, long totalSize) {
        }

        @Override
        public void onSpeedInfo(List<TrafficInfo> trafficInfoList) {
            if (!isCurrentRound(myRound)) {
                return;
            }
            long bytes = 0;
            for (TrafficInfo info : trafficInfoList) {
                bytes += info.uploadTraffic;
            }
            // 只留统计：这是「每秒一次的流量回调」，一秒里一个字节都没有的场景
            //（末尾只剩一条蓝牙，0.1~0.5MB/s）根本收不到值，拿它刷屏幕就是常驻的 0B/s
            //（用户报的「速率几乎一直显示 0B/s」）。屏幕上的速率改由帧循环
            // 按块真值的增量算（见 updateSpeed），再逐帧补间到文字上。
            peakSpeedBytes = Math.max(peakSpeedBytes, bytes);
        }

        @Override
        public void onChannelComplete(String iName, long traffic, long time) {
            if (!isCurrentRound(myRound)) {
                // 上一轮的通道收尾：doneChannels / channelSpeeds 都是新一轮在用的表，
                // 混进一条旧通道名，面板上那一行就会在「正在发送」和「已完成」之间乱跳
                return;
            }
            if (iName != null) {
                // 这条通道把当前文件发完了（EOF 已经发出）：面板上这一行显示「已完成」
                doneChannels.add(iName);
            }
            if (time > 0) {
                double mbps = traffic / 1024.0 / 1024.0 / (time / 1000.0);
                channelSpeeds.put(iName, mbps);
            }
        }

        @Override
        public void onChannelError(String iName, int errorType, String message) {
            if (errorType == TransferFileCallback.ERROR_TYPE_EXCEPTION && !TextUtils.isEmpty(message)) {
                Log.w(TAG, "channel " + iName + ": " + message);
            }
        }

        @Override
        public void onReadFileError(String message) {
            if (!isCurrentRound(myRound)) {
                return;
            }
            fail(message, myRound);
        }

        @Override
        public void onWriteFileError(String message) {
            if (!isCurrentRound(myRound)) {
                return;
            }
            fail(message, myRound);
        }

        @Override
        public void onComplete(boolean isUpload, long traffic, long time) {
            if (!isCurrentRound(myRound)) {
                // 上一轮被取消后走到这里：这一轮已经作废，记账和下一组都不能碰
                return;
            }
            // 只记账，不直接改进度条：显示值是按速率逐帧推的，这里再按真值顶一下会把它推到真值前面。
            completedBytes += currentGroupTotal;
            // 这一组的字节已经并进 completedBytes 了，但 sendProgress 里的账本还是这一组的
            //（新一组的任务线程还没来得及 reset）。这期间 snapshot 会返回同一份 doneBytes，
            // 再和 completedBytes 相加就把这一组算了两遍 —— 真值凭空多出一整组，
            // 界面会闪一下满进度。把「这一组的账本路径」记下来，采样时按它识别陈旧快照。
            accountedLedgerPath = ledgerPath;
            startNextGroup(myRound);
        }

        @Override
        public void onIncomplete() {
            if (!isCurrentRound(myRound)) {
                // 这一轮是我们自己取消掉的（二次分享会先取消上一轮），别把失败甩到新的一轮上
                return;
            }
            fail(getString(R.string.chuan_shu_shi_fa_sheng_yi_chang), myRound);
        }

        @Override
        public void onError(Throwable th) {
            if (!isCurrentRound(myRound)) {
                return;
            }
            fail(String.valueOf(th), myRound);
        }

        @Override
        public void onComplete() {
        }
    }

    private String buildFileSummary() {
        if (sentFileNames.isEmpty()) {
            return getString(R.string.share_files_fallback);
        }
        StringBuilder builder = new StringBuilder();
        for (int i = 0; i < sentFileNames.size(); i++) {
            if (i > 0) {
                builder.append("、");
            }
            builder.append(sentFileNames.get(i));
        }
        if (sentFileCount > sentFileNames.size()) {
            builder.append(" 等 ").append(sentFileCount).append(" 个文件");
        }
        return builder.toString();
    }

    // ------------------------------------------------------------------
    // 进度：真值只信 100ms 采样拍，补间整块交给面板（面板内部 200ms tween）
    // ------------------------------------------------------------------

    /**
     * 每帧回调（vsync）。插值已经整块删掉了 —— 面板（{@code ReceiveProgressPanel}）内部
     * 自己做 200ms 的 tween，这里只做一件事：每 {@link #TICK_MS} 采一次真值，再推给面板。
     *
     * <p>为什么还留着逐帧节拍、不改成 handler.postDelayed：vsync 回调天然和屏幕对齐，
     * 采样时刻稳定；它同时是「这一屏还活着」的心跳，收尾时停掉即可。
     */
    private void onFrame(long frameTimeNanos) {
        if (finished || !transferring || !isCurrentRound(activeRound)) {
            if (!isCurrentRound(activeRound)) {
                // 上一轮留下的那次 vsync 回调：收工时记一笔，别再往新一轮的面板上写
                HFXLog.d("share: 上一轮的帧循环收工 activeRound=" + activeRound + " round=" + round);
            }
            frameClock.stop();
            return;
        }
        // vsync 时刻和 elapsedRealtime 是同一个时间基准（都来自 nanoTime），
        // 折成毫秒以后，采样节拍和速率计算共用一条时间线，不会互相打架
        long now = frameTimeNanos / 1_000_000L;
        if (lastSampleAt == 0L || now - lastSampleAt >= TICK_MS) {
            lastSampleAt = now;
            sampleTruth(now);
            applyToPanel();
        }
    }

    /**
     * 每 100ms 采一次真值。口径没变，仍然是那两个「按块统计」的数：
     * <ul>
     *   <li>数据段：{@code sendProgress.snapshot().doneBytes}（+ 已完成组的字节）；</li>
     *   <li>校验段（{@code verifyTotal > 0}）：{@code verifyDoneBytes()}。</li>
     * </ul>
     * 这里只取数、只做算术，不碰界面。
     */
    private void sampleTruth(long now) {
        long verifyTotal = HFXService.verifyTotalBytes();
        long verifyDone = HFXService.verifyDoneBytes();
        boolean verify = verifyTotal > 0L;
        long truth;
        long total;
        if (verify) {
            // 校验阶段：数据已经发完，两端在算整文件 SHA-256。这条进度是「读完 4MB 加一次」，
            // 一样是跳着长的，跳变由面板补间成连续的
            truth = verifyDone;
            total = verifyTotal;
            lastSnapshot = null;
        } else {
            HFXServer srv = server;
            BlockProgress.Snapshot snapshot = null;
            String path = null;
            if (srv != null) {
                try {
                    snapshot = srv.sendProgress.snapshot();
                    // 账本属于哪个文件：用来识别「上一组的旧账还没被清」的那一小段窗口
                    path = srv.sendProgress.currentPath();
                } catch (Throwable ignored) {
                    // 传输刚结束/账本正在被清空：这一拍沿用上一拍的真值，下一拍再取
                }
            }
            if (snapshot != null) {
                if (accountedLedgerPath != null && accountedLedgerPath.equals(path)) {
                    // 这一组的字节已经并进 completedBytes 了，账本却还停在它上面：
                    // 采信 doneBytes 会把这一组重复算一遍，所以这一拍先当 0。
                    // 一旦账本换了文件（或已经被 reset 成空），下面就会正常采信。
                    currentGroupSent = 0L;
                } else {
                    accountedLedgerPath = null;
                    currentGroupSent = snapshot.doneBytes;
                    ledgerPath = path == null ? "" : path;
                }
            }
            // 分段条顺手用这一拍的快照，不用再取第二次
            lastSnapshot = snapshot;
            truth = completedBytes + currentGroupSent;
            total = totalBytes;
        }
        boolean phaseChanged = verify != verifyPhase;
        boolean rewound = truth < truthBytes;
        verifyPhase = verify;
        truthBytes = truth;
        truthTotal = total;
        if (phaseChanged || rewound) {
            // 换阶段（数据 → 校验）或者账本被重置（真值变小）：基线直接挪到新真值上。
            // 这是整条进度上唯一允许往回退的时刻，因为它不是「进度倒退」，而是「上一拍那个数是错的」：
            // 上一组结束时 completedBytes 加上还没清空的 doneBytes 会重复算一次，
            // 不退回来就会一直挂在一个偏大的数上（就是用户报的「600.00MB/600.00MB · 0B/s」）。
            resetBaseline(truth);
        } else if (truth > shownBytes) {
            // 只增不减：块是 1MiB 一颗地记账的，显示值永远不超过真值
            shownBytes = truth;
        }
        // 合计速率：真值的增量 → 600ms 滑动窗口平均 → 轻度平滑（只用来记峰值，屏幕上的速率面板自己算）
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

    /**
     * 把这一拍的真值组装成面板要的状态，推给 {@link ShareProgress}。
     *
     * <p>面板和双窗口弹窗、应用外悬浮窗是同一个组件（{@code ReceiveProgressPanel}），
     * 里面自己做 200ms 的补间，所以这里只喂真值，不做任何平滑：
     * <ul>
     *   <li>{@code xferDone} / {@code xferBytes}：按块统计的已发字节（只增不减）；</li>
     *   <li>{@code xferTotal}：&lt;= 0 时面板自己走不确定态（只发了文件夹的情况）；</li>
     *   <li>{@code xferSegments}：分段条 = 块空间里「相邻块归属变化」的区间，只有单文件才有意义；</li>
     *   <li>{@code xferChannels}：各通道明细，速率按相邻两拍的增量做低通；</li>
     *   <li>{@code xferNote}：一句话说明，取值优先级见 {@link #currentNote()}。</li>
     * </ul>
     */
    private void applyToPanel() {
        if (!isCurrentRound(activeRound)) {
            // 最后一层拦网：帧循环属于哪一轮就只写哪一轮的面板
            return;
        }
        List<ChannelStat> channels = buildChannels(false);
        boolean allChannelsFinished = true;
        for (ChannelStat channel : channels) {
            if (!channel.getFinished()) {
                allChannelsFinished = false;
                break;
            }
        }
        // 数据已经发完、还在等电脑端复算整文件 SHA-256：这段时间没有字节在流动，
        // 必须给个说法，否则界面就是「79x/800 MB · 0 B/s」一动不动，看着像卡死（实测卡 2 分钟）
        waitingPc = !verifyPhase && truthTotal > 0L && truthBytes >= truthTotal && allChannelsFinished;
        // 多文件/文件夹没有跨文件的块空间，分段画出来会和整条进度对不上，退回整条连续进度
        List<SegInfo> segments;
        BlockProgress.Snapshot snapshot = lastSnapshot;
        if (!verifyPhase && sentFileCount <= 1 && truthTotal > 0L && snapshot != null) {
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
        pushPanel(true, segments, channels);
    }

    /**
     * 各通道明细：名字 / 已发字节 / 实时速率 / 是否已收尾。
     *
     * <p>字节取 {@code trafficSnapshot()} 的累计发出量（真值），速率按相邻两拍的增量做 1 秒低通，
     * 和双窗口传输页同一套口径。{@code idle=true} 时速率一律给 0：暂停/收尾/等电脑校验这些时间里
     * 没有字节在流动，留着旧数字会让「已完成」这一列永远显示不出来。
     */
    private List<ChannelStat> buildChannels(boolean idle) {
        HFXServer srv = server;
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
            boolean finished = doneChannels.contains(name);
            long speed = idle || finished ? 0L : last + (instant - last) * dt / 1000L;
            if (speed < 0L) {
                speed = 0L;
            }
            channelLastSpeed.put(name, speed);
            out.add(new ChannelStat(name, bytes, speed, finished));
        }
        return out;
    }

    /** 新的一次传输：速率的采样窗口、显示用的字节全部归零（否则会接着上一次的数往前跑） */
    private void resetProgressSmoothing() {
        speedSamples.clear();
        lastSampleBytes = 0L;
        lastGrowAt = 0L;
        lastGrowGapMs = 0L;
        smoothedSpeed = 0L;
        shownBytes = 0L;
        truthBytes = 0L;
        truthTotal = 0L;
        verifyPhase = false;
        ledgerPath = "";
        accountedLedgerPath = null;
        lastSnapshot = null;
        lastSampleAt = 0L;
        speedStateStarted = false;
        doneChannels.clear();
        channelLastBytes.clear();
        channelLastSpeed.clear();
        channelSampleAt = 0L;
        // 面板那行的说明也跟着重来：上一次的「已发送/失败」结果、目标目录都不能留着
        statusText = "";
        resultText = "";
        remoteTarget = "";
        waitingPc = false;
    }

    /**
     * 用按块真值的增量算速率：600ms 滑动窗口平均 + 和上一拍 0.75/0.25 混合。
     *
     * <p>为什么不用 {@code onSpeedInfo}：那是「每秒一次的流量回调」，一秒里一个字节都没有的
     * 收尾阶段（只剩一条蓝牙，0.13MB/s）根本不会触发，界面就常驻在 0B/s。
     * 按块真值的增量算，任何速度下都有数；再靠窗口平均把「块一到就尖峰」摊平。
     *
     * @param sent 这一拍按块统计出来的已发字节（真值）
     * @param now  这一拍的时刻（ms，和 vsync 同一条时间线）
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
            // 真值变小了：换文件/换目录时传输核心把账本清了。采样重来，但速率不清零
            //（数据还在接着发），基线挪到新的真值上就够
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
                // 间隔封顶：一次十几秒的停顿（暂停/电脑端卡了一下）不能把后面的门限拖长
                lastGrowGapMs = Math.min(SPEED_IDLE_MAX_MS, now - previousGrowAt);
            }
            lastGrowAt = now;
        }
        lastSampleBytes = sent;
        speedSamples.addLast(new long[]{now, sent});
        // 只留窗口内的采样点，但至少留两个（否则算不出间隔）
        while (speedSamples.size() > 2 && now - speedSamples.peekFirst()[0] > SPEED_WINDOW_MS) {
            speedSamples.removeFirst();
        }
        // 闲置归零：这里必须直接归零（暂停、等电脑校验、已经收尾都落在这一档），
        // 靠 0.75/0.25 往下蹭会蹭好几分钟，期间界面一直挂着旧的高值。
        //
        // 但门限不能用固定的 700ms：块是 1MB 一颗，0.13MB/s 时一颗要七八秒，
        // 固定 700ms 会把「慢但在流」误判成「没流」，屏幕又变成 0B/s（就是用户报的那个）。
        // 所以门限按最近一次块间隔放大到 1.5 倍：连续高速传（间隔几十毫秒）时它就是 700ms，
        // 只有慢通道才放宽；通道真断了，最多也就多等一块的时间。
        long idleLimitMs = Math.min(SPEED_IDLE_MAX_MS,
                Math.max(SPEED_IDLE_MS, lastGrowGapMs + lastGrowGapMs / 2));
        if (now - lastGrowAt >= idleLimitMs) {
            smoothedSpeed = 0;
            return;
        }
        // 这一拍没有新字节（块还没到）：不重算。拿 0 去算只会得到
        // 「块一到就尖峰、块没到就是 0」，正是要修的那个「跳得僵硬」。
        // 真没数据在流的情况由上一步的闲置归零负责。
        if (!grew) {
            return;
        }
        long[] oldest = speedSamples.peekFirst();
        long spanMs = now - oldest[0];
        long deltaBytes = sent - oldest[1];
        // 慢通道是「好几秒才到一块」：这一块整个落进 600ms 窗口会被算成 2MB/s，
        // 还在后面的空拍里被反复放大。离上一个数据点已经超过一个窗口时，
        // 改用「这次涨了多少 ÷ 隔了多久」，这才是这条慢通道的真实速率。
        if (lastGrowGapMs > SPEED_WINDOW_MS) {
            spanMs = lastGrowGapMs;
            deltaBytes = sent - previousBytes;
        }
        // 采样太短算不准（刚起来那一拍）；超过上限说明是重连后的离谱值，不采信
        if (spanMs < 100L || deltaBytes <= 0) {
            return;
        }
        long instant = deltaBytes * 1000L / spanMs;
        if (instant > SPEED_MAX_BYTES_PER_SEC) {
            return;
        }
        // 窗口平均值再和上一拍 0.75/0.25 混合：只削掉残余抖动，不拖慢响应
        smoothedSpeed = smoothedSpeed <= 0 ? instant : (smoothedSpeed * 3 + instant) / 4;
        // 慢通道下 onSpeedInfo 拿不到值，峰值也就记不上，这里补一份
        peakSpeedBytes = Math.max(peakSpeedBytes, smoothedSpeed);
    }

    /**
     * 面板那一行说明（xferNote）的取值，优先级从高到低（2026-11-14 按用户要求重排）：
     * <ol>
     *   <li>完成 / 失败 / 取消这类<b>最终结果</b>（它们一出现就该压住别的）；</li>
     *   <li>校验中 N%（数据发完，两端在算整文件 SHA-256）；</li>
     *   <li>等待电脑校验…（数据发完、各通道都 EOF，但电脑端还没开始复算）；</li>
     *   <li>准备 / 等待电脑连接 / 正在复制 这类过程状态文案 —— <b>只在还没有任何数据在动时</b>
     *       占位（{@link #shownBytes} 为 0 且平滑速率也为 0，见 {@link #dataFlowing()}）；</li>
     *   <li>保存到：电脑名 目标目录（老 share_hint 那行）—— 最低优先级，
     *       <b>只在完全空闲的初始态</b>（进程里没有传输、也没有过程文案）才说。</li>
     * </ol>
     * 面板那行是「已发 / 总量 · X」，X 的位置有 note 就显示 note、没有才显示面板自己算的合计速率。
     * 所以：
     * <ul>
     *   <li><b>只要有字节在流动就必须让 note 为空</b>（用户明确要求「传输中必须看到合计速率」）。
     *       以前 {@code statusText} 会一直停在「正在等待电脑连接」上直到传输结束，
     *       那行就永远看不见速度 —— 这是用户报的「从来不显示合计速率」的根因；</li>
     *   <li>「已暂停」<b>不在这条链里</b>：面板自己有 paused 状态（按钮变「继续」+ 进度条样式），
     *       那一行由面板的 {@code xferPaused} 分支写「已暂停」。塞进 note 只会在继续之后
     *       留下一条擦不掉的残留文案。</li>
     * </ul>
     */
    private String currentNote() {
        if (!resultText.isEmpty()) {
            return resultText;
        }
        if (verifyPhase) {
            return "校验中 " + (truthBytes * 100L / Math.max(1L, truthTotal)) + "%";
        }
        if (waitingPc) {
            return "等待电脑校验…";
        }
        if (dataFlowing()) {
            // 有字节在动：这一行必须留给合计速率
            return "";
        }
        if (!statusText.isEmpty()) {
            return statusText;
        }
        if (transferring) {
            // 已经在传、只是这一拍还没见着块（或刚好暂停着）：也别把「保存到」翻出来，
            // 让那行显示速率（0 B/s）——暂停时面板会自己换成「已暂停」
            return "";
        }
        return targetNote();
    }

    /**
     * 有没有数据在流动：显示值涨过，或者平滑速率不是 0。
     *
     * <p>「过程文案能不能占那行」全靠它 —— done == 0 且 speed == 0 就是还没开跑。
     */
    private boolean dataFlowing() {
        return shownBytes > 0L || smoothedSpeed > 0L;
    }

    /** 「保存到：电脑A /storage/emulated/0/Download」：目录和 startNextGroup 真正发往的是同一份计算 */
    private String targetNote() {
        String dir = remoteTarget;
        HFXServer srv = server;
        if (dir == null || dir.isEmpty() || srv == null) {
            return "";
        }
        return middleEllipsis("保存到：" + srv.getDisplayName() + " " + dir.replace('\\', '/'), 42);
    }

    /**
     * 太长就从中间省略（面板那行放不下）。
     *
     * <p>头部多留一点：要保住「保存到：电脑名」，尾部也要保住目录的最后一级 ——
     * 那才是用户真正想知道"存到哪个文件夹"的部分。
     */
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

    /** 把当前状态推给面板（active=true 表示正在传输，面板那边只用来判断要不要画底部按钮） */
    private void pushPanel(boolean active, List<SegInfo> segments, List<ChannelStat> channels) {
        if (!isCurrentRound(activeRound)) {
            // 界面归哪一轮就只写哪一轮。调用方本来都过了轮次戳，这里是最后一道拦网
            // （handleIntent 会同步把 activeRound 设成新号码，所以正常路径不会被它挡掉）
            return;
        }
        HFXServer srv = server;
        ShareProgress.update(
                active,
                // 和双窗口弹窗读的是同一个闸门：暂停后那行写「已暂停」，按钮变成「继续」
                active && srv != null && srv.isTransferPaused(),
                currentFileName,
                shownBytes,
                truthTotal,
                shownBytes,
                segments,
                channels,
                currentNote(),
                server != null ? server.getDisplayName() : "");
    }

    /**
     * 面板上的过程状态：正在准备 / 正在等待电脑连接 / 正在复制。
     *
     * <p>只记下来，真正显示什么由 {@link #currentNote()} 按优先级决定
     * （例如已经知道目标目录时，优先显示「保存到：…」）。
     */
    private void showStatus(String text, int myRound) {
        handler.post(() -> {
            if (!isCurrentRound(myRound)) {
                // 上一轮的准备线程（还在解析 / 复制文件）：它已经作废，别改新一轮的文案
                return;
            }
            statusText = text;
            resultText = "";
            pushPanel(false, Collections.emptyList(), buildChannels(true));
        });
    }

    /**
     * 收尾（完成 / 失败 / 取消）时把面板最后刷一次：速率一律清零，说明文案换成结果。
     *
     * @param clearProgress true 时把进度也清零（失败、取消）
     */
    private void flushPanel(String note, boolean clearProgress, int myRound) {
        if (!isCurrentRound(myRound)) {
            return;
        }
        if (clearProgress) {
            shownBytes = 0L;
        }
        resultText = note == null ? "" : note;
        pushPanel(false, Collections.emptyList(), buildChannels(true));
    }

    // ------------------------------------------------------------------
    // Uri -> File
    // ------------------------------------------------------------------

    private File resolve(Uri uri, int myRound) {
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
                return copyToCache(uri, myRound);
            }
        } catch (Exception e) {
            Log.w(TAG, "cannot resolve " + uri, e);
        }
        return null;
    }

    private String resolveContentPath(Uri uri) {
        try {
            if (DocumentsContract.isDocumentUri(this, uri)) {
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
            try (Cursor cursor = getContentResolver().query(uri,
                    new String[]{MediaStore.MediaColumns.DATA}, null, null, null)) {
                if (cursor != null && cursor.moveToFirst()) {
                    int index = cursor.getColumnIndex(MediaStore.MediaColumns.DATA);
                    if (index >= 0) {
                        return cursor.getString(index);
                    }
                }
            }
        } catch (Exception e) {
            Log.d(TAG, "resolveContentPath: " + e);
        }
        return null;
    }

    private File copyToCache(Uri uri, int myRound) throws IOException {
        String name = queryDisplayName(uri);
        File dir = new File(getCacheDir(), "share");
        if (!dir.exists() && !dir.mkdirs()) {
            throw new IOException("cannot create " + dir);
        }
        File out = uniqueFile(dir, name);
        long copied = 0;
        long lastUpdate = 0;
        try (InputStream in = getContentResolver().openInputStream(uri)) {
            if (in == null) {
                throw new IOException("cannot open " + uri);
            }
            try (FileOutputStream fos = new FileOutputStream(out)) {
                byte[] buffer = new byte[256 * 1024];
                int read;
                while ((read = in.read(buffer)) > 0) {
                    if (cancelled) {
                        break;
                    }
                    fos.write(buffer, 0, read);
                    copied += read;
                    if (copied - lastUpdate > 8L * 1024 * 1024) {
                        lastUpdate = copied;
                        showStatus(getString(R.string.share_copying_file, Utils.formatFileSize(copied)), myRound);
                    }
                }
            }
        }
        return out;
    }

    private String queryDisplayName(Uri uri) {
        String name = null;
        try (Cursor cursor = getContentResolver().query(uri,
                new String[]{OpenableColumns.DISPLAY_NAME}, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                int index = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                if (index >= 0) {
                    name = cursor.getString(index);
                }
            }
        } catch (Exception ignored) {
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

    /**
     * 返回键和点遮罩同一套出口（{@link #dismissPanel()}）：
     * 传输中收起界面、后台继续传，其它情况直接退出。
     */
    @Override
    public void onBackPressed() {
        dismissPanel();
    }

    @Override
    protected void onPause() {
        super.onPause();
        // 界面不可见了就别再占着 vsync（传输线程不受影响，通知栏那条进度由 ServerService 出）
        frameClock.stop();
    }

    @Override
    protected void onResume() {
        super.onResume();
        collapsed = false;
        // 从后台回来（收起的界面被重新打开，或切走又切回）：采样重新起拍 + 立刻补一帧，
        // 否则面板会先停在「离开那一刻」的进度上，等下一拍（100ms）才动。
        resumePanelUpdates();
    }

    /** 重新开始往面板推数据（只在传输中才有意义；首次进页面时 transferring 还是 false，直接返回） */
    private void resumePanelUpdates() {
        if (finished || !transferring) {
            return;
        }
        if (!isCurrentRound(activeRound)) {
            // 收起的界面被重新打开时，这条 Activity 上的状态可能已经是上一轮的了：
            // 补的那一帧只属于 activeRound，别把旧数据写回面板
            return;
        }
        frameClock.start();
        // 和 onFrame 同一条时间线（vsync 的 frameTimeNanos / 1e6）
        long now = System.nanoTime() / 1_000_000L;
        lastSampleAt = now;
        sampleTruth(now);
        applyToPanel();
    }

    @Override
    protected void onDestroy() {
        // 传输中不立 cancelled：面板收起来（甚至这个 Activity 被系统销毁）之后，
        // SendFilesToRemoteTask 那条线程还要把剩下的文件发完 —— 它只认这两个标志和 server。
        // 没在传（准备 / 等连接 / 复制 / 已经收尾）时照旧立起来，把那条准备线程停掉。
        if (!transferring) {
            cancelled = true;
        }
        // 界面没了就别再占着 vsync：Activity 销毁后控件引用也不能再碰
        activeRound = -1;
        frameClock.stop();
        // 面板的 ComposeView 跟着 Activity 一起没了：owner 走到 DESTROYED，状态清干净
        ShareProgress.release();
        super.onDestroy();
    }

    private static class Group {
        final String dir;
        final long totalBytes;
        final List<RemoteFile> files;

        Group(String dir, long totalBytes, List<RemoteFile> files) {
            this.dir = dir;
            this.totalBytes = totalBytes;
            this.files = files;
        }
    }
}