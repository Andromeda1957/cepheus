package io.github.cepheus.app.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.saveable.rememberSaveableStateHolder
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.repeatOnLifecycle
import io.github.cepheus.app.Deck
import io.github.cepheus.app.protocol.Controls
import io.github.cepheus.app.protocol.QueueContracts as Q
import kotlinx.coroutines.delay
import org.json.JSONArray
import org.json.JSONObject
import java.text.DateFormat
import java.util.Date
import java.util.UUID

@Composable
internal fun QueueScreen(online: Boolean, fleetSupported: Boolean?, onBackChange: ((() -> Unit)?) -> Unit, onSession: (Long) -> Unit, onJob: (Long) -> Unit) {
    val connection by Deck.connection.collectAsStateWithLifecycle()
    val connections = rememberSaveableStateHolder()
    connections.SaveableStateProvider(connection?.id ?: "unpaired") { QueuePage(online, fleetSupported, onBackChange, onSession, onJob) }
}

@Composable
private fun QueuePage(online: Boolean, fleetSupported: Boolean?, onBackChange: ((() -> Unit)?) -> Unit, onSession: (Long) -> Unit, onJob: (Long) -> Unit) {
    val capability = rememberToolRequest("queue-capabilities")
    val status = rememberToolRequest("queue-list")
    val sourcesRequest = rememberToolRequest("queue-sources")
    val choresRequest = rememberToolRequest("queue-chores")
    val action = rememberToolRequest("queue-action")
    var capabilities by remember { mutableStateOf<JSONObject?>(null) }
    var checked by remember { mutableStateOf(false) }
    var listing by remember { mutableStateOf<Q.Listing?>(null) }
    var sources by remember { mutableStateOf<Q.Sources?>(null) }
    var chores by remember { mutableStateOf<Q.Chores?>(null) }
    var tab by rememberSaveable { mutableStateOf("Work") }
    var form by rememberSaveable { mutableStateOf(false) }
    var choreForm by rememberSaveable { mutableStateOf(false) }
    var draftActive by rememberSaveable { mutableStateOf(false) }
    var discardDraft by remember { mutableStateOf(false) }
    var formKey by rememberSaveable { mutableStateOf("phone-" + UUID.randomUUID()) }
    var sourceId by rememberSaveable { mutableStateOf<String?>(null) }
    var sourceTask by rememberSaveable { mutableStateOf<String?>(null) }
    var initialDependency by rememberSaveable { mutableStateOf<String?>(null) }
    var pending by rememberSaveable { mutableStateOf<String?>(null) }
    var selected by rememberSaveable { mutableStateOf<Long?>(null) }
    var confirmation by remember { mutableStateOf<Pair<String, JSONObject>?>(null) }
    var scheduleEdit by remember { mutableStateOf<Q.Chore?>(null) }
    var batch by rememberSaveable { mutableStateOf(false) }
    var batchText by rememberSaveable { mutableStateOf("") }
    var batchPending by rememberSaveable { mutableStateOf<String?>(null) }
    var runKeys by rememberSaveable { mutableStateOf("{}") }
    var decodeError by remember { mutableStateOf<String?>(null) }
    val drafts = rememberSaveableStateHolder()
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    fun supports(facility: String) = capabilities?.let { Q.supported(it, facility) } == true
    val supported = supports("work-queue")
    fun decode(block: () -> Unit) { runCatching(block).onSuccess { decodeError = null }.onFailure { decodeError = it.message } }
    fun refresh() {
        if (!online || !supported) return
        status.run("fleet", Q.request("queue-list"), keep = true) { decode { listing = Q.listing(it) } }
        if (supports("task-sources")) sourcesRequest.run("fleet", Q.request("source-list"), keep = true) { decode { sources = Q.sources(it) } }
        if (supports("chores")) choresRequest.run("fleet", Q.request("chore-list"), keep = true) { decode { chores = Q.chores(it) } }
    }
    fun mutate(args: JSONObject) { action.run("fleet", args) {
        if (args.getString("operation") == "chore-run") { val keys = JSONObject(runKeys); keys.remove(args.getJSONObject("args").getString("id")); runKeys = keys.toString() }
        refresh()
    } }
    fun openForm(source: Q.SourceRef? = null, dependency: Q.Dependency? = null, scheduled: Boolean = false) {
        if (!draftActive) {
            draftActive = true
            formKey = "phone-" + UUID.randomUUID(); sourceId = source?.sourceId; sourceTask = source?.itemId
            initialDependency = dependency?.json()?.toString(); choreForm = scheduled
        }
        form = true; selected = null
    }
    LaunchedEffect(online, fleetSupported) {
        checked = false; capabilities = null
        if (online && fleetSupported == true) capability.run("fleet", Q.request("capabilities")) { capabilities = it; checked = true }
    }
    LaunchedEffect(online, supported, lifecycle) {
        if (!online || !supported) return@LaunchedEffect
        lifecycle.repeatOnLifecycle(Lifecycle.State.RESUMED) { while (true) { refresh(); delay(3000) } }
    }
    DisposableEffect(form, batch, selected, action.busy) {
        onBackChange(if (form || batch || selected != null || action.busy) ({ if (!action.busy) { form = false; batch = false; selected = null } }) else null)
        onDispose { onBackChange(null) }
    }
    BackHandler(enabled = form || batch || selected != null || action.busy) { if (!action.busy) { form = false; batch = false; selected = null } }
    if (fleetSupported == false || checked && !supported) { Text(Controls.UPDATE_NEEDED, color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(16.dp)); return }
    if (!supported) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Text(if (online) "Checking queue support…" else "Connect to the deck to inspect queued work.")
            RequestStatus(capability)
            OutlinedButton(enabled = online && !capability.busy, onClick = { capability.run("fleet", Q.request("capabilities")) { capabilities = it; checked = true } }) { Text("Check queue support") }
        }; return
    }
    val mutationEnabled = online && !action.busy
    confirmation?.let { (label, args) -> AlertDialog(
        onDismissRequest = { if (!action.busy) confirmation = null }, title = { Text(label) },
        text = { Text(when (args.getString("operation")) {
            "queue-enable" -> if (args.getJSONObject("args").getBoolean("enabled")) "Queued work and enabled schedules may start through admission. This grants no integration or shipping authority." else "Request a pause for future dispatch. Offline executors may remain pending until they acknowledge it. Running work continues."
            "queue-reorder" -> "Apply this order to every cancelable item. Existing priority differences are reset."
            "queue-cancel" -> "Cancel this pending item. Dispatched sessions and jobs are controlled separately."
            "chore-enable" -> "Change this registered chore's scheduled starts. The global queue must also be enabled."
            "chore-remove" -> "Remove this disabled registration. Daily claim and outcome receipts remain."
            else -> "Run this enabled chore once through the queue. Existing daily claims remain authoritative."
        }) },
        confirmButton = { TextButton(enabled = mutationEnabled, onClick = { mutate(args); confirmation = null }) { Text("Confirm") } },
        dismissButton = { TextButton(onClick = { confirmation = null }) { Text("Cancel") } },
    ) }
    scheduleEdit?.let { chore -> key(chore.id) { ScheduleDialog(chore, mutationEnabled, { scheduleEdit = null }) { args ->
        action.run("fleet", args) { scheduleEdit = null; refresh() }
    } } }
    if (discardDraft) AlertDialog(onDismissRequest = { discardDraft = false }, title = { Text("Discard this draft?") },
        text = { Text("Remove this unsubmitted draft so another item can be started.") },
        confirmButton = { TextButton(enabled = mutationEnabled && pending == null, onClick = {
            drafts.removeState(formKey); draftActive = false; form = false; discardDraft = false
        }) { Text("Discard draft") } }, dismissButton = { TextButton(onClick = { discardDraft = false }) { Text("Keep draft") } })
    if (form) {
        Column(Modifier.fillMaxSize()) {
            RequestStatus(action); decodeError?.let { ErrorLine(it) }
            drafts.SaveableStateProvider(formKey) {
                QueueForm(formKey, mutationEnabled, pending, sourceId?.let { Q.SourceRef(it, sourceTask.orEmpty()) },
                    initialDependency?.let { Q.dependency(JSONObject(it)) }, choreForm, onBack = { if (!action.busy) form = false }, onDiscard = { discardDraft = true }) { args ->
                    pending = args.toString()
                    action.run("fleet", args, failed = { if (it is IllegalArgumentException) pending = null }) { value -> decode {
                        if (args.getString("operation") == "chore-register") Q.choreInfo(value.getJSONObject("chore")) else Q.info(value.getJSONObject("item"))
                        pending = null; draftActive = false; form = false; drafts.removeState(formKey); refresh()
                    } }
                }
            }
        }; return
    }
    if (batch) {
        LazyColumn(Modifier.fillMaxSize().imePadding().padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            item { TextButton(enabled = !action.busy, onClick = { batch = false }) { Text("Back to queue") }; Text("Batch work", style = MaterialTheme.typography.titleMedium) }
            item { Text("Advanced: paste up to 100 full queue specifications as a JSON array. Each record needs its own stable key and explicit execution spec. The batch is atomic.") }
            item { ToolField(batchText, { batchText = it }, "Batch specifications (JSON array)", multiline = true, enabled = mutationEnabled && batchPending == null) }
            item {
                RequestStatus(action); decodeError?.let { ErrorLine(it) }
                Button(enabled = mutationEnabled, onClick = { decode {
                    val args = batchPending?.let(::JSONObject) ?: Q.batch(batchText)
                    batchPending = args.toString(); action.run("fleet", args, failed = { if (it is IllegalArgumentException) batchPending = null }) { batchPending = null; batch = false; refresh() }
                } }) { Text(if (batchPending != null) "Retry batch" else "Queue batch") }
            }
        }; return
    }
    LazyColumn(Modifier.fillMaxSize().imePadding().padding(horizontal = 16.dp), verticalArrangement = Arrangement.spacedBy(12.dp), contentPadding = PaddingValues(vertical = 12.dp)) {
        item {
            ChoiceGrid(listOf("Work", "Sources", "Chores"), columns = 3, selected = { it == tab }, label = { it }) { if (!action.busy) tab = it }
            Text(when {
                listing == null -> "Reading queue…"
                listing?.enabled == true -> "Queue dispatch enabled"
                listing?.pausePending?.isNotEmpty() == true -> "Pause requested; awaiting executor: ${listing?.pausePending?.joinToString(", ")}"
                listing?.pauseConfirmed == true -> "Queue dispatch paused"
                else -> "Pause requested; awaiting executor confirmation"
            }, style = MaterialTheme.typography.titleMedium, modifier = Modifier.padding(top = 12.dp))
            Text("Disabling holds future dispatch; it does not stop running work.", style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(enabled = mutationEnabled && listing != null, onClick = { confirmation = (if (listing?.enabled == true) "Disable queue?" else "Enable queue?") to Q.request("queue-enable", JSONObject().put("enabled", listing?.enabled != true)) }) { Text(if (listing?.enabled == true) "Disable queue" else "Enable queue") }
                OutlinedButton(enabled = online && !status.busy, onClick = { refresh() }) { Text("Refresh queue") }
            }
            RequestStatus(status); RequestStatus(action); decodeError?.let { ErrorLine(it) }
        }
        when (tab) {
            "Work" -> {
                item {
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        Button(enabled = mutationEnabled, onClick = { openForm() }, colors = accentButtonColors()) { Text(if (draftActive) "Resume draft" else "Add work") }
                        OutlinedButton(enabled = mutationEnabled, onClick = { batch = true }) { Text(if (batchPending != null) "Resume batch" else "Batch") }
                    }
                    Text("Work items · ${listing?.items?.size ?: 0}", style = MaterialTheme.typography.titleMedium)
                }
                val values = listing?.items.orEmpty()
                items(values, key = { it.id }) { item ->
                    QueueCard {
                        TextButton(onClick = { selected = if (selected == item.id) null else item.id }) { Text(item.title) }
                        Text("#${item.id} · ${item.kind} · ${item.state} · priority ${item.priority}", style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
                        if (item.reason.isNotBlank()) Text(item.reason)
                        if (selected == item.id) {
                            Text(listOfNotNull(item.provider, item.model, item.placement).joinToString(" · "))
                            Text("Updated ${queueTime(item.updatedAt)}")
                            item.source?.let { Text("Source: ${it.sourceId} / ${it.itemId}") }
                            item.budget?.let { Text("Budget: $it USD (metadata)") }
                            item.dependencies.forEach { Text("After ${it.description}") }
                            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                OutlinedButton(enabled = mutationEnabled && item.cancelable, onClick = { confirmation = "Cancel queued work?" to Q.target("queue-cancel", item.id) }) { Text("Cancel item") }
                                TextButton(enabled = mutationEnabled, onClick = { openForm(dependency = Q.Dependency(queue = item.id)) }) { Text("Queue after this") }
                            }
                            if (!item.cancelable && item.dispatch == null) Text("The service owns dispatch; cancellation is unavailable.", style = MaterialTheme.typography.bodySmall)
                            item.dispatch?.let { dispatch ->
                                OutlinedButton(enabled = online && !action.busy, onClick = { if (dispatch.kind == "job") onJob(dispatch.id) else onSession(dispatch.id) }) { Text(if (dispatch.kind == "job") "Open job" else "Open session") }
                            }
                            val movable = values.filter { it.cancelable }; val position = movable.indexOfFirst { it.id == item.id }
                            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                OutlinedButton(enabled = mutationEnabled && item.cancelable && position > 0, onClick = { confirmation = "Move item up?" to Q.reorder(values, item.id, -1) }) { Text("Move up") }
                                OutlinedButton(enabled = mutationEnabled && item.cancelable && position >= 0 && position < movable.lastIndex, onClick = { confirmation = "Move item down?" to Q.reorder(values, item.id, 1) }) { Text("Move down") }
                            }
                        }
                    }
                }
                if (values.isEmpty() && listing != null) item { Text("No queued work yet.") }
            }
            "Sources" -> {
                if (!supports("task-sources")) item { ErrorLine(Controls.UPDATE_NEEDED) }
                else {
                    item { RequestStatus(sourcesRequest); Text("Task sources are read-only. Queueing requires an explicit executable spec.", color = deck.textSecondary) }
                    items(sources?.sources.orEmpty(), key = { it.id }) { source -> QueueCard {
                        Text(source.id, style = MaterialTheme.typography.titleMedium)
                        Text("${source.owner} · ${source.repository} · ${if (source.fresh) "fresh" else "stale"}")
                        source.error?.let { ErrorLine(it) }
                        OutlinedButton(enabled = mutationEnabled && source.enabled, onClick = { action.run("fleet", Q.source("source-refresh", source.id)) { decode { sources = Q.sources(it) }; refresh() } }) { Text("Refresh source") }
                    } }
                    items(sources?.tasks.orEmpty(), key = { "${it.sourceId.length}:${it.sourceId}${it.id}" }) { task -> QueueCard {
                        Text(task.title, style = MaterialTheme.typography.titleMedium)
                        Text("${task.sourceId} / ${task.id} · ${task.state}", color = deck.textSecondary)
                        task.branch?.let { Text("Branch: $it") }; task.worktree?.let { Text("Worktree: $it") }; task.nextStep?.let { Text(it) }
                        OutlinedButton(enabled = mutationEnabled, onClick = { openForm(source = Q.SourceRef(task.sourceId, task.id)) }) { Text("Queue source task") }
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            TextButton(enabled = mutationEnabled, onClick = { openForm(dependency = Q.Dependency(source = task.sourceId, task = task.id, condition = "ready")) }) { Text("After ready") }
                            TextButton(enabled = mutationEnabled, onClick = { openForm(dependency = Q.Dependency(source = task.sourceId, task = task.id, condition = "merged")) }) { Text("After merged") }
                        }
                    } }
                    if (sources?.sources?.isEmpty() == true) item { Text("No task sources registered. Configure a source plugin on the deck.") }
                }
            }
            "Chores" -> {
                if (!supports("chores")) item { ErrorLine(Controls.UPDATE_NEEDED) }
                else {
                    item {
                        RequestStatus(choresRequest)
                        Button(enabled = mutationEnabled, onClick = { openForm(scheduled = true) }, colors = accentButtonColors()) { Text("Register scheduled work") }
                        Text("Claims and outcomes are read-only. The registered owner handles claims and completion.", style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
                    }
                    items(chores?.chores.orEmpty(), key = { it.id }) { chore -> QueueCard {
                        Text(chore.title, style = MaterialTheme.typography.titleMedium)
                        Text("${chore.id} · ${chore.target} · ${if (chore.effectiveEnabled) "active" else "inactive"}")
                        Text("${chore.cron} · ${chore.timezone} · next ${queueTime(chore.nextRun)}")
                        Text("Owner: ${chore.owner} · ${chore.repository}${if (chore.ownerLive == false) " · unavailable" else ""}")
                        chore.claim?.let { Text("Claim: ${it.day} · generation ${it.generation} · ${it.session}\nClaimed ${queueTime(it.claimedAt)} · stale ${queueTime(it.staleAt)}") }
                        chore.outcome?.let { Text("Outcome: $it") }
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            OutlinedButton(enabled = mutationEnabled && chore.registered, onClick = { confirmation = "${if (chore.enabled) "Disable" else "Enable"} chore?" to Q.chore("chore-enable", chore.id, !chore.enabled) }) { Text(if (chore.enabled) "Disable chore" else "Enable chore") }
                            OutlinedButton(enabled = mutationEnabled && chore.effectiveEnabled, onClick = {
                                val keys = JSONObject(runKeys); val key = keys.optString(chore.id).ifBlank { "phone-" + UUID.randomUUID() }; keys.put(chore.id, key); runKeys = keys.toString()
                                confirmation = "Run chore now?" to Q.chore("chore-run", chore.id, key = key)
                            }) { Text("Run now") }
                        }
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            TextButton(enabled = mutationEnabled && chore.registered, onClick = { scheduleEdit = chore }) { Text("Edit schedule") }
                            TextButton(enabled = mutationEnabled && chore.registered && !chore.enabled && (chore.claim == null || chore.outcome != null), onClick = { confirmation = "Remove chore?" to Q.chore("chore-remove", chore.id) }) { Text("Remove registration") }
                        }
                        if (listing?.enabled != true) Text("Enable the global queue separately to run chores.", style = MaterialTheme.typography.bodySmall)
                    } }
                }
            }
        }
    }
}

@Composable
private fun QueueCard(content: @Composable ColumnScope.() -> Unit) {
    Surface(color = deck.raised, shape = MaterialTheme.shapes.large, modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp), content = content)
    }
}

@Composable
private fun ScheduleDialog(chore: Q.Chore, enabled: Boolean, onDismiss: () -> Unit, onSave: (JSONObject) -> Unit) {
    var cron by rememberSaveable { mutableStateOf(chore.cron) }; var timezone by rememberSaveable { mutableStateOf(chore.timezone) }
    var error by remember { mutableStateOf<String?>(null) }
    AlertDialog(onDismissRequest = { if (enabled) onDismiss() }, title = { Text("Edit future schedule") }, text = {
        Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
            ToolField(cron, { cron = it }, "Cron schedule", enabled = enabled)
            ChoiceGrid(listOf("local", "UTC"), columns = 2, selected = { it == timezone }, label = { it }) { if (enabled) timezone = it }
            Text("Current claims are preserved."); error?.let { ErrorLine(it) }
        }
    }, confirmButton = { TextButton(enabled = enabled, onClick = {
        runCatching { Q.request("chore-update", JSONObject().put("id", chore.id).put("schedule", Q.schedule(cron, timezone))) }
            .onSuccess(onSave).onFailure { error = it.message }
    }) { Text("Save schedule") } }, dismissButton = { TextButton(enabled = enabled, onClick = onDismiss) { Text("Cancel") } })
}

private fun queueTime(seconds: Long?): String = seconds?.let {
    if (it > Long.MAX_VALUE / 1000) "Unknown time" else DateFormat.getDateTimeInstance(DateFormat.SHORT, DateFormat.SHORT).format(Date(it * 1000))
} ?: "—"
