package dev.vitral.ime

import android.app.Activity
import android.content.ClipboardManager
import android.os.Bundle
import android.widget.Toast
import dev.vitral.ime.core.Core

/**
 * Non-exported transparent Activity launched solely by ClipboardSyncTileService so the tile
 * can read the foreground system clipboard on Android 10+ without exposing a public trigger.
 */
class ClipboardTileCaptureActivity : Activity() {
    private var dispatched = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (intent?.action != ACTION_SYNC_CLIPBOARD) {
            finish()
        }
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (!hasFocus || dispatched) return
        dispatched = true
        val cm = getSystemService(ClipboardManager::class.java)
        val clip = cm?.primaryClip
        val description = clip?.description
        val text = clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString().orEmpty()
        if (text.isEmpty() || description == null) {
            Toast.makeText(this, "当前系统剪贴板为空", Toast.LENGTH_SHORT).show()
            finish()
            return
        }
        val ownMarker = description.extras?.getString(COPY_MARKER)
        val rawStamp = description.timestamp
        val prefs = getSharedPreferences("vitral", 0)
        val alreadyCaptured =
            ownMarker != null ||
                (rawStamp > 0L && prefs.getLong("clipboard_stamp", -1L) == rawStamp) ||
                (rawStamp <= 0L && prefs.getString("clipboard_last_text", null) == text)
        if (alreadyCaptured) {
            Core.call(this, "clipboard-poll-current", background = true) { result ->
                val msg =
                    result.fold(
                        { "当前剪贴板已是最新同步状态" },
                        { "同步刷新失败：${it.message.orEmpty()}" },
                    )
                Toast.makeText(applicationContext, msg, Toast.LENGTH_SHORT).show()
                finish()
            }
            return
        }
        val effectiveStamp = if (rawStamp > 0L) rawStamp else System.currentTimeMillis()
        pushClipboardTextAndFinish(
            this,
            text,
            sourceLabel = "当前剪贴板",
            timestampMs = effectiveStamp,
            rawClipStamp = rawStamp,
            recordFallbackText = rawStamp <= 0L,
        )
    }

    companion object {
        const val ACTION_SYNC_CLIPBOARD = "dev.vitral.ime.ACTION_SYNC_CLIPBOARD"
        private const val COPY_MARKER = "dev.vitral.copy_id"
    }
}
