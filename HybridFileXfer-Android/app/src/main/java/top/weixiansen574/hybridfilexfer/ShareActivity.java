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
import android.view.ViewGroup;
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
    private volatile HFXServer server;
    private int targetConnectionId = -1;

    private long totalBytes;
    private long completedBytes;
    private long currentGroupTotal;
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
    /** 真值对应的总量（数据段 = totalBytes；校验段 = verifyTotal）；&lt;= 0 表示总量未知 */
    private long truthTotal;
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
    private final List<String> sentFileNames = new ArrayList<>();
    private int sentFileCount;
    /** 每条通道的平均速率（MB/s，完成时记一次，写进配置给主页显示） */
    private final java.util.Map<String, Double> channelSpeeds = new java.util.LinkedHashMap<>();
    private long startTime;

    // ---- 面板那一行说明（xferNote）的取值来源，见 currentNote() ----
    /** 准备 / 等待电脑连接 / 正在复制这类过程状态（传输中不占这行） */
    private String statusText = "";
    /** 完成 / 失败 / 取消这类最终结果：一出现就压住其它所有提示 */
    private String resultText = "";
    /** 电脑上的目标目录（和实际发送用的是同一份计算，见 resolveRemoteBase） */
    private String remoteTarget = "";
    /** 数据已经发完、还在等电脑端复算整文件校验和 */
    private boolean waitingPc;

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
        // 窗口跟着卡片走：宽高都 WRAP_CONTENT，背景透明 —— 窗口自带的圆角背景会变成
        // 「一个白框套着卡片」，所以只留卡片本身。
        getWindow().setBackgroundDrawable(new ColorDrawable(Color.TRANSPARENT));
        // 背景压暗交给 Window 自己的 dim：只压后面那层，卡片保持原大小。
        // （双窗口那层 35% 遮罩是 Compose 里 fillMaxSize 的黑 Box，搬到这种 WRAP_CONTENT
        // 小窗口里会把窗口直接撑成满屏，所以这里用 dim 而不是黑 Box。）
        getWindow().setDimAmount(0.35f);
        getWindow().setLayout(
                ViewGroup.LayoutParams.WRAP_CONTENT,
                ViewGroup.LayoutParams.WRAP_CONTENT);

        handleIntent(getIntent());
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        if (transferring) {
            Toast.makeText(this, R.string.share_busy, Toast.LENGTH_SHORT).show();
            return;
        }
        handleIntent(intent);
    }

    // ------------------------------------------------------------------
    // 面板上的按钮（由 ShareProgress.setActions 接进来）
    // ------------------------------------------------------------------

    /**
     * 面板上的「关闭」按钮，以及点面板外面：传输中就提示稍候，传完了就关窗口。
     *
     * <p>以前这是底部那颗 share_action 按钮的活；按钮现在归面板画，判断逻辑仍然在这里。
     */
    private void dismissPanel() {
        if (finished) {
            finish();
            return;
        }
        if (transferring) {
            Toast.makeText(this, R.string.share_busy, Toast.LENGTH_SHORT).show();
            return;
        }
        cancelled = true;
        finish();
    }

    /**
     * 面板上的「取消发送」：原来 share_action 按钮里那段取消逻辑原样搬过来。
     *
     * <p>传输线程是阻塞读写的，没有单独的取消指令：直接断开这条连接，
     * 电脑端会立刻停止并自动重连；半截文件留在电脑上（电脑端自己清理）。
     */
    private void cancelTransfer() {
        if (finished) {
            finish();
            return;
        }
        cancelled = true;
        finished = true;
        transferring = false;
        if (server != null) {
            ServerService.cancelTransfer(server.getId());
        }
        // 采样循环先停：后面那一次「清零」不能被下一拍的真值又盖回去
        frameClock.stop();
        // 面板最后刷一次：进度清零、速率收起、文案换成「已取消」
        flushPanel(getString(R.string.share_cancelled), true);
        handler.postDelayed(this::finish, 800);
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

    /** 收尾之后放开「点窗口外面关闭」：传输中不允许，免得一不小心把传输丢在半路 */
    private void enableDismissOutside() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP) {
            setFinishOnTouchOutside(true);
        }
    }

    private void handleIntent(Intent intent) {
        // 新的一次传输：速率的采样窗口、显示用的字节都从头开始（不然会接着上一次的数跑）
        resetProgressSmoothing();
        List<Uri> uris = extractUris(intent);
        if (uris.isEmpty()) {
            fail(getString(R.string.share_no_file));
            return;
        }
        showStatus(getString(R.string.share_preparing));
        new Thread(() -> prepareAndSend(uris), "SharePrepare").start();
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

    private void prepareAndSend(List<Uri> uris) {
        List<File> files = new ArrayList<>();
        long total = 0;
        for (int i = 0; i < uris.size(); i++) {
            if (cancelled) {
                return;
            }
            final int index = i + 1;
            showStatus(getString(R.string.share_resolving_file, index, uris.size()));
            File file = resolve(uris.get(i));
            if (file != null && file.exists()) {
                files.add(file);
                if (!file.isDirectory()) {
                    total += file.length();
                }
            }
        }
        if (files.isEmpty()) {
            fail(getString(R.string.share_no_file));
            return;
        }
        totalBytes = total;
        // 总量先交给面板：连接电脑可能要等十几秒，这段时间面板上写的是
        // 「0 B / 12.3 MB · 正在等待电脑连接…」，比一条 0 B / 0 B 有信息量（下一拍采样会覆盖它）
        truthTotal = total;

        showStatus(getString(R.string.share_waiting_pc));
        if (!ensureConnected()) {
            fail(getString(R.string.share_pc_not_connected));
            return;
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
        handler.post(this::startNextGroup);
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

    private void startNextGroup() {
        if (finished) {
            return;
        }
        if (cancelled) {
            finish();
            return;
        }
        Group group = groups.poll();
        if (group == null) {
            onAllComplete();
            return;
        }
        if (server == null || !ServerService.isConnected()) {
            fail(getString(R.string.share_pc_not_connected));
            return;
        }
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
        try {
            Directory localDir = new Directory(group.dir, Directory.getCurrentFileSystem());
            // 目标目录的计算抽成 resolveRemoteBase()：面板那行「保存到：…」显示的就是它
            String remoteBase = resolveRemoteBase();
            if (remoteBase.isEmpty()) {
                remoteBase = server.getRemoteHomeDir();
            }
            remoteTarget = remoteBase;
            Directory remoteDir = new Directory(remoteBase, server.getRemoteFileSystem());
            new SendFilesToRemoteTask(new TransferHandler(), server, group.files, localDir, remoteDir).execute();
        } catch (Throwable e) {
            fail(String.valueOf(e));
        }
    }

    private void onAllComplete() {
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
            // 采样循环先停，再把进度拉满、把完成文案交给面板（面板自己会把进度补间到底）
            frameClock.stop();
            if (truthTotal > 0L) {
                shownBytes = truthTotal;
            }
            flushPanel(getString(R.string.share_complete_files, summary, Utils.formatSpeed(peakForToast)), false);
            // 传完了：面板只剩结果文案，点窗口外面或返回键都能退出（这里也会自动关）
            enableDismissOutside();
            handler.postDelayed(this::finish, 1500);
        });
        Toast.makeText(getApplicationContext(),
                getString(R.string.share_complete_files, summary, Utils.formatSpeed(peakForToast)),
                Toast.LENGTH_SHORT).show();
    }

    private void fail(String message) {
        if (finished) {
            return;
        }
        finished = true;
        transferring = false;
        Log.w(TAG, "share failed: " + message);
        handler.post(() -> {
            frameClock.stop();
            // 失败：进度清零、速率收起、文案换成失败原因
            flushPanel(getString(R.string.share_failed, message), true);
            // 失败后窗口留着让人看清原因：点窗口外面或返回键都能退出
            enableDismissOutside();
        });
    }

    // ------------------------------------------------------------------
    // 传输回调（主线程）
    // ------------------------------------------------------------------

    private class TransferHandler implements BTransferFileCallback {

        @Override
        public void onFileUploading(String iName, String path, long targetSize, long totalSize) {
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
            fail(message);
        }

        @Override
        public void onWriteFileError(String message) {
            fail(message);
        }

        @Override
        public void onComplete(boolean isUpload, long traffic, long time) {
            // 只记账，不直接改进度条：显示值是按速率逐帧推的，这里再按真值顶一下会把它推到真值前面。
            completedBytes += currentGroupTotal;
            // 这一组的字节已经并进 completedBytes 了，但 sendProgress 里的账本还是这一组的
            //（新一组的任务线程还没来得及 reset）。这期间 snapshot 会返回同一份 doneBytes，
            // 再和 completedBytes 相加就把这一组算了两遍 —— 真值凭空多出一整组，
            // 界面会闪一下满进度。把「这一组的账本路径」记下来，采样时按它识别陈旧快照。
            accountedLedgerPath = ledgerPath;
            startNextGroup();
        }

        @Override
        public void onIncomplete() {
            fail(getString(R.string.chuan_shu_shi_fa_sheng_yi_chang));
        }

        @Override
        public void onError(Throwable th) {
            fail(String.valueOf(th));
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
        if (finished || !transferring) {
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
     * 面板那一行说明（xferNote）的取值，优先级从高到低：
     * <ol>
     *   <li>完成 / 失败 / 取消这类<b>最终结果</b>（它们一出现就该压住别的）；</li>
     *   <li>校验中 N%（数据发完，两端在算整文件 SHA-256）；</li>
     *   <li>等待电脑校验…（数据发完、各通道都 EOF，但电脑端还没开始复算）；</li>
     *   <li>准备 / 等待电脑连接 / 正在复制 这类过程状态文案；</li>
     *   <li>保存到：电脑名 目标目录（老 share_hint 那行，现在并到这一行）——
     *       <b>只在数据还没开始跑、也没有别的文案时说</b>。</li>
     * </ol>
     * 为什么不把「保存到」常驻：面板那行是「已发 / 总量 · X」，X 的位置
     * 有 note 就显示 note、没有才显示合计速率。常驻「保存到」等于传输全程看不见速度，
     * 那是用户最在意的数。所以传输中留空 → 显示速率，等传完/校验中显示的又是别的。
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
        if (!statusText.isEmpty()) {
            return statusText;
        }
        return targetNote();
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
    private void showStatus(String text) {
        handler.post(() -> {
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
    private void flushPanel(String note, boolean clearProgress) {
        if (clearProgress) {
            shownBytes = 0L;
        }
        resultText = note == null ? "" : note;
        pushPanel(false, Collections.emptyList(), buildChannels(true));
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

    private File copyToCache(Uri uri) throws IOException {
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
                        showStatus(getString(R.string.share_copying_file, Utils.formatFileSize(copied)));
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

    @Override
    public void onBackPressed() {
        if (transferring) {
            Toast.makeText(this, R.string.share_busy, Toast.LENGTH_SHORT).show();
            return;
        }
        cancelled = true;
        super.onBackPressed();
    }

    @Override
    protected void onDestroy() {
        cancelled = true;
        // 界面没了就别再占着 vsync：Activity 销毁后控件引用也不能再碰
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