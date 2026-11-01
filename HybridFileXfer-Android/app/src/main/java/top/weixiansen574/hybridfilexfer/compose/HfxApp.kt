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
private const val TAB_CHANNELS = 1
private const val TAB_SETTINGS = 2

internal data class UiState(
    val state: Int = 0,
    val connected: Boolean = false,
    val interfaceNames: List<String> = emptyList(),
    val connectedNames: List<String> = emptyList(),
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
    val channelDetails: List<String> = emptyList(),
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

internal fun readState(context: Context): UiState {
    val config = Config.getInstance(context)
    val server = ServerService.getServer()
    return UiState(
        state = ServerService.getState(),
        connected = ServerService.isConnected(),
        interfaceNames = ServerService.getInterfaceNames(),
        connectedNames = ServerService.getConnectedInterfaceNames(),
        mode = config.mode,
        autoConnect = config.isAutoConnectEnabled,
        localBuffer = config.localBufferCount,
        remoteBuffer = config.remoteBufferCount,
        homeDir = server?.remoteHomeDir ?: "",
        absDir = config.remoteAbsDir,
        peerIp = server?.peerIp,
        subDir = config.remoteSubDir,
        speedLimit = config.speedLimitMbps,
        disabledChannels = config.disabledChannels,
        updateRepo = config.updateRepo,
        lastPeak = config.lastPeakBytesPerSecond,
        wifiRate = wifiRateText(context),
        channelSpeeds = config.channelSpeeds,
        channelDetails = ServerService.getChannelDetails(),
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
    var infoDialog by remember { mutableStateOf(false) }
    val pagerState = rememberPagerState(pageCount = { 3 })
    // 拖动滑块时临时禁用整页横滑，否则手势会被 HorizontalPager 抢走
    var sliderActive by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val haptic = LocalHapticFeedback.current

    val pickFiles = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        if (!uris.isNullOrEmpty()) {
            val intent = Intent(activity, ShareActivity::class.java).apply {
                action = Intent.ACTION_SEND_MULTIPLE
                putParcelableArrayListExtra(Intent.EXTRA_STREAM, ArrayList(uris))
                type = "*/*"
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
            activity.startActivity(intent)
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
        while (true) {
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
            topBar = { TopAppBar(title = listOf("多轨快传", "通道", "设置")[pagerState.currentPage.coerceIn(0, 2)]) },
            bottomBar = {
                NavigationBar {
                    listOf(
                        "主页" to Icons.Rounded.Home,
                        "通道" to Icons.Rounded.PhonelinkRing,
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
                        interfaceCount = interfaces.size,
                        onStart = ::startServer,
                        onStop = {
                            ServerService.stopServer(activity)
                            state = readState(activity)
                        },
                        onTransfer = { pickFiles.launch(arrayOf("*/*")) },
                        onShowInfo = { infoDialog = true },
                        onOpenChannels = { scope.launch { pagerState.animateScrollToPage(TAB_CHANNELS) } },
                        onOpenSettings = { scope.launch { pagerState.animateScrollToPage(TAB_SETTINGS) } },
                    )
                    TAB_CHANNELS -> ChannelsPage(
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
                    )
                }
            }
        }
        if (infoDialog) {
            ConnectionInfoPanel(state, onDismiss = { infoDialog = false })
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
    interfaceCount: Int,
    onStart: () -> Unit,
    onStop: () -> Unit,
    onTransfer: () -> Unit,
    onShowInfo: () -> Unit,
    onOpenChannels: () -> Unit,
    onOpenSettings: () -> Unit,
) {
    LazyColumn(
        modifier = Modifier.fillMaxSize(),
        contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 96.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        item { StatusCard(state, onShowInfo) }
        item {
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                SmallTile(
                    modifier = Modifier.weight(1f),
                    label = "通道",
                    value = if (state.connected) state.connectedNames.size.toString() else interfaceCount.toString(),
                    onClick = onOpenChannels,
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

@Composable
private fun ChannelsPage(
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