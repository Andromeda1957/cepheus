package io.github.cepheus.app.ui

import android.content.Intent
import android.provider.Settings
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.automirrored.outlined.Logout
import androidx.compose.material.icons.outlined.BatteryAlert
import androidx.compose.material.icons.outlined.BatteryFull
import androidx.compose.material.icons.outlined.CheckCircle
import androidx.compose.material.icons.outlined.CloudSync
import androidx.compose.material.icons.outlined.Computer
import androidx.compose.material.icons.outlined.DarkMode
import androidx.compose.material.icons.outlined.ErrorOutline
import androidx.compose.material.icons.outlined.Info
import androidx.compose.material.icons.outlined.Insights
import androidx.compose.material.icons.outlined.Notifications
import androidx.compose.material.icons.outlined.NotificationsOff
import androidx.compose.material.icons.outlined.PhoneAndroid
import androidx.compose.material.icons.outlined.Refresh
import androidx.compose.material.icons.outlined.Sync
import androidx.compose.material.icons.outlined.TaskAlt
import androidx.compose.material.icons.outlined.VerifiedUser
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.ListItem
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.repeatOnLifecycle
import io.github.cepheus.app.BuildConfig
import io.github.cepheus.app.Deck
import io.github.cepheus.app.net.Link
import io.github.cepheus.app.notify.Notifier
import io.github.cepheus.app.store.ThemeChoice
import kotlinx.coroutines.launch







@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SettingsScreen(snackbar: SnackbarHostState, onBack: () -> Unit, onStats: () -> Unit, onErrors: () -> Unit, onTools: () -> Unit) {
    val context = LocalContext.current
    val state by Deck.state.collectAsStateWithLifecycle()
    val errors = state?.errors?.size ?: 0
    val link by Deck.link.collectAsStateWithLifecycle()
    val credentials by Deck.credentials.collectAsStateWithLifecycle()
    val version by Deck.version.collectAsStateWithLifecycle()
    val settings by Deck.settings.collectAsStateWithLifecycle()
    var confirmUnpair by remember { mutableStateOf(false) }
    var pickTheme by remember { mutableStateOf(false) }
    // Battery and notification settings change in other screens: read them again on return.
    var resumed by remember { mutableIntStateOf(0) }
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    LaunchedEffect(lifecycle) { lifecycle.repeatOnLifecycle(Lifecycle.State.RESUMED) { resumed++ } }
    val unrestricted = remember(resumed) { ignoringBatteryOptimizations(context) }
    val notifying = remember(resumed) { Notifier.canNotify(context) }
    val openNotificationSettings = {
        context.startActivity(
            Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS).putExtra(Settings.EXTRA_APP_PACKAGE, context.packageName),
        )
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Settings") },
                navigationIcon = { BackButton(onBack) },
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.padding(padding).fillMaxSize().verticalScroll(rememberScrollState())) {
            Section("Deck") {
                Pref(
                    title = "Deck tools",
                    summary = "Branches, Ask, Broadcast, Terminals, Scans, Extensions, Scripting and Service",
                    icon = Icons.Outlined.Computer,
                    opens = true,
                    onClick = onTools,
                )
                Pref(
                    title = "Stats",
                    summary = "Agent usage, and the desktop's CPU, memory and SSD",
                    icon = Icons.Outlined.Insights,
                    opens = true,
                    onClick = onStats,
                )
                Pref(
                    title = "Errors",
                    summary = when (errors) {
                        0 -> "None. The deck's own errors, as F2 lists them"
                        1 -> "1 error"
                        else -> "$errors errors"
                    },
                    icon = Icons.Outlined.ErrorOutline,
                    opens = true,
                    onClick = onErrors,
                )
            }

            Section("Connection") {
                Pref(
                    title = "Saved connections",
                    summary = "Connect directly to another machine, even when this deck is offline",
                    icon = Icons.Outlined.Computer,
                    opens = true,
                    onClick = LocalConnections.current,
                )
                Pref(
                    title = "Status",
                    summary = when (val l = link) {
                        Link.Online -> "Connected"
                        Link.Connecting -> "Connecting…"
                        is Link.Offline -> "Offline · ${l.reason}"
                        is Link.Denied -> "Signed out · ${l.reason}"
                        Link.Unpaired -> "Not paired"
                    },
                    icon = if (link == Link.Online) Icons.Outlined.CheckCircle else Icons.Outlined.Sync,
                )
                Pref(
                    title = "Reconnect now",
                    summary = "Try the deck again at once",
                    icon = Icons.Outlined.Refresh,
                    onClick = { Deck.reconnect() },
                )
                credentials?.let { c ->
                    Pref(title = "Deck address", summary = "${c.host}:${c.port}", icon = Icons.Outlined.Computer, selectable = true)
                    Pref(
                        title = "Certificate SHA-256",
                        summary = c.fingerprint.chunked(4).joinToString(" "),
                        icon = Icons.Outlined.VerifiedUser,
                        mono = true,
                        selectable = true,
                    )
                    Pref(
                        title = "This phone",
                        summary = "${Deck.phoneName()} · ${c.device}",
                        icon = Icons.Outlined.PhoneAndroid,
                        selectable = true,
                    )
                }
            }

            Section("Background") {
                Toggle(
                    title = "Stay connected in the background",
                    summary = if (settings.background) {
                        if (unrestricted) "Alerts arrive while the app is closed, without an ongoing notification."
                        else "Alerts arrive while the app is closed. Allow background battery use below to remove the ongoing notification."
                    } else {
                        "Connects only while the app is open. Alerts arrive while it is."
                    },
                    icon = Icons.Outlined.CloudSync,
                    checked = settings.background,
                    onChange = { on ->
                        Deck.updateSettings { it.copy(background = on) }
                        if (on && !ignoringBatteryOptimizations(context)) requestBatteryExemption(context)
                    },
                )
                Pref(
                    title = "Battery use",
                    summary = if (unrestricted) {
                        "Unrestricted: the connection stays up while the phone sleeps"
                    } else {
                        "Optimized: alerts may arrive late while the phone sleeps. Tap to allow."
                    },
                    icon = if (unrestricted) Icons.Outlined.BatteryFull else Icons.Outlined.BatteryAlert,
                    enabled = settings.background,
                    onClick = if (unrestricted) null else ({ requestBatteryExemption(context) }),
                )
            }

            Section("Notifications") {
                Toggle(
                    title = "Finished",
                    summary = "A session finished its turn",
                    icon = Icons.Outlined.TaskAlt,
                    checked = settings.notifyFinished,
                    onChange = { on -> Deck.updateSettings { it.copy(notifyFinished = on) } },
                )
                Toggle(
                    title = "Needs you",
                    summary = "A session asks a question or waits for approval",
                    icon = Icons.Outlined.Notifications,
                    checked = settings.notifyNeedsYou,
                    onChange = { on -> Deck.updateSettings { it.copy(notifyNeedsYou = on) } },
                )
                Toggle(
                    title = "Service errors",
                    summary = "The deck's own errors, as F2 lists them",
                    icon = Icons.Outlined.ErrorOutline,
                    checked = settings.notifyErrors,
                    onChange = { on -> Deck.updateSettings { it.copy(notifyErrors = on) } },
                )
                Pref(
                    title = "System notification settings",
                    summary = if (notifying) "Sounds, channels and the lock screen" else "Off: no alert will show. Tap to turn on.",
                    icon = if (notifying) Icons.Outlined.Notifications else Icons.Outlined.NotificationsOff,
                    onClick = openNotificationSettings,
                )
            }

            Section("Appearance") {
                Pref(
                    title = "Theme",
                    summary = settings.theme.label,
                    icon = Icons.Outlined.DarkMode,
                    onClick = { pickTheme = true },
                )
            }

            Section("About") {
                Pref(
                    title = "App version",
                    summary = "${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE})",
                    icon = Icons.Outlined.Info,
                )
                Pref(
                    title = "Deck version",
                    summary = version ?: "Shown once connected",
                    icon = Icons.Outlined.Computer,
                )
            }

            Section(null) {
                ListItem(
                    headlineContent = { Text("Unpair", color = MaterialTheme.colorScheme.error) },
                    supportingContent = {
                        Text("Remove this connection from the phone. Other saved connections remain available.")
                    },
                    leadingContent = {
                        Icon(Icons.AutoMirrored.Outlined.Logout, contentDescription = null, tint = MaterialTheme.colorScheme.error)
                    },
                    colors = ListItemDefaults.colors(containerColor = Color.Transparent),
                    modifier = Modifier.clickable { confirmUnpair = true },
                )
            }
            Spacer(Modifier.height(24.dp))
        }
    }
    if (confirmUnpair) {
        AlertDialog(
            onDismissRequest = { confirmUnpair = false },
            title = { Text("Remove this connection?") },
            text = { Text("You will need a new code from the deck to pair again.") },
            confirmButton = {
                TextButton(onClick = {
                    confirmUnpair = false
                    Deck.unpair()
                }) { Text("Unpair", color = MaterialTheme.colorScheme.error) }
            },
            dismissButton = { TextButton(onClick = { confirmUnpair = false }) { Text("Cancel") } },
        )
    }
    if (pickTheme) {
        AlertDialog(
            onDismissRequest = { pickTheme = false },
            title = { Text("Theme") },
            text = {
                Column(Modifier.selectableGroup()) {
                    ThemeChoice.entries.forEach { choice ->
                        Row(
                            Modifier.fillMaxWidth()
                                .selectable(
                                    selected = settings.theme == choice,
                                    role = Role.RadioButton,
                                    onClick = {
                                        Deck.updateSettings { it.copy(theme = choice) }
                                        pickTheme = false
                                    },
                                )
                                .padding(vertical = 12.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            RadioButton(selected = settings.theme == choice, onClick = null)
                            Text(choice.label, Modifier.padding(start = 16.dp))
                        }
                    }
                }
            },
            confirmButton = { TextButton(onClick = { pickTheme = false }) { Text("Cancel") } },
        )
    }
}








@Composable
private fun Section(title: String?, rows: @Composable ColumnScope.() -> Unit) {
    if (title != null) {
        Text(
            title,
            style = MaterialTheme.typography.titleSmall,
            color = deck.textSecondary,
            // In line with the icons on the card below it.
            modifier = Modifier.padding(start = 32.dp, end = 32.dp, top = 20.dp, bottom = 8.dp).semantics { heading() },
        )
    } else {
        Spacer(Modifier.height(20.dp))
    }
    Floating(Modifier.fillMaxWidth().padding(horizontal = 16.dp), shape = RoundedCornerShape(20.dp)) {
        Column(content = rows)
    }
}

@Composable
private fun Pref(
    title: String,
    summary: String,
    icon: ImageVector,
    enabled: Boolean = true,
    mono: Boolean = false,
    selectable: Boolean = false,
    opens: Boolean = false,
    onClick: (() -> Unit)? = null,
) {
    val tone = if (enabled) 1f else 0.5f
    ListItem(
        headlineContent = { Text(title, color = MaterialTheme.colorScheme.onSurface.copy(alpha = tone)) },
        supportingContent = {
            val style = MaterialTheme.typography.bodyMedium.let { if (mono) it.copy(fontFamily = FontFamily.Monospace) else it }
            val text = @Composable { Text(summary, style = style, color = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = tone)) }
            if (selectable) SelectionContainer { text() } else text()
        },
        leadingContent = { Icon(icon, contentDescription = null, tint = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = tone)) },
        // A row that opens a page of its own says so, as Android's settings do.
        trailingContent = if (opens) {
            { Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, contentDescription = null, tint = MaterialTheme.colorScheme.onSurfaceVariant) }
        } else {
            null
        },
        colors = ListItemDefaults.colors(containerColor = Color.Transparent),
        modifier = if (onClick != null && enabled) Modifier.clickable(onClick = onClick) else Modifier,
    )
}

@Composable
private fun Toggle(title: String, summary: String, icon: ImageVector, checked: Boolean, onChange: (Boolean) -> Unit) {
    ListItem(
        headlineContent = { Text(title) },
        supportingContent = { Text(summary) },
        leadingContent = { Icon(icon, contentDescription = null, tint = MaterialTheme.colorScheme.onSurfaceVariant) },
        trailingContent = { Switch(checked = checked, onCheckedChange = null) },
        colors = ListItemDefaults.colors(containerColor = Color.Transparent),
        modifier = Modifier.toggleable(value = checked, role = Role.Switch, onValueChange = onChange),
    )
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ErrorsScreen(snackbar: SnackbarHostState, onBack: () -> Unit) {
    val state by Deck.state.collectAsStateWithLifecycle()
    val link by Deck.link.collectAsStateWithLifecycle()
    val scope = rememberCoroutineScope()
    val errors = state?.errors.orEmpty().asReversed()
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Errors") },
                navigationIcon = { BackButton(onBack) },
                actions = {
                    TextButton(
                        enabled = errors.isNotEmpty() && link == Link.Online,
                        onClick = {
                            scope.launch { Deck.clearErrors()?.let { snackbar.showSnackbar(it) } }
                        },
                    ) { Text("Clear") }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.padding(padding).fillMaxSize()) {
            LinkBanner(link)
            if (errors.isEmpty()) {
                Text(
                    "No errors.",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(24.dp),
                )
            } else {
                LazyColumn(contentPadding = PaddingValues(vertical = 8.dp)) {
                    items(errors) { e ->
                        Column(Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 10.dp)) {
                            Text(
                                clock(e.at),
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                            SelectionContainer {
                                Text(e.text, style = MaterialTheme.typography.bodyMedium.copy(fontFamily = FontFamily.Monospace))
                            }
                        }
                        HorizontalDivider(Modifier.padding(horizontal = 20.dp))
                    }
                }
            }
        }
    }
}
