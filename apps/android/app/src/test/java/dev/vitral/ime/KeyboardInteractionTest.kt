package dev.vitral.ime

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class KeyboardInteractionTest {
    @Test
    fun preeditBackspaceLocksUntilNextPress() {
        val hold = BackspaceHold()
        hold.begin(composing = true)
        assertTrue(hold.startedComposing)
        assertTrue(hold.allowsDelete(composing = true))
        assertFalse(hold.allowsDelete(composing = false))
        // Another finger starting new preedit must not unlock the original hold.
        assertFalse(hold.allowsDelete(composing = true))
        hold.begin(composing = false)
        assertFalse(hold.startedComposing)
        assertTrue(hold.allowsDelete(composing = false))
    }

    @Test
    fun hostBackspaceRemainsContinuous() {
        val hold = BackspaceHold()
        hold.begin(composing = false)
        repeat(100) { assertTrue(hold.allowsDelete(composing = false)) }
        assertTrue(hold.allowsDelete(composing = true))
        assertTrue(hold.allowsDelete(composing = false))
    }

    @Test
    fun backspaceFlightGateCoalescesRepeatsButKeepsDiscreteTaps() {
        val gate = BackspaceFlightGate()
        assertFalse(gate.shouldDrop(isRepeat = false))
        gate.onDispatched()
        // Rapid second discrete tap must NOT be dropped while first tap is in flight.
        assertFalse(gate.shouldDrop(isRepeat = false))
        // Auto-repeat tick IS coalesced while a delete is in flight.
        assertTrue(gate.shouldDrop(isRepeat = true))
        gate.onDispatched()
        assertEquals(2, gate.inFlight)
        gate.onCompleted()
        assertTrue(gate.shouldDrop(isRepeat = true))
        gate.onCompleted()
        assertEquals(0, gate.inFlight)
        assertFalse(gate.shouldDrop(isRepeat = true))
    }

    @Test
    fun rapidTwoTapsCrossPreeditBoundaryWhileHoldStaysStopped() {
        // Tap 1 & Tap 2 both dispatched while 1 preedit char ("a") was still composing:
        // Tap 1 callback sees composingAtCallback=true -> clears preedit, does NOT delete host.
        assertFalse(
            shouldDeleteHostText(
                composingAtCallback = true,
                startedComposingAtDispatch = true,
                isRepeat = false,
            )
        )
        // Tap 2 callback now sees composingAtCallback=false -> discrete tap deletes 1 host char!
        assertTrue(
            shouldDeleteHostText(
                composingAtCallback = false,
                startedComposingAtDispatch = true,
                isRepeat = false,
            )
        )
        // Queued repeat tick from a preedit hold sees isRepeat=true -> blocked at boundary!
        assertFalse(
            shouldDeleteHostText(
                composingAtCallback = false,
                startedComposingAtDispatch = true,
                isRepeat = true,
            )
        )
        // Normal host-text hold (startedComposingAtDispatch=false) continues deleting host text.
        assertTrue(
            shouldDeleteHostText(
                composingAtCallback = false,
                startedComposingAtDispatch = false,
                isRepeat = true,
            )
        )
    }

    @Test
    fun wordDeletePreservesSurrogatePairsAcrossCodePoints() {
        val (slice1, matched1) = trailingWordDeleteSlice("😀甲乙丙", "")
        assertFalse(matched1)
        assertEquals("😀甲乙丙", slice1)
        assertEquals(4, slice1.codePointCount(0, slice1.length))

        val (slice2, matched2) = trailingWordDeleteSlice("前😀乙丙丁", "")
        assertFalse(matched2)
        assertEquals("😀乙丙丁", slice2)
        assertFalse(Character.isLowSurrogate(slice2.first()))

        val (slice3, matched3) = trailingWordDeleteSlice("今天我们去吃饭", "我们去吃饭")
        assertTrue(matched3)
        assertEquals("我们去吃饭", slice3)
    }

    @Test
    fun completeAnnotationSeparatesSyllablesWithoutChangingCase() {
        assertEquals("si'hu'you'dian", formatPreedit("sihuyoudian", "si hu you dian"))
        assertEquals("Ni'Hao", formatPreedit("NiHao", "ni hao"))
        assertEquals("ni'hao", formatPreedit("ni'hao", "ni hao"))
        assertEquals("xi'an", formatPreedit("xian", "xi'an"))
        assertEquals("ni'hao", formatPreedit("nihao", "  ni   hao  "))
    }

    @Test
    fun incompleteOrUnreliableAnnotationKeepsRawPreedit() {
        assertEquals("nih", formatPreedit("nih", "ni hao"))
        assertEquals("nh", formatPreedit("nh", "ni hao"))
        assertEquals("nihao", formatPreedit("nihao", "ni3 hao3"))
        assertEquals("nihao", formatPreedit("nihao", "ni hao · 你好"))
        assertEquals("nihao", formatPreedit("nihao", "ni hao!"))
        assertEquals("nihao", formatPreedit("nihao", ""))
        assertEquals("nǐhǎo", formatPreedit("nǐhǎo", "ni hao"))
        assertEquals("ni2hao", formatPreedit("ni2hao", "ni hao"))
        assertEquals("", formatPreedit("", "ni hao"))
    }

    @Test
    fun preeditDisplayMapsCursorAndTapOffsetsAcrossFormattedAndCjkPreedit() {
        val d = buildPreeditDisplay("nihao", "ni hao", 2)
        assertEquals("ni'hao", d.text)
        // Byte 2 is at the boundary between "ni" and "hao"
        assertEquals(3, d.cursorIndex)
        assertEquals(0, visualTapToRawByteOffset(d, 0, hasCaret = true))
        assertEquals(1, visualTapToRawByteOffset(d, 1, hasCaret = true))
        assertEquals(2, visualTapToRawByteOffset(d, 2, hasCaret = true))
        assertEquals(3, visualTapToRawByteOffset(d, 5, hasCaret = true))
        assertEquals(5, visualTapToRawByteOffset(d, 7, hasCaret = true))

        val cjk = buildPreeditDisplay("你好wo", "", 6)
        assertEquals("你好wo", cjk.text)
        assertEquals(2, cjk.cursorIndex)
        assertEquals(3, visualTapToRawByteOffset(cjk, 1, hasCaret = true))
        assertEquals(7, visualTapToRawByteOffset(cjk, 4, hasCaret = true))
    }

    @Test
    fun spatialHitboxRescuesIllegalBorderTapWithoutOverridingLegalKeys() {
        // Quanpin: after 'q', 'o' is illegal while left neighbor 'i' is legal (qi).
        assertEquals(
            'i',
            pickSpatialHitKey(
                rawPreedit = "q",
                flypy = false,
                english = false,
                xInKey = 5f,
                keyWidth = 100f,
                primaryChar = 'o',
                leftNeighbor = 'i',
                rightNeighbor = 'p',
            ),
        )
        // Xiaohe Flypy: after 'q', 'w' (ei) is illegal while right neighbor 'e' is illegal and left 'q' (iu -> qiu) is legal!
        assertEquals(
            'q',
            pickSpatialHitKey(
                rawPreedit = "q",
                flypy = true,
                english = false,
                xInKey = 10f,
                keyWidth = 100f,
                primaryChar = 'w',
                leftNeighbor = 'q',
                rightNeighbor = 'e',
            ),
        )
        // Center tap stays on primary even if illegal; English mode never redirects.
        assertEquals(
            'o',
            pickSpatialHitKey("q", false, false, 50f, 100f, 'o', 'i', 'p'),
        )
        assertEquals(
            'o',
            pickSpatialHitKey("q", false, true, 5f, 100f, 'o', 'i', 'p'),
        )
        // When primary is already legal (after 'n', 'i' is legal), never override to neighbor 'u'.
        assertEquals(
            'i',
            pickSpatialHitKey("n", false, false, 5f, 100f, 'i', 'u', 'o'),
        )
        // Xiaohe Flypy accepts qv -> qu; tapping 'v' near 'c' must NOT rewrite legal 'v'.
        assertEquals(
            'v',
            pickSpatialHitKey("q", true, false, 5f, 100f, 'v', 'c', 'b'),
        )
    }

    @Test
    fun hapticStrengthMapsOffAndTiers() {
        assertEquals(null, hapticParamsFor("off", true))
        val light = hapticParamsFor("light", true)!!
        val strong = hapticParamsFor("strong", true)!!
        assertTrue(strong.first > light.first)
        assertTrue(strong.second > light.second)
    }

    @Test
    fun spatial2dTouchHintCapturesEightNeighborhoodAndPersonalThumbBias() {
        val w = 100f
        val h = 120f
        val keys =
            listOf(
                SpatialKeyCenter('r', 350f, 60f, w, h),
                SpatialKeyCenter('t', 450f, 60f, w, h),
                SpatialKeyCenter('f', 375f, 180f, w, h),
                SpatialKeyCenter('g', 475f, 180f, w, h),
                SpatialKeyCenter('h', 575f, 180f, w, h),
                SpatialKeyCenter('v', 425f, 300f, w, h),
                SpatialKeyCenter('b', 525f, 300f, w, h),
            )
        val primaryG = keys.first { it.ch == 'g' }
        // Clean dead-center tap must not spray bogus neighbor wildcards (even with vertically aligned keys).
        val withVertical = keys + SpatialKeyCenter('b', primaryG.cx, primaryG.cy + h, w, h)
        val center = computeSpatialTouchHint(primaryG.cx, primaryG.cy, primaryG, withVertical)
        assertTrue(center.isEmpty)
        // Tap near top-left corner of 'g' -> vertical/diagonal neighbors 't' and 'f' rank ahead of 'h'.
        val topLeft = computeSpatialTouchHint(432f, 128f, primaryG, keys)
        assertEquals('t'.code, topLeft.alt1Char)
        assertEquals('f'.code, topLeft.alt2Char)
        assertTrue(topLeft.alt1Penalty > 0f && topLeft.alt1Penalty < topLeft.alt2Penalty)

        // Personal thumb bias shifts effective tap back toward center; EMA stays clamped within 0.25 * size.
        val estimator = ThumbBiasEstimator()
        repeat(40) { estimator.observe(38f, -40f, w, h) }
        assertTrue(estimator.biasX in 18f..25f)
        assertTrue(estimator.biasY in -30f..-20f)
        // Wild outlier outside 0.45 * size and non-finite inputs must not move or poison bias.
        val prevX = estimator.biasX
        val prevY = estimator.biasY
        estimator.observe(-90f, 90f, w, h)
        estimator.observe(Float.NaN, 0f, w, h)
        estimator.observe(0f, Float.POSITIVE_INFINITY, w, h)
        assertEquals(prevX, estimator.biasX)
        assertEquals(prevY, estimator.biasY)
    }

    @Test
    fun clearAllHoldDurationIsOneSecond() {
        assertEquals(1000L, HOLD_BATCH_MS)
        assertEquals(300L, BATCH_DELETE_INTERVAL_MS)
        assertEquals(1000L, Key.CLEAR_ALL_HOLD_MS)
        assertEquals(1000L, AeviouKeyboard.CLEAR_ALL_HOLD_MS)
    }

    @Test
    fun batchDeleteSliceDeletesSentencesThenUpgradesToParagraphsAndPreservesEmoji() {
        // Sentence-end punctuation (Chinese & English) for first 3 jumps (batchStep 0..2)
        assertEquals("第二句。", trailingBatchDeleteSlice("第一句。第二句。", batchStep = 0))
        assertEquals("第二句！", trailingBatchDeleteSlice("第一句？第二句！", batchStep = 1))
        assertEquals("分句；", trailingBatchDeleteSlice("前文；分句；", batchStep = 2))
        assertEquals(" World!", trailingBatchDeleteSlice("Hello? World!", batchStep = 0))
        assertEquals(" second;", trailingBatchDeleteSlice("first. second;", batchStep = 1))

        // Consecutive punctuation & trailing whitespace are consumed together with the sentence
        assertEquals("第二句！！！   ", trailingBatchDeleteSlice("第一句。第二句！！！   ", batchStep = 0))
        assertEquals(" World...  ", trailingBatchDeleteSlice("Hello? World...  ", batchStep = 1))

        // Newline acts as sentence boundary too, and trailing newline deletes the preceding sentence cleanly
        assertEquals("第二句。", trailingBatchDeleteSlice("第一句。\n第二句。", batchStep = 0))
        assertEquals("第一句。\n", trailingBatchDeleteSlice("第一句。\n", batchStep = 1))

        // 4th jump onward (batchStep >= 3) upgrades to paragraph deletion across sentence punctuation
        assertEquals(
            "第三句。第四句！",
            trailingBatchDeleteSlice("第一句。第二句。\n第三句。第四句！", batchStep = 3),
        )
        assertEquals(
            "第一句。第二句。\n",
            trailingBatchDeleteSlice("第一句。第二句。\n", batchStep = 4),
        )

        // Caps at 400 code points without splitting UTF-16 surrogate pairs (Emoji)
        val emojis = "😀".repeat(500)
        val emojiSlice = trailingBatchDeleteSlice(emojis, batchStep = 0)
        assertEquals(400, emojiSlice.codePointCount(0, emojiSlice.length))
        assertEquals(800, emojiSlice.length)
        assertFalse(Character.isLowSurrogate(emojiSlice.first()))

        val boundaryEmoji = "x😀" + "a".repeat(399)
        val boundarySlice = trailingBatchDeleteSlice(boundaryEmoji, batchStep = 0)
        assertEquals(400, boundarySlice.codePointCount(0, boundarySlice.length))
        assertTrue(boundarySlice.startsWith("😀"))
        assertFalse(Character.isLowSurrogate(boundarySlice.first()))

        val leaveEmojiIntact = "😀" + "a".repeat(400)
        val asciiSlice = trailingBatchDeleteSlice(leaveEmojiIntact, batchStep = 0)
        assertEquals("a".repeat(400), asciiSlice)
    }

    @Test
    fun backspaceHoldGestureHandlesBatchSwipeUpDownCancelAndEccentricCircles() {
        fun newGesture() =
            BackspaceHoldGesture(
                swipeUpEnterPx = 20f,
                swipeUpExitPx = 14f,
                swipeDownEnterPx = 18f,
                swipeDownExitPx = 12f,
                sampleStepPx = 4f,
                rotateSpanPx = 12f,
                clearAllSpanPx = 16f,
            )

        // 1. Holding in place stays BATCH
        val hold = newGesture()
        hold.begin(100f, 100f)
        assertEquals(BackspaceHoldIntent.BATCH, hold.intent)
        assertEquals(BackspaceHoldIntent.BATCH, hold.update(102f, 98f))

        // 2. Swipe up enters DELETE_BEFORE, hysteresis holds at 15px, returning < 14px cancels to BATCH
        assertEquals(BackspaceHoldIntent.DELETE_BEFORE, hold.update(100f, 79f))
        assertEquals(BackspaceHoldIntent.DELETE_BEFORE, hold.update(100f, 85f))
        assertEquals(BackspaceHoldIntent.BATCH, hold.update(100f, 87f))

        // 3. Swipe down enters DELETE_AFTER, hysteresis holds at 13px, returning < 12px cancels to BATCH
        assertEquals(BackspaceHoldIntent.DELETE_AFTER, hold.update(100f, 119f))
        assertEquals(BackspaceHoldIntent.DELETE_AFTER, hold.update(100f, 113f))
        assertEquals(BackspaceHoldIntent.BATCH, hold.update(100f, 111f))

        // 4. Eccentric CCW circle (center shifted up-left of right-edge Backspace key) -> ROTATING -> CLEAR_ALL
        val ccw = newGesture()
        val cx = 75f
        val cy = 80f
        val radius = 25f
        val startAngle = kotlin.math.atan2(100.0 - cy, 100.0 - cx)
        ccw.begin(
            (cx + radius * kotlin.math.cos(startAngle)).toFloat(),
            (cy + radius * kotlin.math.sin(startAngle)).toFloat(),
        )
        var sawRotatingCcw = false
        for (step in 1..36) {
            val a = startAngle - step * (2.0 * kotlin.math.PI / 36.0)
            val intent = ccw.update(
                (cx + radius * kotlin.math.cos(a)).toFloat(),
                (cy + radius * kotlin.math.sin(a)).toFloat(),
            )
            if (intent == BackspaceHoldIntent.ROTATING) sawRotatingCcw = true
            if (sawRotatingCcw) {
                // Once rotating, passing above/below anchor must never misfire DELETE_BEFORE/DELETE_AFTER
                assertTrue(
                    intent == BackspaceHoldIntent.ROTATING || intent == BackspaceHoldIntent.CLEAR_ALL,
                )
            }
        }
        assertTrue(sawRotatingCcw)
        assertEquals(BackspaceHoldIntent.CLEAR_ALL, ccw.intent)
        // Once unlocked, CLEAR_ALL remains locked regardless of subsequent movement
        assertEquals(BackspaceHoldIntent.CLEAR_ALL, ccw.update(100f, 100f))
        assertEquals(BackspaceHoldIntent.CLEAR_ALL, ccw.update(100f, 50f))

        // 5. Eccentric CW circle also unlocks CLEAR_ALL
        val cw = newGesture()
        cw.begin(
            (cx + radius * kotlin.math.cos(startAngle)).toFloat(),
            (cy + radius * kotlin.math.sin(startAngle)).toFloat(),
        )
        var sawRotatingCw = false
        for (step in 1..36) {
            val a = startAngle + step * (2.0 * kotlin.math.PI / 36.0)
            val intent = cw.update(
                (cx + radius * kotlin.math.cos(a)).toFloat(),
                (cy + radius * kotlin.math.sin(a)).toFloat(),
            )
            if (intent == BackspaceHoldIntent.ROTATING) sawRotatingCw = true
        }
        assertTrue(sawRotatingCw)
        assertEquals(BackspaceHoldIntent.CLEAR_ALL, cw.intent)

        // 6. Straight vertical back-and-forth shaking never triggers ROTATING or CLEAR_ALL
        val verticalJitter = newGesture()
        verticalJitter.begin(100f, 100f)
        repeat(6) {
            for (dy in listOf(-8f, -16f, -24f, -32f, -24f, -16f, -8f, 0f, 8f, 16f, 24f, 16f, 8f, 0f)) {
                val intent = verticalJitter.update(100f + (dy * 0.05f), 100f + dy)
                assertFalse(intent == BackspaceHoldIntent.ROTATING || intent == BackspaceHoldIntent.CLEAR_ALL)
            }
        }

        // 7. Straight horizontal back-and-forth shaking never triggers ROTATING or CLEAR_ALL
        val horizontalJitter = newGesture()
        horizontalJitter.begin(100f, 100f)
        repeat(6) {
            for (dx in listOf(-8f, -16f, -24f, -32f, -24f, -16f, -8f, 0f, 8f, 16f, 24f, 16f, 8f, 0f)) {
                val intent = horizontalJitter.update(100f + dx, 100f + (dx * 0.05f))
                assertFalse(intent == BackspaceHoldIntent.ROTATING || intent == BackspaceHoldIntent.CLEAR_ALL)
            }
        }

        // 8. Unclosed polyline (left 50 -> up 20 -> right 20 -> down 10 -> left 5) must NEVER trigger CLEAR_ALL
        fun tracePolyline(g: BackspaceHoldGesture, startX: Float, startY: Float, segments: List<Pair<Float, Float>>, stepPx: Float) {
            g.begin(startX, startY)
            var x = startX
            var y = startY
            for ((dx, dy) in segments) {
                val dist = kotlin.math.hypot(dx, dy)
                val steps = kotlin.math.max(1, kotlin.math.ceil((dist / stepPx).toDouble()).toInt())
                for (i in 1..steps) {
                    val nx = x + dx * (i.toFloat() / steps)
                    val ny = y + dy * (i.toFloat() / steps)
                    val intent = g.update(nx, ny)
                    assertFalse("Unclosed polyline must not unlock CLEAR_ALL", intent == BackspaceHoldIntent.CLEAR_ALL)
                }
                x += dx
                y += dy
            }
            assertFalse(g.intent == BackspaceHoldIntent.CLEAR_ALL)
        }
        val unclosedSegments = listOf(-50f to 0f, 0f to -20f, 20f to 0f, 0f to 10f, -5f to 0f)
        tracePolyline(newGesture(), 100f, 100f, unclosedSegments, stepPx = 4f)
        tracePolyline(newGesture(), 100f, 100f, unclosedSegments, stepPx = 5f)
        tracePolyline(newGesture(), 100f, 100f, unclosedSegments, stepPx = 2f)

        // 9. Concentric closed circle (anchor near center, then lead-out to orbit and close) unlocks CLEAR_ALL
        val concentric = newGesture()
        concentric.begin(100f, 100f)
        for (rStep in 1..5) {
            concentric.update(100f + rStep * 4f, 100f)
        }
        for (step in 1..36) {
            val a = step * (2.0 * kotlin.math.PI / 36.0)
            concentric.update(
                (100f + 20f * kotlin.math.cos(a)).toFloat(),
                (100f + 20f * kotlin.math.sin(a)).toFloat(),
            )
        }
        assertEquals(BackspaceHoldIntent.CLEAR_ALL, concentric.intent)

        // 10. Swiping up/down into hover then jumping straight back to anchor on final frame reverts to BATCH
        val upRevert = newGesture()
        upRevert.begin(100f, 100f)
        assertEquals(BackspaceHoldIntent.DELETE_BEFORE, upRevert.update(100f, 70f))
        assertEquals(BackspaceHoldIntent.BATCH, upRevert.update(100f, 100f))

        val downRevert = newGesture()
        downRevert.begin(100f, 100f)
        assertEquals(BackspaceHoldIntent.DELETE_AFTER, downRevert.update(100f, 130f))
        assertEquals(BackspaceHoldIntent.BATCH, downRevert.update(100f, 100f))

        // 11. Large closed circle (radius 90, 128 equal-angle steps) keeps start sample and unlocks CLEAR_ALL
        val largeCircle = newGesture()
        val largeRadius = 90f
        largeCircle.begin(100f + largeRadius, 100f)
        for (step in 1..128) {
            val a = step * (2.0 * kotlin.math.PI / 128.0)
            largeCircle.update(
                (100f + largeRadius * kotlin.math.cos(a)).toFloat(),
                (100f + largeRadius * kotlin.math.sin(a)).toFloat(),
            )
        }
        assertEquals(BackspaceHoldIntent.CLEAR_ALL, largeCircle.intent)
    }

    @Test
    fun shortcutEditorClearsOnBatchAndCursorSideDeleteKeys() {
        assertEquals("abc", applyShortcutEditorKey("abc", "⌫-hold-begin"))
        assertEquals("", applyShortcutEditorKey("abc", "⌫-batch"))
        assertEquals("", applyShortcutEditorKey("abc", "delete-before-cursor"))
        assertEquals("", applyShortcutEditorKey("abc", "delete-after-cursor"))
    }

    @Test
    fun autoCapDefaultsOffAndNeverTriggersOnRawOrNullContextEditors() {
        assertEquals(0, resolveAutoCapMode("text", android.text.InputType.TYPE_NULL))
        assertEquals(0, resolveAutoCapMode("text", android.text.InputType.TYPE_CLASS_TEXT))
        assertEquals(
            1,
            resolveAutoCapMode(
                "text",
                android.text.InputType.TYPE_CLASS_TEXT or android.text.InputType.TYPE_TEXT_FLAG_CAP_SENTENCES,
            ),
        )
        assertFalse(shouldAutoCapitalize(autoCapEnabled = false, autoCapMode = 1, beforeCursor = "", manualLowercaseOverride = false))
        assertFalse(shouldAutoCapitalize(autoCapEnabled = true, autoCapMode = 1, beforeCursor = null, manualLowercaseOverride = false))
        assertFalse(shouldAutoCapitalize(autoCapEnabled = true, autoCapMode = 1, beforeCursor = "", manualLowercaseOverride = true))
        assertTrue(shouldAutoCapitalize(autoCapEnabled = true, autoCapMode = 1, beforeCursor = "", manualLowercaseOverride = false))
        assertFalse(shouldAutoCapitalize(autoCapEnabled = true, autoCapMode = 1, beforeCursor = "a", manualLowercaseOverride = false))
    }
}
