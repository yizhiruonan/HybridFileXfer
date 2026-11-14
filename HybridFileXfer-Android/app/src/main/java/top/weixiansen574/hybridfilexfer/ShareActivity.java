package top.weixiansen574.hybridfilexfer;

import android.content.ClipData;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Environment;
import android.os.Handler;
import android.os.Looper;
import android.provider.DocumentsContract;
import android.provider.MediaStore;
import android.provider.OpenableColumns;
import android.text.TextUtils;
import android.util.Log;
import android.view.View;
import android.widget.Button;
import android.widget.ProgressBar;
import android.widget.TextView;
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
import top.weixiansen574.hybridfilexfer.core.FileBlock;
import top.weixiansen574.hybridfilexfer.core.HFXService;
import top.weixiansen574.hybridfilexfer.core.Utils;
import top.weixiansen574.hybridfilexfer.core.bean.Directory;
import top.weixiansen574.hybridfilexfer.core.bean.RemoteFile;
import top.weixiansen574.hybridfilexfer.core.bean.ServerNetInterface;
import top.weixiansen574.hybridfilexfer.core.bean.TrafficInfo;
import top.weixiansen574.hybridfilexfer.core.callback.TransferFileCallback;
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
     * 真值采样节拍：100ms 采一次，只取数、只做算术，不碰控件。
     *
     * <p>界面本身是<b>每帧</b>刷的（{@link FrameClock}，vsync 约 16.7ms 一次），
     * 这里只决定「多久重新看一眼真值」。真值是按块统计的（1MiB 一跳），
     * 100ms 看一眼足够跟上块的变化；屏幕上的连续性交给逐帧死推算
     * （见 {@link #advanceDisplayed}）。
     *
     * <p>反过来说，上一版是「100ms 一拍、一拍才推一次显示值」—— 一秒只有 10 帧，
     * 两个采样点之间的 90ms 屏幕完全静止，这才是「一格一格」的根。
     * 现在这个 100ms 只负责「看真值」，画由每帧的 {@link FrameClock} 负责。
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
    /** 显示用的字节数最多落后真值多少；落后更多就先拉近，免得进度条长时间落后于真值 */
    private static final long MAX_DISPLAY_LAG_BYTES = 8L * 1024 * 1024;
    /**
     * 进度条的分辨率（万分比）。
     *
     * <p>布局里的 max 是 100：600MB 的文件 1% 就是 6MB，4MB/s 时 1.5 秒才动一格。
     * 上一版改成 1000 档还不够 —— 一格 0.6MB，8MB/s 时也要 75ms（约 4 帧）才挪一次，
     * 而且 1000 个整数台阶本来就撑不满 60fps 的平滑度。
     * ProgressBar 内部把 progress 折算成 drawable 的 level（0~10000），
     * 所以 max 取 10000 正好用满它的分辨率：600MB 的文件一格是 60KB，条子看起来是连续爬的。
     */
    private static final int PROGRESS_MAX = 10000;
    /**
     * 速率文字的补间时间常数（ms）。
     *
     * <p>目标速率 100ms 才更新一次，直接显示就是「0 → 21.24MB/s」的一跳；
     * 每帧朝目标逼近 {@code 1-exp(-dt/tau)}，tau=120ms 时约 300ms 贴住目标，
     * 肉眼看到的是数字在爬而不是在跳，也不至于慢半拍。
     */
    private static final double SPEED_TAU_MS = 120.0;
    /** 单帧最大步长：掉帧/切后台回来时 dt 会很大，夹住它，免得一帧把显示值推出去老远 */
    private static final long MAX_FRAME_MS = 60L;
    /** 前瞻余量最多外推多少毫秒的速率 */
    private static final long LOOKAHEAD_MS = 250L;
    /**
     * 前瞻余量的硬上限：一整块（1MiB）。
     *
     * <p>真值是按整块记账的（{@code doneBytes} 只认写完整的块），所以显示值最多只允许
     * 外推「一块」—— 这正好是记账本身的量化误差，越过它就不是补误差而是真撒谎了。
     */
    private static final long LOOKAHEAD_MAX_BYTES = FileBlock.BLOCK_SIZE;
    /** 多台电脑时指定发给哪一台（连接号；不传或 -1 表示随便一台） */
    public static final String EXTRA_CONNECTION_ID = "top.weixiansen574.hybridfilexfer.extra.CONNECTION_ID";

    private final Handler handler = new Handler(Looper.getMainLooper());
    private final Queue<Group> groups = new ConcurrentLinkedQueue<>();

    private TextView statusView;
    private TextView hintView;
    private TextView speedView;
    private ProgressBar progressBar;
    private Button actionButton;

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

    // ---- 速率 / 显示进度的平滑状态：只在主线程（FrameClock 的每帧回调）里读写，不需要 volatile ----
    /**
     * 逐帧节拍（vsync）：进度条和两个文字全由它驱动。
     * 为什么用它、不用 handler 延时也不用 ObjectAnimator，见 {@link FrameClock} 的类注释。
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
    /** 低通后的速率（B/s），也就是「目标速率」：屏幕上的速率会逐帧补间到它，不再用 {@code onSpeedInfo} */
    private long smoothedSpeed;
    /** 这一拍的真值（数据段 = 已完成组 + 当前组按块已发；校验段 = verifyDone） */
    private long truthBytes;
    /** 真值对应的总量（数据段 = totalBytes；校验段 = verifyTotal）；&lt;= 0 表示总量未知 */
    private long truthTotal;
    /** 当前这一拍是不是在校验阶段 */
    private boolean verifyPhase;
    /** 上一次采信的那个账本属于哪个文件（{@code BlockProgress#currentPath()}） */
    private String ledgerPath = "";
    /** 已经把字节并进 completedBytes 的那个账本路径；非 null 时说明这个账本的数不能再采信 */
    private String accountedLedgerPath;
    /** 目标速率（B/s）的 double 版，逐帧补间用 */
    private double speedTarget;
    /**
     * 每帧插值出来的显示速率（B/s）。
     *
     * <p>字节推进和速率文字共用这一个值，两处不会出现「数字说 21MB/s、杆子却不动」的错位。
     */
    private double speedShown;
    /**
     * 显示用的已发字节（double）。
     *
     * <p>用 double 是为了保住小数：0.1MB/s 时一帧只前进 1.6KB，取整会被反复抹掉，
     * 慢链路上就又变成「不连续」。
     */
    private double shownBytesF;
    /** 上一帧已经画出去的显示字节：保证只增不减（进度条绝不回退） */
    private long shownBytes;
    /** 上一帧的 vsync 时刻（nanos），算每帧 dt 用 */
    private long lastFrameNanos;
    /** 上一次真值采样的时刻（ms） */
    private long lastSampleAt;
    /** 上一次画到进度条上的整数值：没变就不调 setProgress（省掉一次无意义的重绘） */
    private int lastBarValue = Integer.MIN_VALUE;
    /** 上一次真的写进 statusView 的字符串：内容一模一样就不 setText（省掉 measure/layout） */
    private String lastStatusText;
    /** 上一次真的写进 speedView 的速率字符串（statusView 里也复用它，不重复格式化） */
    private String lastSpeedText;
    /** {@code R.string.share_sending} 去掉参数后的前缀：逐帧拼字符串时不用再查资源、再走一遍 format */
    private String sendingPrefix = "";
    /** 拼字符串用的复用缓冲：每帧 new 一个 StringBuilder 才是真正会招 GC 的写法 */
    private final StringBuilder textBuffer = new StringBuilder(160);
    private final StringBuilder speedBuffer = new StringBuilder(24);
    private boolean speedStateStarted;
    private final List<String> sentFileNames = new ArrayList<>();
    private int sentFileCount;
    private final java.util.Map<String, Double> channelSpeeds = new java.util.LinkedHashMap<>();
    private long startTime;

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

        statusView = findViewById(R.id.share_status);
        hintView = findViewById(R.id.share_hint);
        speedView = findViewById(R.id.share_speed);
        // 「正在发送：%1$s」把占位参数挖掉，剩下的就是固定前缀（各语言里 %1$s 都在末尾）。
        // 逐帧要拼几十次字符串，每次去查一遍资源再走 String.format 是纯浪费。
        sendingPrefix = getString(R.string.share_sending, "");
        android.util.DisplayMetrics metrics = getResources().getDisplayMetrics();
        getWindow().setLayout(
                (int) (metrics.widthPixels * 0.90f),
                (int) (metrics.heightPixels * 0.45f));
        progressBar = findViewById(R.id.share_progress);
        // 布局里的 max 是 100，这里改成千分比：600MB 的文件 1% 就是 6MB，慢速率下要好几秒才动一格，
        // 加大分辨率后进度条才跟得上 100ms 的刷新节拍（见 PROGRESS_MAX 的注释）
        progressBar.setMax(PROGRESS_MAX);
        actionButton = findViewById(R.id.share_action);
        actionButton.setText(R.string.cancel);
        actionButton.setOnClickListener(v -> {
            if (finished) {
                finish();
                return;
            }
            if (transferring) {
                // 传输线程是阻塞读写的，没有单独的取消指令：直接断开这条连接，
                // 电脑端会立刻停止并自动重连；半截文件留在电脑上（电脑端自己清理）
                cancelled = true;
                finished = true;
                transferring = false;
                if (server != null) {
                    ServerService.cancelTransfer(server.getId());
                }
                // 动画循环先停：后面的 setProgress(0) 不能被某一帧的死推算又推回去
                frameClock.stop();
                setIndeterminate(false);
                progressBar.setProgress(0);
                lastBarValue = Integer.MIN_VALUE;
                // 取消/失败以后不该再挂着上一拍的速率，那一行直接收起来
                if (speedView != null) {
                    speedView.setVisibility(View.GONE);
                }
                showStatus(getString(R.string.share_cancelled));
                actionButton.setText(R.string.close);
                actionButton.setOnClickListener(x -> finish());
                handler.postDelayed(this::finish, 800);
                return;
            }
            cancelled = true;
            finish();
        });

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

    private void handleIntent(Intent intent) {
        // 新的一次传输：速率的采样窗口、显示用的字节都从头开始（不然会接着上一次的数跑）
        resetProgressSmoothing();
        List<Uri> uris = extractUris(intent);
        if (uris.isEmpty()) {
            fail(getString(R.string.share_no_file));
            return;
        }
        showStatus(getString(R.string.share_preparing));
        progressBar.setIndeterminate(true);
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
            postStatus(getString(R.string.share_resolving_file, index, uris.size()));
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

        postStatus(getString(R.string.share_waiting_pc));
        postHint(getString(R.string.share_waiting_pc_hint));
        if (!ensureConnected()) {
            fail(getString(R.string.share_pc_not_connected));
            return;
        }
        postHint(server != null ? server.getRemoteHomeDir() : "");

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
        if (startTime == 0) {
            startTime = System.currentTimeMillis();
        }
        setIndeterminate(false);
        // 进度、速率、校验进度全靠这个帧循环（不再信回调里的 position）：
        // vsync 每帧回调一次，屏幕才可能真的 60fps 连续（见 FrameClock）
        frameClock.start();
        try {
            Directory localDir = new Directory(group.dir, Directory.getCurrentFileSystem());
            String remoteBase = Config.getInstance(this).getRemoteAbsDir();
            if (remoteBase == null || remoteBase.trim().isEmpty()) {
                remoteBase = server.getRemoteHomeDir();
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
            // 先把帧循环停掉再把杆子拉满，否则下一帧的死推算会把它改回真值对应的位置
            frameClock.stop();
            setIndeterminate(false);
            progressBar.setProgress(PROGRESS_MAX);
            lastBarValue = PROGRESS_MAX;
            showStatus(getString(R.string.share_complete_files, summary, Utils.formatSpeed(peakForToast)));
            if (hintView != null) {
                hintView.setVisibility(View.GONE);
            }
            actionButton.setText(R.string.close);
            actionButton.setOnClickListener(v -> finish());
            if (speedView != null) {
                speedView.setVisibility(View.GONE);
            }
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
            setIndeterminate(false);
            progressBar.setProgress(0);
            lastBarValue = Integer.MIN_VALUE;
            if (speedView != null) {
                speedView.setVisibility(View.GONE);
            }
            showStatus(getString(R.string.share_failed, message));
            actionButton.setText(R.string.close);
            actionButton.setOnClickListener(v -> finish());
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
            // 每 100ms 采一次块计数、再逐帧死推算来刷（见 onFrame）。
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
    // 逐帧动画：真值只信 100ms 采样拍，屏幕上的值全部由每帧的死推算给出
    // ------------------------------------------------------------------

    /**
     * 每帧回调（vsync，约 16.7ms 一次），三件事按顺序做：
     * <ol>
     *   <li>每 {@link #TICK_MS} 采一次真值和速率目标（{@link #sampleTruth}，只做算术）；</li>
     *   <li>把显示值和显示速率往前推一步（{@link #advanceDisplayed}，纯算术）；</li>
     *   <li>写控件（{@link #applyToViews}）——值没变得看得出来就不写，省掉 measure/layout。</li>
     * </ol>
     *
     * <p>为什么不在这里再 {@code handler.post(...)}：这个方法本来就在主线程上跑，
     * post 只是把这一帧的更新拖到下一个消息里执行，等于又加了一层 16~33ms 的抖动
     * （上一版每拍都 post 一次，控件更新的时刻和节拍对不上，看着更抖）。
     */
    private void onFrame(long frameTimeNanos) {
        if (finished || !transferring) {
            frameClock.stop();
            return;
        }
        // vsync 时刻和 elapsedRealtime 是同一个时间基准（都来自 nanoTime），
        // 折成毫秒以后，采样节拍和速率计算共用一条时间线，不会互相打架
        long now = frameTimeNanos / 1_000_000L;
        long dtMs = lastFrameNanos == 0L ? 16L : (frameTimeNanos - lastFrameNanos) / 1_000_000L;
        lastFrameNanos = frameTimeNanos;
        // 掉帧、切后台回来时 dt 会很大：夹住，免得一帧把显示值推出去老远
        if (dtMs < 1L) {
            dtMs = 1L;
        } else if (dtMs > MAX_FRAME_MS) {
            dtMs = MAX_FRAME_MS;
        }
        if (lastSampleAt == 0L || now - lastSampleAt >= TICK_MS) {
            lastSampleAt = now;
            sampleTruth(now);
        }
        advanceDisplayed(dtMs);
        applyToViews();
    }

    /**
     * 每 100ms 采一次真值。口径没变，仍然是那两个「按块统计」的数：
     * <ul>
     *   <li>数据段：{@code sendProgress.snapshot().doneBytes}（+ 已完成组的字节）；</li>
     *   <li>校验段（{@code verifyTotal > 0}）：{@code verifyDoneBytes()}。</li>
     * </ul>
     * 这里只取数、只做算术，不碰任何控件。
     */
    private void sampleTruth(long now) {
        long verifyTotal = HFXService.verifyTotalBytes();
        long verifyDone = HFXService.verifyDoneBytes();
        boolean verify = verifyTotal > 0L;
        long truth;
        long total;
        if (verify) {
            // 校验阶段：数据已经发完，两端在算整文件 SHA-256。这条进度是「读完 4MB 加一次」，
            // 同样是跳着长的，所以一样要走死推算
            truth = verifyDone;
            total = verifyTotal;
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
            truth = completedBytes + currentGroupSent;
            total = totalBytes;
        }
        if (verify != verifyPhase || truth < truthBytes) {
            // 换阶段（数据 → 校验）或者账本被重置（真值变小）：显示基线直接挪到新真值上。
            // 这是整个动画里唯一允许往回退的时刻，因为它不是「进度倒退」，而是「上一帧那个数是错的」：
            // 上一组结束时 completedBytes 加上还没清空的 doneBytes 会重复算一次，显示值跟着被推高，
            // 不退回来就会一直挂在一个偏大的数上（就是用户报的「600.00MB/600.00MB · 0B/s」）。
            // 退一次，比一直显示错的强。
            resetDisplayBaseline(truth);
        }
        verifyPhase = verify;
        truthBytes = truth;
        truthTotal = total;
        // 速率：真值的增量 → 600ms 滑动窗口平均 → 轻度平滑，得到这一拍的目标速率
        updateSpeed(truth, now);
        speedTarget = smoothedSpeed;
    }

    /** 换阶段/换账本时的显示基线重置：显示值挪到真值，速率重新量 */
    private void resetDisplayBaseline(long truth) {
        shownBytesF = truth;
        shownBytes = truth;
        speedSamples.clear();
        speedStateStarted = false;
        smoothedSpeed = 0L;
        speedTarget = 0.0;
        speedShown = 0.0;
        lastSampleBytes = truth;
        lastGrowAt = 0L;
        lastGrowGapMs = 0L;
        // 强制下一帧重画一次
        lastBarValue = Integer.MIN_VALUE;
    }

    /** 把这一帧的值写进控件：进度条只在整数值变了才写，文字只在真的不一样时才 setText */
    private void applyToViews() {
        long total = truthTotal;
        if (total <= 0L) {
            // 只发了文件夹（没有可统计的字节数）：进度条保持不确定态，只更新文字，
            // 否则一条永远不动的 0% 杆子比转圈还像卡死
            if (!progressBar.isIndeterminate()) {
                progressBar.setIndeterminate(true);
                lastBarValue = Integer.MIN_VALUE;
            }
        } else {
            if (progressBar.isIndeterminate()) {
                // setIndeterminate 会重设 drawable 和动画，只在真的要变的时候调
                progressBar.setIndeterminate(false);
                lastBarValue = Integer.MIN_VALUE;
            }
            int bar = progressOf(shownBytes);
            if (bar != lastBarValue) {
                lastBarValue = bar;
                // 这里不用 setProgress(v, true)：自带的补间会把传入值当终点、再叠一条插值曲线，
                // 而我们每帧给的本来就是插值好的值，再套一层是双重平滑，还不好保证不越界
                progressBar.setProgress(bar);
            }
        }

        // 速率文字：先拼进复用缓冲，只有内容真的变了才 toString + setText。
        // statusView 和 speedView 共用同一个字符串实例，不重复格式化。
        speedBuffer.setLength(0);
        appendSpeed(speedBuffer, speedShown);
        boolean speedChanged = differs(lastSpeedText, speedBuffer);
        if (speedChanged) {
            lastSpeedText = speedBuffer.toString();
        }

        textBuffer.setLength(0);
        if (verifyPhase) {
            appendVerifyText(textBuffer, total);
        } else {
            // 文案排版和上一版逐字一致：前缀 + 文件名 + 「已发/总量」+ 速率，中间两个空格
            textBuffer.append(sendingPrefix);
            boolean hasField = false;
            if (!TextUtils.isEmpty(currentFileName)) {
                textBuffer.append(currentFileName);
                hasField = true;
            }
            if (totalBytes > 0L) {
                if (hasField) {
                    textBuffer.append("  ");
                }
                appendSize(textBuffer, Math.min(shownBytes, totalBytes));
                textBuffer.append('/');
                appendSize(textBuffer, totalBytes);
                hasField = true;
            }
            if (lastSpeedText != null && lastSpeedText.length() > 0) {
                if (hasField) {
                    textBuffer.append("  ");
                }
                textBuffer.append(lastSpeedText);
            }
        }
        if (differs(lastStatusText, textBuffer)) {
            lastStatusText = textBuffer.toString();
            statusView.setText(lastStatusText);
        }
        if (speedView != null) {
            if (verifyPhase) {
                if (speedView.getVisibility() != View.GONE) {
                    speedView.setVisibility(View.GONE);
                }
            } else {
                boolean visible = speedView.getVisibility() == View.VISIBLE;
                if (!visible) {
                    speedView.setVisibility(View.VISIBLE);
                }
                // 刚显示出来的那一帧也要写上，否则会留着上一阶段的旧值
                if (speedChanged || !visible) {
                    speedView.setText(lastSpeedText);
                }
            }
        }
    }

    /**
     * 校验阶段的文字：百分比和字节数都用插值后的显示值，和进度条一起走。
     *
     * <p>「校验完成，等待电脑端确认…」这句话只看真值（本端真的算完了才说），
     * 拿显示值判断会提前一拍说完成。
     */
    private void appendVerifyText(StringBuilder out, long total) {
        if (truthBytes >= total) {
            out.append("校验完成，等待电脑端确认…");
            return;
        }
        long shown = Math.max(0L, Math.min(shownBytes, total));
        out.append("校验中 ").append(shown * 100L / Math.max(1L, total)).append("%  ");
        appendSize(out, shown);
        out.append('/');
        appendSize(out, total);
    }

    /** 新的一次传输：速率的采样窗口、显示用的字节全部归零（否则会接着上一次的数往前跑） */
    private void resetProgressSmoothing() {
        speedSamples.clear();
        lastSampleBytes = 0L;
        lastGrowAt = 0L;
        lastGrowGapMs = 0L;
        smoothedSpeed = 0L;
        speedTarget = 0.0;
        speedShown = 0.0;
        shownBytesF = 0.0;
        shownBytes = 0L;
        truthBytes = 0L;
        truthTotal = 0L;
        verifyPhase = false;
        ledgerPath = "";
        accountedLedgerPath = null;
        lastFrameNanos = 0L;
        lastSampleAt = 0L;
        lastBarValue = Integer.MIN_VALUE;
        lastStatusText = null;
        lastSpeedText = null;
        speedStateStarted = false;
    }

    /**
     * 用按块真值的增量算速率：600ms 滑动窗口平均 + 和上一帧 0.75/0.25 混合。
     *
     * <p>为什么不用 {@code onSpeedInfo}：那是「每秒一次的流量回调」，一秒里一个字节都没有的
     * 收尾阶段（只剩一条蓝牙，0.13MB/s）根本不会触发，界面就常驻在 0B/s。
     * 按块真值的增量算，任何速度下都有数；再靠窗口平均把「块一到就尖峰」摊平。
     *
     * @param sent 这一拍按块统计出来的已发字节（真值）
     * @param now  这一拍的时刻（ms，和 vsync 同一条时间线）
     * @return 结果写在 {@link #smoothedSpeed}：这一拍的「目标速率」，由调用方逐帧补间到屏幕上
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
        // 窗口平均值再和上一帧 0.75/0.25 混合：只削掉残余抖动，不拖慢响应
        smoothedSpeed = smoothedSpeed <= 0 ? instant : (smoothedSpeed * 3 + instant) / 4;
        // 慢通道下 onSpeedInfo 拿不到值，峰值也就记不上，这里补一份
        peakSpeedBytes = Math.max(peakSpeedBytes, smoothedSpeed);
    }

    /**
     * 每帧把显示值往前推一小步（dead reckoning），并让显示速率逐帧补间到目标速率。
     *
     * <p>为什么速率也要补间：目标速率是 100ms 才更新一次的量，直接拿去显示就是
     * 「0 → 21.24MB/s」这种跳变；这里用指数逼近（时间常数 {@link #SPEED_TAU_MS}），
     * 每帧只挪 10%~15%，几帧之后贴住目标，肉眼看到的是数字在爬。
     * 闲置时目标速率会被 {@link #updateSpeed} 归零，显示速率同样是平滑落下去，
     * 不会突然变成 0B/s。
     *
     * <p>为什么字节数要按速率推、而不是直接画真值：真值是按整块记账的（1MiB 一颗），
     * 慢链路上 0.1MB/s 一颗要十秒 —— 直接画真值就是「静止十秒 + 突然跳一格」。
     *
     * <p><b>为什么这样不骗人</b>（三条约束，缺一条就成了骗）：
     * <ul>
     *   <li>上限永远是「真值 + 前瞻余量」，前瞻最多一块、最多 0.25 秒的速率
     *       （见 {@link #limitBytes}）—— 补的是块记账的量化误差，不越过真发出去的数据；</li>
     *   <li>速率归零就停住：目标速率被归零后，显示值不再增加，最多停在
     *       「最后一块的位置 + 当时那点余量」上等真值追上来，绝不自己往 100% 爬；</li>
     *   <li>只增不减：除了换阶段/账本被重置（那时是之前显示错了），进度条绝不往回退。</li>
     * </ul>
     */
    private void advanceDisplayed(long dtMs) {
        // 1) 显示速率逐帧逼近目标速率（指数逼近，和帧率无关）
        double alpha = 1.0 - Math.exp(-dtMs / SPEED_TAU_MS);
        speedShown += (speedTarget - speedShown) * alpha;
        if (Math.abs(speedTarget - speedShown) < 1.0) {
            // 差不到 1B/s 就直接贴合，免得永远在逼近路上（数字末两位一直抖）
            speedShown = speedTarget;
        }
        // 2) 死推算：这一帧按显示速率前进 speed * dt
        if (speedShown > 0.0) {
            shownBytesF += speedShown * dtMs / 1000.0;
        }
        long shown = (long) shownBytesF;
        // 3) 上限：真值 + 前瞻余量
        long limit = limitBytes();
        if (shown > limit) {
            // 夹到上限；但如果上一帧已经画在上限之外（速率刚掉下来），就停在原处等真值追上来，
            // 绝不往回走 —— 进度条往回退比停住更像出了错
            shown = Math.max(shownBytes, limit);
            shownBytesF = shown;
        }
        // 4) 落后太多（中途停顿过、速率被低估）：先拉近，别让界面长期落后于真值
        long lag = truthBytes - shown;
        if (lag > MAX_DISPLAY_LAG_BYTES) {
            shown = truthBytes - MAX_DISPLAY_LAG_BYTES;
            shownBytesF = shown;
        }
        // 5) 单调：显示值只增不减
        if (shown < shownBytes) {
            shown = shownBytes;
            shownBytesF = shown;
        }
        shownBytes = shown;
    }

    /**
     * 显示值允许到达的上限 = 真值 + 前瞻余量。
     *
     * <p>前瞻余量为什么不算骗人：真值是「按整块记账」的 —— 一块 1MiB 只有整块写完才被记进
     * {@code doneBytes}，而这一块里最多已经有 1MiB-1 个字节真的发出去了，只是还没被记账。
     * 所以允许显示值外推「至多 0.25 秒的速率、且不超过一整块」，补的是记账本身的量化误差，
     * 而不是凭空往前画：速率一归零，余量立刻跟着归零。
     *
     * <p>总量已知时还有一道硬闸：真值没到总量，上限一律压在「总量 - 1」。
     * 这样屏幕上不会再出现「600.00MB/600.00MB · 0B/s」这种自相矛盾的画面 ——
     * 数字写满就必须是传输真的结束了。
     */
    private long limitBytes() {
        long lookahead = (long) (speedShown * LOOKAHEAD_MS / 1000.0);
        if (lookahead < 0L) {
            lookahead = 0L;
        } else if (lookahead > LOOKAHEAD_MAX_BYTES) {
            lookahead = LOOKAHEAD_MAX_BYTES;
        }
        long limit = truthBytes + lookahead;
        long total = truthTotal;
        if (total > 0L) {
            long cap = truthBytes >= total ? total : total - 1L;
            if (limit > cap) {
                limit = cap;
            }
        }
        return limit;
    }

    /** 显示字节 → 进度条的值（0~{@link #PROGRESS_MAX}）；真值没到总量时绝不给满 */
    private int progressOf(long shown) {
        long total = truthTotal;
        if (total <= 0L) {
            return 0;
        }
        long value = Math.max(0L, Math.min(shown, total)) * PROGRESS_MAX / total;
        if (truthBytes < total && value >= PROGRESS_MAX) {
            value = PROGRESS_MAX - 1L;
        }
        return (int) Math.min(PROGRESS_MAX, value);
    }

    // ------------------------------------------------------------------
    // 逐帧要用的字符串：全部手写、零对象、和上一次一模一样就不 setText
    // ------------------------------------------------------------------

    /**
     * 手写「12.34MB」（四舍五入到 0.01），口径和 {@code Utils.formatFileSize} 一致。
     *
     * <p>为什么不用 {@code Utils.formatFileSize}：它里面是 {@code String.format}，
     * 每调一次都要新建 Formatter、参数数组，还会按 locale 决定小数点 ——
     * 逐帧调用就是每秒五十几次纯浪费，而且换 locale 时字符串宽度会跳。
     * 这里全是整数运算，一个对象都不分配。
     */
    private static void appendSize(StringBuilder out, long bytes) {
        if (bytes < 0L) {
            bytes = 0L;
        }
        if (bytes < 1024L) {
            out.append(bytes).append('B');
        } else if (bytes < 1024L * 1024L) {
            appendHundredths(out, hundredthsOf(bytes, 1024L));
            out.append("KB");
        } else if (bytes < 1024L * 1024L * 1024L) {
            appendHundredths(out, hundredthsOf(bytes, 1024L * 1024L));
            out.append("MB");
        } else {
            appendHundredths(out, hundredthsOf(bytes, 1024L * 1024L * 1024L));
            out.append("GB");
        }
    }

    /** 手写速率：「12.34MB/s」，口径和 {@code Utils.formatSpeed} 一致（B/KB/MB，两位小数） */
    private static void appendSpeed(StringBuilder out, double bytesPerSecond) {
        // 写成 bps > 0 而不是 bps <= 0：顺便把 NaN 也挡在外面
        if (!(bytesPerSecond > 0.0)) {
            out.append("0B/s");
            return;
        }
        long whole = (long) (bytesPerSecond + 0.5);
        if (whole < 1024L) {
            out.append(whole).append("B/s");
        } else if (whole < 1024L * 1024L) {
            appendHundredths(out, hundredthsOf(whole, 1024L));
            out.append("KB/s");
        } else {
            appendHundredths(out, hundredthsOf(whole, 1024L * 1024L));
            out.append("MB/s");
        }
    }

    /** {@code bytes / unit} 保留两位小数的整数版（乘 100 后四舍五入） */
    private static long hundredthsOf(long bytes, long unit) {
        return (bytes * 100L + unit / 2L) / unit;
    }

    /** 把「百分之一为单位」的整数写成 {@code 12.34} 这种形式 */
    private static void appendHundredths(StringBuilder out, long hundredths) {
        out.append(hundredths / 100L).append('.');
        long fraction = hundredths % 100L;
        if (fraction < 10L) {
            out.append('0');
        }
        out.append(fraction);
    }

    /**
     * 缓冲里的内容和上一次写进控件的字符串是否不同。
     *
     * <p>为什么要比：TextView.setText 会触发 measure/layout/invalidate，
     * 值没变（比如速率停在同一档、暂停中数字不动）还每帧 setText 一次，
     * 就是每秒几十次白白的布局。这里逐字符比一遍，一个对象都不分配。
     */
    private static boolean differs(String previous, StringBuilder buffer) {
        int length = buffer.length();
        if (previous == null || previous.length() != length) {
            return true;
        }
        for (int i = 0; i < length; i++) {
            if (previous.charAt(i) != buffer.charAt(i)) {
                return true;
            }
        }
        return false;
    }


    private void setIndeterminate(boolean indeterminate) {
        handler.post(() -> progressBar.setIndeterminate(indeterminate));
    }

    private void showStatus(String text) {
        handler.post(() -> statusView.setText(text));
    }

    private void postStatus(String text) {
        handler.post(() -> statusView.setText(text));
    }

    private void postHint(String text) {
        handler.post(() -> {
            hintView.setText(text);
            hintView.setVisibility(TextUtils.isEmpty(text) ? View.GONE : View.VISIBLE);
        });
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
                        postStatus(getString(R.string.share_copying_file, Utils.formatFileSize(copied)));
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