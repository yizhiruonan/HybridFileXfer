package top.weixiansen574.hybridfilexfer.compose

import android.content.Context
import android.content.Intent
import android.provider.Settings
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import top.weixiansen574.hybridfilexfer.NetInterfaces
import top.yukonga.miuix.kmp.basic.Button
import top.yukonga.miuix.kmp.basic.Text
import top.yukonga.miuix.kmp.basic.TextButton
import top.yukonga.miuix.kmp.theme.MiuixTheme

/**
 * 「榨干 IO」引导。
 *
 * <p>多条通道就是多条并行的水管：Wi-Fi、USB 数据线、USB 网络共享、蓝牙共享网络各是一条。
 * 普通用户不知道要去哪开这些开关，所以打开应用时问一句，点「确定」就一条条领着开，
 * 每开好一条这里轮询网卡自动打勾，最后再告诉用户多了几条通道。
 *
 * <p>蓝牙那条额外先走一遍最高权限尝试（su → shell），root 能直接开的就不用用户动手。
 */
private class BoostSlot(
    val key: String,
    val title: String,
    val reason: String,
    /** 这条链路现在可用吗（每次探测都重新枚举网卡） */
    val ready: (Context) -> Boolean,
    /** 直达对应的系统设置页；返回 false 表示系统里没有对应开关（比如 USB 数据线） */
    val open: (Context) -> Boolean,
    /** 最高权限尝试（root / shell），失败不影响后续 */
    val kick: (() -> Unit)? = null,
)

private fun interfaceNames(ctx: Context): List<String> = try {
    NetInterfaces.enumerate(ctx).map { it.name }
} catch (e: Throwable) {
    emptyList()
}

private fun anyOf(names: List<String>, vararg prefixes: String): Boolean =
    names.any { name -> prefixes.any { name.lowercase().startsWith(it) } }

private fun startActivityQuietly(ctx: Context, intent: Intent): Boolean = try {
    ctx.startActivity(intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    true
} catch (e: Throwable) {
    false
}

/** 用 root 跑一句命令；拿不到 root 就返回 false，不抛异常 */
private fun runSu(cmd: String): Boolean = try {
    val process = Runtime.getRuntime().exec(arrayOf("su", "-c", cmd))
    val finished = process.waitFor(4, java.util.concurrent.TimeUnit.SECONDS)
    val out = process.inputStream.bufferedReader().readText().trim()
    val err = process.errorStream.bufferedReader().readText().trim()
    android.util.Log.i("HFXBoost", "su -c \"" + cmd + "\" -> " + finished + " out=" + out + " err=" + err)
    finished && process.exitValue() == 0
} catch (e: Throwable) {
    android.util.Log.i("HFXBoost", "su -c \"" + cmd + "\" failed: " + e.message)
    false
}

/**
 * 没 root 就退到 Shizuku/adb（shell 身份）跑同一条命令。
 * Shizuku.newProcess 是隐藏 API，UsbSpeed.kt 里也是反射调的。
 */
private fun runShizuku(cmd: String): Boolean = try {
    if (!rikka.shizuku.Shizuku.pingBinder()) {
        false
    } else if (rikka.shizuku.Shizuku.checkSelfPermission() != android.content.pm.PackageManager.PERMISSION_GRANTED) {
        false
    } else {
        val method = rikka.shizuku.Shizuku::class.java.getDeclaredMethod(
            "newProcess",
            Array<String>::class.java,
            Array<String>::class.java,
            String::class.java,
        )
        method.isAccessible = true
        val process = method.invoke(null, arrayOf("sh", "-c", cmd), null, null) as rikka.shizuku.ShizukuRemoteProcess
        process.inputStream.bufferedReader().readText()
        process.waitFor()
        val code = process.exitValue()
        android.util.Log.i("HFXBoost", "shizuku 「" + cmd + "」 -> " + code)
        code == 0
    }
} catch (e: Throwable) {
    android.util.Log.i("HFXBoost", "shizuku 「" + cmd + "」 failed: " + e.message)
    false
}

/** 先 root，再 Shizuku/adb；两条路都不通就当没权限 */
private fun runShell(cmd: String): Boolean = runSu(cmd) || runShizuku(cmd)

private fun buildSlots(): List<BoostSlot> = listOf(
    BoostSlot(
        key = "wifi",
        title = "Wi-Fi",
        reason = "接上一个路由器，能开 4 条通道",
        ready = { ctx -> anyOf(interfaceNames(ctx), "wlan", "wifi") },
        open = { ctx -> startActivityQuietly(ctx, Intent(Settings.ACTION_WIFI_SETTINGS)) },
        kick = {
            runShell("svc wifi enable")
            runShell("cmd wifi set-wifi-enabled enabled")
        },
    ),
    BoostSlot(
        key = "usb",
        title = "USB 数据线",
        reason = "插上线，电脑端能多用 2 条通道",
        ready = { ctx -> anyOf(interfaceNames(ctx), "usb_adb") },
        open = { _ -> false },
        // 线插着但 USB 调试关着的话，有 root / adb 权限就能直接打开
        kick = { runShell("settings put global adb_enabled 1") },
    ),
    BoostSlot(
        key = "rndis",
        title = "USB 网络共享",
        reason = "在「个人热点」里打开，USB 线上再多 2 条通道",
        ready = { ctx -> anyOf(interfaceNames(ctx), "rndis", "usb0", "usb1") },
        open = { ctx ->
            startActivityQuietly(ctx, Intent("android.settings.TETHER_SETTINGS"))
                || startActivityQuietly(ctx, Intent(Settings.ACTION_WIRELESS_SETTINGS))
        },
        kick = {
            runShell("svc usb setFunctions rndis")
            runShell("cmd usb setFunctions rndis")
        },
    ),
    // 蓝牙共享网络（bt-pan）不参与传输了：实测只有 0.2~0.3 MB/s，多轨切分时它是最慢那条，
    // 收尾要等它、还占一个通道名额，得不偿失（NetInterfaces.enumerate 已经把它过滤掉）。
    // 所以这里也不再引导用户去开蓝牙网络共享。
)

/** 还有没有可开的链路（全开了就不弹这个引导） */
internal fun boostHasRoom(ctx: Context): Boolean = buildSlots().any { !it.ready(ctx) }

@Composable
internal fun BoostDialog(
    visible: Boolean,
    onDismiss: () -> Unit,
    onDismissForever: () -> Unit,
) {
    if (!visible) {
        return
    }
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    val colorScheme = MiuixTheme.colorScheme
    // 每秒重新探一次网卡：用户从设置页回来，这里自己就更新了
    var tick by remember { mutableIntStateOf(0) }
    LaunchedEffect(Unit) {
        while (true) {
            delay(1000)
            tick++
        }
    }
    val slots = remember(tick) { buildSlots() }
    val missing = remember(tick) { slots.filter { !it.ready(ctx) } }
    // 进入逐条引导后才开始走流程；只引导还缺的那几条
    var guiding by remember { mutableStateOf(false) }
    var index by remember { mutableIntStateOf(0) }
    var kicked by remember { mutableStateOf(false) }
    if (guiding) {
        LaunchedEffect(guiding, index, tick) {
            if (index < missing.size && missing[index].ready(ctx)) {
                index += 1
            }
        }
    }
    Dialog(
        onDismissRequest = onDismiss,
        properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false),
    ) {
        androidx.compose.foundation.layout.Box(
            modifier = Modifier.fillMaxSize().background(Color.Transparent).clickable { onDismiss() },
            contentAlignment = Alignment.Center,
        ) {
            Column(
                modifier = Modifier.width(330.dp).clip(RoundedCornerShape(24.dp))
                    .background(colorScheme.surfaceContainer).clickable { }
                    .verticalScroll(rememberScrollState())
                    .padding(20.dp),
            ) {
                Text("把通道开满", fontSize = 19.sp, fontWeight = FontWeight.Bold)
                Text(
                    if (guiding) {
                        "一条一条来：点「打开设置」开好，回来这里会自动打勾。"
                    } else {
                        "现在 " + (slots.size - missing.size) + "/" + slots.size + " 条链路可用。" +
                            "都打开的话，多轨快传能同时用上它们，传输更快更抗干扰。"
                    },
                    fontSize = 13.sp,
                    color = colorScheme.onSurfaceVariantSummary,
                    modifier = Modifier.padding(top = 8.dp),
                )
                Spacer(Modifier.height(12.dp))
                if (!guiding) {
                    // 总览：哪条已就绪、哪条还缺
                    slots.forEach { slot ->
                        val ok = slot.ready(ctx)
                        Row(
                            modifier = Modifier.fillMaxWidth().padding(vertical = 5.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Text(if (ok) "✓" else "○", fontSize = 14.sp, color = if (ok) colorScheme.primary else colorScheme.onSurfaceVariantSummary)
                            Column(Modifier.padding(start = 8.dp)) {
                                Text(slot.title, fontSize = 14.sp)
                                Text(
                                    if (ok) "已就绪" else slot.reason,
                                    fontSize = 11.sp,
                                    color = colorScheme.onSurfaceVariantSummary,
                                )
                            }
                        }
                    }
                } else if (index < missing.size) {
                    val slot = missing[index]
                    Text(
                        (index + 1).toString() + "/" + missing.size + " · " + slot.title,
                        fontSize = 15.sp,
                        fontWeight = FontWeight.Bold,
                    )
                    Text(slot.reason, fontSize = 12.sp, color = colorScheme.onSurfaceVariantSummary, modifier = Modifier.padding(top = 6.dp))
                    Text(
                        "开好之后回到这里会自动打勾，也可以点「跳过」。",
                        fontSize = 12.sp,
                        color = colorScheme.onSurfaceVariantSummary,
                        modifier = Modifier.padding(top = 6.dp),
                    )
                    // 到这一条就先用 root / adb 试一遍（试过就不重复试）；
                    // su 会阻塞几秒，必须丢到 IO 线程，否则主线程卡住会 ANR
                    LaunchedEffect(index) {
                        if (!kicked) {
                            kicked = true
                            val slot = missing.getOrNull(index)
                            withContext(Dispatchers.IO) { slot?.kick?.invoke() }
                            tick++
                        }
                    }
                    Row(modifier = Modifier.fillMaxWidth().padding(top = 14.dp), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                        TextButton(
                            text = "跳过",
                            onClick = { index += 1 },
                            modifier = Modifier.weight(1f),
                        )
                        Button(
                            onClick = {
                                if (!slot.open(ctx)) {
                                    index += 1
                                }
                            },
                            modifier = Modifier.weight(1f),
                        ) {
                            Text("打开设置")
                        }
                    }
                } else {
                    Text("完成", fontSize = 15.sp, fontWeight = FontWeight.Bold)
                    Text(
                        "能用上的链路都开了，回到主页看看通道数有没有变多。",
                        fontSize = 12.sp,
                        color = colorScheme.onSurfaceVariantSummary,
                        modifier = Modifier.padding(top = 6.dp),
                    )
                    Row(modifier = Modifier.fillMaxWidth().padding(top = 14.dp)) {
                        Button(onClick = onDismiss, modifier = Modifier.weight(1f)) { Text("好") }
                    }
                }
                if (!guiding) {
                    Row(
                        modifier = Modifier.fillMaxWidth().padding(top = 16.dp),
                        horizontalArrangement = Arrangement.spacedBy(10.dp),
                    ) {
                        // 「不再提示」单独放一行：325dp 宽度里三个按钮挤在一起，
                        // 「不再提示」四个字会被压成两行，看着别扭
                        TextButton(text = "稍后", onClick = onDismiss, modifier = Modifier.weight(1f))
                        Button(
                            onClick = {
                                guiding = true
                                index = 0
                                kicked = true
                                // 有 root / adb 权限就先自动把还缺的链路全推一遍，
                                // 推起来的直接从引导里消失，剩下的再一条条让用户点
                                val todo = missing
                                scope.launch {
                                    withContext(Dispatchers.IO) { todo.forEach { it.kick?.invoke() } }
                                    tick++
                                }
                            },
                            modifier = Modifier.weight(1f),
                        ) {
                            Text("确定")
                        }
                    }
                    TextButton(
                        text = "不再提示",
                        onClick = onDismissForever,
                        modifier = Modifier.fillMaxWidth().padding(top = 4.dp),
                    )
                }
            }
        }
    }
}
