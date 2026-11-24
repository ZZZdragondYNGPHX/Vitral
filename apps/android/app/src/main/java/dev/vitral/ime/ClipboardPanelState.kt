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

internal fun clipboardPreviewText(content: String, maxChars: Int = 220): String {
    val flattened = content.take(maxChars + 32).replace(Regex("[\\r\\n\\t]+"), " ").trim()
    return if (content.length > maxChars || flattened.length > maxChars) {
        flattened.take(maxChars).trimEnd() + "…"
    } else {
        flattened
    }
}

internal fun filterClipboardItems(
    items: List<ClipboardItemSnapshot>,
    query: String,
    preset: String = "all",
): List<ClipboardItemSnapshot> {
    val q = query.trim().lowercase()
    return items.filter { item ->
        val matchesPreset =
            when (preset) {
                "pinned" -> item.pinned
                "links" ->
                    item.content.contains("http://", ignoreCase = true) ||
                        item.content.contains("https://", ignoreCase = true) ||
                        item.content.contains("www.", ignoreCase = true)
                "numbers" -> Regex("\\d{4,}").containsMatchIn(item.content)
                else -> true
            }
        val matchesQuery =
            q.isEmpty() ||
                item.content.lowercase().contains(q) ||
                (item.shortcutCode?.lowercase()?.contains(q) == true)
        matchesPreset && matchesQuery
    }
}

private val SEGMENT_REGEX =
    Regex(
        "https?://[^\\s]+|[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\\.[A-Za-z]{2,}|[A-Za-z0-9_./:-]+|[\\u4E00-\\u9FFF]+|[^\\s\\u4E00-\\u9FFFA-Za-z0-9]+",
    )

/** Big-Bang clipboard segmenter: keeps URLs/emails/codes/numbers intact and splits long CJK runs cleanly. */
internal fun tokenizeClipboardSegments(text: String, maxSegments: Int = 48): List<String> {
    val bounded = text.take(1500)
    val result = ArrayList<String>()
    for (match in SEGMENT_REGEX.findAll(bounded)) {
        if (result.size >= maxSegments) break
        val token = match.value.trim()
        if (token.isEmpty()) continue
        val allCjk = token.all { it in '\u4E00'..'\u9FFF' }
        if (allCjk && token.length > 4) {
            var idx = 0
            while (idx < token.length && result.size < maxSegments) {
                val rem = token.length - idx
                val step =
                    when {
                        rem <= 4 -> rem
                        rem == 5 -> 2
                        else -> 3
                    }
                result.add(token.substring(idx, idx + step))
                idx += step
            }
        } else {
            result.add(token)
        }
    }
    return result
}
