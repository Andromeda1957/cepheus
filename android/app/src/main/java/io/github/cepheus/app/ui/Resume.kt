package io.github.cepheus.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Search
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.ListItem
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.cepheus.app.Deck
import io.github.cepheus.app.net.Link
import io.github.cepheus.app.protocol.Conversation
import io.github.cepheus.app.protocol.Tools
import kotlinx.coroutines.launch

/**
 * The conversations every word of `filter` finds: in the title, as the start
 * of the id, or as the program ("codex", "agy"), as the deck's Alt+r filters.
 */
fun conversationMatches(items: List<Conversation>, filter: String): List<Conversation> {
    val words = filter.lowercase().split(Regex("\\s+")).filter { it.isNotEmpty() }
    return items.filter { c ->
        val title = c.title.lowercase()
        val id = c.id.lowercase() // opencode ids mix cases
        val program = Tools.label(c.tool).lowercase()
        words.all { w ->
            title.contains(w) || id.startsWith(w) || c.host?.lowercase()?.contains(w) == true || program.startsWith(w) ||
                (c.tool == "antigravity" && "agy".startsWith(w))
        }
    }
}








@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ResumeSheet(onDismiss: () -> Unit, onResumed: (Long) -> Unit) {
    val link by Deck.link.collectAsStateWithLifecycle()
    val scope = rememberCoroutineScope()
    var items by remember { mutableStateOf<List<Conversation>?>(null) }
    var listError by remember { mutableStateOf<String?>(null) }
    var resumeError by remember { mutableStateOf<String?>(null) }
    var resuming by remember { mutableStateOf<Conversation?>(null) }
    var attempt by remember { mutableIntStateOf(0) }
    var filter by rememberSaveable { mutableStateOf("") }
    val online = link == Link.Online
    LaunchedEffect(online, attempt) {
        if (!online || items != null) return@LaunchedEffect
        listError = null
        Deck.conversations().onSuccess { items = it }.onFailure { listError = it.message }
    }
    val resume: (Conversation) -> Unit = { c ->
        resuming = c
        resumeError = null
        scope.launch {
            Deck.resume(c)
                .onSuccess { id ->
                    resuming = null
                    onResumed(id)
                }
                .onFailure {
                    resuming = null
                    resumeError = it.message
                }
        }
    }

    ModalBottomSheet(
        onDismissRequest = { if (!(resuming != null)) onDismiss() },
        sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true, confirmValueChange = { !(resuming != null) || it != androidx.compose.material3.SheetValue.Hidden }),
        properties = sheetWindowProperties(),
    ) {
        Column(
            Modifier.fillMaxWidth().imePadding().padding(horizontal = 20.dp).padding(bottom = 16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text("Resume a conversation", style = MaterialTheme.typography.titleLarge)
            Text(
                "Pick up an earlier Claude, Codex, Antigravity, Grok or opencode conversation in a session of its own.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            OutlinedTextField(
                value = filter,
                onValueChange = { filter = it },
                placeholder = { Text("Filter by title, id or program") },
                leadingIcon = { Icon(Icons.Filled.Search, contentDescription = null) },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            resumeError?.let { Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
            val all = items
            when {
                listError != null -> Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        listError.orEmpty(),
                        color = MaterialTheme.colorScheme.error,
                        style = MaterialTheme.typography.bodyMedium,
                        modifier = Modifier.weight(1f),
                    )
                    TextButton(onClick = { attempt++ }) { Text("Try again") }
                }
                all == null -> Row(verticalAlignment = Alignment.CenterVertically) {
                    CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
                    Text(
                        if (online) "Reading the deck's conversations…" else "Waiting for the deck…",
                        style = MaterialTheme.typography.bodyMedium,
                        modifier = Modifier.padding(start = 12.dp),
                    )
                }
                else -> {
                    val shown = conversationMatches(all, filter)
                    if (shown.isEmpty()) {
                        Text(
                            if (all.isEmpty()) "No earlier conversations in the deck's folder." else "No conversation matches.",
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            modifier = Modifier.padding(vertical = 16.dp),
                        )
                    }
                    LazyColumn(Modifier.fillMaxWidth().heightIn(max = 520.dp)) {
                        items(shown, key = { "${it.host.orEmpty()}-${it.tool}-${it.id}" }) { c ->
                            ConversationRow(
                                c,
                                busy = resuming == c,
                                enabled = resuming == null && online,
                                onClick = { resume(c) },
                            )
                        }
                    }
                }
            }
        }
    }
}

/** One conversation as a filled card: its title, then the program, its age and whether the deck has it open. */
@Composable
private fun ConversationRow(c: Conversation, busy: Boolean, enabled: Boolean, onClick: () -> Unit) {
    val details = listOfNotNull(
        Tools.label(c.tool),
        c.host,
        ago(c.updatedAt).takeIf { it.isNotEmpty() }?.let { if (it == "now") "just now" else "$it ago" },
        if (c.openIn != null) "open in the deck" else null,
    ).joinToString(" · ")
    Surface(
        onClick = onClick,
        enabled = enabled,
        shape = RoundedCornerShape(18.dp),
        // The sheet itself is surfaceContainerLow; the same colour hid the tile
        // (emulator screenshot, 2026-10-31).
        color = MaterialTheme.colorScheme.surfaceContainerHigh,
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 4.dp)
            .semantics { contentDescription = "Resume ${c.title}, $details" },
    ) {
        ListItem(
            headlineContent = {
                Text(c.title, style = MaterialTheme.typography.titleMedium, maxLines = 2, overflow = TextOverflow.Ellipsis)
            },
            supportingContent = { Text(details, maxLines = 1, overflow = TextOverflow.Ellipsis) },
            trailingContent = if (busy) {
                { CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp) }
            } else {
                null
            },
            colors = ListItemDefaults.colors(containerColor = Color.Transparent),
        )
    }
}
