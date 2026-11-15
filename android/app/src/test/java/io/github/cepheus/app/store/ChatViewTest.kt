package io.github.cepheus.app.store

import io.github.cepheus.app.ui.chatRows
import io.github.cepheus.app.protocol.AsyncQuestion
import io.github.cepheus.app.protocol.Item
import io.github.cepheus.app.protocol.Protocol
import org.json.JSONArray
import io.github.cepheus.app.protocol.ServerMessage
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test

class ChatViewTest {
    private fun user(n: Int) = Item.User("m$n", null)

    private fun page(start: Int, count: Int, total: Int, generation: Long = 1) =
        ServerMessage.Chat(1, 7, start, total, generation, "log", (start until start + count).map(::user), "opus", "high")

    private fun items(start: Int, count: Int, total: Int, reset: Boolean = false, generation: Long? = 1) =
        ServerMessage.Items(7, reset, start, total, generation, (start until start + count).map(::user), null, null)

    private fun retainedItems(start: Int, count: Int, total: Int, floor: Int, reset: Boolean = false) =
        Protocol.parse(JSONObject()
            .put("t", "items").put("id", 7).put("generation", 1)
            .put("start", start).put("total", total).put("reset", reset)
            .put("retained_from", floor)
            .put("items", JSONArray().apply {
                (start until start + count).forEach { put(Protocol.itemJson(user(it))) }
            }).toString()) as ServerMessage.Items

    @Test
    fun aLongLivedChatReleasesItemsTheGatewayNoLongerRetains() {
        var view = ChatView(7).withPage(page(0, 60, 60))
        for (start in 60 until 6_000 step 60) {
            val total = start + 60
            view = view.withItems(retainedItems(start, 60, total, (total - 3_000).coerceAtLeast(0)))
        }
        assertEquals(3_000, view.start)
        assertEquals((3_000 until 6_000).map(::user), view.items)
        assertEquals(6_000, view.total)
        assertFalse(view.hasOlder)
    }

    @Test
    fun retentionKeepsAbsoluteReadingPositionsAndRejectsExpiredOlderPages() {
        var view = ChatView(7).withPage(page(3_000, 60, 3_060))
            .withOlder(page(2_940, 60, 3_060))
        view = view.withItems(retainedItems(3_060, 60, 3_120, 120))
        assertEquals(2_940, view.start)
        assertEquals(user(3_000), view.items[3_000 - view.start])
        view = view.withItems(retainedItems(3_120, 60, 3_180, 2_970))
        assertEquals(2_970, view.start)
        assertEquals(user(3_000), view.items[3_000 - view.start])
        val late = view.copy(loadingOlder = true).withOlder(page(2_910, 60, 3_180))
        assertEquals(view.items, late.items)
        assertEquals(view.start, late.start)
        assertFalse(late.hasOlder)
        assertFalse(late.loadingOlder)
    }

    @Test
    fun aResetAcrossAMissedHistoryGapKeepsTheNewAbsoluteIndexes() {
        val view = ChatView(7).withPage(page(40, 10, 50))
            .withItems(retainedItems(4_000, 60, 4_060, 1_060, reset = true))
        assertEquals(4_000, view.start)
        assertEquals((4_000 until 4_060).map(::user), view.items)
        assertEquals(4_060, view.total)
    }

    @Test
    fun rawTerminalSnapshotsRemainAvailableAfterOfflineRoundTrip() {
        val screen = ChatView(7, source = "screen", total = 1, items = listOf(Item.Screen("PTY output")))
        assertEquals(screen, ChatView.fromJson(JSONObject(screen.toJson().toString())))
    }

    @Test
    fun newItemsAppend() {
        val view = ChatView(7).withPage(page(40, 10, 50)).withItems(items(50, 2, 52))
        assertEquals(40, view.start)
        assertEquals((40 until 52).map(::user), view.items)
        assertEquals(52, view.total)
        assertEquals("opus", view.model)
    }

    @Test
    fun aResetReplacesFromItsStart() {
        val view = ChatView(7).withPage(page(40, 10, 50)).withItems(items(45, 3, 48, reset = true))
        assertEquals(40, view.start)
        assertEquals((40 until 48).map(::user), view.items)
        assertEquals(1L, view.generation)
    }

    /** Another generation is another chat (`/clear`): none of the old one stays. */
    @Test
    fun aResetFromAnotherGenerationReplacesEverything() {
        val view = ChatView(7).withPage(page(0, 10, 10)).withItems(items(5, 3, 8, reset = true, generation = 2))
        assertEquals(5, view.start)
        assertEquals((5 until 8).map(::user), view.items)
        assertEquals(2L, view.generation)

        val cleared = ChatView(7).withPage(page(0, 10, 10)).withItems(items(0, 0, 0, reset = true, generation = 2))
        assertEquals(emptyList<Item>(), cleared.items)
        assertEquals(0, cleared.total)
    }

    @Test
    fun aResetBeforeWhatIsHeldReplacesEverything() {
        val view = ChatView(7).withPage(page(40, 10, 50)).withItems(items(0, 1, 1, reset = true, generation = null))
        assertEquals(0, view.start)
        assertEquals(listOf(user(0)), view.items)
        assertEquals(1L, view.generation)
    }

    @Test
    fun olderPagesPrependOnlyWithinTheSameGeneration() {
        val view = ChatView(7).withPage(page(40, 10, 50)).copy(loadingOlder = true)
        val older = view.withOlder(page(20, 20, 50))
        assertEquals(20, older.start)
        assertEquals((20 until 50).map(::user), older.items)
        assertFalse(older.loadingOlder)

        val stale = view.withOlder(page(20, 20, 50, generation = 2))
        assertEquals(view.items, stale.items)
        assertFalse(stale.loadingOlder)
    }

    @Test
    fun theOfflineCopyRoundTrips() {
        val view = ChatView(7).withPage(page(40, 3, 50))
        val back = ChatView.fromJson(JSONObject(view.toJson().toString()))
        assertEquals(view.copy(live = false), back)
    }

    @Test
    fun aLinkedConversationReplacesTheFallbackScreenAndRestoresLogSource() {
        val screen = ChatView(7, source = "screen", items = listOf(Item.Screen("Working")), total = 1)
        val linked = screen.withItems(items(0, 2, 2, reset = true, generation = 1))
        assertEquals("log", linked.source)
        assertEquals(listOf(user(0), user(1)), linked.items)
        val fallback = linked.withItems(ServerMessage.Items(7, true, 0, 1, 2, listOf(Item.Screen("Live output")), null, null))
        assertEquals("screen", fallback.source)
        assertEquals(listOf(Item.Screen("Live output")), fallback.items)
        val relinked = fallback.withItems(items(0, 0, 0, reset = true, generation = 3))
        assertEquals("log", relinked.source)
        assertEquals(emptyList<Item>(), relinked.items)
    }
    @Test
    fun unfinishedRepliesRenderWithoutChangingMessageIndexesOrOfflineHistory() {
        val base = ChatView(7).withPage(page(0, 1, 1))
        var current = base
        for (text in listOf("First", "First second", "First second third.")) {
            val wire = JSONObject()
                .put("t", "items").put("id", 7).put("start", 1).put("total", 1)
                .put("generation", 1).put("stream", text)
            val update = Protocol.parse(wire.toString()) as ServerMessage.Items
            current = current.withItems(update)
            assertEquals(1, current.total)
            assertEquals(base.items, current.items)
            assertEquals(listOf("m0", text), chatRows(current).map { row ->
                when (val item = row.item) {
                    is Item.User -> item.text
                    is Item.Assistant -> item.text
                    else -> error("unexpected row")
                }
            })
            assertEquals(null, ChatView.fromJson(current.toJson()).stream)
        }
        val saved = current.withItems(ServerMessage.Items(
            7, false, 1, 2, 1, listOf(Item.Assistant("First second third.", null)), null, null,
        ))
        assertEquals(null, saved.stream)
        assertEquals(2, chatRows(saved).size)
        assertEquals(null, current.withPage(page(0, 0, 0, generation = 2)).stream)
    }


    @Test
    fun unansweredQuestionCardsStayIndependentOfHistoryPagesAndOfflineCopies() {
        val question = AsyncQuestion("old:1", "An old question?", listOf("Yes", "No"))
        val snapshot = page(4_000, 2, 4_002).copy(questions = listOf(question))
        val view = ChatView(7).withPage(snapshot)
        assertEquals(listOf(question), view.pendingQuestions)
        assertEquals(4_000, view.start)
        assertEquals(4_002, view.total)
        assertEquals(2, view.items.size)
        val offline = ChatView.fromJson(JSONObject(view.toJson().toString()))
        assertEquals(listOf(question), offline.pendingQuestions)
        val advanced = offline.withItems(items(4_002, 1, 4_003))
            .withOlder(page(3_998, 2, 4_003).copy(questions = emptyList()))
        assertEquals("an older page cannot overwrite the current snapshot", listOf(question), advanced.pendingQuestions)
        val answered = advanced.withItems(items(4_003, 0, 4_003).copy(questions = emptyList()))
        assertEquals(emptyList<AsyncQuestion>(), answered.pendingQuestions)
        val reset = view.withItems(items(0, 0, 0, reset = true, generation = 2))
        assertEquals(emptyList<AsyncQuestion>(), reset.pendingQuestions)
        val screen = view.withItems(ServerMessage.Items(7, true, 0, 1, 1,
            listOf(Item.Screen("Waiting for transcript")), null, null))
        assertEquals(emptyList<AsyncQuestion>(), screen.pendingQuestions)
    }
    @Test
    fun questionSnapshotsAndTransientRepliesShareRetentionWithoutSharingOfflineState() {
        val question = AsyncQuestion("old:0", "Still waiting?", listOf("Yes", "No"))
        val opened = ChatView(7).withPage(page(4_000, 3, 4_003)
            .copy(retainedFrom = 3_000, questions = listOf(question)))
        val current = opened.withItems(items(4_003, 0, 4_003)
            .copy(retainedFrom = 4_001, stream = "Unfinished reply"))
        assertEquals(4_001, current.start)
        assertEquals((4_001 until 4_003).map(::user), current.items)
        assertEquals(listOf(question), current.pendingQuestions)
        assertEquals("Unfinished reply", current.stream)
        val offline = ChatView.fromJson(JSONObject(current.toJson().toString()))
        assertEquals(listOf(question), offline.pendingQuestions)
        assertEquals(4_001, offline.retainedFrom)
        assertEquals(null, offline.stream)
        val older = current.withOlder(page(3_999, 2, 4_003)
            .copy(retainedFrom = 3_999, questions = emptyList()))
        assertEquals(current.items, older.items)
        assertEquals(4_001, older.retainedFrom)
        assertEquals(listOf(question), older.pendingQuestions)
        assertEquals("Unfinished reply", older.stream)
        val answered = older.withItems(items(4_003, 0, 4_003)
            .copy(retainedFrom = 4_001, questions = emptyList()))
        assertEquals(emptyList<AsyncQuestion>(), answered.pendingQuestions)
        assertEquals(current.items, answered.items)
        assertEquals(null, answered.stream)
    }

}
