package dev.vitral.ime

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import dev.vitral.ime.core.Core
import org.json.JSONArray
import org.json.JSONObject

@Composable
fun ShortcutSettings(config: JSONObject, onNotice: (String) -> Unit) {
    val context = LocalContext.current
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
    OutlinedCard(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(16.dp),
    ) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Text(
                "快捷键与功能输入码",
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.SemiBold,
            )
            Text(
                "实体按键支持 Shift、F1–F12 及 Ctrl/Alt 组合；功能输入码使用纯英文字母，完整输入后显示功能候选，留空即可关闭。",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
    val actions = catalog?.optJSONArray("actions")
    if (actions == null) {
        Text(
            "正在读取快捷键规则…",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        return
    }
    val bindings =
        config.optJSONArray("shortcuts") ?: JSONArray().also { config.put("shortcuts", it) }
    for (i in 0 until actions.length()) {
        val action = actions.getJSONObject(i)
        val id = action.getString("id")
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
            OutlinedCard(
                modifier = Modifier.fillMaxWidth(),
                shape = RoundedCornerShape(14.dp),
            ) {
                Column(Modifier.padding(14.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
                    Text(
                        action.getString("label"),
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = FontWeight.SemiBold,
                    )
                    OutlinedTextField(
                        keyText,
                        {
                            keyText = it
                            binding.put("key", it)
                            onNotice("有未保存的设置")
                        },
                        label = { Text(action.getString("label") + "按键") },
                        placeholder = { Text("例如 Shift 或 Ctrl+Shift+V") },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth(),
                    )
                    OutlinedTextField(
                        inputText,
                        {
                            inputText = it
                            binding.put("text", it)
                            onNotice("有未保存的设置")
                        },
                        label = { Text(action.getString("label") + "输入码") },
                        placeholder = { Text("纯字母，例如 setting") },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
            }
        }
    }
}
