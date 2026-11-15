package top.weixiansen574.hybridfilexfer.compose

import android.content.Context
import android.os.Handler
import android.os.Looper
import android.util.Log
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.platform.ComposeView
import androidx.compose.ui.unit.dp
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
 * 由 [SharePanel.setActions] 一次性装进来；[ShareActions.onPause] / [ShareActions.onResume]
 * 传 null 时面板就不画暂停那一颗（和双窗口弹窗传 null 时的表现完全一样）。
 */
class ShareActions(
    val onDismiss: Runnable,
    val onCancel: Runnable,
    val onPause: Runnable?,
    val onResume: Runnable?,
)

/**
 * 「任务队列」列表里的一行。
 *
 * 由 Java 侧的 ShareActivity 每帧组装好（文案、百分比、位次都在那边算），面板只负责画：
 * 面板不知道任务状态机，也就不需要跟着任务的变化去改。
 *
 * 字段全是 val 且由 Java 直接构造，所以不是 data class（不需要 copy/equals，每帧本来就是新的一行）。
 */
class QueueRow(
    /** 这一行是哪个任务：点它切面板、取消它都按这个 taskId 走 */
    @JvmField val taskId: Int,
    /** 行首位次（1 开始） */
    @JvmField val index: Int,
    /** 文件名 */
    @JvmField val title: String,
    /** 第二行小字：目标电脑 · 已传/总量 */
    @JvmField val subtitle: String,
    /** 右边那句话：正在发送 45% / 等待中（前面还有 1 个）/ 已暂停 / 已完成 */
    @JvmField val status: String,
    /** 就是当前这个面板对应的任务（高亮，点了不跳） */
    @JvmField val current: Boolean,
    /** 这一行能不能「取消」（已收尾的不能） */
    @JvmField val cancellable: Boolean,
    /**
     * 这一行还在排队（准备中 / 等通道）。
     *
     * <p>「任务队列」这一段以前只在 {@code queue.size > 1} 时画。可「排队中」这个状态本身
     * 就是因为<b>有别的任务占着通道</b>才出现的，而那一路任务有可能刚好收尾、被回收，
     * 这一瞬间快照里就只剩排队的那一条 —— 面板上写着「排队中」却找不到队列段，
     * 报的「显示正在排队而没有显示队列界面」就是这种状态。所以排队中的任务一律画队列段。
     */
    @JvmField val queued: Boolean,
)

/**
 * 队列行上的两个落点。
 *
 * 用 fun interface 而不是 (Int) -> Unit：Java 侧才能直接写 {@code this::switchToTask}
 * 这种方法引用（Kotlin 的函数类型到 Java 是 Function1，那不是 SAM，Java 没法用 lambda）。
 */
fun interface QueueAction {
    fun onTask(targetTaskId: Int)
}

/**
 * 「分享发送」页的进度面板：把双窗口弹窗 / 应用外悬浮窗用的那个 [ReceiveProgressPanel]
 * 原样塞进 ShareActivity 这条 Java 流程里。
 *
 * 一个面板实例对应一个 Activity、一个发送任务（top.weixiansen574.hybridfilexfer.SendTask 的 taskId）。
 * 以前这里是个 object（全进程一份）：一个 Activity 承载所有分享任务，所以「暂停任务1再发任务2」
 * 时两个任务的面板状态互相覆盖，release() 还会把另一轮的状态一起清掉 —— 屏幕上那个空壳就是这么来的。
 * 现在每开一个面板就是 new 一个 [SharePanel]，Compose 状态、按钮落点、取消确认框全部各自一份。
 *
 * | 参数 | 双窗口 | 这里 |
 * | --- | --- | --- |
 * | state | UiState(...) | 同一个 UiState，字段含义一致（见下面的 update） |
 * | onDismiss | { xferPopup = false } | ShareActivity 的 dismissPanel（准备阶段关窗口 / 传输中收起） |
 * | sending | xferPopupSending | true（这个页面永远是「发」） |
 * | onPause / onResume | server.pauseTransfer / resumeTransfer | 任务自己的 pause() / resume() |
 * | onCancel | server.abortTransfer() + Toast | 任务自己的取消（先弹「保留/不保留」确认框） |
 * | bare | 不传（= false） | true（原因见下） |
 * | showFooter | 不传（= true） | 不传（= true） |
 * | closeWhenIdle | 不传（= false） | true：准备 / 等连接 / 复制 阶段底部补一颗「关闭」 |
 *
 * 为什么 bare 必须是 true：ReceiveProgressPanel 的 bare=false 分支根本不画遮罩框，
 * 它是一整个 Box(fillMaxSize + 黑 35%) —— 双窗口那边它铺在整页的 Composable 里所以正好是全屏压暗；
 * 而在 ShareActivity 这种 WRAP_CONTENT 的窗口里，fillMaxSize 会把这个小窗口直接撑成满屏 + 一层 35% 黑，
 * 也就是「一整块黑」。所以这里走 bare=true：只画那张卡片，窗口自己按卡片大小包住它。
 *
 * 这个 ComposeView 挂在 Activity 的普通 ViewGroup 里，但 Compose 需要三个 owner
 * （Lifecycle / ViewModelStore / SavedStateRegistry）。Activity 的 DecorView 上虽然有一套，
 * 这里按 GlobalTransferOverlay 的做法自己挂一套，行为和应用外悬浮窗完全一致
 * （少挂一个就会在 attach 时抛 IllegalStateException 把进程带崩）。
 */
internal class SharePanel(context: Context) {
    private val main = Handler(Looper.getMainLooper())

    /** 面板现在看到的状态：Compose 侧读它的 value，update 写它的 value */
    private val state = mutableStateOf(UiState())

    /** 面板按钮的落点，setActions 之前是 null（点了也不会有反应，不会崩） */
    private val actions = mutableStateOf<ShareActions?>(null)

    /**
     * 「取消发送」的确认框现在针对哪个任务（-1 = 框没开）。
     *
     * 以前是一个 Boolean：框只可能针对「这个面板的任务」。做了任务队列之后，队列行上的
     * 「取消」也要用同一个框（不另造一套），所以这里记 taskId，确认时按它去取消那个任务。
     */
    private val cancelConfirmTask = mutableStateOf(-1)

    /** 确认框的两个落点（参数 = 要取消的那个 taskId） */
    private var cancelKeep: QueueAction? = null
    private var cancelDiscard: QueueAction? = null

    /** 任务队列（多任务时才非空；空列表 = 面板和以前一模一样，不画这一段） */
    private val queue = mutableStateOf<List<QueueRow>>(emptyList())

    /** 队列行上的落点：切到那个任务的面板 / 取消那个任务（setQueueActions 之前是 null） */
    private val queueOpen = mutableStateOf<QueueAction?>(null)
    private val queueCancel = mutableStateOf<QueueAction?>(null)

    private val owner = Owner().also { it.start() }

    /** 造一个渲染进度面板的 ComposeView，由调用方 addView 到布局里的占位容器 */
    fun view(context: Context): ComposeView {
        val controller = ThemeController(
            colorSchemeMode = colorSchemeModeOf(Config.getInstance(context).themeMode),
        )
        return ComposeView(context).apply {
            setViewTreeLifecycleOwner(owner)
            setViewTreeViewModelStoreOwner(owner)
            setViewTreeSavedStateRegistryOwner(owner)
            setContent {
                MiuixTheme(controller = controller) {
                    // 和 TransferScreen.kt 里双窗口那段一比一：sending=true / 默认 showFooter /
                    // bare=true（false 会把小窗口撑成满屏黑，见类注释）/
                    // closeWhenIdle=true（准备 / 等连接 / 复制 阶段补一颗「关闭」）。
                    // widthDp=390dp：只把卡片放宽（双窗口是 340dp），窄屏时面板自己按「屏宽 - 32dp」兜底。
                    val current = actions.value
                    val openQueue = queueOpen.value
                    val cancelQueue = queueCancel.value
                    ReceiveProgressPanel(
                        state = state.value,
                        onDismiss = { current?.onDismiss?.run() },
                        sending = true,
                        onPause = { current?.onPause?.run() },
                        onResume = { current?.onResume?.run() },
                        onCancel = { current?.onCancel?.run() },
                        bare = true,
                        closeWhenIdle = true,
                        widthDp = 390.dp,
                        // 任务队列：只有多任务时非空（单个任务的面板和以前一个像素都不差）
                        queue = queue.value,
                        onQueueOpen = { id -> openQueue?.onTask(id) },
                        onQueueCancel = { id -> cancelQueue?.onTask(id) },
                    )
                    // 取消确认框：和双窗口那边是同一个 composable（Dialogs.kt）
                    val confirming = cancelConfirmTask.value
                    if (confirming >= 0) {
                        CancelTransferDialog(
                            sending = true,
                            onKeep = {
                                cancelConfirmTask.value = -1
                                cancelKeep?.onTask(confirming)
                            },
                            onDiscard = {
                                cancelConfirmTask.value = -1
                                cancelDiscard?.onTask(confirming)
                            },
                            onDismiss = { cancelConfirmTask.value = -1 },
                        )
                    }
                }
            }
        }
    }

    /** 把面板那几颗按钮接到任务上（在 view 之后随时可调） */
    fun setActions(next: ShareActions) {
        actions.value = next
    }

    /**
     * 装「取消发送」确认框的两个落点。
     *
     * 参数是 QueueAction（不是 Runnable）：队列行上的「取消」也要走这个框，
     * 确认时得知道取消的是**哪个任务**（面板上的「取消发送」传的就是本面板的 taskId）。
     *
     * @param keep 用户选了「保留进度」：电脑上的半截文件留着下次续传
     * @param discard 用户选了「不保留」：让电脑端把不完整文件和续传记录删掉
     */
    fun setCancelHandlers(keep: QueueAction?, discard: QueueAction?) {
        cancelKeep = keep
        cancelDiscard = discard
    }

    /** 装队列行上的两个落点：点一行切到那个任务的面板 / 点「取消」取消那个任务 */
    fun setQueueActions(open: QueueAction?, cancel: QueueAction?) {
        queueOpen.value = open
        queueCancel.value = cancel
    }

    /**
     * 弹「取消发送」确认框。
     *
     * @param targetTaskId 要取消哪个任务（面板上的「取消发送」= 本面板的 taskId；
     *                     队列行上的「取消」= 那一行的 taskId）
     */
    fun showCancelConfirm(targetTaskId: Int) {
        runOnMain { cancelConfirmTask.value = targetTaskId }
    }

    fun hideCancelConfirm() {
        runOnMain { cancelConfirmTask.value = -1 }
    }

    private fun runOnMain(block: Runnable) {
        if (Looper.myLooper() == Looper.getMainLooper()) {
            block.run()
        } else {
            main.post(block)
        }
    }

    /**
     * Java 侧专用入口。
     *
     * UiState 有四十多个字段，从 Java 直接构造太容易错位，这里只暴露进度相关的那几个，
     * 剩下的（连接列表、配置项……）保持默认值 —— 面板只读这几个字段。
     *
     * @param active true = 传输中：面板底部画「暂停发送」（暂停后是「继续」+「取消发送」）
     * @param paused 这个任务现在是暂停着（用户按的暂停，或者为了给别的任务让路而停住）
     */
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
        queueRows: List<QueueRow>,
    ) {
        // 队列是另一份状态（UiState 在 HfxApp.kt 里，这次不动它）：每帧一起换掉
        runOnMain { queue.value = queueRows }
        // 每次都是一份**完整**的 UiState（其余字段走默认值）：面板不可能从上一帧
        // 继承到 xferNote / xferPaused / 通道明细之类的旧值。
        update(UiState(
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
        ))
    }

    fun update(next: UiState) {
        runOnMain { state.value = next }
    }

    /** 面板销毁：owner 走到 DESTROYED，状态清干净（只影响这一个面板实例） */
    fun release() {
        runOnMain {
            actions.value = null
            cancelConfirmTask.value = -1
            queue.value = emptyList()
            state.value = UiState()
        }
        cancelKeep = null
        cancelDiscard = null
        queueOpen.value = null
        queueCancel.value = null
        owner.stop()
    }

    private class Owner : LifecycleOwner, ViewModelStoreOwner, SavedStateRegistryOwner {
        private val registry = LifecycleRegistry(this)
        private val store = ViewModelStore()
        private val savedState = SavedStateRegistryController.create(this)
        override val lifecycle: Lifecycle get() = registry
        override val viewModelStore: ViewModelStore get() = store
        override val savedStateRegistry: SavedStateRegistry get() = savedState.savedStateRegistry

        init {
            // performAttach 必须在 owner 还是 INITIALIZED 的时候叫（savedstate 的限制），
            // 之后再 performRestore，Compose 里的 rememberSaveable 才能正常读
            try {
                savedState.performAttach()
            } catch (e: Throwable) {
                Log.w(TAG, "owner attach failed", e)
            }
        }

        fun start() {
            try {
                // 顺序不能反：先 performRestore 再切到 CREATED
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

    private companion object {
        private const val TAG = "HFXSharePanel"
    }
}
