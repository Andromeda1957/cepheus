package io.github.cepheus.app.store

import io.github.cepheus.app.protocol.AttentionContracts
import org.json.JSONObject
import java.util.UUID

/** A private recovery copy binds ambiguous idle sends to their original key and payload. */
class AttentionDrafts(private val storage: KeyValue) {
    data class Pending(val key: String, val session: Long, val text: String, val id: String? = null, val state: String? = null, val origin: String? = null, val target: AttentionContracts.Target? = null) {
        val unresolved: Boolean get() = state !in setOf("queued", "held", "submitted", "canceled")
        override fun toString(): String = "Pending(session=$session, state=$state)"
    }
    private var observed = emptyMap<String, AttentionContracts.Message>()

    @Synchronized
    fun pending(session: Long): Pending? = storage.getString("attention/send/$session")?.let {
        runCatching {
            val value = JSONObject(it)
            Pending(value.getString("key"), value.getLong("session"), value.getString("text"),
                value.opt("id") as? String, value.opt("state") as? String, value.opt("origin") as? String,
                value.optJSONObject("expected_target")?.let(AttentionContracts::targetOrNull))
        }.getOrElse { throw IllegalStateException("The saved idle delivery is unreadable; it was preserved.") }
    }
    @Synchronized
    fun prepare(session: Long, text: String, origin: String = "@here", newCopy: Boolean = false, target: AttentionContracts.Target? = null): Pending {
        val previous = pending(session)
        if (previous?.text == text && !newCopy) return previous
        require(previous?.unresolved != true) { "Check the previous idle delivery before sending another message." }
        return Pending(UUID.randomUUID().toString(), session, text, origin = origin, target = target).also(::save)
    }
    @Synchronized
    fun received(pending: Pending, message: AttentionContracts.Message): AttentionContracts.Message {
        require(message.session == pending.session && (pending.origin == null || pending.origin == message.origin))
        val current = this.pending(pending.session)?.takeIf { it.key == pending.key } ?: return message
        require(current.id == null || current.id == message.id) { "The original delivery returned another identity." }
        // The state callback may run before the awaiting request coroutine. Use
        // its origin-qualified snapshot even before the first receipt ID exists.
        val public = observed[message.identity]?.takeIf { it.session == message.session }
        val state = advance(advance(current.state, message.state), public?.state)
        save(current.copy(id = message.id, state = state))
        return message.copy(state = state ?: message.state)
    }
    /** Later public status can make an accepted delivery ambiguous; retain its fence. */
    @Synchronized
    fun observe(messages: List<AttentionContracts.Message>) {
        val pendingBySession = (messages.map { it.session } + observed.values.map { it.session }).distinct()
            .associateWith { runCatching { pending(it) }.getOrNull() }
        // A peer can disappear before the first receipt callback. Keep ambiguous
        // evidence for those pending identities until the reply can bind its ID.
        val next = observed.filterValues { message ->
            val previous = pendingBySession[message.session]
            previous != null && previous.id == null && previous.origin == message.origin &&
                message.state !in setOf("queued", "held", "submitted", "canceled")
        }.toMutableMap()
        for (message in messages) {
            val previous = (next[message.identity] ?: observed[message.identity])?.takeIf { it.session == message.session }
            next[message.identity] = message.copy(state = advance(previous?.state, message.state)!!)
        }
        // Match the public metadata bound. If unusually many first-receipt
        // candidates exceed it, retain the request fence rather than infer safety.
        for (message in next.values.drop(10000)) {
            val previous = pendingBySession[message.session]
            if (previous != null && previous.id == null && previous.origin == message.origin) save(previous.copy(state = "unknown"))
        }
        for (message in next.values) {
            val previous = pendingBySession[message.session] ?: continue
            if (previous.id == message.id && previous.origin == message.origin) {
                val state = advance(previous.state, message.state)
                if (state != previous.state) save(previous.copy(state = state))
            }
        }
        observed = next.entries.take(10000).associate { it.toPair() }
    }
    /** Delivery cannot go backwards from attempted or terminal to preattempt. */
    private fun advance(previous: String?, incoming: String?): String? = when {
        previous != null && previous !in setOf("queued", "held", "pasted", "submitted", "canceled") -> previous
        incoming != null && incoming !in setOf("queued", "held", "pasted", "submitted", "canceled") -> incoming
        previous in setOf("submitted", "canceled") -> previous
        incoming in setOf("submitted", "canceled") -> incoming
        previous == "pasted" -> previous
        else -> incoming ?: previous
    }
    private fun save(value: Pending) = storage.putString("attention/send/" + value.session, JSONObject()
        .put("key", value.key).put("session", value.session).put("text", value.text)
        .putOpt("id", value.id).putOpt("state", value.state).putOpt("origin", value.origin)
        .apply { value.target?.let { put("expected_target", AttentionContracts.targetJson(it)) } }.toString())
}
