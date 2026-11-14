package top.weixiansen574.hybridfilexfer.compose

import android.content.Context
import android.util.Log
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
import top.yukonga.miuix.kmp.theme.MiuixTheme
import top.yukonga.miuix.kmp.theme.ThemeController

/**
 * 「分享发送」页的进度面板：把和应用内弹窗、双窗口、应用外悬浮窗<b>完全同一个</b>
 * [ReceiveProgressPanel] 塞进 ShareActivity 那条 Java 流程里。
 *
 * 用法（ShareActivity）：
 * 1. onCreate 里 `((ViewGroup) findViewById(R.id.share_panel)).addView(ShareProgress.host(this))`；
 * 2. 每 100ms 采样到真值后调 [update]，只喂真值，补间由面板自己做；
 * 3. onDestroy 里调 [release]（owner 变 DESTROYED，顺手把状态清掉）。
 *
 * 这个 ComposeView 挂在 Activity 的普通 ViewGroup 里，但 Compose 需要三个 owner
 * （Lifecycle / ViewModelStore / SavedStateRegistry）。Activity 的 DecorView 上虽然有一套，
 * 这里按 [GlobalTransferOverlay] 的做法自己挂一套，行为和应用外悬浮窗完全一致
 * （少挂一个就会在 attach 时抛 IllegalStateException 把进程带崩）。
 */
internal object ShareProgress {
    private const val TAG = "HFXSharePanel"

    /** 面板现在看到的状态：Compose 侧读它的 value，[update] 写它的 value */
    private val state = mutableStateOf(UiState())

    private var owner: Owner? = null

    /**
     * 造一个渲染进度面板的 ComposeView，由调用方 addView 到布局里的占位容器。
     *
     * 传进来的 context 一般是 Activity（自带主题）；服务里的 Context 才需要
     * ContextThemeWrapper 包一层，那是 [GlobalTransferOverlay] 的情况。
     */
    @JvmStatic
    fun host(context: Context): ComposeView {
        val lifecycleOwner = Owner()
        // 必须在 ComposeView attach 之前就进到 CREATED/RESUMED：
        // Compose 一 attach 就会去 consumeRestoredStateForKey，owner 还没起来会直接抛
        lifecycleOwner.start()
        val controller = ThemeController(
            colorSchemeMode = colorSchemeModeOf(Config.getInstance(context).themeMode),
        )
        val compose = ComposeView(context).apply {
            setViewTreeLifecycleOwner(lifecycleOwner)
            setViewTreeViewModelStoreOwner(lifecycleOwner)
            setViewTreeSavedStateRegistryOwner(lifecycleOwner)
            setContent {
                MiuixTheme(controller = controller) {
                    // sending=true → 标题「正在发送文件」；bare=true → 不铺遮罩；
                    // showFooter=false → 底部的暂停/取消/关闭整块不画（按钮在 activity_share.xml 里）
                    ReceiveProgressPanel(
                        state = state.value,
                        sending = true,
                        bare = true,
                        showFooter = false,
                    )
                }
            }
        }
        owner = lifecycleOwner
        return compose
    }

    /** 把最新的 UiState 推进面板（快照写入，任何线程调都安全；实际调用都在主线程） */
    @JvmStatic
    fun update(next: UiState) {
        state.value = next
    }

    /**
     * Java 侧专用入口。
     *
     * [UiState] 有四十多个字段，从 Java 直接构造太容易错位，这里只暴露进度相关的那几个，
     * 剩下的（连接列表、配置项……）保持默认值 —— 面板只读这几个字段。
     */
    @JvmStatic
    fun update(
        active: Boolean,
        name: String,
        done: Long,
        total: Long,
        bytes: Long,
        segments: List<SegInfo>,
        channels: List<ChannelStat>,
        note: String,
        label: String,
    ) {
        state.value = UiState(
            xferActive = active,
            xferName = name,
            xferDone = done,
            xferTotal = total,
            // 面板用累计字节的增量算合计速率，所以它必须是只增不减的真值
            xferBytes = bytes,
            xferSegments = segments,
            xferChannels = channels,
            xferNote = note,
            xferCount = 1,
            xferLabel = label,
            // 分享页没有「暂停/取消」这条面板内的路子（onPause/onCancel 都没传），
            // xferId 保持 -1，面板不会去碰 ServerService 的传输控制
            xferId = -1,
            xferPaused = false,
        )
    }

    /** Activity 销毁：owner 走到 DESTROYED，状态清干净（下次进来重新开始） */
    @JvmStatic
    fun release() {
        val current = owner
        owner = null
        current?.stop()
        state.value = UiState()
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
                Log.w(TAG, "owner attach failed", e)
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
                Log.w(TAG, "owner start failed", e)
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
