package io.github.cepheus.app.ui

import io.github.cepheus.app.protocol.Session
import io.github.cepheus.app.protocol.StartHost
import io.github.cepheus.app.protocol.Status
import io.github.cepheus.app.protocol.OutsideAgent
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.yield
import org.junit.Assert.*
import org.junit.Test

class ToolFormsTest {
    private fun session(id: Long, status: Status, tool: String = "claude", alive: Boolean = true, detail: String = "") =
        Session(id, tool, "test", status, detail, alive, null, null, null, false, 0, 0)

    @Test fun broadcastDefaultsExcludeBlockedEndedAndShellSessions() {
        val sessions = listOf(
            session(1, Status.WORKING), session(2, Status.NEEDS_YOU), session(3, Status.IDLE, "shell"),
            session(4, Status.ENDED, alive = false), session(5, Status.NEEDS_YOU, detail = "queued question"),
        )
        assertEquals(listOf(1L, 5L), ToolForms.targets(sessions))
    }

    @Test fun newSessionKeepsAnExplicitMachineInsteadOfTheLastOne() {
        val local = StartHost(null, "This machine")
        val remote = StartHost("remote", "Remote")
        val hosts = listOf(local, remote)
        assertNull("A local worktree or local chat must stay local",
            ToolForms.initialStartHost(hosts, local, remote.name))
        assertEquals(remote.name, ToolForms.initialStartHost(hosts, remote, null))
        assertEquals("offline", ToolForms.initialStartHost(hosts, StartHost("offline", "Offline"), remote.name))
        assertEquals("Home remembers the last online machine", remote.name,
            ToolForms.initialStartHost(hosts, null, remote.name))
        assertNull("Home does not select an unavailable remembered machine",
            ToolForms.initialStartHost(hosts, null, "offline"))
    }

    @Test fun terminalArgumentsStayLiteral() {
        val args = ToolForms.terminal(null, "", "test", "/bin/printf\n%s\n$(literal); quoted words", false).getJSONArray("command")
        assertEquals(3, args.length())
        assertEquals("$(literal); quoted words", args.getString(2))
    }

    @Test fun keyboardEditsBecomeShellKeys() {
        val mark = TerminalInput.empty().text
        fun field(text: String) = TextFieldValue(mark + text, TextRange(mark.length + text.length))
        fun text(value: String) = TerminalInput.Send.Text(value)
        fun key(value: TerminalKey) = TerminalInput.Send.Key(value)

        assertEquals(listOf(text("s")), TerminalInput.edit(TerminalInput.empty(), field("s")).second)
        // Backspace on an empty field deletes the marker: one Backspace for the shell's own line.
        val (reset, back) = TerminalInput.edit(TerminalInput.empty(), TextFieldValue(""))
        assertEquals(listOf(key(TerminalKey.BACKSPACE)), back)
        assertEquals(TerminalInput.empty(), reset)
        // Enter sends the key and starts a fresh field.
        val (fresh, run) = TerminalInput.edit(field("ls"), field("ls -la\n"))
        assertEquals(listOf(text(" -la"), key(TerminalKey.ENTER)), run)
        assertEquals(TerminalInput.empty(), fresh)
        // Autocorrect rewrites a word: erase only what changed, then type the rest.
        assertEquals(
            List(3) { key(TerminalKey.BACKSPACE) } + text("tus "),
            TerminalInput.edit(field("git stauts"), field("git status ")).second,
        )
        assertEquals(
            listOf(text("echo a"), key(TerminalKey.ENTER), text("echo b")),
            TerminalInput.edit(TerminalInput.empty(), field("echo a\necho b")).second,
        )
        // Moving the cursor changes nothing on the shell.
        assertEquals(emptyList<TerminalInput.Send>(), TerminalInput.edit(field("ls"), field("ls").copy(selection = TextRange(1))).second)
    }

    @Test fun moveInKeepsItsResultWhenTheOutsideRowDisappears() = runBlocking {
        val reply = CompletableDeferred<Result<Long>>()
        var opened: Long? = null
        val outside = OutsideAgent(55, "claude", "outside", false, "/fixture", null, null, null, 0)
        val rows = mutableListOf(outside)
        val request = MoveInRequest(this, { reply.await() }, { opened = it })
        request.select(rows.single())
        request.confirm()
        yield()
        rows.clear()
        assertTrue(rows.isEmpty())
        assertTrue(request.busy)
        reply.complete(Result.success(84))
        yield()
        assertEquals(84L, opened)
        assertNull(request.agent)
        assertFalse(request.busy)
    }

    @Test fun moveInRetainsAVisibleErrorAfterItsSourceRowDisappears() = runBlocking {
        val reply = CompletableDeferred<Result<Long>>()
        val request = MoveInRequest(this, { reply.await() }, { fail("An unsuccessful handoff cannot open a session") })
        request.select(OutsideAgent(55, "claude", "outside", false, "/fixture", null, null, null, 0))
        request.confirm()
        yield()
        reply.complete(Result.failure(IllegalArgumentException("handoff failed")))
        yield()
        assertEquals("handoff failed", request.error)
        assertEquals(55L, request.agent?.pid)
        assertFalse(request.busy)
    }
}
