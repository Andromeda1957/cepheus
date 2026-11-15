package io.github.cepheus.app.ui

import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.IconButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.key
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.cepheus.app.protocol.Item
import io.github.cepheus.app.protocol.Lines
import java.util.Locale









sealed interface ChatEntry {
    val key: String

    data class One(val row: ChatRow) : ChatEntry {
        override val key: String get() = row.key
    }

    /** A run of work from its first tool call on. */
    data class Tools(val rows: List<ChatRow>) : ChatEntry {
        override val key: String get() = "tools-${rows.first().key}"

        val callKeys: List<String> get() = rows.mapNotNull { row ->
            (row.item as? Item.Tool)?.let { it.call.ifBlank { "row-${row.key}" } }
        }

        /** Each call with the result that answers it, in order. */
        val calls: List<Pair<Item.Tool, Item.Result?>>
            get() = rows.mapNotNull { row -> (row.item as? Item.Tool)?.let { it to row.result } }
    }
}

private fun isWork(item: Item) = item is Item.Tool || item is Item.Result || item is Item.Thinking

/**
 * `rows` with each run of work folded from its first call on; thinking
 * before that call stays its own row, as does a run with no call.
 */
fun chatEntries(rows: List<ChatRow>): List<ChatEntry> {
    val out = mutableListOf<ChatEntry>()
    val run = mutableListOf<ChatRow>()
    fun flush() {
        val first = run.indexOfFirst { it.item is Item.Tool }
        if (first < 0) {
            run.forEach { out += ChatEntry.One(it) }
        } else {
            run.subList(0, first).forEach { out += ChatEntry.One(it) }
            out += ChatEntry.Tools(run.subList(first, run.size).toList())
        }
        run.clear()
    }
    for (row in rows) {
        if (isWork(row.item)) {
            run += row
        } else {
            flush()
            out += ChatEntry.One(row)
        }
    }
    flush()
    return out
}

/** What a call does, as the folded line counts it. */
internal enum class ToolKind(val one: String, val many: String) {
    COMMAND("ran a command", "ran %d commands"),
    CREATE("created a file", "created %d files"),
    EDIT("edited a file", "edited %d files"),
    READ("read a file", "read %d files"),
    SEARCH("searched for a pattern", "searched for %d patterns"),
    FETCH("fetched a page", "fetched %d pages"),
    WEB("searched the web", "searched the web %d times"),
    AGENT("ran an agent", "ran %d agents"),
    OTHER("used a tool", "used %d tools"),
}

/** Claude's, Codex's and Antigravity's tool names, by what they do. */
internal fun toolKind(name: String): ToolKind {
    val n = name.lowercase(Locale.ROOT)
    return when {
        n == "write" || n == "write_to_file" -> ToolKind.CREATE
        n == "edit" || n == "multiedit" || n == "notebookedit" || n.contains("patch") || n.contains("replace") ->
            ToolKind.EDIT
        n == "read" || n == "notebookread" || n.contains("view") -> ToolKind.READ
        n == "websearch" || n == "web_search" -> ToolKind.WEB
        n.contains("fetch") || n.contains("web") || n.contains("url") -> ToolKind.FETCH
        n == "grep" || n == "glob" || n.contains("search") || n.contains("find") -> ToolKind.SEARCH
        n == "task" || n.contains("agent") -> ToolKind.AGENT
        n.startsWith("bash") || n.contains("shell") || n.contains("exec") || n.contains("command") -> ToolKind.COMMAND
        else -> ToolKind.OTHER
    }
}

private fun fileName(path: String) = path.trimEnd('/').substringAfterLast('/').ifEmpty { path }

/** A lone call in words: what it ran, read or changed. */
private fun lone(tool: Item.Tool): String {
    val s = tool.summary.trim()
    val kind = toolKind(tool.name)
    if (s.isEmpty() && tool.label.isNullOrBlank()) {
        return if (kind == ToolKind.OTHER) "Used ${tool.name}" else kind.one.capitalized()
    }
    return when (kind) {
        ToolKind.COMMAND -> tool.label?.takeIf { it.isNotBlank() } ?: "Ran $s"
        ToolKind.CREATE -> "Created ${fileName(s)}"
        ToolKind.EDIT -> "Edited ${fileName(s)}"
        ToolKind.READ -> "Read ${fileName(s)}"
        ToolKind.SEARCH -> "Searched for $s"
        ToolKind.FETCH -> "Fetched $s"
        ToolKind.WEB -> "Searched the web for $s"
        ToolKind.AGENT -> s
        ToolKind.OTHER -> "Used ${tool.name}"
    }
}

private fun String.capitalized() = replaceFirstChar { it.titlecase(Locale.ROOT) }

/**
 * The folded line's words: a lone call says what it did; several are
 * counted by kind, in the order they first came, each with how many failed.
 */
fun toolGroupTitle(calls: List<Pair<Item.Tool, Item.Result?>>): String {
    if (calls.size == 1) {
        val (tool, result) = calls[0]
        return lone(tool) + if (result?.error == true) " (failed)" else ""
    }
    val counts = LinkedHashMap<ToolKind, IntArray>()
    for ((tool, result) in calls) {
        val c = counts.getOrPut(toolKind(tool.name)) { IntArray(2) }
        c[0]++
        if (result?.error == true) c[1]++
    }
    return counts.entries.joinToString(", ") { (kind, c) ->
        (if (c[0] == 1) kind.one else kind.many.format(Locale.ROOT, c[0])) + if (c[1] > 0) " (${c[1]} failed)" else ""
    }.capitalized()
}

/** The lines the calls' file edits add and remove together, when any says. */
fun toolGroupDiff(calls: List<Item.Tool>): Lines? {
    val diffs = calls.mapNotNull { it.diff }
    if (diffs.isEmpty()) return null
    return Lines(diffs.sumOf { it.added }, diffs.sumOf { it.removed })
}

/** A run of work as one grey line, opening to its calls' own cards. */
@Composable
fun ToolGroupRow(group: ChatEntry.Tools, live: Boolean, tool: String, sessionId: Long, answerEnabled: Boolean) {
    var open by rememberSaveable(group.key) { mutableStateOf(false) }
    ToolGroupRow(group, live, tool, sessionId, answerEnabled, open = open, onToggle = { open = !open })
}

@Composable
fun ToolGroupRow(
    group: ChatEntry.Tools, live: Boolean, tool: String, sessionId: Long, answerEnabled: Boolean,
    open: Boolean, onToggle: () -> Unit,
) {
    val calls = group.calls
    val title = remember(calls) { toolGroupTitle(calls) }
    val diff = remember(calls) { toolGroupDiff(calls.map { it.first }) }
    val running = live && calls.lastOrNull()?.second == null
    val dark = LocalDarkTheme.current
    val rotation by animateFloatAsState(if (open) 180f else 0f, tween(180), label = "steps disclosure")
    Column(Modifier.fillMaxWidth().animateContentSize(tween(180))) {
        Row(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = ChatMargin - 4.dp, vertical = 2.dp)
                .clip(RoundedCornerShape(10.dp))
                .clickable(onClick = onToggle)
                .heightIn(min = 48.dp)
                .padding(start = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                title,
                style = MaterialTheme.typography.bodyMedium,
                color = deck.textSecondary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f, fill = false),
            )
            if (diff != null) {
                Text(
                    " +${diff.added}",
                    style = MaterialTheme.typography.bodyMedium,
                    color = if (dark) Green else LightGreen,
                )
                Text(
                    " −${diff.removed}",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.error,
                )
            }
            if (running) {
                CircularProgressIndicator(
                    Modifier.padding(start = 8.dp).size(12.dp),
                    strokeWidth = 1.5.dp,
                    color = deck.textSecondary,
                )
            }
            IconButton(onClick = onToggle, modifier = Modifier.size(48.dp)) {
                Icon(
                    DeckIcons.ChevronDown,
                    contentDescription = if (open) "Hide steps" else "Show steps",
                    modifier = Modifier.size(20.dp).graphicsLayer { rotationZ = rotation },
                    tint = deck.textSecondary,
                )
            }
        }
        if (open) {
            group.rows.forEach { row ->
                key((row.item as? Item.Tool)?.call?.ifBlank { null } ?: row.key) {
                    ChatItem(row, live, tool, sessionId, answerEnabled)
                }
            }
        }
    }
}
