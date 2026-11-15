package io.github.cepheus.app.store

import io.github.cepheus.app.protocol.Protocol
import io.github.cepheus.app.protocol.AttentionContracts
import io.github.cepheus.app.protocol.ServerMessage
import org.junit.Assert.*
import org.junit.Test
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking

class SessionReadinessTest {
    @Test fun ownerMessagesCannotDowngradeThroughAnOldOrCachedGateway() {
        val current = state(listOf(7)).copy(protocol = 21)
        val gate = SessionReadiness(cached = current)
        val messages = listOf(
            io.github.cepheus.app.protocol.Requests.send(1, 7, "original"),
            io.github.cepheus.app.protocol.Requests.send(2, (3L shl 40) or 7, "remote"),
            io.github.cepheus.app.protocol.Requests.sendNow(3, 7),
            io.github.cepheus.app.protocol.Requests.control(4, "broadcast", org.json.JSONObject().put("text", "original")),
            io.github.cepheus.app.protocol.Requests.new(5, "codex", null, null, "original", null),
            io.github.cepheus.app.protocol.Requests.control(6, "create", org.json.JSONObject().put("text", "original")),
            io.github.cepheus.app.protocol.Requests.control(7, "fleet", org.json.JSONObject().put("operation", "attention-send")),
        ).map(::orgJson)
        messages.forEach { assertNotNull("cached protocol must not authorize a write", gate.messageError(it)) }
        gate.accept(current.copy(protocol = 18))
        messages.forEach { assertTrue(gate.messageError(it)!!.contains("Update")) }
        gate.accept(current)
        messages.forEach { assertNull(gate.messageError(it)) }
        gate.disconnected()
        messages.forEach { assertNotNull(gate.messageError(it)) }
        gate.accept(current.copy(ready = false))
        messages.forEach { assertNotNull(gate.messageError(it)) }
        assertNull("raw terminal controls remain explicit", gate.messageError(orgJson("""{"t":"control","action":"terminal_input"}""")))
    }
    private fun orgJson(raw: String) = org.json.JSONObject(raw)

    /** Sessions `ids`; another `created` time is another session under the same IDs. */
    private fun state(ids: List<Long>, ready: Boolean = true, created: Long = 10) =
        (Protocol.parse("""{"t":"state","ready":$ready,"sessions":[${ids.joinToString { id ->
            """{"id":$id,"tool":"codex","created_at":$created,"alive":true}"""
        }}]}""") as ServerMessage.State).state

    @Test fun cachedHistoryDoesNotAuthorizeBeforeStateOrAfterAServiceLoss() {
        val cached = state(listOf(7))
        val gate = SessionReadiness(cached = cached)
        assertTrue(gate.error(7)!!.contains("not ready"))
        gate.accept(cached)
        assertNull(gate.error(7))
        val originalReply = gate.marker()
        gate.disconnected()
        assertTrue(gate.error(7)!!.contains("not ready"))
        gate.accept(cached.copy(ready = false))
        assertTrue(gate.error(7)!!.contains("not ready"))
        gate.accept(cached)
        assertNull(gate.error(7))
        assertFalse("a late reply cannot overwrite the reconnected cache", gate.accepts(7, originalReply))
        assertTrue(gate.accepts(7, gate.marker()))
    }

    @Test fun missingOrReusedRoutesNeverInheritAnOldSelectionsAuthority() {
        val old = state(listOf(7))
        val storage = MemoryKeyValue()
        val gate = SessionReadiness(storage, old)
        val originalReply = gate.marker()
        gate.accept(state(listOf((2L shl 40) or 7L)))
        assertTrue(gate.error(7)!!.contains("unavailable"))
        gate.accept(state(listOf(7), created = 11))
        assertTrue(gate.error(7)!!.contains("identity changed"))
        assertFalse("a reused ID cannot receive an old chat reply", gate.accepts(7, originalReply))
        // Restarting with the refreshed list cannot retarget a durable draft either.
        val restarted = SessionReadiness(storage, state(listOf(7), created = 11))
        restarted.accept(state(listOf(7), created = 11))
        assertTrue(restarted.error(7)!!.contains("identity changed"))
    }

    @Test fun fourHostsSharingNativeIdsRemainSeparateThroughCacheRoundTrip() {
        val ids = (0L..3L).map { (it shl 40) or 7L }
        val current = state(ids)
        val cached = (Protocol.parse(Protocol.stateJson(current)) as ServerMessage.State).state
        assertEquals(ids, cached.sessions.map { it.id })
        val gate = SessionReadiness(cached = cached)
        gate.accept(current)
        ids.forEach { id -> assertNull(gate.error(id)) }
    }







    @Test fun aRecoveredSessionTakesMessagesAgainAndAnotherSessionDoesNot() {
        val first = state(listOf(7, 8)).let { state -> state.copy(sessions = state.sessions.map {
            it.copy(pid = 123, conversation = "thread", expectedTarget = AttentionContracts.Target(100, 123, "thread"))
        }) }
        val restored = first.copy(sessions = first.sessions.map {
            if (it.id == 7L) it.copy(pid = 456, expectedTarget = AttentionContracts.Target(200, 456, "thread")) else it
        })
        val storage = MemoryKeyValue()
        val gate = SessionReadiness(storage, first)
        assertEquals(emptySet<Long>(), gate.accept(first))
        assertEquals("the chat of a moved session is read again", setOf(7L), gate.accept(restored))
        assertNull("the same session in a new process takes messages", gate.error(7))
        assertTrue(gate.accepts(7, gate.marker()))
        assertNull(gate.error(8))
        val restarted = SessionReadiness(storage, restored)
        assertEquals("the moved custody survives restarting the phone", emptySet<Long>(), restarted.accept(restored))
        assertNull(restarted.error(7))

        val cleared = restored.copy(sessions = restored.sessions.map {
            if (it.id == 7L) it.copy(conversation = "cleared", expectedTarget = AttentionContracts.Target(200, 456, "cleared")) else it
        })
        assertEquals("a desk /clear keeps the session", setOf(7L), gate.accept(cleared))
        assertNull(gate.error(7))

        val ended = cleared.copy(sessions = cleared.sessions.map { if (it.id == 7L) it.copy(alive = false, pid = null) else it })
        assertEquals(emptySet<Long>(), gate.accept(ended))
        assertNull("ended history remains readable", gate.error(7))

        val current = cleared.sessions.first { it.id == 7L }
        for (other in listOf(current.copy(host = "other"),
            current.copy(tool = "claude"), current.copy(createdAt = current.createdAt + 1))) {
            val isolated = SessionReadiness(cached = cleared)
            isolated.accept(cleared)
            val moved = isolated.accept(cleared.copy(sessions = listOf(other.copy(pid = 789))))
            assertEquals(emptySet<Long>(), moved)
            assertTrue("another session under this ID stays refused", isolated.error(7)!!.contains("identity changed"))
        }

        // A saved list is not the deck's word: it never moves custody.
        val saved = MemoryKeyValue()
        SessionReadiness(saved, first)
        SessionReadiness(saved, restored)
        val reloaded = SessionReadiness(saved, first)
        assertEquals(emptySet<Long>(), reloaded.accept(first))
        assertNull(reloaded.error(7))
        val stopped = SessionReadiness(saved)
        assertEquals(setOf(7L), stopped.accept(restored.copy(sessions = restored.sessions.map { it.copy(alive = false) })))
        assertNull("a session that ended in a new process keeps its history readable", stopped.error(7))
    }

    @Test fun reopenAndClearFollowTheSessionAndAcknowledgeOnlyTheirOwnProcess() = runBlocking {
        val legacy = state(listOf(7))
        val first = legacy.copy(sessions = legacy.sessions.map {
            it.copy(pid = 123, conversation = "first", expectedTarget = AttentionContracts.Target(100, 123, "first"))
        })
        val storage = MemoryKeyValue()
        val gate = SessionReadiness(storage, legacy)
        gate.accept(first)
        assertNull(gate.error(7))
        gate.accept(first.copy(sessions = first.sessions.map { it.copy(expectedTarget = null) }))
        assertNull("protocol14 metadata remains compatible without discarding known custody", gate.error(7))
        assertEquals(setOf(7L), gate.accept(first.copy(sessions = first.sessions.map {
            it.copy(expectedTarget = AttentionContracts.Target(101, 123, "first"))
        })))
        assertNull("a new spawn of the same session takes custody", gate.error(7))
        gate.accept(first.copy(sessions = first.sessions.map { it.copy(alive = false, pid = null) }))
        assertNull("ended history remains accessible", gate.error(7))

        val beforeReopen = gate.publicationMarker()
        val reopened = first.copy(sessions = first.sessions.map {
            it.copy(pid = 456, expectedTarget = AttentionContracts.Target(200, 456, "first"))
        })
        assertEquals(setOf(7L), gate.accept(reopened))
        assertNull(gate.error(7))
        val candidate = gate.awaitPublishedTarget(7, 10_000, beforeReopen, reopening = true)
        assertNotNull("a correlated reopen observes its freshly published process", candidate)
        assertTrue(gate.awaitTarget(7, 20))
        gate.accept(reopened.copy(sessions = reopened.sessions.map {
            it.copy(pid = 789, expectedTarget = AttentionContracts.Target(201, 789, "first"))
        }))
        assertFalse("acknowledgement cannot recapture an earlier process",
            gate.acknowledgePublishedTarget(candidate!!, beforeReopen, reopening = true))
        gate.accept(reopened)
        assertTrue(gate.acknowledgePublishedTarget(candidate, beforeReopen, reopening = true))
        assertNull(gate.error(7))
        val restarted = SessionReadiness(storage, reopened)
        restarted.accept(reopened)
        assertNull("the acknowledged process survives a phone restart", restarted.error(7))

        val beforeClear = gate.publicationMarker()
        assertEquals(setOf(7L), gate.accept(reopened.copy(sessions = reopened.sessions.map {
            it.copy(pid = 789, conversation = "cleared", expectedTarget = AttentionContracts.Target(300, 789, "cleared"))
        })))
        assertNull(gate.error(7))
        val cleared = gate.awaitPublishedTarget(7, 10_000, beforeClear)
        assertNotNull("an explicit clear may acknowledge its fresh conversation", cleared)
        assertTrue(gate.acknowledgePublishedTarget(cleared!!, beforeClear))
        assertNull(gate.error(7))
    }

    @Test fun createdReceiptWaitsForTheExactPublishedTargetOnEachHost() = runBlocking {
        for (tag in 0L..3L) {
            val id = (tag shl 40) or 7L
            val gate = SessionReadiness()
            // Other hosts already use the same native ID; none can satisfy this receipt.
            val others = (0L..3L).filter { it != tag }.map { (it shl 40) or 7L }
            gate.accept(state(others))
            val opened = async(start = CoroutineStart.UNDISPATCHED) { gate.awaitTarget(id, 10_000) }
            assertFalse("Created is not an authoritative session record", opened.isCompleted)
            gate.accept(state(others + id).let { it.copy(sessions = it.sessions.map { s -> if (s.id == id) s.copy(alive = false) else s }) })
            assertFalse("an old ended record is not a usable created target", opened.isCompleted)
            gate.accept(state(others + id))
            assertTrue(opened.await())
        }
    }

    @Test fun serviceReplacementCannotFinishAnOldCreatedWait() = runBlocking {
        val gate = SessionReadiness()
        gate.accept(state(emptyList()))
        val beforeCreate = gate.marker()
        val opened = async(start = CoroutineStart.UNDISPATCHED) { gate.awaitTarget(7, 10_000) }
        gate.disconnected()
        gate.accept(state(listOf(7)))
        assertFalse(opened.await())
        assertFalse("a reply observed after reconnection still belongs to the old create",
            gate.awaitTarget(7, 10_000, beforeCreate))
    }

    @Test fun clearWaitRequiresANewPublicationAndKeepsItsOriginalConnection() = runBlocking {
        val gate = SessionReadiness()
        gate.accept(state(listOf(7, 91)))
        val beforeClear = gate.publicationMarker()
        val cleared = async(start = CoroutineStart.UNDISPATCHED) {
            gate.awaitPublishedTarget(91, 10_000, beforeClear)
        }
        assertFalse("a live cached route cannot complete a later clear receipt", cleared.isCompleted)
        gate.accept(state(listOf(7, 91)))
        val replacement = cleared.await()
        assertNotNull(replacement)
        assertTrue(gate.acknowledgePublishedTarget(replacement!!, beforeClear))

        val beforeReconnect = gate.publicationMarker()
        val interrupted = async(start = CoroutineStart.UNDISPATCHED) {
            gate.awaitPublishedTarget(92, 10_000, beforeReconnect)
        }
        gate.disconnected()
        gate.accept(state(listOf(7, 91, 92)))
        assertNull("a replacement connection cannot finish an old clear", interrupted.await())
        assertNull(gate.awaitPublishedTarget(92, 10_000, beforeReconnect))
        assertFalse("an observed candidate cannot be acknowledged after reconnect",
            gate.acknowledgePublishedTarget(replacement, beforeClear))

        val beforeReuse = gate.publicationMarker()
        gate.accept(state(listOf(7, 91, 92), created = 11))
        assertNull("a same-ID replacement must preserve the old draft's custody",
            gate.awaitPublishedTarget(91, 20, beforeReuse))
    }

    private class MemoryKeyValue : KeyValue {
        private val values = mutableMapOf<String, String>()
        override fun getString(key: String) = values[key]
        override fun putString(key: String, value: String?) { if (value == null) values.remove(key) else values[key] = value }
        override fun getLong(key: String) = values[key]?.toLongOrNull()
        override fun putLong(key: String, value: Long?) = putString(key, value?.toString())
    }
}
