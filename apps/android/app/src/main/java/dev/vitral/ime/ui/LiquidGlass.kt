package dev.vitral.ime.ui

import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.animateDpAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectHorizontalDragGestures
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.vitral.ime.theme.VitralTokens as T
import kotlin.math.roundToInt

@Immutable
data class GlassPalette(
    val dark: Boolean,
    val canvas: Color,
    val panelTop: Color,
    val panelBottom: Color,
    val elevated: Color,
    val fieldBg: Color,
    val borderTop: Color,
    val borderBottom: Color,
    val hairline: Color,
    val accent: Color,
    val accentSoft: Color,
    val accentBorder: Color,
    val accentText: Color,
    val textPrimary: Color,
    val textSecondary: Color,
    val success: Color,
    val successSoft: Color,
    val warning: Color,
    val warningSoft: Color,
    val danger: Color,
    val dangerSoft: Color,
)

fun glassPalette(dark: Boolean): GlassPalette =
    if (dark) {
        GlassPalette(
            dark = true,
            canvas = Color(0xFF0B1019),
            panelTop = Color(0xC21B2335),
            panelBottom = Color(0xB8131926),
            elevated = Color(0x0DFFFFFF),
            fieldBg = Color(0x14FFFFFF),
            borderTop = Color(0x36FFFFFF),
            borderBottom = Color(0x12FFFFFF),
            hairline = Color(0x17FFFFFF),
            accent = T.Dark.accentPrimary,
            accentSoft = T.Dark.accentSoft,
            accentBorder = Color(0x4D0EA5E9),
            accentText = T.Dark.accentText,
            textPrimary = T.Dark.textPrimary,
            textSecondary = T.Dark.textSecondary,
            success = Color(0xFF34D399),
            successSoft = Color(0x2910B981),
            warning = Color(0xFFFBBF24),
            warningSoft = Color(0x29F59E0B),
            danger = Color(0xFFF87171),
            dangerSoft = Color(0x26EF4444),
        )
    } else {
        GlassPalette(
            dark = false,
            canvas = Color(0xFFEAF1F8),
            panelTop = Color(0xE8FFFFFF),
            panelBottom = Color(0xD4F6FAFE),
            elevated = Color(0x0B0F172A),
            fieldBg = Color(0xE0FFFFFF),
            borderTop = Color(0xF5FFFFFF),
            borderBottom = Color(0x180F172A),
            hairline = Color(0x140F172A),
            accent = T.Light.accentPrimary,
            accentSoft = T.Light.accentSoft,
            accentBorder = Color(0x400284C7),
            accentText = T.Light.accentText,
            textPrimary = T.Light.textPrimary,
            textSecondary = T.Light.textSecondary,
            success = Color(0xFF059669),
            successSoft = Color(0x1F059669),
            warning = Color(0xFFD97706),
            warningSoft = Color(0x21D97706),
            danger = Color(0xFFDC2626),
            dangerSoft = Color(0x1CDC2626),
        )
    }

val LocalGlass = staticCompositionLocalOf { glassPalette(true) }

/** Full-screen Liquid Glass ambient backdrop with soft glowing sky & indigo orbs. */
@Composable
fun LiquidGlassBackdrop(modifier: Modifier = Modifier) {
    val g = LocalGlass.current
    Canvas(modifier.fillMaxSize().background(g.canvas)) {
        val w = size.width
        val h = size.height
        val orbAlphaA = if (g.dark) 0.24f else 0.18f
        val orbAlphaB = if (g.dark) 0.20f else 0.13f
        drawCircle(
            brush =
                Brush.radialGradient(
                    colors = listOf(g.accent.copy(alpha = orbAlphaA), Color.Transparent),
                    center = Offset(w * 0.88f, h * 0.08f),
                    radius = w * 0.85f,
                ),
            radius = w * 0.85f,
            center = Offset(w * 0.88f, h * 0.08f),
        )
        drawCircle(
            brush =
                Brush.radialGradient(
                    colors = listOf(Color(0xFF6366F1).copy(alpha = orbAlphaB), Color.Transparent),
                    center = Offset(w * 0.14f, h * 0.72f),
                    radius = w * 0.80f,
                ),
            radius = w * 0.80f,
            center = Offset(w * 0.14f, h * 0.72f),
        )
    }
}

/** Signature Vitral Liquid Glass panel with directional specular edge highlight. */
@Composable
fun GlassPanel(
    modifier: Modifier = Modifier,
    radius: Dp = 20.dp,
    padding: PaddingValues = PaddingValues(18.dp),
    tinted: Boolean = false,
    content: @Composable ColumnScope.() -> Unit,
) {
    val g = LocalGlass.current
    val shape = RoundedCornerShape(radius)
    val bgBrush =
        if (tinted) {
            Brush.linearGradient(
                colors =
                    if (g.dark) listOf(Color(0xCC18263C), Color(0xC2141B2B))
                    else listOf(Color(0xF2FFFFFF), Color(0xE0EDF5FD))
            )
        } else {
            Brush.verticalGradient(listOf(g.panelTop, g.panelBottom))
        }
    val borderBrush = Brush.verticalGradient(listOf(g.borderTop, g.borderBottom))
    Column(
        modifier =
            modifier
                .fillMaxWidth()
                .clip(shape)
                .background(bgBrush)
                .border(1.dp, borderBrush, shape)
                .padding(padding),
        content = content,
    )
}

@Composable
fun GlassSectionHeader(
    title: String,
    subtitle: String? = null,
    trailing: (@Composable () -> Unit)? = null,
) {
    val g = LocalGlass.current
    Row(
        Modifier.fillMaxWidth().padding(bottom = 10.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.Top,
    ) {
        Column(Modifier.weight(1f).padding(end = if (trailing != null) 10.dp else 0.dp)) {
            Text(
                title,
                fontSize = 15.5.sp,
                fontWeight = FontWeight.SemiBold,
                letterSpacing = (-0.2).sp,
                color = g.textPrimary,
            )
            if (!subtitle.isNullOrBlank()) {
                Spacer(Modifier.height(3.dp))
                Text(
                    subtitle,
                    fontSize = 12.5.sp,
                    lineHeight = 17.5.sp,
                    color = g.textSecondary,
                )
            }
        }
        trailing?.invoke()
    }
}

@Composable
fun GlassHairline(modifier: Modifier = Modifier) {
    val g = LocalGlass.current
    Box(modifier.fillMaxWidth().height(1.dp).background(g.hairline))
}

@Composable
fun GlassPillBadge(
    text: String,
    color: Color,
    backgroundColor: Color,
    modifier: Modifier = Modifier,
) {
    Box(
        modifier
            .clip(CircleShape)
            .background(backgroundColor)
            .border(0.8.dp, color.copy(alpha = 0.28f), CircleShape)
            .padding(horizontal = 9.dp, vertical = 3.5.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            text,
            fontSize = 11.sp,
            fontWeight = FontWeight.SemiBold,
            color = color,
        )
    }
}

/** Slim custom Liquid Glass toggle switch matching desktop .switch (44x24dp). */
@Composable
fun GlassSwitch(
    checked: Boolean,
    onCheckedChange: (Boolean) -> Unit,
    contentDesc: String,
    enabled: Boolean = true,
) {
    val g = LocalGlass.current
    val trackColor by
        animateColorAsState(
            targetValue =
                when {
                    checked -> g.accent
                    g.dark -> Color(0x1EFFFFFF)
                    else -> Color(0x200F172A)
                },
            animationSpec = tween(160),
            label = "switchTrack",
        )
    val thumbOffset by
        animateDpAsState(
            targetValue = if (checked) 20.dp else 2.dp,
            animationSpec = tween(160),
            label = "switchThumb",
        )
    Box(
        modifier =
            Modifier.size(width = 44.dp, height = 24.dp)
                .clip(CircleShape)
                .background(trackColor.copy(alpha = if (enabled) 1f else 0.4f))
                .border(
                    1.dp,
                    if (checked) Color.Transparent else g.hairline,
                    CircleShape,
                )
                .clickable(
                    enabled = enabled,
                    role = Role.Switch,
                    interactionSource = remember { MutableInteractionSource() },
                    indication = null,
                    onClick = { onCheckedChange(!checked) },
                )
                .semantics { contentDescription = contentDesc },
        contentAlignment = Alignment.CenterStart,
    ) {
        Box(
            Modifier.offset(x = thumbOffset)
                .size(20.dp)
                .shadow(2.dp, CircleShape)
                .clip(CircleShape)
                .background(if (checked) Color.White else if (g.dark) Color(0xFFE2E8F0) else Color.White)
        )
    }
}

@Composable
fun GlassToggleRow(
    title: String,
    subtitle: String? = null,
    checked: Boolean,
    onCheckedChange: (Boolean) -> Unit,
    showDivider: Boolean = true,
    enabled: Boolean = true,
) {
    val g = LocalGlass.current
    if (showDivider) GlassHairline()
    Row(
        Modifier.fillMaxWidth()
            .clickable(enabled = enabled) { onCheckedChange(!checked) }
            .padding(vertical = 12.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f).padding(end = 14.dp)) {
            Text(
                title,
                fontSize = 14.sp,
                fontWeight = FontWeight.Medium,
                color = if (enabled) g.textPrimary else g.textSecondary,
            )
            if (!subtitle.isNullOrBlank()) {
                Spacer(Modifier.height(2.dp))
                Text(
                    subtitle,
                    fontSize = 12.sp,
                    lineHeight = 16.5.sp,
                    color = g.textSecondary,
                )
            }
        }
        GlassSwitch(checked = checked, onCheckedChange = onCheckedChange, contentDesc = title, enabled = enabled)
    }
}

/** Segmented capsule selector matching Desktop .segmented control. */
@Composable
fun <TVal> GlassSegmentedControl(
    options: List<Pair<TVal, String>>,
    selected: TVal,
    onSelect: (TVal) -> Unit,
    modifier: Modifier = Modifier,
) {
    val g = LocalGlass.current
    val shape = RoundedCornerShape(12.dp)
    Row(
        modifier
            .fillMaxWidth()
            .clip(shape)
            .background(g.elevated)
            .border(1.dp, g.hairline, shape)
            .padding(3.dp),
        horizontalArrangement = Arrangement.spacedBy(3.dp),
    ) {
        options.forEach { (value, label) ->
            val isSelected = value == selected
            val bg by
                animateColorAsState(
                    if (isSelected) g.accent else Color.Transparent,
                    tween(150),
                    label = "segBg",
                )
            Box(
                Modifier.weight(1f)
                    .clip(RoundedCornerShape(9.dp))
                    .background(bg)
                    .clickable { onSelect(value) }
                    .padding(vertical = 8.dp, horizontal = 6.dp),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    label,
                    fontSize = 12.5.sp,
                    fontWeight = if (isSelected) FontWeight.SemiBold else FontWeight.Medium,
                    color = if (isSelected) Color.White else g.textSecondary,
                    maxLines = 1,
                )
            }
        }
    }
}

/** Custom sleek Liquid Glass slider (6dp track + crisp circular thumb) replacing chunky M3 Slider. */
@Composable
fun GlassSlider(
    value: Float,
    valueRange: ClosedFloatingPointRange<Float>,
    onValueChange: (Float) -> Unit,
    onValueChangeFinished: () -> Unit = {},
    contentDesc: String,
    modifier: Modifier = Modifier,
) {
    val g = LocalGlass.current
    var widthPx by remember { mutableIntStateOf(1) }
    val fraction =
        ((value - valueRange.start) / (valueRange.endInclusive - valueRange.start)).coerceIn(0f, 1f)

    fun updateAt(x: Float) {
        val f = (x / widthPx.coerceAtLeast(1)).coerceIn(0f, 1f)
        onValueChange(valueRange.start + f * (valueRange.endInclusive - valueRange.start))
    }

    Box(
        modifier =
            modifier
                .fillMaxWidth()
                .height(34.dp)
                .onSizeChanged { widthPx = it.width }
                .pointerInput(valueRange) {
                    detectTapGestures { offset ->
                        updateAt(offset.x)
                        onValueChangeFinished()
                    }
                }
                .pointerInput(valueRange) {
                    detectHorizontalDragGestures(
                        onDragEnd = { onValueChangeFinished() },
                        onDragCancel = { onValueChangeFinished() },
                    ) { change, _ ->
                        change.consume()
                        updateAt(change.position.x)
                    }
                }
                .semantics { contentDescription = contentDesc },
        contentAlignment = Alignment.CenterStart,
    ) {
        Canvas(Modifier.fillMaxWidth().height(26.dp)) {
            val pad = 11.dp.toPx()
            val trackH = 6.dp.toPx()
            val cy = size.height / 2f
            val usable = (size.width - pad * 2f).coerceAtLeast(1f)
            val thumbX = pad + usable * fraction

            drawRoundRect(
                color = if (g.dark) Color(0x1EFFFFFF) else Color(0x1C0F172A),
                topLeft = Offset(pad, cy - trackH / 2f),
                size = Size(usable, trackH),
                cornerRadius = CornerRadius(trackH / 2f, trackH / 2f),
            )
            drawRoundRect(
                brush = Brush.horizontalGradient(listOf(g.accent.copy(alpha = 0.78f), g.accent)),
                topLeft = Offset(pad, cy - trackH / 2f),
                size = Size((thumbX - pad).coerceAtLeast(trackH), trackH),
                cornerRadius = CornerRadius(trackH / 2f, trackH / 2f),
            )
            // Soft outer glow + crisp white thumb with accent ring
            drawCircle(
                color = g.accent.copy(alpha = 0.24f),
                radius = 12.dp.toPx(),
                center = Offset(thumbX, cy),
            )
            drawCircle(
                color = Color.White,
                radius = 8.5.dp.toPx(),
                center = Offset(thumbX, cy),
            )
            drawCircle(
                color = g.accent,
                radius = 8.5.dp.toPx(),
                center = Offset(thumbX, cy),
                style = Stroke(width = 2.2.dp.toPx()),
            )
        }
    }
}

enum class GlassButtonStyle {
    Primary,
    AccentSoft,
    Ghost,
    DangerGhost,
}

@Composable
fun GlassButton(
    text: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    style: GlassButtonStyle = GlassButtonStyle.Ghost,
    enabled: Boolean = true,
    compact: Boolean = false,
) {
    val g = LocalGlass.current
    val shape = RoundedCornerShape(if (compact) 9.dp else 12.dp)
    val bg =
        when (style) {
            GlassButtonStyle.Primary -> g.accent
            GlassButtonStyle.AccentSoft -> g.accentSoft
            GlassButtonStyle.Ghost -> g.fieldBg
            GlassButtonStyle.DangerGhost -> g.dangerSoft
        }
    val border =
        when (style) {
            GlassButtonStyle.Primary -> Color(0x33FFFFFF)
            GlassButtonStyle.AccentSoft -> g.accentBorder
            GlassButtonStyle.Ghost -> g.hairline
            GlassButtonStyle.DangerGhost -> g.danger.copy(alpha = 0.35f)
        }
    val fg =
        when (style) {
            GlassButtonStyle.Primary -> Color.White
            GlassButtonStyle.AccentSoft -> g.accentText
            GlassButtonStyle.Ghost -> g.textPrimary
            GlassButtonStyle.DangerGhost -> g.danger
        }
    Box(
        modifier
            .clip(shape)
            .background(bg.copy(alpha = if (enabled) bg.alpha else bg.alpha * 0.45f))
            .border(1.dp, border, shape)
            .clickable(enabled = enabled, onClick = onClick)
            .padding(
                horizontal = if (compact) 11.dp else 15.dp,
                vertical = if (compact) 7.dp else 10.dp,
            ),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            text,
            fontSize = if (compact) 12.sp else 13.sp,
            fontWeight = FontWeight.SemiBold,
            color = fg.copy(alpha = if (enabled) 1f else 0.45f),
            maxLines = 1,
        )
    }
}

@Composable
fun GlassTextField(
    value: String,
    onValueChange: (String) -> Unit,
    label: String,
    placeholder: String = "",
    secret: Boolean = false,
    singleLine: Boolean = true,
    minLines: Int = 1,
    modifier: Modifier = Modifier,
) {
    val g = LocalGlass.current
    val shape = RoundedCornerShape(12.dp)
    Column(modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(5.dp)) {
        if (label.isNotEmpty()) {
            Text(
                label,
                fontSize = 12.sp,
                fontWeight = FontWeight.Medium,
                color = g.textSecondary,
            )
        }
        Box(
            Modifier.fillMaxWidth()
                .clip(shape)
                .background(g.fieldBg)
                .border(1.dp, g.hairline, shape)
                .padding(horizontal = 13.dp, vertical = 11.dp)
        ) {
            if (value.isEmpty() && placeholder.isNotEmpty()) {
                Text(
                    placeholder,
                    fontSize = 13.5.sp,
                    color = g.textSecondary.copy(alpha = 0.7f),
                )
            }
            BasicTextField(
                value = value,
                onValueChange = onValueChange,
                singleLine = singleLine,
                minLines = minLines,
                textStyle =
                    TextStyle(
                        color = g.textPrimary,
                        fontSize = 13.5.sp,
                        lineHeight = 19.sp,
                        fontFamily = if (secret) FontFamily.Monospace else FontFamily.Default,
                    ),
                cursorBrush = SolidColor(g.accent),
                visualTransformation =
                    if (secret) PasswordVisualTransformation() else VisualTransformation.None,
                modifier = Modifier.fillMaxWidth(),
            )
        }
    }
}

/** Floating Liquid Glass bottom navigation capsule with crisp 1.75px custom vector glyphs. */
@Composable
fun GlassBottomNavBar(
    selectedIndex: Int,
    onSelect: (Int) -> Unit,
    modifier: Modifier = Modifier,
) {
    val g = LocalGlass.current
    val items = listOf("键盘", "词库同步", "模型", "AI与工具")
    val capsuleShape = RoundedCornerShape(24.dp)
    val bg =
        if (g.dark) Brush.verticalGradient(listOf(Color(0xF7192235), Color(0xFA121826)))
        else Brush.verticalGradient(listOf(Color(0xFAFFFFFF), Color(0xF7F1F6FC)))
    val border = Brush.verticalGradient(listOf(g.borderTop, g.borderBottom))
    val scrim =
        Brush.verticalGradient(
            listOf(Color.Transparent, g.canvas.copy(alpha = 0.82f), g.canvas)
        )

    Box(
        modifier
            .fillMaxWidth()
            .background(scrim)
            .navigationBarsPadding()
            .padding(horizontal = 16.dp, vertical = 10.dp),
        contentAlignment = Alignment.Center,
    ) {
        Row(
            Modifier.fillMaxWidth()
                .shadow(if (g.dark) 18.dp else 12.dp, capsuleShape)
                .clip(capsuleShape)
                .background(bg)
                .border(1.dp, border, capsuleShape)
                .padding(horizontal = 6.dp, vertical = 6.dp),
            horizontalArrangement = Arrangement.spacedBy(4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            items.forEachIndexed { idx, label ->
                val active = idx == selectedIndex
                val pillBg by
                    animateColorAsState(
                        if (active) g.accentSoft else Color.Transparent,
                        tween(160),
                        label = "navBg",
                    )
                val fg by
                    animateColorAsState(
                        if (active) g.accentText else g.textSecondary,
                        tween(160),
                        label = "navFg",
                    )
                Column(
                    Modifier.weight(1f)
                        .clip(RoundedCornerShape(18.dp))
                        .background(pillBg)
                        .border(
                            0.8.dp,
                            if (active) g.accentBorder else Color.Transparent,
                            RoundedCornerShape(18.dp),
                        )
                        .clickable { onSelect(idx) }
                        .padding(vertical = 7.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(3.dp),
                ) {
                    NavGlyph(idx, fg)
                    Text(
                        label,
                        fontSize = 11.sp,
                        fontWeight = if (active) FontWeight.SemiBold else FontWeight.Medium,
                        color = fg,
                        maxLines = 1,
                    )
                }
            }
        }
    }
}

@Composable
private fun NavGlyph(index: Int, color: Color) {
    Canvas(Modifier.size(19.dp)) {
        val stroke =
            Stroke(
                width = 1.75.dp.toPx(),
                cap = StrokeCap.Round,
                join = StrokeJoin.Round,
            )
        val w = size.width
        val h = size.height
        when (index) {
            0 -> {
                // Keyboard glyph
                drawRoundRect(
                    color = color,
                    topLeft = Offset(w * 0.08f, h * 0.20f),
                    size = Size(w * 0.84f, h * 0.60f),
                    cornerRadius = CornerRadius(3.dp.toPx(), 3.dp.toPx()),
                    style = stroke,
                )
                drawLine(color, Offset(w * 0.28f, h * 0.60f), Offset(w * 0.72f, h * 0.60f), stroke.width, StrokeCap.Round)
                for (x in listOf(0.26f, 0.42f, 0.58f, 0.74f)) {
                    drawCircle(color, 1.1.dp.toPx(), Offset(w * x, h * 0.40f))
                }
            }
            1 -> {
                // Cloud / Sync glyph
                drawCircle(color, w * 0.22f, Offset(w * 0.46f, h * 0.44f), style = stroke)
                drawRoundRect(
                    color = color,
                    topLeft = Offset(w * 0.14f, h * 0.48f),
                    size = Size(w * 0.72f, h * 0.30f),
                    cornerRadius = CornerRadius(h * 0.15f, h * 0.15f),
                    style = stroke,
                )
            }
            2 -> {
                // Model Layers glyph
                val p1 = androidx.compose.ui.graphics.Path().apply {
                    moveTo(w * 0.50f, h * 0.14f)
                    lineTo(w * 0.86f, h * 0.34f)
                    lineTo(w * 0.50f, h * 0.54f)
                    lineTo(w * 0.14f, h * 0.34f)
                    close()
                }
                drawPath(p1, color, style = stroke)
                val p2 = androidx.compose.ui.graphics.Path().apply {
                    moveTo(w * 0.16f, h * 0.56f)
                    lineTo(w * 0.50f, h * 0.74f)
                    lineTo(w * 0.84f, h * 0.56f)
                }
                drawPath(p2, color, style = stroke)
                val p3 = androidx.compose.ui.graphics.Path().apply {
                    moveTo(w * 0.16f, h * 0.73f)
                    lineTo(w * 0.50f, h * 0.90f)
                    lineTo(w * 0.84f, h * 0.73f)
                }
                drawPath(p3, color, style = stroke)
            }
            else -> {
                // Spark / AI & Voice glyph
                val p = androidx.compose.ui.graphics.Path().apply {
                    moveTo(w * 0.46f, h * 0.12f)
                    lineTo(w * 0.55f, h * 0.41f)
                    lineTo(w * 0.84f, h * 0.50f)
                    lineTo(w * 0.55f, h * 0.59f)
                    lineTo(w * 0.46f, h * 0.88f)
                    lineTo(w * 0.37f, h * 0.59f)
                    lineTo(w * 0.08f, h * 0.50f)
                    lineTo(w * 0.37f, h * 0.41f)
                    close()
                }
                drawPath(p, color, style = stroke)
            }
        }
    }
}

@Composable
fun GlassPresetChips(
    presets: List<Int>,
    current: Int,
    onSelect: (Int) -> Unit,
) {
    val g = LocalGlass.current
    Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
        presets.forEach { v ->
            val active = v == current
            Box(
                Modifier.clip(RoundedCornerShape(8.dp))
                    .background(if (active) g.accentSoft else g.elevated)
                    .border(
                        1.dp,
                        if (active) g.accentBorder else g.hairline,
                        RoundedCornerShape(8.dp),
                    )
                    .clickable { onSelect(v) }
                    .padding(horizontal = 9.dp, vertical = 4.dp)
            ) {
                Text(
                    "$v",
                    fontSize = 11.5.sp,
                    fontWeight = if (active) FontWeight.SemiBold else FontWeight.Medium,
                    fontFamily = FontFamily.Monospace,
                    color = if (active) g.accentText else g.textSecondary,
                )
            }
        }
    }
}
