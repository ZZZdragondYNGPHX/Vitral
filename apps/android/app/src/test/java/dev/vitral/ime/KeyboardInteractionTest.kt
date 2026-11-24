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
}
