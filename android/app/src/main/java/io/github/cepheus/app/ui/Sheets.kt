package io.github.cepheus.app.ui

import androidx.compose.animation.animateColorAsState
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Tune
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.ModalBottomSheetProperties
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.cepheus.app.Deck
import io.github.cepheus.app.net.Link
import io.github.cepheus.app.protocol.StartHost
import io.github.cepheus.app.protocol.ToolModels
import io.github.cepheus.app.protocol.Tools
import kotlinx.coroutines.launch

/** A modal sheet owns a window, so it must use the app's system-bar theme too. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun sheetWindowProperties(): ModalBottomSheetProperties {
    val lightBars = !LocalDarkTheme.current
    return ModalBottomSheetProperties(
        isAppearanceLightStatusBars = lightBars,
        isAppearanceLightNavigationBars = lightBars,
    )
}












@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun NewSessionSheet(onDismiss: () -> Unit, onStarted: (Long) -> Unit, onResume: () -> Unit, snackbar: SnackbarHostState, host: StartHost? = null, cwd: String = "") {
    val models by Deck.models.collectAsStateWithLifecycle()
    val link by Deck.link.collectAsStateWithLifecycle()
    val state by Deck.state.collectAsStateWithLifecycle()
    val scope = rememberCoroutineScope()
    val haptics = LocalHapticFeedback.current
    val hosts = state?.startHosts.orEmpty()
    val last = remember { Deck.lastStart }
    var picked by rememberSaveable {
        // An explicit target, else the last machine chosen, else this one.

        mutableStateOf(ToolForms.initialStartHost(hosts, host, last.first))
    }
    val where = ToolForms.startHost(hosts, picked)
    val unavailable = state?.hosts?.firstOrNull { it.name == picked }
    val tools = if (models.isEmpty()) Tools.all else models.filterNot { it.cloud }.map { it.tool }
    var tool by rememberSaveable { mutableStateOf(last.second.takeIf { it.isNotBlank() } ?: "claude") }
    var options by rememberSaveable(stateSaver = StartOptions.Saver) { mutableStateOf(StartOptions(directory = cwd)) }
    var advanced by rememberSaveable { mutableStateOf(false) }
    var starting by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(Unit) { Deck.fetchModels() }
    LaunchedEffect(tools) { if (models.isNotEmpty() && tool !in tools) tool = tools.firstOrNull().orEmpty() }
    val choices = models.firstOrNull { it.tool == tool }
    val start: () -> Unit = start@{
        if (starting || tool !in tools) return@start
        haptics.performHapticFeedback(HapticFeedbackType.Confirm)
        starting = true
        error = null
        val chosen = options.forTool(choices)
        scope.launch {
            val result = if (chosen.name.isNotBlank() || chosen.directory.isNotBlank()) {
                Deck.control("create", ToolForms.create(tool, where?.name, chosen.directory, chosen.name, chosen.model, chosen.effort, chosen.message))
                    .mapCatching { data -> data.optLong("id").also { require(it > 0) { "The deck did not return a session." } } }
            } else Deck.newSession(tool, chosen.model, chosen.effort, chosen.message, where?.name)
            result
                .onSuccess { id ->
                    starting = false
                    Deck.lastStart = where?.name to tool
                    onStarted(id)
                }
                .onFailure {
                    starting = false
                    error = it.message
                }
        }
    }

    ModalBottomSheet(
        onDismissRequest = { if (!(starting)) onDismiss() },
        sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true, confirmValueChange = { !(starting) || it != androidx.compose.material3.SheetValue.Hidden }),
        properties = sheetWindowProperties(),
    ) {
        Column(
            Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 20.dp)
                .padding(bottom = 20.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text("New session", style = MaterialTheme.typography.titleLarge, modifier = Modifier.weight(1f))
                TextButton(onClick = onResume, enabled = !starting) { Text("Resume earlier…") }
            }
            if (hosts.size > 1 || where == null) {
                Section("Machine")
                ChoiceGrid(hosts, columns = hosts.size.coerceIn(1, 3), selected = { it == where }, label = { it.label }) {
                    haptics.performHapticFeedback(HapticFeedbackType.SegmentTick)
                    picked = it.name
                }
                if (where == null) Text(
                    "${unavailable?.label ?: picked ?: "This machine"} is offline. " +
                            (unavailable?.reason ?: "Reconnect it, or choose another machine."),
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodyMedium,
                )
            }
            Section("Agent")
            ChoiceGrid(tools, columns = 3, selected = { it == tool }, label = { models.firstOrNull { model -> model.tool == it }?.displayName() ?: Tools.label(it) }) {
                haptics.performHapticFeedback(HapticFeedbackType.SegmentTick)
                tool = it
            }
            Text(
                options.summary(choices),
                style = MaterialTheme.typography.bodyMedium,
                color = deck.textSecondary,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.padding(top = 2.dp),
            )
            error?.let { Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodyMedium) }
            Row(
                Modifier.padding(top = 6.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                OutlinedButton(
                    onClick = { advanced = true },
                    enabled = !starting,
                    modifier = Modifier.height(52.dp),
                    shape = RoundedCornerShape(16.dp),
                ) {
                    Icon(Icons.Outlined.Tune, contentDescription = null, modifier = Modifier.size(18.dp))
                    Text(if (options.changed) "Advanced · ${options.changes}" else "Advanced", Modifier.padding(start = 8.dp))
                }
                Button(
                    enabled = !starting && tool in tools && link == Link.Online && where != null,
                    onClick = start,
                    colors = accentButtonColors(),
                    shape = RoundedCornerShape(16.dp),
                    modifier = Modifier.weight(1f).height(52.dp),
                ) {
                    if (starting) {
                        CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp, color = deck.onAccent)
                        Text("Starting…", Modifier.padding(start = 10.dp))
                    } else {
                        // The agent and machine are the lit buttons above; Start says only what it does.
                        Text("Start", style = MaterialTheme.typography.titleMedium)
                    }
                }
            }
            if (link != Link.Online) Text(
                "Waiting for the deck connection…",
                style = MaterialTheme.typography.bodySmall,
                color = deck.textTertiary,
            )
        }
    }
    if (advanced) AdvancedStartDialog(
        tool = tool,
        choices = choices,
        initial = options,
        onDismiss = { advanced = false },
        onDone = {
            options = it
            advanced = false
        },
    )
}

/** What Advanced changes; blank or null keeps the program's own default. */
data class StartOptions(
    val model: String? = null,
    val effort: String? = null,
    val name: String = "",
    val directory: String = "",
    val message: String = "",
) {
    /** How many choices differ from the defaults, for Advanced's label. */
    val changes: Int
        get() = listOf(model != null, effort != null, name.isNotBlank(), directory.isNotBlank(), message.isNotBlank()).count { it }
    val changed: Boolean get() = changes > 0

    /** These options for `tool`: a model or effort it does not offer falls back to its default. */
    fun forTool(tool: ToolModels?): StartOptions {
        val m = model?.takeIf { id -> tool?.models?.any { it.id == id } == true }
        val e = effort?.takeIf { it in tool?.effortsFor(m).orEmpty() }
        return copy(model = m, effort = e, message = message.trim())
    }

    /** One line saying what Start starts with, including its model, effort and folder. */
    fun summary(tool: ToolModels?): String {
        val chosen = forTool(tool)
        val model = chosen.model?.let { id -> tool?.models?.firstOrNull { it.id == id }?.label ?: id }
            ?: tool?.defaultModel?.let { id -> tool.models.firstOrNull { it.id == id }?.label ?: id }
        val effort = (chosen.effort ?: tool?.defaultEffort)?.let { "$it effort" }
        val folder = chosen.directory.trim().trimEnd('/').substringAfterLast('/').takeIf { it.isNotBlank() }?.let { "in $it" }
        val parts = listOfNotNull(model, effort, folder, chosen.name.trim().takeIf { it.isNotBlank() }?.let { "“$it”" })
        val base = parts.ifEmpty { listOf("Its own default model") }.joinToString(" · ")
        return if (chosen.message.isNotBlank()) "$base · sends your first message" else base
    }

    companion object {
        val Saver = androidx.compose.runtime.saveable.listSaver<StartOptions, String?>(
            save = { listOf(it.model, it.effort, it.name, it.directory, it.message) },
            restore = { StartOptions(it[0], it[1], it[2].orEmpty(), it[3].orEmpty(), it[4].orEmpty()) },
        )
    }
}

/** Model, effort, name, folder and first message: everything New session keeps out of the way. */
@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun AdvancedStartDialog(
    tool: String,
    choices: ToolModels?,
    initial: StartOptions,
    onDismiss: () -> Unit,
    onDone: (StartOptions) -> Unit,
) {
    var model by remember { mutableStateOf(initial.model?.takeIf { id -> choices?.models?.any { it.id == id } == true }) }
    var effort by remember { mutableStateOf(initial.effort) }
    var name by remember { mutableStateOf(initial.name) }
    var directory by remember { mutableStateOf(initial.directory) }
    var message by remember { mutableStateOf(initial.message) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("${Tools.label(tool)} options") },
        text = {
            Column(
                Modifier.verticalScroll(rememberScrollState()).imePadding(),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                ModelAndEffort(choices, model, effort, onModel = {
                    model = it
                    if (effort != null && effort !in choices?.effortsFor(it).orEmpty()) effort = null
                }, onEffort = { effort = it })
                OutlinedTextField(value = name, onValueChange = { name = it }, label = { Text("Session name") }, singleLine = true, modifier = Modifier.fillMaxWidth())
                OutlinedTextField(value = directory, onValueChange = { directory = it }, label = { Text("Folder (absolute path)") }, singleLine = true, modifier = Modifier.fillMaxWidth())
                OutlinedTextField(
                    value = message,
                    onValueChange = { message = it },
                    label = { Text("First message") },
                    minLines = 2,
                    maxLines = 6,
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        },
        confirmButton = {
            TextButton(onClick = { onDone(StartOptions(model, effort, name, directory, message)) }) { Text("Done") }
        },
        dismissButton = {
            TextButton(onClick = { onDone(StartOptions()) }) { Text("Reset") }
        },
    )
}

/**
 * Equal-width choice buttons in rows of `columns`: a filled slate one with a
 * blue edge is chosen. Big enough to hit without looking twice.
 */
@Composable
internal fun <T> ChoiceGrid(
    items: List<T>,
    columns: Int,
    selected: (T) -> Boolean,
    label: (T) -> String,
    onPick: (T) -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        for (row in items.chunked(columns)) {
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                for (item in row) {
                    ChoiceButton(label(item), selected(item), { onPick(item) }, Modifier.weight(1f))
                }
                repeat(columns - row.size) { Spacer(Modifier.weight(1f)) }
            }
        }
    }
}

@Composable
private fun ChoiceButton(label: String, selected: Boolean, onClick: () -> Unit, modifier: Modifier = Modifier) {
    val colors = deck
    val fill by animateColorAsState(if (selected) colors.selected else colors.control, label = "choice fill")
    val edge by animateColorAsState(if (selected) MaterialTheme.colorScheme.primary else Color.Transparent, label = "choice edge")
    Surface(
        onClick = onClick,
        shape = RoundedCornerShape(14.dp),
        color = fill,
        contentColor = colors.text,
        border = BorderStroke(1.5.dp, edge),
        modifier = modifier.height(52.dp).semantics { this.selected = selected },
    ) {
        Box(contentAlignment = Alignment.Center, modifier = Modifier.padding(horizontal = 8.dp)) {
            Text(
                label,
                style = MaterialTheme.typography.labelLarge,
                fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Medium,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

/** Switch a running session's model and effort. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ModelSheet(
    tool: ToolModels,
    currentModel: String?,
    currentEffort: String?,
    onDismiss: () -> Unit,
    onApply: (String?, String?) -> Unit,
) {
    var model by remember { mutableStateOf(currentModel?.takeIf { m -> tool.models.any { it.id == m } }) }
    var effort by remember { mutableStateOf(currentEffort) }
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true),
        properties = sheetWindowProperties(),
    ) {
        Column(
            Modifier.fillMaxWidth().verticalScroll(rememberScrollState()).padding(horizontal = 20.dp).padding(bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text("Model", style = MaterialTheme.typography.titleLarge)
            Text(
                "Now: " + modelLabel(currentModel, currentEffort, tool, tool.tool),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            ModelAndEffort(tool, model, effort, onModel = {
                model = it
                if (effort != null && effort !in tool.effortsFor(it)) effort = null
            }, onEffort = { effort = it })
            Row {
                Text("", Modifier.weight(1f))
                Button(
                    enabled = model != null || effort != null,
                    onClick = { onApply(model, effort) },
                    colors = accentButtonColors(),
                ) { Text("Switch") }
            }
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun ModelAndEffort(
    tool: ToolModels?,
    model: String?,
    effort: String?,
    onModel: (String?) -> Unit,
    onEffort: (String?) -> Unit,
) {
    if (tool != null && tool.models.isNotEmpty()) {
        Section("Model")
        FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            FilterChip(selected = model == null, onClick = { onModel(null) }, label = { Text(tool.defaultModelLabel()) })
            for (m in tool.models) {
                FilterChip(selected = m.id == model, onClick = { onModel(m.id) }, label = { Text(m.label) })
            }
        }
    }
    val efforts = tool?.effortsFor(model).orEmpty()
    if (efforts.isNotEmpty()) {
        Section("Effort")
        FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            FilterChip(
                selected = effort == null,
                onClick = { onEffort(null) },
                label = { Text(tool?.defaultEffortLabel() ?: "Default") },
            )
            for (e in efforts) {
                FilterChip(selected = e == effort, onClick = { onEffort(e) }, label = { Text(e) })
            }
        }
    }
}

@Composable
private fun Section(text: String) {
    Text(text, style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.onSurfaceVariant)
}
