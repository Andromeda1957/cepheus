package io.github.cepheus.app.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Cloud
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.github.cepheus.app.Deck
import io.github.cepheus.app.net.Link
import io.github.cepheus.app.protocol.CloudAgent
import io.github.cepheus.app.protocol.DeckState
import kotlinx.coroutines.launch

internal fun LazyListScope.cloudAgents(state: DeckState?, drawer: Boolean, onSession: (Long) -> Unit) {
    val agents = state?.cloudAgents.orEmpty()
    if (agents.isEmpty()) return
    val count = agents.count { a -> state?.sessions?.any { it.alive && it.tool == a.id && it.host == a.host } == true }
    item(key = "cloud-agents-heading") { GroupHeading("Cloud agents", count.toString() + " on", drawer) }
    items(agents, key = { "cloud-" + it.host + "-" + it.id }) { agent ->
        CloudAgentRow(agent, requireNotNull(state), onSession)
    }
}

@Composable
private fun CloudAgentRow(agent: CloudAgent, state: DeckState, onSession: (Long) -> Unit) {
    val link by Deck.link.collectAsState()
    val active = state.sessions.firstOrNull { it.alive && it.tool == agent.id && it.host == agent.host }
    val available = link == Link.Online && (agent.host == null || state.hosts.any { it.name == agent.host && it.online })
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()
    val change: () -> Unit = {
        if (available && !busy) {
            busy = true
            error = null
            scope.launch {
                if (active != null) {
                    error = Deck.close(active.id)
                } else {
                    Deck.newSession(agent.id, null, null, null, agent.host)
                        .onSuccess(onSession)
                        .onFailure { error = it.message ?: "Could not connect." }
                }
                busy = false
            }
        }
    }
    RowCard(onClick = { if (active != null) onSession(active.id) else change() }) {
        Column(Modifier.fillMaxWidth().padding(horizontal = 14.dp, vertical = 10.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(Icons.Outlined.Cloud, contentDescription = null, tint = deck.accent)
                Spacer(Modifier.width(12.dp))
                Column(Modifier.weight(1f)) {
                    Text(agent.label, style = MaterialTheme.typography.titleMedium)
                    val host = agent.host?.takeUnless { agent.id == "dot" }?.let { name -> state.hosts.firstOrNull { it.name == name }?.label ?: name }
                    Text(listOfNotNull(if (active == null) "Off" else "On", host).joinToString(" · "),
                        style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
                }
                TextButton(onClick = change, enabled = available && !busy) {
                    Text(if (busy) "Please wait…" else if (active == null) "On" else "Off")
                }
            }
            error?.let { Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
        }
    }
}
