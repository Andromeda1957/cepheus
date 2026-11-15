package io.github.cepheus.app.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.repeatOnLifecycle
import io.github.cepheus.app.Deck
import io.github.cepheus.app.protocol.Controls
import io.github.cepheus.app.protocol.Item
import io.github.cepheus.app.protocol.Protocol
import io.github.cepheus.app.protocol.JobContracts
import io.github.cepheus.app.protocol.StartHost
import io.github.cepheus.app.protocol.Tools
import io.github.cepheus.app.store.ChatView
import kotlinx.coroutines.delay
import org.json.JSONArray
import java.text.DateFormat
import java.util.Date
import java.util.UUID
import java.security.MessageDigest

@Composable
internal fun JobsScreen(online: Boolean, fleetSupported: Boolean?, onBackChange: ((() -> Unit)?) -> Unit, initialJob: Long? = null) {
    val connection by Deck.connection.collectAsStateWithLifecycle()
    key(connection?.id) { JobsPage(online, fleetSupported, onBackChange, initialJob) }
}

@Composable
private fun JobsPage(online: Boolean, fleetSupported: Boolean?, onBackChange: ((() -> Unit)?) -> Unit, initialJob: Long?) {
    val capability = rememberToolRequest("job-capabilities")
    var checked by remember { mutableStateOf(false) }
    var supported by remember { mutableStateOf(false) }
    val status = rememberToolRequest("job-list")
    val action = rememberToolRequest("job-action")
    var jobs by remember { mutableStateOf(emptyList<JobContracts.Info>()) }
    var selected by rememberSaveable { mutableStateOf(initialJob) }
    var starting by rememberSaveable { mutableStateOf(false) }
    var decodeError by remember { mutableStateOf<String?>(null) }
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    LaunchedEffect(online, fleetSupported) {
        checked = false
        supported = false
        if (online && fleetSupported == true) {
            capability.run("fleet", JobContracts.request("capabilities")) {
                supported = JobContracts.supported(it)
                checked = true
            }
        }
    }
    fun refresh() {
        status.run("fleet", JobContracts.request("job-status"), keep = true) { value ->
            runCatching { JobContracts.jobs(value) }.onSuccess { jobs = it; decodeError = null }
                .onFailure { decodeError = it.message }
        }
    }
    LaunchedEffect(online, supported, lifecycle) {
        if (!online || !supported) return@LaunchedEffect
        lifecycle.repeatOnLifecycle(Lifecycle.State.RESUMED) {
            while (true) { refresh(); delay(3000) }
        }
    }
    DisposableEffect(selected, starting) {
        onBackChange(if (selected != null || starting) ({ if (!action.busy) { selected = null; starting = false } }) else null)
        onDispose { onBackChange(null) }
    }
    BackHandler(enabled = selected != null || starting) {
        if (!action.busy) { selected = null; starting = false }
    }
    fun act(operation: String, job: JobContracts.Info) {
        action.run("fleet", JobContracts.target(operation, job.id)) { value ->
            runCatching { JobContracts.info(value.getJSONObject("job")) }.onSuccess { updated ->
                if (operation == "job-dismiss") { selected = null; jobs = jobs.filterNot { it.id == updated.id } }
                else jobs = jobs.map { if (it.id == updated.id) updated else it }
                refresh()
            }.onFailure { decodeError = it.message }
        }
    }
    if (fleetSupported == false || (checked && !supported)) {
        Text(Controls.UPDATE_NEEDED, color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(16.dp))
        return
    }
    if (!supported) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Text(if (online) "Checking job support…" else "Connect to the deck to inspect its jobs.")
            RequestStatus(capability)
            if (online && !capability.busy) OutlinedButton(onClick = {
                capability.run("fleet", JobContracts.request("capabilities")) { supported = JobContracts.supported(it); checked = true }
            }) { Text("Check job support") }
        }
        return
    }
    val current = jobs.firstOrNull { it.id == selected }
    if (starting) {
        Column(Modifier.fillMaxSize()) {
            RequestStatus(action)
            decodeError?.let { ErrorLine(it) }
            JobStartForm(online && !action.busy, onBack = { if (!action.busy) starting = false }) { args ->
                action.run("fleet", args) { value ->
                    runCatching { JobContracts.info(value.getJSONObject("job")) }.onSuccess { job ->
                        jobs = listOf(job) + jobs.filterNot { it.id == job.id }
                        selected = job.id
                        starting = false
                        decodeError = null
                    }.onFailure { decodeError = it.message }
                }
            }
        }
    } else if (selected != null) {
        Column(Modifier.fillMaxSize()) {
            TextButton(onClick = { selected = null }) { Text("All jobs") }
            RequestStatus(action)
            decodeError?.let { ErrorLine(it) }
            if (current == null) {
                Text("This job is unavailable. Refresh the job list.", modifier = Modifier.padding(16.dp))
            } else key(current.id) {
                JobDetails(current, online, !action.busy, onStop = { act("job-stop", current) }, onDismiss = { act("job-dismiss", current) })
            }
        }
    } else {
        LazyColumn(
            Modifier.fillMaxSize().imePadding().padding(horizontal = 16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
            contentPadding = PaddingValues(vertical = 12.dp),
        ) {
            item {
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Button(onClick = { starting = true }, enabled = online && !action.busy, colors = accentButtonColors(), modifier = Modifier.heightIn(min = 48.dp)) { Text("Start job") }
                    OutlinedButton(onClick = { refresh() }, enabled = online && !status.busy, modifier = Modifier.heightIn(min = 48.dp)) { Text("Refresh jobs") }
                }
                Text("Headless jobs · ${jobs.size}", style = MaterialTheme.typography.titleMedium, modifier = Modifier.padding(top = 12.dp))
                Text("Completed jobs stay for 7 days by default, or until dismissed.", style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
                RequestStatus(status)
                RequestStatus(action)
                decodeError?.let { ErrorLine(it) }
                if (jobs.isEmpty()) Text(if (status.busy) "Reading jobs…" else "No jobs yet.", modifier = Modifier.padding(top = 16.dp))
            }
            items(jobs, key = { it.id }) { job ->
                Surface(onClick = { selected = job.id }, color = deck.raised, shape = MaterialTheme.shapes.large, modifier = Modifier.fillMaxWidth()) {
                    Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) { JobSummary(job) }
                }
            }
        }
    }
}

@Composable
private fun JobSummary(job: JobContracts.Info) {
    Text(job.title.ifBlank { "Job #${job.id}" }, style = MaterialTheme.typography.titleMedium)
    Text(listOfNotNull(job.host?.ifBlank { null } ?: "This machine", job.provider?.let(Tools::label), job.outcome).joinToString(" · "),
        style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
    if (job.reason.isNotBlank()) Text(job.reason, style = MaterialTheme.typography.bodyMedium)
}

/** Each line is one argument; the advanced vector also represents empty or multiline arguments. */
internal fun jobArguments(program: String, arguments: String): String = JSONArray().apply {
    put(program)
    if (arguments.isNotEmpty()) arguments.split('\n').forEach { put(it) }
}.toString()

@Composable
private fun JobStartForm(enabled: Boolean, onBack: () -> Unit, onStart: (org.json.JSONObject) -> Unit) {
    val state by Deck.state.collectAsStateWithLifecycle()
    val key = rememberSaveable { "phone-" + UUID.randomUUID().toString() }
    var title by rememberSaveable { mutableStateOf("") }
    var program by rememberSaveable { mutableStateOf("") }
    var arguments by rememberSaveable { mutableStateOf("") }
    var advanced by rememberSaveable { mutableStateOf(false) }
    var vector by rememberSaveable { mutableStateOf("") }
    var cwd by rememberSaveable { mutableStateOf("") }
    var placement by rememberSaveable { mutableStateOf("auto") }
    var provider by rememberSaveable { mutableStateOf<String?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    val hosts = listOf(StartHost("auto", "Auto")) + state?.startHosts.orEmpty().map { it.copy(name = it.name ?: "local") }
    LazyColumn(Modifier.fillMaxSize().imePadding().padding(horizontal = 16.dp), verticalArrangement = Arrangement.spacedBy(12.dp), contentPadding = PaddingValues(vertical = 12.dp)) {
        item {
            TextButton(onClick = onBack, enabled = enabled) { Text("All jobs") }
            Text("New headless job", style = MaterialTheme.typography.titleMedium)
        }
        item { ToolField(title, { title = it }, "Job title", enabled = enabled) }
        if (advanced) item { ToolField(vector, { vector = it }, "Argument vector (JSON)", multiline = true, enabled = enabled) }
        else {
            item { ToolField(program, { program = it }, "Program", enabled = enabled) }
            item {
                ToolField(arguments, { arguments = it }, "Arguments (one per line)", multiline = true, enabled = enabled)
                Text("Spaces and punctuation stay literal. Leave blank for no arguments.", style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
            }
        }
        item {
            TextButton(enabled = enabled, onClick = { if (!advanced) vector = jobArguments(program, arguments); advanced = !advanced }) { Text(if (advanced) "Use program fields" else "Advanced argument vector") }
            ToolField(cwd, { cwd = it }, "Working directory", enabled = enabled)
        }
        item {
            Text("Machine", style = MaterialTheme.typography.titleSmall)
            ChoiceGrid(hosts, columns = hosts.size.coerceIn(1, 3), selected = { it.name == placement }, label = { it.label }) { if (enabled) placement = it.name ?: "local" }
        }
        item {
            Text("Output format", style = MaterialTheme.typography.titleSmall)
            ChoiceGrid(listOf<String?>(null) + Tools.all, columns = 3, selected = { it == provider }, label = { it?.let(Tools::label) ?: "Plain" }) { if (enabled) provider = it }
        }
        item {
            error?.let { ErrorLine(it) }
            Button(onClick = {
                runCatching { JobContracts.start(key, title, if (advanced) vector else jobArguments(program, arguments), cwd, placement, provider) }
                    .onSuccess { onStart(it) }.onFailure { error = it.message }
            }, enabled = enabled && hosts.any { it.name == placement }, colors = accentButtonColors(), modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp)) { Text("Launch job") }
        }
    }
}

/** Stable row identities survive the service dropping the oldest retained records. */
internal fun jobChatRows(items: List<Item>): List<ChatRow> {
    val occurrences = mutableMapOf<String, Int>()
    return chatRows(ChatView(id = 0, total = items.size, items = items)).map { row ->
        val digest = MessageDigest.getInstance("SHA-256").digest(Protocol.itemJson(row.item).toString().toByteArray(Charsets.UTF_8))
            .joinToString("") { "%02x".format(it.toInt() and 255) }
        val occurrence = occurrences.getOrDefault(digest, 0)
        occurrences[digest] = occurrence + 1
        row.copy(key = "$digest-$occurrence")
    }
}

@Composable
private fun JobDetails(job: JobContracts.Info, online: Boolean, actionEnabled: Boolean, onStop: () -> Unit, onDismiss: () -> Unit) {
    val log = rememberToolRequest("job-log" to job.id)
    var output by remember(job.id) { mutableStateOf(JobContracts.Output(job.id)) }
    var raw by rememberSaveable(job.id) { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    var confirmStop by remember { mutableStateOf(false) }
    var confirmDismiss by remember { mutableStateOf(false) }
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    fun more() {
        val offset = output.offset
        log.run("fleet", JobContracts.logRequest(job.id, offset), keep = true) { value ->
            runCatching { output.append(offset, JobContracts.page(value)) }.onSuccess { output = it; error = null }.onFailure { error = it.message }
        }
    }
    LaunchedEffect(online, job.id, lifecycle) {
        if (!online) return@LaunchedEffect
        lifecycle.repeatOnLifecycle(Lifecycle.State.RESUMED) {
            while (true) {
                if (!output.eof && (output.offset == 0L || !job.terminal)) more()
                delay(3000)
            }
        }
    }
    if (confirmStop || confirmDismiss) AlertDialog(
        onDismissRequest = { confirmStop = false; confirmDismiss = false },
        title = { Text(if (confirmStop) "Stop this job?" else "Dismiss this job?") },
        text = { Text(if (confirmStop) "Stops this job's process. Its private output remains available." else "Removes this completed job and its private output.") },
        confirmButton = { TextButton(onClick = { if (confirmStop) onStop() else onDismiss(); confirmStop = false; confirmDismiss = false }) { Text(if (confirmStop) "Stop job" else "Dismiss job") } },
        dismissButton = { TextButton(onClick = { confirmStop = false; confirmDismiss = false }) { Text("Cancel") } },
    )
    val entries = remember(output.items) { chatEntries(jobChatRows(output.items)) }
    LazyColumn(Modifier.fillMaxSize(), contentPadding = PaddingValues(bottom = 24.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        item {
            Column(Modifier.padding(horizontal = 16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                JobSummary(job)
                job.createdAt?.let { Text("Created " + DateFormat.getDateTimeInstance().format(Date(it * 1000)), style = MaterialTheme.typography.bodySmall) }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    if (job.canStop) OutlinedButton(onClick = { confirmStop = true }, enabled = online && actionEnabled) { Text("Stop job") }
                    if (job.terminal) OutlinedButton(onClick = { confirmDismiss = true }, enabled = online && actionEnabled) { Text("Dismiss job") }
                    TextButton(onClick = { raw = !raw }) { Text(if (raw) "Show chat" else "Show raw log") }
                }
                Text("Read-only output", style = MaterialTheme.typography.titleSmall)
                if (output.trimmed) Text("Earlier loaded output was trimmed on this phone. The deck retains the private log.", style = MaterialTheme.typography.bodySmall)
                RequestStatus(log)
                error?.let { ErrorLine(it) }
            }
        }
        if (raw) {
            item { SelectionContainer { Text(output.text.ifBlank { "No output yet." }, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall, modifier = Modifier.padding(horizontal = 16.dp)) } }
        } else {
            if (entries.isEmpty()) item { Text("No structured output yet. Use the raw log for plain commands.", modifier = Modifier.padding(horizontal = 16.dp)) }
            items(entries, key = { it.key }) { entry ->
                when (entry) {
                    is ChatEntry.One -> ChatItem(entry.row, live = false, tool = job.provider.orEmpty(), sessionId = 0, answerEnabled = false)
                    is ChatEntry.Tools -> ToolGroupRow(entry, live = false, tool = job.provider.orEmpty(), sessionId = 0, answerEnabled = false)
                }
            }
        }
        item {
            Column(Modifier.padding(horizontal = 16.dp)) {
                OutlinedButton(onClick = { more() }, enabled = online && !log.busy && !output.eof, modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp)) {
                    Text(if (log.busy) "Reading log…" else if (output.eof) "End of log" else "Load next page")
                }
            }
        }
    }
}
