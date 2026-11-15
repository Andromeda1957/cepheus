package io.github.cepheus.app.ui

import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.outlined.AccountTree
import androidx.compose.material.icons.outlined.Campaign
import androidx.compose.material.icons.outlined.Code
import androidx.compose.material.icons.outlined.ErrorOutline
import androidx.compose.material.icons.outlined.ExpandMore
import androidx.compose.material.icons.outlined.Extension
import androidx.compose.material.icons.outlined.KeyboardHide
import androidx.compose.material.icons.outlined.PlayArrow
import androidx.compose.material.icons.outlined.PowerSettingsNew
import androidx.compose.material.icons.outlined.Refresh
import androidx.compose.material.icons.outlined.Security
import androidx.compose.material.icons.outlined.Terminal
import androidx.compose.material.icons.outlined.Tune
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.saveable.rememberSaveableStateHolder
import androidx.compose.runtime.saveable.SaveableStateHolder
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.focus.FocusDirection
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.cepheus.app.Deck
import io.github.cepheus.app.net.Link
import io.github.cepheus.app.protocol.StartHost
import io.github.cepheus.app.protocol.Controls
import io.github.cepheus.app.protocol.Tools
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject

private data class ToolPage(val title: String, val description: String, val icon: ImageVector)

private val toolPages = listOf(
    ToolPage("Queue", "Work, dependencies, task sources and schedules", Icons.Outlined.PlayArrow),
    ToolPage("Jobs", "Headless commands, output and exit status", Icons.Outlined.PlayArrow),
    ToolPage("Scheduler", "Capacity, waiting work and provider holds", Icons.Outlined.Tune),
    ToolPage("Branches", "Worktrees, ahead and behind, and their agents", Icons.Outlined.AccountTree),
    ToolPage("Broadcast", "Send one message to several agents", Icons.Outlined.Campaign),
    ToolPage("Terminals", "Open a shell on any machine", Icons.Outlined.Terminal),
    ToolPage("Scans", "Start security scans and read their reports", Icons.Outlined.Security),
    ToolPage("Extensions", "Install plugins, MCP servers and skills", Icons.Outlined.Extension),
    ToolPage("Scripting", "Run the deck's commands and scripts", Icons.Outlined.Code),
    ToolPage("Service", "Restart services and the development server", Icons.Outlined.PowerSettingsNew),
)

/** Each operation is owned by the visible screen and cancelled when it leaves composition. */
internal class ToolRequest(
    private val scope: CoroutineScope,
    private val capabilities: () -> Set<String>? = { null },
    private val control: suspend (String, JSONObject) -> Result<JSONObject> = { action, args -> Deck.control(action, args) },
) {
    var busy by mutableStateOf(false)
    var error by mutableStateOf<String?>(null)
    var data by mutableStateOf<JSONObject?>(null)
    fun supports(action: String): Boolean = capabilities()?.contains(action) != false
    fun run(action: String, args: JSONObject = JSONObject(), keep: Boolean = false, failed: (Throwable) -> Unit = {}, done: (JSONObject) -> Unit = {}) {
        if (busy) return
        if (!supports(action)) {
            error = Controls.UPDATE_NEEDED
            failed(IllegalArgumentException(Controls.UPDATE_NEEDED))
            return
        }
        busy = true
        error = null
        // A refresh keeps what is shown until the new answer replaces it.
        if (!keep) data = null
        scope.launch {
            try {
                control(action, args).fold(
                    onSuccess = { data = it; done(it) },
                    onFailure = { error = it.message ?: "The deck control failed. Try again."; failed(it) },
                )
            } finally {
                busy = false
            }
        }
    }
}

private val LocalToolCapabilities = staticCompositionLocalOf<Set<String>?> { null }

@Composable
internal fun rememberToolRequest(key: Any? = Unit): ToolRequest {
    val scope = rememberCoroutineScope()
    val capabilities = rememberUpdatedState(LocalToolCapabilities.current)
    return remember(scope, key) { ToolRequest(scope, capabilities = { capabilities.value }) }
}

@OptIn(ExperimentalMaterial3Api::class, ExperimentalLayoutApi::class)
@Composable
fun DeckToolsScreen(snackbar: SnackbarHostState, onBack: () -> Unit, onSession: (Long) -> Unit, onTerminal: (Long) -> Unit, queueState: SaveableStateHolder, entry: Long = 0) {
    var page by rememberSaveable { mutableStateOf<String?>(null) }
    val pageStates = rememberSaveableStateHolder()
    var lastEntry by rememberSaveable { mutableStateOf(-1L) }
    LaunchedEffect(entry) { if (lastEntry != entry) { page = null; lastEntry = entry } }
    var jobsBack by remember { mutableStateOf<(() -> Unit)?>(null) }
    var queueBack by remember { mutableStateOf<(() -> Unit)?>(null) }
    var jobTarget by rememberSaveable { mutableStateOf<Long?>(null) }
    var jobConnection by rememberSaveable { mutableStateOf<String?>(null) }
    val selectedConnection by Deck.connection.collectAsStateWithLifecycle()
    val link by Deck.link.collectAsStateWithLifecycle()
    val overview = rememberToolRequest()
    LaunchedEffect(link) { if (link == Link.Online) overview.run("overview") }
    val capabilities = overview.data?.optJSONArray("capabilities")?.let { values ->
        (0 until values.length()).mapNotNull { values.opt(it) as? String }.toSet()
    }
    val dismissKeyboard = rememberDismissKeyboard()
    val keyboardVisible = WindowInsets.isImeVisible
    val back: () -> Unit = { dismissKeyboard(); if (page == "Queue" && queueBack != null) queueBack?.invoke() else if (page == "Jobs" && jobsBack != null) jobsBack?.invoke() else if (page == null) onBack() else page = null }
    BackHandler(enabled = page != null && !keyboardVisible, onBack = back)
    Scaffold(
        containerColor = deck.page,
        topBar = {
            TopAppBar(
                title = { Text(page ?: "Deck tools") },
                navigationIcon = { BackButton(back) },
                actions = {
                    if (keyboardVisible) IconButton(onClick = dismissKeyboard) {
                        Icon(Icons.Outlined.KeyboardHide, contentDescription = "Hide keyboard")
                    }
                },
                colors = TopAppBarDefaults.topAppBarColors(containerColor = deck.page),
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.padding(padding).consumeWindowInsets(padding).fillMaxSize()) {
            LinkBanner(link)
            val online = link == Link.Online
            CompositionLocalProvider(LocalToolCapabilities provides capabilities) {
                // A page slides in from the side it came from, as Android's own pages do.
                AnimatedContent(
                    targetState = page,
                    transitionSpec = {
                        val forward = targetState != null
                        (slideInHorizontally { if (forward) it / 4 else -it / 4 } + fadeIn()) togetherWith
                            (slideOutHorizontally { if (forward) -it / 4 else it / 4 } + fadeOut())
                    },
                    label = "deck tools page",
                ) { shown ->
                    pageStates.SaveableStateProvider(shown ?: "index") {
                        if (shown == null) {
                            Column(
                                Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(vertical = 8.dp),
                            ) {
                                for (tool in toolPages) {
                                    PageCard(tool) { if (tool.title == "Jobs") jobTarget = null; page = tool.title }
                                    HorizontalDivider(Modifier.padding(start = 64.dp, end = 16.dp), color = deck.border)
                                }
                                Spacer(Modifier.height(24.dp))
                            }
                        } else if (shown == "Queue") {
                            queueState.SaveableStateProvider(selectedConnection?.id ?: "unpaired") {
                                QueueScreen(online, capabilities?.contains("fleet"), onBackChange = { queueBack = it }, onSession = onSession, onJob = { jobTarget = it; jobConnection = selectedConnection?.id; page = "Jobs" })
                            }
                        } else if (shown == "Jobs") {
                            JobsScreen(online, capabilities?.contains("fleet"), onBackChange = { jobsBack = it }, initialJob = jobTarget.takeIf { jobConnection == selectedConnection?.id })
                        } else {
                            Column(
                                Modifier.fillMaxSize().imePadding().verticalScroll(rememberScrollState()).padding(horizontal = 16.dp, vertical = 8.dp),
                                verticalArrangement = Arrangement.spacedBy(12.dp),
                            ) {
                                when (shown) {
                                    "Scheduler" -> SchedulerScreen(online, capabilities?.contains("fleet"))
                                    "Branches" -> BranchTools(online, snackbar, onSession)
                                    "Broadcast" -> BroadcastTools(online)
                                    "Terminals" -> TerminalTools(online, onTerminal)
                                    "Scans" -> ScanTools(online, onSession)
                                    "Extensions" -> ExtensionTools(online)
                                    "Scripting" -> {
                                        ScriptTools(online && capabilities?.contains("scripts") != false, onSession, onTerminal)
                                        if (capabilities?.containsAll(setOf("plugins", "plugin")) == true) PluginTools(online)
                                    }
                                    "Service" -> ServiceTools(online)
                                }
                                Spacer(Modifier.height(24.dp))
                            }
                        }
                    }
                }
            }
        }
    }
}

/** A familiar settings-style row: one clear destination, without a wall of buttons. */
@Composable
private fun PageCard(tool: ToolPage, onClick: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onClick).heightIn(min = 72.dp).padding(horizontal = 20.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(tool.icon, contentDescription = null, tint = deck.textSecondary, modifier = Modifier.size(24.dp))
        Column(Modifier.weight(1f).padding(start = 20.dp, end = 12.dp)) {
            Text(tool.title, style = MaterialTheme.typography.titleMedium)
            Text(tool.description, style = MaterialTheme.typography.bodyMedium, color = deck.textSecondary)
        }
        Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, contentDescription = null, tint = deck.textTertiary)
    }
}

@Composable
private fun IconTile(icon: ImageVector) {
    Box(
        Modifier.size(44.dp).clip(RoundedCornerShape(12.dp)).background(deck.control),
        contentAlignment = Alignment.Center,
    ) { Icon(icon, contentDescription = null, tint = deck.text, modifier = Modifier.size(22.dp)) }
}

/** A raised card, as the home list's session cards: a tool's item, form or result. */
@Composable
private fun ToolCard(
    modifier: Modifier = Modifier,
    onClick: (() -> Unit)? = null,
    padding: PaddingValues = PaddingValues(16.dp),
    content: @Composable ColumnScope.() -> Unit,
) {
    val colors = deck
    val shape = RoundedCornerShape(14.dp)
    val border = BorderStroke(1.dp, colors.border)
    val elevation = 0.dp
    val body: @Composable () -> Unit = {
        Column(Modifier.fillMaxWidth().padding(padding), verticalArrangement = Arrangement.spacedBy(8.dp), content = content)
    }
    if (onClick != null) {
        Surface(onClick = onClick, shape = shape, color = colors.raised, contentColor = colors.text, shadowElevation = elevation, border = border, modifier = modifier.fillMaxWidth()) { body() }
    } else {
        Surface(shape = shape, color = colors.raised, contentColor = colors.text, shadowElevation = elevation, border = border, modifier = modifier.fillMaxWidth()) { body() }
    }
}

/** A quiet heading over a group of cards, with an optional action at its end. */
@Composable
private fun SectionTitle(text: String, action: (@Composable () -> Unit)? = null) {
    Row(Modifier.fillMaxWidth().padding(top = 8.dp).heightIn(min = 40.dp), verticalAlignment = Alignment.CenterVertically) {
        Text(
            text,
            style = MaterialTheme.typography.titleSmall,
            color = deck.textSecondary,
            modifier = Modifier.weight(1f).padding(start = 4.dp).semantics { heading() },
        )
        action?.invoke()
    }
}

@Composable
private fun RefreshButton(description: String, enabled: Boolean, busy: Boolean, onClick: () -> Unit) {
    IconButton(onClick = onClick, enabled = enabled && !busy) {
        if (busy) CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp, color = deck.textTertiary)
        else Icon(Icons.Outlined.Refresh, contentDescription = description, tint = deck.textSecondary)
    }
}

/** The page's main action: the accent button, with a spinner while the deck works on it. */
@Composable
private fun PrimaryButton(
    text: String,
    enabled: Boolean = true,
    busy: Boolean = false,
    icon: ImageVector? = null,
    modifier: Modifier = Modifier.fillMaxWidth(),
    onClick: () -> Unit,
) {
    val haptic = LocalHapticFeedback.current
    val dismissKeyboard = rememberDismissKeyboard()
    Button(
        onClick = {
            dismissKeyboard()
            haptic.performHapticFeedback(HapticFeedbackType.Confirm)
            onClick()
        },
        enabled = enabled && !busy,
        colors = accentButtonColors(),
        shape = RoundedCornerShape(12.dp),
        modifier = modifier.heightIn(min = 48.dp),
    ) {
        when {
            busy -> CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp, color = LocalContentColor.current)
            icon != null -> Icon(icon, contentDescription = null, modifier = Modifier.size(18.dp))
        }
        if (busy || icon != null) Spacer(Modifier.width(10.dp))
        Text(text, style = MaterialTheme.typography.labelLarge, textAlign = TextAlign.Center)
    }
}

@Composable
private fun SecondaryButton(
    text: String,
    enabled: Boolean = true,
    icon: ImageVector? = null,
    modifier: Modifier = Modifier,
    onClick: () -> Unit,
) {
    val dismissKeyboard = rememberDismissKeyboard()
    OutlinedButton(
        onClick = { dismissKeyboard(); onClick() },
        enabled = enabled,
        shape = RoundedCornerShape(12.dp),
        border = BorderStroke(1.dp, deck.border),
        colors = ButtonDefaults.outlinedButtonColors(contentColor = deck.text),
        modifier = modifier.heightIn(min = 48.dp),
    ) {
        if (icon != null) {
            Icon(icon, contentDescription = null, modifier = Modifier.size(18.dp))
            Spacer(Modifier.width(8.dp))
        }
        Text(text, style = MaterialTheme.typography.labelLarge, textAlign = TextAlign.Center)
    }
}

@Composable
private fun Caption(text: String, color: Color = deck.textSecondary) {
    Text(text, style = MaterialTheme.typography.bodyMedium, color = color)
}

@Composable
internal fun ErrorLine(text: String) {
    Row(verticalAlignment = Alignment.Top) {
        Icon(Icons.Outlined.ErrorOutline, contentDescription = null, tint = MaterialTheme.colorScheme.error, modifier = Modifier.size(18.dp).padding(top = 1.dp))
        Text(text, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(start = 8.dp))
    }
}

/** The deck's error for a request, under the action that sent it. The action's own spinner shows it working. */
@Composable
internal fun RequestStatus(request: ToolRequest) {
    request.error?.let { ErrorLine(it) }
}

/** Clear focus as well as hiding the IME, so returning to a result does not reopen it. */
@Composable
internal fun rememberDismissKeyboard(): () -> Unit {
    val focus = LocalFocusManager.current
    val keyboard = LocalSoftwareKeyboardController.current
    return { focus.clearFocus(); keyboard?.hide() }
}

@Composable
internal fun ToolField(
    value: String,
    onValueChange: (String) -> Unit,
    label: String,
    multiline: Boolean = false,
    literal: Boolean = false,
    next: Boolean = false,
    placeholder: String? = null,
    enabled: Boolean = true,
) {
    val focus = LocalFocusManager.current
    val dismissKeyboard = rememberDismissKeyboard()
    OutlinedTextField(
        value = value,
        enabled = enabled,
        onValueChange = onValueChange,
        label = { Text(label) },
        placeholder = placeholder?.let { { Text(it) } },
        modifier = Modifier.fillMaxWidth(),
        singleLine = !multiline,
        minLines = if (multiline) 3 else 1,
        maxLines = if (multiline) 6 else 1,
        shape = RoundedCornerShape(12.dp),
        textStyle = MaterialTheme.typography.bodyLarge.copy(fontFamily = if (literal && multiline) FontFamily.Monospace else FontFamily.Default),
        keyboardOptions = KeyboardOptions(
            capitalization = if (literal) KeyboardCapitalization.None else KeyboardCapitalization.Sentences,
            autoCorrectEnabled = !literal,
            keyboardType = if (literal) KeyboardType.Ascii else KeyboardType.Text,
            imeAction = if (multiline) ImeAction.Default else if (next) ImeAction.Next else ImeAction.Done,
        ),
        keyboardActions = KeyboardActions(
            onDone = { dismissKeyboard() },
            onNext = { focus.moveFocus(FocusDirection.Next) },
        ),
    )
}

/** Native single-choice menus keep machine and provider lists out of the typing area. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun <T> ToolPicker(
    label: String,
    value: String,
    choices: List<Pair<T, String>>,
    enabled: Boolean = true,
    unavailable: Boolean = false,
    onPick: (T) -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    val dismissKeyboard = rememberDismissKeyboard()
    ExposedDropdownMenuBox(expanded = expanded, onExpandedChange = { if (enabled) { dismissKeyboard(); expanded = it } }) {
        OutlinedTextField(
            value = value,
            onValueChange = {},
            readOnly = true,
            enabled = enabled,
            isError = unavailable,
            singleLine = true,
            label = { Text(label) },
            trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(expanded) },
            shape = RoundedCornerShape(12.dp),
            modifier = Modifier.menuAnchor(ExposedDropdownMenuAnchorType.PrimaryNotEditable).fillMaxWidth(),
        )
        ExposedDropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
            for ((choice, text) in choices) DropdownMenuItem(
                text = { Text(text) },
                onClick = { expanded = false; onPick(choice) },
                contentPadding = ExposedDropdownMenuDefaults.ItemContentPadding,
            )
        }
    }
}

/** Keep an unavailable selection visible; an operation must never silently move to another machine. */
@Composable
private fun HostPicker(picked: String?, onPick: (String?) -> Unit, all: Boolean = false, label: String = "Machine", enabled: Boolean = true): Boolean {
    val state by Deck.state.collectAsStateWithLifecycle()
    val hosts = state?.startHosts.orEmpty()
    val choices = (if (all) listOf(StartHost("*", "All machines")) else emptyList()) + hosts
    val chosen = ToolForms.startHost(choices, picked)
    val valid = chosen != null
    if (hosts.size > 1 || state?.hosts?.isNotEmpty() == true || !valid || all) {
        ToolPicker(
            label = label,
            value = chosen?.label ?: state?.hosts?.firstOrNull { it.name == picked }?.let { "${it.label} · offline" } ?: "Choose a machine",
            choices = choices.map { it.name to it.label },
            enabled = enabled && choices.isNotEmpty(),
            unavailable = !valid,
            onPick = onPick,
        )
        if (!valid && state != null) Caption("Choose an available machine to continue.", MaterialTheme.colorScheme.error)
    } else {
        Caption("$label · ${chosen.label}")
    }
    return valid
}

private fun hostArgs(host: String?) = ToolForms.hostArgs(host)

@Composable
internal fun Output(text: String) {
    if (text.isNotBlank()) SelectionContainer {
        Text(
            text,
            fontFamily = FontFamily.Monospace,
            style = MaterialTheme.typography.bodySmall,
            color = deck.text,
            modifier = Modifier.fillMaxWidth().clip(RoundedCornerShape(12.dp)).background(deck.code).padding(12.dp),
        )
    }
}

/** A row that opens or picks something, inside a card. */
@Composable
private fun LinkRow(text: String, line: String? = null, enabled: Boolean = true, onClick: () -> Unit) {
    Row(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            .background(deck.control)
            .clickable(enabled = enabled, role = Role.Button, onClick = onClick)
            .heightIn(min = 48.dp)
            .padding(horizontal = 14.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(text, style = MaterialTheme.typography.bodyLarge, maxLines = 2, overflow = TextOverflow.Ellipsis)
            line?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = deck.textSecondary, maxLines = 1, overflow = TextOverflow.Ellipsis) }
        }
        Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, contentDescription = null, tint = deck.textTertiary)
    }
}

@Composable
private fun BranchTools(online: Boolean, snackbar: SnackbarHostState, onSession: (Long) -> Unit) {
    val request = rememberToolRequest()
    var branch by remember { mutableStateOf<JSONObject?>(null) }
    LaunchedEffect(online) { if (online) request.run("branches") }
    SectionTitle("Checked out branches") {
        RefreshButton("Refresh branches", online, request.busy) { request.run("branches", keep = true) }
    }
    RequestStatus(request)
    val items = request.data?.objects("items").orEmpty()
    if (request.data == null && request.busy) Caption("Looking on every machine…")
    if (request.data != null && items.isEmpty()) Caption("No checked out branches were found.")
    for (item in items) ToolCard {
        val name = item.string("branch").ifBlank { "Detached HEAD" }
        Text(name, style = MaterialTheme.typography.titleMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
        val ahead = if (item.has("ahead") && !item.isNull("ahead")) item.optInt("ahead").toString() else "?"
        val behind = if (item.has("behind") && !item.isNull("behind")) item.optInt("behind").toString() else "?"
        Caption(
            listOfNotNull(
                item.string("host_label").ifBlank { "This machine" },
                "$ahead ahead · $behind behind",
                "main checkout".takeIf { item.optBoolean("main") },
            ).joinToString(" · "),
        )
        Text(item.string("path"), style = MaterialTheme.typography.bodySmall, fontFamily = FontFamily.Monospace, color = deck.textTertiary, maxLines = 1, overflow = TextOverflow.Ellipsis)
        item.string("subject").takeIf { it.isNotBlank() }?.let { Text(it, style = MaterialTheme.typography.bodyMedium, maxLines = 2, overflow = TextOverflow.Ellipsis) }
        for (s in item.objects("sessions")) {
            LinkRow(s.string("title").ifBlank { "Session #${s.optLong("id")}" }) { onSession(s.optLong("id")) }
        }
        SecondaryButton(
            "New session in ${item.string("branch").ifBlank { "this worktree" }}",
            enabled = online && item.string("path").isNotBlank(),
            icon = DeckIcons.Plus,
            modifier = Modifier.fillMaxWidth(),
        ) { branch = item }
    }
    branch?.let { chosen ->
        NewSessionSheet(
            onDismiss = { branch = null }, onStarted = { branch = null; onSession(it) }, onResume = { branch = null },
            snackbar = snackbar, host = StartHost(chosen.string("host").ifEmpty { null }, chosen.string("host_label")), cwd = chosen.string("path"),
        )
    }
}

@Composable
private fun BroadcastTools(online: Boolean) {
    val state by Deck.state.collectAsStateWithLifecycle()
    val sessions = state?.sessions.orEmpty().filter { it.alive && it.tool != "shell" }
    var selected by rememberSaveable { mutableStateOf<List<Long>?>(null) }
    val ids = selected ?: ToolForms.targets(sessions)
    var text by rememberSaveable { mutableStateOf("") }
    val request = rememberToolRequest()
    val haptic = LocalHapticFeedback.current
    Caption("Agents waiting on an approval are left out unless you pick them; the deck checks again before sending.")
    if (sessions.isEmpty()) Caption("No live agents to message.")
    if (sessions.isNotEmpty()) ToolCard(padding = PaddingValues(vertical = 6.dp)) {
        for (session in sessions) {
            val checked = session.id in ids
            Row(
                Modifier
                    .fillMaxWidth()
                    .clickable(enabled = !request.busy, role = Role.Checkbox) {
                        haptic.performHapticFeedback(HapticFeedbackType.SegmentTick)
                        selected = if (checked) ids - session.id else (ids + session.id).distinct()
                    }
                    .heightIn(min = 56.dp)
                    .padding(horizontal = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Checkbox(checked = checked, onCheckedChange = null, enabled = !request.busy, modifier = Modifier.padding(8.dp))
                Column(Modifier.weight(1f).padding(start = 4.dp)) {
                    Text(sessionTitle(session), maxLines = 1, overflow = TextOverflow.Ellipsis)
                    Text(
                        listOfNotNull(session.hostLabel, session.status.label).joinToString(" · "),
                        style = MaterialTheme.typography.bodySmall,
                        color = deck.textSecondary,
                    )
                }
                StatusDot(session.status)
                Spacer(Modifier.width(12.dp))
            }
        }
    }
    ToolField(text, { text = it.take(16000) }, "Message", multiline = true)
    val liveIds = ids.filter { id -> sessions.any { it.id == id } }
    PrimaryButton(
        "Send to ${liveIds.size} agent${if (liveIds.size == 1) "" else "s"}",
        online && liveIds.isNotEmpty() && text.isNotBlank(),
        busy = request.busy,
    ) {
        request.run("broadcast", JSONObject().put("ids", JSONArray(liveIds)).put("text", text.trim())) { data ->
            val accepted = data.optJSONArray("accepted")
            request.error = if (accepted != null && accepted.length() < liveIds.size) "${accepted.length()} accepted; some agents could not receive the message." else null
            if (request.error == null) text = ""
        }
    }
    RequestStatus(request)
    request.data?.let { Caption("Accepted by ${it.optJSONArray("accepted")?.length() ?: 0} agents.") }
}

@Composable
private fun TerminalTools(online: Boolean, onTerminal: (Long) -> Unit) {
    val dismissKeyboard = rememberDismissKeyboard()
    val state by Deck.state.collectAsStateWithLifecycle()
    var host by rememberSaveable { mutableStateOf<String?>(null) }
    var cwd by rememberSaveable { mutableStateOf("") }
    var name by rememberSaveable { mutableStateOf("") }
    var command by rememberSaveable { mutableStateOf("") }
    var scratch by rememberSaveable { mutableStateOf(false) }
    var options by rememberSaveable { mutableStateOf(false) }
    val request = rememberToolRequest()
    val hostAvailable = HostPicker(host, { host = it }, enabled = !request.busy)
    val changed = listOf(name.isNotBlank(), cwd.isNotBlank(), command.isNotBlank(), scratch).count { it }
    Caption(
        listOfNotNull(
            name.trim().ifEmpty { null },
            if (command.isBlank()) "Your shell" else command.lineSequence().first().trim(),
            if (scratch) "scratch" else null,
            cwd.trim().ifEmpty { null }?.let { "in ${it.trimEnd('/').substringAfterLast('/')}" },
        ).joinToString(" · "),
    )
    SecondaryButton(if (changed > 0) "Options · $changed" else "Options", icon = Icons.Outlined.Tune) { options = true }
    AnimatedVisibility(options, enter = expandVertically() + fadeIn(), exit = shrinkVertically() + fadeOut()) {
        Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
            ToolField(name, { name = it }, "Terminal name", next = true)
            ToolField(cwd, { cwd = it }, "Folder (absolute path)", literal = true, next = true)
            ToolField(command, { command = it }, "Command and arguments", multiline = true, literal = true)
            Caption("Leave blank for your shell. Otherwise enter the program, then one literal argument per line.", deck.textTertiary)
            Row(
                Modifier.fillMaxWidth().clickable(role = Role.Switch) { scratch = !scratch }.heightIn(min = 48.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text("Scratch terminal", Modifier.weight(1f))
                Switch(checked = scratch, onCheckedChange = null)
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                SecondaryButton("Done") { options = false }
                TextButton(onClick = { dismissKeyboard(); name = ""; cwd = ""; command = ""; scratch = false }) { Text("Reset") }
            }
        }
    }
    PrimaryButton("Open terminal", online && hostAvailable, busy = request.busy, icon = Icons.Outlined.Terminal) {
        request.run("terminal", ToolForms.terminal(host, cwd, name, command, scratch)) { data ->
            if (data.optLong("id") > 0) onTerminal(data.optLong("id")) else request.error = "The deck did not return a terminal."
        }
    }
    RequestStatus(request)
    // Only running terminals: an ended one has nothing left to type into.
    val terminals = state?.sessions.orEmpty().filter { it.alive && it.tool == "shell" }
    SectionTitle("Running terminals")
    if (terminals.isEmpty()) Caption("No terminals running.")
    for (terminal in terminals) {
        ToolCard(onClick = { onTerminal(terminal.id) }, padding = PaddingValues(12.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                IconTile(Icons.Outlined.Terminal)
                Column(Modifier.weight(1f).padding(horizontal = 14.dp)) {
                    Text(sessionTitle(terminal), style = MaterialTheme.typography.titleMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    Caption(listOfNotNull(terminal.hostLabel ?: "This machine", terminal.worktree?.trimEnd('/')?.substringAfterLast('/')).joinToString(" · "))
                }
                Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, contentDescription = null, tint = deck.textTertiary)
            }
        }
    }

}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun ScanTools(online: Boolean, onSession: (Long) -> Unit) {
    var host by rememberSaveable { mutableStateOf<String?>(null) }
    var filter by rememberSaveable { mutableStateOf<String?>("*") }
    var scope by rememberSaveable { mutableStateOf("") }
    var reading by remember { mutableStateOf<Pair<String, String>?>(null) }
    val list = rememberToolRequest(filter)
    val request = rememberToolRequest()
    val report = rememberToolRequest()
    LaunchedEffect(filter, online) { if (online) list.run("scans", hostArgs(filter)) }
    val hostAvailable = HostPicker(host, { host = it }, enabled = !request.busy)
    ToolField(scope, { scope = it }, "Scope or path (optional)", literal = true)
    PrimaryButton("Start security scan", online && hostAvailable, busy = request.busy, icon = Icons.Outlined.Security) {
        request.run("scan", hostArgs(host).apply { scope.trim().takeIf { it.isNotEmpty() }?.let { put("scope", it) } }) {
            val id = it.optLong("id")
            if (id > 0) onSession(id) else request.error = "The scan did not return its session."
        }
    }
    RequestStatus(request)
    SectionTitle("Saved scans") {
        RefreshButton("Refresh scans", online, list.busy) { list.run("scans", hostArgs(filter), keep = true) }
    }
    HostPicker(filter, { filter = it }, all = true, label = "Show scans from")
    RequestStatus(list)
    val scans = list.data?.objects("items").orEmpty().filter { filter != null || it.string("host").isBlank() }
    if (list.data != null && scans.isEmpty()) Caption("No scans saved on these machines.")
    for (scan in scans) ToolCard {
        val id = scan.string("id")
        val reportKey = scan.string("host") to id
        Text(id, style = MaterialTheme.typography.titleSmall, maxLines = 1, overflow = TextOverflow.Ellipsis)
        Caption(listOf(scan.string("host_label").ifBlank { scan.string("host").ifBlank { "This machine" } }, scan.string("repo"), scan.string("state")).filter { it.isNotBlank() }.joinToString(" · "))
        FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            SecondaryButton("Read report", online && !report.busy) {
                reading = reportKey
                report.run("scan_report", hostArgs(scan.string("host").ifBlank { null }).put("id", id))
            }
            if (scan.optLong("session") > 0) SecondaryButton("Open session") { onSession(scan.optLong("session")) }
        }
        // The report opens under the scan it belongs to.
        if (reading == reportKey) {
            if (report.busy) Caption("Loading the report…")
            RequestStatus(report)
            report.data?.let { Output(it.string("text")) }
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun ExtensionTools(online: Boolean) {
    val dismissKeyboard = rememberDismissKeyboard()
    var host by rememberSaveable { mutableStateOf<String?>(null) }
    var source by rememberSaveable { mutableStateOf("") }
    var agents by rememberSaveable { mutableStateOf(listOf<String>()) }
    var remove by rememberSaveable { mutableStateOf(false) }
    var preview by remember { mutableStateOf<String?>(null) }
    var confirm by remember { mutableStateOf(false) }
    val request = rememberToolRequest()
    val sync = rememberToolRequest()
    val haptic = LocalHapticFeedback.current
    val fingerprint = "$host|$source|$agents|$remove"
    val args = hostArgs(host).put("source", source.trim()).put("agents", JSONArray(agents)).put("remove", remove)
    val hostAvailable = HostPicker(host, { host = it }, enabled = !request.busy)
    ToolField(source, { source = it }, "Extension source", literal = true)
    Caption("owner/repo, npm:package, a folder on that machine, or mcp:name=command / mcp:name=https://…", deck.textTertiary)
    Text("Agents · none picked means every installed agent", style = MaterialTheme.typography.labelLarge, color = deck.textSecondary)
    FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        for (agent in listOf("claude", "codex", "agy")) FilterChip(
            selected = agent in agents,
            onClick = { haptic.performHapticFeedback(HapticFeedbackType.SegmentTick); agents = if (agent in agents) agents - agent else agents + agent },
            label = { Text(Tools.label(agent)) }, modifier = Modifier.heightIn(min = 48.dp),
        )
    }
    Row(
        Modifier.fillMaxWidth().clickable(role = Role.Switch) { remove = !remove }.heightIn(min = 48.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text("Remove this extension", Modifier.weight(1f))
        Switch(checked = remove, onCheckedChange = null)
    }
    // Preview first; the apply button only arms for the choices just previewed.
    if (preview != fingerprint) {
        PrimaryButton("Preview ${if (remove) "removal" else "installation"}", online && hostAvailable && source.isNotBlank(), busy = request.busy) {
            preview = null
            request.run("install", args.put("dry_run", true)) { if (fingerprint == "$host|$source|$agents|$remove") preview = fingerprint }
        }
    } else {
        PrimaryButton(if (remove) "Remove extension" else "Apply installation", online && hostAvailable, busy = request.busy) { confirm = true }
    }
    RequestStatus(request)
    request.data?.let { data ->
        Output(data.optJSONArray("lines")?.let { lines -> (0 until lines.length()).joinToString("\n") { lines.optString(it) } }
            ?: data.objects("items").joinToString("\n") { it.string("text") }.ifBlank { data.string("text") })
    }
    SectionTitle("Skills")
    SecondaryButton("Sync skills on all machines", online && !sync.busy, icon = Icons.Outlined.Refresh, modifier = Modifier.fillMaxWidth()) {
        sync.run("skills_sync")
    }
    if (sync.busy) Caption("Syncing…")
    RequestStatus(sync)
    sync.data?.let { data ->
        Output(data.optJSONArray("lines")?.let { lines -> (0 until lines.length()).joinToString("\n") { lines.optString(it) } }
            ?: data.objects("items").joinToString("\n") { it.string("text") }.ifBlank { data.string("text") })
    }
    if (confirm) AlertDialog(
        onDismissRequest = { confirm = false }, title = { Text(if (remove) "Remove extension?" else "Install extension?") },
        text = { Text("${source.trim()} on ${host ?: "this machine"}. This changes the selected agents' extensions.") },
        confirmButton = { TextButton(onClick = { dismissKeyboard(); confirm = false; preview = null; request.run("install", args.put("dry_run", false)) }) { Text("Apply") } },
        dismissButton = { TextButton(onClick = { confirm = false }) { Text("Cancel") } },
    )
}

/** What one run of a script did: its label and the messages there were before it. */
private class ScriptRun(val label: String, val seen: Set<String>)

private fun messageKey(m: JSONObject) = "${m.optLong("at")}|${m.optBoolean("error")}|${m.string("text")}"








@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun ScriptTools(online: Boolean, onSession: (Long) -> Unit, onTerminal: (Long) -> Unit) {
    val dismissKeyboard = rememberDismissKeyboard()
    val scope = rememberCoroutineScope()
    val haptic = LocalHapticFeedback.current
    var snapshot by remember { mutableStateOf<JSONObject?>(null) }
    var busy by remember { mutableStateOf<String?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var run by remember { mutableStateOf<ScriptRun?>(null) }
    var follow by remember { mutableStateOf<Job?>(null) }
    var other by rememberSaveable { mutableStateOf<String?>(null) }
    var advanced by rememberSaveable { mutableStateOf(false) }
    var code by rememberSaveable { mutableStateOf("") }
    var option by rememberSaveable { mutableStateOf("") }
    var value by rememberSaveable { mutableStateOf("") }
    var confirmation by remember { mutableStateOf<Pair<String, JSONObject>?>(null) }

    fun call(operation: String, args: JSONObject = JSONObject(), label: String? = null, userInitiated: Boolean = true) {
        if (busy != null) return
        if (userInitiated) dismissKeyboard()
        follow?.cancel()
        if (label != null) run = ScriptRun(label, snapshot?.objects("messages")?.map(::messageKey)?.toSet().orEmpty())
        busy = label ?: operation
        error = null
        scope.launch {
            val result = try {
                Deck.control("scripts", args.put("operation", operation))
            } finally {
                busy = null
            }
            result.fold(
                onSuccess = { data ->
                    snapshot = data
                    val selected = data.optLong("selected")
                    if (selected > 0) {
                        if (data.string("selected_tool") == "shell") onTerminal(selected) else onSession(selected)
                    } else if (label != null) {
                        follow = scope.launch {
                            for (wait in listOf(700L, 1500L, 3000L)) {
                                delay(wait)
                                Deck.control("scripts", JSONObject().put("operation", "list")).onSuccess { snapshot = it }
                            }
                        }
                    }
                },
                onFailure = { error = it.message ?: "The deck could not run that. Try again." },
            )
        }
    }
    LaunchedEffect(online) { if (online) call("list", userInitiated = false) }

    val data = snapshot
    val commands = data?.objects("commands").orEmpty()
    val picker = data?.optJSONObject("picker")
    val report = data?.optJSONObject("report")
    val messages = data?.objects("messages").orEmpty()
    val ready = online && busy == null

    run?.let { r ->
        val fresh = messages.filter { messageKey(it) !in r.seen }
        ToolCard {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(r.label, style = MaterialTheme.typography.titleMedium, fontFamily = FontFamily.Monospace, modifier = Modifier.weight(1f), maxLines = 1, overflow = TextOverflow.Ellipsis)
                if (busy == r.label) CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp, color = deck.textTertiary)
                else IconButton(onClick = { run = null; error = null }) { Icon(DeckIcons.Close, contentDescription = "Dismiss result", tint = deck.textSecondary) }
            }
            when {
                busy == r.label -> Caption("Running…")
                error != null -> ErrorLine(error.orEmpty())
                fresh.isEmpty() && picker == null && report == null -> Caption("Done. It printed nothing; anything it opens appears in Local agents.")
            }
            for (m in fresh) MessageLine(m)
        }
    }
    if (run == null) error?.let { ErrorLine(it) }

    picker?.let { p ->
        val callback = p.opt("callback")
        val items = p.optJSONArray("items") ?: JSONArray()
        val title = p.string("title").ifBlank { "Choose" }
        ToolCard {
            Text(title, style = MaterialTheme.typography.titleMedium)
            for (index in 0 until items.length()) {
                val item = items.get(index)
                val label = if (item is JSONObject) item.string("label").ifBlank { item.string("text") }.ifBlank { item.toString() } else item.toString()
                LinkRow(label, enabled = ready && callback != null) {
                    haptic.performHapticFeedback(HapticFeedbackType.SegmentTick)
                    call("choose", JSONObject().put("callback", callback).put("index", index).put("item", item), label = label)
                }
            }
            TextButton(enabled = ready && callback != null, onClick = { call("choose", JSONObject().put("callback", callback).put("cancel", true)) }) {
                Text("Cancel")
            }
        }
    }
    report?.let { r ->
        ToolCard {
            Text(r.string("title"), style = MaterialTheme.typography.titleMedium)
            r.optJSONArray("lines")?.let { lines -> Output((0 until lines.length()).joinToString("\n") { lines.optString(it) }) }
        }
    }

    SectionTitle("Commands") {
        RefreshButton("Refresh commands", online, busy == "list") { call("list") }
    }
    if (data == null && busy != null) Caption("Loading the deck's commands…")
    if (data != null && commands.isEmpty()) Caption("No deck commands yet. A script adds one with deck.command().")
    for (entry in commands) key(entry.string("name")) {
        val name = entry.string("name")
        var arguments by rememberSaveable(name) { mutableStateOf("") }
        val complete = entry.optJSONArray("complete")?.let { a -> (0 until a.length()).map { a.optString(it) } }.orEmpty().filter { it.isNotBlank() }
        val start = { words: List<String> -> call("command", JSONObject().put("name", name).put("args", JSONArray(words)), label = (listOf(":$name") + words).joinToString(" ")) }
        ToolCard(padding = PaddingValues(start = 16.dp, end = 12.dp, top = 12.dp, bottom = 12.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text(name, style = MaterialTheme.typography.titleMedium, fontWeight = FontWeight.SemiBold)
                    entry.string("desc").takeIf { it.isNotBlank() }?.let { Caption(it) }
                }
                FilledTonalButton(
                    onClick = { haptic.performHapticFeedback(HapticFeedbackType.Confirm); start(emptyList()) },
                    enabled = ready,
                    shape = RoundedCornerShape(14.dp),
                    modifier = Modifier.padding(start = 8.dp).heightIn(min = 44.dp).semantics { contentDescription = "Run $name" },
                ) {
                    Icon(Icons.Outlined.PlayArrow, contentDescription = null, modifier = Modifier.size(18.dp))
                    Spacer(Modifier.width(4.dp))
                    Text("Run")
                }
            }
            // Its completions run with that one word; Other takes any arguments.
            FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                for (word in complete) AssistChip(
                    onClick = { haptic.performHapticFeedback(HapticFeedbackType.Confirm); start(listOf(word)) },
                    enabled = ready,
                    label = { Text(word, fontFamily = FontFamily.Monospace) },
                    modifier = Modifier.heightIn(min = 48.dp),
                )
                AssistChip(
                    onClick = { other = name },
                    enabled = ready,
                    label = { Text("Other…") },
                    modifier = Modifier.heightIn(min = 48.dp).semantics { contentDescription = "Run $name with other arguments" },
                )
            }
            if (other == name) {
                ToolField(arguments, { arguments = it }, "Arguments", literal = true)
                Caption("Separate arguments with spaces; quote an argument that contains spaces.", deck.textTertiary)
                FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    SecondaryButton("Run", ready) { other = null; start(ToolForms.words(arguments)) }
                    TextButton(onClick = { dismissKeyboard(); other = null }) { Text("Cancel") }
                }
            }
        }
    }

    val recent = messages.asReversed().take(6)
    if (recent.isNotEmpty()) {
        SectionTitle("Recent messages")
        ToolCard { for (m in recent) MessageLine(m, quiet = true) }
    }

    val turn by animateFloatAsState(if (advanced) 180f else 0f, label = "advanced chevron")
    Row(
        Modifier.fillMaxWidth().clip(RoundedCornerShape(12.dp)).clickable { advanced = !advanced }.heightIn(min = 48.dp).padding(horizontal = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text("Advanced", style = MaterialTheme.typography.titleSmall, color = deck.textSecondary, modifier = Modifier.weight(1f))
        Icon(Icons.Outlined.ExpandMore, contentDescription = null, tint = deck.textSecondary, modifier = Modifier.rotate(turn))
    }
    AnimatedVisibility(advanced, enter = expandVertically() + fadeIn(), exit = shrinkVertically() + fadeOut()) {
        Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
            ToolCard {
                Text("Scripts", style = MaterialTheme.typography.titleSmall)
                val loaded = data?.optJSONArray("loaded")?.let { l -> (0 until l.length()).map { l.optString(it) } }.orEmpty()
                Caption(if (loaded.isEmpty()) "None loaded." else loaded.joinToString("\n"))
                SecondaryButton("Reload scripts", ready, icon = Icons.Outlined.Refresh, modifier = Modifier.fillMaxWidth()) { call("reload", label = "Reload scripts") }
            }
            ToolCard {
                Text("Options", style = MaterialTheme.typography.titleSmall)
                val options = data?.optJSONObject("options")
                val names = options?.keys()?.asSequence()?.toList()?.sorted().orEmpty()
                if (names.isEmpty()) Caption("No script options set.")
                for (key in names) {
                    Row {
                        Text(key, style = MaterialTheme.typography.bodyMedium, fontFamily = FontFamily.Monospace, modifier = Modifier.weight(1f))
                        Text(options?.opt(key).toString(), style = MaterialTheme.typography.bodyMedium, fontFamily = FontFamily.Monospace, color = deck.textSecondary)
                    }
                }
                ToolField(option, { option = it }, "Option name", literal = true, next = true)
                ToolField(value, { value = it }, "Option value (JSON or text)", literal = true)
                SecondaryButton("Set option", ready && option.isNotBlank(), modifier = Modifier.fillMaxWidth()) {
                    val parsed = try { org.json.JSONTokener(value).nextValue() } catch (_: org.json.JSONException) { value }
                    confirmation = "Set ${option.trim()}?" to JSONObject().put("operation", "set").put("name", option.trim()).put("value", parsed)
                }
            }
            ToolCard {
                Text("Lua", style = MaterialTheme.typography.titleSmall)
                ToolField(code, { code = it }, "Lua code", multiline = true, literal = true)
                SecondaryButton("Evaluate Lua", ready && code.isNotBlank(), modifier = Modifier.fillMaxWidth()) {
                    confirmation = "Evaluate this Lua code?" to JSONObject().put("operation", "eval").put("code", code)
                }
            }
        }
    }

    confirmation?.let { (title, args) -> AlertDialog(
        onDismissRequest = { confirmation = null }, title = { Text(title) },
        text = { Text("This runs in the desktop deck and can change its behavior.") },
        confirmButton = {
            TextButton(onClick = {
                dismissKeyboard()
                confirmation = null
                val operation = args.string("operation")
                call(operation, args, label = if (operation == "eval") "Lua" else title.removeSuffix("?"))
            }) { Text(if (args.string("operation") == "eval") "Evaluate" else "Set") }
        },
        dismissButton = { TextButton(onClick = { confirmation = null }) { Text("Cancel") } },
    ) }
}

/** One deck message: an error in red, with how long ago it came. */
@Composable
private fun MessageLine(m: JSONObject, quiet: Boolean = false) {
    val color = if (m.optBoolean("error")) MaterialTheme.colorScheme.error else if (quiet) deck.textSecondary else deck.text
    Row(verticalAlignment = Alignment.Top) {
        SelectionContainer(Modifier.weight(1f)) {
            Text(m.string("text"), style = MaterialTheme.typography.bodyMedium, color = color)
        }
        if (quiet) Text(ago(m.optLong("at")), style = MaterialTheme.typography.labelSmall, color = deck.textTertiary, modifier = Modifier.padding(start = 8.dp))
    }
}

@Composable
private fun ServiceTools(online: Boolean) {
    val request = rememberToolRequest()
    var confirm by remember { mutableStateOf<Triple<String, String, String>?>(null) }
    Caption("These act on the deck and its machines. Reconnect in Settings only reconnects this phone.")
    val operations = listOf(
        Triple("Restart deck services", "service", "restart") to "Every machine's deck service restarts; sessions keep running.",
        Triple("Stop the empty service", "service", "stop") to "Refuses while any agent or terminal is running. Close chosen sessions first.",
        Triple("Start development server", "dev", "on") to "On this deck's machine.",
        Triple("Stop development server", "dev", "off") to "On this deck's machine.",
    )
    for ((operation, line) in operations) {
        ToolCard(onClick = { if (online && !request.busy) confirm = operation }, padding = PaddingValues(12.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                IconTile(if (operation.second == "dev") Icons.Outlined.Code else Icons.Outlined.PowerSettingsNew)
                Column(Modifier.weight(1f).padding(horizontal = 14.dp)) {
                    Text(operation.first, style = MaterialTheme.typography.titleMedium)
                    Caption(line)
                }
            }
        }
    }
    if (request.busy) Caption("Waiting for the deck…")
    RequestStatus(request)
    request.data?.let { Caption("Requested operation: ${it.string("operation")}") }
    confirm?.let { (title, action, operation) -> AlertDialog(
        onDismissRequest = { confirm = null }, title = { Text("$title?") },
        text = { Text(when {
            action == "dev" -> "This changes the configured development server on this deck's machine."
            operation == "stop" -> "The service stops only when no agents or terminals are running. Otherwise it refuses and leaves them running."
            else -> "The deck services will restart across the configured machines. This phone may disconnect briefly."
        }) },
        confirmButton = { TextButton(onClick = { confirm = null; request.run(action, JSONObject().put("operation", operation).put("confirm", true)) }) { Text("Confirm") } },
        dismissButton = { TextButton(onClick = { confirm = null }) { Text("Cancel") } },
    ) }
}

@Composable
internal fun ToolButton(text: String, enabled: Boolean = true, onClick: () -> Unit) {
    SecondaryButton(text, enabled, onClick = onClick)
}
