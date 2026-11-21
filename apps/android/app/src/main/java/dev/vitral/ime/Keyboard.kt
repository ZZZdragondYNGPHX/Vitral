package dev.vitral.ime

import android.content.Context
import android.graphics.Canvas
import android.graphics.DashPathEffect
import android.graphics.Paint
import android.graphics.Path
import android.graphics.Typeface
import android.os.SystemClock
import android.provider.Settings
import android.view.MotionEvent
import android.view.View
import dev.vitral.ime.theme.VitralTokens as T
import kotlin.math.abs
import kotlin.math.sin

internal fun Context.dp(value: Int) = (resources.displayMetrics.density * value + .5f).toInt()

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

internal class Key(
    ctx: Context,
    private val label: String,
    private val flypy: Boolean,
    private val upper: String,
    private val action: (String) -> Unit,
    private val voice: ((Boolean) -> Unit)? = null,
    private val role: KeyRole = KeyRole.PRIMARY,
) : View(ctx) {
    private val paint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val borderPaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = ctx.resources.displayMetrics.density
        }
    private var downX = 0f
    private var downY = 0f
    private var swipeUpActive = false
    private var recording = false
    private var pressedAt = 0L
    private val startVoice = Runnable {
        if (isPressed) {
            recording = true
            voice?.invoke(true)
            invalidate()
        }
    }

    init {
        contentDescription = label
        isFocusable = true
        isClickable = true
        setOnClickListener { action(label) }
    }

    private fun text(
        c: Canvas,
        s: String,
        x: Float,
        y: Float,
        size: Int,
        color: Int,
        align: Paint.Align,
        bold: Boolean = false,
    ) {
        paint.style = Paint.Style.FILL
        paint.color = color
        paint.typeface = if (bold) Typeface.DEFAULT_BOLD else Typeface.DEFAULT
        paint.textSize = context.dp(size).toFloat()
        paint.textAlign = align
        c.drawText(s, x, y - (paint.descent() + paint.ascent()) / 2, paint)
    }

    override fun onDraw(c: Canvas) {
        val dark = context.darkKeyboard
        val padX = context.dp(2).toFloat()
        val padY = context.dp(3).toFloat()
        val r = context.dp(T.Radius.mobileKeycapPx).toFloat()

        val fillColor =
            when {
                isPressed && dark -> T.Dark.surfaceKeyPressedGlowArgb
                isPressed -> T.Light.surfaceKeyPressedGlowArgb
                role == KeyRole.ACCENT && dark -> T.Dark.accentPrimaryArgb
                role == KeyRole.ACCENT -> T.Light.accentPrimaryArgb
                role == KeyRole.FUNCTION && dark -> T.Dark.surfaceKeyFunctionArgb
                role == KeyRole.FUNCTION -> T.Light.surfaceKeyFunctionArgb
                dark -> T.Dark.surfaceKeyPrimaryArgb
                else -> T.Light.surfaceKeyPrimaryArgb
            }.toInt()

        paint.style = Paint.Style.FILL
        paint.color = fillColor
        c.drawRoundRect(padX, padY, width - padX, height - padY, r, r, paint)

        // Subtle 1px specular border (brighter cyan glow when pressed)
        borderPaint.color =
            when {
                isPressed && dark -> T.Dark.accentTextArgb
                isPressed -> T.Light.accentPrimaryArgb
                role == KeyRole.ACCENT -> 0x33FFFFFF
                dark -> T.Dark.borderSpecularArgb
                else -> T.Light.borderSpecularArgb
            }.toInt()
        c.drawRoundRect(padX, padY, width - padX, height - padY, r, r, borderPaint)

        val primaryText =
            when {
                role == KeyRole.ACCENT -> 0xFFFFFFFF.toInt()
                dark -> T.Dark.textPrimaryArgb.toInt()
                else -> T.Light.textPrimaryArgb.toInt()
            }
        val secondaryText =
            (if (dark) T.Dark.textSecondaryArgb else T.Light.textSecondaryArgb).toInt()
        val accentText = (if (dark) T.Dark.accentTextArgb else T.Light.accentTextArgb).toInt()

        if (recording) {
            val animate =
                Settings.Global.getFloat(
                    context.contentResolver,
                    Settings.Global.ANIMATOR_DURATION_SCALE,
                    1f,
                ) > 0
            val phase = if (animate) (SystemClock.uptimeMillis() - pressedAt) / 120.0 else 0.0
            paint.color = accentText
            paint.strokeWidth = context.dp(2).toFloat()
            paint.strokeCap = Paint.Cap.ROUND
            for (i in -8..8) {
                val x = width / 2f + i * context.dp(4)
                val v = (.12f + .22f * abs(sin(i * .75 + phase)).toFloat()) * height
                c.drawLine(x, height / 2f - v, x, height / 2f + v, paint)
            }
            if (animate) postInvalidateOnAnimation()
        } else if (swipeUpActive && upper.isNotEmpty()) {
            // In-key highlight showing the swipe-up alternate symbol clearly
            text(c, upper, width / 2f, height / 2f, 18, accentText, Paint.Align.CENTER, bold = true)
        } else {
            text(
                c,
                label,
                width / 2f,
                height / 2f + if (upper.isNotEmpty()) context.dp(1) else 0,
                if (label.length > 2) 13 else 17,
                primaryText,
                Paint.Align.CENTER,
                bold = role == KeyRole.ACCENT,
            )
            if (upper.isNotEmpty()) {
                val subInsetX = context.dp(5).toFloat()
                val subInsetY = context.dp(7).toFloat()
                text(
                    c,
                    upper,
                    if (flypy) subInsetX else width - subInsetX,
                    subInsetY,
                    8,
                    secondaryText,
                    if (flypy) Paint.Align.LEFT else Paint.Align.RIGHT,
                )
                if (flypy) {
                    val finalLabel = finals[label.singleOrNull()].orEmpty()
                    if (finalLabel.isNotEmpty()) {
                        text(
                            c,
                            finalLabel,
                            width - subInsetX,
                            height - context.dp(6).toFloat(),
                            8,
                            accentText,
                            Paint.Align.RIGHT,
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
                swipeUpActive = false
                pressedAt = e.eventTime
                isPressed = true
                if (voice != null)
                    postDelayed(startVoice, T.Ergonomics.spacebarVoiceHoldThresholdMs.toLong())
            }
            MotionEvent.ACTION_MOVE -> {
                if (abs(e.x - downX) > context.dp(18) || abs(e.y - downY) > context.dp(18))
                    removeCallbacks(startVoice)
                swipeUpActive = upper.isNotEmpty() && downY - e.y > context.dp(16)
            }
            MotionEvent.ACTION_UP -> {
                removeCallbacks(startVoice)
                isPressed = false
                val didSwipeUp = swipeUpActive
                swipeUpActive = false
                when {
                    recording -> {
                        recording = false
                        voice?.invoke(false)
                    }
                    label == "空格" && abs(e.x - downX) > context.dp(20) ->
                        action(if (e.x > downX) "cursor-right" else "cursor-left")
                    label == "⌫" && e.x - downX < -context.dp(20) -> action("delete-word")
                    didSwipeUp || (upper.isNotEmpty() && downY - e.y > context.dp(16)) ->
                        action(upper)
                    e.x in 0f..width.toFloat() && e.y in 0f..height.toFloat() -> performClick()
                }
            }
            MotionEvent.ACTION_CANCEL -> {
                removeCallbacks(startVoice)
                isPressed = false
                swipeUpActive = false
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
        super.onDetachedFromWindow()
    }
}

internal fun alternate(row: Int, col: Int) = alternates[row][col].toString()

internal class InkView(
    ctx: Context,
    private val continuous: Boolean,
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
            pathEffect = DashPathEffect(floatArrayOf(ctx.dp(4).toFloat(), ctx.dp(4).toFloat()), 0f)
        }
    private val hintPaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            textAlign = Paint.Align.CENTER
            textSize = ctx.dp(12).toFloat()
        }
    private val paint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = context.dp(1) * T.Handwriting.strokeWidthPx
            strokeJoin = Paint.Join.ROUND
            strokeCap = Paint.Cap.ROUND
        }
    private val ink = mutableListOf<Pair<Path, Long>>()
    private var path = Path()
    private val xs = ArrayList<Float>()
    private val ys = ArrayList<Float>()
    private val times = ArrayList<Long>()

    init {
        contentDescription = "手写区"
    }

    override fun onDraw(c: Canvas) {
        val dark = context.darkKeyboard
        val pad = context.dp(6).toFloat()
        val r = context.dp(12).toFloat()

        // Frosted recessed canvas + subtle 田字格
        bgPaint.color =
            (if (dark) T.Dark.surfaceKeyFunctionArgb else T.Light.surfaceKeyFunctionArgb).toInt()
        c.drawRoundRect(pad, pad, width - pad, height - pad, r, r, bgPaint)
        borderPaint.color =
            (if (dark) T.Dark.borderSpecularArgb else T.Light.borderSpecularArgb).toInt()
        c.drawRoundRect(pad, pad, width - pad, height - pad, r, r, borderPaint)

        guidePaint.color = if (dark) 0x2694A3B8 else 0x2664748B
        c.drawLine(pad + context.dp(12), height / 2f, width - pad - context.dp(12), height / 2f, guidePaint)
        c.drawLine(width / 2f, pad + context.dp(10), width / 2f, height - pad - context.dp(10), guidePaint)

        val now = SystemClock.uptimeMillis()
        ink.removeAll { continuous && now - it.second >= T.Handwriting.superimposedInkFadeOutMs }

        if (ink.isEmpty() && xs.isEmpty()) {
            hintPaint.color =
                (if (dark) T.Dark.textSecondaryArgb else T.Light.textSecondaryArgb).toInt()
            hintPaint.alpha = 160
            val hint = if (continuous) "原位叠写或横向连写" else "在此手写查音"
            c.drawText(hint, width / 2f, height / 2f - (hintPaint.descent() + hintPaint.ascent()) / 2f, hintPaint)
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
        if (xs.isEmpty()) path.moveTo(x, y) else path.lineTo(x, y)
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
                for (i in 0 until e.historySize) point(
                    e.getHistoricalX(i),
                    e.getHistoricalY(i),
                    e.getHistoricalEventTime(i),
                )
                point(e.x, e.y, e.eventTime)
            }
            MotionEvent.ACTION_UP -> {
                point(e.x, e.y, e.eventTime)
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
