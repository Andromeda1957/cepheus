package io.github.cepheus.app.ui

import io.github.cepheus.app.protocol.Session
import io.github.cepheus.app.protocol.Status
import io.github.cepheus.app.protocol.StartHost
import org.json.JSONArray
import org.json.JSONObject

/** Wire values for terminal controls. Text is sent literally, never interpreted as a command. */
enum class TerminalKey(val label: String, val wire: String, val spoken: String = label) {
    ENTER("Enter", "enter"), CTRL_C("Ctrl C", "ctrl_c", "Control C"), TAB("Tab", "tab"), ESC("Esc", "escape", "Escape"),
    UP("↑", "up", "Up"), DOWN("↓", "down", "Down"), LEFT("←", "left", "Left"), RIGHT("→", "right", "Right"),
    BACKSPACE("⌫", "backspace", "Backspace"), CTRL_D("Ctrl D", "ctrl_d", "Control D"), HOME("Home", "home"), END("End", "end");

    companion object {
        /** The key row over the keyboard, most used first. */
        val row = listOf(ESC, TAB, CTRL_C, UP, DOWN, LEFT, RIGHT, CTRL_D, HOME, END)

        /** A hardware keyboard's keys the hidden field would otherwise swallow. */
        fun forHardware(key: androidx.compose.ui.input.key.Key): TerminalKey? = when (key) {
            androidx.compose.ui.input.key.Key.DirectionUp -> UP
            androidx.compose.ui.input.key.Key.DirectionDown -> DOWN
            androidx.compose.ui.input.key.Key.DirectionLeft -> LEFT
            androidx.compose.ui.input.key.Key.DirectionRight -> RIGHT
            androidx.compose.ui.input.key.Key.Escape -> ESC
            androidx.compose.ui.input.key.Key.Tab -> TAB
            androidx.compose.ui.input.key.Key.MoveHome -> HOME
            androidx.compose.ui.input.key.Key.MoveEnd -> END
            else -> null
        }
    }
}

object ToolForms {
    /** An explicit target can be local (a null name); only an unspecified target uses the last online choice. */
    fun initialStartHost(hosts: List<StartHost>, requested: StartHost?, previous: String?): String? =
        if (requested != null) requested.name else previous?.takeIf { name -> hosts.any { it.name == name } }

    /** Losing a remote machine never silently redirects a start to the desktop. */
    fun startHost(hosts: List<StartHost>, picked: String?): StartHost? = hosts.firstOrNull { it.name == picked }

    fun hostArgs(host: String?) = JSONObject().apply { if (host != null) put("host", if (host == "*") "all" else host) }

    fun targets(sessions: List<Session>): List<Long> = sessions.filter {
        it.alive && it.tool != "shell" && (it.status != Status.NEEDS_YOU || it.detail == "queued question")
    }.map { it.id }

    fun create(tool: String, host: String?, cwd: String, name: String, model: String?, effort: String?, text: String?) =
        JSONObject().put("tool", tool).apply {
            host?.let { put("host", it) }
            cwd.trim().takeIf { it.isNotEmpty() }?.let { put("cwd", it) }
            name.trim().takeIf { it.isNotEmpty() }?.let { put("name", it) }
            model?.let { put("model", it) }
            effort?.let { put("effort", it) }
            text?.takeIf { it.isNotBlank() }?.let { put("text", it) }
        }

    /**
     * A deck command's arguments as its own command line splits them: spaces
     * separate words, quotes keep one together, a backslash takes the next
     * character as it is.
     */
    fun words(line: String): List<String> {
        val words = mutableListOf<String>()
        val word = StringBuilder()
        var quote: Char? = null
        var open = false
        var i = 0
        while (i < line.length) {
            val c = line[i]
            when {
                quote != null && c == quote -> quote = null
                quote == null && (c == '"' || c == '\'') -> { quote = c; open = true }
                c == '\\' && quote != '\'' && i + 1 < line.length -> { i++; word.append(line[i]); open = true }
                quote == null && c.isWhitespace() -> if (open) { words += word.toString(); word.clear(); open = false }
                else -> { word.append(c); open = true }
            }
            i++
        }
        if (open) words += word.toString()
        return words
    }

    /** No shell splitting: each nonempty line is one literal program argument. */
    fun terminal(host: String?, cwd: String, name: String, command: String, scratch: Boolean) =
        JSONObject().put("scratch", scratch).apply {
            host?.let { put("host", it) }
            cwd.trim().takeIf { it.isNotEmpty() }?.let { put("cwd", it) }
            name.trim().takeIf { it.isNotEmpty() }?.let { put("name", it) }
            command.lineSequence().filter { it.isNotBlank() }.toList().takeIf { it.isNotEmpty() }
                ?.let { put("command", JSONArray(it)) }
        }
}

internal fun JSONObject.objects(key: String): List<JSONObject> {
    val array = optJSONArray(key) ?: return emptyList()
    return (0 until array.length()).mapNotNull { array.optJSONObject(it) }
}

internal fun JSONObject.string(key: String): String = optString(key).takeUnless { it == "null" }.orEmpty()
