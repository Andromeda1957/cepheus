package io.github.cepheus.app.ui

import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.cepheus.app.Deck
import io.github.cepheus.app.net.Link

val LocalConnections = staticCompositionLocalOf<() -> Unit> { {} }

/** Local pairing data is available without contacting the currently selected gateway. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ConnectionsScreen(onBack: () -> Unit, onAdd: () -> Unit) {
    val connections by Deck.connections.collectAsStateWithLifecycle()
    val active by Deck.connection.collectAsStateWithLifecycle()
    val link by Deck.link.collectAsStateWithLifecycle()
    var error by remember { mutableStateOf<String?>(null) }
    Scaffold(topBar = {
        TopAppBar(title = { Text("Saved connections") }, navigationIcon = { BackButton(onBack) })
    }) { padding ->
        LazyColumn(Modifier.padding(padding).fillMaxSize().selectableGroup(), contentPadding = PaddingValues(vertical = 16.dp)) {
            item {
                Text("Choose the machine this phone connects to. Each connection works independently; " +
                    "you can switch even when another machine is unreachable.",
                    Modifier.padding(horizontal = 24.dp, vertical = 8.dp),
                    style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            items(connections, key = { it.id }) { connection ->
                val selected = connection.id == active?.id
                ListItem(
                    headlineContent = { Text(connection.name) },
                    supportingContent = { Column {
                        Text("${connection.credentials.host}:${connection.credentials.port}")
                        if (selected) Text(when (link) {
                            Link.Online -> "Connected"
                            Link.Connecting -> "Connecting…"
                            is Link.Denied -> "Sign-in refused"
                            else -> "Offline"
                        })
                    } },
                    leadingContent = { RadioButton(selected = selected, onClick = null) },
                    modifier = Modifier.selectable(selected = selected, role = Role.RadioButton, onClick = {
                        error = Deck.selectConnection(connection.id)
                        if (error == null) onBack()
                    }),
                )
            }
            error?.let { message -> item {
                Text(message, Modifier.padding(24.dp), color = MaterialTheme.colorScheme.error)
            } }
            item {
                Button(onClick = onAdd, modifier = Modifier.fillMaxWidth().padding(24.dp), colors = accentButtonColors()) {
                    Text("Add connection")
                }
            }
        }
    }
}
