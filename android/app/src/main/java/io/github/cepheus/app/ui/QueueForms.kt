package io.github.cepheus.app.ui

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.cepheus.app.Deck
import io.github.cepheus.app.protocol.QueueContracts
import io.github.cepheus.app.protocol.StartHost
import io.github.cepheus.app.protocol.Tools
import org.json.JSONArray
import org.json.JSONObject

/** One explicit execution form serves new work, source imports and registered chores. */
@Composable
internal fun QueueForm(
    requestKey: String, enabled: Boolean, pending: String?, source: QueueContracts.SourceRef?,
    initialDependency: QueueContracts.Dependency?, chore: Boolean, onBack: () -> Unit, onDiscard: () -> Unit, onSubmit: (JSONObject) -> Unit,
) {
    val state by Deck.state.collectAsStateWithLifecycle()
    var title by rememberSaveable { mutableStateOf("") }
    var kind by rememberSaveable { mutableStateOf("job") }
    var program by rememberSaveable { mutableStateOf("") }
    var arguments by rememberSaveable { mutableStateOf("") }
    var vector by rememberSaveable { mutableStateOf("") }
    var advanced by rememberSaveable { mutableStateOf(false) }
    var prompt by rememberSaveable { mutableStateOf("") }
    var provider by rememberSaveable { mutableStateOf<String?>(null) }
    var model by rememberSaveable { mutableStateOf("") }
    var cwd by rememberSaveable { mutableStateOf("") }
    var placement by rememberSaveable { mutableStateOf("auto") }
    var priority by rememberSaveable { mutableStateOf("0") }
    var budget by rememberSaveable { mutableStateOf("") }
    var dependencyText by rememberSaveable { mutableStateOf(initialDependency?.let { JSONArray().put(it.json()).toString() } ?: "[]") }
    var dependencyQueue by rememberSaveable { mutableStateOf("") }
    var dependencySource by rememberSaveable { mutableStateOf("") }
    var dependencyTask by rememberSaveable { mutableStateOf("") }
    var condition by rememberSaveable { mutableStateOf("ready") }
    var choreId by rememberSaveable { mutableStateOf("") }
    var owner by rememberSaveable { mutableStateOf("") }
    var repository by rememberSaveable { mutableStateOf("") }
    var cron by rememberSaveable { mutableStateOf("0 9 * * *") }
    var timezone by rememberSaveable { mutableStateOf("local") }
    var target by rememberSaveable { mutableStateOf("job") }
    var error by remember { mutableStateOf<String?>(null) }
    val editable = enabled && pending == null
    val hosts = listOf(StartHost("auto", "Auto")) + state?.startHosts.orEmpty().map { it.copy(name = it.name ?: "local") }
    val dependencies = remember(dependencyText) { JSONArray(dependencyText).let { array -> (0 until array.length()).map { QueueContracts.dependency(array.getJSONObject(it)) } } }
    fun saveDependency(value: QueueContracts.Dependency) {
        dependencyText = JSONArray((dependencies + value).distinct().map { it.json() }).toString()
    }
    LazyColumn(Modifier.fillMaxSize().imePadding().padding(horizontal = 16.dp), verticalArrangement = Arrangement.spacedBy(12.dp), contentPadding = PaddingValues(vertical = 12.dp)) {
        item {
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                TextButton(onClick = onBack, enabled = enabled) { Text("Back to queue") }
                TextButton(onClick = onDiscard, enabled = editable) { Text("Discard draft") }
            }
            Text("Back keeps this draft. Resume or discard it before starting another item.", style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
            Text(if (chore) "Register scheduled work" else "Add work", style = MaterialTheme.typography.titleMedium)
            source?.let { Text("Source: ${it.sourceId} / ${it.itemId}") }
            Text("Choose the executable work explicitly. Source titles and next steps are reference text.", style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
            if (pending != null) Text("Submission is awaiting confirmation. Retry keeps the same request and draft.", color = deck.textSecondary)
        }
        item { ToolField(title, { title = it }, "Work title", enabled = editable) }
        item { ChoiceGrid(listOf("job", "session"), columns = 2, selected = { it == kind }, label = { if (it == "job") "Headless job" else "Interactive session" }) { if (editable) kind = it } }
        if (kind == "job") {
            if (advanced) item { ToolField(vector, { vector = it }, "Argument vector (JSON)", multiline = true, enabled = editable) }
            else {
                item { ToolField(program, { program = it }, "Program", enabled = editable) }
                item { ToolField(arguments, { arguments = it }, "Arguments (one per line)", multiline = true, enabled = editable) }
            }
            item {
                Text("Arguments stay literal; no shell expansion.", style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
                TextButton(onClick = { if (!advanced) vector = jobArguments(program, arguments); advanced = !advanced }, enabled = editable) { Text(if (advanced) "Use program fields" else "Advanced argument vector") }
            }
        } else item { ToolField(prompt, { prompt = it }, "Session prompt", multiline = true, enabled = editable) }
        item {
            Text(if (kind == "job") "Output format" else "Provider", style = MaterialTheme.typography.titleSmall)
            ChoiceGrid((if (kind == "job") listOf<String?>(null) else emptyList()) + Tools.all, columns = 3,
                selected = { it == provider }, label = { it?.let(Tools::label) ?: "Plain" }) { if (editable) provider = it }
            ToolField(model, { model = it }, "Model (optional)", enabled = editable)
        }
        item { ToolField(cwd, { cwd = it }, "Working directory", enabled = editable) }
        item {
            Text("Machine", style = MaterialTheme.typography.titleSmall)
            ChoiceGrid(hosts, columns = hosts.size.coerceIn(1, 3), selected = { it.name == placement }, label = { it.label }) { if (editable) placement = it.name ?: "local" }
        }
        item {
            ToolField(priority, { priority = it }, "Priority", enabled = editable)
            ToolField(budget, { budget = it }, "Budget in USD (optional)", enabled = editable)
            Text("Budget is recorded as metadata; budget enforcement arrives with cost controls.", style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
        }
        item {
            Text("Dependencies · all must be satisfied", style = MaterialTheme.typography.titleSmall)
            dependencies.forEachIndexed { index, dependency ->
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                    Text(dependency.description, modifier = Modifier.weight(1f))
                    TextButton(onClick = { dependencyText = JSONArray(dependencies.filterIndexed { i, _ -> i != index }.map { it.json() }).toString() }, enabled = editable) { Text("Remove") }
                }
            }
            ToolField(dependencyQueue, { dependencyQueue = it }, "After queue item ID", enabled = editable)
            TextButton(onClick = {
                runCatching { QueueContracts.dependency(JSONObject().put("queue", dependencyQueue.toLongOrNull() ?: 0).put("condition", "finished")) }
                    .onSuccess { saveDependency(it); dependencyQueue = "" }.onFailure { error = it.message }
            }, enabled = editable) { Text("Add finished dependency") }
            ToolField(dependencySource, { dependencySource = it }, "Dependency source ID", enabled = editable)
            ToolField(dependencyTask, { dependencyTask = it }, "Dependency task ID", enabled = editable)
            ChoiceGrid(listOf("ready", "merged"), columns = 2, selected = { it == condition }, label = { it }) { if (editable) condition = it }
            TextButton(onClick = {
                runCatching { QueueContracts.dependency(JSONObject().put("source", dependencySource).put("task", dependencyTask).put("condition", condition)) }
                    .onSuccess { saveDependency(it); dependencySource = ""; dependencyTask = "" }.onFailure { error = it.message }
            }, enabled = editable) { Text("Add source dependency") }
            Text("Finished requires successful completion. Ready and merged are observed source states; they grant no merge authority.", style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
        }
        if (chore) item {
            Text("Schedule", style = MaterialTheme.typography.titleSmall)
            ToolField(choreId, { choreId = it }, "Chore ID", enabled = editable)
            ToolField(owner, { owner = it }, "Registered plugin owner", enabled = editable)
            ToolField(repository, { repository = it }, "Repository identity", enabled = editable)
            ChoiceGrid(if (kind == "job") listOf("job") else listOf("session", "idle"), columns = 2, selected = { it == if (kind == "job") "job" else target.takeIf { selected -> selected != "job" } ?: "session" }, label = { it }) { if (editable) target = it }
            ToolField(cron, { cron = it }, "Cron schedule (minute hour day month weekday)", enabled = editable)
            ChoiceGrid(listOf("local", "UTC"), columns = 2, selected = { it == timezone }, label = { it }) { if (editable) timezone = it }
            Text("Registers disabled. Enable the chore and queue separately to permit scheduled starts.", style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
        }
        item {
            error?.let { ErrorLine(it) }
            Button(enabled = enabled && (pending != null || hosts.any { it.name == placement }), colors = accentButtonColors(), modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp), onClick = {
                runCatching {
                    pending?.let(::JSONObject) ?: QueueContracts.spec(requestKey, title, kind, if (advanced) vector else jobArguments(program, arguments), prompt,
                        provider, model, cwd, placement, priority, dependencies, budget).let { spec ->
                        when {
                            chore -> QueueContracts.registerChore(choreId, title, owner, repository, if (kind == "job") "job" else target.takeIf { it != "job" } ?: "session", cron, timezone, spec)
                            source != null -> QueueContracts.queueSource(source, spec)
                            else -> QueueContracts.request("queue-add", spec)
                        }
                    }
                }.onSuccess(onSubmit).onFailure { error = it.message }
            }) { Text(if (pending != null) "Retry submission" else if (chore) "Register disabled chore" else "Queue work") }
        }
    }
}
