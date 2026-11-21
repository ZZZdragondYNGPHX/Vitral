package dev.vitral.ime.core

import android.content.Context
import android.os.Handler
import android.os.Looper
import java.io.File
import java.util.concurrent.Executors
import org.json.JSONObject

object VitralNative {
    init {
        System.loadLibrary("vitral_jni")
    }

    @JvmStatic external fun abiVersion(): Int

    @JvmStatic external fun nativeInit(configJson: String): Long

    @JvmStatic external fun shutdown(handle: Long)

    @JvmStatic external fun setPureOffline(handle: Long, enabled: Boolean): Int

    @JvmStatic external fun isPureOffline(handle: Long): Boolean

    @JvmStatic external fun setSchema(handle: Long, schemaId: Int): Int

    @JvmStatic external fun setOpenccMode(handle: Long, mode: Int): Int

    @JvmStatic external fun reloadAssets(handle: Long): String

    @JvmStatic external fun onAppFocus(handle: Long, appId: String): String

    @JvmStatic
    external fun processKey(handle: Long, keycode: Int, modifiers: Int, swipeDir: Int): String

    @JvmStatic external fun selectCandidate(handle: Long, index: Int): String

    @JvmStatic external fun pinCandidate(handle: Long, index: Int): String

    @JvmStatic external fun eraseCandidate(handle: Long, index: Int): String

    @JvmStatic external fun hwBegin(handle: Long, mode: Int): Int

    @JvmStatic
    external fun hwPushStroke(
        handle: Long,
        xs: FloatArray,
        ys: FloatArray,
        timestampsMs: LongArray,
        penUp: Boolean,
    ): String

    @JvmStatic external fun hwEnd(handle: Long): String

    @JvmStatic external fun hwClear(handle: Long): Int

    @JvmStatic external fun hwOverrideSlot(handle: Long, slotIdx: Int, candidateIdx: Int): String

    @JvmStatic external fun importLexicon(handle: Long, path: String, formatHint: Int): String

    @JvmStatic external fun clipboardPush(handle: Long, payloadJson: String): String

    @JvmStatic external fun clipboardState(handle: Long): String

    @JvmStatic
    external fun clipboardIngestRemote(
        handle: Long,
        payloadJson: String,
        isColdBoot: Boolean,
    ): String

    @JvmStatic external fun quickPasteState(handle: Long): String

    @JvmStatic external fun consumeQuickPaste(handle: Long): String

    @JvmStatic external fun onImeOpen(handle: Long): String

    @JvmStatic external fun onImeClose(handle: Long): String

    @JvmStatic external fun dismissQuickPaste(handle: Long): String

    @JvmStatic
    external fun clipboardPin(handle: Long, itemId: String, shortcutCode: String?): String

    @JvmStatic external fun clipboardClearUnpinned(handle: Long): String

    @JvmStatic external fun relayDevicesMru(handle: Long): String

    @JvmStatic
    external fun relaySend(
        handle: Long,
        deviceId: String,
        payloadOrPath: String,
        isFilePath: Boolean,
    ): String

    @JvmStatic external fun relayPollInbox(handle: Long): String

    @JvmStatic external fun relayCleanupOrphans(handle: Long): String

    @JvmStatic external fun voiceStart(handle: Long): Int

    @JvmStatic external fun voicePushPcm(handle: Long, pcm: ShortArray, len: Int): Int

    @JvmStatic external fun voiceStop(handle: Long): String

    @JvmStatic
    external fun aiInline(handle: Long, mode: Int, text: String, targetLang: String): String

    @JvmStatic external fun restoreFromKoofr(handle: Long, webdavCfgJson: String): String
}

internal object PlatformCommands {
    @JvmStatic external fun execute(handle: Long, name: String, json: String): String
}

fun json(vararg fields: Pair<String, Any?>) =
    JSONObject().apply { for ((key, value) in fields) put(key, value ?: JSONObject.NULL) }

fun unpack(raw: String): Any {
    val result = JSONObject(raw)
    check(result.getInt("status") == 0) { result.optString("error") }
    return result.get("data")
}

object Core {
    private val input = Executors.newSingleThreadExecutor()
    private val services = Executors.newSingleThreadExecutor()
    private val main = Handler(Looper.getMainLooper())
    private var handle = 0L

    fun settings(ctx: Context) =
        JSONObject(ctx.getSharedPreferences("vitral", 0).getString("settings", "{}") ?: "{}")

    @Synchronized
    private fun engine(ctx: Context): Long {
        if (handle != 0L) return handle
        val version = ctx.packageManager.getPackageInfo(ctx.packageName, 0).lastUpdateTime
        val base = File(ctx.noBackupFilesDir, "models").apply { mkdirs() }
        val dir = File(base, version.toString())
        if (!dir.isDirectory) {
            val temp = File(base, "staging")
            temp.deleteRecursively()
            fun extract(path: String, file: File) {
                val children = ctx.assets.list(path).orEmpty()
                if (children.isEmpty()) {
                    ctx.assets.open(path).use { source ->
                        file.outputStream().use { source.copyTo(it) }
                    }
                } else {
                    check(file.mkdirs() || file.isDirectory)
                    for (name in children) extract("$path/$name", File(file, name))
                }
            }
            try {
                extract("models", temp)
                check(temp.renameTo(dir)) { "无法安装离线模型" }
            } finally {
                temp.deleteRecursively()
            }
            base.listFiles()?.filter { it != dir }?.forEach { it.deleteRecursively() }
        }
        val preferences = ctx.getSharedPreferences("vitral", 0)
        val id =
            preferences.getString("deviceId", null)
                ?: java.util.UUID.randomUUID().toString().also {
                    preferences.edit().putString("deviceId", it).apply()
                }
        val cfg =
            settings(ctx)
                .put("background_sync", true)
                .put("platform", "android")
                .put("device_id", id)
                .put("device_name", android.os.Build.MODEL)
                .put("assets_dir", dir.absolutePath)
                .put("data_dir", ctx.noBackupFilesDir.absolutePath)
        handle = VitralNative.nativeInit(cfg.toString())
        check(handle != 0L) { "输入引擎初始化失败" }
        return handle
    }

    fun run(
        ctx: Context,
        background: Boolean = false,
        operation: (Long) -> Any,
        done: (Result<Any>) -> Unit = {},
    ) {
        val app = ctx.applicationContext
        (if (background) services else input).execute {
            val result = runCatching { operation(engine(app)) }
            main.post { done(result) }
        }
    }

    fun call(
        ctx: Context,
        name: String,
        data: JSONObject = JSONObject(),
        background: Boolean = false,
        done: (Result<Any>) -> Unit = {},
    ) =
        run(
            ctx,
            background,
            { h -> unpack(PlatformCommands.execute(h, name, data.toString())) },
            done,
        )

    fun save(ctx: Context, cfg: JSONObject, done: (Result<Any>) -> Unit) =
        call(ctx, "configure", cfg, true) { result ->
            if (result.isSuccess)
                ctx.getSharedPreferences("vitral", 0)
                    .edit()
                    .putString("settings", cfg.toString())
                    .apply()
            done(result)
        }
}
