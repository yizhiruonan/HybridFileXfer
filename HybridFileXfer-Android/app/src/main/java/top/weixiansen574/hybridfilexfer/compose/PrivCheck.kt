package top.weixiansen574.hybridfilexfer.compose

import android.content.pm.PackageManager
import java.util.concurrent.TimeUnit

/**
 * 权限状态检查：
 * - ROOT：执行 su -c id，看是否拿到 uid=0（会触发 SukiSU 授权框）
 * - SHELL / Shizuku：Shizuku 服务是否在跑 + 本应用是否被授权
 */
internal object PrivCheck {
    private const val CACHE_MS = 30_000L
    private var rootCachedAt = 0L
    private var rootCached = ""

    fun rootStatus(): String {
        val now = System.currentTimeMillis()
        if (now - rootCachedAt < CACHE_MS) {
            return rootCached
        }
        val result = try {
            val process = Runtime.getRuntime().exec(arrayOf("su", "-c", "id"))
            if (!process.waitFor(2, TimeUnit.SECONDS)) {
                process.destroy()
                "等待授权"
            } else {
                val output = process.inputStream.bufferedReader().readText()
                if (output.contains("uid=0")) "已授权" else "未授权"
            }
        } catch (_: Exception) {
            "未授权"
        }
        rootCached = result
        rootCachedAt = now
        return result
    }

    fun shizukuStatus(): String {
        return try {
            if (!rikka.shizuku.Shizuku.pingBinder()) {
                "未运行"
            } else if (rikka.shizuku.Shizuku.checkSelfPermission() == PackageManager.PERMISSION_GRANTED) {
                "已授权"
            } else {
                "未授权"
            }
        } catch (_: Exception) {
            "不可用"
        }
    }
}
