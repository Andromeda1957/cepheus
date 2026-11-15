package io.github.cepheus.app.ui

import io.github.cepheus.app.protocol.Conversation
import org.junit.Assert.assertEquals
import org.junit.Test

/** The resume sheet's filter, as the deck's `App::picker_matches` (cepheus/src/tui.rs) filters Alt+r. */
class ResumeTest {
    private val parser = Conversation("claude", "c0ffee00-1", "Tidy the parser", 1, 2, null)
    private val build = Conversation("codex", "019a2b3c", "Fix the build", 1, 2, null)
    private val agy = Conversation("antigravity", "5e6f7a8b", "Draw the board", 1, 2, 3)
    private val grok = Conversation("grok", "01a16bf6", "Parser speed", 1, 2, null)
    private val open = Conversation("opencode", "ses_ead42ac29ffei5Xcr4QiFwE9uu", "Compare", 1, 2, null)
    private val all = listOf(parser, build, agy, grok, open)

    private fun ids(filter: String) = conversationMatches(all, filter).map { it.id }

    @Test
    fun everyWordMustMatchTheTitleTheIdsStartOrTheProgram() {
        assertEquals("no filter keeps every conversation, in order", all.map { it.id }, ids(""))
        assertEquals(all.map { it.id }, ids("   "))
        assertEquals(listOf(parser.id, grok.id), ids("PARSER"))
        assertEquals(listOf(grok.id), ids("parser grok"))
        assertEquals(listOf(build.id), ids("codex"))
        assertEquals(listOf(build.id), ids("019a"))
        assertEquals("Antigravity answers to its command name", listOf(agy.id), ids("agy"))
        assertEquals(listOf(agy.id), ids("anti board"))
        assertEquals(listOf(open.id), ids("open"))
        assertEquals("an opencode id mixes cases", listOf(open.id), ids("ses_ead42ac29ffei5xcr4"))
        assertEquals(emptyList<String>(), ids("parser codex"))
        assertEquals("an id matches only from its start", emptyList<String>(), ids("2b3c"))
    }
}
