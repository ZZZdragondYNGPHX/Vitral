package dev.vitral.ime

import org.json.JSONArray
import org.json.JSONObject

internal const val MAX_SHORTCUT_CODE_LENGTH = 16

internal data class ClipboardItemSnapshot(
    val id: String,
    val content: String,
    val pinned: Boolean,
    val shortcutCode: String?,
)

internal fun parseClipboardRing(state: JSONObject): List<ClipboardItemSnapshot> {
    val ring = state.optJSONArray("ring") ?: JSONArray()
    val items = ArrayList<ClipboardItemSnapshot>(ring.length())
    for (i in 0 until ring.length()) {
        val item = ring.optJSONObject(i) ?: continue
        val id = item.optString("id")
        if (id.isEmpty()) continue
        val code =
            item.optString("shortcut_code").takeUnless { it.isEmpty() || it == "null" }
        items.add(
            ClipboardItemSnapshot(
                id = id,
                content = item.optString("content"),
                pinned = item.optBoolean("pinned"),
                shortcutCode = code,
            )
        )
    }
    return items
}

internal fun normalizeShortcutCode(raw: String): String =
    buildString {
        for (ch in raw.lowercase()) {
            if (ch in 'a'..'z' && length < MAX_SHORTCUT_CODE_LENGTH) {
                append(ch)
            }
        }
    }

internal fun canEditClipboardShortcut(item: ClipboardItemSnapshot): Boolean = item.pinned

internal fun applyOptimisticPin(
    items: List<ClipboardItemSnapshot>,
    itemId: String,
    pinned: Boolean,
    shortcutCode: String? = null,
    updateShortcut: Boolean = false,
): List<ClipboardItemSnapshot> {
    val normalized =
        if (pinned && updateShortcut) {
            shortcutCode?.let(::normalizeShortcutCode)?.takeIf { it.isNotEmpty() }
        } else {
            null
        }
    val updated =
        items.map { item ->
            if (item.id != itemId) {
                item
            } else {
                item.copy(
                    pinned = pinned,
                    shortcutCode =
                        when {
                            !pinned -> null
                            updateShortcut -> normalized
                            else -> item.shortcutCode
                        },
                )
            }
        }
    return updated.filter { it.pinned } + updated.filter { !it.pinned }
}

internal fun applyOptimisticClearUnpinned(
    items: List<ClipboardItemSnapshot>,
): List<ClipboardItemSnapshot> = items.filter { it.pinned }

internal fun applyShortcutEditorKey(current: String, key: String): String =
    when (key) {
        "⌫" -> if (current.isEmpty()) "" else current.dropLast(1)
        "清空",
        "delete-word",
        "clear-all" -> ""
        else -> {
            val ch = key.singleOrNull()?.lowercaseChar()
            if (ch != null && ch in 'a'..'z' && current.length < MAX_SHORTCUT_CODE_LENGTH) {
                current + ch
            } else {
                current
            }
        }
    }
