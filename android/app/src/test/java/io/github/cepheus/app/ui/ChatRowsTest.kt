package io.github.cepheus.app.ui

import io.github.cepheus.app.protocol.Item
import io.github.cepheus.app.protocol.ServerMessage
import io.github.cepheus.app.store.ChatView
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Test

class ChatRowsTest {
    private val screen = Item.Screen("raw PTY: › Ask Codex anything\nmodel · shortcuts")

    @Test fun unresolvedSendsStayBetweenEarlierAndLaterOutputAndReplaceTheirCanonicalEcho() {
        val messages = (1L..3L).map { n ->
            io.github.cepheus.app.store.Outgoing(n, "correction $n", emptyList(), emptyList(),
                4, n.toInt(), n * 1000, false, false, clientId = "send-$n", correlated = true)
        }
        val view = ChatView(7, total = 4, generation = 4, items = (0L..3L).map { Item.Assistant("update $it", it * 1000) })
        val rows = chatRows(view, messages)
        assertEquals(listOf("0", "out-1", "1", "out-2", "2", "out-3", "3"), rows.map { it.key })
        val echoed = view.copy(items = view.items + Item.User("correction 1", 4000, clientId = "send-1"), total = 5)
        assertEquals(1, chatRows(echoed, messages).count { (it.item as? Item.User)?.text == "correction 1" })
        assertEquals(5, echoed.total)
        // First prompts, reconnects and retained pages can mix history generations.
        // An older unresolved send must never fall below a later send's anchor.
        for (page in listOf(
            ChatView(7, total = 1, generation = 4, source = "screen", items = listOf(screen)),
            ChatView(7, total = 3, generation = 4, items = List(3) { Item.Assistant("untimed $it", null) }),
            ChatView(7, start = 20, total = 24, generation = 4, items = List(4) { Item.Assistant("retained $it", null) }),
        )) {
            val earlier = messages[0].copy(generation = 0)
            val later = messages[1].copy(after = page.start)
            assertEquals(listOf("out-1", "out-2"), chatRows(page, listOf(earlier, later))
                .filter { it.outgoing != null }.map { it.key })
        }
        assertEquals(listOf("0", "out-1", "1", "out-2", "2", "out-3", "3", "stream"),
            chatRows(view.copy(stream = "live response"), messages).map { it.key })
    }

    @Test
    fun anInitialTerminalPageHasNoConversationRows() {
        val view = ChatView(7).withPage(
            ServerMessage.Chat(1, 7, 0, 1, 1, "screen", listOf(screen), null, null),
        )
        assertEquals(emptyList<ChatRow>(), chatRows(view))
        // Dedicated terminals still need the original transport and cache.
        assertEquals(listOf(screen), view.items)
    }

    @Test
    fun aLiveFallbackAndItsOfflineCopyHaveNoConversationRows() {
        val native = ChatView(7, generation = 1, total = 1, items = listOf(Item.User("hello", null)))
        val fallback = native.withItems(
            ServerMessage.Items(7, true, 0, 1, 2, listOf(screen), null, null),
        )
        assertEquals(emptyList<ChatRow>(), chatRows(fallback))
        val cached = ChatView.fromJson(JSONObject(fallback.toJson().toString()))
        assertEquals(listOf(screen), cached.items)
        assertEquals(emptyList<ChatRow>(), chatRows(cached))
        val relinked = fallback.withItems(
            ServerMessage.Items(7, true, 0, 1, 3, listOf(Item.Assistant("Native reply", null)), null, null),
        )
        assertEquals(listOf(Item.Assistant("Native reply", null)), chatRows(relinked).map { it.item })
    }

    @Test
    fun mixedLegacyItemsKeepWirePositionsAndNativeCommandResults() {
        val call = Item.Tool("call", "exec_command", "printf hello", "{}", null, null)
        val result = Item.Result("call", "native command output: › hello", false)
        val reply = Item.Assistant("```sh\nprintf hello\n```", null)
        val items = listOf(
            screen, Item.User("run it", null), call, screen, result, reply,
            Item.Unknown("future"), Item.Result("orphan", "standalone result", false),
        )
        // Source labels and provider metadata are not a presentation boundary.
        for (source in listOf("log", "screen", "none", "future")) {
            val view = ChatView(7, start = 20, total = 28, source = source, items = items)
            val cached = ChatView.fromJson(JSONObject(view.toJson().toString()))
            val rows = chatRows(cached)
            assertEquals(source, listOf("21", "22", "25", "27"), rows.map { it.key })
            assertEquals(result, rows[1].result)
            assertEquals(reply, rows[2].item)
            assertEquals(4, chatEntries(rows).size)
            assertEquals(items, cached.items)
            assertEquals(28, cached.total)
        }
    }
}
