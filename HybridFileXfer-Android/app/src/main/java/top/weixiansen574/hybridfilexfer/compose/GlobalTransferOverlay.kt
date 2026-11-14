package top.weixiansen574.hybridfilexfer.compose

import android.content.Context
import android.graphics.PixelFormat
import android.provider.Settings
import android.view.ContextThemeWrapper
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
import androidx.savedstate.SavedStateRegistry
import androidx.savedstate.SavedStateRegistryController
import androidx.savedstate.SavedStateRegistryOwner
import androidx.savedstate.setViewTreeSavedStateRegistryOwner
import top.weixiansen574.hybridfilexfer.Config
import top.weixiansen574.hybridfilexfer.ServerService
import top.yukonga.miuix.kmp.theme.MiuixTheme
import top.yukonga.miuix.kmp.theme.ThemeController

/**
 * 应用外的全局进度窗口：用 ComposeView 渲染和应用内完全一样的接收/发送弹窗
 * （[ReceiveProgressPanel]，跟主页"正在接收文件"是同一个组件）。
 * 由 GlobalProgress 的 tick 每 250ms 调一次 refresh()。
 *
 * 悬浮窗里的 ComposeView 不在 Activity 树里，必须自己挂齐三个 owner
 * （Lifecycle / ViewModelStore / SavedStateRegistry），少一个就会在
 * attach 到窗口时抛 IllegalStateException 把整个进程带崩。
 */
object GlobalTransferOverlay {
    private val state = mutableStateOf(UiState())
    private val sending = mutableStateOf(false)
    private var view: ComposeView? = null
    private var windowManager: WindowManager? = null
    private var owner: Owner? = null
    private var showing = false

    /** 双窗口里传输时由界面把进度塞进来（发送也一样），完成后传 null */
    @Volatile
    private var sendSnapshot: UiState? = null

    /** 那份快照是"发送中"还是"接收中"：双窗口 ← 复制（拉取）时手机是接收方，别标成"正在发送" */
    @Volatile
    private var sendIsSending = true

    @Volatile
    private var sendAt = 0L

    /** 上一次尝试失败的时间：失败后隔一会儿再试，别每 250ms 崩一次 */
    private var lastFailure = 0L

    internal fun reportSend(snapshot: UiState?, sending: Boolean = true) {
        sendSnapshot = snapshot
        sendIsSending = sending
        sendAt = System.currentTimeMillis()
    }

    fun refresh(context: Context) {
        // 应用在前台时进度由应用内的弹窗显示，悬浮窗没什么可画的：
        // 先判掉，别每 50ms 去读一遍全量进度状态（这条路以前是 250ms 一次，现在跟着主页的 50ms 走）
        if (ServerService.appVisible) {
            hide()
            return
        }
        val recv = try {
            readProgressState(UiState())
        } catch (_: Throwable) {
            UiState()
        }
        val send = sendSnapshot?.takeIf {
            it.xferActive && System.currentTimeMillis() - sendAt < 2500
        }
        // 电脑推送（后台接收）优先级最高：那份快照本来就是按块统计的真值
        val active = recv.xferActive || send != null
        if (!active || !canOverlay(context)) {
            hide()
            return
        }
        val snapshot = if (recv.xferActive) recv else send ?: return
        state.value = snapshot
        // 双窗口里可能是"← 复制"（手机在收），别再固定写成发送
        sending.value = if (recv.xferActive) false else sendIsSending
        show(context)
    }

    /** 悬浮窗权限查询是会走系统服务的，别每 50ms 问一次 */
    private var overlayAllowed = true
    private var overlayCheckedAt = 0L

    private fun canOverlay(context: Context): Boolean {
        val now = System.currentTimeMillis()
        if (now - overlayCheckedAt < 2000) {
            return overlayAllowed
        }
        overlayCheckedAt = now
        overlayAllowed = try {
            Settings.canDrawOverlays(context)
        } catch (_: Throwable) {
            false
        }
        return overlayAllowed
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
        if (System.currentTimeMillis() - lastFailure < 3000) {
            return
        }
        val manager = context.getSystemService(Context.WINDOW_SERVICE) as? WindowManager ?: return
        val lifecycleOwner = Owner()
        // 必须在 ComposeView 挂到窗口之前就进到 CREATED/RESUMED：
        // Compose 一 attach 就会去 consumeRestoredStateForKey，owner 还没起来会直接抛
        // IllegalStateException 把进程带崩
        lifecycleOwner.start()
        // 服务里的 Context 没有 Activity 主题，Compose 里的 material/miuix 控件需要一份
        val themed = ContextThemeWrapper(context, android.R.style.Theme_Material_Light_NoActionBar)
        val compose = ComposeView(themed).apply {
            setViewTreeLifecycleOwner(lifecycleOwner)
            setViewTreeViewModelStoreOwner(lifecycleOwner)
            setViewTreeSavedStateRegistryOwner(lifecycleOwner)
            val controller = ThemeController(
                colorSchemeMode = colorSchemeModeOf(Config.getInstance(themed).themeMode),
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
            view = compose
            windowManager = manager
            owner = lifecycleOwner
            showing = true
        } catch (e: Throwable) {
            // 没有权限 / 系统拒绝悬浮窗时静默退回通知栏
            lastFailure = System.currentTimeMillis()
            android.util.Log.w("HFXOverlay", "overlay add failed", e)
            try {
                lifecycleOwner.stop()
            } catch (_: Throwable) {
            }
        }
    }

    private class Owner : LifecycleOwner, ViewModelStoreOwner, SavedStateRegistryOwner {
        private val registry = LifecycleRegistry(this)
        private val store = ViewModelStore()
        private val savedState = SavedStateRegistryController.create(this)
        override val lifecycle: Lifecycle get() = registry
        override val viewModelStore: ViewModelStore get() = store
        override val savedStateRegistry: SavedStateRegistry get() = savedState.savedStateRegistry

        init {
            // performAttach 必须在 owner 还是 INITIALIZED 的时候叫（savedstate 的限制：
            // "Restarter must be created only during owner's initialization stage"），
            // 之后再 performRestore，Compose 里的 rememberSaveable 才能正常读
            try {
                savedState.performAttach()
            } catch (e: Throwable) {
                android.util.Log.w("HFXOverlay", "owner attach failed", e)
            }
        }

        fun start() {
            try {
                // 顺序不能反：先 performRestore 再切到 CREATED，
                // 否则切状态时 savedstate 的 Recreator 会去 consume，
                // 那时 isRestored 还是 false，直接抛 IllegalStateException
                savedState.performRestore(null)
                registry.currentState = Lifecycle.State.CREATED
                registry.currentState = Lifecycle.State.RESUMED
            } catch (e: Throwable) {
                android.util.Log.w("HFXOverlay", "owner start failed", e)
            }
        }

        fun stop() {
            try {
                registry.currentState = Lifecycle.State.DESTROYED
                store.clear()
            } catch (_: Throwable) {
            }
        }
    }
}
