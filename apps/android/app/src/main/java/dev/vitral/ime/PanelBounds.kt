package dev.vitral.ime

internal data class PanelBounds(val width: Int, val height: Int, val left: Int, val bottom: Int)

internal fun panelBounds(
    screenWidth: Int,
    screenHeight: Int,
    width: Int,
    height: Int,
    minimumWidth: Int,
    minimumHeight: Int,
    left: Int,
    bottom: Int,
): PanelBounds {
    val w = width.coerceIn(minimumWidth.coerceAtMost(screenWidth), screenWidth)
    val h = height.coerceIn(minimumHeight.coerceAtMost(screenHeight), screenHeight)
    return PanelBounds(
        w,
        h,
        left.coerceIn(0, screenWidth - w),
        bottom.coerceIn(0, screenHeight - h),
    )
}
