package io.github.cepheus.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.repeatOnLifecycle
import io.github.cepheus.app.Deck
import io.github.cepheus.app.net.Link
import io.github.cepheus.app.protocol.HostUse
import io.github.cepheus.app.protocol.Panel
import io.github.cepheus.app.protocol.ServerMessage
import io.github.cepheus.app.protocol.Usage
import io.github.cepheus.app.protocol.UsageWindow
import kotlinx.coroutines.delay

private const val REFRESH_MS = 3_000L








@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun StatsScreen(snackbar: SnackbarHostState, onBack: () -> Unit) {
    val link by Deck.link.collectAsStateWithLifecycle()
    val state by Deck.state.collectAsStateWithLifecycle()
    var stats by remember { mutableStateOf<ServerMessage.Stats?>(null) }
    var now by remember { mutableStateOf(System.currentTimeMillis() / 1000) }
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    LaunchedEffect(lifecycle, link) {
        if (link != Link.Online) return@LaunchedEffect
        lifecycle.repeatOnLifecycle(Lifecycle.State.RESUMED) {
            while (true) {
                Deck.stats()?.let { stats = it }
                now = System.currentTimeMillis() / 1000
                delay(REFRESH_MS)
            }
        }
    }
    // Usage also comes with every session list, so it shows before the first reply.
    val usage = stats?.usage ?: state?.usage

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Stats") },
                navigationIcon = { BackButton(onBack) },
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.padding(padding).fillMaxSize()) {
            LinkBanner(link)
            Column(
                Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp),
                verticalArrangement = Arrangement.spacedBy(16.dp),
            ) {
                val models = modelUsage(usage)
                if (models.isNotEmpty()) {
                    Card("Agent usage") {
                        for (model in models) ModelMeter(model, now)
                    }
                }
                val s = stats
                if (s == null) {
                    Card("This desktop") {
                        Box(Modifier.fillMaxWidth().padding(vertical = 12.dp), contentAlignment = Alignment.Center) {
                            if (link == Link.Online) CircularProgressIndicator() else Muted("Shown once connected")
                        }
                    }
                } else if (s.hosts.isEmpty()) {
                    Card("This desktop") { MachineMeters(s.machines.first()) }
                } else {

                    for (m in s.machines) Card(m.label) { MachineMeters(m) }
                }

                // panels shown on the phone too).
                for (p in s?.panels.orEmpty()) Card(p.title.ifEmpty { p.id }) { PanelLines(p) }
            }
        }
    }
}

/** One machine's CPU, memory and disk, or why its numbers are not there. */
@Composable
private fun MachineMeters(m: HostUse) {
    if (!m.online) {
        Muted("Offline" + (m.reason?.let { " · $it" } ?: ""))
        return
    }
    Meter("CPU", m.cpu, Format.percent(m.cpu))
    Meter("Memory", m.memory, Format.percent(m.memory))
    val disk = m.disk
    Meter(
        "SSD",
        disk?.percentUsed,
        disk?.let { "${Format.size(it.used)} used of ${Format.size(it.total)}" } ?: "–",
    )
}

/** A script section's lines, as the deck's sidebar shows them, in monospace. */
@Composable
private fun PanelLines(p: Panel) {
    if (p.lines.isEmpty()) {
        Muted("Nothing to show")
        return
    }
    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
        for (line in p.lines) {
            Text(
                line.trimEnd(),
                style = MaterialTheme.typography.bodySmall.copy(fontFamily = FontFamily.Monospace),
            )
        }
    }
}

@Composable
private fun Card(title: String, content: @Composable () -> Unit) {
    ElevatedCard(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(14.dp)) {
            Text(title, style = MaterialTheme.typography.titleMedium)
            content()
        }
    }
}







internal fun usedWindows(windows: List<UsageWindow>?): List<UsageWindow> =
    windows.orEmpty().filter { w -> w.usedPercent?.let { it.isFinite() && it > 0.0 } == true }

/** One meter: a model, and the window of its nearest its limit, labelled by window alone. */
internal data class ModelUsage(val name: String, val window: UsageWindow)










internal fun modelUsage(usage: Usage?): List<ModelUsage> {
    if (usage == null) return emptyList()
    val rows = mutableListOf<ModelUsage>()
    val nearer = { a: UsageWindow, b: UsageWindow ->
        val (usedA, usedB) = (a.usedPercent ?: 0.0) to (b.usedPercent ?: 0.0)
        usedA > usedB || usedA == usedB && (a.resetsAt ?: Long.MAX_VALUE) > (b.resetsAt ?: Long.MAX_VALUE)
    }
    fun add(program: String, windows: List<UsageWindow>) {
        val first = rows.size
        for (w in usedWindows(windows)) {
            val split = w.label.lastIndexOf(' ')
            val row = if (split > 0) {
                ModelUsage(w.label.substring(0, split), w.copy(label = w.label.substring(split + 1)))
            } else {
                ModelUsage(program, w)
            }
            val shown = (first until rows.size).firstOrNull { rows[it].name == row.name }
            when {
                shown == null -> rows += row
                nearer(row.window, rows[shown].window) -> rows[shown] = row
            }
        }
    }
    add("Claude", usage.claude + usage.claudeModels)
    add("Codex", usage.codex)
    add("Grok", usage.grok)
    add("Gemini", usage.gemini)
    return rows
}

@Composable
private fun ModelMeter(model: ModelUsage, now: Long) {
    val w = model.window
    val used = w.usedPercent ?: return
    val remaining = 100.0 - used.coerceIn(0.0, 100.0)
    Meter(
        label = "${model.name} · ${w.label}",
        percent = remaining,
        value = "${Format.percent(remaining)} remaining",
        note = Format.resets(w.resetsAt, now),
        colorPercent = used,
    )
}

/** A labelled bar, coloured by the used share even when it displays remaining quota. */
@Composable
private fun Meter(
    label: String,
    percent: Double?,
    value: String,
    note: String? = null,
    colorPercent: Double? = percent,
) {
    Column(
        Modifier.semantics(mergeDescendants = true) {
            contentDescription = listOfNotNull(label, value, note).joinToString(", ")
        },
        verticalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(label, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.weight(1f))
            Text(value, style = MaterialTheme.typography.bodyMedium)
        }
        if (percent != null) {
            LinearProgressIndicator(
                progress = { (percent / 100.0).toFloat().coerceIn(0f, 1f) },
                color = level(colorPercent),
                trackColor = MaterialTheme.colorScheme.surfaceVariant,
                strokeCap = StrokeCap.Round,
                drawStopIndicator = {},
                modifier = Modifier.fillMaxWidth().height(8.dp),
            )
        }
        note?.let { Muted(it) }
    }
}

@Composable
private fun Muted(text: String) {
    Text(text, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
}

/** Green while there is room, gold from 60%, red from 85%, as the deck's header (theme.rs `level`). */
@Composable
private fun level(percent: Double?): Color {
    val dark = LocalDarkTheme.current
    return when {
        percent == null -> MaterialTheme.colorScheme.outline
        percent >= 85 -> MaterialTheme.colorScheme.error
        percent >= 60 -> if (dark) Gold else LightGold
        else -> if (dark) Green else LightGreen
    }
}
