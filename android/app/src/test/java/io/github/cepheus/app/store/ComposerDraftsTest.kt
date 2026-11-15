package io.github.cepheus.app.store

import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ComposerDraftsTest {
    private class Memory : KeyValue {
        val values = mutableMapOf<String, String>()
        override fun getString(key: String): String? = values[key]
        override fun putString(key: String, value: String?) { if (value == null) values.remove(key) else values[key] = value }
        override fun getLong(key: String): Long? = values[key]?.toLongOrNull()
        override fun putLong(key: String, value: Long?) { if (value == null) values.remove(key) else values[key] = value.toString() }
    }

    @Test fun relocationPreservesTheCurrentEditSelectionAndRecoveryCopy() {
        val storage = Memory()
        val drafts = ComposerDrafts(storage)
        val edit = TextFieldValue("unfinished\nnext task", TextRange(3, 12))
        drafts.field("7/message").value = edit
        assertTrue(drafts.copyMessage(7, 91))
        assertEquals(edit, drafts.field("91/message").value)
        assertEquals(edit, ComposerDrafts(storage).field("91/message").value)
        assertEquals(edit, drafts.field("7/message").value)
        drafts.field("92/message").value = TextFieldValue("another current edit")
        assertFalse(drafts.copyMessage(7, 92))
        assertEquals("another current edit", drafts.field("92/message").value.text)
        assertEquals(edit, drafts.field("7/message").value)
    }
}
