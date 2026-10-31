package top.weixiansen574.hybridfilexfer.compose

import android.content.Context
import android.content.pm.PackageManager

/**
 * 读取 USB 链路速度（协议等级），按权限从高到低尝试：
 * 1. 直接读内核 sysfs（普通应用）
 * 2. ROOT（su -c）
 * 3. ADB / Shizuku（shell 身份）
 * 4. 都失败则返回空字符串，界面显示"无法读取"
 */
internal object UsbSpeed {
    private const val CACHE_MS = 60_000L
    private var cachedAt = 0L
    private var cached = ""

    fun query(context: Context): String {
        val now = System.currentTimeMillis()
        if (now - cachedAt < CACHE_MS) {
            return cached
        }
        var result = readDirect()
        if (result.isEmpty()) {
            result = execRoot()
        }
        if (result.isEmpty()) {
            result = execShizuku()
        }
        cached = result
        cachedAt = now
        return result
    }

    private fun readDirect(): String {
        return try {
            val udcs = java.io.File("/sys/class/udc").listFiles() ?: return ""
            for (udc in udcs) {
                val file = java.io.File(udc, "current_speed")
                if (file.canRead()) {
                    val text = file.readText().trim()
                    if (text.isNotEmpty()) {
                        return mapSpeed(text)
                    }
                }
            }
            ""
        } catch (_: Exception) {
            ""
        }
    }

    private fun execRoot(): String {
        return try {
            val process = Runtime.getRuntime().exec(
                arrayOf("su", "-c", "cat /sys/class/udc/*/current_speed"),
            )
            val text = process.inputStream.bufferedReader().readText()
            process.waitFor()
            mapSpeed(text.lineSequence().firstOrNull { it.isNotBlank() } ?: "")
        } catch (_: Exception) {
            ""
        }
    }

    private fun execShizuku(): String {
        return try {
            if (!rikka.shizuku.Shizuku.pingBinder()) return ""
            if (rikka.shizuku.Shizuku.checkSelfPermission() != PackageManager.PERMISSION_GRANTED) return ""
            val method = rikka.shizuku.Shizuku::class.java.getDeclaredMethod(
                "newProcess",
                Array<String>::class.java,
                Array<String>::class.java,
                String::class.java,
            )
            method.isAccessible = true
            val process = method.invoke(
                null,
                arrayOf("sh", "-c", "cat /sys/class/udc/*/current_speed"),
                null,
                null,
            ) as rikka.shizuku.ShizukuRemoteProcess
            val text = process.inputStream.bufferedReader().readText()
            process.waitFor()
            mapSpeed(text.lineSequence().firstOrNull { it.isNotBlank() } ?: "")
        } catch (_: Exception) {
            ""
        }
    }

    private fun mapSpeed(raw: String): String {
        val value = raw.trim().lowercase()
        return when {
            value.contains("super") -> "USB 3.x（super-speed）"
            value.contains("high") -> "USB 2.0（high-speed）"
            value.contains("full") -> "USB 1.1（full-speed）"
            value.contains("low") -> "USB 1.0（low-speed）"
            value.isNotEmpty() -> raw.trim()
            else -> ""
        }
    }
}