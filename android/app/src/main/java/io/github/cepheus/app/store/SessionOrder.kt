package io.github.cepheus.app.store

import io.github.cepheus.app.protocol.DeckState
import io.github.cepheus.app.protocol.Session

/** A section of the shared session list. Host status belongs in the resources view. */
data class HostGroup(val host: String?, val label: String, val offline: String?, val sessions: List<Session>)

/** The gateway and phone share one saved order across machines. */
object SessionOrder {
    /** Move session `id` within the whole list, including across host boundaries. */
    fun move(sessions: List<Session>, id: Long, to: Int): List<Session> {
        val from = sessions.indexOfFirst { it.id == id }
        if (from < 0) return sessions
        val target = to.coerceIn(0, sessions.lastIndex)
        if (target == from) return sessions
        return sessions.toMutableList().apply { add(target, removeAt(from)) }
    }

    /** Apply saved positions; new sessions follow by creation time and shared identity. */
    fun arrange(sessions: List<Session>, ids: List<Long>): List<Session> {
        val place = ids.withIndex().associate { (i, id) -> id to i }
        return sessions.sortedWith(compareBy<Session> { place[it.id] ?: Int.MAX_VALUE }.thenBy { it.createdAt }.thenBy { it.id })
    }

    /** Running sessions, as the deck's heading counts them ("Sessions · 3 live"). */
    fun live(sessions: List<Session>): Int = sessions.count { it.alive }

    /** One list keeps the gateway's global saved order, regardless of machine. */
    fun groups(state: DeckState?, sessions: List<Session>): List<HostGroup> =
        listOf(HostGroup(null, "Sessions", null, sessions))

    /** Cached ended sessions stay hidden and never consume a visible row number. */
    fun shown(state: DeckState?, sessions: List<Session>): List<Pair<HostGroup, List<NumberedSession>>> =
        groups(state, sessions).mapNotNull { group ->
            val running = group.sessions.filter { it.alive }
            // The deck numbers live cloud agents ahead of these rows, all but
            // Dot, which goes by its own name and takes no number (owner,
            // 2026-11-11: "its counting the dot agent as a session when it shouldnt").
            val cloudCount = state?.sessions?.count { it.alive && state.isCloud(it) && it.tool != "dot" } ?: 0
            val rows = running.mapIndexed { i, s -> NumberedSession(i + 1 + cloudCount, s) }
            if (rows.isEmpty()) null else group.copy(sessions = running) to rows
        }
}

/** A session on the phone's list and the deck's number for it. */
data class NumberedSession(val number: Int, val session: Session)
