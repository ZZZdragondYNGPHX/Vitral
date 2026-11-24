package dev.vitral.ime

import android.annotation.SuppressLint
import android.content.Context
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.os.SystemClock
import dev.vitral.ime.core.Core
import dev.vitral.ime.core.VitralNative
import dev.vitral.ime.core.unpack
import java.util.concurrent.atomic.AtomicBoolean
import org.json.JSONObject

internal class Microphone {
    private var current: AtomicBoolean? = null
    private var currentCancelled: AtomicBoolean? = null

    fun stop() {
        current?.set(false)
    }

    fun cancel() {
        currentCancelled?.set(true)
        stop()
        current = null
        currentCancelled = null
    }

    @SuppressLint("MissingPermission")
    fun start(
        context: Context,
        onPartial: (String) -> Unit = {},
        done: (Result<String>) -> Unit,
    ) {
        if (current != null) return
        val capturing = AtomicBoolean(true)
        val cancelled = AtomicBoolean(false)
        val previewInFlight = AtomicBoolean(false)
        current = capturing
        currentCancelled = cancelled
        Core.runVoice(
            context,
            { handle ->
                val size =
                    AudioRecord.getMinBufferSize(
                        16000,
                        AudioFormat.CHANNEL_IN_MONO,
                        AudioFormat.ENCODING_PCM_16BIT,
                    )
                check(size > 0) { "麦克风不可用" }
                val mic =
                    AudioRecord(
                        MediaRecorder.AudioSource.VOICE_RECOGNITION,
                        16000,
                        AudioFormat.CHANNEL_IN_MONO,
                        AudioFormat.ENCODING_PCM_16BIT,
                        size.coerceAtLeast(4096),
                    )
                var totalSamples = 0
                var lastPreviewAt = SystemClock.uptimeMillis()
                var lastPartial = ""
                try {
                    check(VitralNative.voiceStart(handle) == 0)
                    mic.startRecording()
                    val pcm = ShortArray(1024)
                    while (capturing.get()) {
                        val n = mic.read(pcm, 0, pcm.size)
                        check(n >= 0) { "无法读取音频" }
                        if (n > 0) {
                            check(VitralNative.voicePushPcm(handle, pcm, n) == 0)
                            totalSamples += n
                            val now = SystemClock.uptimeMillis()
                            if (
                                totalSamples >= 8000 &&
                                    now - lastPreviewAt >= 800L &&
                                    !cancelled.get() &&
                                    previewInFlight.compareAndSet(false, true)
                            ) {
                                lastPreviewAt = now
                                Core.call(context, "voice-preview", background = true) { previewResult ->
                                    previewInFlight.set(false)
                                    val partial =
                                        (previewResult.getOrNull() as? JSONObject)
                                            ?.optString("transcript")
                                            .orEmpty()
                                            .trim()
                                    if (
                                        current === capturing &&
                                            capturing.get() &&
                                            !cancelled.get() &&
                                            partial.isNotEmpty() &&
                                            partial != lastPartial
                                    ) {
                                        lastPartial = partial
                                        onPartial(partial)
                                    }
                                }
                            }
                        }
                    }
                } finally {
                    mic.release()
                }
                if (cancelled.get()) {
                    JSONObject().put("transcript", "")
                } else {
                    unpack(VitralNative.voiceStop(handle))
                }
            },
        ) { result ->
            if (current === capturing) {
                current = null
                currentCancelled = null
                if (!cancelled.get()) {
                    done(result.mapCatching { (it as JSONObject).getString("transcript") })
                }
            }
        }
    }
}
