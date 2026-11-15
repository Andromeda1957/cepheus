package io.github.cepheus.app.ui

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
import android.net.Uri
import android.speech.RecognizerIntent
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.interaction.collectIsDraggedAsState
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.ChatBubbleOutline
import androidx.compose.material.icons.outlined.PanTool
import androidx.compose.material.icons.outlined.QuestionAnswer
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CheckboxDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TextField
import androidx.compose.material3.TextFieldDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.cepheus.app.Deck
import io.github.cepheus.app.net.Link
import io.github.cepheus.app.protocol.AsyncQuestion
import io.github.cepheus.app.protocol.Answer
import io.github.cepheus.app.protocol.AnswerRefusal
import io.github.cepheus.app.protocol.Answers
import io.github.cepheus.app.protocol.CommandEntry
import io.github.cepheus.app.protocol.Item
import io.github.cepheus.app.protocol.PromptOption
import io.github.cepheus.app.protocol.Status
import io.github.cepheus.app.protocol.Tools
import io.github.cepheus.app.protocol.WaitingPrompt
import io.github.cepheus.app.store.ChatView
import io.github.cepheus.app.store.ComposerDrafts
import io.github.cepheus.app.store.Outbox
import io.github.cepheus.app.store.Outgoing
import io.github.cepheus.app.upload.Attachments
import io.github.cepheus.app.upload.Attachment
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch









@Composable
fun ChatScreen(
    sessionId: Long,
    snackbar: SnackbarHostState,
    actionScope: CoroutineScope,
    attachments: Attachments,
    drafts: ComposerDrafts,
    onMenu: () -> Unit,
    onNew: () -> Unit,
    onHome: () -> Unit,
    onClosed: (Long) -> Unit,
    onRelocated: (Long, Long) -> Unit,
) {
    val state by Deck.state.collectAsStateWithLifecycle()
    val link by Deck.link.collectAsStateWithLifecycle()
    val chat by Deck.chat.collectAsStateWithLifecycle()
    val prompts by Deck.prompts.collectAsStateWithLifecycle()
    val models by Deck.models.collectAsStateWithLifecycle()
    val outbox by Deck.outbox.collectAsStateWithLifecycle()
    val session = state?.sessions?.firstOrNull { it.id == sessionId }
    val pending = outbox[sessionId].orEmpty()
    var heldView by remember(sessionId) { mutableStateOf<ChatView?>(null) }
    chat?.takeIf { it.id == sessionId }?.let { heldView = it }
    val view = heldView
    val outgoing = pending.filter { it.conversation == null || it.conversation == session?.conversation }
    val thinking = session != null && Outbox.thinking(session.status, session.alive, outgoing)
    val workingSince = session?.takeIf { it.status == Status.WORKING }
        ?.let { Outbox.workingSince(it.statusSince, view?.items.orEmpty(), outgoing) }
    val workingLabel = session?.let { Outbox.workingLabel(it.status, it.detail) } ?: "Thinking…"
    val waiting = prompts[sessionId]
    val online = link == Link.Online
    val scope = actionScope
    val composerFocus = remember { FocusRequester() }
    val keyboard = LocalSoftwareKeyboardController.current
    var draft by drafts.field("$sessionId/message")
    var sending by drafts.busy("$sessionId/send")
    var idleMode by drafts.field("$sessionId/delivery-mode")
    val whenIdle = idleMode.text == "idle"
    val idleSupported = state?.attentionCapable == true
    val idleRecoveryResult = runCatching { Deck.attentionDrafts.pending(sessionId) }
    val idleRecovery = idleRecoveryResult.getOrNull()
    var repeatIdle by remember(sessionId) { mutableStateOf<TextFieldValue?>(null) }
    var idleError by remember(sessionId) { mutableStateOf<String?>(null) }
    var answering by drafts.busy("$sessionId/answer")
    var stopping by drafts.busy("$sessionId/stop")
    var showModels by remember { mutableStateOf(false) }
    var confirmClose by remember { mutableStateOf(false) }
    var rename by remember { mutableStateOf(false) }
    var name by rememberSaveable(sessionId) { mutableStateOf("") }
    var renaming by remember { mutableStateOf(false) }
    var renameError by remember { mutableStateOf<String?>(null) }
    val tool = session?.tool.orEmpty()
    val toolModels = models.firstOrNull { it.tool == session?.tool }
    val working = session?.status == Status.WORKING || session?.status == Status.STARTING
    val commandsShown = tool in listOf("claude", "codex")
    val query = commandQuery(draft).takeIf { commandsShown }
    var commands by remember(sessionId) { mutableStateOf(emptyList<CommandEntry>()) }
    var commandLoading by remember(sessionId) { mutableStateOf(false) }
    var commandError by remember(sessionId) { mutableStateOf<String?>(null) }
    var commandRetry by remember(sessionId) { mutableStateOf(0) }
    LaunchedEffect(sessionId, online, session?.worktree, query != null, commandRetry) {
        if (query != null && online) {
            commandLoading = true
            commandError = null
            commands = emptyList()
            Deck.commands(sessionId).fold(
                onSuccess = { commands = it },
                onFailure = { commandError = it.message ?: "Could not load commands." },
            )
            commandLoading = false
        }
    }
    val context = LocalContext.current

    fun sendIdle(submitted: TextFieldValue, newCopy: Boolean = false) {
        sending = true; idleError = null
        scope.launch {
            try {
                Deck.sendWhenIdle(sessionId, submitted.text, newCopy).fold(
                    onSuccess = { if (it.state !in setOf("unknown", "pasted") && draft == submitted) draft = TextFieldValue("") },
                    onFailure = { idleError = it.message },
                )
            } finally { sending = false }
        }
    }

    fun report(error: String?) {
        if (error != null) scope.launch { snackbar.showSnackbar(error) }
    }


    // While a question waits the message box answers it, since the deck

    // it says session skipped"). Typed text goes through the question's
    // free-text option, with the boxes picked on its card. Any question can
    // be set aside to message instead, keeping both drafts (QuestionRecovery).
    val pendingQuestions = view?.pendingQuestions.orEmpty()
    var activeQuestion by rememberSaveable(sessionId) { mutableStateOf<String?>(null) }
    val currentQuestion = if (waiting?.prompt?.kind?.let { it != "async question" } == true) null
        else pendingQuestions.firstOrNull { it.id == activeQuestion && !it.submitted }
            ?: pendingQuestions.firstOrNull { !it.submitted } ?: pendingQuestions.firstOrNull()
    val asking = waiting?.takeIf { session?.alive == true && (it.prompt.kind != "async question" || currentQuestion == null) }
    var response by drafts.field("$sessionId/question/${currentQuestion?.id ?: asking?.fp.orEmpty()}")
    // The questions set aside, by card id or prompt fingerprint, so a
    // question asked afterwards opens again while one set aside stays aside
    // as the list around it changes.
    var aside by rememberSaveable(sessionId) { mutableStateOf(emptySet<String>()) }
    val shownQuestions = QuestionRecovery.shown(pendingQuestions, currentQuestion, asking)
    val questionAside = QuestionRecovery.setAside(shownQuestions, aside)
    val inQuestion = QuestionRecovery.answering(asking != null, currentQuestion != null, questionAside, currentQuestion?.submitted == true)
    var questionError by remember(sessionId, currentQuestion?.id, asking?.fp) { mutableStateOf<String?>(null) }
    var picks by remember(asking?.fp) { mutableStateOf(asking?.prompt?.let(Answers::ticked).orEmpty()) }

    // A question that no longer waits gives the composer back; nothing is sent for it.
    fun refused(refusal: AnswerRefusal, shown: Set<String>) {
        questionError = refusal.message
        if (refusal.stale) {
            aside = shown
            report(refusal.message)
            Deck.refreshChat(sessionId)
        }
    }
    fun answer(asked: WaitingPrompt, answer: Answer, sent: () -> Unit = {}) {
        if (answering) return
        answering = true
        questionError = null
        val shown = QuestionRecovery.staleAside(shownQuestions, waiting)
        scope.launch {
            try {
                val refusal = Deck.answer(asked, answer)
                if (refusal == null) sent() else refused(refusal, shown)
            } finally { answering = false }
        }
    }
    val replySpeech = rememberReplySpeech(sessionId) { report(it) }
    fun answerAsync(asked: AsyncQuestion, choice: Int? = null, text: String? = null) {
        if (answering) return
        answering = true
        questionError = null
        val shown = QuestionRecovery.staleAside(shownQuestions, waiting)
        scope.launch {
            try {
                Deck.replyToQuestion(sessionId, asked.id, choice, text)?.let { refused(it, shown) }
            } finally { answering = false }
        }
    }
    val typedAnswer = asking?.let { Answers.typed(it.prompt, picks, response.text) }

    val density = LocalDensity.current
    var topHeight by remember { mutableStateOf(0.dp) }
    var bottomHeight by remember { mutableStateOf(0.dp) }
    val page = deck.page
    BoxWithConstraints(Modifier.fillMaxSize().background(page).imePadding()) {
        val room = maxHeight
        if (view == null || (!view.live && view.items.isEmpty() && outgoing.isEmpty())) {
            Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                if (online || view == null) {
                    CircularProgressIndicator(Modifier.size(28.dp), strokeWidth = 2.5.dp, color = deck.textTertiary)
                } else {
                    Text("Not loaded yet", color = deck.textSecondary)
                }
            }
        } else {
            MessageList(
                MessageListContent(view, outgoing, thinking,
                    session?.status in setOf(Status.FINISHED, Status.IDLE, Status.ENDED)),
                replySpeech = replySpeech,
                onReplyError = { report(it) },
                live = online && view.live,
                answerEnabled = online && session?.alive == true,
                workingLabel = workingLabel,
                workingSince = workingSince,
                tool = tool,
                alive = session?.alive == true,
                top = topHeight,
                bottom = bottomHeight,
                onRecover = { failed ->
                    if (session?.alive != true) {
                        report("Resume this session before editing the message. Your message has been kept.")
                    } else if (draft.text.isNotBlank() || attachments.list.isNotEmpty()) {
                        report("Finish or clear the current draft before editing this message.")
                    } else {
                        val error = attachments.restore(failed.files, failed.uris)
                        if (error != null) report(error) else {
                            draft = TextFieldValue(failed.text)
                            Deck.dismissFailed(sessionId, failed.key)
                            if (session?.alive == true) { composerFocus.requestFocus(); keyboard?.show() }
                        }
                    }
                },
            )
        }

        // The header floats over the messages, which fade out beneath it.
        Column(
            Modifier
                .align(Alignment.TopCenter)
                .fillMaxWidth()
                .onSizeChanged { topHeight = with(density) { it.height.toDp() } }
                .headerBand(page)
                .statusBarsPadding(),
        ) {
            ChatHeader(
                title = session?.let(::sessionTitle) ?: "Conversation",
                status = session?.status,
                branch = session?.branch,
                alive = session?.alive,
                subagents = { session?.let { SubagentSummary(it, details = true) } },
                onMenu = onMenu,
                onNew = onNew,
                onClear = if (commandsShown) {
                    {
                        sending = true
                        actionScope.launch {
                            try {
                                Deck.clear(sessionId).fold(
                                    onSuccess = { replacement -> replacement?.let { onRelocated(sessionId, it) } },
                                    onFailure = { report(it.message ?: "The clear failed.") },
                                )
                            } finally {
                                sending = false
                            }
                        }
                    }
                } else null,
                clearEnabled = online && !sending,
                onClose = { confirmClose = true },
                onReopen = { scope.launch { report(Deck.reopen(sessionId)) } },
                onRename = { name = session?.title.orEmpty(); renameError = null; rename = true },
                idleMode = whenIdle,
                onIdleMode = if (idleSupported || whenIdle) ({ idleMode = TextFieldValue(if (whenIdle) "" else "idle") }) else null,
            )
            LinkBanner(link)
            Spacer(Modifier.height(10.dp))
        }

        // A waiting question, the command list and the composer, stacked over
        // the bottom of the messages. Never taller than the room under the
        // header: a long question with the keyboard up once grew over the top
        // bar (emulator check, 2026-10-27).
        val fade = with(density) { 14.dp.toPx() }
        Column(
            Modifier
                .align(Alignment.BottomCenter)
                .fillMaxWidth()
                .heightIn(max = (room - topHeight).coerceAtLeast(0.dp))
                .onSizeChanged { bottomHeight = with(density) { it.height.toDp() } }
                .drawBehind {
                    drawRect(Brush.verticalGradient(listOf(page.copy(alpha = 0f), page), startY = 0f, endY = fade))
                }
                .navigationBarsPadding()
                .padding(top = 14.dp),
        ) {
            if (session == null) {
                UnavailableSession(onHome)
            } else if (!session.alive) {
                EndedCard(
                    model = modelLabel(view?.model, view?.effort, toolModels, session.tool),
                    onReopen = { scope.launch { report(Deck.reopen(sessionId)) } },
                )
            } else {
                if (query != null && !inQuestion) {
                    CommandSuggestions(
                        modifier = Modifier.weight(1f, fill = false),
                        entries = commands, query = query, loading = commandLoading,
                        error = commandError, online = online,
                        onRetry = { commandRetry++ },
                        onPick = { draft = insertCommand(draft, query, it); composerFocus.requestFocus(); keyboard?.show() },
                    )
                }
                if (currentQuestion != null) {
                    PendingQuestionCard(
                        pendingQuestions, currentQuestion, answering, online && session?.alive == true,
                        modifier = Modifier.weight(1f, fill = false),
                        maxHeight = ((room - topHeight) * 0.45f).coerceAtLeast(0.dp),
                        error = questionError,
                        expanded = !questionAside,
                        onExpanded = { aside = if (it) emptySet() else shownQuestions },
                        onSelect = { activeQuestion = it },
                        onChoice = { answerAsync(currentQuestion, choice = it) },
                    )
                } else if (asking != null) {
                    PromptCard(
                        asking,
                        modifier = Modifier.weight(1f, fill = false),
                        maxHeight = minOf((room - topHeight) * 0.55f, 380.dp),
                        busy = answering,
                        enabled = online,
                        picks = picks,
                        onPicks = { picks = it },
                        onAnswer = { answer(asking, it) },
                        expanded = !questionAside,
                        onExpanded = { aside = if (it) emptySet() else shownQuestions },
                    )
                }
                if (currentQuestion == null && inQuestion) questionError?.let {
                    Text(it, color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(horizontal = 20.dp))
                }
                if (whenIdle && !inQuestion) {
                    Row(Modifier.fillMaxWidth().padding(horizontal = 20.dp), verticalAlignment = Alignment.CenterVertically) {
                        Text("Send when idle", style = MaterialTheme.typography.labelMedium, modifier = Modifier.weight(1f))
                        if (idleRecovery?.unresolved == true) TextButton(enabled = online && idleSupported && !sending, onClick = {
                            sending = true; idleError = null
                            scope.launch {
                                try {
                                    Deck.sendWhenIdle(sessionId, idleRecovery.text).fold(
                                        onSuccess = { if (it.state !in setOf("unknown", "pasted") && draft.text == idleRecovery.text) draft = TextFieldValue("") },
                                        onFailure = { idleError = it.message },
                                    )
                                } finally { sending = false }
                            }
                        }) { Text("Check original delivery") }
                    }
                    val explanation = if (!idleSupported) io.github.cepheus.app.protocol.Controls.UPDATE_NEEDED
                        else if (attachments.list.isNotEmpty()) "Idle delivery sends text only. Your attached files are retained for a normal send."
                        else idleRecoveryResult.exceptionOrNull()?.message ?: idleError ?: idleRecovery?.takeIf { it.unresolved }?.let { "Delivery unconfirmed. Check the original request before sending different text." }
                    explanation?.let { Text(it, color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(horizontal = 20.dp)) }
                }
                Composer(
                    focus = composerFocus,
                    text = if (inQuestion) response else draft,
                    onText = { if (inQuestion) response = it else draft = it },
                    onCommands = if (commandsShown && !inQuestion) {
                        {
                            val opened = draft.text + if (draft.text.isBlank() || draft.text.last().isWhitespace()) "/" else " /"
                            draft = TextFieldValue(opened, TextRange(opened.length))
                            composerFocus.requestFocus()
                            keyboard?.show()
                        }
                    } else {
                        null
                    },
                    model = session?.takeIf { it.tool.isNotEmpty() && it.tool != "dot" }?.let {
                        modelLabel(view?.model, view?.effort, toolModels, it.tool)
                    },
                    canSwitchModel = toolModels?.switch == true && session?.alive == true && online,
                    onModel = { showModels = true },
                    placeholder = "Reply to ${session?.let { Tools.label(it.tool) } ?: "the session"}…",
                    canSend = online && !sending && !answering && session?.alive == true &&
                        (inQuestion || !whenIdle || idleSupported && attachments.list.isEmpty()),
                    stopEnabled = online && !sending && !answering && session?.alive == true,
                    working = working,
                    canAttach = Tools.canAttach(session?.tool) && !whenIdle,
                    canStop = Tools.canStop(session?.tool),
                    attachments = if (inQuestion) emptyList() else attachments.list,
                    onAttach = { uris -> report(attachments.add(uris)) },
                    onRemove = attachments::remove,
                    onRetry = attachments::retry,
                    onSend = {
                        if (whenIdle) {
                            val submitted = draft
                            if (idleRecovery != null && !idleRecovery.unresolved && idleRecovery.text == submitted.text) repeatIdle = submitted
                            else sendIdle(submitted)
                        } else {
                        val attached = attachments.list
                        val files = Attachment.tokens(attached)
                        if (files != null) {
                            val text = draft.text.trim()
                            val names = attached.map { it.name }
                            val paths = attached.mapNotNull { (it.state as? Attachment.State.Ready)?.path }
                            sending = true
                            val sourceUris = attachments.sourceUris()
                            val clearing = files.isEmpty() && text in listOf("/clear", "/new") && commandsShown
                            if (clearing) draft = TextFieldValue("")
                            // Main.immediate enters send and persists its recovery copy before this draft is cleared.
                            scope.launch {
                                try {
                                    if (clearing) {
                                        Deck.clear(sessionId).fold(
                                            onSuccess = { replacement -> replacement?.let { onRelocated(sessionId, it) } },
                                            onFailure = { error ->
                                                if (draft.text.isEmpty()) draft = TextFieldValue(text)
                                                report(error.message ?: "The clear failed.")
                                            },
                                        )
                                    } else report(Deck.send(sessionId, text, files, names, paths, sourceUris))
                                }
                                finally { sending = false }
                            }
                            if (!clearing) {
                                draft = TextFieldValue("")
                                attachments.clear()
                            }
                        }
                        }
                    },
                    onStop = {
                        if (!stopping) {
                            stopping = true
                            scope.launch { report(Deck.stop(sessionId)); stopping = false }
                        }
                    },
                    onError = ::report,
                    question = when {
                        currentQuestion != null && inQuestion -> "Type your own answer…"
                        asking != null && inQuestion -> if (asking.prompt.freeText != null) "Type your own answer…" else "Choose an answer above"
                        else -> null
                    },
                    onAnswer = when {
                        currentQuestion != null && inQuestion && !currentQuestion.submitted && response.text.isNotBlank() -> {
                            { answerAsync(currentQuestion, text = response.text) }
                        }
                        asking != null && inQuestion && typedAnswer != null -> {
                            { val submitted = response; answer(asking, typedAnswer) { if (response == submitted) response = TextFieldValue("") } }
                        }
                        else -> null
                    },
                )
            }
        }
        SnackbarHost(snackbar, Modifier.align(Alignment.BottomCenter).padding(bottom = bottomHeight))
    }

    repeatIdle?.let { submitted -> AlertDialog(
        onDismissRequest = { repeatIdle = null }, title = { Text("Send another copy?") },
        text = { Text("The previous identical message already has a durable receipt. This sends a new delivery with its own identity.") },
        confirmButton = { TextButton(enabled = online && idleSupported && !sending, onClick = { repeatIdle = null; sendIdle(submitted, true) }) { Text("Send another copy") } },
        dismissButton = { TextButton(onClick = { repeatIdle = null }) { Text("Keep previous delivery") } },
    ) }

    if (showModels && toolModels != null && session != null) {
        ModelSheet(
            tool = toolModels,
            currentModel = view?.model,
            currentEffort = view?.effort,
            onDismiss = { showModels = false },
            onApply = { model, effort ->
                showModels = false
                scope.launch {
                    launch { snackbar.showSnackbar("Switching…") }
                    report(Deck.switchModel(sessionId, model, effort))
                }
            },
        )
    }
    if (rename) AlertDialog(
        onDismissRequest = { if (!renaming) rename = false }, title = { Text("Rename session") },
        text = { Column {
            OutlinedTextField(value = name, onValueChange = { name = it.take(200) }, label = { Text("Session name") }, singleLine = true)
            renameError?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        } },
        confirmButton = { TextButton(enabled = online && !renaming && name.isNotBlank(), onClick = {
            renaming = true
            scope.launch {
                Deck.control("rename", org.json.JSONObject().put("id", sessionId).put("name", name.trim()))
                    .onSuccess { rename = false }.onFailure { renameError = it.message }
                renaming = false
            }
        }) { Text(if (renaming) "Saving…" else "Save") } },
        dismissButton = { TextButton(enabled = !renaming, onClick = { rename = false }) { Text("Cancel") } },
    )
    if (confirmClose) {
        AlertDialog(
            onDismissRequest = { confirmClose = false },
            containerColor = deck.raised,
            title = { Text("Close this session?") },
            text = { Text("This ends the session and removes it from the deck.") },
            confirmButton = {
                TextButton(onClick = {
                    confirmClose = false
                    scope.launch {
                        val error = Deck.close(sessionId)
                        if (error == null) onClosed(sessionId) else report(error)
                    }
                }) { Text("Close") }
            },
            dismissButton = { TextButton(onClick = { confirmClose = false }) { Text("Cancel") } },
        )
    }
}

/**
 * The round menu button at the left, the session's title and state between,
 * and at the right the Claude app's pill: start a new session, and more.
 */
@Composable
private fun ChatHeader(
    title: String,
    status: Status?,
    branch: String?,
    alive: Boolean?,
    subagents: @Composable () -> Unit,
    onMenu: () -> Unit,
    onNew: () -> Unit,
    onClear: (() -> Unit)?,
    clearEnabled: Boolean,
    onClose: () -> Unit,
    onReopen: () -> Unit,
    onRename: () -> Unit,
    idleMode: Boolean,
    onIdleMode: (() -> Unit)?,
) {
    var menu by remember { mutableStateOf(false) }
    Row(
        Modifier.fillMaxWidth().padding(start = 8.dp, end = 12.dp, top = 6.dp, bottom = 2.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        FloatingIconButton(DeckIcons.Menu, "Menu", onMenu)
        Column(
            Modifier.weight(1f).padding(horizontal = 8.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text(
                title,
                style = MaterialTheme.typography.titleMedium,
                color = deck.text,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                textAlign = TextAlign.Center,
            )
            if (status != null) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    StateDot(statusColor(status), size = 7.dp)
                    Text(
                        status.label + (branch?.takeIf { it.isNotBlank() }?.let { " · $it" } ?: ""),
                        style = MaterialTheme.typography.labelMedium,
                        color = deck.textSecondary,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.padding(start = 6.dp),
                    )
                }
            }
            subagents()
        }
        FloatingPill {
            PillIconButton(DeckIcons.NewChat, "Start a new session", onNew)
            if (alive != null) Box {
                PillIconButton(DeckIcons.More, "More", { menu = true })
                DropdownMenu(
                    expanded = menu,
                    onDismissRequest = { menu = false },
                    shape = RoundedCornerShape(18.dp),
                    containerColor = deck.raised,
                ) {
                    DropdownMenuItem(text = { Text("Rename session") }, onClick = { menu = false; onRename() })
                    if (alive == true && onIdleMode != null) DropdownMenuItem(
                        text = { Text(if (idleMode) "Use normal delivery" else "Send when idle") },
                        onClick = { menu = false; onIdleMode() },
                    )
                    if (alive == true) {
                        if (onClear != null) {
                            DropdownMenuItem(
                                text = { Text("Clear") },
                                enabled = clearEnabled,
                                onClick = {
                                    menu = false
                                    onClear()
                                },
                            )
                        }
                        DropdownMenuItem(text = { Text("Close session") }, onClick = {
                            menu = false
                            onClose()
                        })
                    } else if (alive == false) {
                        DropdownMenuItem(text = { Text("Reopen session") }, onClick = {
                            menu = false
                            onReopen()
                        })
                    }
                }
            }
        }
    }
}

private data class MessageListContent(
    val view: ChatView,
    val outgoing: List<Outgoing>,
    val thinking: Boolean,
    val settled: Boolean,
)

/**
 * The chat, newest at the bottom: the log's rows, then messages sent from
 * here that the log does not show yet, then "Thinking…" while the agent works.
 */
@Composable
private fun MessageList(
    incoming: MessageListContent,
    replySpeech: ReplySpeech,
    onReplyError: (String) -> Unit,
    live: Boolean,
    answerEnabled: Boolean,
    workingLabel: String,
    workingSince: Long?,
    tool: String,
    alive: Boolean,
    top: Dp,
    bottom: Dp,
    /** Claude Code's send now for the queued messages, when the session can take it. */
    onSendNow: (() -> Unit)? = null,
    onRecover: (Outgoing) -> Unit,
) {
    val latest by rememberUpdatedState(incoming)
    var held by remember(incoming.view.id) { mutableStateOf<MessageListContent?>(null) }
    val (view, outgoing, thinking, settled) = held ?: incoming
    val rows = remember(view, outgoing) { chatRows(view, outgoing) }
    val entries = remember(rows) { chatEntries(rows) }
    val replies = remember(rows, settled) { completedReplies(rows, settled) }
    val listState = rememberLazyListState()
    var followingLatest by rememberSaveable(view.id) { mutableStateOf(true) }
    val follow = remember(view.id) { ConversationFollow(followingLatest) }
    val dragged by listState.interactionSource.collectIsDraggedAsState()
    var expandedCalls by rememberSaveable(view.id) { mutableStateOf(listOf<String>()) }
    LaunchedEffect(listState, follow) {
        snapshotFlow { dragged to !listState.canScrollForward }
            .collect { (scrolling, atLatest) ->
                follow.observe(scrolling, atLatest)
                followingLatest = follow.isFollowing
            }
    }
    if (rows.isEmpty() && outgoing.isEmpty() && !thinking) {
        EmptyChat(tool, view.source, alive, top, bottom)
        return
    }
    // The newest row is at the bottom; follow it while the owner is there.
    val newest = rows.lastOrNull()?.key
    val latestIndex = entries.size + (if (thinking) 1 else 0) + (if (view.hasOlder) 1 else 0)
    val viewportHeight by remember { derivedStateOf { listState.layoutInfo.viewportSize.height } }
    LaunchedEffect(
        newest, view.stream, view.items.lastOrNull(), outgoing, thinking, bottom, viewportHeight,
        listState.isScrollInProgress, held != null,
    ) {
        // A batch can preserve the previous visible key at a much larger index.
        // Follow the pre-update user intent and override that anchor on remeasure.
        if (held == null && follow.shouldFollow(listState.isScrollInProgress)) listState.requestScrollToItem(latestIndex)
    }
    // A message just sent comes into view wherever the owner had scrolled.
    val lastSent = outgoing.lastOrNull { it.error == null }?.key
    var observedSend by rememberSaveable(view.id) { mutableStateOf(lastSent) }
    LaunchedEffect(lastSent) {
        if (lastSent != null && lastSent != observedSend) { follow.resume(); listState.requestScrollToItem(latestIndex) }
        observedSend = lastSent
    }
    val nearTop by remember {
        derivedStateOf {
            val info = listState.layoutInfo
            val first = info.visibleItemsInfo.firstOrNull()?.index ?: 0
            info.totalItemsCount > 0 && first <= 2 && !followingLatest
        }
    }
    LaunchedEffect(nearTop, view.start) {
        if (nearTop && view.hasOlder) Deck.loadOlder()
    }
    val showLatest by remember { derivedStateOf { listState.canScrollForward } }
    val scope = rememberCoroutineScope()
    Box(Modifier.fillMaxSize()) {
        LazyColumn(
            state = listState,
            contentPadding = PaddingValues(top = top, bottom = bottom + 4.dp),
            modifier = Modifier.fillMaxSize().pointerInput(view.id) {
                awaitEachGesture {
                    awaitFirstDown(requireUnconsumed = false, pass = PointerEventPass.Initial)
                    // New rows can move a pressed card before the finger lifts,
                    // canceling its click or the start of a swipe. Keep the same
                    // content until this gesture ends, without consuming it.
                    follow.pause()
                    followingLatest = false
                    held = latest
                    try {
                        while (true) {
                            val event = awaitPointerEvent(PointerEventPass.Final)
                            if (event.changes.none { it.pressed }) break
                        }
                    } finally {
                        held = null
                    }
                }
            },
        ) {
            if (view.hasOlder) {
                item(key = "older") {
                    Box(Modifier.fillMaxWidth().padding(12.dp), contentAlignment = Alignment.Center) {
                        CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp, color = deck.textTertiary)
                    }
                }
            }
            items(entries, key = { it.key }) { entry ->
                when (entry) {
                    is ChatEntry.One -> {
                        val pendingRow = entry.row.outgoing
                        if (pendingRow != null) OutgoingBubble(pendingRow, onSendNow, onRecover = { onRecover(pendingRow) })
                        else ChatItem(entry.row, live, tool, view.id, answerEnabled) {
                            replies[entry.row.key]?.let { reply ->
                                ReplyActions(reply, view.generation.toString() + ":" + entry.row.key, replySpeech, onReplyError)
                            }
                        }
                    }
                    is ChatEntry.Tools -> ToolGroupRow(
                        entry, live, tool, view.id, answerEnabled,
                        open = entry.callKeys.any { it in expandedCalls },
                        onToggle = {
                            follow.pause()
                            followingLatest = false
                            val keys = entry.callKeys
                            expandedCalls = if (keys.any { it in expandedCalls }) expandedCalls - keys.toSet()
                                else (expandedCalls + keys).distinct()
                        },
                    )
                }
            }
            if (thinking) item(key = "thinking") { WorkingRow(workingLabel, workingSince) }
            item(key = "latest") { Spacer(Modifier.height(1.dp)) }
        }
        if (showLatest) {
            FloatingIconButton(
                DeckIcons.ArrowDown,
                "Latest messages",
                onClick = { follow.resume(); scope.launch { listState.animateScrollToItem(latestIndex) } },
                modifier = Modifier.align(Alignment.BottomCenter).padding(bottom = bottom + 6.dp),
            )
        }
    }
}

/**
 * An empty chat greets in the serif, as the Claude app opens a new chat,
 * instead of showing a program's start screen or a bare "No messages yet".
 */
@Composable
private fun EmptyChat(tool: String, source: String, alive: Boolean, top: Dp, bottom: Dp) {
    val name = Tools.label(tool).ifBlank { "the agent" }
    Column(
        Modifier.fillMaxSize().padding(top = top, bottom = bottom).padding(horizontal = 36.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(
            if (alive) "What should $name work on?" else "No messages yet.",
            style = MaterialTheme.typography.headlineMedium,
            color = deck.text,
            textAlign = TextAlign.Center,
        )
        val note = when {
            source == "none" || source == "screen" -> "This program's conversation can't be read yet."
            else -> null
        }
        if (note != null) {
            Text(
                note,
                style = MaterialTheme.typography.bodyMedium,
                color = deck.textSecondary,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(top = 12.dp),
            )
        }
    }
}

fun modelLabel(model: String?, effort: String?, tool: io.github.cepheus.app.protocol.ToolModels?, toolName: String): String {
    val name = model?.let { m -> tool?.models?.firstOrNull { it.id == m }?.label ?: m } ?: Tools.label(toolName)
    return if (effort.isNullOrBlank()) name else "$name · $effort"
}

/**
 * A question or approval the session waits on, as the Claude app's Claude
 * Code list shows a blocked session: a raised card under a gold hand, the
 * question, and each answer as a grey row a thumb can find. A several-choice
 * question ticks its boxes here and sends them with its own Submit. Set aside,
 * only its header stays, to open it again; its typed answer is kept.
 */
@Composable
private fun PromptCard(
    waiting: WaitingPrompt,
    maxHeight: Dp,
    busy: Boolean,
    enabled: Boolean,
    picks: Set<Int>,
    onPicks: (Set<Int>) -> Unit,
    onAnswer: (Answer) -> Unit,
    expanded: Boolean,
    onExpanded: (Boolean) -> Unit,
    modifier: Modifier = Modifier,
) {
    val prompt = waiting.prompt
    var freeOpen by rememberSaveable(waiting.fp) { mutableStateOf(false) }
    var freeText by rememberSaveable(waiting.fp) { mutableStateOf("") }
    val gold = statusColor(Status.NEEDS_YOU)
    val card = deck.raised
    val tappable = enabled && !busy
    val boxes = prompt.options.indices.filter { Answers.isBox(prompt, it) }
    // Boxes without a Submit row to press cannot be sent from here.
    val submit = prompt.submit
    val boxesSendable = submit != null
    val scroll = rememberScrollState()
    val scope = rememberCoroutineScope()
    Floating(modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp), shape = RoundedCornerShape(24.dp)) {
        Box {
            Column(Modifier.heightIn(max = maxHeight).verticalScroll(scroll).padding(16.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Box(
                        Modifier.size(32.dp).clip(RoundedCornerShape(10.dp)).background(gold.copy(alpha = 0.16f)),
                        contentAlignment = Alignment.Center,
                    ) {
                        Icon(Icons.Outlined.PanTool, contentDescription = null, tint = gold, modifier = Modifier.size(17.dp))
                    }
                    Text(
                        prompt.kind.replaceFirstChar { it.uppercase() }.ifBlank { "Question" } +
                            if (boxes.isNotEmpty()) " · choose any" else "",
                        style = MaterialTheme.typography.titleSmall,
                        color = deck.text,
                        modifier = Modifier.padding(start = 12.dp).weight(1f),
                    )
                    if (busy) CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp, color = deck.textTertiary)
                    IconButton(enabled = !busy, onClick = { onExpanded(!expanded) }) {
                        Icon(if (expanded) Icons.Outlined.ChatBubbleOutline else Icons.Outlined.QuestionAnswer,
                            if (expanded) "Message instead" else "Answer")
                    }
                }
                if (expanded) {
                    if (prompt.question.isNotBlank()) {
                        Text(
                            prompt.question,
                            style = MaterialTheme.typography.bodyLarge,
                            color = deck.text,
                            modifier = Modifier.padding(start = 2.dp, end = 2.dp, top = 12.dp, bottom = 4.dp),
                        )
                    }
                    for (i in boxes) {
                        val on = i in picks
                        OptionRow(
                            number = i + 1,
                            option = prompt.options[i],
                            ticked = on,
                            enabled = tappable && boxesSendable,
                            onClick = { onPicks(if (on) picks - i else picks + i) },
                            lead = {
                                Checkbox(
                                    checked = on,
                                    onCheckedChange = null,
                                    enabled = tappable && boxesSendable,
                                    colors = CheckboxDefaults.colors(checkedColor = deck.accent, checkmarkColor = deck.onAccent),
                                )
                            },
                        )
                        if (on && i == prompt.freeText) {
                            OutlinedTextField(
                                value = freeText,
                                onValueChange = { freeText = it },
                                placeholder = { Text("Your answer") },
                                shape = RoundedCornerShape(16.dp),
                                colors = answerFieldColors(),
                                modifier = Modifier.fillMaxWidth().padding(top = 8.dp),
                                maxLines = 4,
                            )
                        }
                    }
                    if (boxes.isNotEmpty()) {
                        val several = Answers.several(prompt, picks, freeText)
                        if (submit != null) {
                            Button(
                                onClick = { several?.let(onAnswer) },
                                enabled = tappable && several != null,
                                shape = RoundedCornerShape(50),
                                colors = accentButtonColors(),
                                modifier = Modifier.fillMaxWidth().padding(top = 12.dp).heightIn(min = 50.dp),
                            ) {
                                val count = several?.choices?.size ?: 0
                                Text(
                                    submit + if (count > 0) " · $count chosen" else "",
                                    style = MaterialTheme.typography.titleSmall,
                                )
                            }
                        } else {
                            Text(
                                "The deck did not find this question's Submit: answer it at the desk.",
                                style = MaterialTheme.typography.bodyMedium,
                                color = deck.textSecondary,
                                modifier = Modifier.padding(top = 10.dp),
                            )
                        }
                    }
                    // Options that answer on their own, such as Claude's "Chat about this".
                    for (i in prompt.options.indices.filter { it !in boxes }) {
                        OptionRow(
                            number = i + 1,
                            option = prompt.options[i],
                            cursor = boxes.isEmpty() && i == prompt.selected,
                            enabled = tappable,
                            onClick = { if (i == prompt.freeText) freeOpen = true else onAnswer(Answer(i, null)) },
                        )
                    }
                    val free = prompt.freeText
                    if (freeOpen && free != null && free !in boxes) {
                        Row(Modifier.padding(top = 10.dp), verticalAlignment = Alignment.CenterVertically) {
                            OutlinedTextField(
                                value = freeText,
                                onValueChange = { freeText = it },
                                placeholder = { Text("Your answer") },
                                shape = RoundedCornerShape(16.dp),
                                colors = answerFieldColors(),
                                modifier = Modifier.weight(1f),
                                maxLines = 4,
                            )
                            AccentCircleButton(
                                DeckIcons.ArrowUp,
                                "Send answer",
                                onClick = { onAnswer(Answer(free, freeText.trim())) },
                                enabled = tappable && freeText.isNotBlank(),
                                modifier = Modifier.padding(start = 4.dp),
                            )
                        }
                    }
                }
            }
            // More below: fade the cut edge so the card reads as scrolling, not clipped.
            if (scroll.canScrollForward) {
                Box(
                    Modifier
                        .align(Alignment.BottomCenter)
                        .fillMaxWidth()
                        .height(48.dp)
                        .clickable { scope.launch { scroll.animateScrollTo(scroll.maxValue) } }
                        .background(Brush.verticalGradient(listOf(card.copy(alpha = 0f), card))),
                    contentAlignment = Alignment.BottomCenter,
                ) {
                    Icon(
                        DeckIcons.ChevronDown,
                        contentDescription = "More below",
                        tint = deck.textSecondary,
                        modifier = Modifier.size(20.dp),
                    )
                }
            }
        }
    }
}

@Composable
private fun answerFieldColors() = OutlinedTextFieldDefaults.colors(
    focusedBorderColor = MaterialTheme.colorScheme.primary,
    unfocusedBorderColor = deck.border,
    cursorColor = deck.accent,
)






@Composable
private fun OptionRow(
    number: Int,
    option: PromptOption,
    enabled: Boolean,
    onClick: () -> Unit,
    ticked: Boolean = false,
    cursor: Boolean = false,
    lead: (@Composable () -> Unit)? = null,
) {
    val marked = ticked || cursor
    Surface(
        onClick = onClick,
        enabled = enabled,
        shape = RoundedCornerShape(18.dp),
        color = if (marked) deck.selected else deck.control,
        contentColor = deck.text,
        border = if (marked) BorderStroke(1.5.dp, MaterialTheme.colorScheme.primary) else null,
        modifier = Modifier.fillMaxWidth().padding(top = 8.dp),
    ) {
        Row(
            Modifier.heightIn(min = 52.dp).padding(horizontal = 14.dp, vertical = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            if (lead != null) {
                lead()
            } else {
                Text(
                    "$number",
                    style = MaterialTheme.typography.labelLarge,
                    color = deck.textTertiary,
                    textAlign = TextAlign.Center,
                    modifier = Modifier.size(width = 20.dp, height = 20.dp),
                )
            }
            Column(Modifier.padding(start = 10.dp).weight(1f)) {
                Text(option.label, style = MaterialTheme.typography.bodyLarge.copy(fontWeight = FontWeight.Medium))
                if (option.detail.isNotBlank()) {
                    Text(
                        option.detail,
                        style = MaterialTheme.typography.bodyMedium,
                        color = deck.textSecondary,
                    )
                }
            }
        }
    }
}

@Composable
private fun UnavailableSession(onHome: () -> Unit) {
    Floating(
        Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp),
        shape = RoundedCornerShape(24.dp),
    ) {
        Row(
            Modifier.padding(start = 18.dp, end = 12.dp, top = 12.dp, bottom = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(Modifier.weight(1f)) {
                Text("Session unavailable", style = MaterialTheme.typography.bodyLarge, color = deck.text)
                Text(
                    "Return Home to check the session list.",
                    style = MaterialTheme.typography.bodyMedium,
                    color = deck.textSecondary,
                )
            }
            TextButton(onClick = onHome) { Text("Home") }
        }
    }
}

@Composable
private fun EndedCard(model: String, onReopen: () -> Unit) {
    Floating(
        Modifier.fillMaxWidth().padding(start = 12.dp, end = 12.dp, top = 4.dp, bottom = 10.dp),
        shape = RoundedCornerShape(24.dp),
    ) {
        Row(
            Modifier.padding(start = 18.dp, end = 12.dp, top = 12.dp, bottom = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(Modifier.weight(1f).padding(end = 12.dp)) {
                Text("This session has ended.", style = MaterialTheme.typography.bodyLarge, color = deck.text)
                Text(
                    "Model: $model",
                    style = MaterialTheme.typography.labelMedium,
                    color = deck.textTertiary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            Button(
                onClick = onReopen,
                shape = RoundedCornerShape(50),
                colors = accentButtonColors(),
            ) { Text("Reopen") }
        }
    }
}

/**
 * The message box, as the Claude app's: a raised card with the draft on top
 * and one row beneath of a grey + circle, the grey model pill, the mic and the
 * blue send circle, which stops the session while it works and the box is
 * empty. The model pill never takes a row of its own; it shortens instead.
 */
@Composable
private fun Composer(
    focus: FocusRequester,
    text: TextFieldValue,
    onText: (TextFieldValue) -> Unit,
    onCommands: (() -> Unit)?,
    model: String?,
    canSwitchModel: Boolean,
    onModel: () -> Unit,
    placeholder: String,
    canSend: Boolean,
    stopEnabled: Boolean,
    working: Boolean,
    canAttach: Boolean,
    canStop: Boolean,
    attachments: List<Attachment>,
    onAttach: (List<Uri>) -> Unit,
    onRemove: (Long) -> Unit,
    onRetry: (Long) -> Unit,
    onSend: () -> Unit,
    onStop: () -> Unit,
    onError: (String?) -> Unit,
    /** The box's hint while a question waits; then only `onAnswer` sends. */
    question: String? = null,
    /** Sends the typed text as the waiting question's answer; null until it makes one. */
    onAnswer: (() -> Unit)? = null,
) {
    // A message goes once every file arrived; files alone are a message too.
    val ready = Attachment.tokens(attachments) != null && (text.text.isNotBlank() || attachments.isNotEmpty())
    val dictation = rememberLauncherForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
        if (result.resultCode == Activity.RESULT_OK) {
            val heard = result.data?.getStringArrayListExtra(RecognizerIntent.EXTRA_RESULTS)?.firstOrNull()
            if (!heard.isNullOrBlank()) {
                val spoken = if (text.text.isBlank()) heard else "${text.text.trimEnd()} $heard"
                onText(TextFieldValue(spoken, TextRange(spoken.length)))
            }
        }
    }
    val colors = deck
    Floating(
        Modifier.fillMaxWidth().padding(start = 10.dp, end = 10.dp, top = 4.dp, bottom = 8.dp),
        shape = RoundedCornerShape(26.dp),
    ) {
        // Controls and attachments measure first; only the draft scrolls.
        Column(Modifier.padding(start = 4.dp, end = 6.dp, top = 4.dp, bottom = 6.dp)) {
            if (attachments.isNotEmpty()) AttachmentChips(attachments, onRemove, onRetry)
            TextField(
                value = text,
                onValueChange = onText,
                placeholder = { Text(question ?: placeholder, color = colors.textTertiary) },
                textStyle = MaterialTheme.typography.bodyLarge,
                maxLines = 6,
                colors = TextFieldDefaults.colors(
                    focusedContainerColor = Color.Transparent,
                    unfocusedContainerColor = Color.Transparent,
                    disabledContainerColor = Color.Transparent,
                    focusedIndicatorColor = Color.Transparent,
                    unfocusedIndicatorColor = Color.Transparent,
                    disabledIndicatorColor = Color.Transparent,
                    cursorColor = colors.accent,
                    focusedTextColor = colors.text,
                    unfocusedTextColor = colors.text,
                ),
                modifier = Modifier.fillMaxWidth().weight(1f, fill = false).focusRequester(focus),
            )
            Row(verticalAlignment = Alignment.CenterVertically) {
                if (canAttach) AttachMenu(enabled = canSend && question == null, onAttach = onAttach, onError = onError, onCommands = onCommands)
                Box(Modifier.weight(1f).padding(start = 2.dp, end = 4.dp)) {
                    if (model != null) {
                        ControlPill(model, "Model: $model", onModel, enabled = canSwitchModel)
                    }
                }
                ControlCircleButton(DeckIcons.Mic, "Dictate", onClick = {
                    val intent = Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH)
                        .putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM)
                        .putExtra(RecognizerIntent.EXTRA_PROMPT, "Speak your message")
                    try {
                        dictation.launch(intent)
                    } catch (_: ActivityNotFoundException) {
                        onError("This phone has no speech recognizer.")
                    }
                })
                if (canStop && question == null && working && (text.text.isNotBlank() || attachments.isNotEmpty())) {
                    ControlCircleButton(DeckIcons.Stop, "Stop", onClick = onStop, enabled = stopEnabled)
                }
                if (canStop && question == null && working && text.text.isBlank() && attachments.isEmpty()) {
                    AccentCircleButton(DeckIcons.Stop, "Stop", onClick = onStop, enabled = stopEnabled)
                } else {
                    AccentCircleButton(
                        DeckIcons.ArrowUp,
                        if (question != null) "Send answer" else "Send",
                        onClick = onAnswer ?: onSend,
                        enabled = canSend && if (question != null) onAnswer != null else ready,
                    )
                }
            }
        }
    }
}
