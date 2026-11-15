package io.github.cepheus.app.ui

import android.util.JsonReader
import android.util.JsonToken
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.heightIn
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import java.io.StringReader
import org.json.JSONArray
import org.json.JSONObject
import org.json.JSONTokener

/** Added plugins describe their phone commands; no dedicated phone screen is required. */
@Composable
internal fun PluginTools(online: Boolean) {
    val discovery = rememberToolRequest()
    LaunchedEffect(online) { if (online) discovery.run("plugins") }
    HorizontalDivider()
    Text("Deck plugins", style = MaterialTheme.typography.titleMedium)
    ToolButton("Refresh plugins", online && !discovery.busy) { discovery.run("plugins") }
    RequestStatus(discovery)
    discovery.data?.let { data ->
        val plugins = data.objects("plugins").filter { it.string("id").isNotBlank() }
        if (plugins.isEmpty()) Text("No deck plugins are available.")
        for (plugin in plugins) key(plugin.string("id")) { PluginCommands(plugin, online) }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun PluginCommands(plugin: JSONObject, online: Boolean) {
    val id = plugin.string("id")
    val enabled = plugin.optBoolean("enabled", false)
    val commands = plugin.objects("commands").filter { it.string("name").isNotBlank() }
    var selected by rememberSaveable(id) { mutableStateOf<String?>(null) }
    val command = commands.firstOrNull { it.string("name") == selected }
    val request = rememberToolRequest(id to selected)
    Text(plugin.string("name").ifBlank { id } + if (enabled) "" else " · disabled", style = MaterialTheme.typography.titleSmall)
    plugin.string("description").takeIf { it.isNotBlank() }?.let { Text(it) }
    FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        for (entry in commands) FilterChip(
            selected = entry.string("name") == selected,
            onClick = { selected = entry.string("name") },
            enabled = enabled && !request.busy,
            label = { Text(entry.string("name")) },
            modifier = Modifier.heightIn(min = 48.dp),
        )
    }
    if (commands.isEmpty()) Text("No phone commands.", style = MaterialTheme.typography.bodySmall)
    command?.let { entry ->
        entry.string("description").takeIf { it.isNotBlank() }?.let { Text(it) }
        val names = entry.optJSONArray("args")?.let { args ->
            (0 until args.length()).mapNotNull { args.opt(it) as? String }.filter { it.isNotBlank() }.distinct()
        }.orEmpty()
        val values = linkedMapOf<String, String>()
        for (name in names) key(id, selected, name) {
            var value by rememberSaveable { mutableStateOf("") }
            values[name] = value
            ToolField(value, { value = it.take(16384) }, name, literal = true)
        }
        if (names.isNotEmpty()) Text(
            "Arguments accept text or JSON. Leave optional arguments empty; use \"\" for an empty string.",
            style = MaterialTheme.typography.bodySmall,
        )
        ToolButton("Run ${entry.string("name")}", online && enabled && !request.busy) {
            val args = JSONObject()
            for ((name, value) in values) if (value.isNotEmpty()) args.put(name, pluginArgument(value))
            request.run("plugin", JSONObject().put("plugin", id).put("command", entry.string("name")).put("args", args))
        }
    }
    RequestStatus(request)
    request.data?.let { Output(it.toString(2)) }
}

/** JsonReader is strict; org.json alone accepts truncated values and non-JSON syntax. */
private fun pluginArgument(text: String): Any = try {
    JsonReader(StringReader("[$text]")).use { reader ->
        reader.beginArray()
        val value = readArgument(reader, 0)
        reader.endArray()
        check(reader.peek() == JsonToken.END_DOCUMENT)
        value
    }
} catch (_: Exception) { text }

private fun readArgument(reader: JsonReader, depth: Int): Any {
    check(depth <= 32)
    return when (reader.peek()) {
        JsonToken.BEGIN_OBJECT -> JSONObject().apply {
            reader.beginObject()
            while (reader.hasNext()) put(reader.nextName(), readArgument(reader, depth + 1))
            reader.endObject()
        }
        JsonToken.BEGIN_ARRAY -> JSONArray().apply {
            reader.beginArray()
            while (reader.hasNext()) put(readArgument(reader, depth + 1))
            reader.endArray()
        }
        JsonToken.STRING -> reader.nextString()
        JsonToken.NUMBER -> JSONTokener(reader.nextString()).nextValue().also {
            require(it !is Number || it.toDouble().isFinite())
        }
        JsonToken.BOOLEAN -> reader.nextBoolean()
        JsonToken.NULL -> { reader.nextNull(); JSONObject.NULL }
        else -> error("Expected a JSON value")
    }
}
