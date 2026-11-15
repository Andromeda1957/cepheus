package io.github.cepheus.app

import android.content.Context
import android.content.Intent
import android.os.Build
import androidx.core.app.RemoteInput
import io.github.cepheus.app.markdown.Markdown
import io.github.cepheus.app.net.DeckException
import io.github.cepheus.app.net.GatewayClient
import io.github.cepheus.app.net.Link
import io.github.cepheus.app.notify.Notifier
import io.github.cepheus.app.notify.AttentionNotifications
import io.github.cepheus.app.pair.PairingTarget
import io.github.cepheus.app.protocol.AttentionContracts
import io.github.cepheus.app.store.AttentionDrafts
import io.github.cepheus.app.protocol.AlertKind
import io.github.cepheus.app.protocol.Answer
import io.github.cepheus.app.protocol.AnswerRefusal
import io.github.cepheus.app.protocol.Conversation
import io.github.cepheus.app.protocol.DeckState
import io.github.cepheus.app.protocol.Prompt
import io.github.cepheus.app.protocol.Protocol
import io.github.cepheus.app.protocol.Requests
import io.github.cepheus.app.protocol.ServerMessage
import io.github.cepheus.app.protocol.Session
import io.github.cepheus.app.protocol.Status
import io.github.cepheus.app.protocol.ToolModels
import io.github.cepheus.app.protocol.WaitingPrompt
import io.github.cepheus.app.service.ActionReceiver
import io.github.cepheus.app.service.DeckService
import io.github.cepheus.app.store.AlertBook
import io.github.cepheus.app.store.AppSettings
import io.github.cepheus.app.store.Cache
import io.github.cepheus.app.store.ComposerDrafts
import io.github.cepheus.app.store.ChatView
import io.github.cepheus.app.store.Credentials
import io.github.cepheus.app.store.ConnectionKeyValue
import io.github.cepheus.app.store.Connections
import io.github.cepheus.app.store.SavedConnection
import io.github.cepheus.app.store.Outbox
import io.github.cepheus.app.store.Outgoing
import io.github.cepheus.app.store.OutboxDrafts
import io.github.cepheus.app.store.Prefs
import io.github.cepheus.app.store.SessionOrder
import io.github.cepheus.app.store.SessionReadiness
import io.github.cepheus.app.upload.Attachments
import io.github.cepheus.app.upload.Source
import io.github.cepheus.app.upload.UploadLink
import io.github.cepheus.app.upload.Uploaded
import io.github.cepheus.app.upload.Uploader
import java.io.IOException
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull

/**
 * The app's one connection to the deck and what it has learned, shared by the
 * screens, the background service and the notification actions.
 */
object Deck {
    private const val PAGE = 60
    private const val PAUSE_AFTER_MS = 30_000L

    /** The resume list: the gateway waits up to 30 s for the deck to read every program's logs. */
    private const val LIST_TIMEOUT_MS = 45_000L

    /** A picture: megabytes, perhaps from another host. */
    private const val IMAGE_TIMEOUT_MS = 60_000L

    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    private lateinit var app: Context
    private lateinit var prefs: Prefs
    private lateinit var book: AlertBook
    private lateinit var cache: Cache
    private lateinit var client: GatewayClient
    private var connectionScope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private var sessionReadiness = SessionReadiness()
    lateinit var composerDrafts: ComposerDrafts
        private set
    /** User actions outlive an individual screen but remain bound to this connection. */
    val actions: CoroutineScope get() = CoroutineScope(connectionScope.coroutineContext + Dispatchers.Main.immediate)
    @Volatile private var connectionGeneration = 0L

    private val attachmentDrafts = mutableMapOf<String, MutableMap<Long, Attachments>>()
    fun attachments(id: Long): Attachments = attachmentDrafts
        .getOrPut(_connection.value?.id ?: Connections.LEGACY) { mutableMapOf() }
        .getOrPut(id) { Attachments(app, id, ConnectionKeyValue(prefs.keyValue, _connection.value?.id ?: Connections.LEGACY)) { error -> if (error != null) actions.launch { notices.emit(error) } } }

    private val _connections = MutableStateFlow<List<SavedConnection>>(emptyList())
    val connections: StateFlow<List<SavedConnection>> = _connections.asStateFlow()
    private val _connection = MutableStateFlow<SavedConnection?>(null)
    val connection: StateFlow<SavedConnection?> = _connection.asStateFlow()

    private val _credentials = MutableStateFlow<Credentials?>(null)
    val credentials: StateFlow<Credentials?> = _credentials.asStateFlow()

    private val _link = MutableStateFlow<Link>(Link.Unpaired)
    val link: StateFlow<Link> = _link.asStateFlow()

    private val _state = MutableStateFlow<DeckState?>(null)
    val state: StateFlow<DeckState?> = _state.asStateFlow()

    private val _prompts = MutableStateFlow<Map<Long, WaitingPrompt>>(emptyMap())
    val prompts: StateFlow<Map<Long, WaitingPrompt>> = _prompts.asStateFlow()

    private val _chat = MutableStateFlow<ChatView?>(null)
    val chat: StateFlow<ChatView?> = _chat.asStateFlow()

    private val _outbox = MutableStateFlow<Map<Long, List<Outgoing>>>(emptyMap())

    /** Messages this phone sent that their session's chat does not show yet, by session. */
    val outbox: StateFlow<Map<Long, List<Outgoing>>> = _outbox.asStateFlow()
    private val outKeys = AtomicLong()
    private lateinit var outgoingDrafts: OutboxDrafts
    lateinit var attentionDrafts: AttentionDrafts
        private set
    @Synchronized private fun saveOutgoing() = outgoingDrafts.save(_outbox.value)

    /** When each session last stopped being busy; absent while it is busy. */
    private val quietSince = ConcurrentHashMap<Long, Long>()
    private var outboxWatch: Job? = null

    private val _models = MutableStateFlow<List<ToolModels>>(emptyList())
    val models: StateFlow<List<ToolModels>> = _models.asStateFlow()

    private val _version = MutableStateFlow<String?>(null)
    val version: StateFlow<String?> = _version.asStateFlow()

    private val _settings = MutableStateFlow(AppSettings())
    val settings: StateFlow<AppSettings> = _settings.asStateFlow()

    /** One-line messages for the screen's snackbar. */
    val notices = MutableSharedFlow<String>(extraBufferCapacity = 16)

    @Volatile
    private var openId: Long? = null
    private var saveChat: Job? = null
    private val retainedChats = linkedMapOf<Long, ChatView>()
    private var initialized = false

    /** The app is on screen. */
    @Volatile
    private var shown = false

    /** Work that needs the connection while the app is closed: an upload, a notification's answer. */
    private val holds = AtomicInteger(0)
    private var pause: Job? = null

    @Synchronized
    fun init(context: Context) {
        if (initialized) return
        app = context.applicationContext
        prefs = Prefs(app)
        Notifier.createChannels(app)
        _settings.value = prefs.settings.load()
        activateConnection(prefs.connections.active(), connect = false)
        initialized = true
    }

    /** A new client and coroutine scope prevent late replies from crossing gateway boundaries. */
    @Synchronized
    private fun activateConnection(selected: SavedConnection?, connect: Boolean = true) {
        val generation = ++connectionGeneration
        if (::client.isInitialized) client.stop()
        connectionScope.cancel()
        connectionScope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        saveChat = null
        outboxWatch = null
        pause = null
        openId = null
        sessionReadiness = SessionReadiness()
        quietSince.clear()
        _prompts.value = emptyMap()
        _chat.value = null
        retainedChats.clear()
        _outbox.value = emptyMap()
        _models.value = emptyList()
        _version.value = null
        cache = Cache(app, selected?.id ?: Connections.LEGACY)
        val connectionBook = AlertBook(ConnectionKeyValue(prefs.keyValue, selected?.id ?: Connections.LEGACY))
        composerDrafts = ComposerDrafts(ConnectionKeyValue(prefs.keyValue, selected?.id ?: Connections.LEGACY))
        outgoingDrafts = OutboxDrafts(ConnectionKeyValue(prefs.keyValue, selected?.id ?: Connections.LEGACY))
        attentionDrafts = AttentionDrafts(ConnectionKeyValue(prefs.keyValue, selected?.id ?: Connections.LEGACY))
        _outbox.value = outgoingDrafts.load()
        outKeys.set(_outbox.value.values.flatten().maxOfOrNull { it.key } ?: 0)
        book = connectionBook
        _state.value = if (selected == null) null else cache.loadState()
        sessionReadiness = SessionReadiness(ConnectionKeyValue(prefs.keyValue, selected?.id ?: Connections.LEGACY), _state.value)
        _link.value = if (selected == null) Link.Unpaired else Link.Connecting
        client = GatewayClient(
            scope = connectionScope,
            credentials = { selected?.credentials },
            alertsHeld = { connectionBook.lastSeq to connectionBook.epoch },
            onLink = { next -> synchronized(this) {
                if (generation == connectionGeneration) {
                    if (next != Link.Online) {
                        sessionReadiness.disconnected()
                        _chat.update { it?.copy(live = false, loadingOlder = false) }
                    }
                    _link.value = next
                }
            } },
            onMessage = { message -> synchronized(this) {
                if (generation == connectionGeneration) onMessage(message)
            } },
        )
        _connections.value = prefs.connections.all()
        _credentials.value = selected?.credentials
        _connection.value = selected
        if (connect && selected != null) {
            start()
            if (_settings.value.background) DeckService.start(app)
        }
    }

    /** Available even when the previous gateway is unreachable. */
    @Synchronized
    fun selectConnection(id: String): String? {
        if (composerDrafts.hasBusy) return "Wait for the current action to finish before switching connections."
        if (holds.get() > 0) return "Wait for the current upload or action to finish before switching connections."
        if (id == _connection.value?.id) return null
        if (!prefs.connections.select(id)) return "That connection is no longer saved."
        Notifier.cancelSessionAlerts(app)
        activateConnection(prefs.connections.active())
        return null
    }

    val paired: Boolean get() = _credentials.value != null

    /** The app asked once to be exempt from battery optimization. */
    var batteryAsked: Boolean
        get() = prefs.batteryAsked
        set(value) {
            prefs.batteryAsked = value
        }

    /**
     * The machine and program the last session started with, which New
     * session selects next time. The machine is null for this deck's own.
     */
    var lastStart: Pair<String?, String>
        get() = prefs.keyValue.getString(LAST_HOST) to (prefs.keyValue.getString(LAST_TOOL) ?: "claude")
        set(value) {
            prefs.keyValue.putString(LAST_HOST, value.first)
            prefs.keyValue.putString(LAST_TOOL, value.second)
        }

    private const val LAST_HOST = "new_session_host"
    private const val LAST_TOOL = "new_session_tool"

    fun start() {
        if (paired) client.start()
    }

    // ---- when to stay connected ----
    //

    // off by default for the app, and you can enable it in the app settings".
    // Off, the app reaches the deck only while it is open, with no ongoing
    // notification; on, DeckService keeps the connection, also after a restart.

    /** The app came on screen ([io.github.cepheus.app.ui.MainActivity] onStart). */
    fun appShown() {
        shown = true
        pause?.cancel()
        start()
        if (_settings.value.background) DeckService.start(app)
    }

    /** The app left the screen: without the background connection it disconnects soon after. */
    fun appHidden() {
        shown = false
        schedulePause()
    }

    /**
     * Leaving the screen for the photo picker, the camera or a quick switch
     * of apps keeps the connection; it ends [PAUSE_AFTER_MS] after the app
     * was last seen, once no upload or answer still needs it.
     */
    private fun schedulePause() {
        pause?.cancel()
        if (_settings.value.background || shown) return
        pause = connectionScope.launch {
            delay(PAUSE_AFTER_MS)
            if (!shown && !_settings.value.background && holds.get() == 0) {
                client.stop()
                _link.value = if (paired) Link.Connecting else Link.Unpaired
            }
        }
    }

    /** Keep the connection for `block`, also while the app is closed. */
    private suspend fun <T> holding(expectedConnection: String? = _connection.value?.id, block: suspend () -> T): T {
        synchronized(this) {
            if (expectedConnection != _connection.value?.id) throw DeckException("The connection changed. Try again on the selected machine.")
            holds.incrementAndGet()
            pause?.cancel()
            start()
        }
        try {
            return block()
        } finally {
            synchronized(this) {
                holds.decrementAndGet()
                schedulePause()
            }
        }
    }

    fun updateSettings(change: (AppSettings) -> AppSettings) {
        val before = _settings.value
        val after = change(before)
        if (after == before) return
        prefs.settings.save(after)
        _settings.value = after
        if (after.background != before.background) {
            if (after.background) {
                DeckService.start(app)
            } else {
                app.stopService(Intent(app, DeckService::class.java))
                schedulePause()
            }
        }
    }

    fun kick() = client.kick()

    /** Try again now, also after the gateway refused this phone. */
    fun reconnect() {
        client.stop()
        client.start()
        client.kick()
    }

    // ---- pairing ----

    suspend fun pair(target: PairingTarget, name: String = ""): String? {
        val generation = connectionGeneration
        return try {
            val creds = GatewayClient.pair(target, phoneName())
            synchronized(this) {
                if (holds.get() > 0 || composerDrafts.hasBusy || generation != connectionGeneration) {
                    "The connection is in use or has changed. Try adding this machine again."
                } else {
                    val selected = prefs.connections.save(creds, name)
                    Notifier.cancelSessionAlerts(app)
                    activateConnection(selected)
                    null
                }
            }
        } catch (e: DeckException) {
            e.message
        } catch (e: IllegalArgumentException) {
            e.message
        }
    }

    @Synchronized
    fun unpair() {
        if (holds.get() > 0 || composerDrafts.hasBusy) {
            notices.tryEmit("Wait for the current upload or action to finish before removing this connection.")
            return
        }
        val selected = _connection.value ?: return
        prefs.connections.remove(selected.id)
        cache.clear()
        Notifier.cancelSessionAlerts(app)
        activateConnection(prefs.connections.active())
        if (!paired) {
            app.stopService(Intent(app, DeckService::class.java))
            io.github.cepheus.app.store.Secrets.forget()
        }
    }

    /** The name this phone pairs under, as the deck's Alt+p screen lists it. */
    fun phoneName(): String =
        listOf(Build.MANUFACTURER, Build.MODEL).filter { it.isNotBlank() }.distinct().joinToString(" ").ifBlank { "Phone" }

    // ---- what the gateway sends ----

    private fun onMessage(msg: ServerMessage) {
        when (msg) {
            is ServerMessage.Hello -> {
                book.hello(msg.epoch)
                _version.value = msg.version
            }
            is ServerMessage.State -> onState(msg.state)
            is ServerMessage.Alert -> onAlert(msg)
            is ServerMessage.PromptUpdate -> onPrompt(msg)
            is ServerMessage.Items -> {
                if (msg.id != openId || sessionReadiness.error(msg.id) != null) return
                _chat.update { view -> view?.takeIf { it.id == msg.id }?.withItems(msg) }
                settleOutbox()
                scheduleChatSave()
            }
            is ServerMessage.Chat -> {
                if (sessionReadiness.error(msg.id) != null) return
                if (msg.id == openId) _chat.update { view -> view?.takeIf { it.id == msg.id }?.withPage(msg) }
                settleOutbox()
            }
            is ServerMessage.Notice -> notices.tryEmit(msg.text)
            is ServerMessage.Err -> notices.tryEmit(msg.error)
            is ServerMessage.Models -> _models.value = msg.tools
            else -> {}
        }
    }

    private fun onState(state: DeckState) {
        state.attention?.let { attentionDrafts.observe(it.messages) }
        val wasReady = sessionReadiness.ready
        val previous = _state.value
        val previousError = openId?.let { sessionReadiness.error(it) }
        val moved = sessionReadiness.accept(state)
        _state.value = state
        // Read the open chat again once it is listed, ready, usable again, or
        // its session runs a new process or conversation.
        openId?.takeIf { id -> state.ready && sessionReadiness.error(id) == null &&
            (!wasReady || previous?.sessions?.none { it.id == id } != false || previousError != null || id in moved)
        }?.let { id -> connectionScope.launch { requestOpen(id) } }
        if (state.ready && !wasReady) connectionScope.launch { fetchModels() }
        if (openId?.let { sessionReadiness.error(it) } != null) {
            _chat.update { it?.copy(live = false, loadingOlder = false) }
        }
        val now = System.currentTimeMillis()
        for (s in state.sessions) {
            if (Outbox.busy(s.status)) quietSince.remove(s.id) else quietSince.putIfAbsent(s.id, now)
        }
        settleOutbox()
        // This callback already runs off the UI thread under Deck's lock.
        // Save in receive order, also before a connection can reload its cache.
        cache.saveState(Protocol.stateJson(state))
        // A session that is no longer waiting has no prompt to answer.
        val waiting = AttentionNotifications.waiting(state)
        for (id in _prompts.value.keys - waiting) {
            dropPrompt(id, cancelNotification = false)
        }
        Notifier.reconcileAttention(app, state)
        for (s in state.sessions) {
            if (s.status == Status.WORKING) Notifier.cancelFinished(app, s.id)
        }
    }








    private fun onAlert(alert: ServerMessage.Alert) {
        alert.message?.takeIf { alert.kind == AlertKind.ERROR }?.let { refused(it, alert.reason) }
        if (!book.accept(alert, System.currentTimeMillis())) return
        val title = alert.title.ifBlank { alert.id?.let { "Session #$it" } ?: "Cepheus" }
        val notify = _settings.value
        when (alert.kind) {
            AlertKind.FINISHED -> alert.id?.takeIf { !alert.replay && notify.notifyFinished }?.let { id ->
                Notifier.finished(app, id, title, alert.reason.ifBlank { "Finished" }, alert.deskFocused)
            }
            AlertKind.NEEDS_YOU -> alert.id?.takeIf { !alert.replay && notify.notifyNeedsYou }?.let { id ->
                val waiting = _prompts.value[id]
                if (AttentionNotifications.prompts(_state.value, id, waiting != null)) {
                    Notifier.needsYou(app, id, title, promptText(waiting?.prompt, alert.reason), waiting, alert.deskFocused)
                }
            }
            AlertKind.ERROR -> if (notify.notifyErrors) {
                Notifier.error(app, alert.seq, "Cepheus error", alert.reason.ifBlank { title }, alert.deskFocused)
            }
            AlertKind.UNKNOWN -> {}
        }
    }

    private fun onPrompt(msg: ServerMessage.PromptUpdate) {
        val waiting = msg.waiting
        if (waiting == null) {
            dropPrompt(msg.id)
            return
        }
        _prompts.update { it + (msg.id to waiting) }
        val fresh = book.promptIsNew(msg.id, msg.fp)
        if (fresh && !msg.replay && _state.value?.attentionCapable != true && _settings.value.notifyNeedsYou) {
            val title = session(msg.id)?.title?.ifBlank { null } ?: "Session #${msg.id}"
            Notifier.needsYou(app, msg.id, title, promptText(waiting.prompt, ""), waiting, silent = false)
        }
    }

    private fun dropPrompt(id: Long, cancelNotification: Boolean = true) {
        if (_prompts.value.containsKey(id)) _prompts.update { it - id }
        book.promptGone(id)
        if (cancelNotification) Notifier.cancelNeedsYou(app, id)
    }

    private fun promptText(prompt: Prompt?, reason: String): String {
        if (prompt == null) return reason.ifBlank { "Waiting for you" }
        val question = Markdown.plain(prompt.question).trim().ifBlank { prompt.kind.replaceFirstChar { it.uppercase() } }
        val options = prompt.options.mapIndexed { i, o -> "${i + 1}. ${o.label}" }.joinToString("\n")
        return if (options.isEmpty()) question else "$question\n$options"
    }

    fun session(id: Long): Session? = _state.value?.sessions?.firstOrNull { it.id == id }

    // ---- chat ----

    /** Follow `id`'s chat (or none): the saved copy at once, then the gateway's. */
    fun open(id: Long?) {
        if (openId == id && _chat.value?.id == id && id != null) {
            if (client.online && _chat.value?.live != true) connectionScope.launch { requestOpen(id) }
            return
        }
        _chat.value?.let { previous ->
            retainedChats.remove(previous.id)
            retainedChats[previous.id] = previous.copy(live = false, loadingOlder = false)
        }
        val restored = id?.let { retainedChats.remove(it) }
        // Bound inactive context to one retained transcript's item budget and eight sessions.
        while (retainedChats.size > 8 || retainedChats.values.sumOf { it.items.size } > 3_000) {
            retainedChats.remove(retainedChats.keys.first())
        }
        openId = id
        _chat.value = id?.let { restored ?: cache.loadChat(it)?.copy(live = false) ?: ChatView(id) }
        connectionScope.launch { requestOpen(id) }
        id?.let { Notifier.cancelFinished(app, it) }
    }

    /** Read the open chat again, as after the deck withdrew a question it refused. */
    fun refreshChat(id: Long) {
        if (openId == id) connectionScope.launch { requestOpen(id) }
    }

    private suspend fun requestOpen(id: Long?) {
        if (!client.online) return
        val marker = sessionStateMarker()
        try {
            val page = client.request { checkedRequest(Requests.open(it, id, PAGE)) }
            if (page is ServerMessage.Chat && page.id == openId && isCurrentSession(page.id, marker)) {
                _chat.update { view -> view?.takeIf { it.id == page.id }?.withPage(page) }
                settleOutbox()
                scheduleChatSave()
            }
        } catch (e: DeckException) {
            if (id != null && id == openId) notices.tryEmit(e.message ?: "Could not open the chat.")
        }
    }

    fun loadOlder() {
        val view = _chat.value ?: return
        if (!view.hasOlder || view.loadingOlder || !view.live || !client.online) return
        _chat.value = view.copy(loadingOlder = true)
        val marker = sessionStateMarker()
        connectionScope.launch {
            try {
                val page = client.request { checkedRequest(Requests.chat(it, view.id, view.start, PAGE)) }
                if (page is ServerMessage.Chat && page.id == view.id && isCurrentSession(page.id, marker)) {
                    _chat.update { v -> if (v?.id == page.id) v.withOlder(page) else v }
                } else {
                    _chat.update { it?.copy(loadingOlder = false) }
                }
            } catch (e: DeckException) {
                _chat.update { it?.copy(loadingOlder = false) }
                notices.tryEmit(e.message ?: "Could not load older messages.")
            }
        }
    }

    private fun scheduleChatSave() {
        saveChat?.cancel()
        val destination = cache
        val view = _chat.value?.takeIf { it.live } ?: return
        saveChat = connectionScope.launch(Dispatchers.IO) {
            delay(2_000)
            destination.saveChat(view)
        }
    }

    // ---- requests ----

    /** Runs a request; null on success, else the error to show. */
    private suspend fun act(timeoutMs: Long = GatewayClient.REQUEST_TIMEOUT_MS, build: (Long) -> String): String? = try {
        client.request(timeoutMs) { checkedRequest(build(it)) }
        null
    } catch (e: DeckException) {
        e.message ?: "The request failed."
    }

    /** Validate against this connection's fresh list immediately before sending. */
    private fun checkedRequest(raw: String): String {
        val request = org.json.JSONObject(raw)
        sessionReadiness.messageError(request)?.let { throw DeckException(it) }
        fun check(id: Long) {
            sessionReadiness.error(id)?.let { throw DeckException(it) }
        }
        // Terminal and scan IDs belong to their own control domains, outside the agent list.
        if (request.optString("t") != "control" || request.optString("action") == "rename") {
            (request.opt("id") as? Number)?.toLong()?.let(::check)
        } else if (request.optString("action") == "broadcast") {
            request.optJSONArray("ids")?.let { ids -> for (i in 0 until ids.length()) check(ids.getLong(i)) }
        }
        return raw
    }

    /**
     * A message, with the tokens of files already uploaded for it; the deck
     * delivers their paths with it. The chat shows it at once (`names` and
     * `paths` are the files'), until the session's log does.
     */
    suspend fun send(
        id: Long,
        text: String,
        files: List<String> = emptyList(),
        names: List<String> = emptyList(),
        paths: List<String> = emptyList(),
        uris: List<String> = emptyList(),
    ): String? {
        val targetTool = session(id)?.tool
        val view = _chat.value?.takeIf { it.id == id }
        val out = Outgoing(
            key = outKeys.incrementAndGet(),
            clientId = java.util.UUID.randomUUID().toString(),
            conversation = session(id)?.conversation,
            correlated = targetTool == "codex",
            text = text,
            files = names,
            tokens = files,
            uris = uris,
            paths = paths,
            generation = view?.generation ?: 0,
            after = view?.takeUnless { it.source == "screen" }?.total ?: 0,
            sentAt = System.currentTimeMillis(),
            delivered = false,
            queued = session(id)?.status == Status.WORKING,
        )
        addOutgoing(id, out)
        // A request refused before it left the phone was certainly not sent.
        var dispatched = false
        var receipt: String? = null
        var hostAt: Long? = null
        var sentPaths: List<String>? = null
        val error = try {
            // Claude's log carries no identity, but a refusal after the deck
            // took the message names it to mark this copy.
            val identity = out.clientId.takeIf { out.correlated || targetTool == "claude" }
            val reply = client.request(45_000L) { checkedRequest(Requests.send(it, id, text, files, identity)).also { dispatched = true } }
            val accepted = reply as? ServerMessage.Ok
            if (accepted?.clientId != null && accepted.clientId != out.clientId) throw DeckException("The delivery receipt has a different message identity. Check the conversation before retrying.")
            receipt = accepted?.delivered
            hostAt = accepted?.at
            sentPaths = accepted?.paths
            null
        } catch (e: DeckException) {
            e.message ?: "The request failed."
        }
        _outbox.update { box ->
            val list = box[id] ?: return@update box
            val next = if (error == null) {
                list.map {
                    if (it.key == out.key) Outbox.acknowledged(it, targetTool, System.currentTimeMillis(), receipt, hostAt, sentPaths)
                    else it
                }
            } else {
                list.map { if (it.key == out.key) it.copy(error = error, knownUnsent = !dispatched) else it }
            }
            if (next.isEmpty()) box - id else box + (id to next)
        }
        saveOutgoing()
        settleOutbox()
        return error
    }

    /** Idempotent private queue admission. A timeout keeps the original key and payload. */
    suspend fun sendWhenIdle(id: Long, text: String, newCopy: Boolean = false): Result<AttentionContracts.Message> = runCatching {
        require(_state.value?.attentionCapable == true) { io.github.cepheus.app.protocol.Controls.UPDATE_NEEDED }
        val connection = _connection.value?.id
        val drafts = attentionDrafts
        val previous = drafts.pending(id)?.takeIf { it.text == text && !newCopy }
        val session = _state.value?.sessions?.firstOrNull { it.id == id }
        if (previous == null) require(session?.let { it.alive && it.tool != "shell" } == true) {
            "Choose a live agent session for idle delivery."
        }
        // Capture once before any request. A replay must never infer a replacement.
        val target = (if (previous != null) previous.target else session?.expectedTarget)
            ?: throw IllegalArgumentException(io.github.cepheus.app.protocol.Controls.UPDATE_NEEDED)
        val originalHost = if (previous != null) previous.origin
            ?: throw IllegalArgumentException(io.github.cepheus.app.protocol.Controls.UPDATE_NEEDED)
            else session?.host ?: "@here"
        AttentionContracts.send("validate", id, text, originalHost, target)
        val pending = drafts.prepare(id, text, origin = originalHost, newCopy = newCopy, target = target)
        holding(connection) {
            val capabilities = client.request { Requests.control(it, "fleet", AttentionContracts.request("capabilities")) }
            val supported = io.github.cepheus.app.protocol.Controls.data("fleet", capabilities)
            require(AttentionContracts.idleSendSupported(supported)) { io.github.cepheus.app.protocol.Controls.UPDATE_NEEDED }
            val reply = client.request(45_000L) { checkedRequest(Requests.control(it, "fleet", AttentionContracts.send(pending.key, pending.session, pending.text, pending.origin, pending.target))) }
            val data = io.github.cepheus.app.protocol.Controls.data("fleet", reply)
            AttentionContracts.message(data).let { drafts.received(pending, it) }
        }
    }

    /** The deck reports that the program refused the message the phone sent as `clientId`. */
    private fun refused(clientId: String, error: String) {
        val before = _outbox.value
        _outbox.update { box -> box.mapValues { (_, list) -> Outbox.refused(list, clientId, error) } }
        if (_outbox.value != before) saveOutgoing()
    }

    fun dismissFailed(id: Long, key: Long) {
        _outbox.update { box -> box + (id to box[id].orEmpty().filterNot { it.key == key && it.error != null }) }
        saveOutgoing()
    }

    private fun addOutgoing(id: Long, out: Outgoing) {
        _outbox.update { it + (id to it[id].orEmpty() + out) }
        saveOutgoing()
        watchOutbox()
    }

    /** Drop what the logs now show, and messages the deck took that no log showed in time. */
    private fun settleOutbox() {
        if (_outbox.value.isEmpty() || !sessionReadiness.ready) return
        val now = System.currentTimeMillis()
        val view = _chat.value?.takeIf { it.live }
        val sessions = _state.value?.sessions
        val before = _outbox.value
        _outbox.update { box ->
            box.mapNotNull { (id, list) ->
                if (sessionReadiness.error(id) != null) return@mapNotNull id to list
                val s = sessions?.firstOrNull { it.id == id }
                // An ended or closed session takes no more messages.
                if (sessions != null && s?.alive != true) {
                    val recovery = list.map { if (it.submitted || it.delivered) it else it.copy(error = it.error ?: "This session ended before delivery was confirmed.") }
                    return@mapNotNull id to recovery
                }
                val (current, held) = list.partition { it.conversation == null || it.conversation == s?.conversation }
                val left = (held + Outbox.settle(current, view?.takeIf { it.id == id }, quietSince[id], now)).sortedBy { it.key }
                if (left.isEmpty()) null else id to left
            }.toMap()
        }
        if (_outbox.value != before) saveOutgoing()
    }

    /** While a message waits, look again each second, so an unseen one leaves on time. */
    @Synchronized
    private fun watchOutbox() {
        if (outboxWatch != null) return
        outboxWatch = connectionScope.launch {
            do {
                delay(1_000)
                settleOutbox()
            } while (!watchEnds())
        }
    }

    /** The watch ends once nothing waits; a message added meanwhile keeps it going. */
    @Synchronized
    private fun watchEnds(): Boolean {
        if (_outbox.value.values.any { rows -> rows.any { !it.submitted && it.error == null } }) return false
        outboxWatch = null
        return true
    }

    private val uploadLink = object : UploadLink {
        override suspend fun request(build: (Long) -> String) = client.request { checkedRequest(build(it)) }
        override fun sendBinary(build: (Long) -> ByteArray) = client.sendBinary(build = build)
    }


    suspend fun upload(id: Long, source: Source, progress: (Long) -> Unit): Result<Uploaded> = holding {
        try {
            Result.success(withContext(Dispatchers.IO) { Uploader.upload(uploadLink, id, source, progress) })
        } catch (e: DeckException) {
            Result.failure(e)
        } catch (e: IOException) {
            Result.failure(DeckException("Could not read ${source.name}: ${e.message ?: "the phone refused"}"))
        } catch (e: SecurityException) {
            Result.failure(DeckException("Could not read ${source.name}: the phone refused"))
        }
    }

    /**
     * Keep the sessions in the order `ids` gives, as the deck's Super+↑/↓
     * does: shown at once, and put back if the deck refuses.
     */
    fun reorder(ids: List<Long>) {
        val before = _state.value ?: return
        val was = before.sessions.map { it.id }
        if (ids == was) return
        _state.value = before.copy(sessions = SessionOrder.arrange(before.sessions, ids))
        connectionScope.launch {
            val error = act { Requests.reorder(it, ids) } ?: return@launch
            _state.update { now -> now?.copy(sessions = SessionOrder.arrange(now.sessions, was)) }
            notices.tryEmit("Could not reorder: $error")
        }
    }

    /** Validated, correlated Deck controls; long operations may outlive an agent request. */
    suspend fun control(action: String, args: org.json.JSONObject = org.json.JSONObject()): Result<org.json.JSONObject> = holding {
        try {
            val marker = sessionStateMarker()
            val timeout = when (action) {
                "install", "skills_sync", "scan" -> 360_000L
                "ask" -> 240_000L
                "scripts", "plugin", "service", "dev" -> 180_000L
                else -> 45_000L
            }
            val reply = client.request(timeout) { checkedRequest(Requests.control(it, action, args)) }
            val data = io.github.cepheus.app.protocol.Controls.data(action, reply)
            if (action == "create") (data.opt("id") as? Number)?.toLong()?.let { awaitSessionState(it, marker) }
            Result.success(data)
        } catch (e: DeckException) {
            val message = io.github.cepheus.app.protocol.Controls.errorMessage(e.message ?: "The deck control failed.")
            Result.failure(DeckException(message))
        } catch (e: IllegalArgumentException) {
            Result.failure(e)
        }
    }

    /** The Stats screen's numbers, or null while the deck is out of reach. */
    suspend fun stats(): ServerMessage.Stats? = try {
        client.request { Requests.stats(it) } as? ServerMessage.Stats
    } catch (_: DeckException) {
        null
    }

    suspend fun replyToQuestion(id: Long, question: String, choice: Int? = null, text: String? = null): AnswerRefusal? =
        refusal { Requests.questionReply(it, id, question, choice, text) }

    suspend fun answer(waiting: WaitingPrompt, answer: Answer): AnswerRefusal? {
        val refused = refusal { Requests.answer(it, waiting, answer.choice, answer.text, answer.choices) }
        if (refused == null) Notifier.cancelNeedsYou(app, waiting.id)
        return refused
    }

    /** An answer request; the deck's refusal says by its code whether the question still waits. */
    private suspend fun refusal(build: (Long) -> String): AnswerRefusal? = try {
        client.request(GatewayClient.REQUEST_TIMEOUT_MS) { checkedRequest(build(it)) }
        null
    } catch (e: DeckException) {
        AnswerRefusal(e.message ?: "The request failed.", e.code == Protocol.QUESTION_STALE)
    }

    suspend fun stop(id: Long) = act { Requests.stop(it, id) }

    /**
     * Claude Code's send now in session `id`: the deck presses its key, and
     * the messages queued behind the running turn reach Claude now. Their
     * bubbles say so at once and say Queued again if the deck refuses.
     */
    suspend fun sendNow(id: Long): String? {
        val keys = Outbox.sendNowTakes(_outbox.value[id].orEmpty())
        markSendingNow(id, keys, true)
        val error = act { Requests.sendNow(it, id) }
        if (error != null) markSendingNow(id, keys, false)
        return error
    }

    private fun markSendingNow(id: Long, keys: Set<Long>, on: Boolean) = _outbox.update { box ->
        val list = box[id] ?: return@update box
        box + (id to Outbox.sendingNow(list, keys, on))
    }

    suspend fun close(id: Long) = act { Requests.close(it, id) }

    /** A fresh-host receipt replaces only this connection's matching chat. */
    suspend fun clear(id: Long): Result<Long?> = holding {
        val (expectedConnection, marker, publication) = synchronized(this@Deck) {
            val marker = sessionStateMarker()
            Triple(_connection.value?.id, marker, marker.readiness.publicationMarker())
        }
        try {
            val reply = client.request(75_000L) { request -> synchronized(this@Deck) {
                if (!isCurrentSession(id, marker)) throw DeckException("The connection or session changed before clearing.")
                checkedRequest(Requests.clear(request, id))
            } }
            if (!isCurrentConnection(marker) || _connection.value?.id != expectedConnection) {
                return@holding Result.failure(DeckException("The connection changed while clearing; the new session was left untouched."))
            }
            val replacement = (reply as? ServerMessage.Ok)?.id?.takeIf { it != id }
            if (replacement != null) {
                val published = marker.readiness.awaitPublishedTarget(replacement, 10_000L, publication)
                return@holding synchronized(this@Deck) {
                    if (published == null || !isCurrentConnection(marker) || _connection.value?.id != expectedConnection ||
                        !marker.readiness.acknowledgePublishedTarget(published, publication) || !isCurrentSession(replacement, marker)) {
                        Result.failure(DeckException("Fresh session $replacement has not reached this connection. Your original draft and attachments were retained."))
                    } else {
                        val attachments = attachmentDrafts[expectedConnection ?: Connections.LEGACY]
                        if (attachments?.get(replacement)?.list?.isNotEmpty() == true || attachments?.get(id)?.canRebind(replacement) == false || !composerDrafts.copyMessage(id, replacement)) {
                            Result.failure(DeckException("Fresh session $replacement is available. Both existing drafts were retained."))
                        } else {
                            if (attachments != null) {
                                attachments.remove(id)?.let { original ->
                                    original.rebind(replacement)
                                    attachments[replacement] = original
                                }
                            }
                            Result.success(replacement)
                        }
                    }
                }
            }
            Result.success(replacement)
        } catch (e: DeckException) {
            Result.failure(e)
        }
    }

    suspend fun reopen(id: Long): String? = holding {
        val (expectedConnection, marker, publication) = synchronized(this@Deck) {
            val marker = sessionStateMarker()
            Triple(_connection.value?.id, marker, marker.readiness.publicationMarker())
        }
        try {
            val reply = client.request(GatewayClient.SLOW_TIMEOUT_MS) { request -> synchronized(this@Deck) {
                if (!isCurrentSession(id, marker)) throw DeckException("The connection or session changed before reopening.")
                checkedRequest(Requests.reopen(request, id))
            } }
            if ((reply as? ServerMessage.Ok)?.id != id) throw DeckException("The deck did not confirm this reopened session.")
            val published = marker.readiness.awaitPublishedTarget(id, 10_000L, publication, reopening = true)
            synchronized(this@Deck) {
                if (published == null || !isCurrentConnection(marker) || _connection.value?.id != expectedConnection ||
                    !marker.readiness.acknowledgePublishedTarget(published, publication, reopening = true) || !isCurrentSession(id, marker)) {
                    "The reopened session has not reached this connection. Your history and drafts were retained."
                } else null
            }
        } catch (e: DeckException) {
            e.message ?: "The session could not be reopened."
        }
    }

    suspend fun clearErrors() = act { Requests.clearErrors(it) }

    suspend fun switchModel(id: Long, model: String?, effort: String?) =
        act(GatewayClient.SLOW_TIMEOUT_MS) { Requests.model(it, id, model, effort) }

    /** Starts a session; its id, or the error. */
    suspend fun newSession(tool: String, model: String?, effort: String?, text: String?, host: String? = null): Result<Long> = try {
        val createdAt = System.currentTimeMillis()
        val marker = sessionStateMarker()
        val reply = client.request(GatewayClient.SLOW_TIMEOUT_MS) { checkedRequest(Requests.new(it, tool, model, effort, text, host)) }
        val id = (reply as? ServerMessage.Ok)?.id
        if (id != null) awaitSessionState(id, marker)
        if (id != null && !text.isNullOrBlank()) {
            // The first native message belongs to the newly created conversation.
            addOutgoing(
                id,
                Outgoing(
                    key = outKeys.incrementAndGet(),
                    text = text,
                    files = emptyList(),
                    paths = emptyList(),
                    generation = 0,
                    after = 0,
                    sentAt = System.currentTimeMillis(),
                    delivered = tool != "claude",
                    submitted = tool == "claude",
                    queued = false,
                    createdAt = createdAt,
                ),
            )
        }
        if (id != null) Result.success(id) else Result.failure(DeckException("The deck did not name the new session."))
    } catch (e: DeckException) {
        Result.failure(e)
    }

    /** The earlier conversations to resume, newest first, as the deck's Alt+r lists them. */
    suspend fun conversations(): Result<List<Conversation>> = try {
        val reply = client.request(LIST_TIMEOUT_MS) { Requests.conversations(it) }
        if (reply is ServerMessage.Conversations) Result.success(reply.items)
        else Result.failure(DeckException("The deck did not list its conversations. Update the deck computer and try again."))
    } catch (e: DeckException) {
        Result.failure(e)
    }






    suspend fun search(q: String): Result<ServerMessage.Search> = try {
        val reply = client.request(LIST_TIMEOUT_MS) { Requests.search(it, q) }
        if (reply is ServerMessage.Search) Result.success(reply)
        else Result.failure(DeckException("The deck did not search. Update the deck computer and try again."))
    } catch (e: DeckException) {
        Result.failure(e)
    }

    /** Resumes an earlier conversation, or finds the session already having it; that session's id. */
    suspend fun resume(conversation: Conversation): Result<Long> = try {
        val marker = sessionStateMarker()
        val reply = client.request(GatewayClient.SLOW_TIMEOUT_MS) {
            Requests.resume(it, conversation.tool, conversation.id, conversation.host)
        }
        val id = (reply as? ServerMessage.Ok)?.id
        if (id != null) awaitSessionState(id, marker)
        if (id != null) Result.success(id) else Result.failure(DeckException("The deck did not name the resumed session."))
    } catch (e: DeckException) {
        Result.failure(e)
    }

    private data class StateMarker(val readiness: SessionReadiness, val epoch: Long, val connection: Long)
    private fun sessionStateMarker() = StateMarker(sessionReadiness, sessionReadiness.marker(), connectionGeneration)
    private fun isCurrentConnection(marker: StateMarker) = marker.connection == connectionGeneration &&
        marker.readiness === sessionReadiness && marker.readiness.marker() == marker.epoch
    private fun isCurrentSession(id: Long, marker: StateMarker) = isCurrentConnection(marker) &&
        marker.readiness.accepts(id, marker.epoch)

    /** A Created receipt is not a session record; wait for the exact routed target. */
    private suspend fun awaitSessionState(id: Long, marker: StateMarker = sessionStateMarker()) {
        if (!marker.readiness.awaitTarget(id, 10_000, marker.epoch) || marker.connection != connectionGeneration) {
            throw DeckException("Session $id was created, but its session state has not arrived. Check Home before starting another.")
        }
    }

    suspend fun commands(id: Long): Result<List<io.github.cepheus.app.protocol.CommandEntry>> = try {
        val reply = client.request { checkedRequest(Requests.commands(it, id)) }
        if (reply is ServerMessage.Commands && reply.id == id) Result.success(reply.entries)
        else Result.failure(DeckException("The deck did not return this session's commands. Update the deck computer and try again."))
    } catch (e: DeckException) {
        Result.failure(e)
    }

    /** A picture the owner sent in session `id`, by the key its message carries. */
    suspend fun image(id: Long, key: String): Result<io.github.cepheus.app.protocol.SentImage> = try {
        val reply = client.request(IMAGE_TIMEOUT_MS) { checkedRequest(Requests.image(it, id, key)) }
        if (reply is ServerMessage.Image && reply.key == key) Result.success(reply.image)
        else Result.failure(DeckException("The deck did not return this picture. Update the deck computer and try again."))
    } catch (e: DeckException) {
        Result.failure(e)
    }

    suspend fun fetchModels() {
        try {
            val reply = client.request { Requests.models(it) }
            if (reply is ServerMessage.Models) _models.value = reply.tools
        } catch (_: DeckException) {
        }
    }

    // ---- notification actions ----

    /** An option button or Reply on a notification. */
    suspend fun handleAction(intent: Intent) {
        if (!Notifier.acceptsAction(intent)) return
        // An old notification may still be delivered after the connection was switched.
        val target = intent.getStringExtra(Notifier.EXTRA_CONNECTION) ?: Connections.LEGACY
        if (target != _connection.value?.id) return
        val id = intent.getLongExtra(ActionReceiver.EXTRA_ID, -1)
        if (id < 0) return
        val title = session(id)?.title?.ifBlank { null } ?: "Session #$id"
        val typed = RemoteInput.getResultsFromIntent(intent)?.getCharSequence(Notifier.REPLY_KEY)?.toString()?.trim()
        val hasPrompt = intent.hasExtra(ActionReceiver.EXTRA_FP)
        if (intent.action == ActionReceiver.ACTION_REPLY && typed.isNullOrEmpty()) {
            Notifier.cancelNeedsYou(app, id, target)
            return
        }
        holding(expectedConnection = target) { answerAction(intent, id, title, typed, hasPrompt) }
    }

    private suspend fun answerAction(intent: Intent, id: Long, title: String, typed: String?, hasPrompt: Boolean) {
        val online = withTimeoutOrNull(15_000) { link.first { it is Link.Online } } != null
        if (!online) {
            Notifier.actionFailed(app, id, title, "Couldn't reach the deck. Open the chat to try again.")
            return
        }
        val error = if (hasPrompt) {
            val waiting = WaitingPrompt(
                id = id,
                inputs = intent.getLongExtra(ActionReceiver.EXTRA_INPUTS, 0),
                fp = intent.getStringExtra(ActionReceiver.EXTRA_FP).orEmpty(),
                // Only inputs and fp travel back; the prompt's text is not needed to answer.
                prompt = Prompt("", "", emptyList(), 0, null, false),
            )
            val choice = intent.getIntExtra(ActionReceiver.EXTRA_CHOICE, 0)
            act { Requests.answer(it, waiting, choice, typed) }
        } else {
            act { Requests.send(it, id, typed.orEmpty()) }
        }
        if (error == null) {
            Notifier.cancelNeedsYou(app, id)
            Notifier.cancelFinished(app, id)
            Notifier.cancelFailed(app, id)
        } else {
            Notifier.actionFailed(app, id, title, "Couldn't answer: $error")
        }
    }
}
