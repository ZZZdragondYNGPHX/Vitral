package dev.vitral.ime.core

import android.content.Context
import android.os.Handler
import android.os.Looper
import java.io.File
import java.util.concurrent.Executors
import org.json.JSONObject

object VitralNative {
    init {
        runCatching { System.loadLibrary("onnxruntime") }
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

    @JvmStatic
    external fun processTouchKey(
        handle: Long,
        keycode: Int,
        modifiers: Int,
        swipeDir: Int,
        alt1Char: Int,
        alt1Penalty: Float,
        alt2Char: Int,
        alt2Penalty: Float,
    ): String

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
    private val auxiliary = Executors.newSingleThreadExecutor()
    private val voice = Executors.newSingleThreadExecutor()
    private val services = Executors.newSingleThreadExecutor()
    private val main = Handler(Looper.getMainLooper())
    private var handle = 0L

    fun settings(ctx: Context) =
        JSONObject(ctx.getSharedPreferences("vitral", 0).getString("settings", "{}") ?: "{}")

    @Synchronized
    private fun engine(ctx: Context): Long {
        if (handle != 0L) return handle
        val version =
            runCatching {
                ctx.assets.open("models/fingerprint.txt").bufferedReader().use { it.readText().trim() }
            }
                .getOrNull()
                ?.takeIf { it.isNotEmpty() && it.all { ch -> ch in '0'..'9' || ch in 'a'..'f' } }
                ?: ctx.packageManager.getPackageInfo(ctx.packageName, 0).lastUpdateTime.toString()
        val base = File(ctx.noBackupFilesDir, "models").apply { mkdirs() }
        val dir = File(base, version)
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
            ctx.noBackupFilesDir
                .listFiles { f ->
                    f.isFile && f.name.startsWith("rime-corpus-") && f.name.endsWith(".bin")
                }
                ?.forEach { it.delete() }
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

    private fun dispatch(
        ctx: Context,
        executor: java.util.concurrent.ExecutorService,
        operation: (Long) -> Any,
        done: (Result<Any>) -> Unit,
    ) {
        val app = ctx.applicationContext
        executor.execute {
            val result = runCatching { operation(engine(app)) }
            main.post { done(result) }
        }
    }

    fun run(
        ctx: Context,
        background: Boolean = false,
        operation: (Long) -> Any,
        done: (Result<Any>) -> Unit = {},
    ) = dispatch(ctx, if (background) services else input, operation, done)

    fun runVoice(
        ctx: Context,
        operation: (Long) -> Any,
        done: (Result<Any>) -> Unit = {},
    ) = dispatch(ctx, voice, operation, done)

    fun call(
        ctx: Context,
        name: String,
        data: JSONObject = JSONObject(),
        background: Boolean = false,
        auxiliary: Boolean = false,
        done: (Result<Any>) -> Unit = {},
    ) =
        dispatch(
            ctx,
            when {
                background -> services
                auxiliary -> this.auxiliary
                else -> input
            },
            { h -> unpack(PlatformCommands.execute(h, name, data.toString())) },
            done,
        )

    fun save(ctx: Context, cfg: JSONObject, done: (Result<Any>) -> Unit) =
        call(ctx, "configure", cfg, true) { result ->
            if (result.isSuccess) {
                (result.getOrNull() as? JSONObject)?.optJSONArray("shortcuts")?.let { cfg.put("shortcuts", it) }
                ctx.getSharedPreferences("vitral", 0)
                    .edit()
                    .putString("settings", cfg.toString())
                    .apply()
            }
            done(result)
        }
}
