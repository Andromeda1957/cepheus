package io.github.cepheus.app.notify

import io.github.cepheus.app.protocol.DeckState
import io.github.cepheus.app.protocol.Status

/** Custody of already-posted question cards; this never creates an alert from state alone. */
internal object AttentionNotifications {
    fun waiting(state: DeckState): Set<Long> = state.sessions
        .filter { it.alive && it.status == Status.NEEDS_YOU }.map { it.id }.toSet()

    /** A delayed alert for a session that is no longer waiting posts no question card. */
    fun prompts(state: DeckState?, session: Long, prompt: Boolean): Boolean =
        state?.attentionCapable != true || prompt || session in waiting(state)

    /** The posted question cards whose sessions no longer wait for an answer. */
    fun stale(state: DeckState, posted: Set<Long>): Set<Long> = posted - waiting(state)
}
