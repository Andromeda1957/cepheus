package io.github.cepheus.app.notify

import io.github.cepheus.app.protocol.DeckState
import io.github.cepheus.app.protocol.Session
import io.github.cepheus.app.protocol.Status
import io.github.cepheus.app.protocol.Usage
import org.junit.Assert.*
import org.junit.Test

class AttentionNotificationsTest {
    private val ids = listOf(1L, (1L shl 40) or 1L, (2L shl 40) or 1L)
    private fun session(id: Long, status: Status = Status.WORKING, alive: Boolean = true) =
        Session(id, "claude", "Worker", status, "", alive, null, null, null, false, 1, 1)
    private fun state(sessions: List<Session>) =
        DeckState(sessions, emptyList(), "/work", Usage(emptyList(), emptyList()), attentionCapable = true)

    @Test fun answeredAndEndedQuestionsLoseTheirCardsWhileWaitingOnesKeepThem() {
        val state = state(listOf(session(ids[0]), session(ids[1], Status.NEEDS_YOU), session(ids[2], Status.NEEDS_YOU, alive = false)))
        assertEquals(setOf(ids[0], ids[2]), AttentionNotifications.stale(state, ids.toSet()))
        assertTrue("a new State cannot recreate a dismissed card", AttentionNotifications.stale(state, emptySet()).isEmpty())
    }
    @Test fun aDelayedAlertPostsAQuestionOnlyWhileTheSessionStillWaits() {
        val resolved = state(listOf(session(ids[0])))
        assertFalse(AttentionNotifications.prompts(resolved, ids[0], false))
        assertTrue(AttentionNotifications.prompts(resolved, ids[0], true))
        assertTrue(AttentionNotifications.prompts(state(listOf(session(ids[0], Status.NEEDS_YOU))), ids[0], false))
        assertTrue(AttentionNotifications.prompts(resolved.copy(attentionCapable = false), ids[0], false))
        assertTrue(AttentionNotifications.prompts(null, ids[0], false))
    }
}
