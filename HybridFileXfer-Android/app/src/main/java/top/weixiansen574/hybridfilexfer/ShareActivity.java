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
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Queue;
import java.util.concurrent.ConcurrentLinkedQueue;

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
 * 分享/打开方式里的"发送到电脑"。
 * <p>
 * 收下文件（content:// 或 路径），确保后台服务已经和电脑连上，然后把文件直接
 * 发送到电脑接收器设置的默认文件夹。
 */
public class ShareActivity extends AppCompatActivity {

    private static final String TAG = "ShareActivity";
    private static final long CONNECT_TIMEOUT_MS = 30_000L;

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

    private long totalBytes;
    private long completedBytes;
    private long currentGroupTotal;
    private volatile String currentFileName = "";
    private volatile String speedText = "";
    private volatile long peakSpeedBytes;
    private final List<String> sentFileNames = new ArrayList<>();
    private int sentFileCount;
    private final java.util.Map<String, Double> channelSpeeds = new java.util.LinkedHashMap<>();
    private long startTime;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        requestWindowFeature(android.view.Window.FEATURE_NO_TITLE);
        super.onCreate(savedInstanceState);
        if (getSupportActionBar() != null) {
            getSupportActionBar().hide();
        }
        setContentView(R.layout.activity_share);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP) {
            setFinishOnTouchOutside(false);
        }

        statusView = findViewById(R.id.share_status);
        hintView = findViewById(R.id.share_hint);
        speedView = findViewById(R.id.share_speed);
        android.util.DisplayMetrics metrics = getResources().getDisplayMetrics();
        getWindow().setLayout(
                (int) (metrics.widthPixels * 0.90f),
                (int) (metrics.heightPixels * 0.45f));
        progressBar = findViewById(R.id.share_progress);
        actionButton = findViewById(R.id.share_action);
        actionButton.setText(R.string.cancel);
        actionButton.setOnClickListener(v -> {
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
            server = ServerService.getServer();
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
                server = ServerService.getServer();
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
        if (startTime == 0) {
            startTime = System.currentTimeMillis();
        }
        setIndeterminate(false);
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
            setIndeterminate(false);
            progressBar.setProgress(100);
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
            setIndeterminate(false);
            progressBar.setProgress(0);
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
            currentFileName = new File(path).getName();
            long sent = completedBytes + Math.min(Math.max(targetSize, 0), currentGroupTotal);
            updateProgress(sent);
            showStatus(getString(R.string.share_sending, buildProgressText()));
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
            peakSpeedBytes = Math.max(peakSpeedBytes, bytes);
            speedText = Utils.formatSpeed(bytes);
            String speed = speedText;
            handler.post(() -> {
                if (speedView != null && !TextUtils.isEmpty(speed)) {
                    speedView.setText(speed);
                    speedView.setVisibility(View.VISIBLE);
                }
            });
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
            completedBytes += currentGroupTotal;
            updateProgress(completedBytes);
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

    private String buildProgressText() {
        StringBuilder builder = new StringBuilder();
        if (!TextUtils.isEmpty(currentFileName)) {
            builder.append(currentFileName);
            builder.append("  ");
        }
        if (totalBytes > 0) {
            long sent = Math.min(completedBytes + currentGroupTotal, totalBytes);
            builder.append(Utils.formatFileSize(sent))
                    .append('/')
                    .append(Utils.formatFileSize(totalBytes));
        }
        if (!TextUtils.isEmpty(speedText)) {
            builder.append("  ").append(speedText);
        }
        return builder.toString().trim();
    }

    private void updateProgress(long sent) {
        if (totalBytes <= 0) {
            setIndeterminate(true);
            return;
        }
        int percent = (int) Math.min(100L, sent * 100L / totalBytes);
        handler.post(() -> {
            progressBar.setIndeterminate(false);
            progressBar.setProgress(percent);
        });
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