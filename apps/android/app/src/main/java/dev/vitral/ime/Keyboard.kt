package dev.vitral.ime

import android.content.Context
import android.graphics.Canvas
import android.graphics.DashPathEffect
import android.graphics.LinearGradient
import android.graphics.Paint
import android.graphics.Path
import android.graphics.RadialGradient
import android.graphics.Rect
import android.graphics.RectF
import android.graphics.Shader
import android.graphics.Typeface
import android.os.Build
import android.os.SystemClock
import android.provider.Settings
import android.view.HapticFeedbackConstants
import android.view.MotionEvent
import android.view.View
import android.widget.LinearLayout
import dev.vitral.ime.theme.VitralTokens as T
import kotlin.math.abs
import kotlin.math.atan2
import kotlin.math.hypot
import kotlin.math.max
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

/** One physical hold may delete preedit or host text, never cross from the former to the latter. */
internal class BackspaceHold {
    var startedComposing = false
        private set
    private var blocked = false

    fun begin(composing: Boolean) {
        startedComposing = composing
        blocked = false
    }

    fun allowsDelete(composing: Boolean): Boolean {
        if (startedComposing && !composing) blocked = true
        return !blocked
    }
}

/** Coalesce auto-repeat ticks while an async delete is in flight, without dropping discrete taps. */
internal class BackspaceFlightGate {
    var inFlight = 0
        private set

    fun shouldDrop(isRepeat: Boolean): Boolean = isRepeat && inFlight > 0

    fun onDispatched() {
        inFlight += 1
    }

    fun onCompleted() {
        if (inFlight > 0) inFlight -= 1
    }

    fun reset() {
        inFlight = 0
    }
}

internal fun shouldDeleteHostText(
    composingAtCallback: Boolean,
    startedComposingAtDispatch: Boolean,
    isRepeat: Boolean,
): Boolean = !composingAtCallback && (!isRepeat || !startedComposingAtDispatch)

internal fun trailingWordDeleteSlice(before: String, lastCommitted: String): Pair<String, Boolean> {
    if (before.isEmpty()) return "" to false
    if (
        lastCommitted.isNotEmpty() &&
            lastCommitted.codePointCount(0, lastCommitted.length) <= 8 &&
            before.endsWith(lastCommitted)
    ) {
        return lastCommitted to true
    }
    if (before.last().code < 128) {
        val trimmedLen = before.trimEnd(' ').length
        val trailSpaces = before.length - trimmedLen
        val wordLen =
            before
                .substring(0, trimmedLen)
                .takeLastWhile { it.code < 128 && it.isLetterOrDigit() }
                .length
        val count = (wordLen + trailSpaces).coerceAtLeast(1)
        return before.takeLast(count) to false
    }
    val cpCount = before.codePointCount(0, before.length).coerceAtMost(4)
    val start = before.offsetByCodePoints(before.length, -cpCount)
    return before.substring(start) to false
}

internal const val HOLD_BATCH_MS = 1000L
internal const val BATCH_DELETE_INTERVAL_MS = 300L
private const val BATCH_DELETE_MAX_CODE_POINTS = 400

internal enum class BackspaceHoldIntent {
    INACTIVE,
    BATCH,
    DELETE_BEFORE,
    DELETE_AFTER,
    ROTATING,
    CLEAR_ALL,
}

/**
 * Pure state machine for the 1-second Backspace hold gestures:
 * - Hold in place: BATCH (periodic sentence/paragraph deletion)
 * - Swipe up: DELETE_BEFORE (delete all text before cursor on release)
 * - Swipe down: DELETE_AFTER (delete all text after cursor on release)
 * - Draw a circle (CW/CCW, off-center friendly via tangential turning angle): ROTATING -> CLEAR_ALL
 */
internal class BackspaceHoldGesture(
    private val swipeUpEnterPx: Float,
    private val swipeUpExitPx: Float,
    private val swipeDownEnterPx: Float,
    private val swipeDownExitPx: Float,
    private val sampleStepPx: Float,
    private val rotateSpanPx: Float,
    private val clearAllSpanPx: Float,
) {
    var intent: BackspaceHoldIntent = BackspaceHoldIntent.INACTIVE
        private set

    val isActive: Boolean
        get() = intent != BackspaceHoldIntent.INACTIVE

    private var anchorX = 0f
    private var anchorY = 0f
    private var lastSampleX = 0f
    private var lastSampleY = 0f
    private var hasPrevDir = false
    private var prevDx = 0f
    private var prevDy = 0f
    private var cumulativeAngle = 0f
    private var minX = 0f
    private var maxX = 0f
    private var minY = 0f
    private var maxY = 0f
    private val sampleX = FloatArray(MAX_SAMPLES)
    private val sampleY = FloatArray(MAX_SAMPLES)
    private var sampleCount = 0

    private fun appendSample(x: Float, y: Float) {
        if (sampleCount >= MAX_SAMPLES) {
            System.arraycopy(sampleX, 1, sampleX, 0, MAX_SAMPLES - 1)
            System.arraycopy(sampleY, 1, sampleY, 0, MAX_SAMPLES - 1)
            sampleCount = MAX_SAMPLES - 1
        }
        sampleX[sampleCount] = x
        sampleY[sampleCount] = y
        sampleCount += 1
    }

    private fun hasClosedLoop(): Boolean {
        if (sampleCount < CLEAR_ALL_MIN_SAMPLES) return false
        val m = sampleCount - 1
        val endX = sampleX[m]
        val endY = sampleY[m]
        var sMinX = endX
        var sMaxX = endX
        var sMinY = endY
        var sMaxY = endY
        var sumX = endX
        var sumY = endY
        for (s in m - 1 downTo 0) {
            val xs = sampleX[s]
            val ys = sampleY[s]
            if (xs < sMinX) sMinX = xs
            if (xs > sMaxX) sMaxX = xs
            if (ys < sMinY) sMinY = ys
            if (ys > sMaxY) sMaxY = ys
            sumX += xs
            sumY += ys
            val count = m - s + 1
            if (count < CLEAR_ALL_MIN_SAMPLES) continue
            val loopSpanX = sMaxX - sMinX
            val loopSpanY = sMaxY - sMinY
            if (loopSpanX < clearAllSpanPx || loopSpanY < clearAllSpanPx) continue
            val minSpan = min(loopSpanX, loopSpanY)
            val closeLimit = max(sampleStepPx * 2.5f, 0.35f * minSpan)
            if (hypot(endX - xs, endY - ys) > closeLimit) continue
            val cx = sumX / count
            val cy = sumY / count
            val minRadius = 0.18f * minSpan
            var prevRx = xs - cx
            var prevRy = ys - cy
            if (hypot(prevRx, prevRy) < minRadius) continue
            var insideOk = true
            var winding = 0f
            for (j in s + 1..m) {
                val rx = sampleX[j] - cx
                val ry = sampleY[j] - cy
                if (hypot(rx, ry) < minRadius) {
                    insideOk = false
                    break
                }
                val cross = prevRx * ry - prevRy * rx
                val dot = prevRx * rx + prevRy * ry
                winding += atan2(cross, dot)
                prevRx = rx
                prevRy = ry
            }
            if (insideOk && abs(winding) >= CLEAR_ALL_WINDING_RAD) {
                return true
            }
        }
        return false
    }

    fun begin(anchorX: Float, anchorY: Float) {
        this.anchorX = anchorX
        this.anchorY = anchorY
        lastSampleX = anchorX
        lastSampleY = anchorY
        hasPrevDir = false
        prevDx = 0f
        prevDy = 0f
        cumulativeAngle = 0f
        minX = anchorX
        maxX = anchorX
        minY = anchorY
        maxY = anchorY
        sampleX[0] = anchorX
        sampleY[0] = anchorY
        sampleCount = 1
        intent = BackspaceHoldIntent.BATCH
    }

    fun reset() {
        intent = BackspaceHoldIntent.INACTIVE
        hasPrevDir = false
        prevDx = 0f
        prevDy = 0f
        cumulativeAngle = 0f
        sampleCount = 0
    }

    fun update(x: Float, y: Float): BackspaceHoldIntent {
        if (intent == BackspaceHoldIntent.INACTIVE || intent == BackspaceHoldIntent.CLEAR_ALL) {
            return intent
        }
        val dx = x - lastSampleX
        val dy = y - lastSampleY
        var sampleAdded = false
        if (hypot(dx, dy) >= sampleStepPx) {
            if (hasPrevDir) {
                val cross = prevDx * dy - prevDy * dx
                val dot = prevDx * dx + prevDy * dy
                val deltaPsi = atan2(cross, dot)
                if (abs(deltaPsi) > SHARP_REVERSAL_RAD) {
                    cumulativeAngle = 0f
                    minX = min(lastSampleX, x)
                    maxX = max(lastSampleX, x)
                    minY = min(lastSampleY, y)
                    maxY = max(lastSampleY, y)
                    sampleX[0] = lastSampleX
                    sampleY[0] = lastSampleY
                    sampleX[1] = x
                    sampleY[1] = y
                    sampleCount = 2
                } else {
                    cumulativeAngle += deltaPsi
                    minX = min(minX, x)
                    maxX = max(maxX, x)
                    minY = min(minY, y)
                    maxY = max(maxY, y)
                    appendSample(x, y)
                }
            } else {
                minX = min(minX, x)
                maxX = max(maxX, x)
                minY = min(minY, y)
                maxY = max(maxY, y)
                appendSample(x, y)
            }
            prevDx = dx
            prevDy = dy
            hasPrevDir = true
            lastSampleX = x
            lastSampleY = y
            sampleAdded = true
        }

        val spanX = maxX - minX
        val spanY = maxY - minY
        val absAngle = abs(cumulativeAngle)
        if (sampleAdded && hasClosedLoop()) {
            intent = BackspaceHoldIntent.CLEAR_ALL
            return intent
        }
        val rotateThreshold =
            if (intent == BackspaceHoldIntent.ROTATING) ROTATING_EXIT_ANGLE_RAD else ROTATING_ENTER_ANGLE_RAD
        if (absAngle >= rotateThreshold && spanX >= rotateSpanPx && spanY >= rotateSpanPx) {
            intent = BackspaceHoldIntent.ROTATING
            return intent
        }

        val upDist = anchorY - y
        val downDist = y - anchorY
        intent =
            when (intent) {
                BackspaceHoldIntent.DELETE_BEFORE ->
                    when {
                        upDist >= swipeUpExitPx -> BackspaceHoldIntent.DELETE_BEFORE
                        downDist >= swipeDownEnterPx -> BackspaceHoldIntent.DELETE_AFTER
                        else -> BackspaceHoldIntent.BATCH
                    }
                BackspaceHoldIntent.DELETE_AFTER ->
                    when {
                        downDist >= swipeDownExitPx -> BackspaceHoldIntent.DELETE_AFTER
                        upDist >= swipeUpEnterPx -> BackspaceHoldIntent.DELETE_BEFORE
                        else -> BackspaceHoldIntent.BATCH
                    }
                else ->
                    when {
                        upDist >= swipeUpEnterPx -> BackspaceHoldIntent.DELETE_BEFORE
                        downDist >= swipeDownEnterPx -> BackspaceHoldIntent.DELETE_AFTER
                        else -> BackspaceHoldIntent.BATCH
                    }
            }
        return intent
    }

    companion object {
        internal const val SHARP_REVERSAL_RAD = 2.1f
        internal const val ROTATING_ENTER_ANGLE_RAD = 2.1f
        internal const val ROTATING_EXIT_ANGLE_RAD = 1.6f
        internal const val CLEAR_ALL_WINDING_RAD = 5.75f
        private const val CLEAR_ALL_MIN_SAMPLES = 8
        private const val MAX_SAMPLES = 256
    }
}

private fun isSentenceEndPunctuation(ch: Char): Boolean =
    ch == '。' || ch == '！' || ch == '？' || ch == '；' ||
        ch == '.' || ch == '!' || ch == '?' || ch == ';'

/**
 * Computes the trailing slice to delete on one batch-delete tick.
 * 0-based `batchStep`: ticks 0..2 delete by sentence (`。！？；.!?;` or `\n`);
 * tick 3 (4th jump) onward upgrades to deleting whole paragraphs (`\n`).
 * Always caps at 400 code points without splitting UTF-16 surrogate pairs.
 */
internal fun trailingBatchDeleteSlice(before: String, batchStep: Int): String {
    if (before.isEmpty()) return ""
    val paragraphMode = batchStep >= 3
    var i = before.length
    while (i > 0 && before[i - 1].isWhitespace()) {
        i--
    }
    if (!paragraphMode) {
        while (i > 0 && isSentenceEndPunctuation(before[i - 1])) {
            i--
        }
    }
    while (i > 0) {
        val ch = before[i - 1]
        if (ch == '\n' || (!paragraphMode && isSentenceEndPunctuation(ch))) {
            break
        }
        i--
    }
    val cpCount = before.codePointCount(i, before.length)
    val start =
        if (cpCount > BATCH_DELETE_MAX_CODE_POINTS) {
            before.offsetByCodePoints(before.length, -BATCH_DELETE_MAX_CODE_POINTS)
        } else {
            i
        }
    return before.substring(start)
}

internal data class PreeditDisplay(
    val text: String,
    val cursorIndex: Int,
    val rawByteOffsets: IntArray,
)

internal fun formatPreedit(raw: String, comment: String): String =
    buildPreeditDisplay(raw, comment, raw.toByteArray(Charsets.UTF_8).size).text

internal fun buildPreeditDisplay(raw: String, comment: String, cursorUtf8: Int): PreeditDisplay {
    val rawBytesLen = raw.toByteArray(Charsets.UTF_8).size
    val clampedUtf8 = cursorUtf8.coerceIn(0, rawBytesLen)
    // Only use complete, matching annotations; abbreviations and partial syllables stay raw.
    val syllables = comment.trim().split(Regex("[\\s'’]+"))
    val useFormatted =
        syllables.size >= 2 &&
            syllables.none { part -> part.isEmpty() || part.any { !it.isLetter() } } &&
            raw.none { !it.isLetter() && it != '\'' && it != '’' && !it.isWhitespace() } &&
            syllables.joinToString("").equals(raw.filter { it.isLetter() }, ignoreCase = true)

    if (!useFormatted) {
        val offsets = IntArray(raw.length + 1)
        var bytes = 0
        var cursorIdx = 0
        for (i in raw.indices) {
            offsets[i] = bytes
            if (bytes <= clampedUtf8) cursorIdx = i
            bytes += raw.substring(i, i + 1).toByteArray(Charsets.UTF_8).size
        }
        offsets[raw.length] = bytes
        if (bytes <= clampedUtf8) cursorIdx = raw.length
        return PreeditDisplay(raw, cursorIdx, offsets)
    }

    val letterRawByteStart = IntArray(raw.length)
    val letterRawByteEnd = IntArray(raw.length)
    var letterCount = 0
    var bytePos = 0
    for (i in raw.indices) {
        val ch = raw[i]
        val chBytes = raw.substring(i, i + 1).toByteArray(Charsets.UTF_8).size
        if (ch.isLetter()) {
            letterRawByteStart[letterCount] = bytePos
            letterRawByteEnd[letterCount] = bytePos + chBytes
            letterCount++
        }
        bytePos += chBytes
    }
    val letters = raw.filter { it.isLetter() }
    val sb = StringBuilder(letterCount + syllables.size)
    val offsetsList = ArrayList<Int>(letterCount + syllables.size + 1)
    var letterOffset = 0
    syllables.forEachIndexed { sIdx, part ->
        if (sIdx > 0) {
            sb.append('\'')
            val boundaryByte =
                if (letterOffset > 0) letterRawByteEnd[letterOffset - 1] else 0
            offsetsList.add(boundaryByte)
        }
        for (j in part.indices) {
            offsetsList.add(letterRawByteStart[letterOffset])
            sb.append(letters[letterOffset])
            letterOffset++
        }
    }
    offsetsList.add(rawBytesLen)
    val offsets = IntArray(offsetsList.size) { offsetsList[it] }
    var cursorIdx = 0
    for (i in offsets.indices) {
        if (offsets[i] <= clampedUtf8) {
            cursorIdx = i
        }
    }
    // Prefer landing right after an auto-inserted apostrophe when cursor is at syllable end
    return PreeditDisplay(sb.toString(), cursorIdx, offsets)
}

internal fun visualTapToRawByteOffset(
    display: PreeditDisplay,
    visualCharOffset: Int,
    hasCaret: Boolean,
): Int {
    val logicalOffset =
        if (!hasCaret) {
            visualCharOffset
        } else if (visualCharOffset <= display.cursorIndex) {
            visualCharOffset
        } else {
            visualCharOffset - 1
        }
    val idx = logicalOffset.coerceIn(0, display.rawByteOffsets.lastIndex)
    return display.rawByteOffsets[idx]
}

internal class Key(
    ctx: Context,
    private val label: String,
    private val flypy: Boolean,
    private val upper: String,
    private val action: (String) -> Unit,
    private val voice: ((Boolean) -> Unit)? = null,
    private val role: KeyRole = KeyRole.PRIMARY,
    private var displayLabel: String = label,
    private var shiftState: Int = 0, // 0=off, 1=single, 2=capsLock
    private val spaceHint: String = "",
    private val darkOverride: Boolean? = null,
    private val sublabelMode: SublabelMode = SublabelMode.DUAL,
    private val isComposing: () -> Boolean = { false },
    private val canScrubSpace: () -> Boolean = { true },
) : View(ctx) {
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
    private val glyphPath = Path()
    private var cachedFillShader: LinearGradient? = null
    private var cachedFillTopColor = 0
    private var cachedFillBottomColor = 0
    private var cachedFillTop = Float.NaN
    private var cachedFillBottom = Float.NaN
    private var cachedBorderShader: LinearGradient? = null
    private var cachedBorderTopColor = 0
    private var cachedBorderBottomColor = 0
    private var cachedBorderTop = Float.NaN
    private var cachedBorderBottom = Float.NaN
    private var cachedGlowShader: RadialGradient? = null
    private var cachedGlowColor = 0
    private var cachedGlowCx = Float.NaN
    private var cachedGlowCy = Float.NaN
    private var cachedGlowRadius = Float.NaN
    private val backspaceHold = BackspaceHold()
    private val backspaceGesture =
        BackspaceHoldGesture(
            swipeUpEnterPx = ctx.dpF(20f),
            swipeUpExitPx = ctx.dpF(14f),
            swipeDownEnterPx = ctx.dpF(18f),
            swipeDownExitPx = ctx.dpF(12f),
            sampleStepPx = ctx.dpF(4f),
            rotateSpanPx = ctx.dpF(12f),
            clearAllSpanPx = ctx.dpF(16f),
        )
    private var downX = 0f
    private var downY = 0f
    private var lastX = 0f
    private var lastY = 0f
    private var swipeUpActive = false
    private var spaceScrubbing = false
    private var spaceAnchorX = 0f
    private var recording = false
    private var repeatingDelete = false
    private var batchHoldPending = false
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
                if (!isPressed || backspaceGesture.isActive) return
                repeatingDelete = true
                if (!backspaceHold.allowsDelete(isComposing())) return
                action("⌫-repeat")
                postDelayed(this, 65L)
            }
        }

    private val batchDelete =
        object : Runnable {
            override fun run() {
                if (!isPressed || backspaceGesture.intent != BackspaceHoldIntent.BATCH) return
                if (!backspaceHold.allowsDelete(isComposing())) return
                action("⌫-batch")
                postDelayed(this, BATCH_DELETE_INTERVAL_MS)
            }
        }

    private val armBatchDelete = Runnable {
        if (isPressed && batchHoldPending && label == "⌫") {
            batchHoldPending = false
            removeCallbacks(repeatDelete)
            if (!backspaceHold.allowsDelete(isComposing())) {
                invalidate()
                return@Runnable
            }
            backspaceGesture.begin(lastX, lastY)
            performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
            action("⌫-batch")
            if (isPressed && backspaceGesture.intent == BackspaceHoldIntent.BATCH) {
                postDelayed(batchDelete, BATCH_DELETE_INTERVAL_MS)
            }
            invalidate()
        }
    }

    init {
        contentDescription = displayLabel.ifEmpty { label }
        isFocusable = true
        isClickable = true
        setOnClickListener {
            if (label != "⌫" || backspaceHold.allowsDelete(isComposing())) action(label)
        }
    }

    internal val keyLabel: String
        get() = label

    internal fun canRolloverCommit(x: Float = lastX, y: Float = lastY): Boolean =
        label.length == 1 &&
            label[0] in 'a'..'z' &&
            isPressed &&
            !swipeUpActive &&
            !recording &&
            !spaceScrubbing &&
            !repeatingDelete &&
            !backspaceGesture.isActive &&
            x in -context.dpF(8f)..(width + context.dpF(8f)) &&
            y in -context.dpF(8f)..(height + context.dpF(8f))

    internal fun commitRollover() {
        removeCallbacks(startVoice)
        removeCallbacks(repeatDelete)
        removeCallbacks(armBatchDelete)
        removeCallbacks(batchDelete)
        isPressed = false
        releasedAt = SystemClock.uptimeMillis()
        swipeUpActive = false
        repeatingDelete = false
        spaceScrubbing = false
        batchHoldPending = false
        backspaceGesture.reset()
        performClick()
        invalidate()
    }

    internal fun cancelActiveHold() {
        removeCallbacks(startVoice)
        removeCallbacks(repeatDelete)
        removeCallbacks(armBatchDelete)
        removeCallbacks(batchDelete)
        val wasRecording = recording
        isPressed = false
        swipeUpActive = false
        repeatingDelete = false
        spaceScrubbing = false
        batchHoldPending = false
        recording = false
        backspaceGesture.reset()
        backspaceHold.begin(composing = false)
        if (wasRecording) {
            action("cancel-voice")
        }
        invalidate()
    }

    fun onCompositionChanged(composing: Boolean) {
        if (label == "⌫" && isPressed && !backspaceHold.allowsDelete(composing)) {
            removeCallbacks(repeatDelete)
            removeCallbacks(armBatchDelete)
            removeCallbacks(batchDelete)
            batchHoldPending = false
            backspaceGesture.reset()
            invalidate()
        }
    }

    fun refreshCase(english: Boolean, state: Int) {
        var changed = false
        if (label.length == 1 && label[0] in 'a'..'z') {
            val nextLabel = if (!english || state > 0) label.uppercase() else label
            if (nextLabel != displayLabel) {
                displayLabel = nextLabel
                contentDescription = displayLabel
                changed = true
            }
        }
        if (label == "⇧" && shiftState != state) {
            shiftState = state
            changed = true
        }
        if (changed) invalidate()
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
            glyphPath.apply {
                reset()
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
            glyphPath.apply {
                reset()
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
            glyphPath.apply {
                reset()
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
        val density = context.resources.displayMetrics.density
        val padX = context.dpF(2.2f)
        val padY = min(context.dpF(3.6f), height * 0.075f)

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
        val keyHdp = (keyRect.height() / density).coerceAtLeast(1f)
        val r = min(context.dpF(T.Radius.mobileKeycapPx.toFloat()), keyRect.height() * 0.28f)
        val maxTextDp = (keyHdp * 0.56f).coerceAtLeast(10f)
        val compactScale = (keyHdp / 40f).coerceIn(0.76f, 1f)

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
        val holdIntent = if (label == "⌫") backspaceGesture.intent else BackspaceHoldIntent.INACTIVE

        // Vertical liquid-glass keycap fill (gentle top-to-bottom luminance instead of flat box)
        val topFill: Int
        val bottomFill: Int
        when {
            holdIntent == BackspaceHoldIntent.CLEAR_ALL -> {
                topFill = 0xFFEF4444.toInt()
                bottomFill = 0xFFDC2626.toInt()
            }
            holdIntent == BackspaceHoldIntent.ROTATING -> {
                topFill = if (dark) 0x55EF4444 else 0x38EF4444
                bottomFill = if (dark) 0x38DC2626 else 0x24DC2626
            }
            holdIntent == BackspaceHoldIntent.DELETE_BEFORE || holdIntent == BackspaceHoldIntent.DELETE_AFTER -> {
                topFill = 0xFFF59E0B.toInt()
                bottomFill = 0xFFD97706.toInt()
            }
            holdIntent == BackspaceHoldIntent.BATCH -> {
                topFill = if (dark) 0x48F59E0B else 0x30F59E0B
                bottomFill = if (dark) 0x30D97706 else 0x20D97706
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
        if (
            cachedFillShader == null ||
                cachedFillTopColor != topFill ||
                cachedFillBottomColor != bottomFill ||
                cachedFillTop != keyRect.top ||
                cachedFillBottom != keyRect.bottom
        ) {
            cachedFillTopColor = topFill
            cachedFillBottomColor = bottomFill
            cachedFillTop = keyRect.top
            cachedFillBottom = keyRect.bottom
            cachedFillShader =
                LinearGradient(
                    0f,
                    keyRect.top,
                    0f,
                    keyRect.bottom,
                    topFill,
                    bottomFill,
                    Shader.TileMode.CLAMP,
                )
        }
        paint.shader = cachedFillShader
        c.drawRoundRect(keyRect, r, r, paint)
        paint.shader = null

        // In-key liquid cyan specular bloom when pressed or decaying (150ms)
        if (glowStrength > 0.01f) {
            val baseGlow =
                (if (dark) T.Dark.surfaceKeyPressedGlowArgb else T.Light.surfaceKeyPressedGlowArgb).toInt()
            val glowCx = keyRect.centerX()
            val glowCy = keyRect.centerY()
            val glowRadius = (min(keyRect.width(), keyRect.height()) * 0.85f).coerceAtLeast(1f)
            if (
                cachedGlowShader == null ||
                    cachedGlowColor != baseGlow ||
                    cachedGlowCx != glowCx ||
                    cachedGlowCy != glowCy ||
                    cachedGlowRadius != glowRadius
            ) {
                val baseAlpha = (baseGlow ushr 24) and 0xFF
                val edgeColor = ((baseAlpha * 0.45f).toInt() shl 24) or (baseGlow and 0x00FFFFFF)
                cachedGlowColor = baseGlow
                cachedGlowCx = glowCx
                cachedGlowCy = glowCy
                cachedGlowRadius = glowRadius
                cachedGlowShader =
                    RadialGradient(
                        glowCx,
                        glowCy,
                        glowRadius,
                        baseGlow,
                        edgeColor,
                        Shader.TileMode.CLAMP,
                    )
            }
            glowPaint.style = Paint.Style.FILL
            glowPaint.shader = cachedGlowShader
            glowPaint.alpha = (glowStrength * 255f).toInt().coerceIn(0, 255)
            c.drawRoundRect(keyRect, r, r, glowPaint)
            glowPaint.alpha = 255
            glowPaint.shader = null
            if (!isPressed && !recording) postInvalidateOnAnimation()
        }

        // Ultra-delicate glass border (never a harsh wireframe box!) + luminous top specular edge
        val borderTopColor =
            when {
                holdIntent == BackspaceHoldIntent.CLEAR_ALL || holdIntent == BackspaceHoldIntent.ROTATING ->
                    0xFFEF4444.toInt()
                holdIntent != BackspaceHoldIntent.INACTIVE -> 0xFFF59E0B.toInt()
                glowStrength > 0.2f -> (if (dark) T.Dark.accentTextArgb else T.Light.accentPrimaryArgb).toInt()
                shiftActive -> (if (dark) 0x557DD3FC else 0x440284C7)
                role == KeyRole.ACCENT -> 0x55FFFFFF
                dark -> 0x26FFFFFF
                else -> 0xE8FFFFFF.toInt()
            }
        val borderBottomColor =
            when {
                holdIntent == BackspaceHoldIntent.CLEAR_ALL || holdIntent == BackspaceHoldIntent.ROTATING ->
                    0xCCDC2626.toInt()
                holdIntent != BackspaceHoldIntent.INACTIVE -> 0xCCD97706.toInt()
                glowStrength > 0.2f -> (if (dark) 0x4438BDF8 else 0x440284C7)
                role == KeyRole.ACCENT -> 0x18FFFFFF
                dark -> 0x0AFFFFFF
                else -> 0x180F172A
            }
        if (
            cachedBorderShader == null ||
                cachedBorderTopColor != borderTopColor ||
                cachedBorderBottomColor != borderBottomColor ||
                cachedBorderTop != keyRect.top ||
                cachedBorderBottom != keyRect.bottom
        ) {
            cachedBorderTopColor = borderTopColor
            cachedBorderBottomColor = borderBottomColor
            cachedBorderTop = keyRect.top
            cachedBorderBottom = keyRect.bottom
            cachedBorderShader =
                LinearGradient(
                    0f,
                    keyRect.top,
                    0f,
                    keyRect.bottom,
                    borderTopColor,
                    borderBottomColor,
                    Shader.TileMode.CLAMP,
                )
        }
        borderPaint.shader = cachedBorderShader
        c.drawRoundRect(keyRect, r, r, borderPaint)
        borderPaint.shader = null

        // Subtle charging progress bar during a continuous 1-second hold on Backspace
        if (label == "⌫" && isPressed && batchHoldPending && !backspaceGesture.isActive && pressedAt > 0L) {
            val elapsed = (now - pressedAt).coerceAtLeast(0L)
            if (elapsed >= CLEAR_ALL_PROGRESS_START_MS) {
                val frac =
                    ((elapsed - CLEAR_ALL_PROGRESS_START_MS).toFloat() /
                        (CLEAR_ALL_HOLD_MS - CLEAR_ALL_PROGRESS_START_MS).toFloat())
                        .coerceIn(0f, 1f)
                val barInset = context.dpF(4f)
                val barH = context.dpF(2.5f)
                val barW = ((keyRect.width() - barInset * 2f) * frac).coerceAtLeast(0f)
                paint.shader = null
                paint.style = Paint.Style.FILL
                paint.color = if (dark) 0xDDFBBF24.toInt() else 0xDDD97706.toInt()
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
            drawCenteredText(
                c,
                upper,
                cx,
                cy,
                20f.coerceAtMost(maxTextDp),
                accentText,
                Paint.Align.CENTER,
                bold = true,
            )
            return
        }

        when (label) {
            "⇧" -> drawShiftGlyph(c, cx, cy, primaryText)
            "⌫" -> {
                when (holdIntent) {
                    BackspaceHoldIntent.CLEAR_ALL ->
                        drawCenteredText(
                            c,
                            "松手清空",
                            cx,
                            cy,
                            10.5f.coerceAtMost(maxTextDp),
                            0xFFFFFFFF.toInt(),
                            Paint.Align.CENTER,
                            bold = true,
                        )
                    BackspaceHoldIntent.ROTATING ->
                        drawCenteredText(
                            c,
                            "↻ 画圈",
                            cx,
                            cy,
                            11.0f.coerceAtMost(maxTextDp),
                            if (dark) 0xFFFCA5A5.toInt() else 0xFFDC2626.toInt(),
                            Paint.Align.CENTER,
                            bold = true,
                        )
                    BackspaceHoldIntent.DELETE_BEFORE ->
                        drawCenteredText(
                            c,
                            "↑ 删前文",
                            cx,
                            cy,
                            10.5f.coerceAtMost(maxTextDp),
                            0xFFFFFFFF.toInt(),
                            Paint.Align.CENTER,
                            bold = true,
                        )
                    BackspaceHoldIntent.DELETE_AFTER ->
                        drawCenteredText(
                            c,
                            "↓ 删下文",
                            cx,
                            cy,
                            10.5f.coerceAtMost(maxTextDp),
                            0xFFFFFFFF.toInt(),
                            Paint.Align.CENTER,
                            bold = true,
                        )
                    BackspaceHoldIntent.BATCH ->
                        drawCenteredText(
                            c,
                            "批量删",
                            cx,
                            cy,
                            11.0f.coerceAtMost(maxTextDp),
                            if (dark) 0xFFFDE68A.toInt() else 0xFFB45309.toInt(),
                            Paint.Align.CENTER,
                            bold = true,
                        )
                    BackspaceHoldIntent.INACTIVE -> drawBackspaceGlyph(c, cx, cy, primaryText)
                }
            }
            "↵" -> {
                if (displayLabel == "↵" || displayLabel.isEmpty()) {
                    drawReturnGlyph(c, cx, cy, primaryText)
                } else {
                    drawCenteredText(
                        c,
                        displayLabel,
                        cx,
                        cy,
                        13.5f.coerceAtMost(maxTextDp),
                        primaryText,
                        Paint.Align.CENTER,
                        bold = true,
                    )
                }
            }
            "空格" -> {
                if (spaceScrubbing) {
                    drawCenteredText(
                        c,
                        "◂  移动光标  ▸",
                        cx,
                        cy,
                        12.5f.coerceAtMost(maxTextDp),
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
                        12f.coerceAtMost(maxTextDp),
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
                    }.coerceAtMost(maxTextDp)
                val yOffset =
                    when {
                        keyHdp < 34f -> 0f
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
                    val subInsetY = min(context.dpF(7.2f), keyRect.height() * 0.25f)
                    drawCenteredText(
                        c,
                        upper,
                        if (flypy) keyRect.left + subInsetX else keyRect.right - subInsetX,
                        keyRect.top + subInsetY,
                        8.2f * compactScale,
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
                            keyRect.bottom - min(context.dpF(5.8f), keyRect.height() * 0.22f),
                            7.8f * compactScale,
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
                removeCallbacks(startVoice)
                removeCallbacks(repeatDelete)
                removeCallbacks(armBatchDelete)
                removeCallbacks(batchDelete)
                downX = e.x
                downY = e.y
                lastX = e.x
                lastY = e.y
                spaceAnchorX = e.x
                spaceScrubbing = false
                swipeUpActive = false
                repeatingDelete = false
                batchHoldPending = false
                backspaceGesture.reset()
                pressedAt = e.eventTime
                releasedAt = 0L
                isPressed = true
                if (voice != null) {
                    postDelayed(startVoice, T.Ergonomics.spacebarVoiceHoldThresholdMs.toLong())
                }
                if (label == "⌫") {
                    action("⌫-hold-begin")
                    backspaceHold.begin(isComposing())
                    batchHoldPending = !backspaceHold.startedComposing
                    postDelayed(repeatDelete, 360L)
                    if (batchHoldPending) postDelayed(armBatchDelete, CLEAR_ALL_HOLD_MS)
                }
            }
            MotionEvent.ACTION_MOVE -> {
                lastX = e.x
                lastY = e.y
                if (!backspaceGesture.isActive && (abs(e.x - downX) > context.dp(14) || abs(e.y - downY) > context.dp(14))) {
                    removeCallbacks(startVoice)
                    removeCallbacks(repeatDelete)
                    removeCallbacks(armBatchDelete)
                    batchHoldPending = false
                }
                if (label == "⌫" && backspaceGesture.isActive) {
                    val prevIntent = backspaceGesture.intent
                    for (h in 0 until e.historySize) {
                        backspaceGesture.update(e.getHistoricalX(h), e.getHistoricalY(h))
                    }
                    val nextIntent = backspaceGesture.update(e.x, e.y)
                    if (prevIntent != BackspaceHoldIntent.CLEAR_ALL && nextIntent == BackspaceHoldIntent.CLEAR_ALL) {
                        performHapticFeedback(HapticFeedbackConstants.CONTEXT_CLICK)
                    }
                    if (prevIntent == BackspaceHoldIntent.BATCH && nextIntent != BackspaceHoldIntent.BATCH) {
                        removeCallbacks(batchDelete)
                    } else if (prevIntent != BackspaceHoldIntent.BATCH && nextIntent == BackspaceHoldIntent.BATCH) {
                        removeCallbacks(batchDelete)
                        postDelayed(batchDelete, BATCH_DELETE_INTERVAL_MS)
                    }
                }
                if (label == "空格" && !recording && canScrubSpace()) {
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
                swipeUpActive = !spaceScrubbing && !backspaceGesture.isActive && upper.isNotEmpty() && downY - e.y > context.dp(16)
            }
            MotionEvent.ACTION_UP -> {
                removeCallbacks(startVoice)
                removeCallbacks(repeatDelete)
                removeCallbacks(armBatchDelete)
                removeCallbacks(batchDelete)
                if (label == "⌫" && backspaceGesture.isActive) {
                    val prevIntent = backspaceGesture.intent
                    for (h in 0 until e.historySize) {
                        backspaceGesture.update(e.getHistoricalX(h), e.getHistoricalY(h))
                    }
                    val nextIntent = backspaceGesture.update(e.x, e.y)
                    if (prevIntent != BackspaceHoldIntent.CLEAR_ALL && nextIntent == BackspaceHoldIntent.CLEAR_ALL) {
                        performHapticFeedback(HapticFeedbackConstants.CONTEXT_CLICK)
                    }
                }
                isPressed = false
                releasedAt = SystemClock.uptimeMillis()
                val didSwipeUp = swipeUpActive
                val didRepeat = repeatingDelete
                val didScrub = spaceScrubbing
                val holdIntent = backspaceGesture.intent
                swipeUpActive = false
                repeatingDelete = false
                spaceScrubbing = false
                batchHoldPending = false
                backspaceGesture.reset()
                when {
                    recording -> {
                        recording = false
                        voice?.invoke(false)
                    }
                    holdIntent == BackspaceHoldIntent.CLEAR_ALL -> {
                        action("clear-all")
                    }
                    holdIntent == BackspaceHoldIntent.DELETE_BEFORE -> {
                        performHapticFeedback(HapticFeedbackConstants.CONTEXT_CLICK)
                        action("delete-before-cursor")
                    }
                    holdIntent == BackspaceHoldIntent.DELETE_AFTER -> {
                        performHapticFeedback(HapticFeedbackConstants.CONTEXT_CLICK)
                        action("delete-after-cursor")
                    }
                    holdIntent != BackspaceHoldIntent.INACTIVE -> Unit
                    didRepeat || didScrub -> Unit
                    label == "⌫" && !backspaceHold.allowsDelete(isComposing()) -> Unit
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
                removeCallbacks(armBatchDelete)
                removeCallbacks(batchDelete)
                isPressed = false
                releasedAt = SystemClock.uptimeMillis()
                swipeUpActive = false
                repeatingDelete = false
                spaceScrubbing = false
                batchHoldPending = false
                backspaceGesture.reset()
                if (recording) {
                    recording = false
                    action("cancel-voice")
                }
            }
        }
        if (e.actionMasked == MotionEvent.ACTION_UP || e.actionMasked == MotionEvent.ACTION_CANCEL)
            backspaceHold.begin(composing = false)
        invalidate()
        return true
    }

    override fun onDetachedFromWindow() {
        removeCallbacks(startVoice)
        removeCallbacks(repeatDelete)
        removeCallbacks(armBatchDelete)
        removeCallbacks(batchDelete)
        batchHoldPending = false
        backspaceGesture.reset()
        super.onDetachedFromWindow()
    }

    companion object {
        internal const val CLEAR_ALL_HOLD_MS = HOLD_BATCH_MS
        private const val CLEAR_ALL_PROGRESS_START_MS = 360L
        private val regularTypeface: Typeface by lazy { Typeface.create("sans-serif", Typeface.NORMAL) }
        private val mediumTypeface: Typeface by lazy { Typeface.create("sans-serif-medium", Typeface.NORMAL) }
        private val boldTypeface: Typeface by lazy { Typeface.create("sans-serif-medium", Typeface.BOLD) }
    }
}

internal fun alternate(row: Int, col: Int) = alternates[row][col].toString()

internal fun hapticParamsFor(strength: String, isLetter: Boolean): Pair<Long, Int>? =
    when (strength) {
        "off" -> null
        "light" -> (if (isLetter) 6L else 10L) to 48
        "strong" -> (if (isLetter) 16L else 24L) to 180
        else ->
            (if (isLetter) T.Ergonomics.hapticLetterTickMs.toLong()
            else T.Ergonomics.hapticActionThudMs.toLong()) to 110
    }

// Complete standard Mandarin syllable set for fast O(1) spatial-hitbox border rescue.
private val MANDARIN_SYLLABLES: Set<String> by lazy {
    setOf(
        "a", "ai", "an", "ang", "ao",
        "ba", "bai", "ban", "bang", "bao", "bei", "ben", "beng", "bi", "bian", "biao", "bie", "bin", "bing", "bo", "bu",
        "ca", "cai", "can", "cang", "cao", "ce", "cen", "ceng", "cha", "chai", "chan", "chang", "chao", "che", "chen", "cheng", "chi", "chong", "chou", "chu", "chua", "chuai", "chuan", "chuang", "chui", "chun", "chuo", "ci", "cong", "cou", "cu", "cuan", "cui", "cun", "cuo",
        "da", "dai", "dan", "dang", "dao", "de", "dei", "den", "deng", "di", "dia", "dian", "diao", "die", "ding", "diu", "dong", "dou", "du", "duan", "dui", "dun", "duo",
        "e", "ei", "en", "eng", "er",
        "fa", "fan", "fang", "fei", "fen", "feng", "fiao", "fo", "fou", "fu",
        "ga", "gai", "gan", "gang", "gao", "ge", "gei", "gen", "geng", "gong", "gou", "gu", "gua", "guai", "guan", "guang", "gui", "gun", "guo",
        "ha", "hai", "han", "hang", "hao", "he", "hei", "hen", "heng", "hong", "hou", "hu", "hua", "huai", "huan", "huang", "hui", "hun", "huo",
        "ji", "jia", "jian", "jiang", "jiao", "jie", "jin", "jing", "jiong", "jiu", "ju", "juan", "jue", "jun",
        "ka", "kai", "kan", "kang", "kao", "ke", "kei", "ken", "keng", "kong", "kou", "ku", "kua", "kuai", "kuan", "kuang", "kui", "kun", "kuo",
        "la", "lai", "lan", "lang", "lao", "le", "lei", "leng", "li", "lia", "lian", "liang", "liao", "lie", "lin", "ling", "liu", "lo", "long", "lou", "lu", "luan", "lun", "luo", "lv", "lve",
        "ma", "mai", "man", "mang", "mao", "me", "mei", "men", "meng", "mi", "mian", "miao", "mie", "min", "ming", "miu", "mo", "mou", "mu",
        "na", "nai", "nan", "nang", "nao", "ne", "nei", "nen", "neng", "ni", "nian", "niang", "niao", "nie", "nin", "ning", "niu", "nong", "nou", "nu", "nuan", "nun", "nuo", "nv", "nve",
        "o", "ou",
        "pa", "pai", "pan", "pang", "pao", "pei", "pen", "peng", "pi", "pian", "piao", "pie", "pin", "ping", "po", "pou", "pu",
        "qi", "qia", "qian", "qiang", "qiao", "qie", "qin", "qing", "qiong", "qiu", "qu", "quan", "que", "qun",
        "ran", "rang", "rao", "re", "ren", "reng", "ri", "rong", "rou", "ru", "rua", "ruan", "rui", "run", "ruo",
        "sa", "sai", "san", "sang", "sao", "se", "sen", "seng", "sha", "shai", "shan", "shang", "shao", "she", "shei", "shen", "sheng", "shi", "shou", "shu", "shua", "shuai", "shuan", "shuang", "shui", "shun", "shuo", "si", "song", "sou", "su", "suan", "sui", "sun", "suo",
        "ta", "tai", "tan", "tang", "tao", "te", "tei", "teng", "ti", "tian", "tiao", "tie", "ting", "tong", "tou", "tu", "tuan", "tui", "tun", "tuo",
        "wa", "wai", "wan", "wang", "wei", "wen", "weng", "wo", "wu",
        "xi", "xia", "xian", "xiang", "xiao", "xie", "xin", "xing", "xiong", "xiu", "xu", "xuan", "xue", "xun",
        "ya", "yan", "yang", "yao", "ye", "yi", "yin", "ying", "yo", "yong", "you", "yu", "yuan", "yue", "yun",
        "za", "zai", "zan", "zang", "zao", "ze", "zei", "zen", "zeng", "zha", "zhai", "zhan", "zhang", "zhao", "zhe", "zhei", "zhen", "zheng", "zhi", "zhong", "zhou", "zhu", "zhua", "zhuai", "zhuan", "zhuang", "zhui", "zhun", "zhuo", "zi", "zong", "zou", "zu", "zuan", "zui", "zun", "zuo",
    )
}

private val QUANPIN_PREFIXES: Set<String> by lazy {
    HashSet<String>(1024).apply {
        for (s in MANDARIN_SYLLABLES) {
            for (len in 1..s.length) add(s.substring(0, len))
        }
    }
}

private val XIAOHE_SYLLABLES: Set<String> by lazy {
    val finalMap =
        mapOf(
            "iu" to 'q', "ei" to 'w', "e" to 'e', "uan" to 'r', "ue" to 't', "ve" to 't',
            "un" to 'y', "u" to 'u', "i" to 'i', "o" to 'o', "uo" to 'o', "ie" to 'p',
            "a" to 'a', "ong" to 's', "iong" to 's', "ai" to 'd', "en" to 'f', "eng" to 'g',
            "ang" to 'h', "an" to 'j', "ing" to 'k', "uai" to 'k', "iang" to 'l', "uang" to 'l',
            "ou" to 'z', "ia" to 'x', "ua" to 'x', "ao" to 'c', "v" to 'v', "ui" to 'v',
            "in" to 'b', "iao" to 'n', "ian" to 'm',
        )
    HashSet<String>(512).apply {
        // Zero-initial Xiaohe pairs + j/q/x/y + v aliases accepted by core (e.g. qv -> qu)
        addAll(listOf("aa", "ai", "an", "ah", "ao", "ee", "ei", "en", "eg", "er", "oo", "ou", "jv", "qv", "xv", "yv"))
        for (syl in MANDARIN_SYLLABLES) {
            val (initKey, rest) =
                when {
                    syl.startsWith("zh") -> 'v' to syl.substring(2)
                    syl.startsWith("ch") -> 'i' to syl.substring(2)
                    syl.startsWith("sh") -> 'u' to syl.substring(2)
                    syl[0] !in "aeo" && syl.length >= 2 -> syl[0] to syl.substring(1)
                    else -> continue
                }
            val finKey = finalMap[rest] ?: continue
            add("$initKey$finKey")
        }
    }
}

internal fun isLegalNextPinyinChar(rawPreedit: String, flypy: Boolean, nextChar: Char): Boolean {
    val ch = nextChar.lowercaseChar()
    if (ch !in 'a'..'z') return true
    val seg = rawPreedit.substringAfterLast('\'').lowercase()
    if (seg.isEmpty()) return true
    if (flypy) {
        // Odd length means expecting the 2nd key (final) of a Xiaohe pair.
        return if (seg.length % 2 == 1) {
            "${seg.last()}$ch" in XIAOHE_SYLLABLES
        } else {
            // Even length starts a new Xiaohe syllable (any letter a..z can be an initial).
            true
        }
    }
    // Quanpin: check whether the trailing partial syllable can legally accept `ch`,
    // or whether the previous text ended on a complete syllable (starting a new syllable).
    val maxTail = min(seg.length, 6)
    for (len in maxTail downTo 1) {
        val tail = seg.substring(seg.length - len)
        if ("$tail$ch" in QUANPIN_PREFIXES) return true
    }
    // If the trailing 1..6 chars already form a complete syllable, `ch` can start the next syllable
    // (unless `ch` is 'i'/'u'/'v' which cannot start a Mandarin syllable).
    if (ch !in setOf('i', 'u', 'v')) {
        for (len in maxTail downTo 1) {
            if (seg.substring(seg.length - len) in MANDARIN_SYLLABLES) return true
        }
    }
    return false
}

/**
 * Spatial-hitbox border rescue: when a tap falls inside the outer 22% edge of a letter key
 * and that key cannot legally continue the active Chinese syllable while the immediate neighbor
 * CAN, smoothly redirect the hit to the legal neighbor. Never overrides when both keys are legal
 * or during English typing.
 */
internal fun pickSpatialHitKey(
    rawPreedit: String,
    flypy: Boolean,
    english: Boolean,
    xInKey: Float,
    keyWidth: Float,
    primaryChar: Char,
    leftNeighbor: Char?,
    rightNeighbor: Char?,
): Char {
    if (english || rawPreedit.isEmpty() || keyWidth <= 0f) return primaryChar
    val primary = primaryChar.lowercaseChar()
    if (primary !in 'a'..'z') return primaryChar
    val border = keyWidth * 0.22f
    val neighbor =
        when {
            xInKey < border && leftNeighbor != null -> leftNeighbor.lowercaseChar()
            xInKey > keyWidth - border && rightNeighbor != null -> rightNeighbor.lowercaseChar()
            else -> return primaryChar
        }
    if (neighbor !in 'a'..'z') return primaryChar
    val primaryLegal = isLegalNextPinyinChar(rawPreedit, flypy, primary)
    if (primaryLegal) return primaryChar
    return if (isLegalNextPinyinChar(rawPreedit, flypy, neighbor)) neighbor else primaryChar
}

internal data class SpatialKeyCenter(
    val ch: Char,
    val cx: Float,
    val cy: Float,
    val width: Float,
    val height: Float,
)

internal data class SpatialTouchHint(
    val alt1Char: Int = 0,
    val alt1Penalty: Float = 0f,
    val alt2Char: Int = 0,
    val alt2Penalty: Float = 0f,
) {
    val isEmpty: Boolean
        get() = alt1Char == 0
}

/**
 * Lightweight per-session EMA estimator for single-handed thumb landing bias.
 * Updates only on clean inner taps so wild misses never poison the offset.
 */
internal class ThumbBiasEstimator {
    var biasX: Float = 0f
        private set
    var biasY: Float = 0f
        private set

    fun observe(dx: Float, dy: Float, keyWidth: Float, keyHeight: Float) {
        if (!dx.isFinite() || !dy.isFinite() || !keyWidth.isFinite() || !keyHeight.isFinite()) return
        if (keyWidth <= 0f || keyHeight <= 0f) return
        if (abs(dx) > keyWidth * 0.45f || abs(dy) > keyHeight * 0.45f) return
        val alpha = 0.08f
        val maxX = keyWidth * 0.25f
        val maxY = keyHeight * 0.25f
        biasX = (biasX + alpha * (dx - biasX)).coerceIn(-maxX, maxX)
        biasY = (biasY + alpha * (dy - biasY)).coerceIn(-maxY, maxY)
    }

    fun reset() {
        biasX = 0f
        biasY = 0f
    }
}

internal fun resolveAutoCapMode(fieldKind: String, inputType: Int): Int {
    if (fieldKind != "text" || inputType == android.text.InputType.TYPE_NULL) return 0
    return when {
        inputType and android.text.InputType.TYPE_TEXT_FLAG_CAP_CHARACTERS != 0 -> 3
        inputType and android.text.InputType.TYPE_TEXT_FLAG_CAP_WORDS != 0 -> 2
        inputType and android.text.InputType.TYPE_TEXT_FLAG_CAP_SENTENCES != 0 -> 1
        else -> 0
    }
}

internal fun shouldAutoCapitalize(
    autoCapEnabled: Boolean,
    autoCapMode: Int,
    beforeCursor: String?,
    manualLowercaseOverride: Boolean,
): Boolean {
    if (!autoCapEnabled || autoCapMode == 0 || manualLowercaseOverride) return false
    val before = beforeCursor ?: return false
    return when (autoCapMode) {
        3 -> true
        2 -> before.isEmpty() || before.last().isWhitespace()
        1 ->
            before.isEmpty() ||
                before.endsWith("\n") ||
                before.endsWith(". ") ||
                before.endsWith("! ") ||
                before.endsWith("? ")
        else -> false
    }
}

/**
 * 2D Gaussian soft spatial model over all letter keys (horizontal, vertical, diagonal 8-neighborhood).
 * Returns up to 2 spatially nearby alternative lowercase letters ordered by increasing log-penalty.
 */
internal fun computeSpatialTouchHint(
    tapX: Float,
    tapY: Float,
    primary: SpatialKeyCenter,
    allLetters: List<SpatialKeyCenter>,
    biasX: Float = 0f,
    biasY: Float = 0f,
): SpatialTouchHint {
    val pChar = primary.ch.lowercaseChar()
    if (pChar !in 'a'..'z' || primary.width <= 0f || primary.height <= 0f) {
        return SpatialTouchHint()
    }
    val adjX = tapX - biasX
    val adjY = tapY - biasY
    val sigmaX = primary.width * 0.36f
    val sigmaY = primary.height * 0.40f
    val dx0 = (adjX - primary.cx) / sigmaX
    val dy0 = (adjY - primary.cy) / sigmaY
    val d0Sq = dx0 * dx0 + dy0 * dy0

    var best1Char = 0
    var best1Pen = Float.POSITIVE_INFINITY
    var best2Char = 0
    var best2Pen = Float.POSITIVE_INFINITY

    for (cand in allLetters) {
        val ch = cand.ch.lowercaseChar()
        if (ch !in 'a'..'z' || ch == pChar) continue
        val rawDx = abs(adjX - cand.cx)
        val rawDy = abs(adjY - cand.cy)
        if (rawDx > primary.width * 1.35f || rawDy > primary.height * 1.35f) continue
        val nx = (adjX - cand.cx) / sigmaX
        val ny = (adjY - cand.cy) / sigmaY
        val dSq = nx * nx + ny * ny
        val penalty = ((dSq - d0Sq).coerceAtLeast(0.15f)) * 0.5f
        if (penalty > 3.0f) continue
        val code = ch.code
        if (penalty < best1Pen) {
            best2Char = best1Char
            best2Pen = best1Pen
            best1Char = code
            best1Pen = penalty
        } else if (penalty < best2Pen && code != best1Char) {
            best2Char = code
            best2Pen = penalty
        }
    }
    if (best1Char == 0) return SpatialTouchHint()
    return SpatialTouchHint(
        alt1Char = best1Char,
        alt1Penalty = best1Pen,
        alt2Char = if (best2Char != 0) best2Char else 0,
        alt2Penalty = if (best2Char != 0) best2Pen else 0f,
    )
}

/**
 * Unified keyboard touch coordinator:
 * - Two-thumb key rollover (earlier letter key commits immediately on 2nd pointer down)
 * - Dynamic spatial hitbox border rescue on 26-key Chinese layout
 * - 2D 8-neighborhood soft touch alternatives + personal EMA thumb bias calibration
 * - Android 10+ edge system-gesture exclusion along left/right letter borders
 */
internal class KeyboardTouchHost(ctx: Context) : LinearLayout(ctx) {
    var touchErgonomicsEnabled: Boolean = false
    var preeditProvider: () -> String = { "" }
    var flypyProvider: () -> Boolean = { true }
    var englishProvider: () -> Boolean = { false }

    private data class PendingTouchSample(
        val hint: SpatialTouchHint,
        val dx: Float,
        val dy: Float,
        val keyWidth: Float,
        val keyHeight: Float,
    )

    private val activePointers = LinkedHashMap<Int, Key>()
    private val pointerSamples = HashMap<Int, PendingTouchSample>()
    private val thumbBias = ThumbBiasEstimator()
    private var committedTouchSample: PendingTouchSample? = null
    private val gestureExclusions = ArrayList<Rect>(2)

    fun consumeCommittedTouchHint(recordBias: Boolean = true): SpatialTouchHint {
        val sample = committedTouchSample
        committedTouchSample = null
        if (sample == null) return SpatialTouchHint()
        if (recordBias) {
            thumbBias.observe(sample.dx, sample.dy, sample.keyWidth, sample.keyHeight)
        }
        return sample.hint
    }

    init {
        orientation = VERTICAL
    }

    override fun onLayout(changed: Boolean, l: Int, t: Int, r: Int, b: Int) {
        super.onLayout(changed, l, t, r, b)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            gestureExclusions.clear()
            val w = r - l
            val h = b - t
            if (touchErgonomicsEnabled && w > 0 && h > 0) {
                val edgeW = context.dp(24).coerceAtMost(w / 6)
                val maxH = context.dp(200)
                val exclH = min(h, maxH)
                val top = ((h - exclH) / 2).coerceAtLeast(0)
                gestureExclusions.add(Rect(0, top, edgeW, top + exclH))
                gestureExclusions.add(Rect(w - edgeW, top, w, top + exclH))
            }
            systemGestureExclusionRects = gestureExclusions
        }
    }

    private fun resolveKey(hostX: Float, hostY: Float): Key? {
        for (rowIdx in 0 until childCount) {
            val row = getChildAt(rowIdx) as? LinearLayout ?: continue
            if (hostY < row.top || hostY > row.bottom) continue
            val rowX = hostX - row.left
            val keysInRow = ArrayList<Key>(row.childCount)
            for (i in 0 until row.childCount) {
                (row.getChildAt(i) as? Key)?.let { keysInRow.add(it) }
            }
            if (keysInRow.isEmpty()) return null
            var hitIdx = -1
            for (i in keysInRow.indices) {
                val k = keysInRow[i]
                if (rowX >= k.left && rowX <= k.right) {
                    hitIdx = i
                    break
                }
            }
            if (hitIdx < 0) {
                // Clamp edge margins to nearest key in this row (prevents dead zones on curved edges).
                hitIdx = if (rowX < keysInRow.first().left) 0 else keysInRow.lastIndex
            }
            return keysInRow[hitIdx]
        }
        return null
    }

    private fun dispatchLocal(
        key: Key,
        action: Int,
        source: MotionEvent,
        pointerIndex: Int,
    ) {
        val row = key.parent as? View ?: return
        val offsetX = (row.left + key.left).toFloat()
        val offsetY = (row.top + key.top).toFloat()
        val localX = source.getX(pointerIndex) - offsetX
        val localY = source.getY(pointerIndex) - offsetY
        val synthetic =
            if (action == MotionEvent.ACTION_MOVE && source.historySize > 0) {
                MotionEvent.obtain(
                    source.downTime,
                    source.getHistoricalEventTime(0),
                    action,
                    source.getHistoricalX(pointerIndex, 0) - offsetX,
                    source.getHistoricalY(pointerIndex, 0) - offsetY,
                    0,
                ).apply {
                    for (h in 1 until source.historySize) {
                        addBatch(
                            source.getHistoricalEventTime(h),
                            source.getHistoricalX(pointerIndex, h) - offsetX,
                            source.getHistoricalY(pointerIndex, h) - offsetY,
                            0f,
                            0f,
                            0,
                        )
                    }
                    addBatch(source.eventTime, localX, localY, 0f, 0f, 0)
                }
            } else {
                MotionEvent.obtain(
                    source.downTime,
                    source.eventTime,
                    action,
                    localX,
                    localY,
                    0,
                )
            }
        try {
            key.onTouchEvent(synthetic)
        } finally {
            synthetic.recycle()
        }
    }

    fun resetActiveTouches() {
        activePointers.clear()
        pointerSamples.clear()
    }

    fun cancelAllTouches() {
        for (rowIdx in 0 until childCount) {
            val child = getChildAt(rowIdx)
            if (child is Key) {
                child.cancelActiveHold()
            } else if (child is LinearLayout) {
                for (i in 0 until child.childCount) {
                    (child.getChildAt(i) as? Key)?.cancelActiveHold()
                }
            }
        }
        resetActiveTouches()
    }

    private fun computeSampleForTouch(hostX: Float, hostY: Float, target: Key): PendingTouchSample? {
        if (englishProvider()) return null
        val targetChar = target.keyLabel.singleOrNull()?.lowercaseChar() ?: return null
        if (targetChar !in 'a'..'z' || target.width <= 0 || target.height <= 0) {
            return null
        }
        val targetRow = target.parent as? View ?: return null
        val primaryCenter =
            SpatialKeyCenter(
                ch = targetChar,
                cx = targetRow.left + target.left + target.width * 0.5f,
                cy = targetRow.top + target.top + target.height * 0.5f,
                width = target.width.toFloat(),
                height = target.height.toFloat(),
            )
        val letters = ArrayList<SpatialKeyCenter>(26)
        for (rowIdx in 0 until childCount) {
            val row = getChildAt(rowIdx) as? LinearLayout ?: continue
            for (i in 0 until row.childCount) {
                val k = row.getChildAt(i) as? Key ?: continue
                val ch = k.keyLabel.singleOrNull()?.lowercaseChar() ?: continue
                if (ch in 'a'..'z' && k.width > 0 && k.height > 0) {
                    letters.add(
                        SpatialKeyCenter(
                            ch = ch,
                            cx = row.left + k.left + k.width * 0.5f,
                            cy = row.top + k.top + k.height * 0.5f,
                            width = k.width.toFloat(),
                            height = k.height.toFloat(),
                        )
                    )
                }
            }
        }
        val hint =
            computeSpatialTouchHint(
                tapX = hostX,
                tapY = hostY,
                primary = primaryCenter,
                allLetters = letters,
                biasX = thumbBias.biasX,
                biasY = thumbBias.biasY,
            )
        return PendingTouchSample(
            hint = hint,
            dx = hostX - primaryCenter.cx,
            dy = hostY - primaryCenter.cy,
            keyWidth = primaryCenter.width,
            keyHeight = primaryCenter.height,
        )
    }

    override fun dispatchTouchEvent(ev: MotionEvent): Boolean {
        if (!touchErgonomicsEnabled) {
            if (activePointers.isNotEmpty()) resetActiveTouches()
            return super.dispatchTouchEvent(ev)
        }
        when (ev.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                val idx = ev.actionIndex
                val pid = ev.getPointerId(idx)
                val target = resolveKey(ev.getX(idx), ev.getY(idx)) ?: return false
                // Ignore a second finger landing on the exact same active key (e.g. space/backspace).
                if (activePointers.values.any { it === target }) {
                    return true
                }
                if (ev.actionMasked == MotionEvent.ACTION_POINTER_DOWN && activePointers.isNotEmpty()) {
                    val toRollover =
                        activePointers.entries
                            .filter { (oldPid, k) ->
                                if (k === target) return@filter false
                                val oldIdx = ev.findPointerIndex(oldPid)
                                val row = k.parent as? View
                                if (oldIdx >= 0 && row != null) {
                                    k.canRolloverCommit(
                                        ev.getX(oldIdx) - row.left - k.left,
                                        ev.getY(oldIdx) - row.top - k.top,
                                    )
                                } else {
                                    k.canRolloverCommit()
                                }
                            }
                            .map { it.key to it.value }
                    for ((oldPid, oldKey) in toRollover) {
                        activePointers.remove(oldPid)
                        committedTouchSample = pointerSamples.remove(oldPid)
                        try {
                            oldKey.commitRollover()
                        } finally {
                            committedTouchSample = null
                        }
                    }
                }
                activePointers[pid] = target
                val sample = computeSampleForTouch(ev.getX(idx), ev.getY(idx), target)
                if (sample != null) pointerSamples[pid] = sample else pointerSamples.remove(pid)
                dispatchLocal(target, MotionEvent.ACTION_DOWN, ev, idx)
                return true
            }
            MotionEvent.ACTION_MOVE -> {
                for (i in 0 until ev.pointerCount) {
                    val pid = ev.getPointerId(i)
                    val key = activePointers[pid] ?: continue
                    dispatchLocal(key, MotionEvent.ACTION_MOVE, ev, i)
                }
                return true
            }
            MotionEvent.ACTION_POINTER_UP, MotionEvent.ACTION_UP -> {
                val idx = ev.actionIndex
                val pid = ev.getPointerId(idx)
                val key = activePointers.remove(pid)
                val sample = pointerSamples.remove(pid)
                if (key != null) {
                    committedTouchSample = sample
                    try {
                        dispatchLocal(key, MotionEvent.ACTION_UP, ev, idx)
                    } finally {
                        committedTouchSample = null
                    }
                }
                if (ev.actionMasked == MotionEvent.ACTION_UP) {
                    resetActiveTouches()
                }
                return true
            }
            MotionEvent.ACTION_CANCEL -> {
                for ((_, key) in activePointers) {
                    val cancel =
                        MotionEvent.obtain(
                            ev.downTime,
                            ev.eventTime,
                            MotionEvent.ACTION_CANCEL,
                            0f,
                            0f,
                            0,
                        )
                    try {
                        key.onTouchEvent(cancel)
                    } finally {
                        cancel.recycle()
                    }
                }
                resetActiveTouches()
                return true
            }
        }
        return super.dispatchTouchEvent(ev)
    }
}

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
