package top.weixiansen574.hybridfilexfer.compose

import android.os.Handler
import android.os.Looper
import android.os.StatFs
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.gestures.detectHorizontalDragGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Folder
import androidx.compose.material.icons.rounded.InsertDriveFile
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import kotlin.math.roundToInt
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import top.weixiansen574.hybridfilexfer.core.bean.Directory
import top.weixiansen574.hybridfilexfer.core.bean.RemoteFile
import top.weixiansen574.hybridfilexfer.core.bean.TrafficInfo
import top.weixiansen574.hybridfilexfer.core.callback.TransferFileCallback
import top.weixiansen574.hybridfilexfer.droidcore.HFXServer
import top.yukonga.miuix.kmp.basic.Button
import top.yukonga.miuix.kmp.basic.ButtonDefaults
import top.yukonga.miuix.kmp.basic.Icon
import top.yukonga.miuix.kmp.basic.Scaffold
import top.yukonga.miuix.kmp.basic.Text
import top.yukonga.miuix.kmp.basic.TextButton
import top.yukonga.miuix.kmp.basic.TopAppBar
import top.yukonga.miuix.kmp.theme.MiuixTheme

/**
 * 左右双窗口文件管理器（旧版 TransferActivity 的新界面版本）。
 *
 * 左边手机、右边电脑，行样式和底部工具条参考 MT 管理器的双窗口，
 * 配色/圆角/字体跟随应用主题（Miuix），深色/Monet 下表现一致。
 */
@Composable
internal fun TransferScreen(
    server: HFXServer,
    startLocal: String,
    onClose: () -> Unit,
) {
    val colorScheme = MiuixTheme.colorScheme
    /** 取消提示用系统 Toast：传输页会随连接断开一起卸载，页内弹窗显示不出来 */
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    val main = remember { Handler(Looper.getMainLooper()) }

    var localPath by remember { mutableStateOf(startLocal) }
    var remotePath by remember { mutableStateOf(server.remoteHomeDir.ifEmpty { "/" }) }
    val localFiles = remember { mutableStateListOf<RemoteFile>() }
    val remoteFiles = remember { mutableStateListOf<RemoteFile>() }
    val localSel = remember { mutableStateListOf<String>() }
    val remoteSel = remember { mutableStateListOf<String>() }
    var loadingLocal by remember { mutableStateOf(false) }
    var loadingRemote by remember { mutableStateOf(false) }
    var busy by remember { mutableStateOf(false) }
    var statusText by remember { mutableStateOf("") }
    var message by remember { mutableStateOf("") }
    var refreshTick by remember { mutableIntStateOf(0) }
    var focusRight by remember { mutableStateOf(false) }

    var mkdirDialog by remember { mutableStateOf(false) }
    var newName by remember { mutableStateOf("") }
    // 长按项目弹出的操作菜单（复制/移动/删除/重命名/属性）
    var menuFile by remember { mutableStateOf<RemoteFile?>(null) }
    var menuLeft by remember { mutableStateOf(false) }
    var renameFile by remember { mutableStateOf<RemoteFile?>(null) }
    var renameName by remember { mutableStateOf("") }
    var propsFile by remember { mutableStateOf<RemoteFile?>(null) }
    // 滑动选块的起点：第一下滑动只选一项并记下起点，第二下滑动把起点到这一项整段选中，然后清空起点
    var localBlockStart by remember { mutableIntStateOf(-1) }
    var remoteBlockStart by remember { mutableIntStateOf(-1) }
    // 双窗口里发送/接收时的进度弹窗（和经典界面的「发送到电脑」一致）
    var xferPopup by remember { mutableStateOf(false) }
    var xferPopupSending by remember { mutableStateOf(true) }
    var xferPopupName by remember { mutableStateOf("") }
    var xferPopupDone by remember { mutableLongStateOf(0L) }
    var xferPopupTotal by remember { mutableLongStateOf(0L) }
    var xferPopupChannels by remember { mutableStateOf<List<ChannelStat>>(emptyList()) }
    var xferPopupPaused by remember { mutableStateOf(false) }
    var xferPopupSpeed by remember { mutableLongStateOf(0L) }
    var xferPopupLastTraffic by remember { mutableLongStateOf(0L) }
    var xferPopupLastAt by remember { mutableLongStateOf(0L) }
    // 每条通道的累计已传字节。注意 onSpeedInfo 给的是"这一秒的流量"（resetCurrentTrafficInfo 会清零），
    // 不能直接当成已传总量 —— 否则进度条每秒钟都被重置回零点几百分比，看着就是不涨。
    val xferPopupCumulative = remember { HashMap<String, Long>() }
    // 304：数据发完之后电脑要算整文件 SHA-256（800MB 实测 1~2 分钟）。这段时间界面必须有个说法，
    // 否则就是"791.0 MB / 800.0 MB · 0 KB/s"一动不动，用户以为是卡死（实测被当成 bug 报上来）。
    var xferPopupWaiting by remember { mutableStateOf(false) }
    /** 已经发完的通道名（onChannelComplete 回调），用来判断"数据阶段结束" */
    val xferPopupDoneNames = remember { HashSet<String>() }

    // 返回键：先关弹窗/菜单 → 取消焦点窗口的多选 → 最后才退出传输页
    androidx.activity.compose.BackHandler(enabled = true) {
        when {
            menuFile != null -> menuFile = null
            renameFile != null -> renameFile = null
            propsFile != null -> propsFile = null
            mkdirDialog -> mkdirDialog = false
            message.isNotEmpty() -> message = ""
            focusRight && remoteSel.isNotEmpty() -> {
                remoteSel.clear()
                remoteBlockStart = -1
            }
            !focusRight && localSel.isNotEmpty() -> {
                localSel.clear()
                localBlockStart = -1
            }
            else -> onClose()
        }
    }

    fun toast(text: String) {
        main.post { message = text }
    }

    suspend fun loadLocal(path: String) {
        loadingLocal = true
        val result = withContext(Dispatchers.IO) { runCatching { server.listLocalFiles(path) ?: emptyList() } }
        loadingLocal = false
        localSel.clear()
        localFiles.clear()
        result.onSuccess { list ->
            localPath = path
            localFiles.addAll(list.sortedWith(compareByDescending<RemoteFile> { it.isDirectory }.thenBy { it.name.lowercase() }))
        }.onFailure { toast("读取手机目录失败：" + it.message) }
    }

    suspend fun loadRemote(path: String) {
        loadingRemote = true
        val result = withContext(Dispatchers.IO) { runCatching { server.listClientFiles(path) ?: emptyList() } }
        loadingRemote = false
        remoteSel.clear()
        remoteFiles.clear()
        result.onSuccess { list ->
            remotePath = path
            remoteFiles.addAll(list.sortedWith(compareByDescending<RemoteFile> { it.isDirectory }.thenBy { it.name.lowercase() }))
        }.onFailure { toast("读取电脑目录失败：" + it.message) }
    }

    LaunchedEffect(refreshTick) {
        loadLocal(localPath)
        loadRemote(remotePath)
    }

    fun callback(isUpload: Boolean): TransferFileCallback = object : TransferFileCallback {
        override fun onFileUploading(iName: String?, path: String?, targetSize: Long, totalSize: Long) {
            main.post { xferPopupName = path ?: "" }
        }

        override fun onFileDownloading(iName: String?, path: String?, targetSize: Long, totalSize: Long) {
            main.post { xferPopupName = path ?: "" }
        }

        override fun onSpeedInfo(trafficInfoList: List<TrafficInfo>?) {
            val list = trafficInfoList ?: return
            // onSpeedInfo 每秒回调一次，每条通道给的是"这一秒的流量"（TransferConnection.resetCurrentTrafficInfo 已经清零），
            // 所以这里自己累加出"已传总量"；之前直接把一秒的流量当已传量用，进度条每秒归零重来，看着永远不涨。
            val delta = list.sumOf { it.uploadTraffic + it.downloadTraffic }
            val now = android.os.SystemClock.elapsedRealtime()
            main.post {
                var done = 0L
                val channels = list.map { info ->
                    val key = info.iName ?: ""
                    val cum = (xferPopupCumulative[key] ?: 0L) + info.uploadTraffic + info.downloadTraffic
                    xferPopupCumulative[key] = cum
                    done += cum
                    // 已经发完的通道显示"已完成"，而不是一直"正在发送数据…"
                    ChannelStat(key, cum, 0, xferPopupDoneNames.contains(key))
                }
                if (xferPopupLastAt > 0 && now > xferPopupLastAt) {
                    val instant = delta * 1000L / (now - xferPopupLastAt)
                    // 指数平滑：瞬时速率会在 4～80MB/s 之间乱跳，直接显示就是"一顿一顿"的来源
                    xferPopupSpeed = if (xferPopupSpeed <= 0) instant else (xferPopupSpeed * 3 + instant * 2) / 5
                }
                xferPopupLastTraffic = done
                xferPopupLastAt = now
                // 数据发完之后进入"等待电脑校验"，进度条补满，别被后面 0 速率的采样打回去
                val shown = if (xferPopupWaiting) xferPopupTotal else done
                // 累计值以回调为准（真值），两次回调之间的推进交给下面的 100ms 插值
                xferPopupDone = shown
                xferPopupChannels = channels
                GlobalTransferOverlay.reportSend(
                    UiState(
                        xferActive = true,
                        xferName = xferPopupName,
                        xferDone = shown,
                        xferTotal = xferPopupTotal,
                        xferBytes = shown,
                        xferChannels = channels,
                        xferNote = if (xferPopupWaiting) "等待电脑校验…" else "",
                        xferCount = 1,
                        xferLabel = server.displayName,
                        xferId = -1,
                    ),
                )
            }
        }

        override fun onChannelComplete(iName: String?, traffic: Long, time: Long) {
            val key = iName ?: ""
            main.post {
                xferPopupDoneNames.add(key)
                if (!xferPopupWaiting && xferPopupSending) {
                    val total = xferPopupChannels.size
                    if (total > 0 && xferPopupDoneNames.size >= total) {
                        // 所有通道的 EOF 都发出去了 = 数据全部写完。之后到电脑回结论帧之前
                        // 只有电脑在算 SHA-256，界面切到"等待电脑校验…"并把进度条补满，
                        // 否则就是"791.0 MB / 800.0 MB · 0 KB/s"一动不动（实测被当成卡死）。
                        xferPopupWaiting = true
                        if (xferPopupTotal > 0) xferPopupDone = xferPopupTotal
                    }
                }
            }
        }

        override fun onChannelError(iName: String?, errorType: Int, message: String?) {
            // 用户主动取消时通道是我们自己关掉的，收尾异常不该报成"通道错误"；
            // 一条通道被别的通道拖停（INTERRUPT）也只是结果，不是错误。
            if (server.isTransferCancelled() || errorType == TransferFileCallback.ERROR_TYPE_INTERRUPT) return
            toast("通道错误：" + (message ?: ""))
        }

        override fun onReadFileError(message: String?) {
            toast("读取失败：" + (message ?: ""))
        }

        override fun onWriteFileError(message: String?) {
            toast("写入失败：" + (message ?: ""))
        }

        override fun onComplete(isUpload: Boolean, traffic: Long, time: Long) {
            // 真正的收尾（移动的删除、刷新）由调用处的协程统一做
            main.post {
                statusText = ""
                // 先把进度条补满再关，避免"进度条没走完就显示复制完成"
                if (xferPopupTotal > 0) xferPopupDone = xferPopupTotal
                GlobalTransferOverlay.reportSend(null)
                scope.launch {
                    kotlinx.coroutines.delay(450)
                    xferPopup = false
                }
            }
        }

        override fun onIncomplete() {
            main.post {
                busy = false
                statusText = ""
                xferPopup = false
                xferPopupWaiting = false
                GlobalTransferOverlay.reportSend(null)
                // 取消和真中断要分开显示：取消是用户自己点的，别写成"传输中断"
                message = if (server.isTransferCancelled()) "已取消传输" else "传输中断"
            }
        }
    }

    /** 复制/移动：把焦点窗口里选中的项目送到另一个窗口的当前目录 */
    fun startTransfer(move: Boolean, fromLeft: Boolean) {
        val files = if (fromLeft) localFiles.filter { it.path in localSel }
        else remoteFiles.filter { it.path in remoteSel }
        if (files.isEmpty() || busy) return
        busy = true
        statusText = if (move) "正在移动…" else "正在复制…"
        xferPopupSending = fromLeft
        xferPopupName = ""
        xferPopupDone = 0
        xferPopupTotal = files.sumOf { if (it.size > 0) it.size else 0L }
        xferPopupChannels = emptyList()
        xferPopupPaused = false
        // 速率/累计/采样时间都必须清掉：上一次传输的速率留着的话，
        // 弹窗一开就按旧速率往前插值，数字会一路虚高（进度条却还是空的）。
        xferPopupSpeed = 0
        xferPopupLastTraffic = 0
        xferPopupLastAt = 0
        xferPopupCumulative.clear()
        xferPopupWaiting = false
        xferPopupDoneNames.clear()
        xferPopup = true
        scope.launch(Dispatchers.IO) {
            try {
                val localDir = Directory(localPath, Directory.getCurrentFileSystem())
                val remoteDir = Directory(remotePath, server.remoteFileSystem)
                if (fromLeft) {
                    server.sendFilesToRemote(files, localDir, remoteDir, callback(true))
                } else {
                    server.sendFilesToShelf(files, localDir, remoteDir, callback(false))
                }
                // 取消时 sendFilesToRemote/Shelf 是正常返回的，靠这个标志区分
                val cancelled = server.isTransferCancelled()
                // 取消的这次传输不能删源文件：不判这一下，"移动"就会在传了一半的情况下
                // 把源文件删掉
                if (move && !cancelled) {
                    files.forEach { file ->
                        if (fromLeft) server.deleteLocalFile(file.path) else server.deleteRemoteFile(file.path)
                    }
                }
                // 先把弹窗里的进度条补满、停 600ms 再关：
                // SpeedMonitor 是每秒采样一次，最后一秒的字节往往还没回调上来，
                // 直接关掉就会出现"文件传完了，可每块进度条都没满"的样子。
                main.post {
                    busy = false
                    statusText = ""
                    if (!cancelled && xferPopupTotal > 0) xferPopupDone = xferPopupTotal
                }
                kotlinx.coroutines.delay(600)
                main.post {
                    xferPopup = false
                    GlobalTransferOverlay.reportSend(null)
                    // 用户取消后这里以前无条件写"复制完成"，界面就弹「复制完成」——
                    // 实际文件只传了一半，用户还以为是电脑端的问题。
                    message = when {
                        cancelled -> "已取消传输"
                        move -> "移动完成"
                        else -> "复制完成"
                    }
                    refreshTick++
                }
            } catch (e: Throwable) {
                main.post {
                    busy = false
                    statusText = ""
                    xferPopup = false
                    GlobalTransferOverlay.reportSend(null)
                    toast((if (move) "移动失败：" else "复制失败：") + e.message)
                }
            }
        }
    }

    /**
     * 滑动选择（按块）：
     * 第一下滑动只选中这一项并记下起点；第二下滑动选中「起点..这一项」整段，然后清空起点。
     * 下一轮滑动又是新的一块，多块并存（1→3 选 1..3，5→10 选 5..10，1..3 保留）。
     */
    fun swipeSelect(fromLeft: Boolean, index: Int) {
        val files = if (fromLeft) localFiles else remoteFiles
        val selected = if (fromLeft) localSel else remoteSel
        if (files.isEmpty() || index !in files.indices) return
        val start = if (fromLeft) localBlockStart else remoteBlockStart
        if (start < 0 || start !in files.indices) {
            selected.add(files[index].path)
            if (fromLeft) localBlockStart = index else remoteBlockStart = index
            return
        }
        val from = minOf(start, index)
        val to = maxOf(start, index)
        for (i in from..to) {
            if (files[i].path !in selected) {
                selected.add(files[i].path)
            }
        }
        if (fromLeft) localBlockStart = -1 else remoteBlockStart = -1
    }

    fun createEntry(isDir: Boolean) {
        val name = newName.trim()
        if (name.isEmpty()) return
        if (focusRight && !isDir) {
            toast("电脑端暂不支持新建空文件，请新建文件夹")
            return
        }
        scope.launch(Dispatchers.IO) {
            runCatching {
                if (!focusRight) {
                    if (isDir) server.createLocalDir(localPath, name) else createLocalFile(localPath, name)
                } else {
                    server.createRemoteDir(remotePath, name)
                }
            }.onSuccess {
                main.post {
                    mkdirDialog = false
                    newName = ""
                    refreshTick++
                }
            }.onFailure { toast("新建失败：" + it.message) }
        }
    }

    fun renameEntry(file: RemoteFile, newNameValue: String, left: Boolean) {
        val name = newNameValue.trim()
        if (name.isEmpty() || name == file.name) return
        if (!left) {
            toast("电脑端暂不支持重命名")
            return
        }
        scope.launch(Dispatchers.IO) {
            val ok = try {
                val target = java.io.File(parentOf(file.path) ?: "/", name)
                java.io.File(file.path).renameTo(target)
            } catch (e: Throwable) {
                false
            }
            main.post {
                if (ok) {
                    renameFile = null
                    refreshTick++
                } else {
                    toast("重命名失败")
                }
            }
        }
    }

    fun deleteSelected() {
        val paths = (if (focusRight) remoteSel else localSel).toList()
        if (paths.isEmpty()) return
        scope.launch(Dispatchers.IO) {
            runCatching {
                paths.forEach { p ->
                    if (focusRight) server.deleteRemoteFile(p) else server.deleteLocalFile(p)
                }
            }.onSuccess {
                main.post { refreshTick++ }
            }.onFailure { toast("删除失败：" + it.message) }
        }
    }

    Column(Modifier.fillMaxSize().background(colorScheme.surface).statusBarsPadding()) {
        // 顶栏：标题绝对居中，关闭按钮在左、电脑名在右
        Box(Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 6.dp)) {
            TextButton(text = "关闭", onClick = onClose, modifier = Modifier.align(Alignment.CenterStart))
            Text(
                "传输文件",
                fontSize = 17.sp,
                fontWeight = FontWeight.Bold,
                textAlign = TextAlign.Center,
                modifier = Modifier.align(Alignment.Center),
            )
            Text(
                server.displayName,
                fontSize = 11.sp,
                color = colorScheme.onSurfaceVariantSummary,
                modifier = Modifier.align(Alignment.CenterEnd),
            )
        }
        Row(Modifier.weight(1f).fillMaxWidth()) {
            FilePane(
                title = "手机",
                path = localPath,
                files = localFiles,
                selected = localSel,
                loading = loadingLocal,
                focused = !focusRight,
                modifier = Modifier.weight(1f),
                onFocus = { focusRight = false },
                onOpen = { p -> scope.launch { loadLocal(p) } },
                onToggle = { file -> toggle(localSel, file.path) },
                onSwipe = { index -> swipeSelect(true, index) },
                onUp = {
                    parentOf(localPath)?.let { up -> scope.launch { loadLocal(up) } }
                },
                onMenu = { file, index ->
                    focusRight = false
                    if (file.path !in localSel) {
                        localSel.clear()
                        localSel.add(file.path)
                    }
                    menuLeft = true
                    menuFile = file
                },
            )
            Box(Modifier.width(6.dp).fillMaxHeight().background(colorScheme.surface)) 
            FilePane(
                title = "电脑",
                path = remotePath,
                files = remoteFiles,
                selected = remoteSel,
                loading = loadingRemote,
                focused = focusRight,
                modifier = Modifier.weight(1f),
                onFocus = { focusRight = true },
                onOpen = { p -> scope.launch { loadRemote(p) } },
                onToggle = { file -> toggle(remoteSel, file.path) },
                onSwipe = { index -> swipeSelect(false, index) },
                onUp = {
                    parentOf(remotePath)?.let { up -> scope.launch { loadRemote(up) } }
                },
                onMenu = { file, index ->
                    focusRight = true
                    if (file.path !in remoteSel) {
                        remoteSel.clear()
                        remoteSel.add(file.path)
                    }
                    menuLeft = false
                    menuFile = file
                },
            )
        }
        // 底部工具条
        Column(Modifier.fillMaxWidth().background(colorScheme.surfaceContainer).navigationBarsPadding()) {
            Row(
                modifier = Modifier.fillMaxWidth().padding(horizontal = 4.dp, vertical = 2.dp),
                horizontalArrangement = Arrangement.SpaceEvenly,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                BottomAction("↑ 上级", enabled = !busy) {
                    val path = if (focusRight) remotePath else localPath
                    parentOf(path)?.let { up ->
                        scope.launch { if (focusRight) loadRemote(up) else loadLocal(up) }
                    }
                }
                BottomAction("新建", enabled = !busy) {
                    newName = "新建文件夹"
                    mkdirDialog = true
                }
                BottomAction("删除", enabled = !busy && (if (focusRight) remoteSel.isNotEmpty() else localSel.isNotEmpty())) {
                    deleteSelected()
                }
            }
        }
    }

    if (mkdirDialog) {
        Dialog(
            onDismissRequest = { mkdirDialog = false },
            properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false),
        ) {
            Box(Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.35f)).clickable { mkdirDialog = false }, contentAlignment = Alignment.Center) {
                Column(
                    Modifier.width(340.dp).clip(RoundedCornerShape(20.dp))
                        .background(colorScheme.surfaceContainer).clickable { }.padding(18.dp),
                ) {
                    Text("新建", fontSize = 18.sp, fontWeight = FontWeight.Bold)
                    Spacer(Modifier.height(10.dp))
                    BasicTextField(
                        value = newName,
                        onValueChange = { newName = it },
                        singleLine = true,
                        textStyle = TextStyle(fontSize = 14.sp, color = colorScheme.onSurface),
                        modifier = Modifier.fillMaxWidth()
                            .clip(RoundedCornerShape(10.dp))
                            .background(colorScheme.surface)
                            .padding(horizontal = 10.dp, vertical = 8.dp),
                    )
                    Spacer(Modifier.height(14.dp))
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        TextButton(text = "取消", onClick = { mkdirDialog = false }, modifier = Modifier.weight(1f))
                        TextButton(text = "文件", onClick = { createEntry(false) }, modifier = Modifier.weight(1f))
                        Button(
                            onClick = { createEntry(true) },
                            modifier = Modifier.weight(1f),
                            colors = ButtonDefaults.buttonColorsPrimary(),
                        ) {
                            Text("文件夹", color = Color.White, fontSize = 13.sp, maxLines = 1, softWrap = false)
                        }
                    }
                }
            }
        }
    }

    // 弹窗显示期间把已传字节按最近速率平滑推进：回调是每秒级的，直接设值会一顿一顿
    LaunchedEffect(xferPopup) {
        var last = android.os.SystemClock.elapsedRealtime()
        while (xferPopup) {
            kotlinx.coroutines.delay(100)
            // 电脑端点暂停/继续时，这里的暂停闸门会被会话线程改掉，弹窗跟着显示"已暂停"
            xferPopupPaused = server.isTransferPaused
            val now = android.os.SystemClock.elapsedRealtime()
            val dt = (now - last).coerceAtLeast(1)
            last = now
            if (!xferPopupPaused && xferPopupSpeed > 0 && xferPopupTotal > 0 && xferPopupDone < xferPopupTotal) {
                val step = xferPopupSpeed * dt / 1000
                if (step > 0) xferPopupDone = minOf(xferPopupTotal, xferPopupDone + step)
            }
        }
    }

    if (xferPopup) {
        // 双窗口没有 BlockProgress 的区间信息，按通道数切出等分段落，填充跟随总进度：
        // 视觉上和主页接收弹窗的分块进度条一致
        val popupSegments = if (xferPopupChannels.isNotEmpty() && xferPopupTotal > 0) {
            // 和主页一样：每一块（这里按通道分）同时增长。
            // 每块负责总量/通道数，填充 = 该通道已收 / 它那一份
            val count = xferPopupChannels.size
            // 每段宽度是全长的 1/count，所以"段内填充 = 本通道已传 / 每段应传份额"，
            // 整条填充面积才等于 done/total（直接拿 cum/total 会把整条压小 count 倍，看着又像不涨）。
            val share = xferPopupTotal.toFloat() / count
            val complete = xferPopupDone >= xferPopupTotal
            List(count) { index ->
                val bytes = xferPopupChannels[index].bytes
                SegInfo(
                    owner = "seg" + index,
                    start = index * 100 / count,
                    end = (index + 1) * 100 / count,
                    filled = when {
                        complete -> 1f
                        share > 0f -> (bytes / share).coerceIn(0f, 1f)
                        else -> 0f
                    },
                )
            }
        } else {
            emptyList()
        }
        ReceiveProgressPanel(
            state = UiState(
                xferActive = true,
                xferName = xferPopupName,
                xferDone = xferPopupDone,
                xferTotal = xferPopupTotal,
                xferBytes = xferPopupDone,
                xferSegments = popupSegments,
                xferChannels = xferPopupChannels,
                xferPaused = xferPopupPaused,
                // 数据已发完、电脑在算 SHA-256：显示"等待电脑校验…"，别让它看着像卡死
                xferNote = if (xferPopupWaiting) "等待电脑校验…" else "",
                xferCount = 1,
                xferLabel = server.displayName,
                xferId = -1,
            ),
            onDismiss = { xferPopup = false },
            sending = xferPopupSending,
            onPause = {
                xferPopupPaused = true
                server.pauseTransfer()
            },
            onResume = {
                xferPopupPaused = false
                server.resumeTransfer()
            },
            onCancel = {
                xferPopupPaused = false
                xferPopup = false
                runCatching { server.abortTransfer() }
                // 取消会让这条连接整体断开、传输页随即卸载，页内弹窗根本来不及显示；
                // 用系统 Toast 才能在页面切走之后还看得到。
                android.widget.Toast.makeText(
                    ctx,
                    "已取消传输",
                    android.widget.Toast.LENGTH_SHORT,
                ).show()
            },
        )
    }

    if (menuFile != null) {
        val file = menuFile!!
        val left = menuLeft
        val count = if (left) localSel.size else remoteSel.size
        Dialog(
            onDismissRequest = { menuFile = null },
            properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false),
        ) {
            Box(
                Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.35f)).clickable { menuFile = null },
                contentAlignment = Alignment.Center,
            ) {
                Column(
                    Modifier.width(322.dp).clip(RoundedCornerShape(22.dp))
                        .background(colorScheme.surfaceContainer).clickable { }.padding(16.dp),
                ) {
                    Text("已选 " + count + " 项", fontSize = 16.sp, fontWeight = FontWeight.Bold)
                    Spacer(Modifier.height(2.dp))
                    Text(
                        if (left) "手机窗口" else "电脑窗口",
                        fontSize = 11.sp,
                        color = colorScheme.onSurfaceVariantSummary,
                    )
                    Spacer(Modifier.height(12.dp))
                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                        MenuTile(if (left) "复制 →" else "← 复制", Modifier.weight(1f)) {
                            menuFile = null
                            startTransfer(false, left)
                        }
                        MenuTile(if (left) "移动 →" else "← 移动", Modifier.weight(1f)) {
                            menuFile = null
                            startTransfer(true, left)
                        }
                    }
                    Spacer(Modifier.height(10.dp))
                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                        MenuTile("删除", Modifier.weight(1f)) {
                            menuFile = null
                            deleteSelected()
                        }
                        MenuTile("重命名", Modifier.weight(1f)) {
                            renameFile = file
                            renameName = file.name
                            menuFile = null
                        }
                    }
                    Spacer(Modifier.height(10.dp))
                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                        MenuTile("属性", Modifier.weight(1f)) {
                            propsFile = file
                            menuFile = null
                        }
                        Spacer(Modifier.weight(1f))
                    }
                    Spacer(Modifier.height(14.dp))
                    TextButton(text = "取消", onClick = { menuFile = null }, modifier = Modifier.fillMaxWidth())
                }
            }
        }
    }

    if (renameFile != null) {
        Dialog(
            onDismissRequest = { renameFile = null },
            properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false),
        ) {
            Box(
                Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.35f)).clickable { renameFile = null },
                contentAlignment = Alignment.Center,
            ) {
                Column(
                    Modifier.width(300.dp).clip(RoundedCornerShape(20.dp))
                        .background(colorScheme.surfaceContainer).clickable { }.padding(18.dp),
                ) {
                    Text("重命名", fontSize = 18.sp, fontWeight = FontWeight.Bold)
                    Spacer(Modifier.height(10.dp))
                    BasicTextField(
                        value = renameName,
                        onValueChange = { renameName = it },
                        singleLine = true,
                        textStyle = TextStyle(fontSize = 14.sp, color = colorScheme.onSurface),
                        modifier = Modifier.fillMaxWidth()
                            .clip(RoundedCornerShape(10.dp))
                            .background(colorScheme.surface)
                            .padding(horizontal = 10.dp, vertical = 8.dp),
                    )
                    Spacer(Modifier.height(14.dp))
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        TextButton(text = "取消", onClick = { renameFile = null }, modifier = Modifier.weight(1f))
                        Button(
                            onClick = {
                                val file = renameFile
                                if (file != null) {
                                    renameEntry(file, renameName, menuLeft)
                                }
                            },
                            modifier = Modifier.weight(1f),
                            colors = ButtonDefaults.buttonColorsPrimary(),
                        ) { Text("确定", color = Color.White) }
                    }
                }
            }
        }
    }

    if (propsFile != null) {
        val file = propsFile!!
        Dialog(
            onDismissRequest = { propsFile = null },
            properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false),
        ) {
            Box(
                Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.35f)).clickable { propsFile = null },
                contentAlignment = Alignment.Center,
            ) {
                Column(
                    Modifier.width(320.dp).clip(RoundedCornerShape(20.dp))
                        .background(colorScheme.surfaceContainer).clickable { }.padding(18.dp),
                ) {
                    Text("属性", fontSize = 18.sp, fontWeight = FontWeight.Bold)
                    Spacer(Modifier.height(8.dp))
                    Text("名称：" + file.name, fontSize = 12.sp)
                    Text("路径：" + file.path, fontSize = 12.sp)
                    Text("大小：" + if (file.isDirectory) "文件夹" else formatSizeText(file.size), fontSize = 12.sp)
                    Text("修改时间：" + timeText(file.lastModified()), fontSize = 12.sp)
                    Spacer(Modifier.height(14.dp))
                    Button(
                        onClick = { propsFile = null },
                        modifier = Modifier.fillMaxWidth(),
                        colors = ButtonDefaults.buttonColorsPrimary(),
                    ) { Text("知道了", color = Color.White) }
                }
            }
        }
    }

    if (message.isNotEmpty()) {
        Box(
            Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.35f)).clickable { message = "" },
            contentAlignment = Alignment.Center,
        ) {
            Column(
                Modifier.width(280.dp).clip(RoundedCornerShape(20.dp))
                    .background(colorScheme.surfaceContainer).clickable { }.padding(18.dp),
            ) {
                Text("提示", fontSize = 17.sp, fontWeight = FontWeight.Bold)
                Spacer(Modifier.height(8.dp))
                Text(message, fontSize = 13.sp)
                Spacer(Modifier.height(14.dp))
                Button(
                    onClick = { message = "" },
                    modifier = Modifier.fillMaxWidth(),
                    colors = ButtonDefaults.buttonColorsPrimary(),
                ) { Text("知道了", color = Color.White) }
            }
        }
    }
}

@Composable
private fun MenuTile(label: String, modifier: Modifier = Modifier, onClick: () -> Unit) {
    val colorScheme = MiuixTheme.colorScheme
    Box(
        modifier = modifier
            .height(46.dp)
            .clip(RoundedCornerShape(12.dp))
            .background(colorScheme.surface)
            .clickable { onClick() },
        contentAlignment = Alignment.Center,
    ) {
        Text(label, fontSize = 14.sp, color = colorScheme.onSurface)
    }
}

@Composable
private fun BottomAction(label: String, enabled: Boolean, onClick: () -> Unit) {
    TextButton(text = label, onClick = onClick, enabled = enabled)
}

@OptIn(androidx.compose.foundation.ExperimentalFoundationApi::class)
@Composable
private fun FilePane(
    title: String,
    path: String,
    files: List<RemoteFile>,
    selected: MutableList<String>,
    loading: Boolean,
    focused: Boolean,
    modifier: Modifier,
    onFocus: () -> Unit,
    onOpen: (String) -> Unit,
    onToggle: (RemoteFile) -> Unit,
    onSwipe: (Int) -> Unit,
    onUp: () -> Unit,
    onMenu: (RemoteFile, Int) -> Unit,
) {
    val colorScheme = MiuixTheme.colorScheme
    val folders = files.count { it.isDirectory }
    val fileCount = files.size - folders
    Column(
        modifier.fillMaxHeight().background(
            if (focused) colorScheme.primary.copy(alpha = 0.04f) else Color.Transparent,
        ),
    ) {
        Column(
            Modifier.fillMaxWidth().clickable { onFocus() }.padding(horizontal = 8.dp, vertical = 5.dp),
        ) {
            Text(title, fontSize = 12.sp, fontWeight = FontWeight.Bold, color = if (focused) colorScheme.primary else colorScheme.onSurface)
            Spacer(Modifier.height(2.dp))
            Text(
                path,
                fontSize = 10.sp,
                color = colorScheme.onSurfaceVariantSummary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                "文件夹 " + folders + " · 文件 " + fileCount + (if (path.startsWith("/")) "\n" + storageText(path) else ""),
                fontSize = 9.sp,
                color = colorScheme.onSurfaceVariantSummary,
                maxLines = 2,
            )
        }
        if (loading) {
            Text("读取中…", fontSize = 11.sp, modifier = Modifier.padding(8.dp))
        }
        val density = androidx.compose.ui.platform.LocalDensity.current
        // 行高：一次滑动跨过几行，用来算这一块选到哪
        var rowHeight by remember { mutableStateOf(0f) }
        LazyColumn(Modifier.weight(1f).fillMaxWidth()) {
            // 顶部「..」：返回上一级（和 MT 一样）
            if (parentOf(path) != null) {
                item(key = "..parent") {
                    Row(
                        modifier = Modifier.fillMaxWidth()
                            .clickable {
                                onFocus()
                                onUp()
                            }
                            .padding(horizontal = 6.dp, vertical = 5.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Icon(
                            imageVector = Icons.Rounded.Folder,
                            contentDescription = null,
                            tint = colorScheme.primary,
                            modifier = Modifier.size(16.dp),
                        )
                        Spacer(Modifier.width(5.dp))
                        Text("..", fontSize = 11.5.sp, fontWeight = FontWeight.Bold)
                    }
                }
            }
            itemsIndexed(files, key = { _, f -> f.path }) { index, file ->
                val checked = file.path in selected
                var dragOffset by remember(file.path) { mutableStateOf(0f) }
                val animatedOffset by animateFloatAsState(
                    targetValue = dragOffset,
                    animationSpec = spring(stiffness = 700f),
                    label = "rowSwipe",
                )
                val maxOffset = with(density) { 42.dp.toPx() }
                val haptic = LocalHapticFeedback.current
                Row(
                    modifier = Modifier.fillMaxWidth()
                        .offset { IntOffset(animatedOffset.roundToInt(), 0) }
                        .onSizeChanged { if (rowHeight <= 0f) rowHeight = it.height.toFloat() }
                        .background(if (checked) colorScheme.primary.copy(alpha = 0.14f) else Color.Transparent)
                        .pointerInput(files, file.path) {
                            var total = 0f
                            var startY = 0f
                            var lastY = 0f
                            detectHorizontalDragGestures(
                                onDragStart = { offset ->
                                    startY = offset.y
                                    lastY = offset.y
                                },
                                onHorizontalDrag = { change, dragAmount ->
                                    total += dragAmount
                                    lastY = change.position.y
                                    dragOffset = total.coerceIn(-maxOffset, maxOffset)
                                },
                                onDragEnd = {
                                    val threshold = with(density) { 30.dp.toPx() }
                                    if (kotlin.math.abs(total) > threshold) {
                                        haptic.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                                        onSwipe(index)
                                    }
                                    dragOffset = 0f
                                },
                                onDragCancel = { dragOffset = 0f },
                            )
                        }
                        .combinedClickable(
                            onClick = {
                                onFocus()
                                // 多选状态（已有选中项）：点谁都是勾选/取消；空闲时点文件夹进入、点文件开始多选
                                if (selected.isNotEmpty()) {
                                    onToggle(file)
                                } else if (file.isDirectory) {
                                    onOpen(file.path)
                                } else {
                                    onToggle(file)
                                }
                            },
                            onLongClick = {
                                onFocus()
                                onMenu(file, index)
                            },
                        )
                        .padding(horizontal = 6.dp, vertical = 5.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(
                        imageVector = if (file.isDirectory) Icons.Rounded.Folder else Icons.Rounded.InsertDriveFile,
                        contentDescription = null,
                        tint = colorScheme.primary,
                        modifier = Modifier.size(16.dp),
                    )
                    Spacer(Modifier.width(5.dp))
                    Column(Modifier.weight(1f)) {
                        Text(
                            file.name,
                            fontSize = 11.5.sp,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                            color = colorScheme.onSurface,
                        )
                        Text(
                            if (file.isDirectory) timeText(file.lastModified())
                            else timeText(file.lastModified()) + "   " + formatSizeText(file.size),
                            fontSize = 9.sp,
                            color = colorScheme.onSurfaceVariantSummary,
                            maxLines = 1,
                        )
                    }
                    if (checked) {
                        Text("✓", fontSize = 12.sp, color = colorScheme.primary)
                    }
                }
            }
        }
    }
}

private fun createLocalFile(parent: String, name: String): Boolean {
    val file = java.io.File(parent, name)
    return try {
        file.createNewFile()
    } catch (e: Throwable) {
        false
    }
}

private fun createRemoteFile(server: HFXServer, parent: String, name: String): Boolean {
    val path = if (parent.endsWith("/") || parent.endsWith("\\")) parent + name else parent + "/" + name
    return try {
        server.sendFilesToRemote(
            emptyList(),
            Directory(parent, Directory.getCurrentFileSystem()),
            Directory(path, server.remoteFileSystem),
            object : TransferFileCallback {
                override fun onFileUploading(iName: String?, path: String?, targetSize: Long, totalSize: Long) {}
                override fun onFileDownloading(iName: String?, path: String?, targetSize: Long, totalSize: Long) {}
                override fun onSpeedInfo(trafficInfoList: List<TrafficInfo>?) {}
                override fun onChannelComplete(iName: String?, traffic: Long, time: Long) {}
                override fun onChannelError(iName: String?, errorType: Int, message: String?) {}
                override fun onReadFileError(message: String?) {}
                override fun onWriteFileError(message: String?) {}
                override fun onComplete(isUpload: Boolean, traffic: Long, time: Long) {}
                override fun onIncomplete() {}
            },
        )
        true
    } catch (e: Throwable) {
        false
    }
}

private fun shortName(path: String?): String {
    if (path.isNullOrEmpty()) return ""
    val cut = maxOf(path.lastIndexOf('/'), path.lastIndexOf('\\'))
    return if (cut >= 0) path.substring(cut + 1) else path
}

private fun toggle(list: MutableList<String>, path: String) {
    if (path in list) list.remove(path) else list.add(path)
}

private fun parentOf(path: String): String? {
    val trimmed = path.trimEnd('/', '\\')
    val idx = maxOf(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'))
    return when {
        idx > 0 -> trimmed.substring(0, idx)
        idx == 0 -> trimmed.substring(0, 1)
        else -> null
    }
}

private fun timeText(millis: Long): String {
    if (millis <= 0) return ""
    val format = java.text.SimpleDateFormat("yy-MM-dd HH:mm", java.util.Locale.US)
    return format.format(java.util.Date(millis))
}

private fun storageText(path: String): String {
    return try {
        val stat = StatFs(path)
        "储存：" + formatSizeText(stat.availableBytes) + "/" + formatSizeText(stat.totalBytes)
    } catch (e: Throwable) {
        ""
    }
}

private fun formatSizeText(bytes: Long): String {
    if (bytes <= 0) return "0 B"
    val kb = 1024.0
    val b = bytes.toDouble()
    return when {
        b >= kb * kb * kb -> String.format(java.util.Locale.US, "%.2f GB", b / (kb * kb * kb))
        b >= kb * kb -> String.format(java.util.Locale.US, "%.1f MB", b / (kb * kb))
        b >= kb -> String.format(java.util.Locale.US, "%.0f KB", b / kb)
        else -> bytes.toString() + " B"
    }
}

private fun formatSpeedText(bytesPerSec: Long): String {
    return formatSizeText(bytesPerSec) + "/s"
}
