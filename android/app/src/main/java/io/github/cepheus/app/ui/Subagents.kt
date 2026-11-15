package io.github.cepheus.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.cepheus.app.Deck
import io.github.cepheus.app.net.Link
import io.github.cepheus.app.protocol.Session
import io.github.cepheus.app.protocol.Subagent

internal fun subagentLabel(agents: List<Subagent>): String {
    val active = agents.count { it.active }
    return if (active > 0) "$active sub-agent" + (if (active == 1) "" else "s") + " active"
    else "0 active · " + agents.size + " recent sub-agent" + (if (agents.size == 1) "" else "s")
}

/** Keep counts separate from the branch line so a long branch cannot hide them. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun SubagentSummary(session: Session, details: Boolean = false) {
    if (session.subagents.isEmpty()) return
    val link by Deck.link.collectAsStateWithLifecycle()
    val state by Deck.state.collectAsStateWithLifecycle()
    val online = link == Link.Online &&
        (session.host == null || state?.hosts?.firstOrNull { it.name == session.host }?.online != false)
    var open by rememberSaveable(session.id, session.host) { mutableStateOf(false) }
    val label = (if (online) "" else "Saved: ") + subagentLabel(session.subagents)
    val color = if (online && session.subagents.any { it.active }) deck.accent else deck.textSecondary
    if (details) {
        TextButton(onClick = { open = true }, modifier = Modifier.heightIn(min = 48.dp)) {
            Text(label, color = color, style = MaterialTheme.typography.labelMedium, maxLines = 2)
        }
    } else {
        Text(label, color = color, style = MaterialTheme.typography.labelMedium,
            fontWeight = FontWeight.Medium, modifier = Modifier.padding(top = 3.dp))
    }
    if (open) {
        ModalBottomSheet(onDismissRequest = { open = false }, containerColor = deck.page) {
            Column(Modifier.fillMaxWidth().padding(horizontal = ChatMargin)) {
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically) {
                    Text("Sub-agents", color = deck.text, style = MaterialTheme.typography.titleLarge)
                    TextButton(onClick = { open = false }) { Text("Done") }
                }
                val active = session.subagents.count { it.active }
                Text("$active active · " + (session.subagents.size - active) + " recent",
                    color = deck.textSecondary, style = MaterialTheme.typography.bodyMedium)
                if (!online) {
                    Text("Saved status · offline", color = deck.textSecondary,
                        style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(top = 8.dp))
                }
                LazyColumn(
                    modifier = Modifier.fillMaxWidth()
                        .heightIn(max = LocalConfiguration.current.screenHeightDp.dp * 0.7f)
                        .padding(vertical = 16.dp),
                    verticalArrangement = Arrangement.spacedBy(20.dp),
                ) {
                    // The native metadata has no unique id; duplicate task names are valid.
                    items(session.subagents.size) { index ->
                        val agent = session.subagents[index]
                        Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                            Text(agent.name, color = deck.text, style = MaterialTheme.typography.bodyLarge,
                                fontWeight = FontWeight.Medium)
                            val status = if (agent.active) "Active" else "Recent"
                            Text(listOfNotNull(agent.kind?.takeIf { it.isNotBlank() }, status).joinToString(" · "),
                                color = if (online && agent.active) deck.accent else deck.textSecondary,
                                style = MaterialTheme.typography.labelLarge)
                            if (agent.startedAt > 0) Text("Started " + clock(agent.startedAt),
                                color = deck.textSecondary, style = MaterialTheme.typography.bodySmall)
                            if (agent.lastActive > 0) Text("Last active " + clock(agent.lastActive),
                                color = deck.textSecondary, style = MaterialTheme.typography.bodySmall)
                        }
                    }
                }
            }
        }
    }
}
