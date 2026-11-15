package io.github.cepheus.app.store

import io.github.cepheus.app.protocol.AttentionContracts
import org.junit.Assert.*
import org.junit.Test

class AttentionDraftsTest {
    private class Memory : KeyValue {
        val strings = mutableMapOf<String, String>()
        override fun getString(key: String) = strings[key]
        override fun putString(key: String, value: String?) { if (value == null) strings.remove(key) else strings[key] = value }
        override fun getLong(key: String): Long? = null
        override fun putLong(key: String, value: Long?) = Unit
    }
    private fun message(session: Long, state: String) = AttentionContracts.Message("receipt", session, state, 1, 2, "categorical")
    @Test fun lostReplyKeepsOriginalKeyAndPayloadAcrossReopen() {
        val memory = Memory()
        val pending = AttentionDrafts(memory).prepare(7, "private text")
        val reopened = AttentionDrafts(memory)
        assertEquals(pending, reopened.prepare(7, "private text"))
        try { reopened.prepare(7, "different text"); fail("Ambiguous delivery replaced") } catch (_: IllegalArgumentException) { }
        assertEquals("private text", reopened.pending(7)?.text)
        assertFalse(pending.toString().contains("private text"))
    }
    @Test fun unknownDeliveryNeverAllocatesAnotherKeyAndOtherSessionsStaySeparate() {
        val memory = Memory(); val drafts = AttentionDrafts(memory)
        val pending = drafts.prepare(7, "one")
        drafts.received(pending, message(7, "unknown"))
        assertEquals(pending.key, AttentionDrafts(memory).prepare(7, "one").key)
        try { drafts.prepare(7, "two"); fail("Unknown delivery replaced") } catch (_: IllegalArgumentException) { }
        assertNotEquals(pending.key, drafts.prepare(8, "one").key)
        try { drafts.received(pending, message(8, "submitted")); fail("Wrong session acknowledged") } catch (_: IllegalArgumentException) { }
    }
    @Test fun onlyADurableAcceptanceAllowsADifferentDraft() {
        val memory = Memory(); val drafts = AttentionDrafts(memory)
        val first = drafts.prepare(7, "first")
        // A generic gateway Err can follow backend acceptance. Nothing clears
        // this fence merely because the transport answered with an error.
        assertEquals(first.key, AttentionDrafts(memory).prepare(7, "first").key)
        try { drafts.prepare(7, "different"); fail("Generic error lost the request fence") } catch (_: IllegalArgumentException) { }
        drafts.received(first, message(7, "queued"))
        assertNotEquals(first.key, drafts.prepare(7, "next message").key)
    }
    @Test fun genericErrorAndChangedCurrentHostNeverRedirectTheOriginalSend() {
        val memory = Memory(); val drafts = AttentionDrafts(memory)
        val target = AttentionContracts.Target(1001, 99, "original-conversation")
        val original = drafts.prepare(7, "same text", origin = "first-worker", target = target)
        val replay = AttentionDrafts(memory).prepare(7, "same text", origin = "different-worker", target = target.copy(spawnedMs = 2002, pid = 101, conversation = "replacement"))
        assertEquals(original, replay)
        assertEquals("first-worker", AttentionContracts.send(replay.key, replay.session, replay.text, replay.origin, replay.target).getJSONObject("args").getString("host"))
        try { drafts.received(replay, message(7, "submitted")); fail("Local receipt replaced remote origin") } catch (_: IllegalArgumentException) { }
        assertTrue(drafts.pending(7)?.unresolved == true)
        drafts.received(replay, message(7, "submitted").copy(host = "first-worker"))
        assertEquals("first-worker", drafts.pending(7)?.origin)
        val acceptedReplay = AttentionDrafts(memory).prepare(7, "same text", target = target.copy(spawnedMs = 3003, pid = 102))
        assertEquals(original.key, acceptedReplay.key)
        assertEquals(target, acceptedReplay.target)
        assertEquals(target, replay.target) // Also frozen before any first admission receipt.
    }
    @Test fun legacyPendingWithoutCapturedTargetNeverInfersAReplacementOrChangesItsBytes() {
        val memory = Memory()
        val raw = """{"key":"old-key","session":7,"text":"private old body","origin":"@here"}"""
        memory.putString("attention/send/7", raw)
        val old = AttentionDrafts(memory).prepare(7, "private old body", target = AttentionContracts.Target(2002, 101, "new-thread"))
        assertNull(old.target)
        try { AttentionContracts.send(old.key, old.session, old.text, old.origin, old.target); fail("Legacy target inferred") }
        catch (e: IllegalArgumentException) { assertEquals(io.github.cepheus.app.protocol.Controls.UPDATE_NEEDED, e.message) }
        assertEquals(raw, memory.getString("attention/send/7"))
    }
    @Test fun laterUnknownMetadataRetainsFenceWithoutCrossingEqualUuidOrigins() {
        val memory = Memory(); val drafts = AttentionDrafts(memory)
        val original = drafts.prepare(7, "same text", origin = "worker")
        drafts.received(original, message(7, "held").copy(host = "worker"))
        drafts.observe(listOf(message(7, "unknown")))
        assertEquals("held", drafts.pending(7)?.state)
        drafts.observe(listOf(message(7, "unknown").copy(host = "worker")))
        assertEquals("unknown", AttentionDrafts(memory).pending(7)?.state)
        try { drafts.prepare(7, "same text", origin = "worker", newCopy = true); fail("Unknown delivery repeated") } catch (_: IllegalArgumentException) { }
        assertEquals(original.key, drafts.prepare(7, "same text", origin = "changed-host").key)
    }
    @Test fun staleAcceptanceCannotWeakenAStatusObservedBeforeOrAfterItsReceipt() {
        for (state in listOf("pasted", "unknown", "future-attempt-state")) {
            val memory = Memory(); val drafts = AttentionDrafts(memory)
            val original = drafts.prepare(7, "one", origin = "worker")
            val accepted = message(7, "held").copy(host = "worker")
            drafts.observe(listOf(accepted.copy(state = state)))
            drafts.observe(emptyList()) // A temporarily missing peer cannot erase the evidence.
            assertEquals(state, drafts.received(original, accepted).state)
            assertEquals(state, drafts.pending(7)?.state)
            drafts.received(original, accepted.copy(state = "queued"))
            assertEquals(state, AttentionDrafts(memory).pending(7)?.state)
            try { drafts.prepare(7, "one", origin = "worker", newCopy = true); fail("Stale acceptance lost the attempt fence") } catch (_: IllegalArgumentException) { }
        }
        val drafts = AttentionDrafts(Memory())
        val original = drafts.prepare(7, "one")
        drafts.received(original, message(7, "held"))
        drafts.observe(listOf(message(7, "unknown")))
        drafts.received(original, message(7, "held"))
        assertEquals("unknown", drafts.pending(7)?.state)
    }
    @Test fun boundedCandidateCacheCannotDropAnUnknownForAnAlreadyBoundReceipt() {
        val drafts = AttentionDrafts(Memory())
        drafts.prepare(8, "not bound yet")
        val original = drafts.prepare(7, "bound")
        drafts.received(original, message(7, "held"))
        drafts.observe(listOf(message(8, "unknown").copy(id = "waiting-for-first-reply")))
        val snapshot = (100L..10098L).map { message(it, "submitted").copy(id = "other-$it") } + message(7, "unknown")
        assertEquals(10000, snapshot.size)
        drafts.observe(snapshot)
        assertEquals("unknown", drafts.pending(7)?.state)
        drafts.received(original, message(7, "held"))
        assertEquals("unknown", drafts.pending(7)?.state)
        try { drafts.prepare(7, "bound", newCopy = true); fail("Bounded cache dropped known attempt evidence") } catch (_: IllegalArgumentException) { }
    }
    @Test fun definitiveSubmissionAdvancesAPasteButCannotRegressToPreattempt() {
        val drafts = AttentionDrafts(Memory())
        val original = drafts.prepare(7, "one")
        drafts.observe(listOf(message(7, "pasted")))
        drafts.received(original, message(7, "held"))
        drafts.observe(listOf(message(7, "submitted")))
        drafts.received(original, message(7, "queued"))
        assertEquals("submitted", drafts.pending(7)?.state)
        assertNotEquals(original.key, drafts.prepare(7, "one", newCopy = true).key)
    }
    @Test fun identicalNewCopyRequiresExplicitIntentAndDurableAcceptance() {
        val memory = Memory(); val drafts = AttentionDrafts(memory)
        val original = drafts.prepare(7, "same text")
        try { drafts.prepare(7, "same text", newCopy = true); fail("Unconfirmed original replaced") } catch (_: IllegalArgumentException) { }
        drafts.received(original, message(7, "submitted"))
        assertEquals(original.key, AttentionDrafts(memory).prepare(7, "same text").key)
        assertNotEquals(original.key, AttentionDrafts(memory).prepare(7, "same text", newCopy = true).key)
    }
}
