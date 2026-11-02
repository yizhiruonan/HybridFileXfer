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

    // 返回键：先关弹窗/菜单 → 取消焦点窗口的多选 → 最后才退出传输页
    androidx.activity.compose.BackHandler(enabled = true) {
        when {
            menuFile != null -> menuFile = null
            renameFile != null -> renameFile = null
            propsFile != null -> propsFile = null
            mkdirDialog -> mkdirDialog = false
            message.isNotEmpty() -> message = ""
            focusRight && remoteSel.isNotEmpty() -> remoteSel.clear()
            !focusRight && localSel.isNotEmpty() -> localSel.clear()
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
            main.post { statusText = "正在发送：" + shortName(path) }
        }

        override fun onFileDownloading(iName: String?, path: String?, targetSize: Long, totalSize: Long) {
            main.post { statusText = "正在接收：" + shortName(path) }
        }

        override fun onSpeedInfo(trafficInfoList: List<TrafficInfo>?) {
            val speed = trafficInfoList?.sumOf { it.uploadTraffic + it.downloadTraffic } ?: 0
            if (speed > 0) {
                main.post { statusText = (if (isUpload) "正在发送  " else "正在接收  ") + formatSpeedText(speed) }
            }
        }

        override fun onChannelComplete(iName: String?, traffic: Long, time: Long) {}

        override fun onChannelError(iName: String?, errorType: Int, message: String?) {
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
            main.post { statusText = "" }
        }

        override fun onIncomplete() {
            main.post {
                busy = false
                statusText = ""
                message = "传输中断"
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
        scope.launch(Dispatchers.IO) {
            try {
                val localDir = Directory(localPath, Directory.getCurrentFileSystem())
                val remoteDir = Directory(remotePath, server.remoteFileSystem)
                if (fromLeft) {
                    server.sendFilesToRemote(files, localDir, remoteDir, callback(true))
                } else {
                    server.sendFilesToShelf(files, localDir, remoteDir, callback(false))
                }
                if (move) {
                    files.forEach { file ->
                        if (fromLeft) server.deleteLocalFile(file.path) else server.deleteRemoteFile(file.path)
                    }
                }
                main.post {
                    busy = false
                    statusText = ""
                    message = if (move) "移动完成" else "复制完成"
                    refreshTick++
                }
            } catch (e: Throwable) {
                main.post {
                    busy = false
                    statusText = ""
                    toast((if (move) "移动失败：" else "复制失败：") + e.message)
                }
            }
        }
    }

    /** 滑动选择：一次滑动 = 一块（起始行到手指滑过的行），多次滑动是不同的块，互不影响 */
    fun swipeSelect(fromLeft: Boolean, index: Int, toIndex: Int) {
        val files = if (fromLeft) localFiles else remoteFiles
        val selected = if (fromLeft) localSel else remoteSel
        if (files.isEmpty()) return
        val from = minOf(index, toIndex).coerceIn(0, files.size - 1)
        val to = maxOf(index, toIndex).coerceIn(0, files.size - 1)
        for (i in from..to) {
            if (files[i].path !in selected) {
                selected.add(files[i].path)
            }
        }
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
        // 顶栏（和应用的 TopAppBar 同一套观感）
        Row(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            TextButton(text = "关闭", onClick = onClose)
            Text(
                "传输文件",
                fontSize = 17.sp,
                fontWeight = FontWeight.Bold,
                textAlign = TextAlign.Center,
                modifier = Modifier.weight(1f),
            )
            Text(server.displayName, fontSize = 11.sp, color = colorScheme.onSurfaceVariantSummary)
        }
        if (statusText.isNotEmpty()) {
            Text(
                statusText,
                fontSize = 11.sp,
                color = colorScheme.primary,
                textAlign = TextAlign.Center,
                modifier = Modifier.fillMaxWidth().padding(vertical = 2.dp),
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
                onSwipe = { from, to -> swipeSelect(true, from, to) },
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
                onSwipe = { from, to -> swipeSelect(false, from, to) },
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
            Row(
                modifier = Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 2.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                val focusCount = if (focusRight) remoteSel.size else localSel.size
                Button(
                    onClick = { startTransfer(false, !focusRight) },
                    enabled = !busy && focusCount > 0,
                    colors = ButtonDefaults.buttonColorsPrimary(),
                    modifier = Modifier.weight(1f),
                ) {
                    Text(
                        if (focusCount == 0) "复制 →" else "复制 " + focusCount + " 项 →",
                        color = Color.White,
                        fontSize = 13.sp,
                    )
                }
                Button(
                    onClick = { startTransfer(true, !focusRight) },
                    enabled = !busy && focusCount > 0,
                    colors = ButtonDefaults.buttonColorsPrimary(),
                    modifier = Modifier.weight(1f),
                ) {
                    Text(
                        if (focusCount == 0) "移动 →" else "移动 " + focusCount + " 项 →",
                        color = Color.White,
                        fontSize = 13.sp,
                    )
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
                    Modifier.width(300.dp).clip(RoundedCornerShape(20.dp))
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
                        ) { Text("文件夹", color = Color.White) }
                    }
                }
            }
        }
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
                    Modifier.width(300.dp).clip(RoundedCornerShape(20.dp))
                        .background(colorScheme.surfaceContainer).clickable { }.padding(14.dp),
                ) {
                    Text("已选 " + count + " 项", fontSize = 13.sp, fontWeight = FontWeight.Bold)
                    Spacer(Modifier.height(6.dp))
                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        MenuButton("复制 →", Modifier.weight(1f)) {
                            menuFile = null
                            startTransfer(false, left)
                        }
                        MenuButton("移动 →", Modifier.weight(1f)) {
                            menuFile = null
                            startTransfer(true, left)
                        }
                    }
                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        MenuButton("删除", Modifier.weight(1f)) {
                            menuFile = null
                            deleteSelected()
                        }
                        MenuButton("重命名", Modifier.weight(1f)) {
                            renameFile = file
                            renameName = file.name
                            menuFile = null
                        }
                    }
                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        MenuButton("属性", Modifier.weight(1f)) {
                            propsFile = file
                            menuFile = null
                        }
                        Spacer(Modifier.weight(1f))
                    }
                    Spacer(Modifier.height(4.dp))
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
private fun MenuButton(label: String, modifier: Modifier = Modifier, onClick: () -> Unit) {
    TextButton(text = label, onClick = onClick, modifier = modifier)
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
    onSwipe: (Int, Int) -> Unit,
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
                                        // 手指竖向滑过几行，这一块就选几行；每次滑动独立成块
                                        val rowsMoved = if (rowHeight > 1f) {
                                            ((lastY - startY) / rowHeight).roundToInt()
                                        } else {
                                            0
                                        }
                                        val end = (index + rowsMoved).coerceIn(0, files.size - 1)
                                        haptic.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                                        onSwipe(index, end)
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
