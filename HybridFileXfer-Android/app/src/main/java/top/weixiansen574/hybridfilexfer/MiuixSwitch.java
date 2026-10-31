package top.weixiansen574.hybridfilexfer;

import android.animation.ValueAnimator;
import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.graphics.RectF;
import android.util.AttributeSet;
import android.view.MotionEvent;
import android.view.View;
import android.view.animation.DecelerateInterpolator;

import androidx.core.content.ContextCompat;
import androidx.core.graphics.ColorUtils;

/**
 * 系统/Miuix 风格的开关：一个胶囊轨道 + 白色圆形滑块，带过渡动画。
 * <p>
 * 自己画是为了不受不同系统版本 SwitchCompat 样式影响，保证各机型一致。
 */
public class MiuixSwitch extends View {

    public interface OnCheckedChangeListener {
        void onCheckedChanged(MiuixSwitch view, boolean isChecked);
    }

    private final Paint trackPaint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint thumbPaint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final RectF trackRect = new RectF();

    private final int colorOff;
    private final int colorOn;
    private final int colorThumb;
    private final float trackHeight;
    private final float thumbRadius;
    private final float inset;
    private final float thumbPadding;

    private boolean checked;
    private float progress;
    private OnCheckedChangeListener listener;
    private ValueAnimator animator;

    public MiuixSwitch(Context context) {
        this(context, null);
    }

    public MiuixSwitch(Context context, AttributeSet attrs) {
        this(context, attrs, 0);
    }

    public MiuixSwitch(Context context, AttributeSet attrs, int defStyleAttr) {
        super(context, attrs, defStyleAttr);
        float density = getResources().getDisplayMetrics().density;
        trackHeight = 30f * density;
        inset = 1f * density;
        thumbRadius = 13f * density;
        thumbPadding = 2f * density;
        colorOff = ContextCompat.getColor(context, R.color.switch_track_off);
        colorOn = ContextCompat.getColor(context, R.color.accent);
        colorThumb = ContextCompat.getColor(context, R.color.switch_thumb);
        trackPaint.setStyle(Paint.Style.FILL);
        thumbPaint.setStyle(Paint.Style.FILL);
        setClickable(true);
        setFocusable(true);
    }

    @Override
    protected void onMeasure(int widthMeasureSpec, int heightMeasureSpec) {
        float density = getResources().getDisplayMetrics().density;
        int width = (int) (52f * density);
        int height = (int) (trackHeight + inset * 2);
        setMeasuredDimension(resolveSize(width, widthMeasureSpec), resolveSize(height, heightMeasureSpec));
    }

    @Override
    protected void onDraw(Canvas canvas) {
        super.onDraw(canvas);
        float width = getWidth();
        float height = getHeight();
        trackRect.set(inset, inset, width - inset, height - inset);
        int trackColor = ColorUtils.blendARGB(colorOff, colorOn, progress);
        trackPaint.setColor(trackColor);
        float radius = trackRect.height() / 2f;
        canvas.drawRoundRect(trackRect, radius, radius, trackPaint);

        float start = trackRect.left + thumbPadding + thumbRadius;
        float end = trackRect.right - thumbPadding - thumbRadius;
        float cx = start + (end - start) * progress;
        float cy = trackRect.centerY();
        // 阴影（浅灰的一圈），让滑块看起来有立体感
        thumbPaint.setColor(ColorUtils.setAlphaComponent(0xFF000000, 28));
        canvas.drawCircle(cx + 0.5f, cy + 1.2f, thumbRadius, thumbPaint);
        thumbPaint.setColor(colorThumb);
        canvas.drawCircle(cx, cy, thumbRadius, thumbPaint);
    }

    @Override
    public boolean onTouchEvent(MotionEvent event) {
        if (!isEnabled()) {
            return false;
        }
        switch (event.getAction()) {
            case MotionEvent.ACTION_DOWN:
                setAlpha(0.75f);
                return true;
            case MotionEvent.ACTION_CANCEL:
                setAlpha(1f);
                return true;
            case MotionEvent.ACTION_UP:
                setAlpha(1f);
                if (isPressed() || true) {
                    toggle();
                }
                return true;
            default:
                return super.onTouchEvent(event);
        }
    }

    public void toggle() {
        setCheckedInternal(!checked, true, true);
    }

    @Override
    public void setEnabled(boolean enabled) {
        super.setEnabled(enabled);
        setAlpha(enabled ? 1f : 0.45f);
    }

    public boolean isChecked() {
        return checked;
    }

    public void setChecked(boolean value) {
        setCheckedInternal(value, true, false);
    }

    public void setOnCheckedChangeListener(OnCheckedChangeListener listener) {
        this.listener = listener;
    }

    private void setCheckedInternal(boolean value, boolean animate, boolean fromUser) {
        if (checked == value) {
            return;
        }
        checked = value;
        if (animator != null) {
            animator.cancel();
        }
        if (animate && isAttachedToWindow()) {
            animator = ValueAnimator.ofFloat(progress, value ? 1f : 0f);
            animator.setDuration(160);
            animator.setInterpolator(new DecelerateInterpolator());
            animator.addUpdateListener(a -> {
                progress = (float) a.getAnimatedValue();
                invalidate();
            });
            animator.start();
        } else {
            progress = value ? 1f : 0f;
            invalidate();
        }
        if (listener != null) {
            listener.onCheckedChanged(this, checked);
        }
    }

    @Override
    protected void onDetachedFromWindow() {
        if (animator != null) {
            animator.cancel();
            animator = null;
        }
        super.onDetachedFromWindow();
    }
}
