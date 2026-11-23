package dev.vitral.ime

import android.annotation.SuppressLint
import android.content.Context
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import dev.vitral.ime.core.Core
import dev.vitral.ime.core.VitralNative
import dev.vitral.ime.core.unpack
import java.util.concurrent.atomic.AtomicBoolean
import org.json.JSONObject

internal class Microphone {
    private var current: AtomicBoolean? = null

    fun stop() {
        current?.set(false)
    }

    fun cancel() {
        stop()
        current = null
    }

    @SuppressLint("MissingPermission")
    fun start(context: Context, done: (Result<String>) -> Unit) {
        if (current != null) return
        val capturing = AtomicBoolean(true)
        current = capturing
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
                try {
                    check(VitralNative.voiceStart(handle) == 0)
                    mic.startRecording()
                    val pcm = ShortArray(1024)
                    while (capturing.get()) {
                        val n = mic.read(pcm, 0, pcm.size)
                        check(n >= 0) { "无法读取音频" }
                        if (n > 0) check(VitralNative.voicePushPcm(handle, pcm, n) == 0)
                    }
                } finally {
                    mic.release()
                }
                unpack(VitralNative.voiceStop(handle))
            },
        ) { result ->
            if (current === capturing) {
                current = null
                done(result.mapCatching { (it as JSONObject).getString("transcript") })
            }
        }
    }
}
