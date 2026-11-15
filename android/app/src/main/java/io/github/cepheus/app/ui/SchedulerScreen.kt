package io.github.cepheus.app.ui

import androidx.compose.foundation.layout.*
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
import kotlinx.coroutines.delay
import org.json.JSONObject
import java.text.DateFormat
import java.util.Date
import kotlin.math.max

/** The phone only submits identifiers. Commands and prompts never become request arguments. */
internal object Scheduler {
    data class Entry(val id: Any, val value: JSONObject)
    data class Snapshot(
        val headroom: JSONObject?,
        val reportedMemoryFree: Boolean?,
        val pools: List<Pair<String, Int>>,
        val holders: List<Entry>,
        val waiters: List<Entry>,
        val starts: List<Entry>,
        val paused: List<JSONObject>,
        val reruns: List<Entry>,
    ) {
        val memoryFree: Boolean get() {
            reportedMemoryFree?.let { return it }
            val h = headroom ?: return false
            val available = h.number("available_mib") ?: return false
            val total = h.number("total_mib")?.takeIf { it > 0 } ?: return false
            val pressure = h.number("memory_pressure")?.takeIf { it >= 0 } ?: return false
            return available >= max(2048.0, total * 0.15) && pressure < 5
        }

        fun canRerun(entry: Entry): Boolean = memoryFree && entry.value.opt("authoritative") == false
        fun canCancelStart(entry: Entry): Boolean = entry.value.isNull("attempted_at")
    }

    fun snapshot(value: JSONObject): Snapshot {
        fun entries(key: String) = value.objects(key).take(1000).mapNotNull { row ->
            val id = row.opt("id")
            if ((id is Int && id > 0) || (id is Long && id > 0)) Entry(id, row) else null
        }
        val pools = value.optJSONObject("pools")
        return Snapshot(
            value.optJSONObject("headroom"),
            value.opt("memory_free") as? Boolean,
            pools?.keys()?.asSequence()?.take(100)?.mapNotNull { name ->
                (pools.opt(name) as? Number)?.toInt()?.takeIf { it >= 0 }?.let { name to it }
            }?.toList().orEmpty(),
            entries("holders"), entries("waiters"), entries("starts").filter { it.value.isNull("session") },
            value.objects("paused").take(1000), entries("reruns"),
        )
    }

    fun target(operation: String, entry: Entry, host: String?): JSONObject =
        Controls.fleet(operation, JSONObject().put("id", entry.id), host)

    fun reset(value: JSONObject): String? = value.number("reset")?.takeIf { it > 0 }?.let {
        DateFormat.getDateTimeInstance(DateFormat.SHORT, DateFormat.SHORT).format(Date((it * 1000).toLong()))
    }
}

private fun JSONObject.number(key: String): Double? = (opt(key) as? Number)?.toDouble()?.takeIf { it.isFinite() }

@Composable
internal fun SchedulerScreen(online: Boolean, supported: Boolean?) {
    val state by Deck.state.collectAsStateWithLifecycle()
    var picked by rememberSaveable { mutableStateOf<String?>(null) }
    val hosts = state?.startHosts.orEmpty()
    val selected = hosts.firstOrNull { it.name == picked }
    val hostOnline = picked == null || selected != null
    val status = rememberToolRequest("scheduler-status" to picked)
    val action = rememberToolRequest("scheduler-action" to picked)
    val admission = rememberToolRequest("scheduler-admission" to picked)
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    LaunchedEffect(online, supported, picked, hostOnline, lifecycle) {
        if (!online || supported != true || !hostOnline) return@LaunchedEffect
        lifecycle.repeatOnLifecycle(Lifecycle.State.RESUMED) {
            while (true) {
                status.run("fleet", Controls.fleet("status", host = picked), keep = true)
                delay(3000)
            }
        }
    }
    if (hosts.size > 1 || !hostOnline) {
        Text("Machine", style = MaterialTheme.typography.titleSmall)
        ChoiceGrid(hosts, columns = hosts.size.coerceIn(1, 3), selected = { it.name == picked }, label = { it.label }) {
            if (!action.busy) picked = it.name
        }
    }
    if (!hostOnline) Text("This machine is offline. Choose an online machine to inspect its scheduler.")
    if (supported == false) {
        Text(Controls.UPDATE_NEEDED, color = MaterialTheme.colorScheme.error)
        return
    }
    if (supported == null) {
        Text(if (online) "Checking the deck’s scheduler support…" else "Connect to the deck to inspect its scheduler.")
        return
    }
    val enabled = online && hostOnline && !action.busy
    OutlinedButton(
        onClick = { status.run("fleet", Controls.fleet("status", host = picked), keep = true) },
        enabled = enabled && !status.busy,
        modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp),
    ) { Text(if (status.busy) "Refreshing…" else "Refresh scheduler") }
    RequestStatus(status)
    RequestStatus(action)
    val snapshot = status.data?.let(Scheduler::snapshot)
    if (snapshot == null) {
        Text(if (status.busy) "Reading capacity and waiting work…" else "Scheduler data is unavailable.")
        return
    }
    fun run(operation: String, entry: Scheduler.Entry) {
        action.run("fleet", Scheduler.target(operation, entry, picked)) {
            status.run("fleet", Controls.fleet("status", host = picked), keep = true)
        }
    }
    SchedulerCard("Headroom") {
        val h = snapshot.headroom
        Text(h?.let {
            val free = it.number("available_mib")?.toLong()?.let { mib -> "$mib MiB available" } ?: "Memory unknown"
            val cpus = it.number("cpus")?.toInt()?.let { count -> "$count CPUs" } ?: "CPU count unknown"
            "$free · $cpus"
        } ?: "Headroom is unavailable.")
        h?.let {
            Text("Memory pressure ${it.number("memory_pressure")?.let { n -> "%.1f".format(n) } ?: "unknown"} · " +
                "CPU pressure ${it.number("cpu_pressure")?.let { n -> "%.1f".format(n) } ?: "unknown"}",
                style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
            Text("Load ${it.number("load")?.let { n -> "%.1f".format(n) } ?: "unknown"}", style = MaterialTheme.typography.bodySmall)
        }
        OutlinedButton(
            onClick = { admission.run("fleet", Controls.fleet("admission", host = picked)) },
            enabled = enabled && !admission.busy,
            modifier = Modifier.heightIn(min = 48.dp),
        ) { Text("Check admission") }
        RequestStatus(admission)
        admission.data?.let {
            Text(when (it.opt("admitted") as? Boolean) {
                true -> "New sessions can start."
                false -> "New sessions are held until this machine has headroom."
                null -> "The deck did not report an admission decision."
            })
        }
    }
    SchedulerCard("Slot pools") {
        if (snapshot.pools.isEmpty()) Text("No slot pools were reported.")
        for ((pool, capacity) in snapshot.pools) {
            val held = snapshot.holders.count { it.value.string("pool") == pool }
            val waiting = snapshot.waiters.count { it.value.string("pool") == pool }
            Text("$pool · $held of $capacity occupied · $waiting waiting")
        }
    }
    SchedulerCard("Holding slots") {
        if (snapshot.holders.isEmpty()) Text("No slots are held.")
        for (entry in snapshot.holders) key(entry.id) {
            SlotEntry(entry)
            TextButton(onClick = { run("slot-release", entry) }, enabled = enabled) { Text("Release slot") }
        }
    }
    SchedulerCard("Waiting for slots") {
        if (snapshot.waiters.isEmpty()) Text("No work is waiting for a slot.")
        for ((index, entry) in snapshot.waiters.withIndex()) key(entry.id) {
            Text("${index + 1}. ${entry.value.string("label").ifBlank { entry.value.string("pool") }}")
            SlotOwner(entry.value)
            TextButton(onClick = { run("slot-release", entry) }, enabled = enabled) { Text("Cancel slot request") }
        }
    }
    SchedulerCard("Queued starts · ${snapshot.starts.size}") {
        if (snapshot.starts.isEmpty()) Text("No session starts are held.")
        for (entry in snapshot.starts) key(entry.id) {
            val value = entry.value
            Text(value.string("title").ifBlank { "${value.string("provider")} session" }, style = MaterialTheme.typography.titleSmall)
            Text(listOf(value.string("provider"), value.string("model")).filter { it.isNotBlank() }.joinToString(" · "))
            Text(value.string("reason").ifBlank { "Waiting for admission" }, color = deck.textSecondary)
            Scheduler.reset(value)?.let { Text("Resets $it", style = MaterialTheme.typography.bodySmall) }
            if (!snapshot.canCancelStart(entry)) {
                Text("This start was submitted to another machine. Cancellation is unavailable here.", style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
            }
            TextButton(onClick = { run("cancel", entry) }, enabled = enabled && snapshot.canCancelStart(entry)) { Text("Cancel queued start") }
        }
    }
    SchedulerCard("Sessions paused for quota · ${snapshot.paused.size}") {
        if (snapshot.paused.isEmpty()) Text("No hosted sessions are paused for quota.")
        for (value in snapshot.paused) {
            Text("Session #${value.opt("session") ?: "?"} · ${value.string("provider")}", style = MaterialTheme.typography.titleSmall)
            Text(value.string("reason").ifBlank { "Usage limit" }, color = deck.textSecondary)
            Text(Scheduler.reset(value)?.let { "Resets $it" } ?: "Reset time is not known yet.", style = MaterialTheme.typography.bodySmall)
        }
    }
    SchedulerCard("Reruns after low memory") {
        if (snapshot.reruns.isEmpty()) Text("No reruns are waiting.")
        for (entry in snapshot.reruns) key(entry.id) {
            val value = entry.value
            Text("Session #${value.opt("session") ?: "?"}", style = MaterialTheme.typography.titleSmall)
            val command = value.optJSONArray("command")?.let { commands ->
                (0 until commands.length()).joinToString(" ") { commands.optString(it) }
            } ?: value.string("command")
            if (command.isNotBlank()) Text(command, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall, maxLines = 4)
            when {
                value.opt("authoritative") == true -> Text("Claude’s memory hook handles this rerun.", color = deck.textSecondary)
                !snapshot.memoryFree -> Text("Waiting for free memory.", color = deck.textSecondary)
            }
            Button(
                onClick = { run("rerun", entry) },
                enabled = enabled && snapshot.canRerun(entry),
                colors = accentButtonColors(),
                modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp),
            ) { Text("Rerun") }
        }
    }
}

@Composable
private fun SchedulerCard(title: String, content: @Composable ColumnScope.() -> Unit) {
    Surface(color = deck.raised, shape = MaterialTheme.shapes.large, modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(title, style = MaterialTheme.typography.titleMedium)
            content()
        }
    }
}

@Composable
private fun SlotEntry(entry: Scheduler.Entry) {
    val value = entry.value
    Text(value.string("label").ifBlank { "${value.string("pool")} slot" }, style = MaterialTheme.typography.titleSmall)
    Text("Pool ${value.string("pool")}", style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
    SlotOwner(value)
}

@Composable
private fun SlotOwner(value: JSONObject) {
    val owner = value.optJSONObject("owner")
    Text(owner?.let {
        listOfNotNull(it.number("session")?.toLong()?.let { id -> "Session #$id" }, it.number("pid")?.toLong()?.let { id -> "PID $id" }).joinToString(" · ")
    }?.ifBlank { "External process" } ?: "External process", style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
}
