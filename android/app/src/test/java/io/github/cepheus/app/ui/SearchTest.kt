package io.github.cepheus.app.ui

import io.github.cepheus.app.protocol.SearchHit
import io.github.cepheus.app.protocol.SearchSnippet
import io.github.cepheus.app.protocol.ServerMessage
import io.github.cepheus.app.protocol.Session
import io.github.cepheus.app.protocol.Status
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** The session search's results, as the drawer and the home list show them (`Search.kt`). */
class SearchTest {
    private fun session(id: Long, tool: String, title: String, branch: String? = null) = Session(
        id = id, tool = tool, title = title, status = Status.IDLE, detail = "", alive = true, branch = branch,
        worktree = null, task = null, review = false, createdAt = 0, statusSince = 0,
    )

    private val timestamps = session(1, "codex", "Add timestamps to messages", "agent/20261102-cepheus-timestamps")
    private val apk = session(2, "claude", "Changes in new APK")
    private val deploy = session(3, "claude", "Deploy session check")
    private val board = session(4, "antigravity", "Draw the board")
    private val sessions = listOf(timestamps, apk, deploy, board)

    private fun hit(id: Long, mine: Int, vararg snippets: SearchSnippet) = SearchHit(id, snippets.size, mine, snippets.toList())

    private fun reply(q: String, vararg hits: SearchHit) = ServerMessage.Search(1, q, hits.toList())

    @Test
    fun aSearchIsItsLowercasedWordsOnceEach() {
        assertEquals(listOf("agy", "agents"), searchWords("  AGY agents agy "))
        assertEquals(emptyList<String>(), searchWords("   "))
    }

    @Test
    fun titlesBranchesAndProgramsMatchEveryWord() {
        assertTrue(titleMatches(timestamps, searchWords("TIMESTAMPS codex")))
        assertTrue("the branch counts", titleMatches(timestamps, searchWords("cepheus-time")))
        assertFalse(titleMatches(timestamps, searchWords("timestamps claude")))
        assertTrue("Antigravity answers to its command name", titleMatches(board, searchWords("agy")))
        assertFalse(titleMatches(apk, searchWords("agy")))
        assertFalse("no words find nothing", titleMatches(apk, emptyList()))
    }






    @Test
    fun theOwnersOwnWordsAndTitlesComeFirst() {
        val found = searchResults(
            sessions,
            "agy",
            reply(
                "agy",
                hit(1, 0, SearchSnippet("assistant", "agy is mentioned", null)),
                hit(3, 2, SearchSnippet("user", "make the deck start agy agents", 5), SearchSnippet("user", "agy", 4)),
            ),
        )
        assertEquals(listOf(3L, 4L, 1L), found.map { it.session.id })
        assertEquals("numbered as the deck numbers them", listOf(3, 4, 1), found.map { it.number })
        assertEquals(listOf(true, false), listOf(found[1].byTitle, found[1].hit != null))
    }

    @Test
    fun anAnswerToOtherWordsIsNotShown() {
        val stale = reply("ag", hit(2, 1, SearchSnippet("user", "ag", null)))
        assertEquals("only the titles until the deck answers", listOf(4L), searchResults(sessions, "agy", stale).map { it.session.id })
        assertEquals(
            "the same words in another case or spacing are the same search",
            listOf(2L, 4L),
            searchResults(sessions, " AGY ", reply("agy", hit(2, 1, SearchSnippet("user", "agy", null)))).map { it.session.id },
        )
        assertEquals(emptyList<SearchResult>(), searchResults(sessions, " ", stale))
    }

    @Test
    fun matchesArePickedOutIgnoringCaseAndMergedWhereTheyMeet() {
        assertEquals(listOf(6..8, 18..20), matchRanges("start AGY or then agy", listOf("agy")))
        assertEquals("touching words make one range", listOf(0..9), matchRanges("agentsdeck", listOf("agents", "deck")))
        assertEquals("overlaps merge", listOf(0..4), matchRanges("agyag", listOf("agya", "yag")))
        assertEquals(emptyList<IntRange>(), matchRanges("nothing here", listOf("agy")))
    }

    @Test
    fun aMatchingMessageSaysWhoWroteIt() {
        assertEquals("You", snippetWho(SearchSnippet("user", "", null), "codex"))
        assertEquals("Codex", snippetWho(SearchSnippet("assistant", "", null), "codex"))
        assertEquals("Screen", snippetWho(SearchSnippet("screen", "", null), "antigravity"))
    }
}
