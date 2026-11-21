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
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.*
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.vitral.ime.core.*
import dev.vitral.ime.theme.VitralTokens as T
import java.io.File
import java.util.Locale
import kotlin.math.roundToInt
import org.json.JSONArray
import org.json.JSONObject

class SettingsActivity : ComponentActivity() {
    private var notice by mutableStateOf("")
    private var busy by mutableStateOf(false)
    private val microphone =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) {
            notice = if (it) "麦克风已启用，长按空格即可语音输入" else "语音输入需要麦克风权限"
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
                            notice = "文件已保存"
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

    private fun formatBytes(bytes: Long): String =
        when {
            bytes <= 0 -> "0 B"
            bytes < 1024 -> "$bytes B"
            bytes < 1024 * 1024 -> String.format(Locale.US, "%.1f KB", bytes / 1024.0)
            else -> String.format(Locale.US, "%.2f MB", bytes / (1024.0 * 1024.0))
        }

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
            val dark = androidx.compose.foundation.isSystemInDarkTheme()
            val scheme =
                if (dark)
                    darkColorScheme(
                        primary = T.Dark.accentPrimary,
                        onPrimary = Color.White,
                        background = Color(0xFF0B1019),
                        surface = Color(0xFF141925),
                        surfaceVariant = Color(0xFF1C2333),
                        onBackground = T.Dark.textPrimary,
                        onSurface = T.Dark.textPrimary,
                        onSurfaceVariant = T.Dark.textSecondary,
                    )
                else
                    lightColorScheme(
                        primary = T.Light.accentPrimary,
                        onPrimary = Color.White,
                        background = Color(0xFFEAF1F8),
                        surface = Color(0xFFF8FBFF),
                        surfaceVariant = Color(0xFFE2ECF6),
                        onBackground = T.Light.textPrimary,
                        onSurface = T.Light.textPrimary,
                        onSurfaceVariant = T.Light.textSecondary,
                    )

            MaterialTheme(colorScheme = scheme) {
                val config = remember { Core.settings(this@SettingsActivity) }
                var section by remember { mutableIntStateOf(0) }
                var flypy by remember {
                    mutableStateOf(
                        config.optString("schema", "double_pinyin_flypy") == "double_pinyin_flypy"
                    )
                }
                var offline by remember { mutableStateOf(config.optBoolean("pure_offline")) }
                var size by remember {
                    mutableFloatStateOf(
                        config
                            .optInt("candidate_font_px", T.Typography.candidateFontSizeDefaultPx)
                            .toFloat()
                    )
                }
                var capacity by remember {
                    mutableFloatStateOf(config.optInt("clipboard_ring_capacity", 50).toFloat())
                }
                var source by remember { mutableStateOf("") }

                Scaffold(
                    containerColor = MaterialTheme.colorScheme.background,
                    bottomBar = {
                        Surface(
                            tonalElevation = 3.dp,
                            shadowElevation = 6.dp,
                            color = MaterialTheme.colorScheme.surface,
                        ) {
                            Row(
                                Modifier.fillMaxWidth()
                                    .navigationBarsPadding()
                                    .padding(horizontal = 16.dp, vertical = 10.dp),
                                horizontalArrangement = Arrangement.SpaceBetween,
                                verticalAlignment = Alignment.CenterVertically,
                            ) {
                                Text(
                                    notice.ifBlank { if (offline) "纯离线模式" else "修改后点击右侧保存" },
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                    modifier =
                                        Modifier.weight(1f)
                                            .padding(end = 12.dp)
                                            .semantics { liveRegion = LiveRegionMode.Polite },
                                )
                                Button(
                                    enabled = !busy,
                                    shape = RoundedCornerShape(12.dp),
                                    onClick = {
                                        config
                                            .put(
                                                "schema",
                                                if (flypy) "double_pinyin_flypy" else "rime_ice",
                                            )
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
                            }
                        }
                    },
                ) { innerPadding ->
                    Column(
                        Modifier.fillMaxSize()
                            .padding(innerPadding)
                            .statusBarsPadding()
                            .verticalScroll(rememberScrollState())
                            .padding(horizontal = 16.dp, vertical = 14.dp),
                        verticalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        Row(
                            Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.SpaceBetween,
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Column {
                                Text(
                                    "Vitral",
                                    style = MaterialTheme.typography.headlineMedium,
                                    fontWeight = FontWeight.Bold,
                                )
                                Text(
                                    "纯粹晶透输入法设置",
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                FilledTonalButton(
                                    onClick = {
                                        startActivity(Intent(Settings.ACTION_INPUT_METHOD_SETTINGS))
                                    },
                                    contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp),
                                ) {
                                    Text("启用")
                                }
                                OutlinedButton(
                                    onClick = {
                                        getSystemService(InputMethodManager::class.java)
                                            .showInputMethodPicker()
                                    },
                                    contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp),
                                ) {
                                    Text("切换")
                                }
                            }
                        }

                        TabRow(
                            selectedTabIndex = section,
                            containerColor = Color.Transparent,
                        ) {
                            listOf("输入", "同步", "语音与AI", "快捷键").forEachIndexed { i, s ->
                                Tab(section == i, { section = i }, text = { Text(s) })
                            }
                        }

                        when (section) {
                            3 -> ShortcutSettings(config) { notice = it }
                            0 -> {
                                // Live candidate font preview
                                SectionCard("候选字号预览", "拖动下方滑块实时查看效果") {
                                    Row(
                                        Modifier.fillMaxWidth()
                                            .clip(RoundedCornerShape(14.dp))
                                            .background(
                                                if (dark) Color(0xFF1A2233) else Color(0xFFFFFFFF)
                                            )
                                            .border(
                                                1.dp,
                                                if (dark) Color(0x28FFFFFF) else Color(0x180F172A),
                                                RoundedCornerShape(14.dp),
                                            )
                                            .padding(horizontal = 10.dp, vertical = 10.dp),
                                        horizontalArrangement = Arrangement.SpaceEvenly,
                                        verticalAlignment = Alignment.CenterVertically,
                                    ) {
                                        listOf("今日", "近日", "尽日", "金日").forEachIndexed { idx, w ->
                                            val active = idx == 0
                                            Text(
                                                "${idx + 1} $w",
                                                fontSize = size.sp,
                                                fontWeight = if (active) FontWeight.SemiBold else FontWeight.Normal,
                                                color =
                                                    if (active)
                                                        (if (dark) T.Dark.accentText else T.Light.accentText)
                                                    else MaterialTheme.colorScheme.onSurface,
                                                modifier =
                                                    if (active)
                                                        Modifier.clip(RoundedCornerShape(9.dp))
                                                            .background(
                                                                if (dark) T.Dark.accentSoft
                                                                else T.Light.accentSoft
                                                            )
                                                            .padding(horizontal = 8.dp, vertical = 3.dp)
                                                    else Modifier.padding(horizontal = 6.dp, vertical = 3.dp),
                                            )
                                        }
                                    }
                                }

                                SectionCard("输入与词库", "小鹤双拼四排键 · 全拼五排数字行") {
                                    Check("小鹤双拼（关闭使用雾凇全拼）", flypy) { flypy = it }
                                    HorizontalDivider(Modifier.padding(vertical = 4.dp))
                                    Row(
                                        Modifier.fillMaxWidth(),
                                        horizontalArrangement = Arrangement.SpaceBetween,
                                    ) {
                                        Text("候选字号")
                                        Text(
                                            "${size.roundToInt()} px",
                                            color = MaterialTheme.colorScheme.primary,
                                            fontWeight = FontWeight.SemiBold,
                                        )
                                    }
                                    Slider(
                                        size,
                                        { size = it },
                                        modifier = Modifier.semantics { contentDescription = "候选字号" },
                                        valueRange =
                                            T.Typography.candidateFontSizeMinPx.toFloat()..T
                                                .Typography
                                                .candidateFontSizeMaxPx
                                                .toFloat(),
                                        steps = 8,
                                    )
                                    Row(
                                        Modifier.fillMaxWidth(),
                                        horizontalArrangement = Arrangement.SpaceBetween,
                                    ) {
                                        Text("剪贴板保留条数")
                                        Text(
                                            "${capacity.roundToInt()} 条",
                                            color = MaterialTheme.colorScheme.primary,
                                            fontWeight = FontWeight.SemiBold,
                                        )
                                    }
                                    Slider(
                                        capacity,
                                        { capacity = it },
                                        modifier = Modifier.semantics { contentDescription = "历史容量" },
                                        valueRange = 20f..200f,
                                    )
                                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                        FilledTonalButton(
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
                                            Text("授权麦克风")
                                        }
                                    }
                                }
                            }
                            1 -> {
                                SectionCard("网络模式", "纯离线模式下物理阻断局域网与外部网络请求") {
                                    Check("纯离线模式", offline) { offline = it }
                                }

                                SectionCard("设备隔空闪传", "局域网 LocalSend 直连优先，跨网阅后即焚") {
                                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                        FilledTonalButton(
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
                                                                if (devices.isEmpty())
                                                                    "已开启接收，尚未发现设备"
                                                                else "已发现 ${devices.size} 台设备"
                                                        }
                                                        .onFailure { notice = it.message.orEmpty() }
                                                }
                                            },
                                        ) {
                                            Text("发现设备")
                                        }
                                        OutlinedButton(
                                            enabled = !busy && !offline,
                                            onClick = ::readInbox,
                                        ) {
                                            Text("检查收件箱")
                                        }
                                    }
                                    for (device in devices) {
                                        OutlinedButton(
                                            modifier = Modifier.fillMaxWidth(),
                                            enabled = !busy && !offline,
                                            onClick = {
                                                destination = device.getString("device_id")
                                                transfer.launch(arrayOf("*/*"))
                                            },
                                        ) {
                                            Text("发送文件到 " + device.getString("device_name"))
                                        }
                                    }
                                    for (file in received) {
                                        TextButton(
                                            onClick = {
                                                exportItem = file
                                                export.launch(file.name)
                                            }
                                        ) {
                                            Text("保存已接收文件：" + file.name)
                                        }
                                    }
                                }

                                SectionCard("Koofr WebDAV 同步", T.Brand.cloudRoot) {
                                    Field(config, "koofr_endpoint", "WebDAV 端点")
                                    Field(config, "koofr_username", "账户")
                                    Field(config, "koofr_password", "应用密码", true)
                                    Field(config, "sync_key", "端到端同步密钥", true)
                                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                        FilledTonalButton(
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
                                                                    .optInt(
                                                                        "clipboard_ring_capacity",
                                                                        50,
                                                                    )
                                                                    .toFloat()
                                                            Core.save(
                                                                this@SettingsActivity,
                                                                config,
                                                            ) { saved ->
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
                                            Text("从云端恢复")
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
                                                                "词库 ${formatBytes(v.optLong("lexicon"))} · 剪贴板 ${formatBytes(v.optLong("clipboard"))} · 中转 ${formatBytes(v.optLong("relay"))}"
                                                            },
                                                            { it.message.orEmpty() },
                                                        )
                                                }
                                            },
                                        ) {
                                            Text("云盘用量")
                                        }
                                    }
                                }
                            }
                            2 -> {
                                SectionCard("AI 服务配置", "可选配置 Ollama 或 OpenAI 兼容接口") {
                                    Field(config, "ai_endpoint", "服务地址")
                                    Field(config, "ai_model", "模型")
                                    Field(config, "ai_api_key", "API 密钥", true)
                                    var tier3 by remember {
                                        mutableStateOf(config.optBoolean("ai_tier3_enabled"))
                                    }
                                    Check("AI 辅助连续手写消歧", tier3) {
                                        tier3 = it
                                        config.put("ai_tier3_enabled", it)
                                    }
                                }

                                SectionCard("本机离线录音与文本处理", "SenseVoice-Small 本地语音转写") {
                                    OutlinedTextField(
                                        source,
                                        { source = it },
                                        label = { Text("输入或听写原文") },
                                        minLines = 3,
                                        modifier = Modifier.fillMaxWidth(),
                                    )
                                    Row(
                                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                                        verticalAlignment = Alignment.CenterVertically,
                                    ) {
                                        FilledTonalButton(
                                            enabled = !busy || recording,
                                            onClick = {
                                                if (recording) {
                                                    recording = false
                                                    recorder.stop()
                                                } else if (
                                                    checkSelfPermission(
                                                        Manifest.permission.RECORD_AUDIO
                                                    ) != PackageManager.PERMISSION_GRANTED
                                                ) {
                                                    microphone.launch(
                                                        Manifest.permission.RECORD_AUDIO
                                                    )
                                                } else {
                                                    recording = true
                                                    busy = true
                                                    recorder.start(this@SettingsActivity) { result
                                                        ->
                                                        recording = false
                                                        busy = false
                                                        result
                                                            .onSuccess {
                                                                source = it
                                                                notice =
                                                                    if (it.isBlank()) "未识别到语音"
                                                                    else "本机转写完成"
                                                            }
                                                            .onFailure {
                                                                notice = it.message.orEmpty()
                                                            }
                                                    }
                                                }
                                            },
                                        ) {
                                            Text(if (recording) "停止并转写" else "开始录音")
                                        }
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
                                                                        (it as JSONObject)
                                                                            .optString("output_text")
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
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun SectionCard(
    title: String,
    subtitle: String,
    content: @Composable ColumnScope.() -> Unit,
) {
    OutlinedCard(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(16.dp),
    ) {
        Column(
            Modifier.padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Column {
                Text(title, style = MaterialTheme.typography.titleMedium, fontWeight = FontWeight.SemiBold)
                Text(
                    subtitle,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            content()
        }
    }
}

@Composable
private fun Check(label: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    Row(
        Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(label, modifier = Modifier.weight(1f).padding(end = 8.dp))
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
