package top.weixiansen574.hybridfilexfer.compose

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.runtime.SideEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.core.view.WindowCompat
import top.weixiansen574.hybridfilexfer.Config
import top.yukonga.miuix.kmp.theme.ColorSchemeMode
import top.yukonga.miuix.kmp.theme.MiuixTheme
import top.yukonga.miuix.kmp.theme.ThemeController

/** 主题下拉的 6 个选项，顺序与 Miuix 的 [ColorSchemeMode] 枚举完全一致。 */
internal val themeModeLabels = listOf(
    "跟随系统",
    "浅色",
    "深色",
    "Monet（跟随系统）",
    "Monet（浅色）",
    "Monet（深色）",
)

internal fun colorSchemeModeOf(index: Int): ColorSchemeMode =
    ColorSchemeMode.entries.getOrElse(index) { ColorSchemeMode.System }

/**
 * 新的 Miuix 界面入口（Compose）。
 * 原来的 MainActivity 保留为"经典界面"，在设置里可以切回去。
 */
class MiuixMainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        setContent {
            val config = remember { Config.getInstance(this) }
            var themeMode by remember { mutableIntStateOf(config.themeMode) }
            // ThemeController 的 colorSchemeMode 对外是只读的（内部 val + mutableStateOf），
            // 所以换主题靠 key 重建控制器；构造很轻，重建即可。
            val controller = remember(themeMode) { ThemeController(colorSchemeMode = colorSchemeModeOf(themeMode)) }

            // 状态栏 / 导航栏图标颜色要跟着"应用自己的主题"走。
            // enableEdgeToEdge 只在启动时按系统主题设一次，应用内强制深色时会出现黑底黑字。
            val dark = when (themeMode) {
                1, 4 -> false
                2, 5 -> true
                else -> isSystemInDarkTheme()
            }
            SideEffect {
                WindowCompat.getInsetsController(window, window.decorView).apply {
                    isAppearanceLightStatusBars = !dark
                    isAppearanceLightNavigationBars = !dark
                }
            }

            MiuixTheme(controller = controller) {
                HfxApp(
                    activity = this,
                    themeMode = themeMode,
                    onThemeModeChange = { index ->
                        themeMode = index
                        config.themeMode = index
                    },
                )
            }
        }
    }
}
