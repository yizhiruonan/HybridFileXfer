package top.weixiansen574.hybridfilexfer.compose

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import top.weixiansen574.hybridfilexfer.ServerService
import top.yukonga.miuix.kmp.basic.Button
import top.yukonga.miuix.kmp.basic.ButtonDefaults
import top.yukonga.miuix.kmp.basic.Text
import top.yukonga.miuix.kmp.basic.TextButton
import top.yukonga.miuix.kmp.theme.MiuixTheme

internal data class NumberDialogSpec(
    val title: String,
    val subtitle: String,
    val initial: Int,
    val min: Int,
    val max: Int,
    val suffix: String,
    val onConfirm: (Int) -> Unit,
)

internal data class TextDialogSpec(
    val title: String,
    val subtitle: String,
    val initial: String,
    val onConfirm: (String) -> Unit,
)

private data class PcEntry(val name: String, val isDir: Boolean)

/** 让弹窗窗口铺满整屏（连状态栏/导航栏一起），遮罩才能完整覆盖。 */
@Composable
private fun FullScreenWindow() {
    val view = androidx.compose.ui.platform.LocalView.current
    androidx.compose.runtime.LaunchedEffect(Unit) {
        val window = ((view.parent as? androidx.compose.ui.window.DialogWindowProvider)?.window) ?: return@LaunchedEffect
        window.setLayout(
            android.view.ViewGroup.LayoutParams.MATCH_PARENT,
            android.view.ViewGroup.LayoutParams.MATCH_PARENT,
        )
        window.addFlags(
            android.view.WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN or
                android.view.WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
        )
        if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.R) {
            window.setDecorFitsSystemWindows(false)
        }
        window.addFlags(android.view.WindowManager.LayoutParams.FLAG_DIM_BEHIND)
        window.setDimAmount(0.35f)
    }
}

private fun httpGet(url: String): String {
    val connection = (java.net.URL(url).openConnection() as java.net.HttpURLConnection)
    connection.connectTimeout = 4000
    connection.readTimeout = 6000
    try {
        val stream = if (connection.responseCode in 200..299) connection.inputStream else connection.errorStream
        return stream?.bufferedReader(Charsets.UTF_8)?.use { it.readText() } ?: ""
    } finally {
        connection.disconnect()
    }
}

private fun parentPath(path: String): String? {
    val normalized = path.trimEnd('/', '\\')
    val index = maxOf(normalized.lastIndexOf('/'), normalized.lastIndexOf('\\'))
    if (index <= 0) return null
    val parent = normalized.substring(0, index)
    return if (parent.length == 2 && parent[1] == ':') parent + "/" else parent
}

@Composable
internal fun NumberDialog(spec: NumberDialogSpec, onDismiss: () -> Unit, onConfirm: (Int) -> Unit) {
    val colorScheme = MiuixTheme.colorScheme
    var input by remember { mutableStateOf(spec.initial.toString()) }
    Dialog(onDismissRequest = onDismiss, properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false)) {
        Box(
            modifier = Modifier.fillMaxSize().background(Color.Transparent).clickable { onDismiss() },
            contentAlignment = Alignment.Center,
        ) {
            FullScreenWindow()
            Column(
                modifier = Modifier.width(320.dp).clip(RoundedCornerShape(24.dp)).background(colorScheme.surfaceContainer).clickable { }.padding(20.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Text(spec.title, fontSize = 20.sp, fontWeight = FontWeight.Bold)
                Text(spec.subtitle, fontSize = 14.sp, color = colorScheme.onSurfaceVariantSummary, modifier = Modifier.padding(top = 6.dp))
                Row(
                    modifier = Modifier.fillMaxWidth().padding(top = 18.dp).clip(RoundedCornerShape(14.dp))
                        .background(colorScheme.secondaryContainerVariant).padding(horizontal = 16.dp, vertical = 14.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    BasicTextField(
                        value = input,
                        onValueChange = { value -> input = value.filter { it.isDigit() }.take(4) },
                        singleLine = true,
                        textStyle = androidx.compose.ui.text.TextStyle(fontSize = 18.sp, color = colorScheme.onSurface),
                        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                        modifier = Modifier.weight(1f),
                    )
                    if (spec.suffix.isNotEmpty()) {
                        Text(spec.suffix, fontSize = 18.sp, color = colorScheme.onSurfaceVariantSummary)
                    }
                }
                Row(
                    modifier = Modifier.fillMaxWidth().padding(top = 18.dp),
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    Button(onClick = onDismiss, modifier = Modifier.weight(1f)) { Text("取消") }
                    Button(
                        onClick = {
                            val value = (input.toIntOrNull() ?: spec.initial).coerceIn(spec.min, spec.max)
                            onConfirm(value)
                        },
                        modifier = Modifier.weight(1f),
                        colors = ButtonDefaults.buttonColorsPrimary(),
                    ) { Text("确定", color = Color.White) }
                }
            }
        }
    }
}

@Composable
internal fun TextDialog(spec: TextDialogSpec, onDismiss: () -> Unit, onConfirm: (String) -> Unit) {
    val colorScheme = MiuixTheme.colorScheme
    var input by remember { mutableStateOf(spec.initial) }
    Dialog(onDismissRequest = onDismiss, properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false)) {
        Box(
            modifier = Modifier.fillMaxSize().background(Color.Transparent).clickable { onDismiss() },
            contentAlignment = Alignment.Center,
        ) {
            Column(
                modifier = Modifier.width(320.dp).clip(RoundedCornerShape(24.dp)).background(colorScheme.surfaceContainer).clickable { }.padding(20.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Text(spec.title, fontSize = 20.sp, fontWeight = FontWeight.Bold)
                Text(spec.subtitle, fontSize = 13.sp, color = colorScheme.onSurfaceVariantSummary, modifier = Modifier.padding(top = 6.dp))
                Row(
                    modifier = Modifier.fillMaxWidth().padding(top = 18.dp).clip(RoundedCornerShape(14.dp))
                        .background(colorScheme.secondaryContainerVariant).padding(horizontal = 16.dp, vertical = 14.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    BasicTextField(
                        value = input,
                        onValueChange = { value -> input = value.take(120) },
                        singleLine = true,
                        textStyle = androidx.compose.ui.text.TextStyle(fontSize = 16.sp, color = colorScheme.onSurface),
                        modifier = Modifier.weight(1f),
                    )
                }
                Row(
                    modifier = Modifier.fillMaxWidth().padding(top = 18.dp),
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    Button(onClick = onDismiss, modifier = Modifier.weight(1f)) { Text("取消") }
                    Button(onClick = { onConfirm(input) }, modifier = Modifier.weight(1f), colors = ButtonDefaults.buttonColorsPrimary()) {
                        Text("确定", color = Color.White)
                    }
                }
            }
        }
    }
}

@Composable
internal fun PcFolderDialog(
    initialPath: String,
    peerIp: String?,
    onDismiss: () -> Unit,
    onSelected: (String) -> Unit,
) {
    val colorScheme = MiuixTheme.colorScheme
    var currentPath by remember { mutableStateOf(initialPath) }
    var entries by remember { mutableStateOf(listOf<PcEntry>()) }
    var loading by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf("") }
    var retryTick by remember { mutableStateOf(0) }

    LaunchedEffect(currentPath, peerIp, retryTick) {
        if (peerIp == null) {
            error = "adb 连接下无法访问电脑，请用同一 WIFI 连接后再浏览"
            return@LaunchedEffect
        }
        loading = true
        error = ""
        val result = withContext(Dispatchers.IO) {
            try {
                val encoded = java.net.URLEncoder.encode(currentPath, "UTF-8")
                val body = httpGet("http://" + peerIp + ":5742/list?path=" + encoded)
                val list = ArrayList<PcEntry>()
                body.lineSequence().forEach { line ->
                    when {
                        line.startsWith("D\t") -> list.add(PcEntry(line.substring(2), true))
                        line.startsWith("F\t") -> list.add(PcEntry(line.substring(2), false))
                        line.startsWith("E\t") -> error = line.substring(2)
                    }
                }
                list
            } catch (e: Exception) {
                error = e.message ?: "连接电脑失败"
                ArrayList()
            }
        }
        entries = result
        loading = false
    }

    fun selectDirectory(path: String) {
        if (peerIp == null) return
        try {
            httpGet("http://" + peerIp + ":5742/set?path=" + java.net.URLEncoder.encode(path, "UTF-8"))
        } catch (_: Exception) {
        }
    }

    Dialog(onDismissRequest = onDismiss, properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false)) {
        Box(
            modifier = Modifier.fillMaxSize().background(Color.Transparent).clickable { onDismiss() },
            contentAlignment = Alignment.Center,
        ) {
            FullScreenWindow()
            Column(
                modifier = Modifier.width(340.dp).clip(RoundedCornerShape(24.dp)).background(colorScheme.surfaceContainer).clickable { }.padding(vertical = 16.dp),
            ) {
                Text("浏览电脑文件夹", modifier = Modifier.fillMaxWidth(), fontSize = 20.sp, fontWeight = FontWeight.Bold, textAlign = TextAlign.Center)
                Text(currentPath, modifier = Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 6.dp), fontSize = 12.sp, color = colorScheme.onSurfaceVariantSummary, maxLines = 2, overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis)
                if (peerIp != null) {
                    Text("电脑 IP：" + peerIp, modifier = Modifier.padding(horizontal = 20.dp), fontSize = 11.sp, color = colorScheme.onSurfaceVariantSummary)
                }
                if (error.isNotEmpty()) {
                    Text(error, modifier = Modifier.padding(horizontal = 20.dp, vertical = 8.dp), fontSize = 13.sp, color = colorScheme.primary)
                    TextButton(text = "重试", onClick = { retryTick++ })
                }
                if (loading) {
                    Text("正在读取电脑目录…", modifier = Modifier.padding(horizontal = 20.dp, vertical = 8.dp), fontSize = 13.sp, color = colorScheme.onSurfaceVariantSummary)
                }
                LazyColumn(modifier = Modifier.fillMaxWidth().height(300.dp)) {
                    item {
                        Row(
                            modifier = Modifier.fillMaxWidth().clickable { parentPath(currentPath)?.let { currentPath = it } }.padding(horizontal = 20.dp, vertical = 12.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Text("..", fontSize = 15.sp, fontWeight = FontWeight.Bold)
                            Spacer(Modifier.width(8.dp))
                            Text("上一级", fontSize = 13.sp, color = colorScheme.onSurfaceVariantSummary)
                        }
                    }
                    items(entries.filter { it.isDir }) { entry ->
                        Row(
                            modifier = Modifier.fillMaxWidth().clickable {
                                val base = currentPath.trimEnd('/', '\\')
                                currentPath = base + "/" + entry.name
                            }.padding(horizontal = 20.dp, vertical = 12.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Text(entry.name, fontSize = 15.sp)
                        }
                    }
                }
                Row(modifier = Modifier.fillMaxWidth().padding(start = 20.dp, end = 20.dp, top = 12.dp), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    Button(onClick = onDismiss, modifier = Modifier.weight(1f)) { Text("取消") }
                    Button(
                        onClick = {
                            selectDirectory(currentPath)
                            onSelected(currentPath)
                        },
                        modifier = Modifier.weight(1f),
                        colors = ButtonDefaults.buttonColorsPrimary(),
                    ) { Text("选定此目录", color = Color.White) }
                }
            }
        }
    }
}

private fun formatSpeed(bytesPerSecond: Long): String {
    if (bytesPerSecond <= 0) {
        return "无记录"
    }
    val mb = bytesPerSecond / 1024.0 / 1024.0
    return if (mb >= 1) {
        String.format(java.util.Locale.US, "%.2f MB/s", mb)
    } else {
        String.format(java.util.Locale.US, "%.0f KB/s", bytesPerSecond / 1024.0)
    }
}

private fun humanSize(bytes: Long): String {
    if (bytes <= 0) return "0 B"
    val kb = 1024.0
    val b = bytes.toDouble()
    return when {
        b >= kb * kb * kb -> String.format("%.2f GB", b / (kb * kb * kb))
        b >= kb * kb -> String.format("%.1f MB", b / (kb * kb))
        b >= kb -> String.format("%.0f KB", b / kb)
        else -> "$bytes B"
    }
}

/** 电脑推送文件时的进度提示：分块进度条 + 每通道明细（IDM / 迅雷 那种） */
@Composable
internal fun ReceiveProgressPanel(state: UiState, onDismiss: () -> Unit = {}) {
    val colorScheme = MiuixTheme.colorScheme
    val haptic = LocalHapticFeedback.current
    // 用累计字节数算速率：多台电脑同时接收时显示的是合计速率。
    // 采样值先做一次指数平滑，再交给动画逐帧插值，数字才是"滑"的而不是一跳一跳。
    var lastBytes by remember { mutableStateOf(state.xferBytes) }
    var lastAt by remember { mutableStateOf(System.currentTimeMillis()) }
    var targetSpeed by remember { mutableStateOf(0f) }

    LaunchedEffect(state.xferBytes) {
        val now = System.currentTimeMillis()
        val dt = (now - lastAt).coerceAtLeast(16L)
        val instant = if (state.xferBytes >= lastBytes) (state.xferBytes - lastBytes) * 1000f / dt else 0f
        targetSpeed = if (targetSpeed <= 0f) instant else targetSpeed * 0.6f + instant * 0.4f
        lastBytes = state.xferBytes
        lastAt = now
    }
    val animatedSpeed by androidx.compose.animation.core.animateFloatAsState(
        targetValue = targetSpeed,
        animationSpec = androidx.compose.animation.core.tween(
            durationMillis = 300,
            easing = androidx.compose.animation.core.LinearEasing,
        ),
        label = "speed",
    )
    // 已接收字节也做插值，"x MB / y MB" 才会连着走而不是每块跳一下
    val animatedDone by androidx.compose.animation.core.animateFloatAsState(
        targetValue = state.xferDone.toFloat(),
        animationSpec = androidx.compose.animation.core.tween(
            durationMillis = 200,
            easing = androidx.compose.animation.core.LinearEasing,
        ),
        label = "doneBytes",
    )

    val name = state.xferName.substringAfterLast('/').substringAfterLast('\\')

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black.copy(alpha = 0.35f))
            .clickable { onDismiss() },
        contentAlignment = Alignment.Center,
    ) {
        Column(
            modifier = Modifier
                .width(340.dp)
                .heightIn(max = 560.dp)
                .clip(RoundedCornerShape(24.dp))
                .background(colorScheme.surfaceContainer)
                .clickable { }
                .verticalScroll(rememberScrollState())
                .padding(20.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text(
                if (state.xferCount > 1) "正在接收 " + state.xferCount + " 台电脑的文件" else "正在接收文件",
                fontSize = 19.sp,
                fontWeight = FontWeight.Bold,
            )
            Spacer(Modifier.height(10.dp))
            Text(
                (if (state.xferLabel.isNotEmpty()) state.xferLabel + " · " else "") + name.ifEmpty { "…" },
                fontSize = 13.sp,
                maxLines = 1,
                overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
            )
            Spacer(Modifier.height(12.dp))
            // IDM 式动态分段：一段 = 一条连接负责的文件区间，段数随切分变化。
            // 底色用深一点的灰，没传到的部分也能看出进度条的总长度。
            SegmentedProgressBar(
                segments = state.xferSegments,
                fallback = if (state.xferTotal > 0) state.xferDone.toFloat() / state.xferTotal else 0f,
                color = colorScheme.primary,
                track = colorScheme.onSurface.copy(alpha = 0.16f),
                // 分段竖线：细、显眼（参考 IDM 的分段红线）
                divider = Color(0xFFE53935),
            )
            Spacer(Modifier.height(10.dp))
            Text(
                humanSize(animatedDone.toLong()) + " / " + humanSize(state.xferTotal) +
                    "   ·   " + if (state.xferPaused) {
                    "已暂停"
                } else {
                    (if (state.xferCount > 1) "合计 " else "") + formatSpeed(animatedSpeed.toLong())
                },
                fontSize = 12.sp,
                color = colorScheme.onSurfaceVariantSummary,
            )
            if (state.xferChannels.isNotEmpty()) {
                Spacer(Modifier.height(14.dp))
                Box(Modifier.fillMaxWidth().height(1.dp).background(colorScheme.surfaceVariant))
                Spacer(Modifier.height(10.dp))
                Text(
                    "各通道进度",
                    fontSize = 12.sp,
                    fontWeight = FontWeight.Bold,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(4.dp))
                state.xferChannels.forEachIndexed { index, channel ->
                    Row(
                        modifier = Modifier.fillMaxWidth().padding(vertical = 3.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(
                            (index + 1).toString(),
                            fontSize = 11.sp,
                            color = colorScheme.onSurfaceVariantSummary,
                            modifier = Modifier.width(18.dp),
                        )
                        Text(
                            channel.name,
                            fontSize = 12.sp,
                            maxLines = 1,
                            overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                            modifier = Modifier.weight(1f),
                        )
                        Text(
                            humanSize(channel.bytes),
                            fontSize = 11.sp,
                            color = colorScheme.onSurfaceVariantSummary,
                            maxLines = 1,
                            textAlign = TextAlign.End,
                            modifier = Modifier.width(74.dp),
                        )
                        Text(
                            when {
                                channel.speed > 0 -> formatSpeed(channel.speed)
                                channel.finished -> "已完成"
                                else -> "正在接收数据…"
                            },
                            fontSize = 11.sp,
                            maxLines = 1,
                            color = if (channel.speed > 0) colorScheme.primary else colorScheme.onSurfaceVariantSummary,
                            textAlign = TextAlign.End,
                            modifier = Modifier.width(82.dp),
                        )
                    }
                }
            }
            // 接收中只给「暂停接收」；暂停之后才给「继续 / 取消接收」
            if (state.xferActive) {
                Spacer(Modifier.height(10.dp))
                if (state.xferPaused) {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(10.dp),
                    ) {
                        Button(
                            onClick = {
                                haptic.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                                ServerService.resumeTransfer(state.xferId)
                            },
                            modifier = Modifier.weight(1f),
                            colors = ButtonDefaults.buttonColorsPrimary(),
                        ) {
                            Text("继续", color = Color.White)
                        }
                        TextButton(
                            text = "取消接收",
                            onClick = {
                                haptic.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                                ServerService.cancelTransfer(state.xferId)
                            },
                            modifier = Modifier.weight(1f),
                        )
                    }
                } else {
                    TextButton(
                        text = "暂停接收",
                        onClick = {
                            haptic.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                            ServerService.pauseTransfer(state.xferId)
                        },
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
            }
        }
    }
}

/**
 * 分块进度条：一段 = 一条连接负责的文件区间（IDM / 迅雷 那种）。
 * 外观跟设置页的滑动条一致：整条是圆角胶囊，填充块也是圆头；
 * 段宽按区间块数分配，段数会随"空闲连接去切别人的段"变化。
 */
@Composable
private fun SegmentedProgressBar(
    segments: List<SegInfo>,
    fallback: Float,
    color: Color,
    track: Color,
    divider: Color,
) {
    val shape = RoundedCornerShape(50)
    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(16.dp)
            .clip(shape)
            .background(track),
    ) {
        Row(modifier = Modifier.fillMaxSize()) {
            if (segments.isEmpty()) {
                ProgressCell(fallback.coerceIn(0f, 1f), Modifier.weight(1f), color, divider, first = true)
            } else {
                segments.forEachIndexed { index, seg ->
                    val span = (seg.end - seg.start).coerceAtLeast(1)
                    ProgressCell(
                        fraction = seg.filled.coerceIn(0f, 1f),
                        modifier = Modifier.weight(span.toFloat()),
                        color = color,
                        divider = divider,
                        first = index == 0,
                    )
                }
            }
        }
    }
}

/**
 * 一段的填充：整条被外层胶囊裁掉圆角，所以最左/最右自动是圆头，中间是直角。
 * 段与段之间不空开，而是画一条竖线隔开（IDM 那种）。
 */
@Composable
private fun ProgressCell(fraction: Float, modifier: Modifier, color: Color, divider: Color, first: Boolean) {
    // 50ms 采一次样，补 120ms 线性动画：每次只追一小段，看起来是连续流动的
    val animated by androidx.compose.animation.core.animateFloatAsState(
        targetValue = fraction.coerceIn(0f, 1f),
        animationSpec = androidx.compose.animation.core.tween(
            durationMillis = 120,
            easing = androidx.compose.animation.core.LinearEasing,
        ),
        label = "segmentFill",
    )
    Box(modifier = modifier.fillMaxHeight()) {
        if (animated > 0f) {
            Box(
                modifier = Modifier
                    .fillMaxHeight()
                    .fillMaxWidth(animated)
                    .background(color),
            )
        }
        if (!first) {
            Box(
                modifier = Modifier
                    .align(Alignment.CenterStart)
                    .width(1.dp)
                    .fillMaxHeight()
                    .background(divider),
            )
        }
    }
}

/** 多台电脑时选一台作为"发送到电脑"的目标 */
@Composable
internal fun PcChooserDialog(
    connections: List<PcConn>,
    onDismiss: () -> Unit,
    onSelected: (Int) -> Unit,
) {
    val colorScheme = MiuixTheme.colorScheme
    Dialog(
        onDismissRequest = onDismiss,
        properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false),
    ) {
        Box(
            modifier = Modifier.fillMaxSize().background(Color.Transparent).clickable { onDismiss() },
            contentAlignment = Alignment.Center,
        ) {
            FullScreenWindow()
            Column(
                modifier = Modifier.width(320.dp).clip(RoundedCornerShape(24.dp))
                    .background(colorScheme.surfaceContainer).clickable { }.padding(20.dp),
            ) {
                Text("发送到哪台电脑", fontSize = 20.sp, fontWeight = FontWeight.Bold)
                Text(
                    "已连接 " + connections.size + " 台，选一台接收",
                    fontSize = 13.sp,
                    color = colorScheme.onSurfaceVariantSummary,
                    modifier = Modifier.padding(top = 6.dp),
                )
                connections.forEach { pc ->
                    Row(
                        modifier = Modifier.fillMaxWidth().padding(top = 12.dp)
                            .clip(RoundedCornerShape(14.dp))
                            .background(colorScheme.secondaryContainerVariant)
                            .clickable { onSelected(pc.id) }
                            .padding(horizontal = 14.dp, vertical = 12.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Column(Modifier.weight(1f)) {
                            Text(pc.label, fontSize = 16.sp, fontWeight = FontWeight.Bold)
                            Text(
                                pc.ip ?: "USB / ADB",
                                fontSize = 12.sp,
                                color = colorScheme.onSurfaceVariantSummary,
                                modifier = Modifier.padding(top = 2.dp),
                            )
                        }
                        Text(
                            pc.channels.size.toString() + " 通道",
                            fontSize = 12.sp,
                            color = colorScheme.onSurfaceVariantSummary,
                        )
                    }
                }
                Button(
                    onClick = onDismiss,
                    modifier = Modifier.fillMaxWidth().padding(top = 16.dp),
                ) { Text("取消") }
            }
        }
    }
}

/**
 * 取消接收之后：已经收到的那部分要不要保留。
 * 保留 = 留一个不完整的文件（预分配过，大小是整文件大小）；删除 = 直接删掉。
 */
@Composable
internal fun KeepPartialDialog(paths: List<String>, onKeep: () -> Unit, onDelete: () -> Unit) {
    val colorScheme = MiuixTheme.colorScheme
    Dialog(
        onDismissRequest = onKeep,
        properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false),
    ) {
        Box(
            modifier = Modifier.fillMaxSize().background(Color.Transparent).clickable { onKeep() },
            contentAlignment = Alignment.Center,
        ) {
            FullScreenWindow()
            Column(
                modifier = Modifier.width(320.dp).clip(RoundedCornerShape(24.dp))
                    .background(colorScheme.surfaceContainer).clickable { }.padding(20.dp),
            ) {
                Text("已取消接收", fontSize = 20.sp, fontWeight = FontWeight.Bold)
                Text(
                    if (paths.size == 1) {
                        "已经接收到的那部分要保留吗？保留后是一个不完整的文件。"
                    } else {
                        "已经接收到的那部分要保留吗？保留后是 " + paths.size + " 个不完整的文件。"
                    },
                    fontSize = 13.sp,
                    color = colorScheme.onSurfaceVariantSummary,
                    modifier = Modifier.padding(top = 8.dp),
                )
                Text(
                    paths.joinToString("\n") { java.io.File(it).name },
                    fontSize = 12.sp,
                    color = colorScheme.onSurfaceVariantSummary,
                    maxLines = 3,
                    overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                    modifier = Modifier.padding(top = 6.dp),
                )
                Row(
                    modifier = Modifier.fillMaxWidth().padding(top = 16.dp),
                    horizontalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    TextButton(text = "删除", onClick = onDelete, modifier = Modifier.weight(1f))
                    Button(onClick = onKeep, modifier = Modifier.weight(1f)) { Text("保留") }
                }
            }
        }
    }
}
