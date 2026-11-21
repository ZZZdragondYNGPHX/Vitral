package dev.vitral.ime

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Bundle
import android.provider.OpenableColumns
import android.provider.Settings
import android.view.inputmethod.InputMethodManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.*
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import dev.vitral.ime.core.*
import dev.vitral.ime.theme.VitralTokens as T
import java.io.File
import kotlin.math.roundToInt
import org.json.JSONArray
import org.json.JSONObject

class SettingsActivity : ComponentActivity() {
    private var notice by mutableStateOf("")
    private var busy by mutableStateOf(false)
    private val microphone =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) {
            notice = if (it) "麦克风已启用，长按空格即可录音" else "语音输入需要麦克风权限"
        }
    private val choose =
        registerForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
            if (uri == null) return@registerForActivityResult
            busy = true
            Core.run(
                this,
                true,
                { h ->
                    val name =
                        contentResolver
                            .query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
                            ?.use { if (it.moveToFirst()) it.getString(0) else "" }
                            .orEmpty()
                    val hint =
                        when {
                            name.endsWith(".dict.yaml", true) -> "dict_yaml"
                            name.endsWith(".scel", true) -> "scel"
                            name.endsWith(".tsv", true) -> "tsv"
                            else -> "txt"
                        }
                    val file = File.createTempFile("lexicon-", ".txt", cacheDir)
                    try {
                        requireNotNull(contentResolver.openInputStream(uri)).use { src ->
                            file.outputStream().use { src.copyTo(it) }
                        }
                        unpack(
                            PlatformCommands.execute(
                                h,
                                "import-lexicon",
                                json("path" to file.absolutePath, "format_hint" to hint).toString(),
                            )
                        )
                    } finally {
                        file.delete()
                    }
                },
            ) { r ->
                busy = false
                notice =
                    r.fold(
                        {
                            "已导入 " +
                                (it as JSONObject).optInt("imported_count") +
                                " 条，跳过 " +
                                it.optInt("skipped_count") +
                                " 条"
                        },
                        { it.message.orEmpty() },
                    )
            }
        }
    private val recorder = Microphone()
    private var recording by mutableStateOf(false)
    private var devices by mutableStateOf(listOf<JSONObject>())
    private var destination = ""
    private val received = mutableStateListOf<File>()
    private var exportItem: File? = null
    private val export =
        registerForActivityResult(
            ActivityResultContracts.CreateDocument("application/octet-stream")
        ) { uri ->
            val file = exportItem
            if (uri != null && file != null) {
                Core.run(
                    this,
                    true,
                    {
                        requireNotNull(contentResolver.openOutputStream(uri)).use { out ->
                            file.inputStream().use { it.copyTo(out) }
                        }
                        check(file.delete())
                    },
                ) { result ->
                    result
                        .onSuccess {
                            received.remove(file)
                            notice = "已保存"
                        }
                        .onFailure { notice = it.message.orEmpty() }
                }
            }
        }
    private val transfer =
        registerForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
            if (uri == null) return@registerForActivityResult
            val device = destination
            busy = true
            Core.run(
                this,
                true,
                { handle ->
                    val name =
                        contentResolver
                            .query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
                            ?.use { if (it.moveToFirst()) it.getString(0) else null }
                    val folder =
                        kotlin.io.path.createTempDirectory(cacheDir.toPath(), "transfer-").toFile()
                    try {
                        val file = File(folder, safeName(name))
                        requireNotNull(contentResolver.openInputStream(uri)).use { input ->
                            file.outputStream().use { input.copyTo(it) }
                        }
                        unpack(VitralNative.relaySend(handle, device, file.absolutePath, true))
                    } finally {
                        folder.deleteRecursively()
                    }
                },
            ) { result ->
                busy = false
                notice = result.fold({ "文件已发送" }, { it.message.orEmpty() })
            }
        }

    private fun safeName(name: String?) =
        name?.substringAfterLast('/')?.takeUnless { it.isBlank() || it == "." || it == ".." }
            ?: "Vitral.bin"

    private val inboxDir
        get() = File(noBackupFilesDir, "inbox")

    private fun readInbox() {
        busy = true
        Core.run(
            this,
            true,
            { handle ->
                val response = unpack(VitralNative.relayPollInbox(handle)) as JSONObject
                val items = response.getJSONArray("received_items")
                for (i in 0 until items.length()) {
                    val item = items.getJSONObject(i)
                    val folder = File(inboxDir, java.util.UUID.randomUUID().toString())
                    check(folder.mkdirs())
                    val file =
                        File(
                            folder,
                            safeName(
                                item.optString("file_name").takeUnless { it == "null" } ?: "文本.txt"
                            ),
                        )
                    val bytes = item.getJSONArray("raw_bytes")
                    file.outputStream().buffered().use { output ->
                        for (j in 0 until bytes.length()) output.write(bytes.getInt(j))
                    }
                }
                items.length()
            },
        ) { result ->
            busy = false
            refreshInbox()
            notice = result.fold({ "已接收 ${it} 项" }, { it.message.orEmpty() })
        }
    }

    private fun refreshInbox() {
        received.clear()
        received.addAll(inboxDir.walkTopDown().filter { it.isFile }.toList())
    }

    override fun onDestroy() {
        recorder.cancel()
        if (devices.isNotEmpty() || discoveryStarted)
            Core.call(this, "relay-stop-discovery", background = true)
        super.onDestroy()
    }

    private var discoveryStarted = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        refreshInbox()
        setContent {
            MaterialTheme(
                colorScheme =
                    if (androidx.compose.foundation.isSystemInDarkTheme())
                        darkColorScheme(primary = T.Dark.accentText)
                    else lightColorScheme(primary = T.Light.accentPrimary)
            ) {
                Surface(Modifier.fillMaxSize()) {
                    Column(
                        Modifier.systemBarsPadding()
                            .verticalScroll(rememberScrollState())
                            .padding(20.dp),
                        verticalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        Text("Vitral", style = MaterialTheme.typography.headlineLarge)
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            OutlinedButton(
                                onClick = {
                                    startActivity(Intent(Settings.ACTION_INPUT_METHOD_SETTINGS))
                                }
                            ) {
                                Text("启用键盘")
                            }
                            OutlinedButton(
                                onClick = {
                                    getSystemService(InputMethodManager::class.java)
                                        .showInputMethodPicker()
                                }
                            ) {
                                Text("切换键盘")
                            }
                        }
                        var section by remember { mutableIntStateOf(0) }
                        TabRow(selectedTabIndex = section) {
                            listOf("输入", "同步", "AI").forEachIndexed { i, s ->
                                Tab(section == i, { section = i }, text = { Text(s) })
                            }
                        }
                        val config = remember { Core.settings(this@SettingsActivity) }
                        var flypy by remember {
                            mutableStateOf(
                                config.optString("schema", "double_pinyin_flypy") ==
                                    "double_pinyin_flypy"
                            )
                        }
                        var offline by remember {
                            mutableStateOf(config.optBoolean("pure_offline"))
                        }
                        var size by remember {
                            mutableFloatStateOf(
                                config
                                    .optInt(
                                        "candidate_font_px",
                                        T.Typography.candidateFontSizeDefaultPx,
                                    )
                                    .toFloat()
                            )
                        }
                        var capacity by remember {
                            mutableFloatStateOf(
                                config.optInt("clipboard_ring_capacity", 50).toFloat()
                            )
                        }
                        var source by remember { mutableStateOf("") }
                        when (section) {
                            0 -> {
                                Check("小鹤双拼（关闭使用全拼）", flypy) { flypy = it }
                                Text("候选字号 " + size.roundToInt() + " px")
                                Slider(
                                    size,
                                    { size = it },
                                    modifier = Modifier.semantics { contentDescription = "候选字号" },
                                    valueRange =
                                        T.Typography.candidateFontSizeMinPx.toFloat()..T.Typography
                                                .candidateFontSizeMaxPx
                                                .toFloat(),
                                    steps = 8,
                                )
                                Text("剪贴板保留 " + capacity.roundToInt() + " 条")
                                Slider(
                                    capacity,
                                    { capacity = it },
                                    modifier = Modifier.semantics { contentDescription = "历史容量" },
                                    valueRange = 20f..200f,
                                )
                                OutlinedButton(
                                    onClick = { choose.launch(arrayOf("*/*")) },
                                    enabled = !busy,
                                ) {
                                    Text("导入词库")
                                }
                                OutlinedButton(
                                    onClick = {
                                        microphone.launch(Manifest.permission.RECORD_AUDIO)
                                    }
                                ) {
                                    Text("启用麦克风")
                                }
                            }
                            1 -> {
                                Check("纯离线模式", offline) { offline = it }
                                OutlinedButton(
                                    enabled = !offline && !busy,
                                    onClick = {
                                        busy = true
                                        discoveryStarted = true
                                        Core.call(
                                            this@SettingsActivity,
                                            "relay-discover",
                                            background = true,
                                        ) { result ->
                                            busy = false
                                            result
                                                .onSuccess { value ->
                                                    val entries = value as JSONArray
                                                    devices =
                                                        List(entries.length()) {
                                                            entries.getJSONObject(it)
                                                        }
                                                    notice =
                                                        if (devices.isEmpty()) "已开启接收，尚未发现设备"
                                                        else "请选择目标设备"
                                                }
                                                .onFailure { notice = it.message.orEmpty() }
                                        }
                                    },
                                ) {
                                    Text("发现设备并开启接收")
                                }
                                for (device in devices) {
                                    TextButton(
                                        enabled = !busy && !offline,
                                        onClick = {
                                            destination = device.getString("device_id")
                                            transfer.launch(arrayOf("*/*"))
                                        },
                                    ) {
                                        Text("发送文件到 " + device.getString("device_name"))
                                    }
                                }
                                OutlinedButton(enabled = !busy && !offline, onClick = ::readInbox) {
                                    Text("读取收件箱")
                                }
                                for (file in received) {
                                    TextButton(
                                        onClick = {
                                            exportItem = file
                                            export.launch(file.name)
                                        }
                                    ) {
                                        Text("保存 " + file.name)
                                    }
                                }
                                Text("Koofr WebDAV · " + T.Brand.cloudRoot)
                                Field(config, "koofr_endpoint", "服务地址")
                                Field(config, "koofr_username", "账户")
                                Field(config, "koofr_password", "应用密码", true)
                                Field(config, "sync_key", "同步密钥", true)
                                OutlinedButton(
                                    enabled = !busy && !offline,
                                    onClick = {
                                        busy = true
                                        Core.call(
                                            this@SettingsActivity,
                                            "restore-from-koofr",
                                            json("config" to config),
                                            true,
                                        ) { r ->
                                            r.onSuccess { value ->
                                                    val result = value as JSONObject
                                                    val restored =
                                                        result.getJSONObject("preferences")
                                                    for (key in restored.keys()) config.put(
                                                        key,
                                                        restored.get(key),
                                                    )
                                                    flypy =
                                                        config.optString("schema") ==
                                                            "double_pinyin_flypy"
                                                    capacity =
                                                        config
                                                            .optInt("clipboard_ring_capacity", 50)
                                                            .toFloat()
                                                    Core.save(this@SettingsActivity, config) { saved
                                                        ->
                                                        busy = false
                                                        notice =
                                                            saved.fold(
                                                                {
                                                                    "已恢复 " +
                                                                        result.optInt(
                                                                            "snapshots_applied"
                                                                        ) +
                                                                        " 项"
                                                                },
                                                                { it.message.orEmpty() },
                                                            )
                                                    }
                                                }
                                                .onFailure {
                                                    busy = false
                                                    notice = it.message.orEmpty()
                                                }
                                        }
                                    },
                                ) {
                                    Text("从 Koofr 恢复")
                                }
                                OutlinedButton(
                                    enabled = !busy && !offline,
                                    onClick = {
                                        busy = true
                                        Core.call(
                                            this@SettingsActivity,
                                            "storage",
                                            background = true,
                                        ) { r ->
                                            busy = false
                                            notice =
                                                r.fold(
                                                    {
                                                        val v = it as JSONObject
                                                        "词库与配置 " +
                                                            v.optLong("lexicon") +
                                                            " B · 剪贴板 " +
                                                            v.optLong("clipboard") +
                                                            " B · 中转 " +
                                                            v.optLong("relay") +
                                                            " B"
                                                    },
                                                    { it.message.orEmpty() },
                                                )
                                        }
                                    },
                                ) {
                                    Text("查看云盘用量")
                                }
                            }
                            2 -> {
                                Field(config, "ai_endpoint", "Ollama 或兼容接口")
                                Field(config, "ai_model", "模型")
                                Field(config, "ai_api_key", "API 密钥", true)
                                var tier3 by remember {
                                    mutableStateOf(config.optBoolean("ai_tier3_enabled"))
                                }
                                Check("使用 AI 辅助手写消歧", tier3) {
                                    tier3 = it
                                    config.put("ai_tier3_enabled", it)
                                }
                                OutlinedButton(
                                    enabled = !busy || recording,
                                    onClick = {
                                        if (recording) {
                                            recording = false
                                            recorder.stop()
                                        } else if (
                                            checkSelfPermission(Manifest.permission.RECORD_AUDIO) !=
                                                PackageManager.PERMISSION_GRANTED
                                        ) {
                                            microphone.launch(Manifest.permission.RECORD_AUDIO)
                                        } else {
                                            recording = true
                                            busy = true
                                            recorder.start(this@SettingsActivity) { result ->
                                                recording = false
                                                busy = false
                                                result
                                                    .onSuccess {
                                                        source = it
                                                        notice =
                                                            if (it.isBlank()) "未识别到语音" else "转写完成"
                                                    }
                                                    .onFailure { notice = it.message.orEmpty() }
                                            }
                                        }
                                    },
                                ) {
                                    Text(if (recording) "停止并转写" else "录音笔")
                                }
                                OutlinedTextField(
                                    source,
                                    { source = it },
                                    label = { Text("输入或听写原文") },
                                    modifier = Modifier.fillMaxWidth(),
                                )
                                Row {
                                    listOf("translate" to "译英", "polish" to "润色", "summary" to "摘要")
                                        .forEach { (mode, label) ->
                                            TextButton(
                                                enabled = !busy && !offline && source.isNotBlank(),
                                                onClick = {
                                                    busy = true
                                                    Core.call(
                                                        this@SettingsActivity,
                                                        "ai-inline",
                                                        json(
                                                            "mode" to mode,
                                                            "text" to source,
                                                            "target_lang" to "en",
                                                        ),
                                                        true,
                                                    ) { r ->
                                                        busy = false
                                                        notice =
                                                            r.fold(
                                                                {
                                                                    (it as JSONObject).optString(
                                                                        "output_text"
                                                                    )
                                                                },
                                                                { it.message.orEmpty() },
                                                            )
                                                    }
                                                },
                                            ) {
                                                Text(label)
                                            }
                                        }
                                }
                            }
                        }
                        Button(
                            enabled = !busy,
                            onClick = {
                                config
                                    .put("schema", if (flypy) "double_pinyin_flypy" else "rime_ice")
                                    .put("pure_offline", offline)
                                    .put("candidate_font_px", size.roundToInt())
                                    .put("clipboard_ring_capacity", capacity.roundToInt())
                                busy = true
                                Core.save(this@SettingsActivity, config) { r ->
                                    busy = false
                                    notice = r.fold({ "设置已保存" }, { it.message.orEmpty() })
                                }
                            },
                        ) {
                            Text(if (busy) "处理中…" else "保存设置")
                        }
                        Text(notice, Modifier.semantics { liveRegion = LiveRegionMode.Polite })
                    }
                }
            }
        }
    }
}

@Composable
private fun Check(label: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    Row(
        Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = androidx.compose.ui.Alignment.CenterVertically,
    ) {
        Text(label)
        Switch(checked, onChange, modifier = Modifier.semantics { contentDescription = label })
    }
}

@Composable
private fun Field(config: JSONObject, key: String, title: String, secret: Boolean = false) {
    var value by remember(key) { mutableStateOf(config.optString(key)) }
    OutlinedTextField(
        value,
        {
            value = it
            config.put(key, it)
        },
        Modifier.fillMaxWidth(),
        label = { Text(title) },
        singleLine = true,
        visualTransformation =
            if (secret) PasswordVisualTransformation() else VisualTransformation.None,
    )
}
