package io.github.cepheus.app.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.ContentCopy
import androidx.compose.material.icons.outlined.Keyboard
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
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
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.key.Key
import androidx.compose.ui.input.key.KeyEventType
import androidx.compose.ui.input.key.key
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.input.key.type
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.repeatOnLifecycle
import io.github.cepheus.app.Deck
import io.github.cepheus.app.net.Link
import io.github.cepheus.app.protocol.Item
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import org.json.JSONObject

/** The terminal's text: small enough for about 50 columns on a phone held upright. */
private val TerminalStyle = TextStyle(fontFamily = FontFamily.Monospace, fontSize = 12.sp, lineHeight = 15.sp)










@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun TerminalScreen(id: Long, snackbar: SnackbarHostState, onBack: () -> Unit) {
    val link by Deck.link.collectAsStateWithLifecycle()
    val chat by Deck.chat.collectAsStateWithLifecycle()
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    val scope = rememberCoroutineScope()
    val haptics = LocalHapticFeedback.current
    val clipboard = LocalClipboardManager.current
    val keyboard = LocalSoftwareKeyboardController.current
    val focus = remember { FocusRequester() }
    var title by rememberSaveable(id) { mutableStateOf("Terminal") }
    var alive by remember(id) { mutableStateOf(true) }
    val online = link == Link.Online
    val screen = chat?.takeIf { it.id == id }?.items?.lastOrNull { it is Item.Screen }?.let { (it as Item.Screen).text }
    var error by remember(id) { mutableStateOf<String?>(null) }
    var confirmClose by remember { mutableStateOf(false) }
    var field by remember(id) { mutableStateOf(TerminalInput.empty()) }
    val inputs = remember(id) { Channel<TerminalInput.Send>(Channel.UNLIMITED) }
    val measurer = rememberTextMeasurer()
    var area by remember { mutableStateOf(IntSize.Zero) }

    // One request at a time, in order, so fast typing never drops or reorders a key.
    LaunchedEffect(id) {
        for (send in inputs) {
            val args = JSONObject().put("id", id)
            when (send) {
                is TerminalInput.Send.Text -> args.put("text", send.text)
                is TerminalInput.Send.Key -> args.put("key", send.key.wire)
            }
            Deck.control("terminal_input", args)
                .onSuccess { error = null }
                .onFailure { error = it.message ?: "The terminal did not take that input." }
        }
    }
    // The phone's session list carries agents only, so the shell's name and
    // whether it still runs come from the deck, checked while the page shows
    // until the shell ends. The screen itself stays pushed.
    LaunchedEffect(id, online, lifecycle) {
        if (online) lifecycle.repeatOnLifecycle(Lifecycle.State.RESUMED) {
            while (isActive && alive) {
                Deck.control("terminal_screen", JSONObject().put("id", id)).onSuccess { data ->
                    data.string("title").takeIf { it.isNotBlank() }?.let { title = it }
                    alive = data.optBoolean("alive", true)
                }
                delay(3000)
            }
        }
    }
    // Fit the shell to the page: once it settles, and again when the keyboard opens or closes.
    LaunchedEffect(id, online, alive) {
        if (!online || !alive) return@LaunchedEffect
        var sent: Pair<Int, Int>? = null
        androidx.compose.runtime.snapshotFlow { area }.collectLatest { size ->
            if (size.width == 0 || size.height == 0) return@collectLatest
            delay(250)
            val cell = measurer.measure("M", TerminalStyle).size
            val fit = TerminalInput.fit(size.width, size.height, cell.width, cell.height) ?: return@collectLatest
            if (fit == sent) return@collectLatest
            Deck.control("terminal_resize", JSONObject().put("id", id).put("cols", fit.first).put("rows", fit.second))
                .onSuccess { sent = fit }
        }
    }
    fun send(input: TerminalInput.Send) {
        if (!online || !alive) return
        inputs.trySend(input)
    }
    fun type() {
        focus.requestFocus()
        keyboard?.show()
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text(title, maxLines = 1, overflow = TextOverflow.Ellipsis)
                        Text(
                            when {
                                !alive -> "Ended"
                                !online -> "Reconnecting…"
                                else -> "Live"
                            },
                            style = MaterialTheme.typography.labelMedium,
                            color = deck.textTertiary,
                        )
                    }
                },
                navigationIcon = { BackButton(onBack) },
                actions = {
                    IconButton(enabled = screen != null, onClick = {
                        clipboard.setText(AnnotatedString(screen.orEmpty()))
                        scope.launch { snackbar.showSnackbar("Copied the screen") }
                    }) { Icon(Icons.Outlined.ContentCopy, contentDescription = "Copy the screen") }
                    TextButton(enabled = online && alive, onClick = { confirmClose = true }) { Text("Close") }
                },
                colors = TopAppBarDefaults.topAppBarColors(containerColor = deck.page),
            )
        },
        snackbarHost = { SnackbarHost(snackbar) },
        containerColor = deck.page,
    ) { padding ->
        Column(Modifier.padding(padding).fillMaxSize().imePadding()) {
            LinkBanner(link)
            Box(
                Modifier
                    .weight(1f)
                    .fillMaxWidth()
                    .padding(horizontal = 8.dp)
                    .background(deck.code, RoundedCornerShape(12.dp))
                    .clickable(
                        interactionSource = remember { MutableInteractionSource() },
                        indication = null,
                        enabled = alive,
                        onClick = ::type,
                    )
                    .semantics { contentDescription = "Terminal screen. Tap to type." }
                    .padding(horizontal = 8.dp, vertical = 6.dp)
                    .onSizeChanged { area = it },
            ) {
                val scroll = rememberScrollState()
                // Output that outgrows the page keeps its newest line in view.
                LaunchedEffect(screen) { scroll.scrollTo(scroll.maxValue) }
                Text(
                    screen ?: if (online) "Waiting for the terminal…" else "Reconnect to read this terminal.",
                    style = TerminalStyle,
                    color = if (screen == null) deck.textTertiary else deck.text,
                    softWrap = false,
                    modifier = Modifier.fillMaxSize().verticalScroll(scroll).horizontalScroll(rememberScrollState()),
                )
                // The keyboard types here: invisible, it turns each edit into keys for the shell.
                BasicTextField(
                    value = field,
                    onValueChange = { next ->
                        val (keep, sends) = TerminalInput.edit(field, next)
                        field = keep
                        sends.forEach(::send)
                    },
                    textStyle = TerminalStyle.copy(color = Color.Transparent),
                    cursorBrush = SolidColor(Color.Transparent),
                    keyboardOptions = KeyboardOptions(
                        capitalization = KeyboardCapitalization.None,
                        autoCorrectEnabled = false,
                        keyboardType = KeyboardType.Ascii,
                    ),
                    modifier = Modifier
                        .size(1.dp)
                        .focusRequester(focus)
                        .onPreviewKeyEvent { event ->
                            val key = TerminalKey.forHardware(event.key) ?: return@onPreviewKeyEvent false
                            if (event.type == KeyEventType.KeyDown) send(TerminalInput.Send.Key(key))
                            true
                        },
                )
            }
            error?.let {
                Text(
                    it,
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodySmall,
                    modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp),
                )
            }
            if (alive) {
                KeyRow(
                    enabled = online,
                    onKeyboard = ::type,
                    onKey = { key ->
                        haptics.performHapticFeedback(HapticFeedbackType.KeyboardTap)
                        send(TerminalInput.Send.Key(key))
                    },
                )
            } else {
                Text(
                    "This terminal has ended.",
                    style = MaterialTheme.typography.bodyMedium,
                    color = deck.textSecondary,
                    modifier = Modifier.navigationBarsPadding().padding(16.dp),
                )
            }
        }
    }
    if (confirmClose) AlertDialog(
        onDismissRequest = { confirmClose = false }, title = { Text("Close this terminal?") },
        text = { Text("Its running program will stop.") },
        confirmButton = { TextButton(onClick = {
            confirmClose = false
            scope.launch { val failure = Deck.close(id); if (failure == null) onBack() else error = failure }
        }) { Text("Close terminal") } },
        dismissButton = { TextButton(onClick = { confirmClose = false }) { Text("Cancel") } },
    )
}

/** The keys a phone keyboard lacks, one scrolling row of small keys as terminal apps put them. */
@Composable
private fun KeyRow(enabled: Boolean, onKeyboard: () -> Unit, onKey: (TerminalKey) -> Unit) {
    Row(
        Modifier
            .fillMaxWidth()
            .navigationBarsPadding()
            .horizontalScroll(rememberScrollState())
            .padding(horizontal = 8.dp, vertical = 6.dp),
        horizontalArrangement = Arrangement.spacedBy(6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Surface(
            onClick = onKeyboard,
            shape = RoundedCornerShape(10.dp),
            color = deck.accent,
            contentColor = deck.onAccent,
            modifier = Modifier.height(44.dp).widthIn(min = 48.dp).semantics { contentDescription = "Keyboard" },
        ) {
            Box(contentAlignment = Alignment.Center) { Icon(Icons.Outlined.Keyboard, contentDescription = null, modifier = Modifier.size(22.dp)) }
        }
        for (key in TerminalKey.row) {
            Surface(
                onClick = { onKey(key) },
                enabled = enabled,
                shape = RoundedCornerShape(10.dp),
                color = deck.control,
                contentColor = if (enabled) deck.text else deck.textTertiary,
                modifier = Modifier.height(44.dp).widthIn(min = 48.dp).semantics { contentDescription = key.spoken },
            ) {
                Box(contentAlignment = Alignment.Center, modifier = Modifier.padding(horizontal = 10.dp)) {
                    Text(key.label, style = MaterialTheme.typography.labelLarge, maxLines = 1)
                }
            }
        }
    }
}

/**
 * The keyboard's edits as terminal input. The hidden field always starts with
 * one marker character, so a Backspace on an otherwise empty line still
 * reaches the field as an edit; a line ends at Enter, and the field empties.
 */
object TerminalInput {
    private const val MARK = "​"

    sealed interface Send {
        data class Text(val text: String) : Send
        data class Key(val key: TerminalKey) : Send
    }

    fun empty() = TextFieldValue(MARK, TextRange(MARK.length))

    /** What to send for an edit from `old` to `new`, and the field to keep. */
    fun edit(old: TextFieldValue, new: TextFieldValue): Pair<TextFieldValue, List<Send>> {
        val before = old.text.removePrefix(MARK)
        if (!new.text.startsWith(MARK)) {
            // The marker went. On an empty line that is one Backspace for the shell's own
            // text; otherwise a selection replaced everything this field typed.
            return empty() to List(before.length.coerceAtLeast(1)) { Send.Key(TerminalKey.BACKSPACE) } + typed("", new.text)
        }
        val after = new.text.removePrefix(MARK)
        // Only the text changes the shell; a moved cursor or composing region does not.
        if (after == before) return new to emptyList()
        val sends = typed(before, after)
        val keep = if (after.contains('\n')) empty() else new.copy(selection = TextRange(new.text.length))
        return keep to sends
    }

    private fun typed(before: String, after: String): List<Send> {
        val same = before.commonPrefixWith(after).length
        val sends = mutableListOf<Send>()
        repeat(before.length - same) { sends += Send.Key(TerminalKey.BACKSPACE) }
        val added = after.substring(same)
        for ((i, part) in added.split('\n').withIndex()) {
            if (i > 0) sends += Send.Key(TerminalKey.ENTER)
            if (part.isNotEmpty()) sends += Send.Text(part)
        }
        return sends
    }

    /** Columns and rows that fit `width`×`height` pixels of cells, inside the deck's bounds. */
    fun fit(width: Int, height: Int, cellWidth: Int, cellHeight: Int): Pair<Int, Int>? {
        if (cellWidth <= 0 || cellHeight <= 0) return null
        val cols = (width / cellWidth).coerceIn(20, 500)
        val rows = (height / cellHeight).coerceIn(5, 300)
        return cols to rows
    }
}
