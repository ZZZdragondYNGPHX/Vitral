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

internal fun dockedKeyboardHeight(
    screenHeightPx: Int,
    usableHeightPx: Int,
    density: Float,
    isLandscape: Boolean,
    isFiveRow: Boolean,
    heightPreset: String,
    dockedScreenRatio: Float,
): Int {
    fun toPx(dp: Int) = (dp * density + 0.5f).toInt()
    if (isLandscape) {
        val targetDp =
            when (heightPreset) {
                "compact" -> 200
                "comfort" -> 236
                else -> 218
            } + if (isFiveRow) 24 else 0
        val maxRatio = if (isFiveRow) 0.64f else 0.58f
        val maxPx = (usableHeightPx * maxRatio).toInt().coerceAtLeast(1)
        val minPx = toPx(168).coerceAtMost(usableHeightPx)
        return toPx(targetDp).coerceAtMost(maxPx).coerceAtLeast(minPx)
    }
    val scale =
        when (heightPreset) {
            "compact" -> 1.06f
            "comfort" -> 1.30f
            else -> 1.18f
        }
    val minDockDp =
        when (heightPreset) {
            "compact" -> 296
            "comfort" -> 352
            else -> 324
        } + if (isFiveRow) 46 else 0
    val dockBase =
        (screenHeightPx * dockedScreenRatio * scale).toInt() + if (isFiveRow) toPx(42) else 0
    return dockBase.coerceAtLeast(toPx(minDockDp)).coerceAtMost((usableHeightPx * 0.56f).toInt())
}

internal fun dockedHorizontalPadding(
    screenWidthPx: Int,
    leftInsetPx: Int,
    rightInsetPx: Int,
    density: Float,
    isLandscape: Boolean,
): Pair<Int, Int> {
    fun toPx(dp: Int) = (dp * density + 0.5f).toInt()
    if (!isLandscape) {
        return (toPx(6) + leftInsetPx) to (toPx(6) + rightInsetPx)
    }
    val baseLeft = toPx(8) + leftInsetPx
    val baseRight = toPx(8) + rightInsetPx
    val avail = (screenWidthPx - baseLeft - baseRight).coerceAtLeast(0)
    val maxGridPx = toPx(680)
    val extraEach = ((avail - maxGridPx) / 2).coerceIn(0, toPx(56))
    return (baseLeft + extraEach) to (baseRight + extraEach)
}
