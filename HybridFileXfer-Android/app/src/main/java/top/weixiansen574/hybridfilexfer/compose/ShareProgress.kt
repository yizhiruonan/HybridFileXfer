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
 * 面板上那几颗按钮（暂停发送 / 继续 / 取消发送）点下去要做什么。
 *
 * 由 [ShareProgress.setActions] 一次性装进来；[onPause] / [onResume] 传 null 时
 * 面板就不画暂停那一颗（和双窗口弹窗传 null 时的表现完全一样）。
 */
class ShareActions(
    val onDismiss: Runnable,
    val onCancel: Runnable,
    val onPause: Runnable?,
    val onResume: Runnable?,
)

/**
 * 「分享发送」页的进度面板：把双窗口弹窗 / 应用外悬浮窗用的那个 [ReceiveProgressPanel]
 * <b>原样</b>塞进 ShareActivity 这条 Java 流程里，参数和 TransferScreen.kt 里
 * `if (xferPopup) { ReceiveProgressPanel(...) }` 那段一一对应：
 *
 * | 参数 | 双窗口 | 这里 |
 * | --- | --- | --- |
 * | state | UiState(...) | 同一个 UiState，字段含义一致（见下面的 update） |
 * | onDismiss | { xferPopup = false } | ShareActivity 的 dismissPanel（关闭窗口） |
 * | sending | xferPopupSending | true（这个页面永远是「发」） |
 * | onPause | server.pauseTransfer() | server.pauseTransfer() |
 * | onResume | server.resumeTransfer() | server.resumeTransfer() |
 * | onCancel | server.abortTransfer() + Toast | ShareActivity 原来的取消逻辑 |
 * | bare | 不传（= false） | <b>true</b>（唯一的不同，原因见下） |
 * | showFooter | 不传（= true） | 不传（= true） |
 *
 * 为什么 bare 必须是 true：`ReceiveProgressPanel` 的 bare=false 分支根本不画遮罩框，
 * 它是一整个 `Box(Modifier.fillMaxSize().background(黑 35%))` —— 双窗口那边它铺在
 * 整页的 Composable 里所以正好是全屏压暗；而在 ShareActivity 这种 WRAP_CONTENT 的窗口里，
 * fillMaxSize 会把这个小窗口直接撑成满屏 + 一层 35% 黑，也就是「一整块黑」。
 * 所以这里走 bare=true：只画那张卡片（卡片和双窗口那边是同一个 composable，像素级一样），
 * 窗口自己按卡片大小包住它，不铺任何遮罩。
 *
 * 用法（ShareActivity）：
 * 1. onCreate 里 `((ViewGroup) findViewById(R.id.share_panel)).addView(ShareProgress.host(this))`；
 * 2. 同一处调 [setActions] 把按钮接到自己的取消 / 暂停逻辑上；
 * 3. 每 100ms 采样到真值后调 [update]，只喂真值，补间由面板自己做；
 * 4. onDestroy 里调 [release]（owner 变 DESTROYED，顺手把状态清掉）。
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

    /** 面板按钮的落点，[setActions] 之前是 null（点了也不会有反应，不会崩） */
    private val actions = mutableStateOf<ShareActions?>(null)

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
                    // 和 TransferScreen.kt 里双窗口那段一比一：
                    // sending=true / showFooter 不传（默认 true，底部按钮由面板自己画）/
                    // bare=true（唯一的不同，见类注释：false 会把小窗口撑成满屏黑）
                    val current = actions.value
                    ReceiveProgressPanel(
                        state = state.value,
                        onDismiss = { current?.onDismiss?.run() },
                        sending = true,
                        onPause = { current?.onPause?.run() },
                        onResume = { current?.onResume?.run() },
                        onCancel = { current?.onCancel?.run() },
                        bare = true,
                    )
                }
            }
        }
        owner = lifecycleOwner
        return compose
    }

    /** 把面板那几颗按钮接到 Activity 自己的逻辑上（在 [host] 之后随时可调） */
    @JvmStatic
    fun setActions(next: ShareActions) {
        actions.value = next
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
     *
     * @param active true = 传输中：面板底部画「暂停发送」（暂停后是「继续」+「取消发送」）
     * @param paused 电脑端 / HFXServer 那边的暂停闸门现在是关着的（和双窗口读 isTransferPaused 一样）
     */
    @JvmStatic
    fun update(
        active: Boolean,
        paused: Boolean,
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
            // 暂停/取消走 ShareActivity 传进来的回调，面板不用去碰 ServerService 的传输控制
            xferId = -1,
            xferPaused = paused,
        )
    }

    /** Activity 销毁：owner 走到 DESTROYED，状态清干净（下次进来重新开始） */
    @JvmStatic
    fun release() {
        val current = owner
        owner = null
        actions.value = null
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
