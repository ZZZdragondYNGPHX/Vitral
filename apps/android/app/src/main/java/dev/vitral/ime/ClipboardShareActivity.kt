package dev.vitral.ime

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.widget.Toast
import dev.vitral.ime.core.Core
import dev.vitral.ime.core.json

internal fun pushClipboardTextAndFinish(
    activity: Activity,
    text: String,
    sourceLabel: String,
    timestampMs: Long = System.currentTimeMillis(),
    rawClipStamp: Long = 0L,
    recordFallbackText: Boolean = false,
) {
    val pureOffline = Core.settings(activity).optBoolean("pure_offline", false)
    Core.call(
        activity,
        "clipboard-push",
        json(
            "payload" to
                json(
                    "content" to text,
                    "timestamp_ms" to timestampMs,
                )
        ),
        background = true,
    ) { result ->
        val msg =
            result.fold(
                {
                    val editor = activity.getSharedPreferences("vitral", 0).edit()
                    if (rawClipStamp > 0L) editor.putLong("clipboard_stamp", rawClipStamp)
                    if (recordFallbackText) editor.putString("clipboard_last_text", text)
                    editor.apply()
                    if (pureOffline) "已将${sourceLabel}保存到本机剪贴板（纯离线未同步）"
                    else "已将${sourceLabel}加入跨端剪贴板队列"
                },
                { "同步失败：${it.message.orEmpty()}" },
            )
        Toast.makeText(activity.applicationContext, msg, Toast.LENGTH_SHORT).show()
        activity.finish()
    }
}

/** Exported system Share-sheet target ("发送到 Vitral 剪贴板", ACTION_SEND text/plain). */
class ClipboardShareActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (intent?.action != Intent.ACTION_SEND || intent.type?.startsWith("text/") != true) {
            finish()
            return
        }
        val shared =
            intent.getCharSequenceExtra(Intent.EXTRA_TEXT)?.toString()
                ?: intent.clipData?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString()
                ?: ""
        if (shared.isEmpty()) {
            Toast.makeText(this, "未收到可同步的文本", Toast.LENGTH_SHORT).show()
            finish()
            return
        }
        pushClipboardTextAndFinish(this, shared, sourceLabel = "分享文本")
    }
}
