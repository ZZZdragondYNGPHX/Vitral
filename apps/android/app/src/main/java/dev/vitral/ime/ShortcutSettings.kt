package dev.vitral.ime

import androidx.compose.foundation.layout.*
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.vitral.ime.core.Core
import dev.vitral.ime.ui.*
import org.json.JSONArray
import org.json.JSONObject

@Composable
fun ShortcutSettings(
    config: JSONObject,
    onDirty: () -> Unit,
    onNotice: (String) -> Unit,
) {
    val context = LocalContext.current
    val g = LocalGlass.current
    var catalog by remember { mutableStateOf<JSONObject?>(null) }
    LaunchedEffect(Unit) {
        Core.call(context, "shortcuts-state") { result ->
            result
                .onSuccess {
                    val state = it as JSONObject
                    if (!config.has("shortcuts"))
                        config.put("shortcuts", state.getJSONArray("bindings"))
                    catalog = state
                }
                .onFailure { onNotice(it.message.orEmpty()) }
        }
    }

    GlassPanel {
        GlassSectionHeader(
            "快捷键与功能输入码",
            "实体按键支持 Shift、F1–F12 及 Ctrl/Alt 组合；功能输入码使用纯英文字母，留空即关闭",
        )
        val actions = catalog?.optJSONArray("actions")
        if (actions == null) {
            Text("正在读取快捷键规则…", fontSize = 12.5.sp, color = g.textSecondary)
            return@GlassPanel
        }
        val bindings =
            config.optJSONArray("shortcuts") ?: JSONArray().also { config.put("shortcuts", it) }
        Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
            for (i in 0 until actions.length()) {
                val action = actions.getJSONObject(i)
                val id = action.getString("id")
                val label = action.getString("label")
                val binding =
                    (0 until bindings.length())
                        .map { bindings.getJSONObject(it) }
                        .find { it.optString("action") == id }
                        ?: JSONObject()
                            .put("action", id)
                            .put("key", "")
                            .put("text", "")
                            .also { bindings.put(it) }
                key(id) {
                    var keyText by remember { mutableStateOf(binding.optString("key")) }
                    var inputText by remember { mutableStateOf(binding.optString("text")) }
                    if (i > 0) GlassHairline()
                    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Text(label, fontSize = 13.5.sp, color = g.textPrimary)
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            GlassTextField(
                                value = keyText,
                                onValueChange = {
                                    keyText = it
                                    binding.put("key", it)
                                    onDirty()
                                },
                                label = "实体按键",
                                placeholder = "如 Shift",
                                modifier = Modifier.weight(1f),
                            )
                            GlassTextField(
                                value = inputText,
                                onValueChange = {
                                    inputText = it
                                    binding.put("text", it)
                                    onDirty()
                                },
                                label = "功能输入码",
                                placeholder = "如 setting",
                                modifier = Modifier.weight(1f),
                            )
                        }
                    }
                }
            }
        }
    }
}
