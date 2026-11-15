package io.github.cepheus.app.protocol

// The gateway's messages as cepheus/README.md ("Phone", "Protocol")
// describes them. Timestamps keep the gateway's units: `createdAt`,
// `statusSince` and error `at` are seconds; alert and chat `at` milliseconds.

enum class Status(val wire: String, val label: String) {
    STARTING("starting", "Starting"),
    WORKING("working", "Working"),
    NEEDS_YOU("needs_you", "Needs you"),
    FINISHED("finished", "Finished"),
    IDLE("idle", "Idle"),
    ENDED("ended", "Ended"),
    UNKNOWN("", "Unknown");

    companion object {
        fun of(wire: String?): Status = entries.firstOrNull { it != UNKNOWN && it.wire == wire } ?: UNKNOWN
    }
}

/** The programs the deck hosts, by their wire names. */
object Tools {
    val all = listOf("claude", "codex", "antigravity", "grok", "opencode")

    @Suppress("UNUSED_PARAMETER")
    fun canAttach(tool: String?): Boolean = true
    fun canStop(tool: String?): Boolean = tool != "dot"

    fun label(tool: String): String = when (tool) {
        "claude" -> "Claude"
        "codex" -> "Codex"
        "antigravity" -> "Antigravity"
        "grok" -> "Grok"
        "opencode" -> "opencode"
        "dot" -> "Dot"
        else -> tool
    }
}

data class Session(
    val id: Long,
    val tool: String,
    val title: String,
    val status: Status,
    val detail: String,
    val alive: Boolean,
    val branch: String?,
    val worktree: String?,
    val task: String?,
    val review: Boolean,
    val createdAt: Long,
    val statusSince: Long,
    /** The host it runs on, by name; null for the desktop the phone pairs with. */
    val host: String? = null,
    /** How the drawer heads that host. */
    val hostLabel: String? = null,
    /** Active and recent children reported by the provider; timestamps are seconds. */
    val subagents: List<Subagent> = emptyList(),
    /** Original process/conversation custody for automatic idle admission (protocol 17). */
    val expectedTarget: AttentionContracts.Target? = null,
    val pid: Long? = null,
    val conversation: String? = null,
)

data class Subagent(val name: String, val kind: String?, val active: Boolean, val startedAt: Long, val lastActive: Long)

/**
 * Another machine the deck drives, as the drawer heads its sessions. [agents]
 * is false for a build host: it reports resources and runs no sessions.
 */
data class HostSummary(val name: String, val label: String, val online: Boolean, val reason: String?, val agents: Boolean = true)

/**
 * The machine a new session starts on: null for the desktop the phone pairs
 * with, else a [HostSummary.name].
 */
data class StartHost(val name: String?, val label: String)

data class LogEntry(val at: Long, val text: String)

data class UsageWindow(val label: String, val usedPercent: Double?, val resetsAt: Long?)

/**
 * Each program's usage windows. A window labelled with a model before its
 * window (`Argon wk`, `Fable wk`) is that model's own limit.
 */
data class Usage(
    val claude: List<UsageWindow>,
    val codex: List<UsageWindow>,
    val grok: List<UsageWindow> = emptyList(),
    val gemini: List<UsageWindow> = emptyList(),
    /** Claude's per-model limits (`Fable wk`), kept apart from [claude]. */
    val claudeModels: List<UsageWindow> = emptyList(),
)

/**
 * An agent the deck sees running outside it, in a terminal of its own. The
 * phone can move it into the deck through its routed process id. `busy` is
 * null when the deck cannot tell.
 */
data class OutsideAgent(
    val pid: Long,
    val tool: String,
    val title: String,
    val busy: Boolean?,
    val cwd: String,
    val branch: String?,
    val worktree: String?,
    val task: String?,
    val startedAt: Long,
    val host: String? = null,
    val hostLabel: String? = null,
)

data class CloudAgent(val id: String, val label: String, val host: String? = null)

data class DeckState(
    /** In the coordinator's saved global order across every machine. */
    val sessions: List<Session>,
    val errors: List<LogEntry>,
    val startDir: String,
    val usage: Usage,
    val outside: List<OutsideAgent> = emptyList(),
    /** The other machines the deck drives, online or not; empty when it drives none. */
    val hosts: List<HostSummary> = emptyList(),
    /** The coordinator's native machine label, also used by explicit placement. */
    val hereLabel: String = "",
    /** Registered cloud connections remain discoverable when switched off. */
    val cloudAgents: List<CloudAgent> = emptyList(),
    val attentionCapable: Boolean = false,
    val attention: AttentionContracts.Snapshot? = null,
    /** A saved list or a connected gateway alone cannot authorize an action. */
    val ready: Boolean = true,
    /** Native message admission is guaranteed only from protocol 19 onward. */
    val protocol: Long = 0,
) {
    fun isCloud(session: Session): Boolean = cloudAgents.any { it.id == session.tool && it.host == session.host }
    /** Where a new session can start: the desktop, then each other agent host that is online. */
    val startHosts: List<StartHost>
        get() = listOf(StartHost(null, hereLabel.ifEmpty { "This machine" })) +
            hosts.filter { it.online && it.agents }.map { StartHost(it.name, it.label) }
}

/**
 * A section the deck's scripts add to its sidebar (`deck.ui.section` in
 * init.lua or a plugin), shown on the Stats screen too: its title and lines.
 */
data class Panel(val id: String, val title: String, val lines: List<String>)

/** One machine's CPU and memory (percent) and disk, for the Stats screen. */
data class HostUse(
    val name: String?,
    val label: String,
    val online: Boolean,
    val reason: String?,
    val cpu: Double?,
    val memory: Double?,
    val disk: DiskUse?,
)

/** The desktop's disk, as `df` counts it: bytes in use and bytes still free to write. */
data class DiskUse(val used: Long, val available: Long) {
    val total: Long get() = used + available
    val percentUsed: Double? get() = if (total > 0) used * 100.0 / total else null
}

data class AsyncQuestion(val id: String, val title: String, val options: List<String>, val answered: Boolean = false, val submitted: Boolean = false)

/** Lines added and removed. */
data class Lines(val added: Int, val removed: Int)

/** A picture the owner sent, as `image` answers: its media type and its bytes in base64. */
data class SentImage(val media: String, val data: String)

/** One entry of a session's chat, by its `k`. */
sealed interface Item {
    /** The owner's message; `images` are the keys of the pictures sent in it, fetched with `image`. */
    data class User(val text: String, val at: Long?, val images: List<String> = emptyList(), val clientId: String? = null) : Item
    data class Assistant(val text: String, val at: Long?, val questions: List<AsyncQuestion> = emptyList()) : Item
    data class Thinking(val text: String) : Item

    /**
     * A tool call; `call` ties it to its [Result]. `label` is what the agent
     * said it does (a Claude command's description) and `diff` the lines a
     * file edit adds and removes, both when the deck knows them.
     */
    data class Tool(
        val call: String,
        val name: String,
        val summary: String,
        val input: String,
        val label: String? = null,
        val diff: Lines? = null,
    ) : Item
    data class Result(val call: String, val text: String, val error: Boolean) : Item
    data class Notice(val text: String) : Item

    /**
     * The screen of a program whose log is not read: Grok, and Antigravity
     * until its conversation has a database.
     */
    data class Screen(val text: String) : Item

    /** A kind this app does not know yet; shown as nothing. */
    data class Unknown(val kind: String) : Item
}

/**
 * One choice. `checked` is whether its box is ticked now, set only for the
 * boxes of a several-choice question; a plain option there, such as Claude's
 * "Chat about this", answers on its own.
 */
data class PromptOption(val label: String, val detail: String, val checked: Boolean? = null)

/** A question or approval a session waits on, as the deck read it from its screen. */
data class Prompt(
    val kind: String,
    val question: String,
    val options: List<PromptOption>,
    val selected: Int,
    /** The option that takes typed text, if any. */
    val freeText: Int?,
    /** Several options may be chosen: their boxes are sent together in `answer`'s `choices`. */
    val multi: Boolean,
    /** The several-choice question's own button, "Submit" or "Next"; the deck sends none without one. */
    val submit: String? = null,
)

/** A prompt as it waits now: `inputs` and `fp` go back unchanged in `answer`. */
data class WaitingPrompt(val id: Long, val inputs: Long, val fp: String, val prompt: Prompt)

/** A completion in the agent's native invocation syntax. */
data class CommandEntry(val name: String, val insert: String, val description: String, val kind: String)

/**
 * An earlier conversation the deck can resume, as its Alt+r lists them. The
 * times are Unix seconds; `openIn` is the session already having it.
 */
data class Conversation(
    val tool: String,
    val id: String,
    val title: String,
    val createdAt: Long,
    val updatedAt: Long,
    val openIn: Long?,
    val host: String? = null,
)

/**
 * A message of a session that holds every word of a search, cut around its
 * first match. `role` is "user" (the owner's own), "assistant" or "screen";
 * `at` is milliseconds when the log says.
 */
data class SearchSnippet(val role: String, val text: String, val at: Long?)

/**
 * A session whose conversation holds every word of a search: how many of its
 * messages match, how many of them are the owner's, and up to three of
 * them, the owner's first, newest first.
 */
data class SearchHit(val id: Long, val count: Int, val mine: Int, val snippets: List<SearchSnippet>)

data class ModelChoice(val id: String, val label: String, val efforts: List<String>)

data class ToolModels(
    val tool: String,
    val models: List<ModelChoice>,
    val efforts: List<String>,
    /** A running session's model can be switched from the phone. */
    val switch: Boolean,
    /** What the program starts with when nothing is picked, from its own settings. */
    val defaultModel: String? = null,
    val defaultEffort: String? = null,
    val label: String? = null,
    val cloud: Boolean = false,
) {
    fun displayName(): String = if (tool in Tools.all) Tools.label(tool) else label?.takeIf { it.isNotBlank() } ?: tool

    /** The efforts `model` (the default one when none) takes: its own list, else the program's. */
    fun effortsFor(model: String?): List<String> =
        models.firstOrNull { it.id == (model ?: defaultModel) }?.efforts?.takeIf { it.isNotEmpty() } ?: efforts

    fun defaultModelLabel(): String =
        defaultModel?.let { id -> "Default · " + (models.firstOrNull { it.id == id }?.label ?: id) } ?: "Default"

    fun defaultEffortLabel(): String = defaultEffort?.let { "Default · $it" } ?: "Default"
}

enum class AlertKind(val wire: String) {
    FINISHED("finished"),
    NEEDS_YOU("needs_you"),
    ERROR("error"),
    UNKNOWN("");

    companion object {
        fun of(wire: String?): AlertKind = entries.firstOrNull { it != UNKNOWN && it.wire == wire } ?: UNKNOWN
    }
}

sealed interface ServerMessage {
    data class Paired(val device: String, val key: String) : ServerMessage {
        // The key is a credential: never print it.
        override fun toString() = "Paired(device=$device)"
    }

    data class Denied(val reason: String) : ServerMessage
    data class Hello(val epoch: String, val version: String) : ServerMessage
    data class State(val state: DeckState) : ServerMessage
    data class Alert(
        val seq: Long,
        val epoch: String,
        val kind: AlertKind,
        /** The session; null for a service error. */
        val id: Long?,
        val title: String,
        val reason: String,
        val deskFocused: Boolean,
        val at: Long,
        /** Kept from before this connection: a finish or a question in it is recorded, not notified. */
        val replay: Boolean = false,
        /** The client id of the owner message an error refused after the deck took it. */
        val message: String? = null,
    ) : ServerMessage

    /** A session's waiting prompt, or `prompt == null` once it is gone; `replay` as for [Alert]. */
    data class PromptUpdate(val id: Long, val inputs: Long, val fp: String, val prompt: Prompt?, val replay: Boolean = false) : ServerMessage {
        val waiting: WaitingPrompt? get() = prompt?.let { WaitingPrompt(id, inputs, fp, it) }
    }

    /** New items of the followed chat; with `reset`, they replace what is shown from `start`. */
    data class Items(
        val id: Long,
        val reset: Boolean,
        val start: Int,
        val total: Int,
        val generation: Long?,
        val items: List<Item>,
        val model: String?,
        val effort: String?,
        /** The earliest item the gateway can still page; absent on older gateways. */
        val retainedFrom: Int? = null,
        /** Unfinished reply, outside transcript indexes and offline history. */
        val stream: String? = null,
        val questions: List<AsyncQuestion>? = null,
    ) : ServerMessage

    /** A page of a session's chat, the reply to `open` and `chat`. */
    data class Chat(
        val rid: Long?,
        val id: Long,
        val start: Int,
        val total: Int,
        val generation: Long,
        val source: String,
        val items: List<Item>,
        val model: String?,
        val effort: String?,
        /** The earliest item the gateway can still page; absent on older gateways. */
        val retainedFrom: Int? = null,
        val questions: List<AsyncQuestion>? = null,
    ) : ServerMessage

    data class Notice(val text: String, val error: Boolean) : ServerMessage
    data class Commands(val rid: Long?, val id: Long, val tool: String, val entries: List<CommandEntry>) : ServerMessage
    data class Models(val rid: Long?, val tools: List<ToolModels>) : ServerMessage

    /** The earlier conversations to resume, newest first. */
    data class Conversations(val rid: Long?, val items: List<Conversation>) : ServerMessage

    /** The sessions whose conversation holds every word of search `q`. */
    data class Search(val rid: Long?, val q: String, val hits: List<SearchHit>) : ServerMessage
    data class Control(val rid: Long?, val action: String, val data: org.json.JSONObject) : ServerMessage

    /** A picture the owner sent in session `id`, by its key. */
    data class Image(val rid: Long?, val id: Long, val key: String, val image: SentImage) : ServerMessage {
        // Megabytes of base64: never print it.
        override fun toString() = "Image(rid=$rid, id=$id, key=$key, media=${image.media})"
    }
    data class Pong(val rid: Long?) : ServerMessage

    /**
     * Success; `id` names the session a `new` started, `upload` the file an
     * `upload` began, and `path` where a file was saved once it is whole. A
     * `send` gets `at`, the gateway's time in milliseconds just before it
     * passed the message on, and `paths`, the file paths written after its
     * words (the session host's own for a session on another host).
     */
    data class Ok(
        val rid: Long?,
        val id: Long?,
        val upload: String? = null,
        val path: String? = null,
        val delivered: String? = null,
        val clientId: String? = null,
        val at: Long? = null,
        val paths: List<String>? = null,
    ) : ServerMessage
    /** A refusal; `code` names its kind when the phone acts on it, such as [Protocol.QUESTION_STALE]. */
    data class Err(val rid: Long?, val error: String, val code: String? = null) : ServerMessage

    /**
     * The Stats screen: the agents' usage windows and the desktop's CPU,
     * memory (percent) and disk, then each other host's under `hosts`.
     */
    data class Stats(
        val rid: Long?,
        val usage: Usage,
        val cpu: Double?,
        val memory: Double?,
        val disk: DiskUse?,
        val label: String = "",
        val hosts: List<HostUse> = emptyList(),
        val panels: List<Panel> = emptyList(),
    ) : ServerMessage {
        /** Every machine's use, the desktop's first. */
        val machines: List<HostUse>
            get() = listOf(HostUse(null, label.ifEmpty { "This machine" }, true, null, cpu, memory, disk)) + hosts
    }

    /** A message this app does not know, or one that did not parse. */
    data class Unknown(val type: String) : ServerMessage
}

/** The request a message answers, when it is a reply. */
val ServerMessage.rid: Long?
    get() = when (this) {
        is ServerMessage.Chat -> rid
        is ServerMessage.Commands -> rid
        is ServerMessage.Models -> rid
        is ServerMessage.Conversations -> rid
        is ServerMessage.Search -> rid
        is ServerMessage.Control -> rid
        is ServerMessage.Image -> rid
        is ServerMessage.Pong -> rid
        is ServerMessage.Ok -> rid
        is ServerMessage.Err -> rid
        is ServerMessage.Stats -> rid
        else -> null
    }
