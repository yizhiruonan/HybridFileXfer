package top.weixiansen574.hybridfilexfer.compose

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.foundation.LocalIndication
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Check
import androidx.compose.material.icons.rounded.PhonelinkRing
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import top.weixiansen574.hybridfilexfer.ServerService
import top.yukonga.miuix.kmp.basic.Icon
import top.yukonga.miuix.kmp.basic.Text
import top.yukonga.miuix.kmp.theme.MiuixTheme

/** 当前应用实际拿到的最高权限：root > shizuku > normal */
internal fun privilegeTag(state: UiState): String = when {
    state.rootState == "已授权" -> "root"
    state.shellState == "已授权" -> "shizuku"
    else -> "normal"
}

internal fun modeLabel(mode: Int): String = when (mode) {
    1 -> "ROOT"
    2 -> "ADB"
    else -> "正常"
}

/**
 * 圆角卡片：按压高亮裁成圆角，按下时整卡缩放。
 * Miuix 0.9 的配色语义：页面底色是 surface（浅灰），卡片是 surfaceContainer（白 / 深灰），
 * 卡片必须用 surfaceContainer，否则和背景同色就看不出圆角分组。
 */
@Composable
internal fun SurfaceCard(
    modifier: Modifier = Modifier,
    corner: Dp = 16.dp,
    color: Color = MiuixTheme.colorScheme.surfaceContainer,
    onClick: (() -> Unit)? = null,
    interactionSource: MutableInteractionSource? = null,
    content: @Composable ColumnScope.() -> Unit,
) {
    val shape = remember(corner) { RoundedCornerShape(corner) }
    val haptic = LocalHapticFeedback.current
    val interaction = interactionSource ?: remember { MutableInteractionSource() }
    val pressed by interaction.collectIsPressedAsState()
    val scale by animateFloatAsState(
        targetValue = if (pressed) 0.975f else 1f,
        animationSpec = spring(dampingRatio = 0.55f, stiffness = 700f),
        label = "cardPressScale",
    )
    val clickModifier = if (onClick != null) {
        Modifier
            .clip(shape)
            .clickable(interactionSource = interaction, indication = LocalIndication.current) {
                haptic.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                onClick()
            }
    } else {
        Modifier
    }
    Box(
        modifier = modifier
            .graphicsLayer {
                scaleX = scale
                scaleY = scale
            }
            .clip(shape)
            .then(clickModifier)
            .background(color = color, shape = shape),
    ) {
        Column(content = content)
    }
}

@Composable
internal fun StatusCard(state: UiState, onClick: () -> Unit) {
    val colorScheme = MiuixTheme.colorScheme
    val connected = state.connected
    val background = if (connected) colorScheme.primary.copy(alpha = 0.12f) else colorScheme.surfaceContainer
    val iconColor = if (connected) colorScheme.primary else colorScheme.onSurfaceVariantSummary
    val title = when {
        connected -> "已连接电脑"
        state.state == ServerService.STATE_WAITING -> "等待电脑连接"
        state.state == ServerService.STATE_STARTING -> "正在启动…"
        else -> "未开始接收"
    }
    val subtitle = when {
        connected -> "通道：" + state.connectedNames.joinToString(", ")
        state.state == ServerService.STATE_WAITING -> "插上数据线，或在电脑上运行接收器"
        state.state == ServerService.STATE_STARTING -> "正在启动 IO 服务"
        else -> "点下面的按钮开始等待手机"
    }
    val cardInteraction = remember { MutableInteractionSource() }
    val pressed by cardInteraction.collectIsPressedAsState()
    val watermarkScale by animateFloatAsState(
        targetValue = if (pressed) 1.18f else 1f,
        animationSpec = spring(dampingRatio = 0.5f, stiffness = 650f),
        label = "watermarkScale",
    )
    SurfaceCard(
        modifier = Modifier.fillMaxWidth(),
        corner = 22.dp,
        color = background,
        onClick = onClick,
        interactionSource = cardInteraction,
    ) {
        Box(modifier = Modifier.fillMaxWidth()) {
            Icon(
                imageVector = Icons.Rounded.Check,
                contentDescription = null,
                tint = iconColor.copy(alpha = 0.16f),
                modifier = Modifier
                    .align(Alignment.BottomEnd)
                    .size(130.dp)
                    .graphicsLayer {
                        scaleX = watermarkScale
                        scaleY = watermarkScale
                    },
            )
            Column(modifier = Modifier.fillMaxWidth().padding(20.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Box(
                        modifier = Modifier.size(46.dp).clip(CircleShape),
                        contentAlignment = Alignment.Center,
                    ) {
                        Icon(
                            imageVector = if (connected) Icons.Rounded.Check else Icons.Rounded.PhonelinkRing,
                            contentDescription = null,
                            tint = iconColor,
                            modifier = Modifier.size(34.dp),
                        )
                    }
                    Spacer(Modifier.width(14.dp))
                    Text(title, fontSize = 21.sp, fontWeight = FontWeight.Bold)
                }
                Text(
                    "<" + privilegeTag(state) + ">  " + state.version,
                    fontSize = 13.sp,
                    fontWeight = FontWeight.Medium,
                    color = iconColor,
                    modifier = Modifier.padding(start = 60.dp, top = 2.dp),
                )
                Text(
                    subtitle,
                    fontSize = 13.sp,
                    color = colorScheme.onSurfaceVariantSummary,
                    modifier = Modifier.padding(start = 60.dp, top = 2.dp),
                )
            }
        }
    }
}

@Composable
internal fun SmallTile(modifier: Modifier = Modifier, label: String, value: String, onClick: (() -> Unit)? = null) {
    val colorScheme = MiuixTheme.colorScheme
    SurfaceCard(modifier = modifier, corner = 16.dp, color = colorScheme.surfaceContainer, onClick = onClick) {
        Column(Modifier.padding(16.dp)) {
            Text(label, fontSize = 12.sp, color = colorScheme.onSurfaceVariantSummary)
            Text(value, fontSize = 26.sp, fontWeight = FontWeight.Bold, modifier = Modifier.padding(top = 2.dp))
        }
    }
}

/** preference 行尾的次要文字（Miuix 0.9 用 endActions 取代了 0.4 的 rightText）。 */
@Composable
internal fun ActionText(text: String) {
    Text(
        text = text,
        fontSize = 14.sp,
        color = MiuixTheme.colorScheme.onSurfaceVariantActions,
        maxLines = 1,
        overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
    )
}

@Composable
internal fun InfoRow(label: String, value: String) {
    Column(Modifier.padding(vertical = 6.dp)) {
        Text(label, fontSize = 12.sp, color = MiuixTheme.colorScheme.onSurfaceVariantSummary)
        Text(
            value,
            fontSize = 15.sp,
            modifier = Modifier.padding(top = 2.dp),
            maxLines = 3,
            overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
        )
    }
}
