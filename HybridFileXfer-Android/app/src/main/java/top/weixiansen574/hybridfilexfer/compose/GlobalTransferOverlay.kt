package top.weixiansen574.hybridfilexfer.compose

import android.content.Context
import android.graphics.PixelFormat
import android.provider.Settings
import android.view.WindowManager
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.platform.ComposeView
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.LifecycleRegistry
import androidx.lifecycle.ViewModelStore
import androidx.lifecycle.ViewModelStoreOwner
import androidx.lifecycle.setViewTreeLifecycleOwner
import androidx.lifecycle.setViewTreeViewModelStoreOwner
import top.weixiansen574.hybridfilexfer.Config
import top.weixiansen574.hybridfilexfer.ServerService
import top.yukonga.miuix.kmp.theme.MiuixTheme
import top.yukonga.miuix.kmp.theme.ThemeController

/**
 * 应用外的全局进度窗口：用 ComposeView 渲染和应用内完全一样的接收/发送弹窗。
 * 由 GlobalProgress 的 tick 每 250ms 调一次 refresh()。
 */
object GlobalTransferOverlay {
    private val state = mutableStateOf(UiState())
    private val sending = mutableStateOf(false)
    private var view: ComposeView? = null
    private var windowManager: WindowManager? = null
    private var owner: Owner? = null
    private var showing = false

    /** 双窗口发送时由界面把合成的进度塞进来，完成后传 null */
    @Volatile
    private var sendSnapshot: UiState? = null

    @Volatile
    private var sendAt = 0L

    internal fun reportSend(snapshot: UiState?) {
        sendSnapshot = snapshot
        sendAt = System.currentTimeMillis()
    }

    fun refresh(context: Context) {
        val recv = readProgressState(UiState())
        val send = sendSnapshot?.takeIf {
            it.xferActive && System.currentTimeMillis() - sendAt < 2500
        }
        val active = recv.xferActive || send != null
        if (!active || ServerService.appVisible || !Settings.canDrawOverlays(context)) {
            hide()
            return
        }
        val snapshot = if (recv.xferActive) recv else send ?: return
        state.value = snapshot
        sending.value = !recv.xferActive
        show(context)
    }

    fun hide() {
        if (!showing) return
        showing = false
        val v = view
        val w = windowManager
        view = null
        windowManager = null
        owner?.stop()
        owner = null
        try {
            if (v != null && w != null) {
                w.removeView(v)
            }
        } catch (_: Throwable) {
        }
    }

    private fun show(context: Context) {
        if (showing) return
        val manager = context.getSystemService(Context.WINDOW_SERVICE) as? WindowManager ?: return
        val lifecycleOwner = Owner()
        val compose = ComposeView(context).apply {
            setViewTreeLifecycleOwner(lifecycleOwner)
            setViewTreeViewModelStoreOwner(object : ViewModelStoreOwner {
                override val viewModelStore = ViewModelStore()
            })
            val controller = ThemeController(
                colorSchemeMode = colorSchemeModeOf(Config.getInstance(context).themeMode),
            )
            setContent {
                MiuixTheme(controller = controller) {
                    ReceiveProgressPanel(
                        state = state.value,
                        onDismiss = { hide() },
                        sending = sending.value,
                        bare = true,
                    )
                }
            }
        }
        val params = WindowManager.LayoutParams(
            WindowManager.LayoutParams.WRAP_CONTENT,
            WindowManager.LayoutParams.WRAP_CONTENT,
            WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY,
            WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or
                WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL or
                WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN,
            PixelFormat.TRANSLUCENT,
        ).apply {
            gravity = android.view.Gravity.CENTER
        }
        try {
            manager.addView(compose, params)
            lifecycleOwner.start()
            view = compose
            windowManager = manager
            owner = lifecycleOwner
            showing = true
        } catch (_: Throwable) {
            // 没有权限 / 系统拒绝悬浮窗时静默退回通知栏
            try {
                lifecycleOwner.stop()
            } catch (_: Throwable) {
            }
        }
    }

    private class Owner : LifecycleOwner {
        private val registry = LifecycleRegistry(this)
        override val lifecycle: Lifecycle get() = registry

        fun start() {
            try {
                registry.currentState = Lifecycle.State.RESUMED
            } catch (_: Throwable) {
            }
        }

        fun stop() {
            try {
                registry.currentState = Lifecycle.State.DESTROYED
            } catch (_: Throwable) {
            }
        }
    }
}
