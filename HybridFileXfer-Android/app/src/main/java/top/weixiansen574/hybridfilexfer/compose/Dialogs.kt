package top.weixiansen574.hybridfilexfer.compose

import androidx.compose.foundation.Canvas
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
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
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
    // 一位小数就够：两位小数时 106.48 → 107.02 这种末位变化看着像在抖。
    // 不足 1 KB/s 的别四舍五入成 0 KB/s（蓝牙、限速很低的时候会显示成 0，像断了）
    return when {
        mb >= 1 -> String.format(java.util.Locale.US, "%.1f MB/s", mb)
        bytesPerSecond >= 1024 -> String.format(java.util.Locale.US, "%.0f KB/s", bytesPerSecond / 1024.0)
        else -> bytesPerSecond.toString() + " B/s"
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
internal fun ReceiveProgressPanel(
    state: UiState,
    onDismiss: () -> Unit = {},
    sending: Boolean = false,
    /** 全局悬浮窗用：不铺黑色遮罩，只画卡片本身，窗口是 WRAP_CONTENT */
    bare: Boolean = false,
    /**
     * 底部按钮区（关闭 / 暂停 / 取消）。
     * 分享发送页的取消按钮在 Activity 自己的布局里，面板只要进度，这里传 false 整块不画。
     */
    showFooter: Boolean = true,
    /** 由调用方自己控制暂停/继续/取消（双窗口、悬浮窗）；为空时走 ServerService 的应用内接收 */
    onPause: (() -> Unit)? = null,
    onResume: (() -> Unit)? = null,
    onCancel: (() -> Unit)? = null,
) {
    val colorScheme = MiuixTheme.colorScheme
    val haptic = LocalHapticFeedback.current
    // 用累计字节数算速率：多台电脑同时接收时显示的是合计速率。
    //
    // 采样改成「滑动窗口 + 轻度指数平滑 + 逐帧补间」三层，数字才不"僵"：
    // 老写法是「距上次被采用 ≥100ms 的瞬时值」再和上一帧 6:4 混合 —— 100ms 的窗口太短，
    // 块一到就是一波尖峰、块没到就是 0，混合权重又大，于是数字在 90~115 之间一格一格地蹦
    // （用户反馈「速率数值跳动有点僵硬」）。
    // 现在窗口取最近 600ms：窗口内的字节差本身就是平均值，尖峰被摊平；
    // 再和上一帧 3:1 混合，只削掉残余抖动，不拖慢响应。
    var lastBytes by remember { mutableStateOf(state.xferBytes) }
    // 累计字节最后一次「变大」的时刻：用来判断最近窗口内到底有没有数据在流动
    var lastGrowAt by remember { mutableLongStateOf(System.currentTimeMillis()) }
    var targetSpeed by remember { mutableStateOf(0f) }
    val samples = remember { ArrayDeque<Pair<Long, Long>>() } // (时刻, 累计字节)
    val windowMs = 600L
    // 闲置判定窗口：比采样窗口略长一点，卡在「字节刚刚长过」那个边界上不会被误判成没流动
    val idleMs = 700L

    // 重算的节拍。以前这里写的是 LaunchedEffect(state.xferBytes)，整段采样/平滑都由
    // 「累计字节变了」来驱动。传输末段只剩一条蓝牙（0.2~0.6 MB/s）时，字节是按 1MB 的块记的，
    // 好久才涨一次 —— key 不变就一次都不重算，targetSpeed 一直钉在最后那次 21 MB/s 上，
    // animateFloatAsState 也只是把旧值补间到底，于是出现「合计速率基本不动、还显示 21MB/s」。
    // 改成固定 100ms 自增的 tick 驱动：字节不动时也照常重算，数字才会自己往下衰减、最后归零。
    var speedTick by remember { mutableLongStateOf(0L) }
    LaunchedEffect(Unit) {
        while (true) {
            kotlinx.coroutines.delay(100L)
            speedTick++
        }
    }

    LaunchedEffect(speedTick) {
        val now = System.currentTimeMillis()
        val bytes = state.xferBytes
        // 新的一次传输（字节变小）或者刚进入这一屏：窗口重来
        if (bytes < lastBytes || samples.isEmpty()) {
            samples.clear()
            samples.addLast(now to bytes)
            lastBytes = bytes
            lastGrowAt = now
            targetSpeed = 0f
            return@LaunchedEffect
        }
        // 这一拍字节有没有涨；同时留一份「涨之前的值」和「上一次涨是什么时候」，
        // 用来处理「一块 1MB 隔了三秒才到」这种稀疏流动（见下面 spanMs）
        val grew = bytes > lastBytes
        val previousGrowAt = lastGrowAt
        val previousBytes = lastBytes
        if (grew) {
            lastGrowAt = now
        }
        lastBytes = bytes
        samples.addLast(now to bytes)
        // 只留窗口内的点；至少留两个，否则算不出间隔
        while (samples.size > 2 && now - samples.first().first > windowMs) {
            samples.removeFirst()
        }
        // 最近 idleMs 内累计字节一点没涨：确实没有数据在流（只剩一条慢通道、暂停、等待校验、
        // 已经收尾都在这一档）。这里必须按「没流动」直接归零。
        // 以前没有这一档，只靠下面 0.75/0.25 的平滑往下蹭，而平滑是「每次采样」做一次，
        // 采样本就稀疏，从 21 MB/s 蹭下来要几十次采样、好几分钟，期间界面一直挂着旧的高值。
        // 归零后交给外面 220ms 的 animateFloatAsState 补间，所以是滑下去而不是硬跳。
        if (now - lastGrowAt >= idleMs) {
            targetSpeed = 0f
            return@LaunchedEffect
        }
        // 这一拍没有新字节：瞬时值本来就是 0，拿它去重算只会得到「块一到就尖峰、块没到就是 0」，
        // 正是以前用户抱怨的「速率数值跳动僵硬」。所以空拍不重算，保持上一次的值；
        // 真正「没有数据在流」的情况由上面 700ms 的闲置归零负责。
        if (!grew) return@LaunchedEffect
        val oldest = samples.first()
        val dt = now - oldest.first
        // 600ms 的窗口只适合连续流动：只剩一条蓝牙（0.2~0.6 MB/s）时是「三秒才到一块 1MB」，
        // 这一块整个落进窗口里会被算成 1.7 MB/s，还在后面的几个空拍里被反复放大。
        // 所以「离上一个数据点已经超过一个窗口」时，改用「这次涨了多少 ÷ 隔了多久」当瞬时值；
        // 连续传输时间隔只有几十毫秒，走不到这一支，口径和以前完全一样。
        val gapMs = now - previousGrowAt
        val sparse = gapMs > windowMs
        val spanMs = if (sparse) gapMs else dt
        val deltaBytes = if (sparse) bytes - previousBytes else bytes - oldest.second
        // 悬浮窗刚弹出来 / 中断后重连时会算出十几 GB/s 的离谱值，这种情况不采信
        if (spanMs >= 200L && deltaBytes >= 0L) {
            val instant = deltaBytes * 1000f / spanMs
            if (instant <= 2000f * 1024f * 1024f) {
                targetSpeed = if (targetSpeed <= 0f) instant else targetSpeed * 0.75f + instant * 0.25f
            }
        }
    }
    val animatedSpeed by androidx.compose.animation.core.animateFloatAsState(
        targetValue = targetSpeed,
        animationSpec = androidx.compose.animation.core.tween(
            durationMillis = 220,
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

    val card: @Composable () -> Unit = {
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
                when {
                    sending -> "正在发送文件"
                    state.xferCount > 1 -> "正在接收 " + state.xferCount + " 台电脑的文件"
                    else -> "正在接收文件"
                },
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
                    "   ·   " + when {
                    state.xferPaused -> "已暂停"
                    // 数据已经发完、电脑在算整文件 SHA-256：这里必须有个说法，
                    // 否则界面就是"79x/800 MB · 0 KB/s"一动不动，看着像卡死（实测卡 2 分钟）
                    state.xferNote.isNotEmpty() -> state.xferNote
                    else -> (if (state.xferCount > 1) "合计 " else "") + formatSpeed(animatedSpeed.toLong())
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
                                sending -> "正在发送数据…"
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
            // 暂停/取消：双窗口（onPause）由调用方控制；应用内接收走 ServerService；
            // 都没有时发送卡片只给「关闭」。
            // showFooter=false（分享发送页）：整块不画，底部按钮由 Activity 的布局自己提供
            if (showFooter) {
                val showPause = state.xferActive && (onPause != null || (!sending && state.xferId >= 0))
                if (showPause) {
                    Spacer(Modifier.height(10.dp))
                    if (state.xferPaused) {
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.spacedBy(10.dp),
                        ) {
                            Button(
                                onClick = {
                                    haptic.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                                    (onResume ?: { ServerService.resumeTransfer(state.xferId) })()
                                },
                                modifier = Modifier.weight(1f),
                                colors = ButtonDefaults.buttonColorsPrimary(),
                            ) {
                                Text("继续", color = Color.White)
                            }
                            TextButton(
                                text = if (sending) "取消发送" else "取消接收",
                                onClick = {
                                    haptic.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                                    (onCancel ?: { ServerService.cancelTransfer(state.xferId) })()
                                },
                                modifier = Modifier.weight(1f),
                            )
                        }
                    } else {
                        TextButton(
                            text = if (sending) "暂停发送" else "暂停接收",
                            onClick = {
                                haptic.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                                (onPause ?: { ServerService.pauseTransfer(state.xferId) })()
                            },
                            modifier = Modifier.fillMaxWidth(),
                        )
                    }
                } else if (sending && !bare) {
                    Spacer(Modifier.height(10.dp))
                    TextButton(
                        text = "关闭",
                        onClick = { haptic.performHapticFeedback(HapticFeedbackType.TextHandleMove); onDismiss() },
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
            }
        }
    }
    if (bare) {
        Box(Modifier.fillMaxWidth().padding(12.dp), contentAlignment = Alignment.Center) { card() }
    } else {
        Box(
            Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.35f)).clickable { onDismiss() },
            contentAlignment = Alignment.Center,
        ) { card() }
    }
}

/**
 * 分块进度条：一段 = 一条连接负责的文件区间（IDM / 迅雷 那种）。
 * 外观跟设置页的滑动条一致：整条是圆角胶囊，填充块也是圆头；
 * 段宽按区间块数分配，段数会随"空闲连接去切别人的段"变化。
 *
 * 整条画在同一块 Canvas 上，几何**只由当前真值决定**。
 * 以前一格是一个 Composable + 各自的 animateFloatAsState，而 forEachIndexed 没有 key，
 * Compose 是按**位置**复用动画状态的：段一合并（格子数量变少），合并后的整格会继承旧第 1 格
 * 已经走到 1.0 的动画值，于是"进度先跳到 100% 再滑回来"——实测每次合并闪一下，
 * 段被新通道拆开时同样闪。现在没有格子、没有按位置复用的动画状态，合并只让一条竖线消失。
 *
 * 平滑：只有**一个**标量做补间（"该画到多少"），它和段的个数/位置无关，所以合并不会串状态；
 * 补间比真值慢一点时，把差值按"缺口比例"从还没填满的段上扣掉 —— 填充前沿平滑地跟在真值后面，
 * 已经填满的段不受影响（不会出现"传完的段又缩回去"）。
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
    // 单一标量补间：与段的个数/位置无关，段合并或拆开都不会让动画状态串位置
    val animated by androidx.compose.animation.core.animateFloatAsState(
        targetValue = fallback.coerceIn(0f, 1f),
        animationSpec = androidx.compose.animation.core.tween(
            durationMillis = 120,
            easing = androidx.compose.animation.core.LinearEasing,
        ),
        label = "barProgress",
    )
    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(16.dp)
            .clip(shape)
            .background(track),
    ) {
        Canvas(modifier = Modifier.fillMaxSize()) {
            val width = size.width
            val height = size.height
            if (segments.isEmpty()) {
                if (animated > 0f) {
                    drawRect(color, size = Size(width * animated, height))
                }
                return@Canvas
            }
            val spans = IntArray(segments.size) { (segments[it].end - segments[it].start).coerceAtLeast(1) }
            var spanSum = 0L
            for (s in spans) spanSum += s
            val totalSpan = spanSum.toFloat().coerceAtLeast(1f)
            val fills = FloatArray(segments.size) { segments[it].filled.coerceIn(0f, 1f) }
            // 平滑：补间值比真值慢的时候，把差值按"缺口比例"从还没填满的段上扣掉。
            // 已经填满的段（缺口为 0）不动，所以不会出现"传完的段又缩回去"；
            // totalSpan、fills 都和段的划分方式无关，合并前后扣掉的量完全一样 → 合并不跳变。
            val target = fallback.coerceIn(0f, 1f)
            val deficit = (target - animated) * totalSpan
            if (deficit > 0f) {
                var room = 0f
                for (i in fills.indices) room += (1f - fills[i]) * spans[i]
                if (room > 0f) {
                    val k = minOf(deficit, room) / room
                    for (i in fills.indices) fills[i] = (fills[i] - (1f - fills[i]) * k).coerceIn(0f, 1f)
                }
            }
            val dividerWidth = 1.dp.toPx()
            var x = 0f
            segments.forEachIndexed { index, seg ->
                // 最后一段吃掉浮点误差，别在右端留一条缝
                val segWidth = if (index == segments.size - 1) width - x else width * spans[index] / totalSpan
                val fill = fills[index]
                if (fill > 0f) {
                    drawRect(
                        color = color,
                        topLeft = Offset(x, 0f),
                        size = Size(segWidth * fill, height),
                    )
                }
                if (index > 0) {
                    // 切分竖线（IDM 那种红线）
                    drawRect(
                        color = divider,
                        topLeft = Offset(x, 0f),
                        size = Size(dividerWidth, height),
                    )
                }
                x += segWidth
            }
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
