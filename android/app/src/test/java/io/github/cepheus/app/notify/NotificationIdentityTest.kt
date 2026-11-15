package io.github.cepheus.app.notify

import org.junit.Assert.*
import org.junit.Test

class NotificationIdentityTest {
    private val ids = listOf(1L, (1L shl 40) or 1L, (2L shl 40) or 1L)
    @Test fun equalLocalIdsOnThreeHostsHaveDistinctPostedKeysAndRoundTripInFull() {
        assertEquals(1, ids.map { it.toInt() }.toSet().size) // Actual legacy collision.
        val tags = ids.map { NotificationIdentity.tag("needs_you", "connection", it) }
        assertEquals(3, tags.toSet().size)
        ids.zip(tags).forEach { (id, tag) -> assertEquals(id, NotificationIdentity.session("needs_you", "connection", tag, id.toInt())) }
    }
    @Test fun activityDataSeparatesFullSessionConnectionAndScreen() {
        val data = ids.map { NotificationIdentity.open("connection", it, null) }
        assertEquals(3, data.toSet().size)
        assertNotEquals(data[0], NotificationIdentity.open("other", ids[0], null))
        assertNotEquals(data[0], NotificationIdentity.open("connection", null, "errors"))
        assertTrue(NotificationIdentity.matchesOpen(data[1], "connection", ids[1], null))
        assertFalse(NotificationIdentity.matchesOpen(data[1], "connection", ids[0], null))
    }
    @Test fun answerAndReplyDataPreserveTargetSlotAndPromptCustody() {
        for (role in listOf("answer", "reply")) {
            assertEquals(1, ids.map { (it * 8 + 1).toInt() }.toSet().size)
            val data = ids.map { NotificationIdentity.action("connection", it, role, 1, 7, "fingerprint") }
            assertEquals(3, data.toSet().size)
            assertTrue(NotificationIdentity.matchesAction(data[1], "connection", ids[1], role, 1, 7, "fingerprint"))
            assertFalse(NotificationIdentity.matchesAction(data[1], "connection", ids[0], role, 1, 7, "fingerprint"))
            assertFalse(NotificationIdentity.matchesAction(data[1], "other", ids[1], role, 1, 7, "fingerprint"))
            assertFalse(NotificationIdentity.matchesAction(data[1], "connection", ids[1], role, 2, 7, "fingerprint"))
            assertFalse(NotificationIdentity.matchesAction(data[1], "connection", ids[1], role, 1, 8, "fingerprint"))
            assertFalse(NotificationIdentity.matchesAction(data[1], "connection", ids[1], role, 1, 7, "new fingerprint"))
        }
        assertNotEquals(NotificationIdentity.action("connection", 1, "answer", 1, 7, "fp"), NotificationIdentity.action("connection", 1, "reply", 1, 7, "fp"))
    }
    @Test fun legacyAndMismatchedIntentDataNeverAcquireCurrentExtrasAuthority() {
        val legacy = "cepheus://connection/connection"
        assertFalse(NotificationIdentity.matchesOpen(legacy, "connection", ids[2], null))
        assertFalse(NotificationIdentity.matchesOpen(legacy, "connection", null, null))
        assertFalse(NotificationIdentity.matchesAction(legacy, "connection", ids[2], "answer", 1, 7, "fp"))
        assertFalse(NotificationIdentity.matchesAction(null, "connection", ids[2], "reply", 0, null, null))
        val stale = NotificationIdentity.action("connection", ids[1], "answer", 1, 7, "old")
        assertFalse(NotificationIdentity.matchesAction(stale, "connection", ids[2], "answer", 1, 7, "new"))
    }
    @Test fun currentConnectionAndKindFilterNeverTreatLegacyIntIdsAsFullTargets() {
        val tag = NotificationIdentity.tag("needs_you", "connection", ids[1])
        assertNull(NotificationIdentity.session("needs_you", "other", tag, 1))
        assertNull(NotificationIdentity.session("finished", "connection", tag, 1))
        assertNull(NotificationIdentity.session("needs_you", "connection", "needs_you", 1))
        assertNull(NotificationIdentity.session("needs_you", "connection", tag, 2))
        assertTrue(NotificationIdentity.retired("attention_digest", 0))
        assertTrue(NotificationIdentity.retired(NotificationIdentity.tag("attention_digest", "old/connection", 0), 0))
        assertFalse(NotificationIdentity.retired(NotificationIdentity.tag("needs_you", "connection", ids[1]), 1))
        assertTrue(NotificationIdentity.retired(NotificationIdentity.tag("attention_warning", "connection", ids[1]), 1))
        assertFalse(NotificationIdentity.retired(NotificationIdentity.tag("finished", "connection", ids[1]), 1))
        assertFalse(NotificationIdentity.retired("attention_digest:connection:1", 0))
        assertFalse(NotificationIdentity.retired(null, 1))
    }
    @Test fun oneHundredFederatedSessionKeysRemainDistinctWithCollidingAndroidInts() {
        val fleet = (0 until 100).map { ((it % 2 + 1).toLong() shl 40) or (it / 2 + 1).toLong() }
        assertEquals(50, fleet.map { it.toInt() }.toSet().size)
        assertEquals(100, fleet.map { NotificationIdentity.tag("finished", "connection", it) }.toSet().size)
        assertEquals(100, fleet.map { NotificationIdentity.action("connection", it, "reply", 0, null, null) }.toSet().size)
    }
    @Test fun routeEncodingSeparatesComponentsAndKeepsFingerprintTextPrivate() {
        assertNotEquals(NotificationIdentity.open("a/b", 1, null), NotificationIdentity.open("a%2Fb", 1, null))
        val privateText = "private question <details> & contents"
        val route = NotificationIdentity.action("connection", 1, "answer", 0, 1, privateText)
        assertFalse(route.contains(privateText)); assertFalse(route.contains("question"))
    }
}
