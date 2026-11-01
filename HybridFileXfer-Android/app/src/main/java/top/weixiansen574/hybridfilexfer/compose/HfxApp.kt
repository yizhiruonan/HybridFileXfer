package top.weixiansen574.hybridfilexfer.compose

import android.content.Context
import android.content.Intent
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.waitForUpOrCancellation
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Check
import androidx.compose.material.icons.rounded.Home
import androidx.compose.material.icons.rounded.PhonelinkRing
import androidx.compose.material.icons.rounded.Settings
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import kotlin.math.roundToInt
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import top.weixiansen574.hybridfilexfer.Config
import top.weixiansen574.hybridfilexfer.NetInterfaceInfo
import top.weixiansen574.hybridfilexfer.NetInterfaces
import top.weixiansen574.hybridfilexfer.R
import top.weixiansen574.hybridfilexfer.ServerService
import top.weixiansen574.hybridfilexfer.ShareActivity
import top.yukonga.miuix.kmp.basic.Button
import top.yukonga.miuix.kmp.basic.ButtonDefaults
import top.yukonga.miuix.kmp.basic.Card
import top.yukonga.miuix.kmp.basic.HorizontalDivider
import top.yukonga.miuix.kmp.basic.Icon
import top.yukonga.miuix.kmp.basic.NavigationBar
import top.yukonga.miuix.kmp.basic.NavigationBarItem
import top.yukonga.miuix.kmp.basic.Scaffold
import top.yukonga.miuix.kmp.basic.Text
import top.yukonga.miuix.kmp.basic.TextButton
import top.yukonga.miuix.kmp.basic.TopAppBar
import top.yukonga.miuix.kmp.preference.ArrowPreference
import top.yukonga.miuix.kmp.preference.OverlayDropdownPreference
import top.yukonga.miuix.kmp.preference.SliderPreference
import top.yukonga.miuix.kmp.preference.SwitchPreference
import top.yukonga.miuix.kmp.theme.MiuixTheme

private const val TAB_HOME = 0
private const val TAB_CONNECTIONS = 1
private const val TAB_SETTINGS = 2

/** 一条传输通道的进度（进度浮层表格里的一行） */
internal data class ChannelStat(
    val name: String,
    val bytes: Long,
    val speed: Long,
    val finished: Boolean,
)

/** 进度条上的一段：一个连接负责的文件区间（IDM 式动态分段） */
internal data class SegInfo(
    val owner: String,
    val start: Int,
    val end: Int,
    val filled: Float,
)

/** 一台已连接电脑的界面模型（B 方案：手机同时接多台电脑） */
internal data class PcConn(
    val id: Int,
    val label: String,
    val ip: String?,
    val channels: List<String>,
    val details: List<String>,
    val transferring: Boolean = false,
    val fileName: String = "",
    val done: Long = 0,
    val total: Long = 0,
    /** 当前文件的分段进度（一段 = 一条连接负责的区间，段数随动态切分变化） */
    val segments: List<SegInfo> = emptyList(),
    val channelStats: List<ChannelStat> = emptyList(),
    /** 当前传输是否被用户暂停 */
    val paused: Boolean = false,
)

internal data class UiState(
    val state: Int = 0,
    val connected: Boolean = false,
    val interfaceNames: List<String> = emptyList(),
    val connectedNames: List<String> = emptyList(),
    val connections: List<PcConn> = emptyList(),
    val channelCount: Int = 0,
    val maxConnections: Int = 3,
    // 电脑推送文件的实时进度
    val pushDir: String = "",
    val xferActive: Boolean = false,
    val xferName: String = "",
    val xferDone: Long = 0,
    val xferTotal: Long = 0,
    val xferCount: Int = 0,
    /** 所有连接累计接收的字节数（只增不减），用来算合计速率 */
    val xferBytes: Long = 0,
    /** 当前展示的那台电脑与它的分块进度 / 通道明细 */
    val xferId: Int = -1,
    val xferPaused: Boolean = false,
    val xferLabel: String = "",
    val xferSegments: List<SegInfo> = emptyList(),
    val xferChannels: List<ChannelStat> = emptyList(),
    val mode: Int = 0,
    val autoConnect: Boolean = true,
    val localBuffer: Int = 256,
    val remoteBuffer: Int = 512,
    val homeDir: String = "",
    val absDir: String = "",
    val peerIp: String? = null,
    val subDir: String = "",
    val speedLimit: Int = 0,
    val disabledChannels: Set<String> = emptySet(),
    val updateRepo: String = "weixiansen574/HybridFileXfer",
    val lastPeak: Long = 0L,
    val wifiRate: String = "",
    val channelSpeeds: String = "",
    val usbSpeed: String = "",
    val rootState: String = "",
    val shellState: String = "",
    val version: String = "",
)

internal fun wifiRateText(context: Context): String {
    return try {
        val wifi = context.applicationContext.getSystemService(Context.WIFI_SERVICE) as? android.net.wifi.WifiManager
        val info = wifi?.connectionInfo ?: return ""
        val speed = info.linkSpeed
        val frequency = info.frequency
        if (speed <= 0) return ""
        val band = when {
            frequency >= 5925 -> "6GHz"
            frequency >= 4900 -> "5GHz"
            frequency >= 2400 -> "2.4GHz"
            else -> ""
        }
        if (band.isEmpty()) speed.toString() + " Mbps" else speed.toString() + " Mbps（" + band + "）"
    } catch (e: Exception) {
        ""
    }
}

/** 把 SAF 的 tree URI 还原为真实路径（主存储 / SD 卡） */
internal fun treeUriToPath(uri: android.net.Uri): String? {
    return try {
        if (uri.authority != "com.android.externalstorage.documents") return null
        val docId = android.provider.DocumentsContract.getTreeDocumentId(uri) ?: return null
        val parts = docId.split(':', limit = 2)
        val volume = parts[0]
        val relative = if (parts.size > 1) parts[1] else ""
        val base = if (volume.equals("primary", ignoreCase = true)) {
            android.os.Environment.getExternalStorageDirectory().absolutePath
        } else {
            "/storage/" + volume
        }
        if (relative.isEmpty()) base else base + "/" + relative
    } catch (e: Exception) {
        null
    }
}

/** 把已连接电脑转换成界面模型（完整刷新和快速进度刷新共用） */
internal fun buildPcConnections(servers: List<top.weixiansen574.hybridfilexfer.droidcore.HFXServer>): List<PcConn> {
    return servers.map { server ->
        val live = ServerService.liveFor(server.id)
        PcConn(
            id = server.id,
            label = server.displayName,
            ip = server.peerIp,
            channels = server.connectionListINames,
            details = server.channelDetails,
            transferring = live.active,
            fileName = live.name,
            // 分段传输时"最后一块的位置"不能代表进度，按已完成块数算
            done = server.blockProgress.doneBytes,
            total = live.total,
            segments = server.blockProgress.segments().map { seg ->
                SegInfo(seg.owner ?: "", seg.start, seg.end, seg.filled)
            },
            channelStats = live.channelSnapshot().mapNotNull { stat ->
                stat.name?.let { name -> ChannelStat(name, stat.bytes, stat.speed, stat.finished) }
            },
            paused = server.isTransferPaused,
        )
    }
}

/**
 * 传输中的快速刷新：只读进度相关的轻量状态（端口/权限/版本都不重读），
 * 让进度条和速度每 100 多毫秒就能动一次，避免 800ms 一跳的顿挫感。
 */
internal fun readProgressState(base: UiState): UiState {
    val connections = buildPcConnections(ServerService.getConnections())
    val current = ServerService.currentTransfer()
    val currentConn = connections.firstOrNull { it.id == (current?.connectionId ?: -1) }
    return base.copy(
        state = ServerService.getState(),
        connected = ServerService.isConnected(),
        connections = connections,
        channelCount = connections.sumOf { it.channels.size },
        xferActive = current != null,
        xferName = current?.name ?: "",
        xferDone = currentConn?.done ?: 0,
        xferTotal = currentConn?.total ?: 0,
        xferId = currentConn?.id ?: -1,
        xferPaused = currentConn?.paused ?: false,
        xferCount = ServerService.transferringCount(),
        xferBytes = connections.filter { it.transferring }.sumOf { it.done },
        xferLabel = currentConn?.label ?: "",
        xferSegments = currentConn?.segments ?: emptyList(),
        xferChannels = currentConn?.channelStats ?: emptyList(),
    )
}

internal fun readState(context: Context): UiState {
    val config = Config.getInstance(context)
    val servers = ServerService.getConnections()
    val connections = buildPcConnections(servers)
    val current = ServerService.currentTransfer()
    val currentConn = connections.firstOrNull { it.id == (current?.connectionId ?: -1) }
    return UiState(
        state = ServerService.getState(),
        connected = ServerService.isConnected(),
        interfaceNames = ServerService.getInterfaceNames(),
        connectedNames = ServerService.getConnectedInterfaceNames(),
        connections = connections,
        channelCount = connections.sumOf { it.channels.size },
        maxConnections = config.maxConnections,
        pushDir = config.pushDir,
        xferActive = current != null,
        xferName = current?.name ?: "",
        // 已完成字节按块数算（分段/乱序到达时，最后一块的位置不等于进度）
        xferDone = currentConn?.done ?: 0,
        xferTotal = currentConn?.total ?: 0,
        xferId = currentConn?.id ?: -1,
        xferPaused = currentConn?.paused ?: false,
        xferCount = ServerService.transferringCount(),
        // 合计速率用"各连接已完成字节"求和（按块统计，分段/乱序都不会乱跳），
        // 不能用 lastBlock 位置，旧算法在多通道时就地飙升
        xferBytes = connections.filter { it.transferring }.sumOf { it.done },
        xferLabel = currentConn?.label ?: "",
        xferSegments = currentConn?.segments ?: emptyList(),
        xferChannels = currentConn?.channelStats ?: emptyList(),
        mode = config.mode,
        autoConnect = config.isAutoConnectEnabled,
        localBuffer = config.localBufferCount,
        remoteBuffer = config.remoteBufferCount,
        homeDir = servers.firstOrNull()?.remoteHomeDir ?: "",
        absDir = config.remoteAbsDir,
        peerIp = servers.firstOrNull()?.peerIp,
        subDir = config.remoteSubDir,
        speedLimit = config.speedLimitMbps,
        disabledChannels = config.disabledChannels,
        updateRepo = config.updateRepo,
        lastPeak = config.lastPeakBytesPerSecond,
        wifiRate = wifiRateText(context),
        channelSpeeds = config.channelSpeeds,
        usbSpeed = UsbSpeed.query(context),
        rootState = PrivCheck.rootStatus(),
        shellState = PrivCheck.shizukuStatus(),
        version = versionName(context),
    )
}

@Composable
fun HfxApp(
    activity: ComponentActivity,
    themeMode: Int = 0,
    onThemeModeChange: (Int) -> Unit = {},
) {
    var state by remember { mutableStateOf(readState(activity)) }
    var interfaces by remember { mutableStateOf(listOf<NetInterfaceInfo>()) }
    var uiScale by remember { mutableStateOf(Config.getInstance(activity).uiScale) }
    var numberDialog by remember { mutableStateOf<NumberDialogSpec?>(null) }
    var pcBrowse by remember { mutableStateOf(false) }
    // 取消接收后待决定"保留还是删除"的半成品文件
    var keepPartials by remember { mutableStateOf<List<String>>(emptyList()) }
    val pagerState = rememberPagerState(pageCount = { 3 })
    // 拖动滑块时临时禁用整页横滑，否则手势会被 HorizontalPager 抢走
    var sliderActive by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val haptic = LocalHapticFeedback.current

    // 手动输入保存路径：SAF 不让选的目录（存储根、Android/data 等）用这个绕过去
    var pathDialog by remember { mutableStateOf(false) }

    // 选一个目录作为“电脑推送到手机”的保存位置
    val pickPushDir = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
        if (uri != null) {
            val path = treeUriToPath(uri)
            if (path != null) {
                Config.getInstance(activity).pushDir = path
                state = readState(activity)
            }
        }
    }

    // 多台电脑时先把"发给谁"选清楚，再启动系统文件选择器
    var transferTarget by remember { mutableStateOf(-1) }
    var targetChooser by remember { mutableStateOf(false) }
    val pickFiles = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        if (!uris.isNullOrEmpty()) {
            val intent = Intent(activity, ShareActivity::class.java).apply {
                action = Intent.ACTION_SEND_MULTIPLE
                putParcelableArrayListExtra(Intent.EXTRA_STREAM, ArrayList(uris))
                type = "*/*"
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                putExtra(ShareActivity.EXTRA_CONNECTION_ID, transferTarget)
            }
            activity.startActivity(intent)
        }
    }

    fun beginTransfer() {
        val targets = state.connections
        if (targets.size > 1) {
            targetChooser = true
        } else {
            transferTarget = targets.firstOrNull()?.id ?: -1
            pickFiles.launch(arrayOf("*/*"))
        }
    }

    LaunchedEffect(Unit) {
        if (!hasStoragePermission(activity)) {
            try {
                activity.startActivity(
                    Intent(
                        android.provider.Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION,
                        android.net.Uri.parse("package:" + activity.packageName),
                    ),
                )
            } catch (_: Exception) {
            }
        } else if (Config.getInstance(activity).isAutoConnectEnabled && !ServerService.isRunning()) {
            ServerService.startAuto(activity)
        }
        interfaces = try {
            NetInterfaces.enumerate(activity)
        } catch (e: Exception) {
            emptyList()
        }
        var lastFullRead = 0L
        while (true) {
            // 用户取消接收后，服务会把半成品路径放这里，弹窗问"保留还是删除"
            val cancelledPartials = ServerService.takeCancelledPartials()
            if (cancelledPartials.isNotEmpty()) {
                keepPartials = cancelledPartials
            }
            val now = android.os.SystemClock.elapsedRealtime()
            // 传输中：每 120ms 只刷进度相关的轻量状态，进度条和速度才跟得上；
            // 其余字段（权限/端口/网卡）仍然按原来的节奏全量刷新
            if (state.xferActive && now - lastFullRead < 1200) {
                state = withContext(Dispatchers.IO) { readProgressState(state) }
                delay(80)
                continue
            }
            lastFullRead = now
            val next = withContext(Dispatchers.IO) { readState(activity) }
            if (next != state) {
                state = next
            }
            // 实时刷新网卡：插拔数据线 / 连上或断开 WiFi 后不必重启应用。
            // 按名字合并，保留用户已做的勾选与指定 IP；新出现的网卡沿用枚举默认值。
            val fresh = withContext(Dispatchers.IO) {
                try {
                    NetInterfaces.enumerate(activity)
                } catch (e: Exception) {
                    null
                }
            }
            if (fresh != null) {
                val previous = interfaces.associateBy { it.name }
                fresh.forEach { item ->
                    val old = previous[item.name]
                    if (old != null) {
                        item.enabled = old.enabled
                        item.bindAddress = old.bindAddress
                    }
                }
                val changed = fresh.size != interfaces.size || fresh.indices.any { index ->
                    val a = fresh[index]
                    val b = interfaces[index]
                    a.name != b.name ||
                        a.address?.hostAddress != b.address?.hostAddress ||
                        a.enabled != b.enabled ||
                        a.bindAddress != b.bindAddress
                }
                if (changed) {
                    interfaces = fresh
                    // 服务在跑但还没连上电脑时，用新网卡列表重启会话。
                    // 否则会残留上一轮的网卡（比如 WiFi 重连后名字从 wlan0 变成 wlan1，旧通道还在）。
                    if (ServerService.isRunning() && !ServerService.isConnected()) {
                        try {
                            ServerService.restartSession(activity)
                        } catch (_: Exception) {
                        }
                    }
                }
            }
            delay(if (next.connected) 800 else 1500)
        }
    }

    fun toggleInterface(name: String, checked: Boolean) {
        haptic.performHapticFeedback(HapticFeedbackType.TextHandleMove)
        val updated = interfaces.map { source ->
            val copy = NetInterfaceInfo(source.name, source.address)
            copy.enabled = if (source.name == name) checked else source.enabled
            copy.bindAddress = source.bindAddress
            copy.state = source.state
            copy
        }
        interfaces = updated
        try {
            NetInterfaces.save(activity, updated)
        } catch (_: Exception) {
        }
        if (ServerService.isRunning()) {
            ServerService.restartSession(activity)
        }
    }

    fun toggleChannel(channel: String, enabled: Boolean) {
        haptic.performHapticFeedback(HapticFeedbackType.TextHandleMove)
        val disabled = Config.getInstance(activity).disabledChannels.toMutableSet()
        if (enabled) disabled.remove(channel) else disabled.add(channel)
        Config.getInstance(activity).disabledChannels = disabled
        state = readState(activity)
        if (ServerService.isRunning()) {
            ServerService.restartSession(activity)
        }
    }

    fun startServer() {
        try {
            val config = Config.getInstance(activity)
            val selected = NetInterfaces.selected(interfaces)
            if (selected.isEmpty()) return
            NetInterfaces.save(activity, interfaces)
            ServerService.startServer(activity, config.mode, selected, config.localBufferCount, config.remoteBufferCount)
            state = readState(activity)
        } catch (_: Exception) {
        }
    }

    fun setAutoConnect(enabled: Boolean) {
        haptic.performHapticFeedback(HapticFeedbackType.TextHandleMove)
        Config.getInstance(activity).isAutoConnectEnabled = enabled
        if (enabled) ServerService.startAuto(activity) else ServerService.stopServer(activity)
        state = readState(activity)
    }

    fun setMode(index: Int) {
        Config.getInstance(activity).mode = index
        state = readState(activity)
    }

    fun applyScale(value: Float) {
        uiScale = value.coerceIn(0.8f, 1.1f)
        Config.getInstance(activity).uiScale = uiScale
    }

    fun applySpeedLimit(value: Int) {
        Config.getInstance(activity).speedLimitMbps = value
        top.weixiansen574.hybridfilexfer.core.SpeedLimiter.setLimitBytesPerSecond(value * 1024L * 1024L)
        state = readState(activity)
    }

    val baseDensity = LocalDensity.current
    CompositionLocalProvider(
        LocalDensity provides Density(baseDensity.density * uiScale, baseDensity.fontScale * uiScale),
    ) {
        Box(modifier = Modifier.fillMaxSize()) {
        Scaffold(
            topBar = { TopAppBar(title = listOf("多轨快传", "连接", "设置")[pagerState.currentPage.coerceIn(0, 2)]) },
            bottomBar = {
                NavigationBar {
                    listOf(
                        "主页" to Icons.Rounded.Home,
                        "连接" to Icons.Rounded.PhonelinkRing,
                        "设置" to Icons.Rounded.Settings,
                    ).forEachIndexed { index, (label, icon) ->
                        NavigationBarItem(
                            selected = pagerState.currentPage == index,
                            onClick = {
                                haptic.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                                scope.launch { pagerState.animateScrollToPage(index, animationSpec = androidx.compose.animation.core.tween(220)) }
                            },
                            icon = icon,
                            label = label,
                        )
                    }
                }
            },
        ) { padding ->
            HorizontalPager(
                state = pagerState,
                modifier = Modifier.fillMaxSize().padding(padding),
                beyondViewportPageCount = 1,
                userScrollEnabled = !sliderActive,
            ) { page ->
                when (page) {
                    TAB_HOME -> HomePage(
                        state = state,
                        // 空闲时按"预计通道数"显示，和已连接时的口径保持一致：
                        // 一张 USB 网卡开 2 条通道，一张 WiFi/以太网开 4 条
                        plannedChannels = interfaces
                            .filter { it.enabled }
                            .sumOf { if (it.name.startsWith("USB_ADB")) 2 else 4 },
                        onStart = ::startServer,
                        onStop = {
                            ServerService.stopServer(activity)
                            state = readState(activity)
                        },
                        onTransfer = ::beginTransfer,
                        onOpenConnections = { scope.launch { pagerState.animateScrollToPage(TAB_CONNECTIONS) } },
                        onOpenSettings = { scope.launch { pagerState.animateScrollToPage(TAB_SETTINGS) } },
                    )
                    TAB_CONNECTIONS -> ConnectionsPage(
                        state = state,
                        interfaces = interfaces,
                        onToggleInterface = ::toggleInterface,
                        onToggleChannel = ::toggleChannel,
                        onOpenNumber = { spec -> numberDialog = spec },
                        onSpeedLimit = ::applySpeedLimit,
                    )
                    else -> SettingsPage(
                        activity = activity,
                        state = state,
                        uiScale = uiScale,
                        themeMode = themeMode,
                        onThemeModeChange = onThemeModeChange,
                        onScaleChange = ::applyScale,
                        onSliderActiveChange = { sliderActive = it },
                        onAutoConnect = ::setAutoConnect,
                        onModeChange = ::setMode,
                        onOpenNumber = { spec -> numberDialog = spec },
                        onOpenUrl = { url -> openUrl(activity, url) },
                        onBrowsePc = { pcBrowse = true },
                        onPickPushDir = { pickPushDir.launch(null) },
                        onTypePushDir = { pathDialog = true },
                        onMaxConnections = { value ->
                            Config.getInstance(activity).maxConnections = value
                            state = readState(activity)
                        },
                    )
                }
            }
        }
        if (pathDialog) {
            TextDialog(
                spec = TextDialogSpec(
                    title = "保存路径",
                    subtitle = "例如 /storage/emulated/0/Download/HybridFileXfer",
                    initial = state.pushDir,
                ) { value ->
                    Config.getInstance(activity).pushDir = value.trim()
                    state = readState(activity)
                },
                onDismiss = { pathDialog = false },
                onConfirm = { value ->
                    Config.getInstance(activity).pushDir = value.trim()
                    state = readState(activity)
                    pathDialog = false
                },
            )
        }
        if (state.xferActive) {
            ReceiveProgressPanel(state)
        }
        if (keepPartials.isNotEmpty()) {
            KeepPartialDialog(
                paths = keepPartials,
                onKeep = { keepPartials = emptyList() },
                onDelete = {
                    keepPartials.forEach { path ->
                        runCatching { java.io.File(path).delete() }
                    }
                    keepPartials = emptyList()
                },
            )
        }
        if (targetChooser) {
            PcChooserDialog(
                connections = state.connections,
                onDismiss = { targetChooser = false },
                onSelected = { id ->
                    transferTarget = id
                    targetChooser = false
                    pickFiles.launch(arrayOf("*/*"))
                },
            )
        }
        }
    }
    if (pcBrowse) {
        PcFolderDialog(
            initialPath = state.absDir.ifEmpty { state.homeDir },
            peerIp = state.peerIp,
            onDismiss = { pcBrowse = false },
            onSelected = { path ->
                Config.getInstance(activity).remoteAbsDir = path
                pcBrowse = false
                state = readState(activity)
            },
        )
    }
    numberDialog?.let { spec ->
        NumberDialog(spec = spec, onDismiss = { numberDialog = null }, onConfirm = { value ->
            spec.onConfirm(value)
            numberDialog = null
        })
    }
}

@Composable
private fun HomePage(
    state: UiState,
    plannedChannels: Int,
    onStart: () -> Unit,
    onStop: () -> Unit,
    onTransfer: () -> Unit,
    onOpenConnections: () -> Unit,
    onOpenSettings: () -> Unit,
) {
    LazyColumn(
        modifier = Modifier.fillMaxSize(),
        contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 96.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        item {
            StatusCard(state, onOpenConnections)
        }
        item {
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                SmallTile(
                    modifier = Modifier.weight(1f),
                    label = "通道",
                    value = if (state.connected) state.channelCount.toString() else plannedChannels.toString(),
                    onClick = onOpenConnections,
                )
                SmallTile(
                    modifier = Modifier.weight(1f),
                    label = "模式",
                    value = modeLabel(state.mode),
                    onClick = onOpenSettings,
                )
            }
        }
        item {
            if (state.state != ServerService.STATE_STOPPED) {
                Button(onClick = onStop, modifier = Modifier.fillMaxWidth(), colors = ButtonDefaults.buttonColorsPrimary()) {
                    Text("停止服务", color = Color.White)
                }
            } else {
                Button(onClick = onStart, modifier = Modifier.fillMaxWidth(), colors = ButtonDefaults.buttonColorsPrimary()) {
                    Text("启动服务并等待连接", color = Color.White)
                }
            }
        }
        item {
            TextButton(text = "传输文件", onClick = onTransfer, modifier = Modifier.fillMaxWidth(), enabled = state.connected)
        }
        if (state.homeDir.isNotEmpty()) {
            item {
                Text(
                    "接收文件夹：" + state.homeDir,
                    fontSize = 12.sp,
                    color = MiuixTheme.colorScheme.onSurfaceVariantSummary,
                    modifier = Modifier.padding(horizontal = 4.dp),
                )
            }
        }
    }
}

/**
 * 「连接」标签页：已连接的电脑 + 通道/网卡配置 + 环境信息。
 * （B 方案：一台手机同时接多台电脑；原来独立的「通道」页并到这里）
 */
@Composable
private fun ConnectionsPage(
    state: UiState,
    interfaces: List<NetInterfaceInfo>,
    onToggleInterface: (String, Boolean) -> Unit,
    onToggleChannel: (String, Boolean) -> Unit,
    onOpenNumber: (NumberDialogSpec) -> Unit,
    onSpeedLimit: (Int) -> Unit,
) {
    val usb = interfaces.filter { it.name.startsWith("USB_ADB") }
    val wlan = interfaces.filter { !it.name.startsWith("USB_ADB") }
    LazyColumn(
        modifier = Modifier.fillMaxSize(),
        contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 96.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        item {
            ConnectedComputersCard(state)
        }
        item {
            Card {
                ArrowPreference(
                    title = "限速",
                    summary = "0 表示不限速",
                    endActions = {
                        ActionText(if (state.speedLimit == 0) "不限速" else state.speedLimit.toString() + " MB/s")
                    },
                    onClick = {
                        onOpenNumber(
                            NumberDialogSpec(
                                title = "限速",
                                subtitle = "MB/s，0 表示不限速",
                                initial = state.speedLimit,
                                min = 0,
                                max = 1000,
                                suffix = "MB/s",
                                onConfirm = onSpeedLimit,
                            ),
                        )
                    },
                )
            }
        }
        listOf("USB 数据线" to usb, "WLAN 无线" to wlan).forEach { (title, list) ->
            if (list.isNotEmpty()) {
                item {
                    Text(
                        title,
                        fontSize = 13.sp,
                        fontWeight = FontWeight.Bold,
                        color = MiuixTheme.colorScheme.onSurfaceVariantSummary,
                        modifier = Modifier.padding(start = 6.dp, top = 6.dp),
                    )
                }
                list.forEach { iface ->
                    item {
                        Card {
                            SwitchPreference(
                                checked = iface.enabled,
                                onCheckedChange = { checked -> onToggleInterface(iface.name, checked) },
                                title = iface.name,
                                summary = iface.address.hostAddress,
                            )
                            val channelCount = if (iface.name.startsWith("USB_ADB")) 2 else 4
                            for (index in 1..channelCount) {
                                val channel = if (index == 1) iface.name else iface.name + "#" + index
                                SwitchPreference(
                                    checked = !state.disabledChannels.contains(channel),
                                    onCheckedChange = { checked -> onToggleChannel(channel, checked) },
                                    title = if (index == 1) "线路 1（主）" else "线路 " + index,
                                    summary = channel,
                                    enabled = iface.enabled,
                                )
                            }
                        }
                    }
                }
            }
        }
        item {
            EnvironmentCard(state)
        }
    }
}

/** 「连接」页顶部：现在连着哪几台电脑、各自用了几条通道、正在接收什么 */
@Composable
private fun ConnectedComputersCard(state: UiState) {
    val colorScheme = MiuixTheme.colorScheme
    val connected = state.connected
    Card {
        Column(modifier = Modifier.fillMaxWidth().padding(16.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    modifier = Modifier.size(38.dp).clip(CircleShape)
                        .background(colorScheme.primary.copy(alpha = if (connected) 0.15f else 0.08f)),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        imageVector = Icons.Rounded.PhonelinkRing,
                        contentDescription = null,
                        tint = if (connected) colorScheme.primary else colorScheme.onSurfaceVariantSummary,
                        modifier = Modifier.size(24.dp),
                    )
                }
                Spacer(Modifier.width(12.dp))
                Column(Modifier.weight(1f)) {
                    Text(
                        if (connected) "已连接 " + state.connections.size + " 台电脑" else "等待电脑连接",
                        fontSize = 17.sp,
                        fontWeight = FontWeight.Bold,
                    )
                    Text(
                        if (connected) state.channelCount.toString() + " 条通道" else "插上数据线，或在电脑上运行接收器",
                        fontSize = 12.sp,
                        color = colorScheme.onSurfaceVariantSummary,
                        modifier = Modifier.padding(top = 2.dp),
                    )
                }
            }
            // 每台电脑一张小卡：IP / 通道明细 / 当前传输
            state.connections.forEach { pc ->
                Column(
                    modifier = Modifier.fillMaxWidth().padding(top = 12.dp)
                        .clip(RoundedCornerShape(12.dp))
                        .background(colorScheme.surfaceVariant)
                        .padding(12.dp),
                ) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text(
                            pc.label,
                            fontSize = 15.sp,
                            fontWeight = FontWeight.Bold,
                            modifier = Modifier.weight(1f),
                        )
                        Text(
                            pc.channels.size.toString() + " 条通道",
                            fontSize = 12.sp,
                            color = colorScheme.onSurfaceVariantSummary,
                        )
                    }
                    Text(
                        pc.ip ?: "USB / ADB",
                        fontSize = 12.sp,
                        color = colorScheme.onSurfaceVariantSummary,
                        modifier = Modifier.padding(top = 2.dp),
                    )
                    pc.details.forEach { detail ->
                        val parts = detail.split("\t")
                        val name = parts.getOrNull(0) ?: detail
                        val address = parts.getOrNull(1) ?: ""
                        val rate = when {
                            name.contains("wlan", ignoreCase = true) -> state.wifiRate
                            name.startsWith("USB") -> state.usbSpeed
                            else -> ""
                        }
                        val line = buildString {
                            append(name)
                            if (address.isNotEmpty()) {
                                append(" · ")
                                append(address)
                            }
                            if (rate.isNotEmpty()) {
                                append(" · ")
                                append(rate)
                            }
                        }
                        Text(
                            line,
                            fontSize = 11.sp,
                            color = colorScheme.onSurfaceVariantSummary,
                            maxLines = 1,
                            overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                            modifier = Modifier.padding(top = 2.dp),
                        )
                    }
                    if (pc.transferring) {
                        val file = pc.fileName.substringAfterLast('/').substringAfterLast('\\')
                        val percent = if (pc.total > 0) (pc.done * 100 / pc.total).toInt() else 0
                        Text(
                            "接收中 " + percent + "% · " + file,
                            fontSize = 11.sp,
                            color = colorScheme.primary,
                            maxLines = 1,
                            overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                            modifier = Modifier.padding(top = 4.dp),
                        )
                    }
                }
            }
        }
    }
}

/** 「连接」页底部：接收目录、权限、链路速率这些"看一眼"的信息 */
@Composable
private fun EnvironmentCard(state: UiState) {
    Card {
        Column(modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 12.dp)) {
            Text("环境", fontSize = 17.sp, fontWeight = FontWeight.Bold, modifier = Modifier.padding(bottom = 4.dp))
            InfoRow("接收目录", state.homeDir.ifEmpty { "未连接" })
            InfoRow("模式", modeLabel(state.mode))
            InfoRow("WLAN 协商速率", state.wifiRate.ifEmpty { "未连接 WLAN" })
            InfoRow("USB 协议", state.usbSpeed.ifEmpty { "无法读取（需要 ROOT 权限）" })
            InfoRow("ROOT 权限", state.rootState.ifEmpty { "未知" })
            InfoRow("SHELL（Shizuku）", state.shellState.ifEmpty { "未知" })
        }
    }
}

@Composable
private fun SettingsPage(
    activity: ComponentActivity,
    state: UiState,
    uiScale: Float,
    themeMode: Int,
    onThemeModeChange: (Int) -> Unit,
    onScaleChange: (Float) -> Unit,
    onSliderActiveChange: (Boolean) -> Unit,
    onAutoConnect: (Boolean) -> Unit,
    onModeChange: (Int) -> Unit,
    onOpenNumber: (NumberDialogSpec) -> Unit,
    onOpenUrl: (String) -> Unit,
    onBrowsePc: () -> Unit,
    onPickPushDir: () -> Unit,
    onTypePushDir: () -> Unit,
    onMaxConnections: (Int) -> Unit,
) {
    LazyColumn(
        modifier = Modifier.fillMaxSize(),
        contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 96.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        item {
            Card {
                OverlayDropdownPreference(
                    title = "主题",
                    summary = "选择应用的主题模式",
                    items = themeModeLabels,
                    selectedIndex = themeMode.coerceIn(0, themeModeLabels.size - 1),
                    onSelectedIndexChange = onThemeModeChange,
                )
                SwitchPreference(
                    checked = state.autoConnect,
                    onCheckedChange = onAutoConnect,
                    title = "自动连接",
                    summary = "后台一直等待电脑连接（通知栏常驻）",
                )
                OverlayDropdownPreference(
                    title = "连接模式",
                    summary = "IO 服务的启动方式",
                    items = listOf("正常", "ROOT", "ADB"),
                    selectedIndex = state.mode.coerceIn(0, 2),
                    onSelectedIndexChange = onModeChange,
                )
                // 无级拖动 + 吸附到 80/90/100/110 四个刻度点；
                // 拖动过程中只更新本地值，松手后才真正应用缩放（实时缩放会拖慢整页重排）。
                // 外面套一层只观察不消费的手势，拖动期间暂停 Pager 横滑。
                var dragScale by remember(uiScale) { mutableStateOf(uiScale) }
                Box(
                    modifier = Modifier.pointerInput(Unit) {
                        awaitEachGesture {
                            awaitFirstDown(requireUnconsumed = false, pass = PointerEventPass.Initial)
                            onSliderActiveChange(true)
                            waitForUpOrCancellation(pass = PointerEventPass.Initial)
                            onSliderActiveChange(false)
                        }
                    },
                ) {
                    SliderPreference(
                        value = dragScale,
                        onValueChange = { raw -> dragScale = raw },
                        onValueChangeFinished = {
                            val snapped = (dragScale * 10f).roundToInt() / 10f
                            dragScale = snapped
                            onScaleChange(snapped)
                        },
                        title = "界面缩放",
                        summary = "调整全局显示比例（80% - 110%）",
                        valueText = (dragScale * 100).roundToInt().toString() + "%",
                        valueRange = 0.8f..1.1f,
                        steps = 0,
                        keyPoints = listOf(0.8f, 0.9f, 1.0f, 1.1f),
                        showKeyPoints = true,
                        magnetThreshold = 0.03f,
                        onClick = {
                            onOpenNumber(
                                NumberDialogSpec(
                                    title = "界面缩放",
                                    subtitle = "80% - 110%",
                                    initial = (uiScale * 100).roundToInt(),
                                    min = 80,
                                    max = 110,
                                    suffix = "%",
                                    onConfirm = { value -> onScaleChange(value / 100f) },
                                ),
                            )
                        },
                    )
                }
                ArrowPreference(
                    title = "最多连接电脑数",
                    summary = "每台电脑占用约 8 MiB 缓冲；同时连接多台时按这个上限",
                    endActions = { ActionText(state.maxConnections.toString() + " 台") },
                    onClick = {
                        onOpenNumber(
                            NumberDialogSpec(
                                title = "最多连接电脑数",
                                subtitle = "1 - 5 台",
                                initial = state.maxConnections,
                                min = 1,
                                max = 5,
                                suffix = "台",
                                onConfirm = onMaxConnections,
                            ),
                        )
                    },
                )
                ArrowPreference(
                    title = "电脑推送保存到",
                    summary = if (state.pushDir.isNotEmpty()) state.pushDir else "默认：Download/HybridFileXfer",
                    endActions = { ActionText("选择") },
                    onClick = onPickPushDir,
                )
                ArrowPreference(
                    title = "手动指定保存路径",
                    summary = "系统选择器不让选的目录（如存储根目录）可以用这里手输",
                    endActions = { ActionText("输入") },
                    onClick = onTypePushDir,
                )
                ArrowPreference(
                    title = "浏览电脑文件夹",
                    summary = if (state.absDir.isEmpty()) "点此在手机上浏览并选择电脑的接收目录" else state.absDir,
                    endActions = { ActionText("浏览") },
                    onClick = onBrowsePc,
                )
            }
        }
        item {
            Card {
                ArrowPreference(
                    title = "原项目 GitHub 主页",
                    summary = "weixiansen574/HybridFileXfer",
                    onClick = { onOpenUrl("https://github.com/weixiansen574/HybridFileXfer") },
                )
                ArrowPreference(
                    title = "获取更新",
                    summary = "在 GitHub Releases 查看最新版本",
                    onClick = { onOpenUrl("https://github.com/" + state.updateRepo + "/releases") },
                )
            }
        }
        item {
            SurfaceCard(corner = 16.dp) {
                Row(
                    modifier = Modifier.fillMaxWidth().padding(16.dp),
                    verticalAlignment = Alignment.Top,
                ) {
                    Box(
                        modifier = Modifier.size(44.dp).clip(CircleShape).background(MiuixTheme.colorScheme.primary.copy(alpha = 0.15f)),
                        contentAlignment = Alignment.Center,
                    ) {
                        Icon(Icons.Rounded.PhonelinkRing, contentDescription = null, tint = MiuixTheme.colorScheme.primary, modifier = Modifier.size(26.dp))
                    }
                    Spacer(Modifier.width(12.dp))
                    Column {
                        Text("多轨快传", fontSize = 16.sp, fontWeight = FontWeight.Bold)
                        Text("版本 " + versionName(activity), fontSize = 12.sp, color = MiuixTheme.colorScheme.onSurfaceVariantSummary, modifier = Modifier.padding(top = 2.dp))
                        Text("原项目作者：weixiansen574", fontSize = 12.sp, color = MiuixTheme.colorScheme.onSurfaceVariantSummary, modifier = Modifier.padding(top = 10.dp))
                        Text("当前作者：nilnan & DeepSeek-V4.1-Flash-1017", fontSize = 12.sp, color = MiuixTheme.colorScheme.onSurfaceVariantSummary, modifier = Modifier.padding(top = 4.dp))
                    }
                }
            }
        }
    }
}

private fun versionName(context: Context): String {
    return try {
        context.packageManager.getPackageInfo(context.packageName, 0).versionName ?: ""
    } catch (e: Exception) {
        ""
    }
}

private fun openUrl(context: Context, url: String) {
    try {
        context.startActivity(Intent(Intent.ACTION_VIEW, android.net.Uri.parse(url)))
    } catch (_: Exception) {
    }
}

private fun hasStoragePermission(context: Context): Boolean {
    return if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.R) {
        android.os.Environment.isExternalStorageManager()
    } else {
        context.checkSelfPermission(android.Manifest.permission.WRITE_EXTERNAL_STORAGE) ==
            android.content.pm.PackageManager.PERMISSION_GRANTED
    }
}