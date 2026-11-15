package io.github.cepheus.app.ui

import android.Manifest
import android.annotation.SuppressLint
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.clickable
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyItemScope
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.outlined.Home
import androidx.compose.material.icons.outlined.PanTool
import androidx.compose.material.icons.outlined.PauseCircle
import androidx.compose.material.icons.outlined.Settings
import androidx.compose.material.icons.outlined.Build
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Badge
import androidx.compose.material3.BadgedBox
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DrawerValue
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalDrawerSheet
import androidx.compose.material3.ModalNavigationDrawer
import androidx.compose.material3.NavigationDrawerItem
import androidx.compose.material3.NavigationDrawerItemDefaults
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberDrawerState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.saveable.rememberSaveableStateHolder
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.cepheus.app.Deck
import io.github.cepheus.app.net.Link
import io.github.cepheus.app.protocol.DeckState
import io.github.cepheus.app.protocol.Session
import io.github.cepheus.app.protocol.Status
import io.github.cepheus.app.protocol.Tools
import io.github.cepheus.app.store.SessionOrder
import kotlinx.coroutines.launch
import java.text.DateFormat
import java.util.Date

/** Room under the drawer's list for its Settings and New session buttons. */
private val DRAWER_BAR_SPACE = 96.dp

@Composable
fun AppRoot(launch: Launch?, onLaunchHandled: () -> Unit) {
    val connection by Deck.connection.collectAsStateWithLifecycle()
    val savedConnections by Deck.connections.collectAsStateWithLifecycle()
    val screens = rememberSaveableStateHolder()
    var retainedConnections by remember { mutableStateOf(emptySet<String>()) }
    LaunchedEffect(savedConnections) {
        val ids = savedConnections.map { it.id }.toSet()
        (retainedConnections - ids).forEach(screens::removeState)
        retainedConnections = ids
    }
    var chooseConnection by rememberSaveable { mutableStateOf(false) }
    var addConnection by rememberSaveable { mutableStateOf(false) }
    BackHandler(enabled = chooseConnection || addConnection) {
        if (addConnection) addConnection = false else chooseConnection = false
    }
    CompositionLocalProvider(LocalConnections provides { chooseConnection = true }) {
        when {
            addConnection -> PairScreen(
                onBack = { addConnection = false },
                onPaired = { addConnection = false; chooseConnection = false },
            )
            chooseConnection && connection != null -> ConnectionsScreen(
                onBack = { chooseConnection = false }, onAdd = { addConnection = true },
            )
            connection == null -> PairScreen()
            else -> screens.SaveableStateProvider(requireNotNull(connection).id) {
                MainShell(launch, onLaunchHandled)
            }
        }
    }
}

@Composable
private fun MainShell(launch: Launch?, onLaunchHandled: () -> Unit) {
    var screen by rememberSaveable { mutableStateOf(Screens.HOME) }
    var settingsFrom by rememberSaveable { mutableStateOf(Screens.HOME) }
    var toolsFrom by rememberSaveable { mutableStateOf(Screens.SETTINGS) }
    var sessionId by rememberSaveable { mutableLongStateOf(-1L) }
    var terminalFrom by rememberSaveable { mutableStateOf(Screens.TOOLS) }
    var showNew by rememberSaveable { mutableStateOf(false) }
    var showResume by rememberSaveable { mutableStateOf(false) }
    val queueState = rememberSaveableStateHolder()
    var toolsEntry by rememberSaveable { mutableLongStateOf(0L) }
    val drawer = rememberDrawerState(DrawerValue.Closed)
    val focus = LocalFocusManager.current
    val scope = rememberCoroutineScope()
    val chatDrafts = Deck.composerDrafts
    val chatStates = rememberSaveableStateHolder()
    val openConnections = LocalConnections.current
    val snackbar = remember { SnackbarHostState() }
    val state by Deck.state.collectAsStateWithLifecycle()
    val link by Deck.link.collectAsStateWithLifecycle()
    // One search for the drawer and the home list; it stays while a found
    // session is read, so the next one is a swipe away.
    var query by rememberSaveable { mutableStateOf("") }
    val search = rememberSessionSearch(query)
    val find = Find(query, { query = it }, search)

    LaunchedEffect(launch) {
        val l = launch ?: return@LaunchedEffect
        if (l.sessionId != null) {
            sessionId = l.sessionId
            screen = Screens.CHAT
        } else if (l.screen == Screens.ERRORS) {
            settingsFrom = Screens.settingsFrom(screen, settingsFrom)
            screen = Screens.ERRORS
        }
        drawer.close()
        onLaunchHandled()
    }
    LaunchedEffect(Unit) { Deck.notices.collect { snackbar.showSnackbar(it) } }
    // A terminal follows its session the same way: the gateway pushes its screen as it changes.
    LaunchedEffect(screen, sessionId) {
        Deck.open(if ((screen == Screens.CHAT || screen == Screens.TERMINAL) && sessionId >= 0) sessionId else null)
    }
    // Navigation releases the field on the page or drawer being left.
    LaunchedEffect(drawer.isOpen, screen, sessionId) { focus.clearFocus() }

    Onboarding()

    val goBack: () -> Unit = {
        screen = when (screen) {
            Screens.TERMINAL -> terminalFrom
            Screens.TOOLS -> toolsFrom
            else -> Screens.back(screen, settingsFrom)
        }
    }
    BackHandler(enabled = drawer.isOpen) { scope.launch { drawer.close() } }
    BackHandler(enabled = !drawer.isOpen && screen != Screens.HOME, onBack = goBack)
    // Back clears a search before it closes the drawer or leaves the list.
    BackHandler(enabled = query.isNotEmpty() && (drawer.isOpen || screen == Screens.HOME)) { query = "" }

    val openSession: (Long) -> Unit = { id ->
        sessionId = id
        if (state?.sessions?.firstOrNull { it.id == id }?.tool == "shell") {
            terminalFrom = Screens.HOME
            screen = Screens.TERMINAL
        } else screen = Screens.CHAT
        scope.launch { drawer.close() }
    }
    val openTerminal: (Long) -> Unit = { id ->
        sessionId = id
        terminalFrom = Screens.TOOLS
        screen = Screens.TERMINAL
        scope.launch { drawer.close() }
    }
    val openMenu: () -> Unit = { scope.launch { drawer.open() } }
    ModalNavigationDrawer(
        // Text fields and buttons handle their own taps. An unhandled tap on
        // the heading or background releases the keyboard without clearing text.
        modifier = Modifier.pointerInput(focus) {
            detectTapGestures(onTap = { focus.clearFocus() })
        },
        drawerState = drawer,
        drawerContent = {
            DrawerContent(
                state = state,
                current = if (screen == Screens.CHAT || screen == Screens.TERMINAL) sessionId else null,
                atHome = screen == Screens.HOME,
                link = link,
                find = find,
                onHome = {
                    screen = Screens.HOME
                    scope.launch { drawer.close() }
                },
                onSession = openSession,
                onNew = {
                    showNew = true
                    scope.launch { drawer.close() }
                },
                onResume = {
                    showResume = true
                    scope.launch { drawer.close() }
                },
                onSettings = {
                    settingsFrom = Screens.settingsFrom(screen, settingsFrom)
                    screen = Screens.SETTINGS
                    scope.launch { drawer.close() }
                },
                onConnections = { scope.launch { drawer.close(); openConnections() } },
            )
        },
    ) {
        // A page slides over the one it opens from and back off it, as Android's
        // own pages do, instead of cutting in a single frame.
        AnimatedContent(
            targetState = screen to if (screen == Screens.CHAT || screen == Screens.TERMINAL) sessionId else 0L,
            transitionSpec = {
                val deeper = Screens.depth(targetState.first) >= Screens.depth(initialState.first)
                val enter = slideInHorizontally(tween(260)) { if (deeper) it / 5 else -it / 5 } + fadeIn(tween(200))
                val exit = slideOutHorizontally(tween(260)) { if (deeper) -it / 8 else it / 5 } + fadeOut(tween(160))
                (enter togetherWith exit).apply { targetContentZIndex = if (deeper) 1f else 0f }
            },
            label = "screen",
        ) { route -> chatStates.SaveableStateProvider("${route.first}:${route.second}") { when (route.first) {
            Screens.TERMINAL -> TerminalScreen(id = route.second, snackbar = snackbar, onBack = goBack)
            Screens.TOOLS -> DeckToolsScreen(snackbar, goBack, openSession, openTerminal, queueState, toolsEntry)
            Screens.CHAT -> ChatScreen(
                sessionId = route.second,
                snackbar = snackbar,
                actionScope = Deck.actions,
                drafts = chatDrafts,
                attachments = Deck.attachments(route.second),
                onMenu = openMenu,
                onNew = { showNew = true },
                onHome = { screen = Screens.HOME },
                onClosed = { closedId ->
                    // A late response must not navigate away from a different conversation.
                    if (screen == Screens.CHAT && sessionId == closedId) screen = Screens.HOME
                },
                onRelocated = { original, replacement ->
                    // A completed clear cannot steal selection from another page.
                    if (screen == Screens.CHAT && sessionId == original) openSession(replacement)
                },
            )
            Screens.SETTINGS -> SettingsScreen(
                snackbar = snackbar,
                onBack = goBack,
                onStats = { screen = Screens.STATS },
                onErrors = { screen = Screens.ERRORS },
                onTools = { toolsFrom = Screens.SETTINGS; toolsEntry++; screen = Screens.TOOLS },
            )
            Screens.ERRORS -> ErrorsScreen(snackbar = snackbar, onBack = goBack)
            Screens.STATS -> StatsScreen(snackbar = snackbar, onBack = goBack)
            else -> HomeScreen(
                snackbar = snackbar,
                find = find,
                onMenu = openMenu,
                onSession = openSession,
                onNew = { showNew = true },
                onResume = { showResume = true },
                onTools = { toolsFrom = Screens.HOME; toolsEntry++; screen = Screens.TOOLS },
            )
        } } }
    }

    if (showNew) {
        NewSessionSheet(
            onDismiss = { showNew = false },
            onStarted = { id ->
                showNew = false
                openSession(id)
            },
            onResume = {
                showNew = false
                showResume = true
            },
            snackbar = snackbar,
        )
    }
    if (showResume) {
        ResumeSheet(
            onDismiss = { showResume = false },
            onResumed = { id ->
                showResume = false
                openSession(id)
            },
        )
    }
}

/**
 * Asks once for notifications. Only with the background connection on does
 * it also ask, once, to be exempt from battery optimization: the app open
 * on screen needs no exemption.
 */
@Composable
private fun Onboarding() {
    val context = LocalContext.current
    var askBattery by remember { mutableStateOf(false) }
    val wantsBattery = {
        Deck.settings.value.background && !ignoringBatteryOptimizations(context) && !Deck.batteryAsked
    }
    val permission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {
        askBattery = wantsBattery()
    }
    LaunchedEffect(Unit) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            permission.launch(Manifest.permission.POST_NOTIFICATIONS)
        } else {
            askBattery = wantsBattery()
        }
    }
    if (askBattery) {
        AlertDialog(
            onDismissRequest = {
                Deck.batteryAsked = true
                askBattery = false
            },
            title = { Text("Keep alerts on time") },
            text = {
                Text(
                    "Android pauses apps while the phone sleeps. Let Cepheus keep its connection " +
                        "to the deck, so a session that finishes or needs you notifies at once.",
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    Deck.batteryAsked = true
                    askBattery = false
                    requestBatteryExemption(context)
                }) { Text("Allow") }
            },
            dismissButton = {
                TextButton(onClick = {
                    Deck.batteryAsked = true
                    askBattery = false
                }) { Text("Not now") }
            },
        )
    }
}

fun ignoringBatteryOptimizations(context: Context): Boolean =
    context.getSystemService(PowerManager::class.java)?.isIgnoringBatteryOptimizations(context.packageName) == true

@SuppressLint("BatteryLife") // The connection is the app's purpose; the owner installs it directly.
fun requestBatteryExemption(context: Context) {
    val intent = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:${context.packageName}"))
    try {
        context.startActivity(intent)
    } catch (_: RuntimeException) {
        context.startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
    }
}







@Composable
private fun DrawerContent(
    state: DeckState?,
    current: Long?,
    atHome: Boolean,
    link: Link,
    find: Find,
    onHome: () -> Unit,
    onSession: (Long) -> Unit,
    onNew: () -> Unit,
    onResume: () -> Unit,
    onSettings: () -> Unit,
    onConnections: () -> Unit,
) {
    val connection by Deck.connection.collectAsStateWithLifecycle()
    // The Claude app's drawer: the page's own colour, the app's name in the
    // serif, plain rows with a grey pill for the open one.
    ModalDrawerSheet(drawerContainerColor = deck.page, drawerContentColor = deck.text) {
        Row(
            Modifier.padding(start = 24.dp, end = 20.dp, top = 22.dp, bottom = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text("Cepheus", style = MaterialTheme.typography.headlineSmall, modifier = Modifier.weight(1f))
            LinkDot(link)
        }
        TextButton(onClick = onConnections, modifier = Modifier.padding(horizontal = 16.dp)) {
            Text("Connection: ${connection?.name.orEmpty()}", maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
        NavigationDrawerItem(
            label = { Text("Home") },
            icon = { Icon(Icons.Outlined.Home, contentDescription = null) },
            selected = atHome,
            onClick = onHome,
            modifier = Modifier.padding(NavigationDrawerItemDefaults.ItemPadding),
        )
        SessionSearchField(
            find.query,
            find.onQuery,
            busy = find.search.busy,
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
        )
        Box(Modifier.weight(1f).fillMaxWidth()) {
            if (find.query.isBlank()) {
                DrawerSessions(state, current, onSession)
            } else {
                SearchResultsList(
                    sessions = state?.sessions.orEmpty(),
                    query = find.query,
                    search = find.search,
                    current = current,
                    // The sheet is surfaceContainerLow: cards a step above it.
                    tile = MaterialTheme.colorScheme.surfaceContainerHigh,
                    onSession = onSession,
                    contentPadding = PaddingValues(bottom = DRAWER_BAR_SPACE),
                )
            }
            DrawerBar(
                errors = state?.errors?.size ?: 0,
                onSettings = onSettings,
                onNew = onNew,
                onResume = onResume,
                modifier = Modifier.align(Alignment.BottomCenter),
            )
        }
    }
}

@Composable
private fun DrawerSessions(state: DeckState?, current: Long?, onSession: (Long) -> Unit) {
    val list = rememberLazyListState()
    val (all, reorder) = rememberSessionReorder(state?.sessions.orEmpty().filterNot { state?.isCloud(it) == true }, list)
    val sessions = running(all)
    LazyColumn(
        Modifier.fillMaxSize().reorderable(reorder),
        state = list,
        // The last row scrolls clear of the buttons over the list's end.
        contentPadding = PaddingValues(bottom = DRAWER_BAR_SPACE),
    ) {
        cloudAgents(state, drawer = true, onSession = onSession)
        if (state?.hosts?.isNotEmpty() == true || sessions.isEmpty()) item(key = "local-agents-heading") {
            GroupHeading("Local agents", sessions.size.toString() + " live", drawer = true)
        }
        if (sessions.isEmpty()) item(key = "sessions-empty") { NoSessions(drawer = true) }
        sessionGroups(state, all, drawer = true) { number, s ->
            ReorderableRow(reorder, s.id) {
                NavigationDrawerItem(
                    label = {
                        Column {
                            Text(sessionTitle(s), style = MaterialTheme.typography.bodyLarge, maxLines = 1, overflow = TextOverflow.Ellipsis)
                            Text(
                                sessionLine(s),
                                style = MaterialTheme.typography.bodySmall,
                                color = deck.textSecondary,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                            SubagentSummary(s)
                        }
                    },
                    icon = { Numbered(number) { StatusDot(s.status) } },
                    badge = { if (s.status == Status.NEEDS_YOU && s.alive) Badge(containerColor = statusColor(Status.NEEDS_YOU)) { Text("!") } },
                    selected = s.id == current,
                    onClick = { onSession(s.id) },
                    colors = drawerItemColors(),
                    modifier = Modifier.padding(NavigationDrawerItemDefaults.ItemPadding)
                        .heightIn(min = if (s.subagents.isEmpty()) 56.dp else 80.dp * LocalDensity.current.fontScale.coerceAtLeast(1f)),
                )
            }
        }
    }
}

/**
 * Settings and Resume, round at the left, and New session at the right, over
 * the end of the list. The list fades out under them. A badge on Settings
 * counts the deck's errors, which its Errors page lists.
 */
@Composable
private fun DrawerBar(errors: Int, onSettings: () -> Unit, onNew: () -> Unit, onResume: () -> Unit, modifier: Modifier) {
    val sheet = deck.page
    Row(
        modifier
            .fillMaxWidth()
            .background(Brush.verticalGradient(0f to sheet.copy(alpha = 0f), 0.3f to sheet))
            .padding(start = 12.dp, end = 16.dp, top = 24.dp, bottom = 16.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        BadgedBox(badge = { if (errors > 0) Badge { Text("$errors") } }) {
            FloatingIconButton(Icons.Outlined.Settings, "Settings", onSettings)
        }
        FloatingIconButton(DeckIcons.History, "Resume a conversation", onResume, Modifier.padding(start = 4.dp))
        Spacer(Modifier.weight(1f))
        NewSessionPill(onNew)
    }
}

/** The blue pill that starts a session, with the Claude app's new-chat bubble. */
@Composable
private fun NewSessionPill(onClick: () -> Unit) {
    val colors = deck
    Surface(
        onClick = onClick,
        shape = RoundedCornerShape(50),
        color = colors.accent,
        contentColor = colors.onAccent,
        // The label alone never reached the accessibility tree on the emulator.
        modifier = Modifier.height(48.dp).semantics { contentDescription = "New session" },
    ) {
        Row(Modifier.padding(start = 16.dp, end = 20.dp), verticalAlignment = Alignment.CenterVertically) {
            Icon(DeckIcons.NewChat, contentDescription = null, modifier = Modifier.size(20.dp))
            Text("New session", style = MaterialTheme.typography.labelLarge, modifier = Modifier.padding(start = 8.dp))
        }
    }
}

@Composable
private fun drawerItemColors() = NavigationDrawerItemDefaults.colors(
    selectedContainerColor = deck.selected,
    unselectedContainerColor = Color.Transparent,
    selectedTextColor = deck.text,
    unselectedTextColor = deck.text,
    selectedIconColor = deck.text,
    unselectedIconColor = deck.textSecondary,
)

/** The top bar's Back, on a page that opens from another. */
@Composable
fun BackButton(onBack: () -> Unit) {
    IconButton(onClick = onBack) { Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back") }
}

/**
 * The sessions the lists show, running ones only ([SessionOrder.shown]).
 * Earlier conversations remain available through Resume.
 */
private fun running(sessions: List<Session>) = sessions.filter { it.alive }

/** What an empty list says, with where to start one. */
@Composable
private fun NoSessions(drawer: Boolean) {
    Column(Modifier.fillMaxWidth().padding(horizontal = if (drawer) 28.dp else 20.dp, vertical = 24.dp)) {
        Text("No local agents running", style = MaterialTheme.typography.titleSmall, color = deck.text)
        Text(
            "Start one with New session, or pick up an earlier one with Resume.",
            style = MaterialTheme.typography.bodyMedium,
            color = deck.textSecondary,
            modifier = Modifier.padding(top = 4.dp),
        )
    }
}

/**
 * The shared session list keeps the deck's visible numbers across machines.
 * The drawer adds its heading; Home already has a Sessions title.
 */
private fun LazyListScope.sessionGroups(
    state: DeckState?,
    all: List<Session>,
    drawer: Boolean,
    hint: String = "",
    row: @Composable LazyItemScope.(Int, Session) -> Unit,
) {
    val groups = SessionOrder.shown(state, all)
    for ((g, listed) in groups.withIndex()) {
        val (group, rows) = listed
        val note = group.offline?.let { "offline · $it" } ?: "${SessionOrder.live(group.sessions)} live"
        // Home already has the shared list's title.
        if (drawer || groups.size > 1) {
            item(key = if (g == 0) "sessions-heading" else "host-${group.host}") {
                GroupHeading(group.label, note + if (g == 0) hint else "", drawer)
            }
        }
        items(rows, key = { it.session.id }) { r -> row(r.number, r.session) }
    }
}

/** "Sessions · 3 live", as the deck heads its list, in the Claude app's quiet section label. */
@Composable
internal fun GroupHeading(title: String, note: String, drawer: Boolean = false) {
    Row(
        Modifier.fillMaxWidth().padding(start = if (drawer) 28.dp else 20.dp, end = 16.dp, top = 14.dp, bottom = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(title, style = MaterialTheme.typography.titleSmall, color = deck.text)
        Text(
            " · $note",
            style = MaterialTheme.typography.labelMedium,
            color = deck.textTertiary,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

/** The deck's number for a row, before its dot. */
@Composable
fun Numbered(number: Int?, dot: @Composable () -> Unit) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Text(
            number?.toString().orEmpty(),
            style = MaterialTheme.typography.labelMedium,
            color = deck.textTertiary,
            textAlign = TextAlign.End,
            // At least the column's width; wider rather than "1" over "2" at a large font.
            maxLines = 1,
            softWrap = false,
            modifier = Modifier.widthIn(min = 18.dp),
        )
        Spacer(Modifier.width(8.dp))
        dot()
    }
}

/** Dim an ended session's details when it is already open. */
fun Modifier.ended(ended: Boolean) = if (ended) alpha(0.6f) else this

/**
 * The sessions, as the Claude app lists its Claude Code sessions: a round
 * menu button, the earlier-conversations button and the blue new-session
 * circle floating over the list, a serif "Sessions" title, and each session
 * a raised card with its state in a square tile and a waiting question in a
 * grey box beneath its title.
 */
@Composable
private fun HomeScreen(
    snackbar: SnackbarHostState,
    find: Find,
    onMenu: () -> Unit,
    onSession: (Long) -> Unit,
    onNew: () -> Unit,
    onResume: () -> Unit,
    onTools: () -> Unit,
) {
    val state by Deck.state.collectAsStateWithLifecycle()
    val link by Deck.link.collectAsStateWithLifecycle()
    val prompts by Deck.prompts.collectAsStateWithLifecycle()
    val density = LocalDensity.current
    var top by remember { mutableStateOf(0.dp) }
    val page = deck.page
    Box(Modifier.fillMaxSize().background(page)) {
        val list = rememberLazyListState()
        val current = state
        val (all, reorder) = rememberSessionReorder(current?.sessions.orEmpty().filterNot { current?.isCloud(it) == true }, list)
        val sessions = running(all)
            if (current == null) {
            Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                CircularProgressIndicator(Modifier.size(28.dp), strokeWidth = 2.5.dp, color = deck.textTertiary)
            }
        } else {
            val bottom = WindowInsets.navigationBars.asPaddingValues().calculateBottomPadding()
            if (find.query.isNotBlank()) {
                SearchResultsList(
                    sessions = current.sessions,
                    query = find.query,
                    search = find.search,
                    current = null,
                    tile = MaterialTheme.colorScheme.surfaceContainerLow,
                    onSession = onSession,
                    contentPadding = PaddingValues(top = top, bottom = bottom + 32.dp),
                )
            } else LazyColumn(
                Modifier.fillMaxSize().reorderable(reorder),
                state = list,
                contentPadding = PaddingValues(top = top, bottom = bottom + 32.dp),
            ) {
                cloudAgents(current, drawer = false, onSession = onSession)
                val hint = if (sessions.size > 1) " · hold and drag to reorder" else ""
                item(key = "title") {
                    val waiting = sessions.count { it.alive && prompts[it.id] != null }
                    Column(Modifier.padding(start = 20.dp, end = 20.dp, top = 6.dp, bottom = 10.dp)) {
                        Text("Local agents", style = MaterialTheme.typography.headlineMedium, color = deck.text)
                        Text(
                            "${SessionOrder.live(sessions)} live" +
                                (if (waiting > 0) " · $waiting need${if (waiting == 1) "s" else ""} you" else "") + hint,
                            style = MaterialTheme.typography.bodyMedium,
                            color = deck.textSecondary,
                            modifier = Modifier.padding(top = 2.dp),
                        )
                    }
                }
                sessionGroups(current, all, drawer = false, hint = hint) { number, s ->
                    ReorderableRow(reorder, s.id) {
                        val waiting = prompts[s.id]?.prompt?.question?.lineSequence()?.firstOrNull { it.isNotBlank() }
                        SessionCard(number, s, waiting?.takeIf { s.alive }, onClick = { onSession(s.id) })
                    }
                }
                if (sessions.isEmpty()) item(key = "sessions-empty") { NoSessions(drawer = false) }
            }
        }
        Column(
            Modifier
                .align(Alignment.TopCenter)
                .fillMaxWidth()
                .onSizeChanged { top = with(density) { it.height.toDp() } }
                .headerBand(page)
                .statusBarsPadding(),
        ) {
            Row(
                Modifier.fillMaxWidth().padding(start = 8.dp, end = 12.dp, top = 6.dp, bottom = 2.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                FloatingIconButton(DeckIcons.Menu, "Menu", onMenu)
                Spacer(Modifier.weight(1f))
                FloatingIconButton(Icons.Outlined.Build, "Deck tools", onTools)
                FloatingIconButton(DeckIcons.History, "Resume a conversation", onResume)
                AccentCircleButton(DeckIcons.NewChat, "New session", onNew, size = 44.dp, iconSize = 22.dp)
            }
            LinkBanner(link)
            if (current != null) {
                SessionSearchField(
                    find.query,
                    find.onQuery,
                    busy = find.search.busy,
                    modifier = Modifier.padding(horizontal = 20.dp, vertical = 8.dp),
                )
            }
            Spacer(Modifier.height(8.dp))
        }
        SnackbarHost(snackbar, Modifier.align(Alignment.BottomCenter).navigationBarsPadding())
    }
}

@Composable
private fun IdleQueued(id: Long) {
    val state by Deck.state.collectAsStateWithLifecycle()
    val queued = state?.attention?.queued?.get(id) ?: 0
    if (queued > 0) Text("$queued idle queued", style = MaterialTheme.typography.labelSmall, color = deck.textSecondary)
}

/** One session on the home list. */
@Composable
private fun SessionCard(number: Int, s: Session, waiting: String?, onClick: () -> Unit) {
    RowCard(onClick = onClick) {
        CardBody(
            tile = { StatusTile(s.status) },
            number = number,
            title = sessionTitle(s),
            trailing = ago(s.statusSince),
            line = sessionLine(s),
            subagents = { SubagentSummary(s); IdleQueued(s.id) },
            waiting = waiting,
        )
    }
}

/** A card's insides: the tile, the title with its time, a line beneath, and a waiting question. */
@Composable
private fun CardBody(
    tile: @Composable () -> Unit,
    title: String,
    trailing: String,
    line: String,
    subagents: @Composable () -> Unit = {},
    number: Int? = null,
    waiting: String? = null,
) {
    Row(Modifier.padding(start = 12.dp, end = 14.dp, top = 12.dp, bottom = 12.dp)) {
        tile()
        Column(Modifier.padding(start = 12.dp).weight(1f)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                if (number != null) {
                    Text(
                        "$number",
                        style = MaterialTheme.typography.labelMedium,
                        color = deck.textTertiary,
                        modifier = Modifier.padding(end = 6.dp),
                    )
                }
                Text(
                    title,
                    style = MaterialTheme.typography.titleMedium,
                    color = deck.text,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                if (trailing.isNotEmpty()) {
                    Text(
                        trailing,
                        style = MaterialTheme.typography.labelMedium,
                        color = deck.textTertiary,
                        modifier = Modifier.padding(start = 8.dp),
                    )
                }
            }
            Text(
                line,
                style = MaterialTheme.typography.bodyMedium,
                color = deck.textSecondary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.padding(top = 2.dp),
            )
            subagents()
            if (waiting != null) {
                Text(
                    waiting,
                    style = MaterialTheme.typography.bodyMedium,
                    color = deck.text,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier
                        .padding(top = 10.dp)
                        .fillMaxWidth()
                        .background(deck.control, RoundedCornerShape(12.dp))
                        .padding(horizontal = 12.dp, vertical = 10.dp),
                )
            }
        }
    }
}

/** A 40 dp rounded square holding a state's mark, as the Claude app's session tiles. */
@Composable
private fun Tile(color: Color, content: @Composable () -> Unit) {
    Box(
        Modifier.size(40.dp).clip(RoundedCornerShape(12.dp)).background(color),
        contentAlignment = Alignment.Center,
    ) { content() }
}

/**
 * A session's state as its tile shows it: a spinner while it works, the
 * gold hand while it waits on you, a check once it finished, a pause while
 * idle and a stop once ended. Named for TalkBack by the state's label.
 */
@Composable
private fun StatusTile(status: Status) {
    val color = statusColor(status)
    val quiet = status == Status.IDLE || status == Status.ENDED || status == Status.UNKNOWN
    Box(Modifier.semantics { contentDescription = status.label }) {
        Tile(if (quiet) deck.control else color.copy(alpha = 0.14f)) {
            val mark = Modifier.size(19.dp)
            when (status) {
                Status.WORKING, Status.STARTING -> CircularProgressIndicator(Modifier.size(17.dp), strokeWidth = 2.dp, color = color)
                Status.NEEDS_YOU -> Icon(Icons.Outlined.PanTool, null, mark, tint = color)
                Status.FINISHED -> Icon(DeckIcons.Check, null, mark, tint = color)
                Status.IDLE -> Icon(Icons.Outlined.PauseCircle, null, mark, tint = deck.textSecondary)
                Status.ENDED -> Icon(DeckIcons.Stop, null, mark, tint = deck.textTertiary)
                Status.UNKNOWN -> StateDot(deck.textTertiary)
            }
        }
    }
}

/** The connection's state, a quiet pill under the header whenever the deck is not live. */
@Composable
fun LinkBanner(link: Link) {
    val openConnections = LocalConnections.current
    val text = when (link) {
        Link.Online, Link.Unpaired -> null
        Link.Connecting -> "Connecting to the deck…"
        is Link.Offline -> "Reconnecting · ${link.reason}"
        is Link.Denied -> "The deck signed this phone out: ${link.reason}"
    }
    AnimatedVisibility(visible = text != null) {
        Surface(
            color = deck.control,
            contentColor = deck.text,
            shape = RoundedCornerShape(16.dp),
            modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 6.dp),
        ) {
            Row(
                Modifier.padding(start = 14.dp, end = 6.dp, top = 6.dp, bottom = 6.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                if (link !is Link.Denied) {
                    CircularProgressIndicator(Modifier.size(14.dp), strokeWidth = 2.dp, color = deck.textSecondary)
                }
                Text(
                    text.orEmpty(),
                    style = MaterialTheme.typography.bodyMedium,
                    modifier = Modifier.weight(1f).padding(vertical = 6.dp),
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                )
                if (link is Link.Offline || link is Link.Denied) {
                    TextButton(onClick = { Deck.reconnect() }) { Text("Retry", color = deck.text) }
                }
                TextButton(onClick = openConnections) { Text("Connections", color = deck.text) }
            }
        }
    }
}






@Composable
internal fun RowCard(
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    content: @Composable () -> Unit,
) {
    Surface(
        onClick = onClick,
        shape = RoundedCornerShape(20.dp),
        color = deck.raised,
        contentColor = deck.text,
        shadowElevation = if (deck.shadows) 1.5.dp else 0.dp,
        border = BorderStroke(1.dp, deck.border),
        modifier = modifier.fillMaxWidth().padding(horizontal = 14.dp, vertical = 5.dp),
    ) { content() }
}

/** A session's state as a small dot of its colour. */
@Composable
fun StatusDot(status: Status) {
    StateDot(statusColor(status), size = 9.dp)
}

@Composable
private fun LinkDot(link: Link) {
    val color = when (link) {
        Link.Online -> statusColor(Status.FINISHED)
        Link.Connecting, is Link.Offline -> statusColor(Status.NEEDS_YOU)
        else -> MaterialTheme.colorScheme.error
    }
    Row(verticalAlignment = Alignment.CenterVertically) {
        Box(Modifier.size(8.dp).clip(CircleShape).background(color))
        Spacer(Modifier.width(6.dp))
        Text(
            when (link) {
                Link.Online -> "Connected"
                Link.Connecting -> "Connecting"
                is Link.Offline -> "Offline"
                is Link.Denied -> "Signed out"
                Link.Unpaired -> "Not paired"
            },
            style = MaterialTheme.typography.labelMedium,
            color = deck.textSecondary,
        )
    }
}

fun sessionTitle(s: Session): String = s.title.ifBlank { "${Tools.label(s.tool)} #${s.id}" }

fun sessionLine(s: Session): String = listOfNotNull(
    Tools.label(s.tool),
    s.branch?.takeIf { it.isNotBlank() },
    s.detail.takeIf { it.isNotBlank() },
    s.hostLabel?.takeIf { it.isNotBlank() },
    if (!s.alive) "ended" else null,
).joinToString(" · ")

/** How long ago a time in seconds was, briefly. */
fun ago(secs: Long, nowMs: Long = System.currentTimeMillis()): String {
    if (secs <= 0) return ""
    val d = (nowMs / 1000 - secs).coerceAtLeast(0)
    return when {
        d < 60 -> "now"
        d < 3600 -> "${d / 60}m"
        d < 86_400 -> "${d / 3600}h"
        else -> "${d / 86_400}d"
    }
}

/** A time in seconds as the phone's locale writes it. */
fun clock(secs: Long): String =
    DateFormat.getDateTimeInstance(DateFormat.SHORT, DateFormat.SHORT).format(Date(secs * 1000))
