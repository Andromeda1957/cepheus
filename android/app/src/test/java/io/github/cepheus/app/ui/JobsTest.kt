package io.github.cepheus.app.ui

import io.github.cepheus.app.protocol.Item
import io.github.cepheus.app.protocol.JobContracts
import org.json.JSONArray
import org.junit.Assert.*
import org.junit.Test

class JobsTest {
    @Test fun programFieldsPreserveLiteralArgumentsAndEmptyPositions() {
        val args = JSONArray(jobArguments("/tool path", "two words\n$(echo example); *\n\nlast"))
        assertEquals(listOf("/tool path", "two words", "$(echo example); *", "", "last"), (0 until args.length()).map { args.getString(it) })
        assertEquals(1, JSONArray(jobArguments("/tool", "")).length())
    }

    @Test fun toolResultOnNextPagePairsWithItsCallWithoutUsingByteCursorAsRowIndex() {
        val call = Item.Tool("one", "Bash", "test", "{}")
        val first = JobContracts.Output(22).append(0, JobContracts.Page(22, 1024, "record1", listOf(call), false))
        val final = first.append(1024, JobContracts.Page(22, 2048, "record2", listOf(call, Item.Result("one", "ok", false), Item.Assistant("Finished", null)), true))
        val rows = jobChatRows(final.items)
        assertEquals(2, rows.size)
        val beforeTrim = jobChatRows(listOf(Item.Notice("earlier")) + final.items)
        assertEquals(beforeTrim.first { it.item == call }.key, rows.first().key)
        assertEquals("ok", rows.first().result?.text)
        assertEquals(2048L, final.offset)
        assertEquals(2, chatEntries(rows).size)
    }
}
