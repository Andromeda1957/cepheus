package io.github.cepheus.app.store

import io.github.cepheus.app.protocol.AlertKind
import io.github.cepheus.app.protocol.ServerMessage
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AlertBookTest {
    private class Memory : KeyValue {
        val strings = mutableMapOf<String, String>()
        val longs = mutableMapOf<String, Long>()
        override fun getString(key: String) = strings[key]
        override fun putString(key: String, value: String?) {
            if (value == null) strings.remove(key) else strings[key] = value
        }
        override fun getLong(key: String) = longs[key]
        override fun putLong(key: String, value: Long?) {
            if (value == null) longs.remove(key) else longs[key] = value
        }
    }

    private val now = 1_800_000_000_000L

    private fun alert(seq: Long, epoch: String = "e1", at: Long = now) =
        ServerMessage.Alert(seq, epoch, AlertKind.FINISHED, 3, "t", "r", false, at)

    @Test
    fun aReconnectReplaysNothingTwice() {
        val kv = Memory()
        val book = AlertBook(kv)
        book.hello("e1")
        assertTrue(book.accept(alert(1), now))
        assertTrue(book.accept(alert(2), now))
        // The gateway resends from last_seq; a second copy is not news.
        assertFalse(book.accept(alert(2), now))
        assertFalse(book.accept(alert(1), now))
        assertEquals(2L, book.lastSeq)
        assertEquals("e1", book.epoch)

        // Kept across restarts of the app.
        val again = AlertBook(kv)
        again.hello("e1")
        assertFalse(again.accept(alert(2), now))
        assertTrue(again.accept(alert(3), now))
    }

    @Test
    fun aNewGatewayStartsNumberingAgain() {
        val book = AlertBook(Memory())
        book.hello("e1")
        book.accept(alert(40), now)
        book.hello("e2")
        assertNull(book.lastSeq)
        assertTrue(book.accept(alert(1, "e2"), now))
        assertEquals("e2", book.epoch)
    }

    @Test
    fun anAlertFromAnotherEpochResetsWithoutHello() {
        val book = AlertBook(Memory())
        book.hello("e1")
        book.accept(alert(40), now)
        assertTrue(book.accept(alert(1, "e2"), now))
        assertEquals(1L, book.lastSeq)
    }

    @Test
    fun replayedOldAlertsAreRecordedButNotNotified() {
        val book = AlertBook(Memory())
        book.hello("e1")
        assertFalse(book.accept(alert(1, at = now - AlertBook.FRESH_MS - 1), now))
        assertEquals(1L, book.lastSeq)
        assertTrue(book.accept(alert(2, at = now - AlertBook.FRESH_MS), now))
    }

    @Test
    fun aWaitingPromptNotifiesOncePerFingerprint() {
        val kv = Memory()
        val book = AlertBook(kv)
        assertTrue(book.promptIsNew(3, "fp1"))
        assertFalse(book.promptIsNew(3, "fp1"))
        assertFalse(AlertBook(kv).promptIsNew(3, "fp1"))
        assertTrue(book.promptIsNew(3, "fp2"))
        assertTrue(book.promptIsNew(4, "fp2"))
        book.promptGone(3)
        assertTrue(book.promptIsNew(3, "fp2"))
    }

}
