package dev.vitral.ime

import android.content.Context
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Path
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

internal class Key(
    ctx: Context,
    private val label: String,
    private val flypy: Boolean,
    private val upper: String,
    private val action: (String) -> Unit,
    private val voice: ((Boolean) -> Unit)? = null,
) : View(ctx) {
    private val paint = Paint(Paint.ANTI_ALIAS_FLAG)
    private var downX = 0f
    private var downY = 0f
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
        color: Long,
        align: Paint.Align,
    ) {
        paint.color = color.toInt()
        paint.textSize = context.dp(size).toFloat()
        paint.textAlign = align
        c.drawText(s, x, y - (paint.descent() + paint.ascent()) / 2, paint)
    }

    override fun onDraw(c: Canvas) {
        val pad = context.dp(2).toFloat()
        paint.color =
            when {
                isPressed && context.darkKeyboard -> T.Dark.surfaceKeyPressedGlowArgb
                isPressed -> T.Light.surfaceKeyPressedGlowArgb
                context.darkKeyboard -> T.Dark.surfaceKeyPrimaryArgb
                else -> T.Light.surfaceKeyPrimaryArgb
            }.toInt()
        val r = context.dp(T.Radius.mobileKeycapPx).toFloat()
        c.drawRoundRect(pad, pad, width - pad, height - pad, r, r, paint)
        if (recording) {
            val animate =
                Settings.Global.getFloat(
                    context.contentResolver,
                    Settings.Global.ANIMATOR_DURATION_SCALE,
                    1f,
                ) > 0
            val phase = if (animate) (SystemClock.uptimeMillis() - pressedAt) / 120.0 else 0.0
            paint.color =
                (if (context.darkKeyboard) T.Dark.accentTextArgb else T.Light.accentTextArgb)
                    .toInt()
            paint.strokeWidth = pad
            for (i in -8..8) {
                val x = width / 2f + i * context.dp(4)
                val v = (.12f + .2f * abs(sin(i * .8 + phase)).toFloat()) * height
                c.drawLine(x, height / 2f - v, x, height / 2f + v, paint)
            }
            if (animate) postInvalidateOnAnimation()
        } else
            text(
                c,
                label,
                width / 2f,
                height / 2f,
                if (label.length > 2) 14 else 18,
                if (context.darkKeyboard) T.Dark.textPrimaryArgb else T.Light.textPrimaryArgb,
                Paint.Align.CENTER,
            )
        if (upper.isNotEmpty()) {
            text(
                c,
                upper,
                if (flypy) pad * 3 else width - pad * 3,
                pad * 4,
                8,
                if (context.darkKeyboard) T.Dark.textSecondaryArgb else T.Light.textSecondaryArgb,
                if (flypy) Paint.Align.LEFT else Paint.Align.RIGHT,
            )
            if (flypy)
                text(
                    c,
                    finals[label.singleOrNull()].orEmpty(),
                    width - pad * 3,
                    height - pad * 4,
                    8,
                    if (context.darkKeyboard) T.Dark.accentTextArgb else T.Light.accentTextArgb,
                    Paint.Align.RIGHT,
                )
        }
    }

    override fun onTouchEvent(e: MotionEvent): Boolean {
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                downX = e.x
                downY = e.y
                pressedAt = e.eventTime
                isPressed = true
                if (voice != null)
                    postDelayed(startVoice, T.Ergonomics.spacebarVoiceHoldThresholdMs.toLong())
            }
            MotionEvent.ACTION_MOVE ->
                if (abs(e.x - downX) > context.dp(18) || abs(e.y - downY) > context.dp(18))
                    removeCallbacks(startVoice)
            MotionEvent.ACTION_UP -> {
                removeCallbacks(startVoice)
                isPressed = false
                when {
                    recording -> {
                        recording = false
                        voice?.invoke(false)
                    }
                    label == "空格" && abs(e.x - downX) > context.dp(20) ->
                        action(if (e.x > downX) "cursor-right" else "cursor-left")
                    label == "⌫" && e.x - downX < -context.dp(20) -> action("delete-word")
                    upper.isNotEmpty() && downY - e.y > context.dp(16) -> action(upper)
                    e.x in 0f..width.toFloat() && e.y in 0f..height.toFloat() -> performClick()
                }
            }
            MotionEvent.ACTION_CANCEL -> {
                removeCallbacks(startVoice)
                isPressed = false
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
    private val paint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color =
                (if (context.darkKeyboard) T.Dark.inkStrokePrimaryArgb
                    else T.Light.inkStrokePrimaryArgb)
                    .toInt()
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
        val now = SystemClock.uptimeMillis()
        ink.removeAll { continuous && now - it.second >= T.Handwriting.superimposedInkFadeOutMs }
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
            }
            MotionEvent.ACTION_CANCEL -> path = Path()
        }
        invalidate()
        return true
    }
}
