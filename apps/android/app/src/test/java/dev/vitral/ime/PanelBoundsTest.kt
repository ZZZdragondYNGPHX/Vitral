package dev.vitral.ime

import org.junit.Assert.assertEquals
import org.junit.Test

class PanelBoundsTest {
    @Test
    fun resizeAndDragStayOnScreen() {
        assertEquals(
            PanelBounds(220, 165, 180, 0),
            panelBounds(400, 800, -50, 1, 220, 165, 390, -80),
        )
        assertEquals(
            PanelBounds(400, 800, 0, 0),
            panelBounds(400, 800, 600, 1400, 220, 165, 240, 400),
        )
    }

    @Test
    fun splitScreenCanBeSmallerThanMinimum() {
        assertEquals(PanelBounds(160, 100, 0, 0), panelBounds(160, 100, 220, 165, 220, 165, 4, 12))
    }

    @Test
    fun landscapeFiveRowLeavesRoomyKeysAndSafeSidePadding() {
        val density = 3f
        val height =
            dockedKeyboardHeight(
                screenHeightPx = (400 * density).toInt(),
                usableHeightPx = (380 * density).toInt(),
                density = density,
                isLandscape = true,
                isFiveRow = true,
                heightPreset = "standard",
                dockedScreenRatio = 0.26f,
            )
        // ~242dp total => after 67dp chrome, >= 175dp for 5 key rows (~35dp/row).
        assertEquals((242 * density).toInt(), height)

        val (leftPad, rightPad) =
            dockedHorizontalPadding(
                screenWidthPx = (880 * density).toInt(),
                leftInsetPx = (32 * density).toInt(),
                rightInsetPx = 0,
                density = density,
                isLandscape = true,
            )
        assertEquals((96 * density).toInt(), leftPad)
        assertEquals((64 * density).toInt(), rightPad)
    }
}
