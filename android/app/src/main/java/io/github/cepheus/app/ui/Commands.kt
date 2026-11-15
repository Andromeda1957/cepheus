package io.github.cepheus.app.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.cepheus.app.protocol.CommandEntry

data class CommandQuery(val start: Int, val end: Int, val prefix: Char, val filter: String)

/** Complete the token at the cursor, including skills in the middle of a prompt. */
fun commandQuery(value: TextFieldValue): CommandQuery? {
    if (!value.selection.collapsed) return null
    val cursor = value.selection.end
    val start = value.text.take(cursor).indexOfLast(Char::isWhitespace) + 1
    val before = value.text.substring(start, cursor)
    if (before.isEmpty() || before.first() !in "/$") return null
    if (before.drop(1).any { !it.isLetterOrDigit() && it !in "-_.:" }) return null
    val end = (cursor until value.text.length).firstOrNull { value.text[it].isWhitespace() } ?: value.text.length
    return CommandQuery(start, end, before.first(), before.drop(1))
}

fun commandMatches(entries: List<CommandEntry>, query: CommandQuery): List<CommandEntry> =
    entries.filter {
        (query.prefix != '$' || it.kind == "skill") &&
            (it.name.drop(1).contains(query.filter, ignoreCase = true) ||
                it.description.contains(query.filter, ignoreCase = true))
    }.sortedWith(compareBy<CommandEntry> { !it.name.drop(1).startsWith(query.filter, ignoreCase = true) }.thenBy { it.name })

fun insertCommand(value: TextFieldValue, query: CommandQuery, entry: CommandEntry): TextFieldValue {
    val suffix = value.text.substring(query.end)
    val insert = entry.insert + if (suffix.startsWith(' ')) "" else " "
    val text = value.text.substring(0, query.start) + insert + suffix
    val cursor = query.start + insert.length + if (suffix.startsWith(' ')) 1 else 0
    return TextFieldValue(text, TextRange(cursor))
}

@Composable
fun CommandSuggestions(
    entries: List<CommandEntry>,
    query: CommandQuery,
    loading: Boolean,
    error: String?,
    online: Boolean,
    onRetry: () -> Unit,
    onPick: (CommandEntry) -> Unit,
    modifier: Modifier = Modifier,
) {
    val matches = commandMatches(entries, query)
    val scroll = rememberLazyListState()
    LaunchedEffect(query.filter, query.prefix) { scroll.scrollToItem(0) }
    // A raised card over the composer, as the Claude app lists its slash
    // commands: the name in the interface sans, its description quieter.
    Floating(modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp), shape = RoundedCornerShape(22.dp)) {
        Column(Modifier.padding(vertical = 6.dp)) {
            Row(Modifier.fillMaxWidth().padding(start = 18.dp, end = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(
                    if (loading) "Loading skills and commands…" else "Skills and commands · ${matches.size}",
                    style = MaterialTheme.typography.labelMedium,
                    color = deck.textTertiary,
                    modifier = Modifier.weight(1f).padding(vertical = 10.dp),
                )
                if (online && error != null) TextButton(onClick = onRetry) { Text("Retry") }
            }
            val note = Modifier.padding(start = 18.dp, end = 18.dp, bottom = 12.dp)
            when {
                !online -> Text("Reconnect to load skills and commands.", note, color = deck.textSecondary)
                error != null -> Text(error, note, style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
                !loading && matches.isEmpty() -> Text("No matching skills or commands.", note, color = deck.textSecondary)
                else -> LazyColumn(state = scroll, modifier = Modifier.heightIn(max = 220.dp)) {
                    items(matches, key = { it.name }) { entry ->
                        Column(
                            Modifier.fillMaxWidth().clickable { onPick(entry) }.padding(horizontal = 18.dp, vertical = 9.dp),
                        ) {
                            Text(entry.name, color = deck.text, style = MaterialTheme.typography.titleSmall)
                            Text(
                                entry.description,
                                maxLines = 2,
                                overflow = TextOverflow.Ellipsis,
                                style = MaterialTheme.typography.bodySmall,
                                color = deck.textSecondary,
                            )
                        }
                    }
                }
            }
        }
    }
}
