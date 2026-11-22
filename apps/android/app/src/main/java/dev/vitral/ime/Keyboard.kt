package dev.vitral.ime

import android.content.Context
import android.graphics.Canvas
import android.graphics.DashPathEffect
import android.graphics.LinearGradient
import android.graphics.Paint
import android.graphics.Path
import android.graphics.RadialGradient
import android.graphics.RectF
import android.graphics.Shader
import android.graphics.Typeface
import android.os.SystemClock
import android.provider.Settings
import android.view.HapticFeedbackConstants
import android.view.MotionEvent
import android.view.View
import dev.vitral.ime.theme.VitralTokens as T
import kotlin.math.abs
import kotlin.math.min
import kotlin.math.sin

internal fun Context.dp(value: Int) = (resources.displayMetrics.density * value + .5f).toInt()

internal fun Context.dpF(value: Float) = resources.displayMetrics.density * value

internal val Context.darkKeyboard: Boolean
    get() =
        resources.configuration.uiMode and android.content.res.Configuration.UI_MODE_NIGHT_MASK ==
            android.content.res.Configuration.UI_MODE_NIGHT_YES

internal val rows = listOf("qwertyuiop", "asdfghjkl", "zxcvbnm")
private val alternates = listOf("1234567890", "@#$%&-+()", "!?;:'/.")
private val finals =
    mapOf(
        'q' to "iu",
        'w' to "ei",
        'r' to "uan",
        't' to "üe",
        'y' to "un",
        'u' to "sh",
        'i' to "ch",
        'o' to "uo",
        'p' to "ie",
        's' to "ong",
        'd' to "ai",
        'f' to "en",
        'g' to "eng",
        'h' to "ang",
        'j' to "an",
        'k' to "ing",
        'l' to "iang",
        'z' to "ou",
        'x' to "ia",
        'c' to "ao",
        'v' to "ü",
        'b' to "in",
        'n' to "iao",
        'm' to "ian",
    )

internal enum class KeyRole {
    PRIMARY,
    FUNCTION,
    ACCENT,
}

internal enum class SublabelMode {
    DUAL,    // top-left symbol + bottom-right xiaohe final
    SYMBOLS, // swipe-up symbols only
    CLEAN,   // zen pure letters
}

internal class Key(
    ctx: Context,
    private val label: String,
    private val flypy: Boolean,
    private val upper: String,
    private val action: (String) -> Unit,
    private val voice: ((Boolean) -> Unit)? = null,
    private val role: KeyRole = KeyRole.PRIMARY,
    private val displayLabel: String = label,
    private val shiftState: Int = 0, // 0=off, 1=single, 2=capsLock
    private val spaceHint: String = "",
    private val darkOverride: Boolean? = null,
    private val sublabelMode: SublabelMode = SublabelMode.DUAL,
) : View(ctx) {
    private val regularTypeface = Typeface.create("sans-serif", Typeface.NORMAL)
    private val mediumTypeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
    private val boldTypeface = Typeface.create("sans-serif-medium", Typeface.BOLD)
    private val paint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val glowPaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val strokePaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeCap = Paint.Cap.ROUND
            strokeJoin = Paint.Join.ROUND
        }
    private val borderPaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = ctx.resources.displayMetrics.density * 0.85f
        }
    private val keyRect = RectF()
    private val shadowRect = RectF()
    private var downX = 0f
    private var downY = 0f
    private var swipeUpActive = false
    private var spaceScrubbing = false
    private var spaceAnchorX = 0f
    private var recording = false
    private var repeatingDelete = false
    private var clearAllPending = false
    private var clearAllArmed = false
    private var clearAllSwipeUp = false
    private var pressedAt = 0L
    private var releasedAt = 0L

    private val startVoice = Runnable {
        if (isPressed) {
            recording = true
            voice?.invoke(true)
            invalidate()
        }
    }

    private val repeatDelete =
        object : Runnable {
            override fun run() {
                if (!isPressed || clearAllArmed) return
                repeatingDelete = true
                action("⌫")
                postDelayed(this, 65L)
            }
        }

    private val armClearAll = Runnable {
        if (isPressed && clearAllPending && label == "⌫") {
            clearAllPending = false
            clearAllArmed = true
            removeCallbacks(repeatDelete)
            performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
            invalidate()
        }
    }

    init {
        contentDescription = displayLabel.ifEmpty { label }
        isFocusable = true
        isClickable = true
        setOnClickListener { action(label) }
    }

    private fun drawCenteredText(
        c: Canvas,
        s: String,
        x: Float,
        y: Float,
        sizeDp: Float,
        color: Int,
        align: Paint.Align,
        bold: Boolean = false,
        medium: Boolean = false,
        alpha: Int = 255,
    ) {
        paint.shader = null
        paint.style = Paint.Style.FILL
        paint.color = color
        paint.alpha = alpha
        paint.typeface =
            when {
                bold -> boldTypeface
                medium -> mediumTypeface
                else -> regularTypeface
            }
        paint.textSize = context.dpF(sizeDp)
        paint.textAlign = align
        c.drawText(s, x, y - (paint.descent() + paint.ascent()) / 2f, paint)
        paint.alpha = 255
    }

    private fun drawShiftGlyph(c: Canvas, cx: Float, cy: Float, color: Int) {
        val s = min(width, height) * 0.19f
        val p =
            Path().apply {
                moveTo(cx, cy - s)
                lineTo(cx + s * 0.92f, cy + s * 0.04f)
                lineTo(cx + s * 0.40f, cy + s * 0.04f)
                lineTo(cx + s * 0.40f, cy + s * 0.80f)
                lineTo(cx - s * 0.40f, cy + s * 0.80f)
                lineTo(cx - s * 0.40f, cy + s * 0.04f)
                lineTo(cx - s * 0.92f, cy + s * 0.04f)
                close()
            }
        if (shiftState > 0) {
            paint.shader = null
            paint.style = Paint.Style.FILL
            paint.color = color
            c.drawPath(p, paint)
            if (shiftState == 2) {
                strokePaint.color = color
                strokePaint.strokeWidth = context.dpF(1.85f)
                c.drawLine(cx - s * 0.52f, cy + s * 1.15f, cx + s * 0.52f, cy + s * 1.15f, strokePaint)
            }
        } else {
            strokePaint.color = color
            strokePaint.strokeWidth = context.dpF(1.65f)
            c.drawPath(p, strokePaint)
        }
    }

    private fun drawBackspaceGlyph(c: Canvas, cx: Float, cy: Float, color: Int) {
        val w = min(width, height) * 0.25f
        val h = w * 0.66f
        val leftTip = cx - w
        val boxLeft = cx - w * 0.40f
        val right = cx + w * 0.92f
        val p =
            Path().apply {
                moveTo(leftTip, cy)
                lineTo(boxLeft, cy - h)
                lineTo(right, cy - h)
                lineTo(right, cy + h)
                lineTo(boxLeft, cy + h)
                close()
            }
        strokePaint.color = color
        strokePaint.strokeWidth = context.dpF(1.6f)
        c.drawPath(p, strokePaint)
        val xMid = (boxLeft + right) * 0.52f
        val xr = h * 0.40f
        c.drawLine(xMid - xr, cy - xr, xMid + xr, cy + xr, strokePaint)
        c.drawLine(xMid + xr, cy - xr, xMid - xr, cy + xr, strokePaint)
    }

    private fun drawReturnGlyph(c: Canvas, cx: Float, cy: Float, color: Int) {
        val s = min(width, height) * 0.20f
        strokePaint.color = color
        strokePaint.strokeWidth = context.dpF(1.9f)
        val p =
            Path().apply {
                moveTo(cx + s * 0.82f, cy - s * 0.52f)
                lineTo(cx + s * 0.82f, cy + s * 0.12f)
                lineTo(cx - s * 0.70f, cy + s * 0.12f)
            }
        c.drawPath(p, strokePaint)
        c.drawLine(cx - s * 0.70f, cy + s * 0.12f, cx - s * 0.24f, cy - s * 0.34f, strokePaint)
        c.drawLine(cx - s * 0.70f, cy + s * 0.12f, cx - s * 0.24f, cy + s * 0.58f, strokePaint)
    }

    private fun drawMicDot(c: Canvas, cx: Float, cy: Float, color: Int) {
        val r = context.dpF(3.0f)
        strokePaint.color = color
        strokePaint.alpha = 115
        strokePaint.strokeWidth = context.dpF(1.3f)
        c.drawRoundRect(cx - r * 0.65f, cy - r * 1.25f, cx + r * 0.65f, cy + r * 0.35f, r, r, strokePaint)
        c.drawArc(cx - r * 1.25f, cy - r * 0.55f, cx + r * 1.25f, cy + r * 0.95f, 0f, 180f, false, strokePaint)
        c.drawLine(cx, cy + r * 0.95f, cx, cy + r * 1.45f, strokePaint)
        strokePaint.alpha = 255
    }

    override fun onDraw(c: Canvas) {
        val dark = darkOverride ?: context.darkKeyboard
        val padX = context.dpF(2.2f)
        val padY = context.dpF(3.6f)
        val r = context.dpF(T.Radius.mobileKeycapPx.toFloat())

        val now = SystemClock.uptimeMillis()
        val glowStrength =
            when {
                isPressed || recording -> 1f
                releasedAt > 0L && now - releasedAt < T.Motion.durationFastMs ->
                    1f - (now - releasedAt).toFloat() / T.Motion.durationFastMs
                else -> 0f
            }
        val pressDepth = if (isPressed) context.dpF(0.6f) else 0f
        keyRect.set(padX, padY + pressDepth, width - padX, height - padY + pressDepth)

        // Soft bottom shadow for liquid-glass keycap elevation
        if (!isPressed) {
            shadowRect.set(
                padX + context.dpF(0.4f),
                padY + context.dpF(1.6f),
                width - padX - context.dpF(0.4f),
                height - padY + context.dpF(1.6f),
            )
            paint.shader = null
            paint.style = Paint.Style.FILL
            paint.color =
                when {
                    role == KeyRole.ACCENT -> if (dark) 0x4D0EA5E9 else 0x380284C7
                    dark -> 0x45000000
                    else -> 0x180F172A
                }
            c.drawRoundRect(shadowRect, r, r, paint)
        }

        val shiftActive = label == "⇧" && shiftState > 0
        val clearAllActive = label == "⌫" && clearAllArmed

        // Vertical liquid-glass keycap fill (gentle top-to-bottom luminance instead of flat box)
        val topFill: Int
        val bottomFill: Int
        when {
            clearAllActive && clearAllSwipeUp -> {
                topFill = 0xFFEF4444.toInt()
                bottomFill = 0xFFDC2626.toInt()
            }
            clearAllActive -> {
                topFill = if (dark) 0x55EF4444 else 0x38EF4444
                bottomFill = if (dark) 0x38DC2626 else 0x24DC2626
            }
            role == KeyRole.ACCENT -> {
                topFill = if (dark) 0xFF19B5FA.toInt() else 0xFF0A93D9.toInt()
                bottomFill = (if (dark) T.Dark.accentPrimaryArgb else T.Light.accentPrimaryArgb).toInt()
            }
            shiftActive -> {
                topFill = (if (dark) T.Dark.accentSoftArgb else T.Light.accentSoftArgb).toInt()
                bottomFill = topFill
            }
            role == KeyRole.FUNCTION -> {
                topFill = if (dark) 0x10FFFFFF else 0x140F172A
                bottomFill = (if (dark) T.Dark.surfaceKeyFunctionArgb else T.Light.surfaceKeyFunctionArgb).toInt()
            }
            else -> {
                topFill = if (dark) 0x20FFFFFF else 0xF5FFFFFF.toInt()
                bottomFill = if (dark) 0x12FFFFFF else 0xDCFFFFFF.toInt()
            }
        }
        paint.style = Paint.Style.FILL
        paint.shader =
            LinearGradient(
                0f,
                keyRect.top,
                0f,
                keyRect.bottom,
                topFill,
                bottomFill,
                Shader.TileMode.CLAMP,
            )
        c.drawRoundRect(keyRect, r, r, paint)
        paint.shader = null

        // In-key liquid cyan specular bloom when pressed or decaying (150ms)
        if (glowStrength > 0.01f) {
            val baseGlow =
                (if (dark) T.Dark.surfaceKeyPressedGlowArgb else T.Light.surfaceKeyPressedGlowArgb).toInt()
            val alpha = (((baseGlow ushr 24) and 0xFF) * glowStrength).toInt().coerceIn(0, 255)
            val centerColor = (alpha shl 24) or (baseGlow and 0x00FFFFFF)
            val edgeColor = ((alpha * 0.45f).toInt() shl 24) or (baseGlow and 0x00FFFFFF)
            glowPaint.style = Paint.Style.FILL
            glowPaint.shader =
                RadialGradient(
                    keyRect.centerX(),
                    keyRect.centerY(),
                    (min(keyRect.width(), keyRect.height()) * 0.85f).coerceAtLeast(1f),
                    centerColor,
                    edgeColor,
                    Shader.TileMode.CLAMP,
                )
            c.drawRoundRect(keyRect, r, r, glowPaint)
            glowPaint.shader = null
            if (!isPressed && !recording) postInvalidateOnAnimation()
        }

        // Ultra-delicate glass border (never a harsh wireframe box!) + luminous top specular edge
        borderPaint.shader =
            LinearGradient(
                0f,
                keyRect.top,
                0f,
                keyRect.bottom,
                when {
                    clearAllActive -> 0xFFEF4444.toInt()
                    glowStrength > 0.2f -> (if (dark) T.Dark.accentTextArgb else T.Light.accentPrimaryArgb).toInt()
                    shiftActive -> (if (dark) 0x557DD3FC else 0x440284C7)
                    role == KeyRole.ACCENT -> 0x55FFFFFF
                    dark -> 0x26FFFFFF
                    else -> 0xE8FFFFFF.toInt()
                },
                when {
                    clearAllActive -> 0xCCDC2626.toInt()
                    glowStrength > 0.2f -> (if (dark) 0x4438BDF8 else 0x440284C7)
                    role == KeyRole.ACCENT -> 0x18FFFFFF
                    dark -> 0x0AFFFFFF
                    else -> 0x180F172A
                },
                Shader.TileMode.CLAMP,
            )
        c.drawRoundRect(keyRect, r, r, borderPaint)
        borderPaint.shader = null

        // Subtle charging progress bar during a continuous 3-second hold on Backspace
        if (label == "⌫" && isPressed && clearAllPending && !clearAllArmed && pressedAt > 0L) {
            val elapsed = (now - pressedAt).coerceAtLeast(0L)
            if (elapsed >= 600L) {
                val frac = ((elapsed - 600f) / (CLEAR_ALL_HOLD_MS - 600f)).coerceIn(0f, 1f)
                val barInset = context.dpF(4f)
                val barH = context.dpF(2.5f)
                val barW = ((keyRect.width() - barInset * 2f) * frac).coerceAtLeast(0f)
                paint.shader = null
                paint.style = Paint.Style.FILL
                paint.color = if (dark) 0xDDF87171.toInt() else 0xDDEF4444.toInt()
                c.drawRoundRect(
                    keyRect.left + barInset,
                    keyRect.top + context.dpF(2.5f),
                    keyRect.left + barInset + barW,
                    keyRect.top + context.dpF(2.5f) + barH,
                    barH * 0.5f,
                    barH * 0.5f,
                    paint,
                )
            }
            if (elapsed < CLEAR_ALL_HOLD_MS) postInvalidateOnAnimation()
        }

        val primaryText =
            when {
                role == KeyRole.ACCENT -> 0xFFFFFFFF.toInt()
                shiftActive && dark -> T.Dark.accentTextArgb.toInt()
                shiftActive -> T.Light.accentTextArgb.toInt()
                dark -> T.Dark.textPrimaryArgb.toInt()
                else -> T.Light.textPrimaryArgb.toInt()
            }
        val secondaryText =
            (if (dark) T.Dark.textSecondaryArgb else T.Light.textSecondaryArgb).toInt()
        val accentText = (if (dark) T.Dark.accentTextArgb else T.Light.accentTextArgb).toInt()

        val cx = keyRect.centerX()
        val cy = keyRect.centerY()

        if (recording) {
            val animate =
                Settings.Global.getFloat(
                    context.contentResolver,
                    Settings.Global.ANIMATOR_DURATION_SCALE,
                    1f,
                ) > 0
            val phase = if (animate) (now - pressedAt) / 105.0 else 0.0
            paint.shader = null
            paint.color = accentText
            paint.strokeWidth = context.dpF(2.2f)
            paint.strokeCap = Paint.Cap.ROUND
            for (i in -10..10) {
                val x = cx + i * context.dpF(4.0f)
                val envelope = 1f - (abs(i) / 11.5f)
                val v = (.09f + .26f * envelope * abs(sin(i * .70 + phase)).toFloat()) * keyRect.height()
                c.drawLine(x, cy - v, x, cy + v, paint)
            }
            if (animate) postInvalidateOnAnimation()
            return
        }

        if (swipeUpActive && upper.isNotEmpty()) {
            drawCenteredText(c, upper, cx, cy, 20f, accentText, Paint.Align.CENTER, bold = true)
            return
        }

        when (label) {
            "⇧" -> drawShiftGlyph(c, cx, cy, primaryText)
            "⌫" -> {
                when {
                    clearAllArmed && clearAllSwipeUp ->
                        drawCenteredText(c, "松手清空", cx, cy, 10.5f, 0xFFFFFFFF.toInt(), Paint.Align.CENTER, bold = true)
                    clearAllArmed ->
                        drawCenteredText(
                            c,
                            "↑ 清空",
                            cx,
                            cy,
                            11.5f,
                            if (dark) 0xFFFCA5A5.toInt() else 0xFFDC2626.toInt(),
                            Paint.Align.CENTER,
                            bold = true,
                        )
                    else -> drawBackspaceGlyph(c, cx, cy, primaryText)
                }
            }
            "↵" -> {
                if (displayLabel == "↵" || displayLabel.isEmpty()) {
                    drawReturnGlyph(c, cx, cy, primaryText)
                } else {
                    drawCenteredText(c, displayLabel, cx, cy, 13.5f, primaryText, Paint.Align.CENTER, bold = true)
                }
            }
            "空格" -> {
                if (spaceScrubbing) {
                    drawCenteredText(
                        c,
                        "◂  移动光标  ▸",
                        cx,
                        cy,
                        12.5f,
                        accentText,
                        Paint.Align.CENTER,
                        bold = true,
                    )
                } else {
                    val hint = spaceHint.ifEmpty { "Vitral" }
                    drawCenteredText(
                        c,
                        hint,
                        cx,
                        cy,
                        12f,
                        secondaryText,
                        Paint.Align.CENTER,
                        medium = true,
                        alpha = 195,
                    )
                    drawMicDot(c, keyRect.right - context.dpF(14f), cy, secondaryText)
                }
            }
            else -> {
                val showUpper = upper.isNotEmpty() && sublabelMode != SublabelMode.CLEAN
                val showFinal = flypy && sublabelMode == SublabelMode.DUAL
                val textSizeDp =
                    when {
                        displayLabel.length >= 3 -> 13.0f
                        displayLabel.length == 2 -> 14.0f
                        role == KeyRole.PRIMARY -> if (showFinal) 19.0f else 20.0f
                        else -> 16.0f
                    }
                val yOffset =
                    when {
                        showFinal -> -context.dpF(0.5f)
                        showUpper -> context.dpF(1.0f)
                        else -> 0f
                    }
                drawCenteredText(
                    c,
                    displayLabel,
                    cx,
                    cy + yOffset,
                    textSizeDp,
                    primaryText,
                    Paint.Align.CENTER,
                    bold = role == KeyRole.ACCENT,
                    medium = role == KeyRole.FUNCTION,
                )
                if (showUpper) {
                    val subInsetX = context.dpF(4.8f)
                    val subInsetY = context.dpF(7.2f)
                    drawCenteredText(
                        c,
                        upper,
                        if (flypy) keyRect.left + subInsetX else keyRect.right - subInsetX,
                        keyRect.top + subInsetY,
                        8.2f,
                        secondaryText,
                        if (flypy) Paint.Align.LEFT else Paint.Align.RIGHT,
                        alpha = 148,
                    )
                }
                if (showFinal) {
                    val finalLabel = finals[label.lowercase().singleOrNull()].orEmpty()
                    if (finalLabel.isNotEmpty()) {
                        drawCenteredText(
                            c,
                            finalLabel,
                            keyRect.right - context.dpF(4.6f),
                            keyRect.bottom - context.dpF(5.8f),
                            7.8f,
                            accentText,
                            Paint.Align.RIGHT,
                            medium = true,
                            alpha = 192,
                        )
                    }
                }
            }
        }
    }

    override fun onTouchEvent(e: MotionEvent): Boolean {
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                downX = e.x
                downY = e.y
                spaceAnchorX = e.x
                spaceScrubbing = false
                swipeUpActive = false
                repeatingDelete = false
                clearAllPending = false
                clearAllArmed = false
                clearAllSwipeUp = false
                pressedAt = e.eventTime
                releasedAt = 0L
                isPressed = true
                if (voice != null) {
                    postDelayed(startVoice, T.Ergonomics.spacebarVoiceHoldThresholdMs.toLong())
                }
                if (label == "⌫") {
                    clearAllPending = true
                    postDelayed(repeatDelete, 360L)
                    postDelayed(armClearAll, CLEAR_ALL_HOLD_MS)
                }
            }
            MotionEvent.ACTION_MOVE -> {
                if (!clearAllArmed && (abs(e.x - downX) > context.dp(14) || abs(e.y - downY) > context.dp(14))) {
                    removeCallbacks(startVoice)
                    removeCallbacks(repeatDelete)
                    removeCallbacks(armClearAll)
                    clearAllPending = false
                }
                if (label == "⌫" && clearAllArmed) {
                    clearAllSwipeUp =
                        if (clearAllSwipeUp) downY - e.y >= context.dp(16)
                        else downY - e.y >= context.dp(24)
                }
                if (label == "空格" && !recording) {
                    val step = context.dpF(14f)
                    if (!spaceScrubbing && abs(e.x - downX) > step) {
                        spaceScrubbing = true
                        spaceAnchorX = downX
                    }
                    if (spaceScrubbing) {
                        while (e.x - spaceAnchorX >= step) {
                            spaceAnchorX += step
                            action("cursor-right")
                        }
                        while (spaceAnchorX - e.x >= step) {
                            spaceAnchorX -= step
                            action("cursor-left")
                        }
                    }
                }
                swipeUpActive = !spaceScrubbing && !clearAllArmed && upper.isNotEmpty() && downY - e.y > context.dp(16)
            }
            MotionEvent.ACTION_UP -> {
                removeCallbacks(startVoice)
                removeCallbacks(repeatDelete)
                removeCallbacks(armClearAll)
                isPressed = false
                releasedAt = SystemClock.uptimeMillis()
                val didSwipeUp = swipeUpActive
                val didRepeat = repeatingDelete
                val didScrub = spaceScrubbing
                val didClearAllArmed = clearAllArmed
                val didClearAllSwipeUp = clearAllSwipeUp
                swipeUpActive = false
                repeatingDelete = false
                spaceScrubbing = false
                clearAllPending = false
                clearAllArmed = false
                clearAllSwipeUp = false
                when {
                    recording -> {
                        recording = false
                        voice?.invoke(false)
                    }
                    didClearAllArmed && didClearAllSwipeUp -> {
                        performHapticFeedback(HapticFeedbackConstants.CONTEXT_CLICK)
                        action("clear-all")
                    }
                    didClearAllArmed -> Unit
                    didRepeat || didScrub -> Unit
                    label == "⌫" && e.x - downX < -context.dp(20) -> action("delete-word")
                    didSwipeUp || (upper.isNotEmpty() && downY - e.y > context.dp(16)) ->
                        action(upper)
                    e.x in -context.dpF(8f)..(width + context.dpF(8f)) &&
                        e.y in -context.dpF(8f)..(height + context.dpF(8f)) -> performClick()
                }
            }
            MotionEvent.ACTION_CANCEL -> {
                removeCallbacks(startVoice)
                removeCallbacks(repeatDelete)
                removeCallbacks(armClearAll)
                isPressed = false
                releasedAt = SystemClock.uptimeMillis()
                swipeUpActive = false
                repeatingDelete = false
                spaceScrubbing = false
                clearAllPending = false
                clearAllArmed = false
                clearAllSwipeUp = false
                if (recording) {
                    recording = false
                    action("cancel-voice")
                }
            }
        }
        invalidate()
        return true
    }

    override fun onDetachedFromWindow() {
        removeCallbacks(startVoice)
        removeCallbacks(repeatDelete)
        removeCallbacks(armClearAll)
        super.onDetachedFromWindow()
    }

    companion object {
        private const val CLEAR_ALL_HOLD_MS = 3000L
    }
}

internal fun alternate(row: Int, col: Int) = alternates[row][col].toString()

internal class InkView(
    ctx: Context,
    private val continuous: Boolean,
    private val darkOverride: Boolean? = null,
    val stroke: (FloatArray, FloatArray, LongArray) -> Unit,
) : View(ctx) {
    private val bgPaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val borderPaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = ctx.resources.displayMetrics.density
        }
    private val guidePaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = ctx.resources.displayMetrics.density
            pathEffect = DashPathEffect(floatArrayOf(ctx.dpF(4f), ctx.dpF(5f)), 0f)
        }
    private val cornerPaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = ctx.dpF(1.5f)
            strokeCap = Paint.Cap.ROUND
        }
    private val hintPaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            textAlign = Paint.Align.CENTER
            textSize = ctx.dpF(12.5f)
            typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
        }
    private val paint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = context.dpF(T.Handwriting.strokeWidthPx)
            strokeJoin = Paint.Join.ROUND
            strokeCap = Paint.Cap.ROUND
        }
    private val ink = mutableListOf<Pair<Path, Long>>()
    private var path = Path()
    private var lastX = 0f
    private var lastY = 0f
    private val xs = ArrayList<Float>()
    private val ys = ArrayList<Float>()
    private val times = ArrayList<Long>()

    init {
        contentDescription = "手写区"
    }

    override fun onDraw(c: Canvas) {
        val dark = darkOverride ?: context.darkKeyboard
        val pad = context.dpF(6f)
        val r = context.dpF(14f)

        bgPaint.color =
            (if (dark) T.Dark.surfaceKeyFunctionArgb else T.Light.surfaceKeyFunctionArgb).toInt()
        c.drawRoundRect(pad, pad, width - pad, height - pad, r, r, bgPaint)
        borderPaint.color = if (dark) 0x22FFFFFF else 0x1C0F172A
        c.drawRoundRect(pad, pad, width - pad, height - pad, r, r, borderPaint)

        guidePaint.color = if (dark) 0x2094A3B8 else 0x1E64748B
        c.drawLine(pad + context.dpF(14f), height / 2f, width - pad - context.dpF(14f), height / 2f, guidePaint)
        c.drawLine(width / 2f, pad + context.dpF(12f), width / 2f, height - pad - context.dpF(12f), guidePaint)

        cornerPaint.color =
            (if (dark) T.Dark.accentTextArgb else T.Light.accentPrimaryArgb).toInt()
        cornerPaint.alpha = 85
        val inset = pad + context.dpF(9f)
        val arm = context.dpF(9f)
        c.drawLine(inset, inset, inset + arm, inset, cornerPaint)
        c.drawLine(inset, inset, inset, inset + arm, cornerPaint)
        c.drawLine(width - inset, inset, width - inset - arm, inset, cornerPaint)
        c.drawLine(width - inset, inset, width - inset, inset + arm, cornerPaint)
        c.drawLine(inset, height - inset, inset + arm, height - inset, cornerPaint)
        c.drawLine(inset, height - inset, inset, height - inset - arm, cornerPaint)
        c.drawLine(width - inset, height - inset, width - inset - arm, height - inset, cornerPaint)
        c.drawLine(width - inset, height - inset, width - inset, height - inset - arm, cornerPaint)

        val now = SystemClock.uptimeMillis()
        ink.removeAll { continuous && now - it.second >= T.Handwriting.superimposedInkFadeOutMs }

        if (ink.isEmpty() && xs.isEmpty()) {
            hintPaint.color =
                (if (dark) T.Dark.textSecondaryArgb else T.Light.textSecondaryArgb).toInt()
            hintPaint.alpha = 165
            val hint = if (continuous) "原位叠写或横向连写 · 写完自动联句" else "在此从容书写生僻字 · 自动注音与双拼码"
            c.drawText(
                hint,
                width / 2f,
                height / 2f - (hintPaint.descent() + hintPaint.ascent()) / 2f,
                hintPaint,
            )
            return
        }

        paint.color =
            (if (dark) T.Dark.inkStrokePrimaryArgb else T.Light.inkStrokePrimaryArgb).toInt()
        for ((p, t) in ink) {
            paint.alpha =
                if (continuous)
                    (255 * (1 - (now - t).toFloat() / T.Handwriting.superimposedInkFadeOutMs))
                        .toInt()
                        .coerceIn(0, 255)
                else 255
            c.drawPath(p, paint)
        }
        paint.alpha = 255
        c.drawPath(path, paint)
        if (continuous && ink.isNotEmpty()) postInvalidateOnAnimation()
    }

    private fun point(x: Float, y: Float, t: Long) {
        if (xs.isEmpty()) {
            path.moveTo(x, y)
        } else {
            val midX = (lastX + x) * 0.5f
            val midY = (lastY + y) * 0.5f
            path.quadTo(lastX, lastY, midX, midY)
        }
        lastX = x
        lastY = y
        xs.add(x / width.coerceAtLeast(1))
        ys.add(y / height.coerceAtLeast(1))
        times.add(t)
    }

    override fun onTouchEvent(e: MotionEvent): Boolean {
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                path = Path()
                xs.clear()
                ys.clear()
                times.clear()
                point(e.x, e.y, e.eventTime)
            }
            MotionEvent.ACTION_MOVE -> {
                for (i in 0 until e.historySize) {
                    point(
                        e.getHistoricalX(i),
                        e.getHistoricalY(i),
                        e.getHistoricalEventTime(i),
                    )
                }
                point(e.x, e.y, e.eventTime)
            }
            MotionEvent.ACTION_UP -> {
                point(e.x, e.y, e.eventTime)
                path.lineTo(e.x, e.y)
                ink.add(path to e.eventTime)
                stroke(xs.toFloatArray(), ys.toFloatArray(), times.toLongArray())
                path = Path()
                xs.clear()
                ys.clear()
                times.clear()
            }
            MotionEvent.ACTION_CANCEL -> {
                path = Path()
                xs.clear()
                ys.clear()
                times.clear()
            }
        }
        invalidate()
        return true
    }
}
