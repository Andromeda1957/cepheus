package io.github.cepheus.app.ui

import io.github.cepheus.app.protocol.Item
import io.github.cepheus.app.protocol.Lines
import io.github.cepheus.app.store.ChatView
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test







class ToolGroupsTest {
    private fun tool(call: String, name: String, summary: String = "", label: String? = null, diff: Lines? = null) =
        Item.Tool(call, name, summary, "{}", label, diff)

    private fun done(call: String, error: Boolean = false) = Item.Result(call, "out", error)

    private fun view(vararg items: Item) =
        ChatView(7, start = 0, total = items.size, generation = 1, source = "log", items = items.toList(), live = true)

    private fun shape(entries: List<ChatEntry>): List<String> = entries.map { entry ->
        when (entry) {
            is ChatEntry.One -> entry.row.item.javaClass.simpleName
            is ChatEntry.Tools -> "Tools(${entry.rows.size})"
        }
    }

    @Test
    fun replyActionsFollowCompletedRepliesInsteadOfProgressAndToolOutput() {
        val rows = chatRows(view(
            Item.User("start", null),
            Item.Assistant("Checking.", null),
            tool("1", "Bash"), done("1"),
            Item.Assistant("First paragraph.", null),
            Item.Assistant("Second paragraph.", null),
            Item.User("next", null),
            Item.Assistant("Current progress.", null),
        ))
        val running = completedReplies(rows, settled = false)
        assertEquals(setOf("5"), running.keys)
        assertEquals("First paragraph.\n\nSecond paragraph.", running.getValue("5").text())
        assertEquals(setOf("5", "7"), completedReplies(rows, settled = true).keys)
        val interrupted = rows + ChatRow("8", Item.Notice("Interrupted"), null)
        assertEquals(setOf("5"), completedReplies(interrupted, settled = true).keys)
        val terminalFallback = rows + ChatRow("8", Item.Screen("raw PTY output"), null)
        assertEquals(setOf("5"), completedReplies(terminalFallback, settled = true).keys)
        val streamed = rows + ChatRow("stream", Item.Assistant("Unfinished reply", null), null)
        assertEquals(setOf("5"), completedReplies(streamed, settled = true).keys)
        assertEquals(setOf("5"), completedReplies(streamed, settled = false).keys)
        val paged = rows.map { it.copy(key = (it.key.toInt() + 400).toString()) }
        assertEquals(setOf("405"), completedReplies(paged, settled = false).keys)
    }

    @Test
    fun longReadAloudRepliesKeepAllTextAndWholeUnicodeCharacters() {
        val text = ("A sentence with an emoji 😀 and a newline.\n").repeat(250)
        val parts = speechParts(text, 37)
        assertEquals(text, parts.joinToString(""))
        val unbroken = "x".repeat(100_000)
        assertEquals(unbroken, speechParts(unbroken, 37).joinToString(""))
        assertEquals(true, parts.all { it.length <= 37 && !it.last().isHighSurrogate() && !it.first().isLowSurrogate() })
    }

    @Test
    fun aRunOfWorkBetweenMessagesFoldsFromItsFirstCall() {
        val entries = chatEntries(chatRows(view(
            Item.User("fix it", null),
            Item.Thinking("Let me look."),
            tool("1", "Bash"), done("1"),
            Item.Thinking("Now the file."),
            tool("2", "Read"), done("2"),
            Item.Assistant("Found it.", null),
            Item.Thinking("Only thinking here."),
            Item.Assistant("Done.", null),
        )))
        assertEquals(
            listOf("User", "Thinking", "Tools(3)", "Assistant", "Thinking", "Assistant"),
            shape(entries),
        )
        val group = entries[2] as ChatEntry.Tools
        assertEquals(listOf("1", "2"), group.calls.map { it.first.call })
        assertEquals(listOf(false, false), group.calls.map { it.second!!.error })
        // The group keeps its first call's place as it grows.
        assertEquals("tools-2", group.key)
    }

    @Test
    fun severalCallsAreCountedByKindInTheOrderTheyCame() {
        val calls = listOf(
            tool("w", "Write", "/r/new.kt") to done("w"),
            tool("a", "Bash") to done("a"),
            tool("b", "Bash") to done("b", error = true),
            tool("c", "exec_command") to done("c"),
            tool("d", "Bash") to null,
        )
        assertEquals("Created a file, ran 4 commands (1 failed)", toolGroupTitle(calls))
        assertEquals(
            "Read 2 files, used a tool",
            toolGroupTitle(listOf(tool("r", "Read") to done("r"), tool("v", "view_file") to done("v"), tool("t", "TodoWrite") to done("t"))),
        )
        assertEquals(
            "Ran 6 commands",
            toolGroupTitle((1..6).map { tool("$it", "Bash") to done("$it") }),
        )
        assertEquals(
            "Edited 2 files, searched for a pattern, fetched a page, searched the web, ran an agent",
            toolGroupTitle(listOf(
                tool("e", "Edit") to done("e"), tool("p", "apply_patch") to done("p"), tool("g", "Grep") to done("g"),
                tool("f", "WebFetch") to done("f"), tool("s", "WebSearch") to done("s"), tool("t", "Task") to done("t"),
            )),
        )
    }

    @Test
    fun theLinesEditsChangeAddUp() {
        assertEquals(
            Lines(155, 0),
            toolGroupDiff(listOf(tool("w", "Write", diff = Lines(150, 0)), tool("b", "Bash"), tool("e", "Edit", diff = Lines(5, 0)))),
        )
        assertNull("no edit, no count", toolGroupDiff(listOf(tool("b", "Bash"))))
    }
}
