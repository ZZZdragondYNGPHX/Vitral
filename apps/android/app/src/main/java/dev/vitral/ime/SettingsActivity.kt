package dev.vitral.ime

import android.Manifest
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Bundle
import android.provider.OpenableColumns
import android.provider.Settings
import android.view.inputmethod.InputMethodManager
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.vitral.ime.core.*
import dev.vitral.ime.theme.VitralTokens as T
import dev.vitral.ime.ui.*
import java.io.File
import java.util.Locale
import kotlin.math.roundToInt
import kotlinx.coroutines.delay
import org.json.JSONArray
import org.json.JSONObject

class SettingsActivity : ComponentActivity() {
    private var toastText by mutableStateOf("")
    private var toastSeq by mutableIntStateOf(0)
    private var requestedTab by mutableIntStateOf(0)
    private var requestedModelGroup by mutableStateOf<String?>(null)
    private var requestedModelCap by mutableStateOf<String?>(null)
    private var busy by mutableStateOf(false)
    private var imeEnabled by mutableStateOf(false)
    private var imeDefault by mutableStateOf(false)
    private var storageUsage by mutableStateOf<Triple<Long, Long, Long>?>(null)
    private var aiOutput by mutableStateOf("")
    private var clipboardState by mutableStateOf<JSONObject?>(null)

    private fun summarizeNotice(raw: String): String {
        val compact = raw.replace(Regex("\\s+"), " ").trim()
        if (compact.isEmpty()) return ""
        val lower = compact.lowercase(Locale.US)
        if ("401" in lower && ("webdav" in lower || "propfind" in lower || "authorization" in lower)) {
            return "WebDAV 认证失败 (401)：请检查账户、密码或端点路径"
        }
        if ("403" in lower && ("webdav" in lower || "propfind" in lower)) {
            return "WebDAV 权限被拒 (403)：请检查目录权限"
        }
        if ("404" in lower && ("webdav" in lower || "propfind" in lower)) {
            return "WebDAV 路径不存在 (404)：请检查端点路径"
        }
        val stripped = compact.substringBefore("<html").substringBefore("<!DOCTYPE").trim()
        val chosen = stripped.ifEmpty { compact }
        return if (chosen.length > 140) chosen.take(137) + "…" else chosen
    }

    private fun showToast(msg: String) {
        val clean = summarizeNotice(msg)
        if (clean.isBlank()) return
        toastText = clean
        toastSeq++
    }

    private val microphone =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) {
            showToast(if (it) "麦克风已授权，长按空格即可语音输入" else "语音输入需要麦克风权限")
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
                showToast(
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
                            showToast("文件已保存")
                        }
                        .onFailure { showToast(it.message.orEmpty()) }
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
                showToast(result.fold({ "文件已发送" }, { it.message.orEmpty() }))
            }
        }

    private fun refreshImeStatus() {
        val imm = getSystemService(InputMethodManager::class.java)
        imeEnabled =
            runCatching {
                imm.enabledInputMethodList.any { it.packageName == packageName }
            }.getOrDefault(false)
        imeDefault =
            runCatching {
                val active =
                    Settings.Secure.getString(contentResolver, Settings.Secure.DEFAULT_INPUT_METHOD)
                active?.startsWith("$packageName/") == true
            }.getOrDefault(false)
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
            showToast(result.fold({ "已接收 ${it} 项" }, { it.message.orEmpty() }))
        }
    }

    private fun refreshInbox() {
        received.clear()
        received.addAll(inboxDir.walkTopDown().filter { it.isFile }.toList())
    }

    override fun onResume() {
        super.onResume()
        refreshImeStatus()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) refreshImeStatus()
    }

    override fun onDestroy() {
        recorder.cancel()
        if (devices.isNotEmpty() || discoveryStarted)
            Core.call(this, "relay-stop-discovery", background = true)
        super.onDestroy()
    }

    private var discoveryStarted = false

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        if (intent.hasExtra("tab")) {
            requestedTab = intent.getIntExtra("tab", 0).coerceIn(0, 3)
        }
        if (intent.hasExtra("model_group")) {
            requestedModelGroup = intent.getStringExtra("model_group")
            requestedModelCap = intent.getStringExtra("model_cap")
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        requestedTab = intent.getIntExtra("tab", 0).coerceIn(0, 3)
        requestedModelGroup = intent.getStringExtra("model_group")
        requestedModelCap = intent.getStringExtra("model_cap")
        refreshInbox()
        refreshImeStatus()

        setContent {
            val config = remember { Core.settings(this@SettingsActivity) }
            var tab by remember { mutableIntStateOf(requestedTab) }
            var modelGroup by remember { mutableStateOf(requestedModelGroup) }
            var modelCap by remember { mutableStateOf(requestedModelCap) }
            LaunchedEffect(requestedTab, requestedModelGroup, requestedModelCap) {
                tab = requestedTab
                if (requestedModelGroup != null) {
                    modelGroup = requestedModelGroup
                    modelCap = requestedModelCap
                }
            }
            var themeMode by remember { mutableStateOf(config.optString("theme", "system")) }
            val systemDark = isSystemInDarkTheme()
            val dark =
                when (themeMode) {
                    "dark" -> true
                    "light" -> false
                    else -> systemDark
                }
            val palette = remember(dark) { glassPalette(dark) }

            LaunchedEffect(dark) {
                enableEdgeToEdge(
                    statusBarStyle =
                        if (dark) SystemBarStyle.dark(android.graphics.Color.TRANSPARENT)
                        else SystemBarStyle.light(android.graphics.Color.TRANSPARENT, android.graphics.Color.TRANSPARENT),
                    navigationBarStyle =
                        if (dark) SystemBarStyle.dark(android.graphics.Color.TRANSPARENT)
                        else SystemBarStyle.light(android.graphics.Color.TRANSPARENT, android.graphics.Color.TRANSPARENT),
                )
            }

            // Input & Feel State
            var flypy by remember {
                mutableStateOf(
                    config.optString("schema", "double_pinyin_flypy") == "double_pinyin_flypy"
                )
            }
            var fuzzyPinyin by remember { mutableStateOf(config.optBoolean("fuzzy_pinyin", false)) }
            var openccMode by remember { mutableStateOf(config.optString("opencc_mode", "simplified")) }
            var sublabelMode by remember { mutableStateOf(config.optString("mobile_sublabel_mode", "dual")) }
            var keyboardHeight by remember { mutableStateOf(config.optString("mobile_keyboard_height", "standard")) }
            var size by remember {
                mutableFloatStateOf(
                    config.optInt("candidate_font_px", T.Typography.candidateFontSizeDefaultPx).toFloat()
                )
            }
            var activePreviewIdx by remember { mutableIntStateOf(0) }
            var sandboxVisible by remember { mutableStateOf(false) }
            var sandboxText by remember { mutableStateOf("") }

            // Personalization & Sync State
            var adaptive by remember { mutableStateOf(config.optBoolean("adaptive_learning", true)) }
            var clipLearn by remember { mutableStateOf(config.optBoolean("learn_from_clipboard", true)) }
            var predictionEnabled by remember { mutableStateOf(config.optBoolean("prediction_enabled", true)) }
            var profileDistillEnabled by remember { mutableStateOf(config.optBoolean("profile_distill_enabled", true)) }
            var profileState by remember { mutableStateOf<JSONObject?>(null) }
            var offline by remember { mutableStateOf(config.optBoolean("pure_offline")) }
            var capacity by remember { mutableFloatStateOf(config.optInt("clipboard_ring_capacity", 50).toFloat()) }

            // Models & AI State
            var modelsState by remember { mutableStateOf<JSONObject?>(null) }
            var confirmDeleteModelId by remember { mutableStateOf<String?>(null) }
            var aiPreferLocal by remember { mutableStateOf(config.optBoolean("ai_prefer_local", true)) }
            var aiTier3 by remember { mutableStateOf(config.optBoolean("ai_tier3_enabled")) }
            var targetLang by remember { mutableStateOf("en") }
            var source by remember { mutableStateOf("") }
            var textFieldsDirty by remember { mutableStateOf(false) }

            fun persistConfig(feedback: String? = null, onSaved: (() -> Unit)? = null) {
                for (trimKey in listOf("koofr_endpoint", "koofr_username", "ai_endpoint", "ai_model")) {
                    if (config.has(trimKey) && !config.isNull(trimKey)) {
                        config.put(trimKey, config.optString(trimKey).trim())
                    }
                }
                if (config.has("koofr_password") && !config.isNull("koofr_password")) {
                    config.put(
                        "koofr_password",
                        config.optString("koofr_password").trim('\r', '\n'),
                    )
                }
                config
                    .put("schema", if (flypy) "double_pinyin_flypy" else "rime_ice")
                    .put("fuzzy_pinyin", fuzzyPinyin)
                    .put("opencc_mode", openccMode)
                    .put("theme", themeMode)
                    .put("mobile_sublabel_mode", sublabelMode)
                    .put("mobile_keyboard_height", keyboardHeight)
                    .put("pure_offline", offline)
                    .put("adaptive_learning", adaptive)
                    .put("learn_from_clipboard", clipLearn)
                    .put("prediction_enabled", predictionEnabled)
                    .put("profile_distill_enabled", profileDistillEnabled)
                    .put("ai_prefer_local", aiPreferLocal)
                    .put("ai_tier3_enabled", aiTier3)
                    .put("candidate_font_px", size.roundToInt())
                    .put("clipboard_ring_capacity", capacity.roundToInt())
                Core.save(this@SettingsActivity, config) { r ->
                    r.onSuccess {
                        textFieldsDirty = false
                        if (!feedback.isNullOrBlank()) showToast(feedback)
                        onSaved?.invoke()
                    }.onFailure {
                        busy = false
                        showToast(it.message.orEmpty())
                    }
                }
            }

            fun refreshModels(silent: Boolean = true) {
                Core.call(this@SettingsActivity, "models-state", background = true) { r ->
                    r.onSuccess { value ->
                        modelsState = value as? JSONObject
                        if (!silent) showToast("已刷新模型状态")
                    }.onFailure {
                        if (!silent) showToast(it.message.orEmpty())
                    }
                }
            }

            fun refreshClipboard() {
                Core.call(this@SettingsActivity, "clipboard-state", background = true) { r ->
                    r.onSuccess { clipboardState = it as? JSONObject }
                }
            }

            LaunchedEffect(toastSeq) {
                if (toastSeq > 0) {
                    delay(if (toastText.length > 24) 3600 else 2400)
                    toastText = ""
                }
            }

            LaunchedEffect(tab) {
                when (tab) {
                    0 -> Core.call(this@SettingsActivity, "profile-state", background = true) { r ->
                        r.onSuccess { profileState = it as? JSONObject }
                    }
                    1 -> refreshClipboard()
                    2 -> refreshModels(true)
                }
            }

            LaunchedEffect(tab, modelsState) {
                if (tab != 2) return@LaunchedEffect
                val arr = modelsState?.optJSONArray("models") ?: return@LaunchedEffect
                var downloading = false
                for (i in 0 until arr.length()) {
                    if (arr.optJSONObject(i)?.optJSONObject("download")?.optString("status") == "downloading") {
                        downloading = true
                        break
                    }
                }
                if (downloading) {
                    delay(700)
                    refreshModels(true)
                }
            }

            val m3Scheme =
                if (dark) darkColorScheme(primary = palette.accent, background = palette.canvas, surface = palette.panelTop)
                else lightColorScheme(primary = palette.accent, background = palette.canvas, surface = palette.panelTop)

            CompositionLocalProvider(LocalGlass provides palette) {
                MaterialTheme(colorScheme = m3Scheme) {
                    Box(Modifier.fillMaxSize()) {
                        LiquidGlassBackdrop()

                        Column(
                            Modifier.fillMaxSize()
                                .statusBarsPadding()
                                .verticalScroll(rememberScrollState())
                                .padding(horizontal = 16.dp)
                                .padding(top = 12.dp, bottom = 112.dp),
                            verticalArrangement = Arrangement.spacedBy(14.dp),
                        ) {
                            // Top Brand Bar — sleek, airy, uncluttered
                            Row(
                                Modifier.fillMaxWidth().padding(horizontal = 2.dp, vertical = 2.dp),
                                horizontalArrangement = Arrangement.SpaceBetween,
                                verticalAlignment = Alignment.CenterVertically,
                            ) {
                                Row(
                                    verticalAlignment = Alignment.CenterVertically,
                                    horizontalArrangement = Arrangement.spacedBy(11.dp),
                                ) {
                                    Image(
                                        painter = painterResource(id = R.drawable.vitral),
                                        contentDescription = "Vitral",
                                        modifier =
                                            Modifier.size(38.dp)
                                                .clip(RoundedCornerShape(12.dp))
                                                .border(1.dp, palette.borderTop, RoundedCornerShape(12.dp)),
                                    )
                                    Column {
                                        Text(
                                            "Vitral",
                                            fontSize = 20.sp,
                                            fontWeight = FontWeight.Bold,
                                            letterSpacing = (-0.5).sp,
                                            color = palette.textPrimary,
                                        )
                                        Text(
                                            when (tab) {
                                                0 -> "键盘与液态晶透外观"
                                                1 -> "词库 · 剪贴板 · 私有云同步"
                                                2 -> "本机离线模型中心"
                                                else -> "离线语音 · AI 工作台 · 快捷键"
                                            },
                                            fontSize = 11.5.sp,
                                            color = palette.textSecondary,
                                        )
                                    }
                                }
                                Row(
                                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                                    verticalAlignment = Alignment.CenterVertically,
                                ) {
                                    if (offline) {
                                        GlassPillBadge("纯离线", palette.warning, palette.warningSoft)
                                    }
                                    if (imeDefault) {
                                        GlassPillBadge(
                                            if (sandboxVisible) "收起试打" else "试打",
                                            palette.accentText,
                                            palette.accentSoft,
                                            Modifier.clickable { sandboxVisible = !sandboxVisible },
                                        )
                                    }
                                }
                            }

                            // Smart Activation Banner: prominent 2-step guide ONLY until active;
                            // once active, stays out of the way unless user opens Sandbox!
                            if (!imeEnabled || !imeDefault) {
                                GlassPanel(tinted = true, padding = PaddingValues(16.dp)) {
                                    Row(
                                        Modifier.fillMaxWidth(),
                                        horizontalArrangement = Arrangement.SpaceBetween,
                                        verticalAlignment = Alignment.CenterVertically,
                                    ) {
                                        Column(Modifier.weight(1f).padding(end = 10.dp)) {
                                            Text(
                                                if (!imeEnabled) "启用 Vitral 键盘" else "切换为默认键盘",
                                                fontSize = 15.sp,
                                                fontWeight = FontWeight.SemiBold,
                                                color = palette.textPrimary,
                                            )
                                            Spacer(Modifier.height(2.dp))
                                            Text(
                                                if (!imeEnabled) "第 1 步：在系统键盘列表中打开 Vitral 开关"
                                                else "第 2 步：已开启，点击下方按钮即可设为默认",
                                                fontSize = 12.sp,
                                                color = palette.textSecondary,
                                            )
                                        }
                                        GlassPillBadge(
                                            if (!imeEnabled) "1 / 2" else "2 / 2",
                                            if (!imeEnabled) palette.warning else palette.accentText,
                                            if (!imeEnabled) palette.warningSoft else palette.accentSoft,
                                        )
                                    }
                                    Spacer(Modifier.height(12.dp))
                                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                        if (!imeEnabled) {
                                            GlassButton(
                                                text = "前往系统开启",
                                                style = GlassButtonStyle.Primary,
                                                modifier = Modifier.weight(1f),
                                                onClick = {
                                                    startActivity(Intent(Settings.ACTION_INPUT_METHOD_SETTINGS))
                                                },
                                            )
                                            GlassButton(
                                                text = "切换键盘",
                                                style = GlassButtonStyle.Ghost,
                                                onClick = {
                                                    refreshImeStatus()
                                                    if (!imeEnabled) {
                                                        showToast("请先开启 Vitral 开关")
                                                        startActivity(Intent(Settings.ACTION_INPUT_METHOD_SETTINGS))
                                                    } else {
                                                        getSystemService(InputMethodManager::class.java)
                                                            .showInputMethodPicker()
                                                    }
                                                },
                                            )
                                        } else {
                                            GlassButton(
                                                text = "立即切换为 Vitral",
                                                style = GlassButtonStyle.Primary,
                                                modifier = Modifier.weight(1f),
                                                onClick = {
                                                    getSystemService(InputMethodManager::class.java)
                                                        .showInputMethodPicker()
                                                },
                                            )
                                            GlassButton(
                                                text = "系统管理",
                                                style = GlassButtonStyle.Ghost,
                                                onClick = {
                                                    startActivity(Intent(Settings.ACTION_INPUT_METHOD_SETTINGS))
                                                },
                                            )
                                        }
                                    }
                                }
                            }

                            AnimatedVisibility(visible = sandboxVisible) {
                                GlassPanel(tinted = true, padding = PaddingValues(14.dp)) {
                                    GlassTextField(
                                        value = sandboxText,
                                        onValueChange = { sandboxText = it },
                                        label = "实时试打区",
                                        placeholder = "点此唤起 Vitral 键盘，立即体验双拼、手写与候选字号…",
                                        singleLine = false,
                                        minLines = 2,
                                    )
                                }
                            }

                            when (tab) {
                                0 -> {
                                    // ==================== TAB 0: KEYBOARD & APPEARANCE ====================
                                    // Desktop-grade Liquid Glass Live Preview Stage
                                    GlassPanel(tinted = true, padding = PaddingValues(16.dp)) {
                                        Row(
                                            Modifier.fillMaxWidth(),
                                            horizontalArrangement = Arrangement.SpaceBetween,
                                            verticalAlignment = Alignment.CenterVertically,
                                        ) {
                                            Text(
                                                "候选面板实时预览",
                                                fontSize = 12.5.sp,
                                                fontWeight = FontWeight.SemiBold,
                                                color = palette.accentText,
                                            )
                                            Text(
                                                "${size.roundToInt()} px · 点击体验高亮",
                                                fontSize = 11.5.sp,
                                                color = palette.textSecondary,
                                            )
                                        }
                                        Spacer(Modifier.height(14.dp))
                                        Column(
                                            Modifier.fillMaxWidth(),
                                            horizontalAlignment = Alignment.CenterHorizontally,
                                            verticalArrangement = Arrangement.spacedBy(8.dp),
                                        ) {
                                            // Preedit monospace pill
                                            Box(
                                                Modifier.clip(RoundedCornerShape(8.dp))
                                                    .background(palette.elevated)
                                                    .border(1.dp, palette.hairline, RoundedCornerShape(8.dp))
                                                    .padding(horizontal = 10.dp, vertical = 3.dp)
                                            ) {
                                                Text(
                                                    if (flypy) "jr" else "jin'ri",
                                                    fontFamily = FontFamily.Monospace,
                                                    fontSize = 12.5.sp,
                                                    color = palette.textPrimary,
                                                )
                                            }
                                            // Floating Liquid Glass Candidate Capsule
                                            Row(
                                                Modifier.shadow(if (dark) 12.dp else 8.dp, RoundedCornerShape(16.dp))
                                                    .clip(RoundedCornerShape(16.dp))
                                                    .background(if (dark) T.Dark.surfacePopover else T.Light.surfacePopover)
                                                    .border(
                                                        1.dp,
                                                        Brush.verticalGradient(listOf(palette.borderTop, palette.borderBottom)),
                                                        RoundedCornerShape(16.dp),
                                                    )
                                                    .padding(horizontal = 8.dp, vertical = 6.dp),
                                                horizontalArrangement = Arrangement.spacedBy(4.dp),
                                                verticalAlignment = Alignment.CenterVertically,
                                            ) {
                                                listOf("今日", "近日", "尽日", "金日").forEachIndexed { idx, w ->
                                                    val active = idx == activePreviewIdx
                                                    Box(
                                                        Modifier.clip(RoundedCornerShape(10.dp))
                                                            .background(if (active) palette.accentSoft else Color.Transparent)
                                                            .border(
                                                                1.dp,
                                                                if (active) palette.accentBorder else Color.Transparent,
                                                                RoundedCornerShape(10.dp),
                                                            )
                                                            .clickable { activePreviewIdx = idx }
                                                            .padding(horizontal = 10.dp, vertical = 4.dp)
                                                    ) {
                                                        Text(
                                                            w,
                                                            fontSize = size.sp,
                                                            fontWeight = if (active) FontWeight.SemiBold else FontWeight.Normal,
                                                            color = if (active) palette.accentText else palette.textPrimary,
                                                        )
                                                    }
                                                }
                                            }
                                        }
                                        Spacer(Modifier.height(14.dp))
                                        Row(
                                            Modifier.fillMaxWidth(),
                                            horizontalArrangement = Arrangement.SpaceBetween,
                                            verticalAlignment = Alignment.CenterVertically,
                                        ) {
                                            Text("候选字号", fontSize = 13.sp, fontWeight = FontWeight.Medium, color = palette.textPrimary)
                                            GlassPresetChips(
                                                presets = listOf(14, 16, 18),
                                                current = size.roundToInt(),
                                                onSelect = {
                                                    size = it.toFloat()
                                                    persistConfig("字号已调整为 ${it} px")
                                                },
                                            )
                                        }
                                        Spacer(Modifier.height(4.dp))
                                        GlassSlider(
                                            value = size,
                                            valueRange =
                                                T.Typography.candidateFontSizeMinPx.toFloat()..T
                                                    .Typography
                                                    .candidateFontSizeMaxPx
                                                    .toFloat(),
                                            onValueChange = { size = it },
                                            onValueChangeFinished = { persistConfig("字号已保存") },
                                            contentDesc = "候选字号",
                                        )
                                    }

                                    // Typing & Ergonomics Card
                                    GlassPanel {
                                        GlassSectionHeader("输入方案与键位手感", "修改后即时生效，无需手动点击保存")

                                        Text("拼音方案", fontSize = 13.sp, fontWeight = FontWeight.Medium, color = palette.textPrimary)
                                        Spacer(Modifier.height(6.dp))
                                        GlassSegmentedControl(
                                            options = listOf(true to "小鹤双拼（4排）", false to "雾凇全拼（5排）"),
                                            selected = flypy,
                                            onSelect = {
                                                flypy = it
                                                persistConfig(if (it) "已切换小鹤双拼" else "已切换雾凇全拼")
                                            },
                                        )

                                        Spacer(Modifier.height(10.dp))
                                        GlassToggleRow(
                                            "模糊音容错",
                                            "平翘舌（z/zh、c/ch、s/sh）、前后鼻音（an/ang、en/eng、in/ing）及 l/n、f/h 互通",
                                            checked = fuzzyPinyin,
                                            showDivider = false,
                                            onCheckedChange = {
                                                fuzzyPinyin = it
                                                persistConfig(if (it) "已开启模糊音容错" else "已关闭模糊音容错")
                                            },
                                        )

                                        Spacer(Modifier.height(14.dp))
                                        Text("键帽副标风格", fontSize = 13.sp, fontWeight = FontWeight.Medium, color = palette.textPrimary)
                                        Spacer(Modifier.height(6.dp))
                                        GlassSegmentedControl(
                                            options = listOf("dual" to "对角双副标", "symbols" to "仅上滑符号", "clean" to "纯净无副标"),
                                            selected = sublabelMode,
                                            onSelect = {
                                                sublabelMode = it
                                                persistConfig("键帽样式已更新")
                                            },
                                        )

                                        Spacer(Modifier.height(14.dp))
                                        Text("键盘停靠高度", fontSize = 13.sp, fontWeight = FontWeight.Medium, color = palette.textPrimary)
                                        Spacer(Modifier.height(6.dp))
                                        GlassSegmentedControl(
                                            options = listOf("compact" to "紧凑", "standard" to "标准", "comfort" to "舒适"),
                                            selected = keyboardHeight,
                                            onSelect = {
                                                keyboardHeight = it
                                                persistConfig("键盘高度已更新")
                                            },
                                        )

                                        Spacer(Modifier.height(14.dp))
                                        Text("晶透主题", fontSize = 13.sp, fontWeight = FontWeight.Medium, color = palette.textPrimary)
                                        Spacer(Modifier.height(6.dp))
                                        GlassSegmentedControl(
                                            options = listOf("system" to "跟随系统", "dark" to "深色晶透", "light" to "浅色冰晶"),
                                            selected = themeMode,
                                            onSelect = {
                                                themeMode = it
                                                persistConfig("主题已切换")
                                            },
                                        )

                                        Spacer(Modifier.height(14.dp))
                                        Text("输出字形（OpenCC）", fontSize = 13.sp, fontWeight = FontWeight.Medium, color = palette.textPrimary)
                                        Spacer(Modifier.height(6.dp))
                                        GlassSegmentedControl(
                                            options = listOf(
                                                "simplified" to "简体",
                                                "traditional" to "繁體",
                                                "taiwan_standard" to "臺灣正體",
                                                "hong_kong" to "香港",
                                            ),
                                            selected = openccMode,
                                            onSelect = {
                                                openccMode = it
                                                persistConfig("输出字形已更新")
                                            },
                                        )

                                        Spacer(Modifier.height(14.dp))
                                        GlassHairline()
                                        Spacer(Modifier.height(12.dp))
                                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                            GlassButton(
                                                text = "切换系统键盘",
                                                style = GlassButtonStyle.AccentSoft,
                                                modifier = Modifier.weight(1f),
                                                onClick = {
                                                    getSystemService(InputMethodManager::class.java)
                                                        .showInputMethodPicker()
                                                },
                                            )
                                            GlassButton(
                                                text = "授权麦克风",
                                                style = GlassButtonStyle.Ghost,
                                                modifier = Modifier.weight(1f),
                                                onClick = {
                                                    microphone.launch(Manifest.permission.RECORD_AUDIO)
                                                },
                                            )
                                        }
                                    }

                                    // Personalization Engine Card
                                    GlassPanel {
                                        GlassSectionHeader(
                                            "懂你引擎 · 越用越顺手",
                                            "纯本地实时学习与语境预测，随时可清空",
                                        )
                                        GlassToggleRow(
                                            "个人专属排序与上下文学习",
                                            "按应用隔离词频、近期高频优先与错词纠偏",
                                            checked = adaptive,
                                            showDivider = false,
                                            onCheckedChange = {
                                                adaptive = it
                                                persistConfig()
                                            },
                                        )
                                        GlassToggleRow(
                                            "剪贴板静默识词",
                                            "本地提取专名与术语，自动跳过密码与网址",
                                            checked = clipLearn,
                                            onCheckedChange = {
                                                clipLearn = it
                                                persistConfig()
                                            },
                                        )
                                        GlassToggleRow(
                                            "本地语境预测输入",
                                            "空闲候选栏给出下一句建议，点击即上屏",
                                            checked = predictionEnabled,
                                            onCheckedChange = {
                                                predictionEnabled = it
                                                persistConfig()
                                            },
                                        )
                                        GlassToggleRow(
                                            "本地用户画像蒸馏",
                                            "脱敏蒸馏个人常用表达风格，仅存本地",
                                            checked = profileDistillEnabled,
                                            onCheckedChange = {
                                                profileDistillEnabled = it
                                                persistConfig()
                                            },
                                        )

                                        val prof = profileState?.optJSONObject("profile")
                                        val summary = prof?.optString("summary").orEmpty()
                                        val accepted = prof?.optLong("accepted_count", 0L) ?: 0L
                                        val dismissed = prof?.optLong("dismissed_count", 0L) ?: 0L
                                        Box(
                                            Modifier.fillMaxWidth()
                                                .clip(RoundedCornerShape(12.dp))
                                                .background(palette.elevated)
                                                .border(1.dp, palette.hairline, RoundedCornerShape(12.dp))
                                                .padding(12.dp)
                                        ) {
                                            Text(
                                                if (summary.isNotBlank()) {
                                                    "画像摘要：$summary（采纳 $accepted · 忽略 $dismissed）"
                                                } else {
                                                    "尚未生成画像摘要（采纳 $accepted · 忽略 $dismissed）"
                                                },
                                                fontSize = 12.sp,
                                                lineHeight = 17.sp,
                                                color = palette.textSecondary,
                                            )
                                        }
                                        Spacer(Modifier.height(10.dp))
                                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                            GlassButton(
                                                text = "立即蒸馏画像",
                                                style = GlassButtonStyle.AccentSoft,
                                                enabled = !busy,
                                                modifier = Modifier.weight(1f),
                                                onClick = {
                                                    busy = true
                                                    Core.call(
                                                        this@SettingsActivity,
                                                        "profile-distill",
                                                        background = true,
                                                    ) { r ->
                                                        busy = false
                                                        r.onSuccess {
                                                            profileState = it as? JSONObject
                                                            showToast("已完成本地画像蒸馏")
                                                        }.onFailure { showToast(it.message.orEmpty()) }
                                                    }
                                                },
                                            )
                                            GlassButton(
                                                text = "清空习惯",
                                                style = GlassButtonStyle.DangerGhost,
                                                enabled = !busy,
                                                onClick = {
                                                    busy = true
                                                    Core.call(
                                                        this@SettingsActivity,
                                                        "personalization-clear",
                                                        background = true,
                                                    ) { r ->
                                                        busy = false
                                                        r.onSuccess {
                                                            profileState = null
                                                            showToast("已清空个人习惯与画像")
                                                        }.onFailure { showToast(it.message.orEmpty()) }
                                                    }
                                                },
                                            )
                                        }
                                    }
                                }

                                1 -> {
                                    // ==================== TAB 1: LEXICON, CLIPBOARD & SYNC ====================
                                    // Lexicon Drop Zone
                                    GlassPanel {
                                        GlassSectionHeader(
                                            "多格式词库直导",
                                            "导入词条同时参与拼音解码与连续手写重排",
                                        )
                                        Box(
                                            Modifier.fillMaxWidth()
                                                .clip(RoundedCornerShape(14.dp))
                                                .background(palette.accentSoft)
                                                .border(1.dp, palette.accentBorder, RoundedCornerShape(14.dp))
                                                .clickable(enabled = !busy) { choose.launch(arrayOf("*/*")) }
                                                .padding(vertical = 18.dp, horizontal = 14.dp),
                                            contentAlignment = Alignment.Center,
                                        ) {
                                            Column(
                                                horizontalAlignment = Alignment.CenterHorizontally,
                                                verticalArrangement = Arrangement.spacedBy(5.dp),
                                            ) {
                                                Text(
                                                    "+ 点击选择词库文件导入",
                                                    fontSize = 14.sp,
                                                    fontWeight = FontWeight.SemiBold,
                                                    color = palette.accentText,
                                                )
                                                Text(
                                                    "支持搜狗 .scel · Rime .dict.yaml · .tsv · .txt 自动注音",
                                                    fontSize = 11.5.sp,
                                                    color = palette.textSecondary,
                                                )
                                            }
                                        }
                                    }

                                    // Clipboard Ring & Retention
                                    GlassPanel {
                                        GlassSectionHeader(
                                            "剪贴板历史与容量",
                                            "置顶收藏不受环形上限淘汰，可绑定简码直出",
                                            trailing = {
                                                GlassPillBadge("${capacity.roundToInt()} 条", palette.accentText, palette.accentSoft)
                                            },
                                        )
                                        GlassSlider(
                                            value = capacity,
                                            valueRange = 20f..200f,
                                            onValueChange = { capacity = it },
                                            onValueChangeFinished = { persistConfig("剪贴板上限已更新") },
                                            contentDesc = "历史容量",
                                        )
                                        val ring = clipboardState?.optJSONArray("ring")
                                        if (ring != null && ring.length() > 0) {
                                            Spacer(Modifier.height(6.dp))
                                            val previewCount = ring.length().coerceAtMost(4)
                                            Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                                                for (i in 0 until previewCount) {
                                                    val item = ring.getJSONObject(i)
                                                    val pinned = item.optBoolean("pinned")
                                                    Row(
                                                        Modifier.fillMaxWidth()
                                                            .clip(RoundedCornerShape(10.dp))
                                                            .background(palette.elevated)
                                                            .border(1.dp, palette.hairline, RoundedCornerShape(10.dp))
                                                            .padding(horizontal = 11.dp, vertical = 8.dp),
                                                        horizontalArrangement = Arrangement.SpaceBetween,
                                                        verticalAlignment = Alignment.CenterVertically,
                                                    ) {
                                                        Text(
                                                            item.optString("content").replace('\n', ' '),
                                                            fontSize = 12.5.sp,
                                                            maxLines = 1,
                                                            overflow = TextOverflow.Ellipsis,
                                                            color = palette.textPrimary,
                                                            modifier = Modifier.weight(1f).padding(end = 8.dp),
                                                        )
                                                        if (pinned) {
                                                            GlassPillBadge("置顶", palette.accentText, palette.accentSoft)
                                                        }
                                                    }
                                                }
                                            }
                                            Spacer(Modifier.height(10.dp))
                                            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                                GlassButton("刷新", style = GlassButtonStyle.Ghost, compact = true, onClick = { refreshClipboard() })
                                                GlassButton(
                                                    "清空未置顶",
                                                    style = GlassButtonStyle.DangerGhost,
                                                    compact = true,
                                                    onClick = {
                                                        Core.call(this@SettingsActivity, "clipboard-clear-unpinned", background = true) { r ->
                                                            r.onSuccess {
                                                                clipboardState = it as? JSONObject
                                                                showToast("已清空未置顶记录")
                                                            }
                                                        }
                                                    },
                                                )
                                            }
                                        }
                                    }

                                    // Offline Mode Hero Toggle
                                    GlassPanel(tinted = offline) {
                                        GlassToggleRow(
                                            "纯离线模式",
                                            "物理阻断一切局域网与外部网络请求，本地输入与离线模型依然完整可用",
                                            checked = offline,
                                            showDivider = false,
                                            onCheckedChange = {
                                                offline = it
                                                persistConfig(if (it) "已进入纯离线模式" else "已恢复网络协同")
                                            },
                                        )
                                    }

                                    // Koofr WebDAV Sync
                                    GlassPanel {
                                        GlassSectionHeader(
                                            "Koofr WebDAV 私有云同步",
                                            "端到端加密同步词频、词库与置顶剪贴板",
                                            trailing = {
                                                GlassPillBadge(T.Brand.cloudRoot, palette.textSecondary, palette.elevated)
                                            },
                                        )
                                        Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                                            ConfigField(config, "koofr_endpoint", "WebDAV 端点", "https://app.koofr.net/dav/Koofr") {
                                                textFieldsDirty = true
                                            }
                                            ConfigField(config, "koofr_username", "账户邮箱", "name@example.com") {
                                                textFieldsDirty = true
                                            }
                                            ConfigField(config, "koofr_password", "独立应用密码", "留空保持不变", secret = true) {
                                                textFieldsDirty = true
                                            }
                                            ConfigField(config, "sync_key", "端到端同步密钥", "跨设备一致即可还原", secret = true) {
                                                textFieldsDirty = true
                                            }
                                        }
                                        storageUsage?.let { (lex, clip, relay) ->
                                            Spacer(Modifier.height(12.dp))
                                            Row(
                                                Modifier.fillMaxWidth(),
                                                horizontalArrangement = Arrangement.spacedBy(8.dp),
                                            ) {
                                                UsageTile("词库", formatBytes(lex), Modifier.weight(1f))
                                                UsageTile("剪贴板", formatBytes(clip), Modifier.weight(1f))
                                                UsageTile("中转区", formatBytes(relay), Modifier.weight(1f))
                                            }
                                        }
                                        Spacer(Modifier.height(12.dp))
                                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                            GlassButton(
                                                "立即同步",
                                                style = GlassButtonStyle.AccentSoft,
                                                enabled = !busy && !offline,
                                                modifier = Modifier.weight(1f),
                                                onClick = {
                                                    busy = true
                                                    persistConfig {
                                                        Core.call(this@SettingsActivity, "poll-services", background = true) { r ->
                                                            busy = false
                                                            showToast(r.fold({ "已完成 Koofr 同步" }, { it.message.orEmpty() }))
                                                        }
                                                    }
                                                },
                                            )
                                            GlassButton(
                                                "云端恢复",
                                                style = GlassButtonStyle.Ghost,
                                                enabled = !busy && !offline,
                                                onClick = {
                                                    busy = true
                                                    persistConfig {
                                                        Core.call(
                                                            this@SettingsActivity,
                                                            "restore-from-koofr",
                                                            json("config" to config),
                                                            true,
                                                        ) { r ->
                                                            r.onSuccess { value ->
                                                                val result = value as JSONObject
                                                                val restored = result.getJSONObject("preferences")
                                                                for (key in restored.keys()) config.put(key, restored.get(key))
                                                                flypy = config.optString("schema") == "double_pinyin_flypy"
                                                                capacity = config.optInt("clipboard_ring_capacity", 50).toFloat()
                                                                Core.save(this@SettingsActivity, config) { saved ->
                                                                    busy = false
                                                                    showToast(
                                                                        saved.fold(
                                                                            { "已恢复 ${result.optInt("snapshots_applied")} 项" },
                                                                            { it.message.orEmpty() },
                                                                        )
                                                                    )
                                                                }
                                                            }.onFailure {
                                                                busy = false
                                                                showToast(it.message.orEmpty())
                                                            }
                                                        }
                                                    }
                                                },
                                            )
                                            GlassButton(
                                                "用量",
                                                style = GlassButtonStyle.Ghost,
                                                enabled = !busy && !offline,
                                                onClick = {
                                                    busy = true
                                                    persistConfig {
                                                        Core.call(this@SettingsActivity, "storage", background = true) { r ->
                                                            busy = false
                                                            r.onSuccess {
                                                                val v = it as JSONObject
                                                                storageUsage = Triple(v.optLong("lexicon"), v.optLong("clipboard"), v.optLong("relay"))
                                                                showToast("已刷新云端用量")
                                                            }.onFailure { showToast(it.message.orEmpty()) }
                                                        }
                                                    }
                                                },
                                            )
                                        }
                                    }

                                    // Air Transfer
                                    GlassPanel {
                                        GlassSectionHeader(
                                            "设备隔空闪传",
                                            "局域网 LocalSend 直连优先，跨网校验 SHA-256 后即焚",
                                        )
                                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                            GlassButton(
                                                "发现附近设备",
                                                style = GlassButtonStyle.AccentSoft,
                                                enabled = !busy && !offline,
                                                modifier = Modifier.weight(1f),
                                                onClick = {
                                                    busy = true
                                                    discoveryStarted = true
                                                    Core.call(this@SettingsActivity, "relay-discover", background = true) { result ->
                                                        busy = false
                                                        result.onSuccess { value ->
                                                            val entries = value as JSONArray
                                                            devices = List(entries.length()) { entries.getJSONObject(it) }
                                                            showToast(
                                                                if (devices.isEmpty()) "已开启接收，暂未发现设备"
                                                                else "已发现 ${devices.size} 台设备"
                                                            )
                                                        }.onFailure { showToast(it.message.orEmpty()) }
                                                    }
                                                },
                                            )
                                            GlassButton(
                                                "收件箱",
                                                style = GlassButtonStyle.Ghost,
                                                enabled = !busy && !offline,
                                                onClick = ::readInbox,
                                            )
                                        }
                                        if (devices.isNotEmpty() || received.isNotEmpty()) {
                                            Spacer(Modifier.height(10.dp))
                                            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                                                devices.forEach { device ->
                                                    Row(
                                                        Modifier.fillMaxWidth()
                                                            .clip(RoundedCornerShape(12.dp))
                                                            .background(palette.elevated)
                                                            .border(1.dp, palette.hairline, RoundedCornerShape(12.dp))
                                                            .clickable(enabled = !busy && !offline) {
                                                                destination = device.getString("device_id")
                                                                transfer.launch(arrayOf("*/*"))
                                                            }
                                                            .padding(horizontal = 14.dp, vertical = 12.dp),
                                                        horizontalArrangement = Arrangement.SpaceBetween,
                                                        verticalAlignment = Alignment.CenterVertically,
                                                    ) {
                                                        Text(device.getString("device_name"), fontWeight = FontWeight.Medium, color = palette.textPrimary)
                                                        GlassPillBadge("发送 ›", palette.accentText, palette.accentSoft)
                                                    }
                                                }
                                                received.forEach { file ->
                                                    GlassButton(
                                                        "保存已接收：${file.name}",
                                                        style = GlassButtonStyle.AccentSoft,
                                                        modifier = Modifier.fillMaxWidth(),
                                                        onClick = {
                                                            exportItem = file
                                                            export.launch(file.name)
                                                        },
                                                    )
                                                }
                                            }
                                        }
                                    }
                                }

                                2 -> {
                                    // ==================== TAB 2: LOCAL MODEL CENTER ====================
                                    val state = modelsState
                                    val totalRamMb = state?.optLong("total_ram_mb", 0L) ?: 0L
                                    val ramTier = state?.optString("ram_tier", "unknown").orEmpty()
                                    val tierLabel =
                                        when (ramTier) {
                                            "low" -> "轻量档位"
                                            "standard" -> "标准档位"
                                            "high" -> "高性能档位"
                                            else -> "检测中"
                                        }
                                    val ramBadge =
                                        if (totalRamMb > 0L)
                                            String.format(Locale.US, "%.1f GB · %s", totalRamMb / 1024.0, tierLabel)
                                        else "检测内存中…"

                                    GlassPanel(tinted = true) {
                                        GlassSectionHeader(
                                            "本机模型中心",
                                            if (offline) "纯离线模式已开启：可自由启停已下载的本地模型"
                                            else "语法、手写、重排与离线语音按需下载并热加载",
                                            trailing = {
                                                GlassPillBadge(ramBadge, palette.accentText, palette.accentSoft)
                                            },
                                        )
                                        ConfigField(config, "model_catalog_url", "自定义模型目录地址（可选）", "留空使用官方目录") {
                                            textFieldsDirty = true
                                        }
                                        Spacer(Modifier.height(10.dp))
                                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                            GlassButton("刷新状态", style = GlassButtonStyle.Ghost, enabled = !busy, onClick = { refreshModels(false) })
                                            GlassButton(
                                                "检查目录更新",
                                                style = GlassButtonStyle.AccentSoft,
                                                enabled = !busy && !offline,
                                                modifier = Modifier.weight(1f),
                                                onClick = {
                                                    busy = true
                                                    val url = config.optString("model_catalog_url").trim()
                                                    val payload = if (url.isNotEmpty()) json("url" to url) else JSONObject()
                                                    Core.call(this@SettingsActivity, "models-refresh-catalog", payload, true) { r ->
                                                        busy = false
                                                        r.onSuccess {
                                                            modelsState = it as? JSONObject
                                                            showToast("模型目录已更新")
                                                        }.onFailure { showToast(it.message.orEmpty()) }
                                                    }
                                                },
                                            )
                                        }
                                    }

                                    val models = state?.optJSONArray("models")
                                    if (models == null || models.length() == 0) {
                                        GlassPanel {
                                            Text("正在读取本机模型状态…", fontSize = 13.sp, color = palette.textSecondary)
                                        }
                                    } else {
                                        data class ModelGroupDef(
                                            val id: String,
                                            val title: String,
                                            val subtitle: String,
                                            val caps: List<String>,
                                        )
                                        data class CapDef(
                                            val id: String,
                                            val title: String,
                                            val subtitle: String,
                                        )
                                        val groupDefs =
                                            listOf(
                                                ModelGroupDef(
                                                    "input",
                                                    "输入与语言增强",
                                                    "万象长句语法解码与上下文语义重排",
                                                    listOf("grammar", "reranker"),
                                                ),
                                                ModelGroupDef(
                                                    "multimodal",
                                                    "多模态交互",
                                                    "离线手写认字与本地语音转写",
                                                    listOf("handwriting", "voice"),
                                                ),
                                                ModelGroupDef(
                                                    "ai",
                                                    "本地大模型与智能预测",
                                                    "端侧语境预测、用户画像蒸馏与离线 AI 工具",
                                                    listOf("local_llm"),
                                                ),
                                            )
                                        val capDefs =
                                            mapOf(
                                                "grammar" to CapDef("grammar", "中文语法模型", "提升全拼与双拼长句切分及语境排序"),
                                                "reranker" to CapDef("reranker", "语义重排模型", "连续手写模糊字位重排与语义增强"),
                                                "handwriting" to CapDef("handwriting", "离线手写识别", "单字查音与连续手写联句（含生僻字索引）"),
                                                "voice" to CapDef("voice", "离线语音转写", "按硬件档位选择轻快均衡或高精多语方言"),
                                                "local_llm" to CapDef("local_llm", "本地 Qwen3 推理", "按内存档位选择低 / 中 / 高精端侧预测与 AI"),
                                            )
                                        val allModels = (0 until models.length()).map { models.getJSONObject(it) }
                                        fun modelCapOf(m: JSONObject) =
                                            m.optString("capability").ifEmpty {
                                                m.optJSONObject("model")?.optString("capability").orEmpty()
                                            }
                                        fun modelTierLabel(m: JSONObject): String {
                                            val rec = m.optLong("recommended_ram_mb", 0L)
                                            return when {
                                                rec <= 2048L -> "低配 · 轻量档"
                                                rec <= 6144L -> "中配 · 均衡档"
                                                else -> "高配 · 旗舰档"
                                            }
                                        }
                                        val knownCaps = groupDefs.flatMap { it.caps }.toSet()
                                        val extraCaps = allModels.map(::modelCapOf).filter { it.isNotEmpty() && it !in knownCaps }.distinct()
                                        val effectiveGroups =
                                            if (extraCaps.isEmpty()) groupDefs
                                            else groupDefs + ModelGroupDef("other", "扩展模型", "自定义目录中的扩展能力模型", extraCaps)

                                        // Active downloads summary strip on root/mid levels
                                        val activeDls = allModels.filter { it.optJSONObject("download")?.optString("status") == "downloading" }
                                        if (activeDls.isNotEmpty() && modelCap == null) {
                                            GlassPanel(tinted = true) {
                                                Text("正在下载 (${activeDls.size})", fontSize = 12.5.sp, fontWeight = FontWeight.SemiBold, color = palette.accentText)
                                                Spacer(Modifier.height(6.dp))
                                                activeDls.forEach { dm ->
                                                    val dl = dm.optJSONObject("download")
                                                    val total = dl?.optLong("total_bytes", 1L)?.coerceAtLeast(1L) ?: 1L
                                                    val done = dl?.optLong("downloaded_bytes", 0L)?.coerceIn(0L, total) ?: 0L
                                                    val pct = ((done * 100L) / total).toInt()
                                                    Row(
                                                        Modifier.fillMaxWidth().padding(vertical = 3.dp),
                                                        horizontalArrangement = Arrangement.SpaceBetween,
                                                    ) {
                                                        Text(dm.optString("name"), fontSize = 12.sp, color = palette.textPrimary, modifier = Modifier.weight(1f), maxLines = 1, overflow = TextOverflow.Ellipsis)
                                                        Text("$pct%", fontSize = 12.sp, fontFamily = FontFamily.Monospace, color = palette.accentText)
                                                    }
                                                }
                                            }
                                        }

                                        // Breadcrumb navigation when drilled into a category
                                        if (modelGroup != null) {
                                            val curGroup = effectiveGroups.find { it.id == modelGroup }
                                            val curCapDef = modelCap?.let { capDefs[it] ?: CapDef(it, it, "") }
                                            GlassPanel(padding = PaddingValues(horizontal = 14.dp, vertical = 10.dp)) {
                                                Row(
                                                    Modifier.fillMaxWidth(),
                                                    horizontalArrangement = Arrangement.SpaceBetween,
                                                    verticalAlignment = Alignment.CenterVertically,
                                                ) {
                                                    Row(
                                                        verticalAlignment = Alignment.CenterVertically,
                                                        horizontalArrangement = Arrangement.spacedBy(4.dp),
                                                        modifier = Modifier.weight(1f),
                                                    ) {
                                                        Text(
                                                            "模型目录",
                                                            fontSize = 12.5.sp,
                                                            color = palette.accentText,
                                                            fontWeight = FontWeight.Medium,
                                                            modifier = Modifier.clickable {
                                                                modelGroup = null
                                                                modelCap = null
                                                            },
                                                        )
                                                        if (curGroup != null) {
                                                            Text("›", fontSize = 12.5.sp, color = palette.textSecondary)
                                                            Text(
                                                                curGroup.title,
                                                                fontSize = 12.5.sp,
                                                                color = if (curCapDef != null) palette.accentText else palette.textPrimary,
                                                                fontWeight = FontWeight.SemiBold,
                                                                modifier = Modifier.clickable { modelCap = null },
                                                            )
                                                        }
                                                        if (curCapDef != null) {
                                                            Text("›", fontSize = 12.5.sp, color = palette.textSecondary)
                                                            Text(
                                                                curCapDef.title,
                                                                fontSize = 12.5.sp,
                                                                color = palette.textPrimary,
                                                                fontWeight = FontWeight.SemiBold,
                                                                maxLines = 1,
                                                                overflow = TextOverflow.Ellipsis,
                                                            )
                                                        }
                                                    }
                                                    GlassButton(
                                                        "← 返回",
                                                        style = GlassButtonStyle.Ghost,
                                                        compact = true,
                                                        onClick = {
                                                            if (modelCap != null) modelCap = null
                                                            else modelGroup = null
                                                        },
                                                    )
                                                }
                                            }
                                        }

                                        when {
                                            // Level 1: Primary Categories
                                            modelGroup == null -> {
                                                effectiveGroups.forEach { grp ->
                                                    val grpModels = allModels.filter { modelCapOf(it) in grp.caps }
                                                    if (grpModels.isEmpty()) return@forEach
                                                    val activeCnt = grpModels.count { it.optBoolean("active") }
                                                    val installedCnt = grpModels.count { it.optBoolean("installed") || it.optBoolean("bundled") }
                                                    GlassPanel {
                                                        Row(
                                                            Modifier.fillMaxWidth().clickable {
                                                                modelGroup = grp.id
                                                                val availCaps = grp.caps.filter { cap -> grpModels.any { modelCapOf(it) == cap } }
                                                                modelCap = if (availCaps.size == 1) availCaps.first() else null
                                                            },
                                                            horizontalArrangement = Arrangement.SpaceBetween,
                                                            verticalAlignment = Alignment.CenterVertically,
                                                        ) {
                                                            Column(Modifier.weight(1f).padding(end = 10.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                                                                Text(grp.title, fontSize = 15.5.sp, fontWeight = FontWeight.SemiBold, color = palette.textPrimary)
                                                                Text(grp.subtitle, fontSize = 12.sp, lineHeight = 17.sp, color = palette.textSecondary)
                                                                Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                                                                    GlassPillBadge(
                                                                        if (activeCnt > 0) "已启用 $activeCnt 项" else "共 ${grpModels.size} 个模型",
                                                                        if (activeCnt > 0) palette.success else palette.textSecondary,
                                                                        if (activeCnt > 0) palette.successSoft else palette.elevated,
                                                                    )
                                                                    if (installedCnt > 0) {
                                                                        GlassPillBadge("已安装 $installedCnt", palette.accentText, palette.accentSoft)
                                                                    }
                                                                }
                                                            }
                                                            Text("›", fontSize = 22.sp, color = palette.textSecondary)
                                                        }
                                                    }
                                                }
                                            }

                                            // Level 2: Sub-feature Categories within Selected Primary Category
                                            modelCap == null -> {
                                                val grp = effectiveGroups.find { it.id == modelGroup }
                                                val subCaps = grp?.caps.orEmpty().filter { cap -> allModels.any { modelCapOf(it) == cap } }
                                                subCaps.forEach { cap ->
                                                    val capDef = capDefs[cap] ?: CapDef(cap, cap, "")
                                                    val capModels = allModels.filter { modelCapOf(it) == cap }.sortedBy { it.optLong("recommended_ram_mb", 0L) }
                                                    val activeCnt = capModels.count { it.optBoolean("active") }
                                                    val tiers = capModels.map(::modelTierLabel).distinct().joinToString(" / ")
                                                    GlassPanel {
                                                        Row(
                                                            Modifier.fillMaxWidth().clickable { modelCap = cap },
                                                            horizontalArrangement = Arrangement.SpaceBetween,
                                                            verticalAlignment = Alignment.CenterVertically,
                                                        ) {
                                                            Column(Modifier.weight(1f).padding(end = 10.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                                                                Text(capDef.title, fontSize = 15.sp, fontWeight = FontWeight.SemiBold, color = palette.textPrimary)
                                                                if (capDef.subtitle.isNotBlank()) {
                                                                    Text(capDef.subtitle, fontSize = 12.sp, lineHeight = 17.sp, color = palette.textSecondary)
                                                                }
                                                                Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                                                                    GlassPillBadge(
                                                                        if (activeCnt > 0) "已启用" else "未启用",
                                                                        if (activeCnt > 0) palette.success else palette.textSecondary,
                                                                        if (activeCnt > 0) palette.successSoft else palette.elevated,
                                                                    )
                                                                    GlassPillBadge(tiers, palette.accentText, palette.accentSoft)
                                                                }
                                                            }
                                                            Text("›", fontSize = 22.sp, color = palette.textSecondary)
                                                        }
                                                    }
                                                }
                                            }

                                            // Level 3: Tiered Models (Low / Mid / High) under Selected Sub-feature
                                            else -> {
                                                if (modelCap == "local_llm") {
                                                    GlassPanel {
                                                        GlassSectionHeader(
                                                            "低配 · 内置算法预测引擎",
                                                            "无需下载大模型，基于本地词典前缀续写与个人习惯记忆实现零延迟预测",
                                                            trailing = { GlassPillBadge("内置已启用", palette.success, palette.successSoft) },
                                                        )
                                                        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                                                            GlassPillBadge("低配 · 零体积", palette.accentText, palette.accentSoft)
                                                            GlassPillBadge("0 MB", palette.textSecondary, palette.elevated)
                                                            GlassPillBadge("全机型推荐", palette.success, palette.successSoft)
                                                        }
                                                    }
                                                }
                                                val tiered =
                                                    allModels
                                                        .filter { modelCapOf(it) == modelCap }
                                                        .sortedBy { it.optLong("recommended_ram_mb", 0L) }
                                                tiered.forEach { m ->
                                                    val id = m.getString("id")
                                                    val name = m.getString("name")
                                                    val desc = m.optString("description")
                                                    val version = m.optString("version")
                                                    val fit = m.optString("fit")
                                                    val installed = m.optBoolean("installed")
                                                    val bundled = m.optBoolean("bundled")
                                                    val enabled = m.optBoolean("enabled")
                                                    val active = m.optBoolean("active")
                                                    val minRam = m.optLong("min_ram_mb", 0L)
                                                    val recRam = m.optLong("recommended_ram_mb", 0L)
                                                    val files = m.optJSONArray("files")
                                                    var totalBytes = 0L
                                                    if (files != null) {
                                                        for (fi in 0 until files.length()) {
                                                            totalBytes += files.getJSONObject(fi).optLong("size", 0L)
                                                        }
                                                    }
                                                    val dl = m.optJSONObject("download")
                                                    val dlStatus = dl?.optString("status").orEmpty()
                                                    val downloading = dlStatus == "downloading"
                                                    val statusText =
                                                        when {
                                                            downloading -> "下载中"
                                                            active && installed -> "已启用"
                                                            active && bundled -> "内置启用"
                                                            installed -> "已下载"
                                                            bundled -> "本地内置"
                                                            else -> "未下载"
                                                        }
                                                    val (badgeFg, badgeBg) =
                                                        when {
                                                            downloading -> palette.accentText to palette.accentSoft
                                                            active -> palette.success to palette.successSoft
                                                            fit == "exceeds_ram" -> palette.warning to palette.warningSoft
                                                            installed || bundled -> palette.accentText to palette.accentSoft
                                                            else -> palette.textSecondary to palette.elevated
                                                        }

                                                    GlassPanel {
                                                        GlassSectionHeader(
                                                            name,
                                                            desc,
                                                            trailing = { GlassPillBadge(statusText, badgeFg, badgeBg) },
                                                        )
                                                        Row(
                                                            horizontalArrangement = Arrangement.spacedBy(6.dp),
                                                            verticalAlignment = Alignment.CenterVertically,
                                                        ) {
                                                            GlassPillBadge(modelTierLabel(m), palette.accentText, palette.accentSoft)
                                                            GlassPillBadge(formatBytes(totalBytes), palette.textSecondary, palette.elevated)
                                                            GlassPillBadge(
                                                                when (fit) {
                                                                    "recommended" -> "内存推荐"
                                                                    "compatible" -> "内存兼容"
                                                                    "exceeds_ram" -> "超出推荐内存"
                                                                    else -> fit
                                                                },
                                                                if (fit == "exceeds_ram") palette.warning else palette.success,
                                                                if (fit == "exceeds_ram") palette.warningSoft else palette.successSoft,
                                                            )
                                                        }
                                                        Spacer(Modifier.height(6.dp))
                                                        Text(
                                                            String.format(
                                                                Locale.US,
                                                                "版本 v%s · 内存要求：最低 %.1f GB · 推荐 %.1f GB",
                                                                version,
                                                                minRam / 1024.0,
                                                                recRam / 1024.0,
                                                            ),
                                                            fontSize = 11.5.sp,
                                                            color = palette.textSecondary,
                                                        )

                                                        if (downloading && dl != null) {
                                                            val dlTotal = dl.optLong("total_bytes", totalBytes).coerceAtLeast(1L)
                                                            val dlDone = dl.optLong("downloaded_bytes", 0L).coerceIn(0L, dlTotal)
                                                            val prog = dlDone.toFloat() / dlTotal.toFloat()
                                                            Spacer(Modifier.height(10.dp))
                                                            Box(
                                                                Modifier.fillMaxWidth()
                                                                    .height(6.dp)
                                                                    .clip(CircleShape)
                                                                    .background(palette.elevated)
                                                            ) {
                                                                Box(
                                                                    Modifier.fillMaxWidth(prog)
                                                                        .fillMaxHeight()
                                                                        .clip(CircleShape)
                                                                        .background(palette.accent)
                                                                )
                                                            }
                                                            Spacer(Modifier.height(4.dp))
                                                            Text(
                                                                "${formatBytes(dlDone)} / ${formatBytes(dlTotal)} (${(prog * 100).roundToInt()}%)",
                                                                fontSize = 11.5.sp,
                                                                fontFamily = FontFamily.Monospace,
                                                                color = palette.textSecondary,
                                                            )
                                                        } else if (dlStatus == "failed") {
                                                            val err = dl?.optString("error").orEmpty()
                                                            if (err.isNotBlank()) {
                                                                Spacer(Modifier.height(6.dp))
                                                                Text("下载失败：$err", fontSize = 12.sp, color = palette.danger)
                                                            }
                                                        }

                                                        Spacer(Modifier.height(10.dp))
                                                        GlassHairline()
                                                        Spacer(Modifier.height(10.dp))
                                                        Row(
                                                            Modifier.fillMaxWidth(),
                                                            horizontalArrangement = Arrangement.SpaceBetween,
                                                            verticalAlignment = Alignment.CenterVertically,
                                                        ) {
                                                            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                                                if (downloading) {
                                                                    GlassButton(
                                                                        "取消下载",
                                                                        style = GlassButtonStyle.Ghost,
                                                                        compact = true,
                                                                        enabled = !busy,
                                                                        onClick = {
                                                                            Core.call(this@SettingsActivity, "models-cancel", json("id" to id), true) { r ->
                                                                                r.onSuccess {
                                                                                    modelsState = it as? JSONObject
                                                                                    showToast("已取消下载")
                                                                                }.onFailure { showToast(it.message.orEmpty()) }
                                                                            }
                                                                        },
                                                                    )
                                                                } else if (!installed) {
                                                                    GlassButton(
                                                                        when {
                                                                            dlStatus == "failed" -> "重试下载"
                                                                            fit == "exceeds_ram" -> "仍要下载"
                                                                            else -> "下载并启用"
                                                                        },
                                                                        style = GlassButtonStyle.AccentSoft,
                                                                        compact = true,
                                                                        enabled = !busy && !offline,
                                                                        onClick = {
                                                                            busy = true
                                                                            Core.call(this@SettingsActivity, "models-download", json("id" to id), true) { r ->
                                                                                busy = false
                                                                                r.onSuccess {
                                                                                    modelsState = it as? JSONObject
                                                                                    showToast("开始下载「$name」")
                                                                                }.onFailure { showToast(it.message.orEmpty()) }
                                                                            }
                                                                        },
                                                                    )
                                                                }
                                                                if (installed && !downloading) {
                                                                    GlassButton(
                                                                        if (confirmDeleteModelId == id) "确认删除" else "删除",
                                                                        style = GlassButtonStyle.DangerGhost,
                                                                        compact = true,
                                                                        enabled = !busy,
                                                                        onClick = {
                                                                            if (confirmDeleteModelId != id) {
                                                                                confirmDeleteModelId = id
                                                                                showToast("再次点击「确认删除」以清理「$name」")
                                                                                return@GlassButton
                                                                            }
                                                                            confirmDeleteModelId = null
                                                                            busy = true
                                                                            Core.call(this@SettingsActivity, "models-delete", json("id" to id), true) { r ->
                                                                                busy = false
                                                                                r.onSuccess {
                                                                                    modelsState = it as? JSONObject
                                                                                    showToast("已删除「$name」")
                                                                                }.onFailure { showToast(it.message.orEmpty()) }
                                                                            }
                                                                        },
                                                                    )
                                                                }
                                                            }
                                                            GlassSwitch(
                                                                checked = enabled && (installed || bundled),
                                                                enabled = !busy && !downloading && (installed || bundled),
                                                                contentDesc = "启用$name",
                                                                onCheckedChange = { next ->
                                                                    val replacedPeer =
                                                                        if (next) {
                                                                            tiered
                                                                                .firstOrNull {
                                                                                    it.optString("id") != id &&
                                                                                        it.optBoolean("active")
                                                                                }
                                                                                ?.optString("name")
                                                                                .orEmpty()
                                                                        } else ""
                                                                    busy = true
                                                                    Core.call(
                                                                        this@SettingsActivity,
                                                                        "models-set-enabled",
                                                                        json("id" to id, "enabled" to next),
                                                                        true,
                                                                    ) { r ->
                                                                        busy = false
                                                                        r.onSuccess {
                                                                            modelsState = it as? JSONObject
                                                                            showToast(
                                                                                when {
                                                                                    next && replacedPeer.isNotEmpty() ->
                                                                                        "已切换为「$name」，已自动关闭「$replacedPeer」"
                                                                                    next -> "已启用「$name」"
                                                                                    else -> "已停用「$name」"
                                                                                }
                                                                            )
                                                                        }.onFailure { showToast(it.message.orEmpty()) }
                                                                    }
                                                                },
                                                            )
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                else -> {
                                    // ==================== TAB 3: VOICE, AI & SHORTCUTS ====================
                                    GlassPanel(tinted = true) {
                                        GlassSectionHeader(
                                            "离线录音与文本工作台",
                                            "SenseVoice / Qwen3-ASR 语音转写 · 本地 Qwen3 离线翻译与润色",
                                            trailing = {
                                                if (recording) {
                                                    GlassPillBadge("录音中", palette.danger, palette.dangerSoft)
                                                }
                                            },
                                        )
                                        GlassTextField(
                                            value = source,
                                            onValueChange = { source = it },
                                            label = "",
                                            placeholder = "输入原文，或点击下方「开始录音」进行本机离线听写…",
                                            singleLine = false,
                                            minLines = 3,
                                        )
                                        Spacer(Modifier.height(10.dp))
                                        GlassSegmentedControl(
                                            options = listOf("en" to "译为英语", "zh" to "译为中文", "ja" to "译为日语"),
                                            selected = targetLang,
                                            onSelect = { targetLang = it },
                                        )
                                        Spacer(Modifier.height(10.dp))
                                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                            GlassButton(
                                                if (recording) "停止转写" else "开始录音",
                                                style = if (recording) GlassButtonStyle.DangerGhost else GlassButtonStyle.Primary,
                                                enabled = !busy || recording,
                                                modifier = Modifier.weight(1.2f),
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
                                                            result.onSuccess {
                                                                source = it
                                                                showToast(if (it.isBlank()) "未识别到语音" else "本机转写完成")
                                                            }.onFailure { showToast(it.message.orEmpty()) }
                                                        }
                                                    }
                                                },
                                            )
                                            listOf("translate" to "翻译", "polish" to "润色", "summary" to "摘要").forEach { (m, label) ->
                                                GlassButton(
                                                    label,
                                                    style = GlassButtonStyle.Ghost,
                                                    compact = true,
                                                    enabled = !busy && source.isNotBlank(),
                                                    onClick = {
                                                        busy = true
                                                        Core.call(
                                                            this@SettingsActivity,
                                                            "ai-inline",
                                                            json("mode" to m, "text" to source, "target_lang" to targetLang),
                                                            true,
                                                        ) { r ->
                                                            busy = false
                                                            r.onSuccess {
                                                                aiOutput = (it as JSONObject).optString("output_text")
                                                                showToast("处理完成")
                                                            }.onFailure { showToast(it.message.orEmpty()) }
                                                        }
                                                    },
                                                )
                                            }
                                        }
                                        if (aiOutput.isNotBlank()) {
                                            Spacer(Modifier.height(12.dp))
                                            Column(
                                                Modifier.fillMaxWidth()
                                                    .clip(RoundedCornerShape(14.dp))
                                                    .background(palette.accentSoft)
                                                    .border(1.dp, palette.accentBorder, RoundedCornerShape(14.dp))
                                                    .padding(12.dp),
                                                verticalArrangement = Arrangement.spacedBy(8.dp),
                                            ) {
                                                Row(
                                                    Modifier.fillMaxWidth(),
                                                    horizontalArrangement = Arrangement.SpaceBetween,
                                                    verticalAlignment = Alignment.CenterVertically,
                                                ) {
                                                    Text("处理结果", fontSize = 12.sp, fontWeight = FontWeight.SemiBold, color = palette.accentText)
                                                    Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                                                        GlassButton(
                                                            "复制",
                                                            style = GlassButtonStyle.Ghost,
                                                            compact = true,
                                                            onClick = {
                                                                getSystemService(ClipboardManager::class.java)
                                                                    .setPrimaryClip(ClipData.newPlainText("Vitral", aiOutput))
                                                                showToast("已复制到剪贴板")
                                                            },
                                                        )
                                                        GlassButton(
                                                            "替换原文",
                                                            style = GlassButtonStyle.Ghost,
                                                            compact = true,
                                                            onClick = {
                                                                source = aiOutput
                                                                showToast("已替换原文")
                                                            },
                                                        )
                                                    }
                                                }
                                                Text(aiOutput, fontSize = 13.5.sp, lineHeight = 20.sp, color = palette.textPrimary)
                                            }
                                        }
                                    }

                                    GlassPanel {
                                        GlassSectionHeader("AI 服务配置", "支持本机 Qwen3 纯离线执行，也可选配远端兼容接口")
                                        GlassToggleRow(
                                            "优先使用本机 Qwen3 模型",
                                            "纯离线模式下同样可执行翻译、润色与摘要",
                                            checked = aiPreferLocal,
                                            showDivider = false,
                                            onCheckedChange = {
                                                aiPreferLocal = it
                                                persistConfig()
                                            },
                                        )
                                        GlassToggleRow(
                                            "AI 辅助连续手写消歧",
                                            "仅在已有模糊字位候选中择优",
                                            checked = aiTier3,
                                            onCheckedChange = {
                                                aiTier3 = it
                                                persistConfig()
                                            },
                                        )
                                        Spacer(Modifier.height(8.dp))
                                        Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                                            ConfigField(config, "ai_endpoint", "服务地址", "http://localhost:11434") { textFieldsDirty = true }
                                            ConfigField(config, "ai_model", "模型名称", "qwen2.5:7b") { textFieldsDirty = true }
                                            ConfigField(config, "ai_api_key", "API 密钥", "本地模型可留空", secret = true) { textFieldsDirty = true }
                                        }
                                    }

                                    ShortcutSettings(
                                        config = config,
                                        onDirty = { textFieldsDirty = true },
                                        onNotice = { showToast(it) },
                                    )
                                }
                            }
                        }

                        // Floating Contextual Save Pill (ONLY shown when text credentials/shortcuts edited)
                        AnimatedVisibility(
                            visible = textFieldsDirty,
                            enter = fadeIn(),
                            exit = fadeOut(),
                            modifier =
                                Modifier.align(Alignment.BottomCenter)
                                    .navigationBarsPadding()
                                    .padding(bottom = 86.dp, start = 20.dp, end = 20.dp),
                        ) {
                            Row(
                                Modifier.shadow(12.dp, CircleShape)
                                    .clip(CircleShape)
                                    .background(if (dark) Color(0xF0182235) else Color(0xFAFFFFFF))
                                    .border(1.dp, palette.accentBorder, CircleShape)
                                    .padding(start = 16.dp, end = 6.dp, top = 6.dp, bottom = 6.dp),
                                verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.spacedBy(12.dp),
                            ) {
                                Text("有未保存的文本配置", fontSize = 12.5.sp, color = palette.textPrimary)
                                GlassButton(
                                    "立即保存",
                                    style = GlassButtonStyle.Primary,
                                    compact = true,
                                    onClick = { persistConfig("配置已保存并生效") },
                                )
                            }
                        }

                        // Floating Translucent Toast Banner (compact rounded card, never expands into a giant oval)
                        AnimatedVisibility(
                            visible = toastText.isNotBlank() && !textFieldsDirty,
                            enter = fadeIn(),
                            exit = fadeOut(),
                            modifier =
                                Modifier.align(Alignment.BottomCenter)
                                    .navigationBarsPadding()
                                    .padding(bottom = 86.dp, start = 20.dp, end = 20.dp),
                        ) {
                            val toastShape = RoundedCornerShape(16.dp)
                            Box(
                                Modifier.widthIn(max = 336.dp)
                                    .shadow(10.dp, toastShape)
                                    .clip(toastShape)
                                    .background(if (dark) Color(0xEB1B2438) else Color(0xF5FFFFFF))
                                    .border(1.dp, palette.borderTop, toastShape)
                                    .clickable { toastText = "" }
                                    .padding(horizontal = 16.dp, vertical = 10.dp)
                            ) {
                                Text(
                                    toastText,
                                    fontSize = 12.5.sp,
                                    lineHeight = 18.sp,
                                    maxLines = 3,
                                    overflow = TextOverflow.Ellipsis,
                                    fontWeight = FontWeight.Medium,
                                    color = palette.textPrimary,
                                )
                            }
                        }

                        // Floating Liquid Glass Bottom Navigation Capsule
                        GlassBottomNavBar(
                            selectedIndex = tab,
                            onSelect = { tab = it },
                            modifier = Modifier.align(Alignment.BottomCenter),
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun UsageTile(label: String, value: String, modifier: Modifier = Modifier) {
    val g = LocalGlass.current
    Column(
        modifier
            .clip(RoundedCornerShape(12.dp))
            .background(g.elevated)
            .border(1.dp, g.hairline, RoundedCornerShape(12.dp))
            .padding(horizontal = 10.dp, vertical = 9.dp),
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        Text(label, fontSize = 11.sp, color = g.textSecondary)
        Text(value, fontSize = 13.5.sp, fontWeight = FontWeight.SemiBold, fontFamily = FontFamily.Monospace, color = g.textPrimary)
    }
}

@Composable
private fun ConfigField(
    config: JSONObject,
    key: String,
    title: String,
    placeholder: String = "",
    secret: Boolean = false,
    onEdit: () -> Unit,
) {
    var value by remember(key) { mutableStateOf(config.optString(key)) }
    GlassTextField(
        value = value,
        onValueChange = {
            value = it
            config.put(key, it)
            onEdit()
        },
        label = title,
        placeholder = placeholder,
        secret = secret,
    )
}
