package io.github.cepheus.app.store

import io.github.cepheus.app.protocol.Item
import io.github.cepheus.app.protocol.Status
import java.util.Locale















data class Outgoing(
    val key: Long,
    val text: String,
    /** Names of the files sent with it, as the chat shows them. */
    val files: List<String>,
    /** Where the deck keeps those files, included after the message text. */
    val paths: List<String>,
    val generation: Long,
    val after: Int,
    /** Phone time, in milliseconds: when it was sent, then when the deck took it. */
    val sentAt: Long,
    /** The deck took it; until then it is still being sent. */
    val delivered: Boolean,
    /** The session was working, so the program holds it until its turn reaches it. */
    val queued: Boolean,
    /** Original phone send time; delivery acknowledgements only advance sentAt. */
    val createdAt: Long = sentAt,
    /** Send now was pressed: the deck asked Claude to take it in now. */
    val sendingNow: Boolean = false,
    val error: String? = null,
    val tokens: List<String> = emptyList(),
    val uris: List<String> = emptyList(),
    /** The owner API admitted it; canonical conversation history has not shown it yet. */
    val submitted: Boolean = false,
    /**
     * Refused before it left the phone, or dropped by the program after the
     * deck took it: no message in the log can be this one.
     */
    val knownUnsent: Boolean = false,
    /** One identity from the initial send through the native transcript. */
    val clientId: String? = null,
    val conversation: String? = null,
    val delivery: String? = null,
    val correlated: Boolean = false,
    /**
     * Desktop time, in milliseconds, when the gateway passed it on, as its
     * receipt says; null until then or from a gateway that does not say.
     * The log's times are a desktop's, which the phone's clock may run ahead
     * of.
     */
    val hostAt: Long? = null,
)

object Outbox {
    /**
     * The deck took `out`. `hostAt` and `paths` are the gateway's receipt:
     * when it passed the message on, and the file paths it wrote after the
     * words, which for a session on another host are that host's copies.
     */
    fun acknowledged(
        out: Outgoing,
        tool: String?,
        now: Long,
        receipt: String? = null,
        hostAt: Long? = null,
        paths: List<String>? = null,
    ): Outgoing =
        out.copy(
            delivered = receipt in setOf("queued", "started", "steered") || (receipt == null && tool !in setOf("claude", "codex")),
            submitted = receipt == "submitted" || (receipt == null && tool in setOf("claude", "codex")),
            queued = receipt == "queued", sentAt = now, delivery = receipt, error = null,
            hostAt = hostAt ?: out.hostAt,
            paths = paths?.takeIf { it.size == out.paths.size } ?: out.paths,
        )








    fun refused(pending: List<Outgoing>, clientId: String, error: String): List<Outgoing> =
        pending.map {
            if (it.clientId != clientId) it
            else it.copy(error = error, submitted = false, delivered = false, queued = false, sendingNow = false, knownUnsent = true)
        }

    /**
     * How long a message the deck took may stay unseen once its session is
     * not busy. A program that took it logs it within its prompt hooks; one
     * that never logs it leaves the chat this long after it was taken or the
     * session last stopped working, whichever is later.
     */
    const val SETTLE_MS = 20_000L

    /** Characters of a message compared with the log, which keeps 24,000 of each message. */
    private const val MATCH_CHARS = 2_000

    /**
     * How far a session host's clock may run behind the gateway's when a
     * message is found by its time. A session on the gateway's own desktop
     * shares its clock; another host's is kept in step by network time.
     */
    private const val HOST_SKEW_MS = 2_000L








    fun sendNowAvailable(tool: String?, status: Status, alive: Boolean, online: Boolean): Boolean =
        tool == "claude" && alive && online && status == Status.WORKING

    /** A message Send now can take: Claude holds it behind the turn, and no Send now is on its way. */
    fun offersSendNow(out: Outgoing): Boolean = !out.submitted && out.error == null && out.delivered && out.queued && !out.sendingNow

    /** The messages a Send now takes: the key sends every one queued behind the turn. */
    fun sendNowTakes(pending: List<Outgoing>): Set<Long> = pending.filter(::offersSendNow).mapTo(mutableSetOf()) { it.key }

    /** Messages `keys` on their way after a Send now (`on`), or waiting again once the deck refused it. */
    fun sendingNow(pending: List<Outgoing>, keys: Set<Long>, on: Boolean): List<Outgoing> =
        pending.map { if (it.key in keys) it.copy(sendingNow = on) else it }

    /** A message sent now waits: the session works, starts or asks a question. */
    fun busy(status: Status): Boolean =
        status == Status.WORKING || status == Status.STARTING || status == Status.NEEDS_YOU

    /**
     * The chat shows the agent at work, as Claude's and Codex's apps do:
     * the session works, starts with a first message, or the phone is still
     * sending to an idle session. Once delivery finishes, an idle or finished
     * session wins: native commands may never enter the log. A question
     * waiting for the owner shows its card instead.
     */
    fun thinking(status: Status, alive: Boolean, pending: List<Outgoing>): Boolean =
        alive && (status == Status.WORKING ||
            (status == Status.STARTING && pending.any { !it.submitted && it.error == null }) ||
            (status == Status.IDLE && pending.any { !it.submitted && !it.delivered && it.error == null }))









    fun workingSince(statusSince: Long, items: List<Item>, pending: List<Outgoing>): Long {
        val logged = items.asReversed().firstNotNullOfOrNull { (it as? Item.User)?.at }
        val sent = pending.filter { it.error == null }.maxOfOrNull { it.createdAt }
        return listOfNotNull(statusSince, logged?.div(1000), sent?.div(1000)).max()
    }








    fun workingLabel(status: Status, detail: String): String =
        detail.trim().takeIf { status == Status.WORKING && it.isNotEmpty() }
            ?.replaceFirstChar { it.titlecase(Locale.ROOT) } ?: "Thinking…"

    /**
     * What of `pending` still waits for the session's `view` to show it (null
     * when that chat is not open). `quietSince` is when the session stopped
     * being busy, or null while it is busy. Each message the log gained since
     * a message was sent takes the first waiting one with the same text, in
     * the order they were sent. A terminal echo never acknowledges a native
     * outgoing bubble.
     */
    fun settle(
        pending: List<Outgoing>,
        view: ChatView?,
        quietSince: Long?,
        now: Long,
    ): List<Outgoing> {
        if (pending.isEmpty()) return pending
        val start = view?.start ?: 0
        val users = view?.items.orEmpty().mapIndexedNotNull { i, item ->
            (item as? Item.User)?.let { (start + i) to it }
        }
        val taken = mutableSetOf<Int>()
        return pending.filter { p ->
            // An equal message in the log is another one: this was never
            // sent, and dropping it lost the owner's words.
            if (p.knownUnsent) return@filter true
            // Another generation is another chat (`/clear`), a rewritten log
            // or the log read afresh, whose indexes start elsewhere.
            val from = if (view != null && view.generation == p.generation) p.after else 0
            val want = normalize(logged(p)).take(MATCH_CHARS)
            // There an item is this message only if logged once it was sent,
            // by the desktop clock the log keeps when the gateway said when it
            // passed the message on. A new session's first log is such a
            // generation, and a phone clock running ahead of the desktop's

            // mesaages with screenshots doublesend, fix that too").
            val since = p.hostAt?.let { it - HOST_SKEW_MS } ?: p.createdAt
            val hit = users.firstOrNull { (index, user) ->
                if (index in taken) false
                else if (p.correlated || (p.clientId != null && user.clientId != null))
                    p.clientId != null && p.clientId == user.clientId
                else index >= from &&
                    (view?.generation == p.generation || p.conversation == null || user.at?.let { it >= since } == true) &&
                    ((want.isNotEmpty() && normalize(user.text).startsWith(want)) || showsPictured(p, user.text))
            }
            if (hit != null) {
                taken += hit.first
                false
            } else {
                p.error != null || !expired(p, quietSince, now)
            }
        }
    }











    private fun showsPictured(p: Outgoing, raw: String): Boolean {
        if (p.paths.isEmpty()) return false
        // Dot names the actual uploaded files on trailing lines. Match that
        // exact ordered suffix, including native filenames or the provider's
        // literal missing-name fallback. Unconfirmed uploads keep their copy.
        if (p.delivered && p.error == null && p.paths.size <= 10) {
            var body: String? = raw
            for (path in p.paths.asReversed()) {
                val current = body ?: break
                val named = "\n[Image: ${path.substringAfterLast('/')}]"
                val fallback = "\n[Image: image]"
                body = when {
                    current.endsWith(named) -> current.removeSuffix(named)
                    current.endsWith(fallback) -> current.removeSuffix(fallback)
                    else -> null
                }
            }
            if (body != null && normalize(body).take(MATCH_CHARS) == normalize(p.text).take(MATCH_CHARS)) return true
        }
        val text = normalize(raw)
        if (!IMAGE_MARK.containsMatchIn(text)) return false
        var words = normalize(IMAGE_MARK.replace(text, " ").replace(IMAGE_PART, " "))
        val want = normalize(p.text).take(MATCH_CHARS)
        if (want.isNotEmpty()) return words.startsWith(want)
        for (path in p.paths) words = words.replace(normalize(path), " ")
        return words.isBlank()
    }

    private val IMAGE_MARK = Regex("""\[Image #\d+]""")
    private const val IMAGE_PART = "[image]"

    private fun expired(p: Outgoing, quietSince: Long?, now: Long): Boolean =
        !p.submitted && p.delivered && p.files.isEmpty() && p.text.trim().matches(Regex("/[A-Za-z0-9][A-Za-z0-9_.:-]*")) && quietSince != null && now - maxOf(p.sentAt, quietSince) >= SETTLE_MS

    /** The message as the deck types it: the text, a blank line and one file path per line. */
    private fun logged(p: Outgoing): String = when {
        p.paths.isEmpty() -> p.text
        p.text.isBlank() -> p.paths.joinToString("\n")
        else -> p.text.trimEnd() + "\n\n" + p.paths.joinToString("\n")
    }

    /**
     * A message as the log and the phone can both compare it: the deck types
     * it without control characters, and programs may trim it or change its
     * line breaks.
     */
    fun normalize(text: String): String = buildString {
        var space = false
        for (c in text) {
            when {
                c.isWhitespace() -> space = isNotEmpty()
                c.isISOControl() -> {}
                else -> {
                    if (space) append(' ')
                    space = false
                    append(c)
                }
            }
        }
    }
}
