package io.github.cepheus.app.store

import io.github.cepheus.app.protocol.DeckState
import io.github.cepheus.app.protocol.Session
import io.github.cepheus.app.protocol.long
import io.github.cepheus.app.protocol.str
import org.json.JSONArray
import org.json.JSONObject
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withTimeoutOrNull

/** A cached selection stays bound to its original session through reconnects. */
internal class SessionReadiness(private val storage: KeyValue? = null, cached: DeckState? = null) {
    data class Publication(val connection: Long, val revision: Long)
    /**
     * A session is the deck's record of it: its ID (which carries its host's
     * tag), host, program and creation time. The process and the provider's
     * conversation it runs are the session's current target.
     */
    private data class Target(val id: Long, val host: String?, val tool: String, val created: Long,
        val pid: Long?, val conversation: String?, val spawnedMs: Long?) {
        fun sameSession(other: Target) = id == other.id && host == other.host && tool == other.tool && created == other.created
        fun matches(other: Target) = sameSession(other) &&
            (pid == null || other.pid == null || pid == other.pid) &&
            (conversation == null || other.conversation == null || conversation == other.conversation) &&
            (spawnedMs == null || other.spawnedMs == null || spawnedMs == other.spawnedMs)
        fun enriched(other: Target) = copy(pid = pid ?: other.pid,
            conversation = conversation ?: other.conversation, spawnedMs = spawnedMs ?: other.spawnedMs)
    }
    private fun Session.target() = Target(id, host, tool, createdAt, pid, conversation, expectedTarget?.spawnedMs)
    private var sessions: List<Session> = emptyList()
    // Keep custody separately from the latest list, which can omit a session or reuse an ID.
    private val targets = linkedMapOf<Long, Target>()
    private val changes = MutableStateFlow(0L)
    @Volatile private var connection = 0L
    @Volatile private var publication = 0L
    private var protocol = 0L
    @Volatile var ready = false
        private set

    init {
        val saved = runCatching { JSONArray(storage?.getString("session-targets")) }.getOrNull()
        if (saved != null) for (i in 0 until saved.length()) {
            val value = saved.getJSONObject(i)
            val target = runCatching { Target(value.getLong("id"), value.str("host"), value.getString("tool"), value.getLong("created"),
                value.long("pid"), value.str("conversation"), value.long("spawned_ms")) }.getOrNull() ?: continue
            targets[target.id] = target
        }
        remember(cached)
    }











    private fun remember(state: DeckState?, fresh: Boolean = false): Set<Long> {
        var changed = false
        val moved = mutableSetOf<Long>()
        state?.sessions?.forEach { session ->
            val current = session.target()
            val held = targets[session.id]
            val next = when {
                held == null -> current
                held.matches(current) -> held.enriched(current)
                fresh && held.sameSession(current) -> current.enriched(held).also { moved += session.id }
                else -> held
            }
            if (held != next) { targets[session.id] = next; changed = true }
        }
        if (changed) persist()
        return moved
    }

    private fun persist() {
        storage?.putString("session-targets", JSONArray().also { values ->
            targets.values.forEach { target -> values.put(JSONObject().put("id", target.id)
                .putOpt("host", target.host).put("tool", target.tool).put("created", target.created)
                .putOpt("pid", target.pid).putOpt("conversation", target.conversation)
                .putOpt("spawned_ms", target.spawnedMs)) }
        }.toString())
    }

    @Synchronized fun disconnected() { ready = false; protocol = 0; connection += 1; changes.value += 1 }
    /** Take the deck's current list; the IDs whose custody moved to a new process or conversation. */
    @Synchronized fun accept(state: DeckState): Set<Long> {
        sessions = state.sessions
        protocol = state.protocol
        if (ready && !state.ready) connection += 1
        ready = state.ready
        var moved = emptySet<Long>()
        if (ready) { publication += 1; moved = remember(state, fresh = true) }
        changes.value += 1
        return moved
    }
    /** Only a State from this connection can promise native owner delivery. */
    @Synchronized fun messageError(request: JSONObject): String? {
        val kind = request.optString("t")
        val action = request.optString("action")
        val message = kind in setOf("send", "send_now") ||
            (kind == "new" && !request.isNull("text") && request.optString("text").isNotBlank()) ||
            (kind == "control" && (action == "broadcast" ||
                (action == "create" && !request.isNull("text") && request.optString("text").isNotBlank()) ||
                (action == "fleet" && request.optString("operation") == "attention-send")))
        if (!message) return null
        if (!ready) return "The Cepheus service is not ready; waiting for its session state."
        val steering = kind == "send" && sessions.firstOrNull { it.id == request.long("id") }?.tool == "codex"
        return if (protocol < (if (steering) 21 else 19)) "Update Cepheus on the paired machine before sending messages. Your message is preserved." else null
    }
    @Synchronized private fun usable(id: Long) = error(id) == null && sessions.any { it.id == id && it.alive }
    @Synchronized fun marker(): Long = connection
    @Synchronized fun publicationMarker() = Publication(connection, publication)
    @Synchronized fun accepts(id: Long, generation: Long): Boolean = connection == generation && error(id) == null

    suspend fun awaitTarget(id: Long, timeoutMs: Long, generation: Long = marker()): Boolean {
        if (!ready || connection != generation) return false
        return withTimeoutOrNull(timeoutMs) {
            changes.first { connection != generation || usable(id) }
            connection == generation && usable(id)
        } ?: false
    }
    @Synchronized private fun published(id: Long, marker: Publication, reopening: Boolean): Session? {
        if (!ready || connection != marker.connection || publication <= marker.revision) return null
        val current = sessions.firstOrNull { it.id == id && it.alive } ?: return null
        val held = targets[id] ?: return current
        if (!held.sameSession(current.target()) || (held.pid != null && current.pid == null)) return null
        if (reopening && held.conversation != null && held.conversation != current.conversation) return null
        return current
    }

    /** A lifecycle receipt may observe a new process, without replacing ordinary selection custody. */
    suspend fun awaitPublishedTarget(id: Long, timeoutMs: Long, marker: Publication, reopening: Boolean = false): Session? {
        if (!ready || connection != marker.connection) return null
        return withTimeoutOrNull(timeoutMs) {
            changes.first { connection != marker.connection || published(id, marker, reopening) != null }
            published(id, marker, reopening)
        }
    }

    /** Call only after a correlated receipt, while holding the caller's original connection fence. */
    @Synchronized fun acknowledgePublishedTarget(candidate: Session, marker: Publication, reopening: Boolean = false): Boolean {
        val current = published(candidate.id, marker, reopening) ?: return false
        if (current.target() != candidate.target() || current.expectedTarget != candidate.expectedTarget) return false
        targets[candidate.id] = current.target()
        persist()
        return true
    }
    @Synchronized fun error(id: Long): String? {
        if (!ready) return "The Cepheus service is not ready; waiting for its session state."
        val current = sessions.firstOrNull { it.id == id }
            ?: return "This session is unavailable. Its saved history and drafts are still on this phone."
        if (targets[id]?.matches(current.target()) != true) {
            return "This session's identity changed. Its saved history and drafts are preserved; choose another session."
        }
        return null
    }
}
