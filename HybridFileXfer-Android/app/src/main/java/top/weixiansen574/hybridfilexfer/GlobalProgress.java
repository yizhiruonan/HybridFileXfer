package top.weixiansen574.hybridfilexfer;

import android.content.Intent;
import android.graphics.PixelFormat;
import android.graphics.drawable.ClipDrawable;
import android.graphics.drawable.GradientDrawable;
import android.graphics.drawable.LayerDrawable;
import android.os.Build;
import android.os.Handler;
import android.provider.Settings;
import android.text.TextUtils;
import android.view.Gravity;
import android.view.View;
import android.view.WindowManager;
import android.widget.LinearLayout;
import android.widget.ProgressBar;
import android.widget.TextView;

import top.weixiansen574.hybridfilexfer.droidcore.HFXServer;

/**
 * 全局接收进度：系统悬浮窗（在别的应用上面也看得到）+ 通知栏进度。
 *
 * <p>悬浮窗需要"显示在其他应用上层"权限；没有权限就只更新通知栏，不影响接收。
 */
final class GlobalProgress {
    private final ServerService service;
    private final Handler handler;
    private WindowManager windowManager;
    private View root;
    private TextView titleView;
    private TextView fileView;
    private TextView speedView;
    private ProgressBar bar;
    private TextView pauseButton;
    private int connectionId = -1;
    private long lastDone;
    private long lastAt;
    private double speedValue;

    /**
     * 轮询间隔：应用外的进度窗口要把"和应用内弹窗一样的动画"画出来，
     * 就必须和应用内的刷新节奏一致 —— 主页的接收进度条是 50ms 读一次按块真值。
     * 以前这里是 250ms，分段和数字都是一跳一跳地走（200ms 的 tween 补不满 250ms 的窟窿）。
     * 通知栏那条路径自己有 1 秒一次的门闩，不受影响。
     */
    private static final long TICK_MS = 50;

    private final Runnable ticker = new Runnable() {
        @Override
        public void run() {
            try {
                tick();
            } catch (Throwable ignored) {
            }
            handler.postDelayed(this, TICK_MS);
        }
    };

    GlobalProgress(ServerService service, Handler handler) {
        this.service = service;
        this.handler = handler;
    }

    void start() {
        handler.post(ticker);
    }

    void destroy() {
        handler.removeCallbacks(ticker);
        hide();
    }

    private Boolean lastShow;

    private void tick() {
        // 应用外：用 ComposeView 渲染和应用内完全一样的接收/发送弹窗
        top.weixiansen574.hybridfilexfer.compose.GlobalTransferOverlay.INSTANCE.refresh(service);
        // 通知栏进度：发送和接收都跟（以前只跟接收，发送方向在通知栏上完全看不到进度）。
        // 这里每 50ms 调一次，服务内部自己限速成 500ms 一帧；传输结束/停止后它会换回常驻文案。
        service.refreshProgressNotification();
    }

    private boolean ensureView() {
        if (root != null) {
            return true;
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M && !Settings.canDrawOverlays(service)) {
            return false;
        }
        try {
            float dp = service.getResources().getDisplayMetrics().density;
            LinearLayout card = new LinearLayout(service);
            card.setOrientation(LinearLayout.VERTICAL);
            GradientDrawable bg = new GradientDrawable();
            bg.setColor(0xF2FFFFFF);
            bg.setCornerRadius(18 * dp);
            card.setBackground(bg);
            card.setPadding((int) (16 * dp), (int) (12 * dp), (int) (16 * dp), (int) (10 * dp));

            titleView = text("正在接收文件", 14, 0xFF1A1A1A, true);
            card.addView(titleView, new LinearLayout.LayoutParams(
                    LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT));

            fileView = text("", 12, 0xFF444444, false);
            fileView.setSingleLine(true);
            fileView.setEllipsize(TextUtils.TruncateAt.MIDDLE);
            LinearLayout.LayoutParams fileParams = new LinearLayout.LayoutParams(
                    LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT);
            fileParams.topMargin = (int) (2 * dp);
            card.addView(fileView, fileParams);

            bar = new ProgressBar(service, null, android.R.attr.progressBarStyleHorizontal);
            bar.setMax(100);
            bar.setProgressDrawable(progressDrawable(dp));
            LinearLayout.LayoutParams barParams = new LinearLayout.LayoutParams(
                    LinearLayout.LayoutParams.MATCH_PARENT, (int) (8 * dp));
            barParams.topMargin = (int) (8 * dp);
            card.addView(bar, barParams);

            LinearLayout footer = new LinearLayout(service);
            footer.setOrientation(LinearLayout.HORIZONTAL);
            footer.setGravity(Gravity.CENTER_VERTICAL);
            speedView = text("", 11, 0xFF666666, false);
            footer.addView(speedView, new LinearLayout.LayoutParams(
                    0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f));
            pauseButton = text("暂停", 12, 0xFF2F6BFF, true);
            pauseButton.setPadding((int) (10 * dp), (int) (6 * dp), (int) (10 * dp), (int) (6 * dp));
            pauseButton.setOnClickListener(v -> {
                if (connectionId < 0) {
                    return;
                }
                if (ServerService.isTransferPaused(connectionId)) {
                    ServerService.resumeTransfer(connectionId);
                    pauseButton.setText("暂停");
                } else {
                    ServerService.pauseTransfer(connectionId);
                    pauseButton.setText("继续");
                }
            });
            footer.addView(pauseButton);
            TextView cancel = text("取消", 12, 0xFFD03030, true);
            cancel.setPadding((int) (10 * dp), (int) (6 * dp), (int) (4 * dp), (int) (6 * dp));
            cancel.setOnClickListener(v -> {
                if (connectionId >= 0) {
                    ServerService.cancelTransfer(connectionId);
                }
            });
            footer.addView(cancel);
            LinearLayout.LayoutParams footerParams = new LinearLayout.LayoutParams(
                    LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT);
            footerParams.topMargin = (int) (4 * dp);
            card.addView(footer, footerParams);

            card.setOnClickListener(v -> {
                Intent open = new Intent(service, top.weixiansen574.hybridfilexfer.compose.MiuixMainActivity.class);
                open.setFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TOP);
                service.startActivity(open);
            });

            WindowManager.LayoutParams params = new WindowManager.LayoutParams(
                    WindowManager.LayoutParams.MATCH_PARENT,
                    WindowManager.LayoutParams.WRAP_CONTENT,
                    Build.VERSION.SDK_INT >= Build.VERSION_CODES.O
                            ? WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY
                            : WindowManager.LayoutParams.TYPE_PHONE,
                    WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE
                            | WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
                    PixelFormat.TRANSLUCENT);
            params.gravity = Gravity.TOP;
            params.y = (int) (32 * dp);
            params.horizontalMargin = 0.05f;
            windowManager = (WindowManager) service.getSystemService(ServerService.WINDOW_SERVICE);
            windowManager.addView(card, params);
            root = card;
            lastDone = 0;
            lastAt = 0;
            speedValue = 0;
            return true;
        } catch (Throwable e) {
            root = null;
            ServerService.debugFile("overlay add failed: " + e);
            return false;
        }
    }

    private void hide() {
        if (root != null && windowManager != null) {
            try {
                windowManager.removeView(root);
            } catch (Throwable ignored) {
            }
        }
        root = null;
        windowManager = null;
    }

    private TextView text(String value, float size, int color, boolean bold) {
        TextView view = new TextView(service);
        view.setText(value);
        view.setTextSize(size);
        view.setTextColor(color);
        if (bold) {
            view.setTypeface(android.graphics.Typeface.DEFAULT_BOLD);
        }
        return view;
    }

    private android.graphics.drawable.Drawable progressDrawable(float dp) {
        GradientDrawable track = new GradientDrawable();
        track.setColor(0x22000000);
        track.setCornerRadius(4 * dp);
        GradientDrawable fill = new GradientDrawable();
        fill.setColor(0xFF2F6BFF);
        fill.setCornerRadius(4 * dp);
        ClipDrawable clip = new ClipDrawable(fill, Gravity.START, ClipDrawable.HORIZONTAL);
        LayerDrawable layers = new LayerDrawable(new android.graphics.drawable.Drawable[]{track, clip});
        layers.setId(0, android.R.id.background);
        layers.setId(1, android.R.id.progress);
        return layers;
    }

    static String formatSize(long bytes) {
        if (bytes <= 0) {
            return "0 B";
        }
        double kb = 1024.0;
        double b = bytes;
        if (b >= kb * kb * kb) {
            return String.format(java.util.Locale.US, "%.2f GB", b / (kb * kb * kb));
        }
        if (b >= kb * kb) {
            return String.format(java.util.Locale.US, "%.1f MB", b / (kb * kb));
        }
        if (b >= kb) {
            return String.format(java.util.Locale.US, "%.0f KB", b / kb);
        }
        return bytes + " B";
    }

    static String formatSpeed(double bytesPerSecond) {
        if (bytesPerSecond <= 0) {
            return "0 B/s";
        }
        return formatSize((long) bytesPerSecond) + "/s";
    }
}
