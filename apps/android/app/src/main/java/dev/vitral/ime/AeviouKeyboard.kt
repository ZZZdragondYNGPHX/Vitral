package dev.vitral.ime

import android.content.Context
import android.graphics.Canvas
import android.graphics.LinearGradient
import android.graphics.Paint
import android.graphics.Path
import android.graphics.PointF
import android.graphics.Shader
import android.graphics.Typeface
import android.os.SystemClock
import android.view.HapticFeedbackConstants
import android.view.MotionEvent
import android.view.View
import dev.vitral.ime.theme.VitralTokens as T
import kotlin.math.PI
import kotlin.math.cos
import kotlin.math.hypot
import kotlin.math.min
import kotlin.math.sin

internal enum class AeviouEnv {
    PURE_ZH,
    MIXED,
    PURE_EN,
}

/**
 * Experimental AEVIOU+ Tri-Mode Adaptive Honeycomb Sliding Keyboard.
 *
 * Explicitly supports THREE tailored environments under the experimental toggle:
 * 1. Pure Chinese (`AeviouEnv.PURE_ZH`):
 *    - 24-hex Pinyin-first honeycomb (20 consonant hexes + `a·e·i·o·u` vowel hub + 3 actions).
 *    - Pure Pinyin Trie ONLY: 6 petals are devoted exclusively to Mandarin finals & Chinese
 *      context shortcuts (`正` -> Q blooms `UE · 确`), with zero English petal clutter.
 * 2. Mixed Chinese-English (`AeviouEnv.MIXED`):
 *    - 24-hex bilingual honeycomb with Dual-Trie simultaneous branching: blooms Pinyin petals
 *      AND seamlessly branches into English completion petals (e.g. `gi` -> `thub · GitHub`,
 *      `ap` -> `p · App`) plus hybrid Chinese + English context gravity.
 * 3. Pure English (`AeviouEnv.PURE_EN`):
 *    - 30-hex all-26-letters honeycomb (`a–z` + `⇧/空格/⌫/↵`) with elastic next-word gravity.
 *    - Pure English Probability Trie: Top-5 conditional next letters + 1 whole-word shortcut
 *      petal + automatic space on slid words.
 */
internal class AeviouKeyboard(
    ctx: Context,
    private val env: AeviouEnv = AeviouEnv.MIXED,
    private var shiftState: Int = 0,
    private val leftHanded: Boolean = false,
    private val darkOverride: Boolean? = null,
    private val onSyllable: (String) -> Unit,
    private val onActionKey: (String) -> Unit,
    private val isComposing: () -> Boolean = { false },
) : View(ctx) {

    private val isPureEnglish: Boolean get() = env == AeviouEnv.PURE_EN
    private val allowMixedEnglish: Boolean get() = env == AeviouEnv.MIXED

    private data class HexCell(
        val id: String,
        var label: String,
        val row: Int,
        val col: Int,
        val isAction: Boolean = false,
        val isAccent: Boolean = false,
        var cx: Float = 0f,
        var cy: Float = 0f,
        var fillShader: LinearGradient? = null,
        var shaderTopCol: Int = 0,
        var shaderBotCol: Int = 0,
        var shaderCy: Float = Float.NaN,
        var shaderR: Float = Float.NaN,
    )

    private data class Petal(
        val append: String,      // 1-letter step OR compound/word shortcut (e.g. "ue", "thub", "he")
        val hintChar: String,    // converging target hint (e.g. "确", "GitHub", "the")
        val cx: Float,
        val cy: Float,
        val isBacktrack: Boolean = false,
        val isShortcut: Boolean = false,
        val weightRank: Int = 0,
    )

    // Contextual next-word/syllable targets: initial letter -> (targetToken, targetDisplay, rank 0..5)
    private var contextTargets: Map<String, Triple<String, String, Int>> = emptyMap()
    private var cachedContextDrawOrder: List<HexCell>? = null

    fun setContextHints(hints: Map<String, Triple<String, String, Int>>) {
        if (contextTargets != hints) {
            contextTargets = hints
            cachedContextDrawOrder =
                if (hints.isEmpty()) cells
                else
                    cells.sortedBy { cell ->
                        val rank = if (!cell.isAction) hints[cell.id]?.third else null
                        if (rank == null) 0 else (10 - rank)
                    }
            if (sliding) refreshSlideHud()
            invalidate()
        }
    }

    private val baseRows: List<List<Triple<String, String, Int>>> =
        run {
            // Right-hand canonical layouts follow the "grid-traversal degree-of-freedom" topology:
            //   - Highest branching + highest frequency keys occupy the 360° inner core, biased right-of-center
            //     for comfortable right-thumb reach without wall collisions.
            //   - Lowest branching / lowest frequency keys sit on corners and outer left edges.
            // Left-hand mode mirrors every row horizontally for symmetric left-thumb ergonomics.
            val rightHandRows =
                if (isPureEnglish) {
                    // 5-row Pure English honeycomb (7/6/7/6/5 = 31 hexes):
                    // All 9 Tier-1 English superstars (R,N,E,T / H,A,O,I,S) occupy 100% of the 9 inner-core cells,
                    // with vowels A/E/I/O clustered in the heart and low-branching Q/X/Z/J/V/K on outer left edges.
                    listOf(
                        listOf(
                            Triple("q", "q", 0), Triple("v", "v", 0), Triple("p", "p", 0), Triple("f", "f", 0),
                            Triple("m", "m", 0), Triple("c", "c", 0), Triple("w", "w", 0),
                        ),
                        listOf(
                            Triple("g", "g", 0), Triple("r", "r", 0), Triple("n", "n", 0),
                            Triple("e", "e", 0), Triple("t", "t", 0), Triple("l", "l", 0),
                        ),
                        listOf(
                            Triple("x", "x", 0), Triple("h", "h", 0), Triple("a", "a", 0), Triple("o", "o", 0),
                            Triple("i", "i", 0), Triple("s", "s", 0), Triple("d", "d", 0),
                        ),
                        listOf(
                            Triple("z", "z", 0), Triple("j", "j", 0), Triple("k", "k", 0),
                            Triple("y", "y", 0), Triple("u", "u", 0), Triple("b", "b", 0),
                        ),
                        listOf(
                            Triple("123", "123", 1), Triple("⇧", "⇧", 1), Triple("空格", "空格", 1),
                            Triple("⌫", "⌫", 1), Triple("↵", "↵", 2),
                        ),
                    )
                } else {
                    // 5-row Chinese Quanpin / Mixed-English honeycomb (5/5/5/5/5 = 25 hexes):
                    // Phonological families stay intact while matching branching degree to board liberties:
                    //   - Inner 6-way core: N,L,D & C,Z,S (all five 6-branch initials + #1 high-freq D), biased right.
                    //   - Outer corners/left: 2-branch Q,X,J clustered bottom-left; low-count F(10), W(9) on top corners.
                    val vowelHub = Triple("aeiou", if (allowMixedEnglish) "aeiou" else "aeo", 0)
                    listOf(
                        listOf(
                            Triple("f", "F", 0), Triple("p", "P", 0), Triple("b", "B", 0),
                            Triple("m", "M", 0), Triple("w", "W", 0),
                        ),
                        listOf(
                            Triple("t", "T", 0), Triple("n", "N", 0), Triple("l", "L", 0),
                            Triple("d", "D", 0), Triple("y", "Y", 0),
                        ),
                        listOf(
                            Triple("r", "R", 0), Triple("c", "C", 0), Triple("z", "Z", 0),
                            Triple("s", "S", 0), Triple("h", "H", 0),
                        ),
                        listOf(
                            Triple("q", "Q", 0), Triple("x", "X", 0), Triple("j", "J", 0),
                            Triple("k", "K", 0), Triple("g", "G", 0),
                        ),
                        listOf(
                            Triple("123", "123", 1), vowelHub, Triple("空格", "空格", 1),
                            Triple("⌫", "⌫", 1), Triple("↵", "↵", 2),
                        ),
                    )
                }
            if (leftHanded) rightHandRows.map { it.reversed() } else rightHandRows
        }

    private val cells: List<HexCell> =
        baseRows.flatMapIndexed { r, row ->
            row.mapIndexed { c, (id, lbl, kind) ->
                val display =
                    if (isPureEnglish && kind == 0 && shiftState > 0) lbl.uppercase() else lbl
                HexCell(id, display, r, c, isAction = kind != 0, isAccent = kind == 2)
            }
        }

    fun refreshCase(state: Int) {
        if (shiftState == state) return
        shiftState = state
        if (isPureEnglish) {
            cells.filter { !it.isAction }.forEach {
                it.label = if (state > 0) it.id.uppercase() else it.id
            }
            invalidate()
        }
    }

    fun onCompositionChanged(composing: Boolean) {
        touches.values.forEach { it.deleteHold.allowsDelete(composing) }
        if (pressedAction?.id == "⌫" && !backspaceHold.allowsDelete(composing)) {
            removeCallbacks(repeatDelete)
            removeCallbacks(armBatchDelete)
            removeCallbacks(batchDelete)
            batchHoldPending = false
            backspaceGesture.reset()
            invalidate()
        }
    }

    private data class Touch(
        val cell: HexCell,
        val x: Float,
        val y: Float,
        val time: Long,
        val deleteHold: BackspaceHold = BackspaceHold(),
        var moved: Boolean = false,
    )

    // Only the owner drives the sliding petals; overlapping fingers keep independent taps.
    private val touches = linkedMapOf<Int, Touch>()
    private var gesturePointerId: Int? = null
    private var backspaceHold = BackspaceHold()
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
    private var hexRadius = 0f
    private var sliding = false
    private var armedSlide = false
    private var stepAnchor = PointF()
    private var pressedAction: HexCell? = null
    private var actionDownX = 0f
    private var actionDownY = 0f
    private var actionLastX = 0f
    private var actionLastY = 0f
    private var actionPressedAt = 0L
    private var repeatingDelete = false
    private var batchHoldPending = false
    private val stepStack = ArrayList<String>()
    private val centerHistory = ArrayList<PointF>()
    private var activeCenter = PointF()
    private var petals: List<Petal> = emptyList()
    private val trailX = FloatArray(TRAIL_CAPACITY)
    private val trailY = FloatArray(TRAIL_CAPACITY)
    private var trailHead = 0
    private var trailSize = 0
    private var cachedSlidePrefix = ""
    private var cachedCenterHint = ""
    private var cachedHudSummary = ""

    private fun clearTrail() {
        trailHead = 0
        trailSize = 0
    }

    private fun pushTrail(x: Float, y: Float) {
        if (trailSize < TRAIL_CAPACITY) {
            val idx = (trailHead + trailSize) % TRAIL_CAPACITY
            trailX[idx] = x
            trailY[idx] = y
            trailSize++
        } else {
            trailX[trailHead] = x
            trailY[trailHead] = y
            trailHead = (trailHead + 1) % TRAIL_CAPACITY
        }
    }

    private fun refreshSlideHud() {
        val cur = currentPrefix()
        val centerHint =
            if (cur.isEmpty()) ""
            else if (isPureEnglish) englishTopHint(cur)
            else {
                val first = cur.substring(0, 1)
                val ctx = contextTargets[first]
                if (ctx != null && ctx.first == cur) ctx.second
                else if (allowMixedEnglish) topHintChar(cur).ifEmpty { englishTopHint(cur) }
                else topHintChar(cur)
            }
        cachedSlidePrefix = cur
        cachedCenterHint = centerHint
        cachedHudSummary =
            buildString {
                append(cur.ifEmpty { "韵母" })
                if (centerHint.isNotEmpty()) append(" · ").append(centerHint)
                var shownCount = 0
                for (p in petals) {
                    if (p.isBacktrack) continue
                    if (shownCount == 0) append("   |   ") else append("  ")
                    val lbl = if (isPureEnglish) p.append else p.append.uppercase()
                    append(lbl)
                    if (p.hintChar.isNotEmpty()) {
                        append(':')
                        append(if (p.hintChar.length > 2) p.hintChar.substring(0, 2) else p.hintChar)
                    }
                    shownCount++
                    if (shownCount >= 4) break
                }
            }
    }

    private val repeatDelete =
        object : Runnable {
            override fun run() {
                if (pressedAction?.id != "⌫" || backspaceGesture.isActive) return
                repeatingDelete = true
                if (!backspaceHold.allowsDelete(isComposing())) return
                onActionKey("⌫-repeat")
                postDelayed(this, 65L)
            }
        }

    private val batchDelete =
        object : Runnable {
            override fun run() {
                if (pressedAction?.id != "⌫" || backspaceGesture.intent != BackspaceHoldIntent.BATCH) return
                if (!backspaceHold.allowsDelete(isComposing())) return
                onActionKey("⌫-batch")
                postDelayed(this, BATCH_DELETE_INTERVAL_MS)
            }
        }

    private val armBatchDelete = Runnable {
        if (pressedAction?.id == "⌫" && batchHoldPending) {
            batchHoldPending = false
            removeCallbacks(repeatDelete)
            if (!backspaceHold.allowsDelete(isComposing())) {
                invalidate()
                return@Runnable
            }
            backspaceGesture.begin(actionLastX, actionLastY)
            performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
            onActionKey("⌫-batch")
            if (pressedAction?.id == "⌫" && backspaceGesture.intent == BackspaceHoldIntent.BATCH) {
                postDelayed(batchDelete, BATCH_DELETE_INTERVAL_MS)
            }
            invalidate()
        }
    }

    private val fillPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply { style = Paint.Style.FILL }
    private val borderPaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = ctx.dpF(1.1f)
            strokeJoin = Paint.Join.ROUND
        }
    private val trailPaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = ctx.dpF(3.2f)
            strokeCap = Paint.Cap.ROUND
            strokeJoin = Paint.Join.ROUND
        }
    private val textPaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            textAlign = Paint.Align.CENTER
            typeface = Typeface.create("sans-serif-medium", Typeface.BOLD)
        }
    private val subPaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            textAlign = Paint.Align.CENTER
            typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
        }
    private val hexPath = Path()
    private val trailPath = Path()

    init {
        contentDescription =
            when (env) {
                AeviouEnv.PURE_ZH -> "AEVIOU 纯中文自适应蜂窝实验键盘"
                AeviouEnv.MIXED -> "AEVIOU 中英混输自适应蜂窝实验键盘"
                AeviouEnv.PURE_EN -> "AEVIOU 纯英文自适应蜂窝实验键盘"
            }
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        super.onSizeChanged(w, h, oldw, oldh)
        layoutHexGrid(w.toFloat(), h.toFloat())
    }

    private fun layoutHexGrid(w: Float, h: Float) {
        if (w <= 0f || h <= 0f) return
        val padLeft = context.dpF(if (leftHanded) 20f else 12f)
        val padRight = context.dpF(if (leftHanded) 12f else 20f)
        val padTop = context.dpF(28f)
        val padBottom = context.dpF(6f)
        val availW = (w - padLeft - padRight).coerceAtLeast(1f)
        val availH = (h - padTop - padBottom).coerceAtLeast(1f)
        val rowCountTotal = baseRows.size // 5 airy rows
        val maxCols = if (isPureEnglish) 7.2f else 5.55f
        val pitchFactor = if (isPureEnglish) 1.09f else 1.20f
        val rByW = availW / (maxCols * 1.73205f * pitchFactor)
        val rByH = availH / ((rowCountTotal - 1) * 1.76f + 2.0f)
        hexRadius = min(rByW, rByH)
        val hexW = hexRadius * 1.73205f
        val colPitchX = hexW * pitchFactor
        // Stretch rowStepY to utilize the full vertical height cleanly (no dead top/bottom void!)
        val rowStepY =
            ((availH - 2f * hexRadius) / (rowCountTotal - 1).coerceAtLeast(1))
                .coerceIn(hexRadius * 1.62f, hexRadius * 2.18f)
        val totalGridH = (rowCountTotal - 1) * rowStepY + 2f * hexRadius
        val startY = padTop + (availH - totalGridH) * 0.5f + hexRadius
        val centerX = padLeft + availW * 0.5f
        for (cell in cells) {
            val colsInRow = baseRows[cell.row].size
            val rowW = (colsInRow - 1) * colPitchX
            val stagger =
                when {
                    isPureEnglish -> 0f // 7 / 6 / 7 / 6 / 5 naturally interlocks symmetrically!
                    cell.row == 4 -> 0f // Bottom action row centered cleanly
                    cell.row % 2 == 1 -> colPitchX * (if (leftHanded) -0.24f else 0.24f)
                    else -> colPitchX * (if (leftHanded) 0.24f else -0.24f)
                }
            cell.cx = centerX - rowW * 0.5f + cell.col * colPitchX + stagger
            cell.cy = startY + cell.row * rowStepY
        }
    }

    private fun buildHexPath(cx: Float, cy: Float, radius: Float, out: Path) {
        out.reset()
        for (i in 0 until 6) {
            val x = cx + radius * HEX_COS[i]
            val y = cy + radius * HEX_SIN[i]
            if (i == 0) out.moveTo(x, y) else out.lineTo(x, y)
        }
        out.close()
    }

    private fun currentPrefix(): String = stepStack.joinToString("")

    private fun matchingContextShortcut(prefix: String): Pair<String, String>? {
        if (prefix.isEmpty()) return null
        val first = prefix.substring(0, 1)
        val (targetToken, targetDisplay, _) = contextTargets[first] ?: return null
        if (targetToken.length > prefix.length && targetToken.startsWith(prefix)) {
            return targetToken.substring(prefix.length) to targetDisplay
        }
        return null
    }

    private fun computePetals(center: PointF, prefix: String) {
        // Keep activeCenter strictly on the real touched key/petal so petals NEVER spawn under a stationary finger!
        activeCenter = PointF(center.x, center.y)

        data class Choice(val append: String, val hint: String, val shortcut: Boolean)
        val choices = ArrayList<Choice>(6)
        val ctxShortcut = matchingContextShortcut(prefix)

        if (isPureEnglish) {
            // Pure English Mode: 1 context/whole-word completion shortcut + Top-5 conditional next letters
            val topWord =
                ctxShortcut ?: englishWordCompletion(prefix)?.let { (word, display) ->
                    word.removePrefix(prefix) to display
                }
            if (topWord != null && topWord.first.length >= 2) {
                choices.add(Choice(topWord.first, topWord.second, shortcut = true))
            }
            val nextLetters = englishNextLetters(prefix, topWord?.first?.take(1))
            val existing = choices.map { it.append }.toHashSet()
            for (letter in nextLetters) {
                if (choices.size >= 5) break
                if (existing.add(letter)) {
                    val np = prefix + letter
                    val hint = englishTopHint(np)
                    choices.add(Choice(letter, hint, shortcut = (topWord != null && topWord.first == letter)))
                }
            }
            if (choices.size < 5) {
                for ((word, display) in englishWordsWithPrefix(prefix)) {
                    val suffix = word.removePrefix(prefix)
                    if (suffix.length >= 2 && existing.add(suffix)) {
                        choices.add(Choice(suffix, display, shortcut = true))
                        if (choices.size >= 5) break
                    }
                }
            }
        } else {
            // Pure Chinese (`PURE_ZH`) OR Mixed Chinese-English (`MIXED`)
            val rawPinyinNext =
                if (prefix.isEmpty()) {
                    if (allowMixedEnglish) listOf("a", "e", "i", "o", "u", "v")
                    else listOf("a", "e", "o", "an", "en", "ai")
                } else nextValidLettersByFreq(prefix, ctxShortcut?.first?.take(1))

            if (allowMixedEnglish && rawPinyinNext.isEmpty() && prefix.isNotEmpty()) {
                // Seamless Mixed-English fallback ONLY in MIXED mode (e.g. "gi" -> "thub·GitHub")
                val engWord = englishWordCompletion(prefix)
                if (engWord != null && engWord.first.length > prefix.length) {
                    choices.add(
                        Choice(
                            engWord.first.removePrefix(prefix),
                            engWord.second,
                            shortcut = true,
                        )
                    )
                }
                for (letter in englishNextLetters(prefix, null)) {
                    if (choices.size >= 5) break
                    val np = prefix + letter
                    choices.add(Choice(letter, englishTopHint(np), shortcut = false))
                }
            } else {
                val canAddExtraShortcuts = rawPinyinNext.size < 5
                if (ctxShortcut != null && ctxShortcut.first.length >= 2 && canAddExtraShortcuts) {
                    choices.add(Choice(ctxShortcut.first, ctxShortcut.second, shortcut = true))
                }
                for (letter in rawPinyinNext) {
                    val nextPrefix = prefix + letter
                    val hint =
                        if (ctxShortcut != null && ctxShortcut.first == letter) ctxShortcut.second
                        else topHintChar(nextPrefix)
                    choices.add(
                        Choice(
                            letter,
                            hint,
                            shortcut = (ctxShortcut != null && ctxShortcut.first == letter),
                        )
                    )
                }
                if (prefix.isNotEmpty() && choices.size < 5) {
                    val existing = choices.map { it.append }.toHashSet()
                    // High-frequency Pinyin compound finals first
                    for ((syl, hint) in topSyllablesWithPrefix(prefix)) {
                        val suffix = syl.removePrefix(prefix)
                        if (suffix.length >= 2 && existing.add(suffix)) {
                            choices.add(Choice(suffix, hint, shortcut = true))
                            if (choices.size >= 5) break
                        }
                    }
                    // Mixed-English tech/everyday shortcut ONLY in MIXED mode (never in PURE_ZH)
                    if (allowMixedEnglish && choices.size < 5 && prefix.length >= 2) {
                        englishWordCompletion(prefix)?.let { (word, display) ->
                            val suffix = word.removePrefix(prefix)
                            if (suffix.isNotEmpty() && existing.add(suffix)) {
                                choices.add(Choice(suffix, display, shortcut = true))
                            }
                        }
                    }
                }
            }
        }

        // True 2-ring Axial Hexagonal Lattice placement:
        // Every cell on Ring 1 (6 direct neighbors) & Ring 2 (12 outer neighbors) is separated by
        // at least `pitch = 1.96 * hexRadius` (~27% clear air gap for `petalR = 0.88 * hexRadius`).
        // Mathematically impossible for any two petals or activeCenter to overlap, even at corners like D!
        val petalR = hexRadius * 0.88f
        val pitch = hexRadius * 1.96f
        val padX = petalR * 1.02f
        val padTop = context.dpF(24f) + petalR * 0.96f
        val padBot = petalR * 1.02f

        data class SlotCand(val pos: PointF, val ring: Int, val cost: Float)
        val validSlots = ArrayList<SlotCand>(18)
        for (q in -2..2) {
            for (r in -2..2) {
                val s = -q - r
                val ring = maxOf(kotlin.math.abs(q), kotlin.math.abs(r), kotlin.math.abs(s))
                if (ring !in 1..2) continue
                val dx = pitch * (q + r * 0.5f)
                val dy = pitch * (r * 0.8660254f)
                val px = activeCenter.x + dx
                val py = activeCenter.y + dy
                if (width > padX * 2 && (px < padX || px > width - padX)) continue
                if (height > padTop + padBot && (py < padTop || py > height - padBot)) continue
                // Ergonomic dominant-hand swipe arc cost:
                // Right-hand thumb rests bottom-right, so the most comfortable & unobstructed swipe directions
                // are Upper-Left (#1) -> Left (#2) -> Upper-Right (#3) -> Lower-Left (#4), while Right (#5)
                // and Lower-Right (#6, under thumb base) are reserved for the rarest combinations.
                // Left-hand mode mirrors symmetrically: Upper-Right (#1) -> Right (#2) -> Upper-Left (#3)...
                val towardThumbRootX = if (leftHanded) -(dx / pitch) else (dx / pitch)
                val distCost = hypot(dx, dy) / pitch * 60f
                val palmPenalty = if (towardThumbRootX > 0.1f && dy > 1f) 24f else 0f
                val cost = ring * 100f + distCost + towardThumbRootX * 26f + (dy / pitch) * 32f + palmPenalty
                validSlots.add(SlotCand(PointF(px, py), ring, cost))
            }
        }
        validSlots.sortBy { it.cost }

        // Reserve closest Ring-1 slot toward previous center as backtrack "↩" when sliding multi-step
        var backtrackIdx = -1
        if (centerHistory.size >= 2) {
            val prev = centerHistory[centerHistory.size - 2]
            var bestD = Float.MAX_VALUE
            validSlots.forEachIndexed { idx, slot ->
                if (slot.ring == 1) {
                    val d = hypot(slot.pos.x - prev.x, slot.pos.y - prev.y)
                    if (d < bestD) {
                        bestD = d
                        backtrackIdx = idx
                    }
                }
            }
        }

        val chosenSlots = validSlots.indices.filter { it != backtrackIdx || choices.size >= validSlots.size }
        val result = ArrayList<Petal>(6)
        choices.take(6).forEachIndexed { idx, ch ->
            val slotIdx = chosenSlots.getOrNull(idx) ?: return@forEachIndexed
            val p = validSlots[slotIdx].pos
            result.add(
                Petal(
                    append = ch.append,
                    hintChar = ch.hint,
                    cx = p.x,
                    cy = p.y,
                    isShortcut = ch.shortcut,
                    weightRank = idx,
                )
            )
        }
        if (backtrackIdx >= 0 && result.size < 6) {
            val p = validSlots[backtrackIdx].pos
            result.add(
                Petal(
                    append = "↩",
                    hintChar = "",
                    cx = p.x,
                    cy = p.y,
                    isBacktrack = true,
                )
            )
        }
        petals = result
        refreshSlideHud()
    }

    override fun onDraw(c: Canvas) {
        val dark = darkOverride ?: context.darkKeyboard
        // Crisp separated hexagonal islands (0.82x visual radius) with comfortable breathing gaps
        val r = (hexRadius * 0.82f).coerceAtLeast(1f)
        val primaryText = (if (dark) T.Dark.textPrimaryArgb else T.Light.textPrimaryArgb).toInt()
        val secondaryText = (if (dark) T.Dark.textSecondaryArgb else T.Light.textSecondaryArgb).toInt()
        val accentText = (if (dark) T.Dark.accentTextArgb else T.Light.accentTextArgb).toInt()
        val accentSolid = (if (dark) T.Dark.accentPrimaryArgb else T.Light.accentPrimaryArgb).toInt()

        // 1. Base honeycomb grid with elastic magnification on context-predicted initials
        val drawOrder = if (sliding) cells else (cachedContextDrawOrder ?: cells)
        for (cell in drawOrder) {
            val ctxHit = if (!cell.isAction) contextTargets[cell.id] else null
            val isCtxGlow = ctxHit != null && !sliding
            val scaleFactor =
                when {
                    !isCtxGlow -> if (contextTargets.isNotEmpty() && !cell.isAction && !sliding) 0.94f else 1.0f
                    ctxHit.third == 0 -> 1.16f
                    ctxHit.third <= 2 -> 1.09f
                    else -> 1.04f
                }
            buildHexPath(cell.cx, cell.cy, r * scaleFactor, hexPath)
            val isPressedAct = pressedAction == cell || (cell.id == "⇧" && shiftState > 0)
            val holdIntent = if (cell.id == "⌫") backspaceGesture.intent else BackspaceHoldIntent.INACTIVE
            val isHoldCell = holdIntent != BackspaceHoldIntent.INACTIVE
            val dim = sliding && !cell.isAction
            val topCol =
                when {
                    holdIntent == BackspaceHoldIntent.CLEAR_ALL -> 0xFFEF4444.toInt()
                    holdIntent == BackspaceHoldIntent.ROTATING -> if (dark) 0x55EF4444 else 0x38EF4444
                    holdIntent == BackspaceHoldIntent.DELETE_BEFORE ||
                        holdIntent == BackspaceHoldIntent.DELETE_AFTER -> 0xFFF59E0B.toInt()
                    holdIntent == BackspaceHoldIntent.BATCH -> if (dark) 0x48F59E0B else 0x30F59E0B
                    cell.isAccent -> if (dark) 0xFF19B5FA.toInt() else 0xFF0A93D9.toInt()
                    isCtxGlow && ctxHit.third == 0 -> if (dark) 0xCC0369A1.toInt() else 0xF5BAE6FD.toInt()
                    isPressedAct || isCtxGlow -> (if (dark) T.Dark.accentSoftArgb else T.Light.accentSoftArgb).toInt()
                    cell.isAction -> if (dark) 0x16FFFFFF else 0x1A0F172A
                    dim -> if (dark) 0x0AFFFFFF else 0x60FFFFFF
                    dark -> 0x1EFFFFFF
                    else -> 0xE8FFFFFF.toInt()
                }
            val botCol =
                when {
                    holdIntent == BackspaceHoldIntent.CLEAR_ALL -> 0xFFDC2626.toInt()
                    holdIntent == BackspaceHoldIntent.ROTATING -> if (dark) 0x38DC2626 else 0x24DC2626
                    holdIntent == BackspaceHoldIntent.DELETE_BEFORE ||
                        holdIntent == BackspaceHoldIntent.DELETE_AFTER -> 0xFFD97706.toInt()
                    holdIntent == BackspaceHoldIntent.BATCH -> if (dark) 0x30D97706 else 0x20D97706
                    cell.isAccent -> accentSolid
                    isCtxGlow && ctxHit.third == 0 -> if (dark) 0xCC075985.toInt() else 0xF0E0F2FE.toInt()
                    isCtxGlow -> if (dark) 0x330EA5E9 else 0x260284C7
                    cell.isAction -> (if (dark) T.Dark.surfaceKeyFunctionArgb else T.Light.surfaceKeyFunctionArgb).toInt()
                    dim -> if (dark) 0x06FFFFFF else 0x45FFFFFF
                    dark -> 0x10FFFFFF
                    else -> 0xCEFFFFFF.toInt()
                }
            if (
                cell.fillShader == null ||
                    cell.shaderTopCol != topCol ||
                    cell.shaderBotCol != botCol ||
                    cell.shaderCy != cell.cy ||
                    cell.shaderR != r
            ) {
                cell.shaderTopCol = topCol
                cell.shaderBotCol = botCol
                cell.shaderCy = cell.cy
                cell.shaderR = r
                cell.fillShader =
                    LinearGradient(0f, cell.cy - r, 0f, cell.cy + r, topCol, botCol, Shader.TileMode.CLAMP)
            }
            fillPaint.shader = cell.fillShader
            c.drawPath(hexPath, fillPaint)
            fillPaint.shader = null

            borderPaint.color =
                when {
                    holdIntent == BackspaceHoldIntent.CLEAR_ALL || holdIntent == BackspaceHoldIntent.ROTATING ->
                        0xFFEF4444.toInt()
                    isHoldCell -> 0xFFF59E0B.toInt()
                    cell.isAccent -> 0x55FFFFFF
                    isCtxGlow && ctxHit.third == 0 -> if (dark) 0xFF38BDF8.toInt() else 0xFF0284C7.toInt()
                    isCtxGlow || isPressedAct -> accentSolid
                    dim -> if (dark) 0x10FFFFFF else 0x120F172A
                    dark -> 0x26FFFFFF
                    else -> 0x220F172A
                }
            borderPaint.strokeWidth = context.dpF(if (isHoldCell || (isCtxGlow && ctxHit.third == 0)) 2.0f else if (isCtxGlow) 1.5f else 1.1f)
            c.drawPath(hexPath, borderPaint)
            borderPaint.strokeWidth = context.dpF(1.1f)

            textPaint.color =
                when {
                    holdIntent == BackspaceHoldIntent.CLEAR_ALL ||
                        holdIntent == BackspaceHoldIntent.DELETE_BEFORE ||
                        holdIntent == BackspaceHoldIntent.DELETE_AFTER -> 0xFFFFFFFF.toInt()
                    holdIntent == BackspaceHoldIntent.ROTATING ->
                        if (dark) 0xFFFCA5A5.toInt() else 0xFFDC2626.toInt()
                    holdIntent == BackspaceHoldIntent.BATCH ->
                        if (dark) 0xFFFDE68A.toInt() else 0xFFB45309.toInt()
                    cell.isAccent -> 0xFFFFFFFF.toInt()
                    isCtxGlow && ctxHit.third == 0 && dark -> 0xFFFFFFFF.toInt()
                    isCtxGlow || isPressedAct -> accentText
                    cell.isAction -> secondaryText
                    else -> primaryText
                }
            textPaint.alpha =
                when {
                    dim -> 65
                    contextTargets.isNotEmpty() && !isCtxGlow && !cell.isAction && !sliding -> 195
                    else -> 255
                }
            val displayCellLabel =
                when (holdIntent) {
                    BackspaceHoldIntent.CLEAR_ALL -> "松手清空"
                    BackspaceHoldIntent.ROTATING -> "↻ 画圈"
                    BackspaceHoldIntent.DELETE_BEFORE -> "↑ 删前文"
                    BackspaceHoldIntent.DELETE_AFTER -> "↓ 删下文"
                    BackspaceHoldIntent.BATCH -> "批量删"
                    BackspaceHoldIntent.INACTIVE -> cell.label
                }
            textPaint.textSize =
                context.dpF(
                    when {
                        displayCellLabel.length >= 6 -> 8.8f
                        displayCellLabel.length >= 4 -> 9.6f
                        displayCellLabel.length >= 2 -> 11.2f
                        isCtxGlow && ctxHit.third == 0 -> if (isPureEnglish) 16.0f else 18.0f
                        else -> if (isPureEnglish) 14.5f else 16.5f
                    }
                )
            val yShift = if (isCtxGlow) -context.dpF(4.2f) else 0f
            val ty = cell.cy + yShift - (textPaint.descent() + textPaint.ascent()) * 0.5f
            c.drawText(displayCellLabel, cell.cx, ty, textPaint)

            if (isCtxGlow) {
                subPaint.color = if (ctxHit.third == 0 && dark) 0xE6FFFFFF.toInt() else accentText
                subPaint.alpha = 240
                subPaint.textSize = context.dpF(if (isPureEnglish) 7.6f else if (ctxHit.third == 0) 10.5f else 9.5f)
                val hintLabel = if (ctxHit.second.length > 5) ctxHit.second.take(4) + "…" else ctxHit.second
                val sy = cell.cy + context.dpF(9.5f) - (subPaint.descent() + subPaint.ascent()) * 0.5f
                c.drawText(hintLabel, cell.cx, sy, subPaint)
            }
        }
        textPaint.alpha = 255
        subPaint.alpha = 255

        // 2. Dynamic Progressive-Convergence Flower Overlay while sliding
        if (sliding) {
            if (trailSize >= 2) {
                trailPath.reset()
                trailPath.moveTo(trailX[trailHead], trailY[trailHead])
                for (i in 1 until trailSize) {
                    val idx = (trailHead + i) % TRAIL_CAPACITY
                    trailPath.lineTo(trailX[idx], trailY[idx])
                }
                trailPaint.color = accentText
                trailPaint.alpha = 185
                c.drawPath(trailPath, trailPaint)
            }

            val petalR = hexRadius * 0.88f
            for (petal in petals) {
                buildHexPath(petal.cx, petal.cy, petalR, hexPath)
                fillPaint.color =
                    when {
                        petal.isBacktrack -> if (dark) 0xCC334155.toInt() else 0xE6E2E8F0.toInt()
                        petal.isShortcut -> if (dark) 0xF0075985.toInt() else 0xF5BAE6FD.toInt()
                        petal.weightRank <= 1 -> if (dark) 0xEA0B3B4E.toInt() else 0xF2E0F2FE.toInt()
                        else -> if (dark) 0xD8152336.toInt() else 0xECF1F5F9.toInt()
                    }
                c.drawPath(hexPath, fillPaint)
                borderPaint.color =
                    when {
                        petal.isBacktrack -> if (dark) 0x6694A3B8 else 0x5564748B
                        petal.isShortcut -> if (dark) 0xFF38BDF8.toInt() else 0xFF0284C7.toInt()
                        else -> accentSolid
                    }
                borderPaint.strokeWidth = context.dpF(if (petal.isShortcut) 1.9f else 1.4f)
                c.drawPath(hexPath, borderPaint)
                borderPaint.strokeWidth = context.dpF(1.1f)

                val mainColor =
                    when {
                        petal.isBacktrack -> secondaryText
                        petal.isShortcut && dark -> 0xFFFFFFFF.toInt()
                        dark -> 0xFF7DD3FC.toInt()
                        else -> 0xFF0369A1.toInt()
                    }
                val hasHint = petal.hintChar.isNotEmpty() && !petal.isBacktrack
                textPaint.color = mainColor
                val labelStr =
                    when {
                        petal.isBacktrack -> petal.append
                        isPureEnglish -> petal.append
                        else -> petal.append.uppercase()
                    }
                textPaint.textSize =
                    context.dpF(
                        when {
                            labelStr.length >= 4 -> 10.5f
                            labelStr.length == 3 -> 12.0f
                            labelStr.length == 2 -> 14.0f
                            else -> 16.5f
                        }
                    )
                val py =
                    petal.cy + (if (hasHint) -context.dpF(4.0f) else 0f) -
                        (textPaint.descent() + textPaint.ascent()) * 0.5f
                c.drawText(labelStr, petal.cx, py, textPaint)
                if (hasHint) {
                    subPaint.color = mainColor
                    subPaint.alpha = 220
                    subPaint.textSize = context.dpF(if (petal.hintChar.length > 3) 8.0f else 9.5f)
                    val hText = if (petal.hintChar.length > 6) petal.hintChar.take(5) + "…" else petal.hintChar
                    val hy = petal.cy + context.dpF(9.5f) - (subPaint.descent() + subPaint.ascent()) * 0.5f
                    c.drawText(hText, petal.cx, hy, subPaint)
                }
            }

            // Active center hex
            buildHexPath(activeCenter.x, activeCenter.y, hexRadius * 0.94f, hexPath)
            fillPaint.color = accentSolid
            c.drawPath(hexPath, fillPaint)
            borderPaint.color = 0xAAFFFFFF.toInt()
            borderPaint.strokeWidth = context.dpF(1.8f)
            c.drawPath(hexPath, borderPaint)
            borderPaint.strokeWidth = context.dpF(1.1f)

            val cur = cachedSlidePrefix
            val centerHint = cachedCenterHint
            textPaint.color = 0xFFFFFFFF.toInt()
            textPaint.textSize = context.dpF(if (cur.length >= 5) 11.5f else if (cur.length >= 4) 13.0f else 16.0f)
            val cyText =
                activeCenter.y + (if (centerHint.isNotEmpty()) -context.dpF(4.2f) else 0f) -
                    (textPaint.descent() + textPaint.ascent()) * 0.5f
            c.drawText(
                cur.ifEmpty { if (allowMixedEnglish) "aeiou" else "aeo" },
                activeCenter.x,
                cyText,
                textPaint,
            )
            if (centerHint.isNotEmpty()) {
                subPaint.color = 0xE6FFFFFF.toInt()
                subPaint.textSize = context.dpF(if (centerHint.length > 3) 8.5f else 10.5f)
                val chText = if (centerHint.length > 6) centerHint.take(5) + "…" else centerHint
                val chY = activeCenter.y + context.dpF(10f) - (subPaint.descent() + subPaint.ascent()) * 0.5f
                c.drawText(chText, activeCenter.x, chY, subPaint)
            }

            // 3. Unobstructed Top-Center Floating HUD Capsule (always visible above right/left thumb!)
            val hudSummary = cachedHudSummary
            subPaint.textSize = context.dpF(11f)
            val hudW = (subPaint.measureText(hudSummary) + context.dpF(22f)).coerceAtMost(width - context.dpF(16f))
            val hudH = context.dpF(24f)
            val hudL = (width - hudW) * 0.5f
            val hudT = context.dpF(3f)
            fillPaint.color = if (dark) 0xEB0F172A.toInt() else 0xF5FFFFFF.toInt()
            c.drawRoundRect(hudL, hudT, hudL + hudW, hudT + hudH, hudH * 0.5f, hudH * 0.5f, fillPaint)
            borderPaint.color = if (dark) 0x6638BDF8 else 0x550284C7
            c.drawRoundRect(hudL, hudT, hudL + hudW, hudT + hudH, hudH * 0.5f, hudH * 0.5f, borderPaint)
            subPaint.color = if (dark) 0xFF7DD3FC.toInt() else 0xFF0369A1.toInt()
            subPaint.alpha = 255
            val hudTextY = hudT + hudH * 0.5f - (subPaint.descent() + subPaint.ascent()) * 0.5f
            c.drawText(hudSummary, width * 0.5f, hudTextY, subPaint)
        } else if (pressedAction?.id == "⌫" && (backspaceGesture.isActive || batchHoldPending)) {
            val holdIntent = backspaceGesture.intent
            val elapsed = (SystemClock.uptimeMillis() - actionPressedAt).coerceAtLeast(0L)
            if (backspaceGesture.isActive || elapsed >= CLEAR_ALL_PROGRESS_START_MS) {
                val hudText =
                    when (holdIntent) {
                        BackspaceHoldIntent.CLEAR_ALL -> "松手立即清空输入框全文"
                        BackspaceHoldIntent.ROTATING -> "继续画圈解锁清空全文"
                        BackspaceHoldIntent.DELETE_BEFORE -> "松手删除光标前全部内容"
                        BackspaceHoldIntent.DELETE_AFTER -> "松手删除光标后全部内容"
                        BackspaceHoldIntent.BATCH -> "按住逐句删 · ↑前文 ↓下文 ↻画圈清空"
                        BackspaceHoldIntent.INACTIVE -> {
                            val remain = ((CLEAR_ALL_HOLD_MS - elapsed) / 1000f).coerceAtLeast(0.1f)
                            String.format(java.util.Locale.US, "继续按住 %.1fs 进入批量删除", remain)
                        }
                    }
                subPaint.textSize = context.dpF(11f)
                val hudW = (subPaint.measureText(hudText) + context.dpF(24f)).coerceAtMost(width - context.dpF(16f))
                val hudH = context.dpF(24f)
                val hudL = (width - hudW) * 0.5f
                val hudT = context.dpF(3f)
                fillPaint.color =
                    when (holdIntent) {
                        BackspaceHoldIntent.CLEAR_ALL -> 0xFFDC2626.toInt()
                        BackspaceHoldIntent.ROTATING -> if (dark) 0xEB450A0A.toInt() else 0xF5FEF2F2.toInt()
                        BackspaceHoldIntent.DELETE_BEFORE, BackspaceHoldIntent.DELETE_AFTER -> 0xFFD97706.toInt()
                        BackspaceHoldIntent.BATCH -> if (dark) 0xEB451A03.toInt() else 0xF5FFFBEB.toInt()
                        BackspaceHoldIntent.INACTIVE -> if (dark) 0xEB0F172A.toInt() else 0xF5FFFFFF.toInt()
                    }
                c.drawRoundRect(hudL, hudT, hudL + hudW, hudT + hudH, hudH * 0.5f, hudH * 0.5f, fillPaint)
                borderPaint.color =
                    when (holdIntent) {
                        BackspaceHoldIntent.CLEAR_ALL, BackspaceHoldIntent.ROTATING -> 0xFFEF4444.toInt()
                        BackspaceHoldIntent.DELETE_BEFORE,
                        BackspaceHoldIntent.DELETE_AFTER,
                        BackspaceHoldIntent.BATCH -> 0xFFF59E0B.toInt()
                        BackspaceHoldIntent.INACTIVE -> 0x88F59E0B.toInt()
                    }
                c.drawRoundRect(hudL, hudT, hudL + hudW, hudT + hudH, hudH * 0.5f, hudH * 0.5f, borderPaint)
                subPaint.color =
                    when (holdIntent) {
                        BackspaceHoldIntent.CLEAR_ALL,
                        BackspaceHoldIntent.DELETE_BEFORE,
                        BackspaceHoldIntent.DELETE_AFTER -> 0xFFFFFFFF.toInt()
                        BackspaceHoldIntent.ROTATING -> if (dark) 0xFFFCA5A5.toInt() else 0xFFDC2626.toInt()
                        BackspaceHoldIntent.BATCH,
                        BackspaceHoldIntent.INACTIVE -> if (dark) 0xFFFDE68A.toInt() else 0xFFB45309.toInt()
                    }
                subPaint.alpha = 255
                val hudTextY = hudT + hudH * 0.5f - (subPaint.descent() + subPaint.ascent()) * 0.5f
                c.drawText(hudText, width * 0.5f, hudTextY, subPaint)
            }
            if (!backspaceGesture.isActive && elapsed < CLEAR_ALL_HOLD_MS) {
                postInvalidateOnAnimation()
            }
        }
    }

    private fun hitCell(x: Float, y: Float): HexCell? {
        var best: HexCell? = null
        var bestScore = hexRadius * 0.96f
        for (cell in cells) {
            val d = hypot(x - cell.cx, y - cell.cy)
            val rank = if (!cell.isAction) contextTargets[cell.id]?.third else null
            val gravity =
                when {
                    rank == 0 -> 1.18f
                    rank != null && rank <= 2 -> 1.10f
                    rank != null -> 1.05f
                    else -> 1.0f
                }
            val effective = d / gravity
            if (d <= hexRadius * (0.94f * gravity) && effective < bestScore) {
                bestScore = effective
                best = cell
            }
        }
        return best
    }

    private fun beginTouch(touch: Touch) {
        val hit = touch.cell
        val x = touch.x
        val y = touch.y
        removeCallbacks(repeatDelete)
        removeCallbacks(armBatchDelete)
        removeCallbacks(batchDelete)
        repeatingDelete = false
        batchHoldPending = false
        backspaceGesture.reset()
        if (hit.isAction) {
            pressedAction = hit
            actionDownX = x
            actionDownY = y
            actionLastX = x
            actionLastY = y
            actionPressedAt = touch.time
            sliding = false
            armedSlide = false
            if (hit.id == "⌫") {
                onActionKey("⌫-hold-begin")
                backspaceHold = touch.deleteHold
                batchHoldPending = !backspaceHold.startedComposing
                postDelayed(repeatDelete, 360L)
                if (batchHoldPending) postDelayed(armBatchDelete, CLEAR_ALL_HOLD_MS)
            }
        } else {
            pressedAction = null
            sliding = true
            armedSlide = false
            stepAnchor = PointF(x, y)
            stepStack.clear()
            centerHistory.clear()
            clearTrail()
            pushTrail(x, y)
            if (hit.id != "aeiou") {
                stepStack.add(hit.id)
            }
            centerHistory.add(PointF(hit.cx, hit.cy))
            computePetals(PointF(hit.cx, hit.cy), currentPrefix())
        }
        invalidate()
    }

    private fun moveBackspaceGesture(e: MotionEvent, pointerIndex: Int) {
        actionLastX = e.getX(pointerIndex)
        actionLastY = e.getY(pointerIndex)
        val prevIntent = backspaceGesture.intent
        for (h in 0 until e.historySize) {
            backspaceGesture.update(e.getHistoricalX(pointerIndex, h), e.getHistoricalY(pointerIndex, h))
        }
        val nextIntent = backspaceGesture.update(actionLastX, actionLastY)
        if (prevIntent != BackspaceHoldIntent.CLEAR_ALL && nextIntent == BackspaceHoldIntent.CLEAR_ALL) {
            performHapticFeedback(HapticFeedbackConstants.CONTEXT_CLICK)
        }
        if (prevIntent == BackspaceHoldIntent.BATCH && nextIntent != BackspaceHoldIntent.BATCH) {
            removeCallbacks(batchDelete)
        } else if (prevIntent != BackspaceHoldIntent.BATCH && nextIntent == BackspaceHoldIntent.BATCH) {
            removeCallbacks(batchDelete)
            postDelayed(batchDelete, BATCH_DELETE_INTERVAL_MS)
        }
        invalidate()
    }

    private fun moveTouch(x: Float, y: Float) {
        if (pressedAction?.id == "⌫") {
            actionLastX = x
            actionLastY = y
            if (!backspaceGesture.isActive && hypot(x - actionDownX, y - actionDownY) > context.dpF(14f)) {
                removeCallbacks(repeatDelete)
                removeCallbacks(armBatchDelete)
                batchHoldPending = false
            }
            invalidate()
            return
        }
        if (!sliding) return
        pushTrail(x, y)
        // Require genuine finger displacement before triggering any petal so tapping edge keys (e.g. J) NEVER misfires!
        val armThreshold = if (stepStack.size <= 1) hexRadius * 0.68f else hexRadius * 0.48f
        if (!armedSlide) {
            if (hypot(x - stepAnchor.x, y - stepAnchor.y) < armThreshold) {
                invalidate()
                return
            }
            armedSlide = true
        }
        val triggerR = hexRadius * 0.72f
        val entered =
            petals.minByOrNull { hypot(x - it.cx, y - it.cy) }?.takeIf {
                hypot(x - it.cx, y - it.cy) <= triggerR
            }
        if (entered != null) {
            armedSlide = false
            stepAnchor = PointF(x, y)
            if (entered.isBacktrack) {
                if (stepStack.isNotEmpty()) stepStack.removeAt(stepStack.lastIndex)
                if (centerHistory.size > 1) centerHistory.removeAt(centerHistory.lastIndex)
                val prevCenter = centerHistory.lastOrNull() ?: PointF(x, y)
                computePetals(prevCenter, currentPrefix())
            } else {
                stepStack.add(entered.append)
                val nextCenter = PointF(entered.cx, entered.cy)
                centerHistory.add(nextCenter)
                computePetals(nextCenter, currentPrefix())
            }
        }
        invalidate()
    }

    fun cancelActiveTouches() {
        removeCallbacks(repeatDelete)
        removeCallbacks(armBatchDelete)
        removeCallbacks(batchDelete)
        touches.clear()
        gesturePointerId = null
        pressedAction = null
        repeatingDelete = false
        batchHoldPending = false
        backspaceGesture.reset()
        backspaceHold.begin(composing = false)
        sliding = false
        armedSlide = false
        petals = emptyList()
        clearTrail()
        cachedSlidePrefix = ""
        cachedCenterHint = ""
        cachedHudSummary = ""
        stepStack.clear()
        centerHistory.clear()
        invalidate()
    }

    private fun finishTouch(x: Float, y: Float) {
        removeCallbacks(repeatDelete)
        removeCallbacks(armBatchDelete)
        removeCallbacks(batchDelete)
        val act = pressedAction
        if (act?.id == "⌫" && backspaceGesture.isActive) {
            val prevIntent = backspaceGesture.intent
            val nextIntent = backspaceGesture.update(x, y)
            if (prevIntent != BackspaceHoldIntent.CLEAR_ALL && nextIntent == BackspaceHoldIntent.CLEAR_ALL) {
                performHapticFeedback(HapticFeedbackConstants.CONTEXT_CLICK)
            }
        }
        val didRepeat = repeatingDelete
        val holdIntent = backspaceGesture.intent
        pressedAction = null
        repeatingDelete = false
        batchHoldPending = false
        backspaceGesture.reset()
        if (act != null) {
            when {
                holdIntent == BackspaceHoldIntent.CLEAR_ALL -> {
                    onActionKey("clear-all")
                }
                holdIntent == BackspaceHoldIntent.DELETE_BEFORE -> {
                    performHapticFeedback(HapticFeedbackConstants.CONTEXT_CLICK)
                    onActionKey("delete-before-cursor")
                }
                holdIntent == BackspaceHoldIntent.DELETE_AFTER -> {
                    performHapticFeedback(HapticFeedbackConstants.CONTEXT_CLICK)
                    onActionKey("delete-after-cursor")
                }
                holdIntent != BackspaceHoldIntent.INACTIVE || didRepeat -> Unit
                act.id == "⌫" && !backspaceHold.allowsDelete(isComposing()) -> Unit
                act.id == "⌫" && x - actionDownX < -context.dpF(20f) -> onActionKey("delete-word")
                hitCell(x, y) == act -> onActionKey(act.id)
            }
        } else if (sliding) {
            val token = currentPrefix()
            sliding = false
            petals = emptyList()
            clearTrail()
            cachedSlidePrefix = ""
            cachedCenterHint = ""
            cachedHudSummary = ""
            if (token.isNotEmpty()) {
                // In pure English mode, append a trailing space when a multi-letter word was slid
                val finalOut =
                    if (isPureEnglish && token.length >= 2 && ENGLISH_WORDS.any { it.first == token }) {
                        "$token "
                    } else {
                        token
                    }
                onSyllable(finalOut)
            }
        }
        invalidate()
    }

    override fun onTouchEvent(e: MotionEvent): Boolean {
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                val index = e.actionIndex
                val id = e.getPointerId(index)
                val x = e.getX(index)
                val y = e.getY(index)
                val hit = hitCell(x, y) ?: return true
                val touch = Touch(hit, x, y, e.eventTime)
                touch.deleteHold.begin(isComposing())
                touches[id] = touch
                if (gesturePointerId == null) {
                    gesturePointerId = id
                    beginTouch(touch)
                }
            }
            MotionEvent.ACTION_MOVE -> {
                for ((id, touch) in touches) {
                    val index = e.findPointerIndex(id)
                    if (index < 0) continue
                    val x = e.getX(index)
                    val y = e.getY(index)
                    if (id == gesturePointerId) {
                        if (pressedAction?.id == "⌫" && backspaceGesture.isActive) {
                            moveBackspaceGesture(e, index)
                        } else {
                            moveTouch(x, y)
                        }
                    } else if (hypot(x - touch.x, y - touch.y) > context.dpF(14f)) {
                        touch.moved = true
                    }
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> {
                val index = e.actionIndex
                val id = e.getPointerId(index)
                val touch = touches.remove(id) ?: return true
                val x = e.getX(index)
                val y = e.getY(index)
                if (id == gesturePointerId) {
                    finishTouch(x, y)
                    gesturePointerId = null
                    touches.entries.firstOrNull { !it.value.moved }?.let { (nextId, next) ->
                        gesturePointerId = nextId
                        beginTouch(next)
                    }
                } else if (!touch.moved && hitCell(x, y) == touch.cell) {
                    val cell = touch.cell
                    if (cell.isAction) {
                        if (cell.id != "⌫" || touch.deleteHold.allowsDelete(isComposing())) onActionKey(cell.id)
                    } else if (cell.id != "aeiou") {
                        onSyllable(cell.id)
                    }
                }
            }
            MotionEvent.ACTION_CANCEL -> {
                cancelActiveTouches()
            }
        }
        return true
    }

    override fun onDetachedFromWindow() {
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
        private const val TRAIL_CAPACITY = 48
        private val HEX_COS = FloatArray(6) { i -> cos((PI / 180.0) * (60.0 * i - 30.0)).toFloat() }
        private val HEX_SIN = FloatArray(6) { i -> sin((PI / 180.0) * (60.0 * i - 30.0)).toFloat() }
        // Compact frequency-ranked English everyday + tech lexicon: (lowercase, display, weight)
        private val ENGLISH_WORDS: List<Triple<String, String, Int>> =
            listOf(
                Triple("the", "the", 100), Triple("and", "and", 98), Triple("you", "you", 97),
                Triple("that", "that", 96), Triple("this", "this", 95), Triple("with", "with", 94),
                Triple("for", "for", 93), Triple("have", "have", 92), Triple("not", "not", 91),
                Triple("are", "are", 90), Triple("from", "from", 89), Triple("will", "will", 88),
                Triple("can", "can", 87), Triple("what", "what", 86), Triple("when", "when", 85),
                Triple("where", "where", 84), Triple("which", "which", 83), Triple("about", "about", 82),
                Triple("there", "there", 81), Triple("their", "their", 80), Triple("they", "they", 79),
                Triple("would", "would", 78), Triple("could", "could", 77), Triple("should", "should", 76),
                Triple("think", "think", 75), Triple("thank", "thank", 74), Triple("thanks", "thanks", 73),
                Triple("please", "please", 72), Triple("sorry", "sorry", 71), Triple("hello", "hello", 70),
                Triple("good", "good", 69), Triple("great", "great", 68), Triple("want", "want", 67),
                Triple("need", "need", 66), Triple("know", "know", 65), Triple("make", "make", 64),
                Triple("like", "like", 63), Triple("just", "just", 62), Triple("time", "time", 61),
                Triple("work", "work", 60), Triple("also", "also", 59), Triple("more", "more", 58),
                Triple("very", "very", 57), Triple("only", "only", 56), Triple("some", "some", 55),
                Triple("then", "then", 54), Triple("them", "them", 53), Triple("here", "here", 52),
                Triple("back", "back", 51), Triple("well", "well", 50), Triple("over", "over", 49),
                Triple("after", "after", 48), Triple("first", "first", 47), Triple("right", "right", 46),
                Triple("check", "check", 45), Triple("build", "build", 44), Triple("start", "start", 43),
                Triple("update", "update", 42), Triple("change", "change", 41), Triple("issue", "issue", 40),
                Triple("github", "GitHub", 95), Triple("iphone", "iPhone", 94), Triple("vscode", "VSCode", 93),
                Triple("python", "Python", 92), Triple("rust", "Rust", 91), Triple("linux", "Linux", 90),
                Triple("android", "Android", 89), Triple("windows", "Windows", 88), Triple("vitral", "Vitral", 96),
                Triple("wifi", "Wi-Fi", 87), Triple("docker", "Docker", 86), Triple("chrome", "Chrome", 85),
                Triple("wechat", "WeChat", 84), Triple("google", "Google", 83), Triple("apple", "Apple", 82),
                Triple("email", "Email", 81), Triple("gmail", "Gmail", 80), Triple("webdav", "WebDAV", 86),
                Triple("koofr", "Koofr", 85), Triple("openai", "OpenAI", 84), Triple("chatgpt", "ChatGPT", 83),
                Triple("react", "React", 80), Triple("node", "Node", 79), Triple("java", "Java", 78),
                Triple("json", "JSON", 77), Triple("http", "HTTP", 76), Triple("https", "HTTPS", 76),
                Triple("html", "HTML", 75), Triple("api", "API", 85), Triple("app", "App", 85),
                Triple("pdf", "PDF", 82), Triple("sql", "SQL", 78), Triple("ssh", "SSH", 78),
                Triple("vpn", "VPN", 78), Triple("git", "Git", 84), Triple("fix", "fix", 76),
                Triple("test", "test", 79), Triple("todo", "TODO", 76), Triple("demo", "demo", 74),
                Triple("model", "model", 75), Triple("cloud", "cloud", 74), Triple("sync", "sync", 76),
                Triple("input", "input", 74), Triple("keyboard", "keyboard", 74), Triple("mobile", "mobile", 73),
                Triple("open", "open", 72), Triple("close", "close", 71), Triple("send", "send", 72),
                Triple("next", "next", 71), Triple("done", "done", 72), Triple("sure", "sure", 72),
                Triple("yes", "yes", 75), Triple("ok", "OK", 88), Triple("ai", "AI", 86),
                Triple("ui", "UI", 80), Triple("ux", "UX", 80), Triple("os", "OS", 75),
                Triple("to", "to", 96), Triple("in", "in", 96), Triple("is", "is", 96),
                Triple("it", "it", 95), Triple("of", "of", 95), Triple("on", "on", 94),
                Triple("at", "at", 92), Triple("be", "be", 92), Triple("we", "we", 92),
                Triple("do", "do", 91), Triple("go", "go", 90), Triple("my", "my", 90),
                Triple("so", "so", 89), Triple("up", "up", 88), Triple("no", "no", 88),
            )

        private val ENGLISH_NEXT_MAP: Map<String, List<String>> by lazy {
            val scores = HashMap<String, HashMap<String, Int>>()
            for ((word, _, weight) in ENGLISH_WORDS) {
                for (i in 1 until word.length) {
                    val p = word.substring(0, i)
                    val nxt = word.substring(i, i + 1)
                    val bucket = scores.getOrPut(p) { HashMap() }
                    bucket[nxt] = (bucket[nxt] ?: 0) + weight
                }
            }
            scores.mapValues { (_, bucket) ->
                bucket.entries.sortedByDescending { it.value }.map { it.key }
            }
        }

        private fun englishNextLetters(prefix: String, boost: String?): List<String> {
            val base = ENGLISH_NEXT_MAP[prefix].orEmpty()
            if (boost == null || boost !in base) return base
            return listOf(boost) + base.filter { it != boost }
        }

        private fun englishWordCompletion(prefix: String): Pair<String, String>? {
            if (prefix.isEmpty()) return null
            return ENGLISH_WORDS
                .filter { it.first.startsWith(prefix) && it.first.length > prefix.length }
                .maxByOrNull { it.third }
                ?.let { it.first to it.second }
        }

        private fun englishWordsWithPrefix(prefix: String): List<Pair<String, String>> =
            ENGLISH_WORDS
                .filter { it.first.startsWith(prefix) && it.first.length > prefix.length }
                .sortedByDescending { it.third }
                .take(4)
                .map { it.first to it.second }

        private fun englishTopHint(prefix: String): String {
            ENGLISH_WORDS.firstOrNull { it.first == prefix }?.let { return it.second }
            return englishWordCompletion(prefix)?.second.orEmpty()
        }

        internal val ENGLISH_BIGRAM_HINTS: Map<String, List<Pair<String, String>>> =
            mapOf(
                "i" to listOf("am" to "am", "have" to "have", "will" to "will", "want" to "want", "think" to "think"),
                "you" to listOf("are" to "are", "can" to "can", "want" to "want", "have" to "have"),
                "thank" to listOf("you" to "you"),
                "thanks" to listOf("for" to "for"),
                "good" to listOf("morning" to "morning", "night" to "night", "work" to "work"),
                "how" to listOf("are" to "are", "to" to "to", "about" to "about"),
                "to" to listOf("the" to "the", "be" to "be", "do" to "do", "check" to "check", "update" to "update"),
                "in" to listOf("the" to "the", "this" to "this", "android" to "Android", "linux" to "Linux"),
                "on" to listOf("the" to "the", "github" to "GitHub", "android" to "Android", "mobile" to "mobile"),
                "this" to listOf("is" to "is", "will" to "will", "feature" to "feature"),
            )

        // High-frequency Mandarin exemplar char + relative priority weight per syllable
        private val TOP_SYLLABLE_HINTS: Map<String, Pair<String, Int>> =
            mapOf(
                "de" to ("的" to 100), "yi" to ("一" to 99), "shi" to ("是" to 98), "bu" to ("不" to 97),
                "le" to ("了" to 96), "wo" to ("我" to 95), "zai" to ("在" to 94), "ren" to ("人" to 93),
                "you" to ("有" to 92), "zhe" to ("这" to 91), "zhong" to ("中" to 90), "da" to ("大" to 89),
                "wei" to ("为" to 88), "shang" to ("上" to 87), "ge" to ("个" to 86), "guo" to ("国" to 85),
                "men" to ("们" to 84), "dao" to ("到" to 83), "shuo" to ("说" to 82), "he" to ("和" to 81),
                "di" to ("地" to 80), "ye" to ("也" to 79), "zi" to ("子" to 78), "yao" to ("要" to 77),
                "jiu" to ("就" to 76), "chu" to ("出" to 75), "hui" to ("会" to 74), "ke" to ("可" to 73),
                "ni" to ("你" to 72), "dui" to ("对" to 71), "sheng" to ("生" to 70), "neng" to ("能" to 69),
                "er" to ("而" to 68), "na" to ("那" to 67), "hao" to ("好" to 66), "ta" to ("他" to 65),
                "lai" to ("来" to 64), "qu" to ("去" to 63), "xia" to ("下" to 62),
                "tian" to ("天" to 60), "mei" to ("没" to 59), "shen" to ("什" to 58), "me" to ("么" to 57),
                "zen" to ("怎" to 56), "yang" to ("样" to 55), "xiang" to ("想" to 54), "kan" to ("看" to 53),
                "zuo" to ("做" to 52), "xin" to ("新" to 51), "jin" to ("今" to 50), "ming" to ("明" to 49),
                "fa" to ("发" to 48), "shu" to ("输" to 47), "ru" to ("入" to 46), "que" to ("确" to 45),
                "zheng" to ("正" to 44), "chang" to ("常" to 43), "yong" to ("用" to 42), "wan" to ("万" to 41),
                "wu" to ("无" to 40), "ba" to ("把" to 39), "bei" to ("被" to 38), "cong" to ("从" to 37),
                "duo" to ("多" to 36), "fang" to ("方" to 35), "fen" to ("分" to 34), "gao" to ("高" to 33),
                "gong" to ("工" to 32), "guan" to ("关" to 31), "hai" to ("还" to 30), "hen" to ("很" to 29),
                "hou" to ("后" to 28), "hua" to ("话" to 27), "jia" to ("家" to 26), "jian" to ("见" to 25),
                "jiang" to ("将" to 24), "jiao" to ("叫" to 23), "jie" to ("界" to 22), "jing" to ("经" to 21),
                "kai" to ("开" to 20), "kuai" to ("快" to 19), "li" to ("里" to 18), "liang" to ("两" to 17),
                "lv" to ("绿" to 16), "ma" to ("吗" to 15), "nv" to ("女" to 14), "pa" to ("怕" to 13),
                "ping" to ("平" to 12), "qi" to ("起" to 11), "qian" to ("前" to 10), "qing" to ("请" to 9),
                "ran" to ("然" to 8), "ri" to ("日" to 7), "san" to ("三" to 6), "si" to ("四" to 5),
                "suo" to ("所" to 4), "tai" to ("太" to 3), "tong" to ("同" to 2), "wen" to ("问" to 2),
                "xi" to ("系" to 2), "xian" to ("先" to 2), "xiao" to ("小" to 2), "xie" to ("写" to 2),
                "xing" to ("行" to 2), "xue" to ("学" to 2), "ai" to ("爱" to 2), "an" to ("安" to 2),
                "ao" to ("奥" to 2), "ou" to ("欧" to 2), "a" to ("啊" to 2), "e" to ("额" to 2), "o" to ("哦" to 2),
            )

        private val SYLLABLES: List<String> =
            listOf(
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

        private val NEXT_MAP: Map<String, List<String>> by lazy {
            val map = HashMap<String, MutableSet<String>>()
            for (syl in SYLLABLES) {
                for (i in 1 until syl.length) {
                    map.getOrPut(syl.substring(0, i)) { LinkedHashSet() }.add(syl.substring(i, i + 1))
                }
            }
            map.mapValues { (prefix, set) ->
                set.sortedByDescending { letter -> prefixScore(prefix + letter) }
            }
        }

        private fun prefixScore(prefix: String): Int {
            var best = TOP_SYLLABLE_HINTS[prefix]?.second ?: 0
            for ((syl, pair) in TOP_SYLLABLE_HINTS) {
                if (syl.startsWith(prefix) && pair.second > best) {
                    best = pair.second
                }
            }
            return best
        }

        internal fun topHintChar(prefix: String): String {
            TOP_SYLLABLE_HINTS[prefix]?.let { return it.first }
            var bestChar = ""
            var bestScore = -1
            for ((syl, pair) in TOP_SYLLABLE_HINTS) {
                if (syl.startsWith(prefix) && pair.second > bestScore) {
                    bestScore = pair.second
                    bestChar = pair.first
                }
            }
            return bestChar
        }

        private fun topSyllablesWithPrefix(prefix: String): List<Pair<String, String>> =
            TOP_SYLLABLE_HINTS.entries
                .filter { it.key.startsWith(prefix) && it.key.length > prefix.length }
                .sortedByDescending { it.value.second }
                .take(4)
                .map { it.key to it.value.first }

        internal fun nextValidLettersByFreq(prefix: String, boostLetter: String?): List<String> {
            val base = NEXT_MAP[prefix].orEmpty()
            if (boostLetter == null || boostLetter !in base) return base
            return listOf(boostLetter) + base.filter { it != boostLetter }
        }

        internal val CHAR_CONTEXT_HINTS: Map<Char, List<Pair<String, String>>> =
            mapOf(
                '正' to listOf("que" to "确", "zai" to "在", "chang" to "常", "shi" to "式", "hao" to "好"),
                '我' to listOf("men" to "们", "de" to "的", "yao" to "要", "xiang" to "想", "jue" to "觉"),
                '你' to listOf("hao" to "好", "men" to "们", "de" to "的", "yao" to "要", "shuo" to "说"),
                '不' to listOf("shi" to "是", "yao" to "要", "hui" to "会", "zhi" to "知", "cuo" to "错"),
                '没' to listOf("you" to "有", "guan" to "关", "shi" to "事", "wen" to "问"),
                '输' to listOf("ru" to "入", "chu" to "出", "fa" to "法"),
                '入' to listOf("fa" to "法", "kou" to "口"),
                '今' to listOf("tian" to "天", "nian" to "年", "wan" to "晚"),
                '明' to listOf("tian" to "天", "bai" to "白", "nian" to "年"),
                '怎' to listOf("me" to "么", "yang" to "样"),
                '什' to listOf("me" to "么"),
                '可' to listOf("yi" to "以", "neng" to "能", "shi" to "是"),
                '工' to listOf("zuo" to "作", "cheng" to "程", "ju" to "具"),
                '中' to listOf("guo" to "国", "wen" to "文", "xin" to "心"),
                '时' to listOf("jian" to "间", "hou" to "候"),
                '问' to listOf("ti" to "题"),
                '打' to listOf("ka" to "卡", "kai" to "开", "zi" to "字"),
            )
    }
}
