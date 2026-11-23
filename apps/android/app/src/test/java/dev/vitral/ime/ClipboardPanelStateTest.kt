package dev.vitral.ime

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ClipboardPanelStateTest {
    @Test
    fun onlyPinnedItemsCanEditShortcutCode() {
        val pinned = ClipboardItemSnapshot("p1", "收藏内容", pinned = true, shortcutCode = null)
        val normal = ClipboardItemSnapshot("n1", "普通剪贴板", pinned = false, shortcutCode = null)
        assertTrue(canEditClipboardShortcut(pinned))
        assertFalse(canEditClipboardShortcut(normal))
    }

    @Test
    fun optimisticPinAndUnpinReorderAndClearShortcut() {
        val initial =
            listOf(
                ClipboardItemSnapshot("p1", "one", pinned = true, shortcutCode = "aa"),
                ClipboardItemSnapshot("n1", "two", pinned = false, shortcutCode = null),
                ClipboardItemSnapshot("n2", "three", pinned = false, shortcutCode = null),
            )

        val afterPin = applyOptimisticPin(initial, "n2", pinned = true)
        assertEquals(listOf("p1", "n2", "n1"), afterPin.map { it.id })
        assertTrue(afterPin[1].pinned)
        assertNull(afterPin[1].shortcutCode)

        val afterCode =
            applyOptimisticPin(
                afterPin,
                "n2",
                pinned = true,
                shortcutCode = " KSB!123 ",
                updateShortcut = true,
            )
        assertEquals("ksb", afterCode[1].shortcutCode)

        val afterUnpin = applyOptimisticPin(afterCode, "n2", pinned = false)
        assertEquals(listOf("p1", "n2", "n1"), afterUnpin.map { it.id })
        assertFalse(afterUnpin[1].pinned)
        assertNull(afterUnpin[1].shortcutCode)

        val afterClear = applyOptimisticClearUnpinned(afterUnpin)
        assertEquals(listOf("p1"), afterClear.map { it.id })
    }

    @Test
    fun shortcutEditorKeyHandlesLettersBackspaceAndClear() {
        var code = ""
        code = applyShortcutEditorKey(code, "K")
        code = applyShortcutEditorKey(code, "s")
        code = applyShortcutEditorKey(code, "b")
        code = applyShortcutEditorKey(code, "1")
        assertEquals("ksb", code)

        code = applyShortcutEditorKey(code, "⌫")
        assertEquals("ks", code)

        code = applyShortcutEditorKey(code, "清空")
        assertEquals("", code)

        val longCode = (1..24).fold("") { acc, _ -> applyShortcutEditorKey(acc, "a") }
        assertEquals(MAX_SHORTCUT_CODE_LENGTH, longCode.length)
    }
}
