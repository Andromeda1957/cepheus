package io.github.cepheus.app.store

import io.github.cepheus.app.protocol.AlertKind
import io.github.cepheus.app.protocol.ServerMessage
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ConnectionsTest {
    private class Memory : KeyValue {
        private val strings = mutableMapOf<String, String>()
        private val longs = mutableMapOf<String, Long>()
        override fun getString(key: String) = strings[key]
        override fun putString(key: String, value: String?) {
            if (value == null) strings.remove(key) else strings[key] = value
        }
        override fun getLong(key: String) = longs[key]
        override fun putLong(key: String, value: Long?) {
            if (value == null) longs.remove(key) else longs[key] = value
        }
    }

    private fun store(kv: KeyValue) = Connections(kv, { it }, { it })
    private fun credentials(host: String) = Credentials(host, 8765, "a".repeat(64), "phone", "key-$host")

    @Test
    fun switchingAndRemovingOneGatewayPreservesTheOtherPairingsAcrossAppRestarts() {
        val kv = Memory()
        val saved = store(kv)
        val first = saved.save(credentials("first.invalid"), "First", legacy = true)
        val second = saved.save(credentials("second.invalid"), "Second")
        val third = saved.save(credentials("third.invalid"), "Third")
        assertTrue(saved.select(first.id))
        assertEquals(first.credentials, store(kv).active()?.credentials)
        saved.remove(first.id)
        assertEquals(setOf(second, third), store(kv).all().toSet())
        assertEquals(second.credentials, store(kv).active()?.credentials)
        // Re-pairing changes the sign-in, while the gateway keeps its cache namespace.
        val updated = saved.save(second.credentials.copy(key = "replacement"), "Second")
        assertEquals(second.id, updated.id)
        assertEquals(2, store(kv).all().size)
        assertEquals("replacement", store(kv).active()?.credentials?.key)
        assertFalse(saved.select(first.id))
        assertEquals(updated, saved.active())
    }

    @Test
    fun identicalAlertAndSessionNumbersOnDifferentGatewaysDoNotSuppressEachOther() {
        val kv = Memory()
        val saved = store(kv)
        val first = saved.save(credentials("first.invalid"), "First", legacy = true)
        val second = saved.save(credentials("second.invalid"), "Second")
        val alert = ServerMessage.Alert(1, "same-epoch", AlertKind.FINISHED, 1, "Title", "", false, 1000)
        val firstBook = AlertBook(ConnectionKeyValue(kv, first.id))
        val secondBook = AlertBook(ConnectionKeyValue(kv, second.id))
        assertTrue(firstBook.accept(alert, 1000))
        assertTrue(firstBook.promptIsNew(1, "same-question"))
        assertTrue(secondBook.accept(alert, 1000))
        assertTrue(secondBook.promptIsNew(1, "same-question"))
        assertFalse(AlertBook(ConnectionKeyValue(kv, first.id)).accept(alert, 1000))
        assertFalse(AlertBook(ConnectionKeyValue(kv, second.id)).promptIsNew(1, "same-question"))
    }
}
