package io.github.cepheus.app.protocol

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class JobsTest {
    private fun info(status: String = "finished") = JSONObject().put("id", 17).put("title", "Fixture")
        .put("status", status).put("created_at", 1794000000L).put("reason", "")

    @Test fun literalArgumentVectorAndPlacementNeverBecomeShellOrRoutingFields() {
        val vector = JSONArray().put("/tool").put("two words").put("$(touch sentinel); *").put("").put("line1\nline2").toString()
        val request = JobContracts.start("native-one", "Fixture", vector, "/work/a b", "worker", "codex")
        val wire = JSONObject(Requests.control(9, "fleet", request))
        assertEquals("job-start", wire.getString("operation"))
        val args = wire.getJSONObject("args")
        assertEquals(vector, args.getJSONArray("argv").toString())
        assertEquals("worker", args.getString("placement"))
        assertEquals("/work/a b", args.getString("cwd"))
        assertFalse(args.has("host"))
        assertFalse(args.has("command"))
        for (invalid in listOf("echo one", "[]", "[1]", "[null]", JSONArray().put("").toString(), JSONArray().put("tool").put("\u0000").toString())) {
            assertThrows(IllegalArgumentException::class.java) { JobContracts.start("native-one", "Fixture", invalid, "/work", "local", null) }
        }
    }

    @Test fun terminalEvidenceKeepsZeroSignalAndMissingExitDistinct() {
        val done = JobContracts.info(info().put("exit_code", 0))
        assertTrue(done.terminal)
        assertFalse(done.canStop)
        assertEquals("Exit 0", done.outcome)
        val signal = JobContracts.info(info("failed").put("signal", 15).put("reason", "stopped"))
        assertEquals("Signal 15", signal.outcome)
        assertEquals("stopped", signal.reason)
        assertTrue(signal.canStop)
        val unknown = JobContracts.info(info("failed").put("reason", "supervisor exited"))
        assertEquals("Exit status unknown", unknown.outcome)
        assertTrue(unknown.canStop)
        assertFalse(JobContracts.info(info("failed").put("exit_code", 23)).canStop)
        assertTrue(JobContracts.info(info("working")).canStop)
        assertTrue(JobContracts.info(info("starting").put("reason", "quota resets later")).canStop)
        assertFalse(JobContracts.info(info("future-state")).canStop)
        for (id in listOf<Any>("17", 1.5, -1, 0)) {
            assertThrows(IllegalArgumentException::class.java) { JobContracts.info(info().put("id", id)) }
        }
        assertThrows(IllegalArgumentException::class.java) { JobContracts.info(info().put("exit_code", 1.5)) }
    }

    @Test fun jobsNeedProtocolThirteenFacilityAndNeverFallbackToSessionControls() {
        assertFalse(JobContracts.supported(JSONObject().put("protocol", 12).put("facilities", JSONArray().put("jobs"))))
        assertFalse(JobContracts.supported(JSONObject().put("protocol", 13).put("facilities", JSONArray().put("jobs"))))
        assertFalse(JobContracts.supported(JSONObject().put("protocol", 14).put("facilities", JSONArray().put("fleet"))))
        assertTrue(JobContracts.supported(JSONObject().put("protocol", 14).put("facilities", JSONArray().put("jobs"))))
        val target = JobContracts.target("job-stop", 9876543210L)
        assertEquals("job-stop", target.getString("operation"))
        assertEquals(setOf("job"), target.getJSONObject("args").keys().asSequence().toSet())
        assertEquals(9876543210L, target.getJSONObject("args").getLong("job"))
        assertThrows(IllegalArgumentException::class.java) { JobContracts.request("job-stop", JSONObject().put("host", "worker")) }
    }

    @Test fun logPagesRejectStaleCursorAndAnotherJobAndBoundRetainedOutput() {
        val first = JobContracts.Page(17, 7, "first", listOf(Item.Assistant("hello", null)), false)
        val state = JobContracts.Output(17).append(0, first)
        assertEquals(7L, state.offset)
        assertEquals("first", state.text)
        assertFalse(state.eof)
        val sameCursor = state.append(7, first.copy(text = "", items = listOf(Item.Assistant("latest snapshot", null))))
        assertEquals("latest snapshot", (sameCursor.items.single() as Item.Assistant).text)
        assertThrows(IllegalArgumentException::class.java) { state.append(0, first) }
        assertThrows(IllegalArgumentException::class.java) { state.append(7, first.copy(job = 18, offset = 9)) }
        assertThrows(IllegalArgumentException::class.java) { state.append(7, first.copy(offset = 6)) }
        val end = state.append(7, JobContracts.Page(17, 200000, "x".repeat(JobContracts.MAX_TEXT), emptyList(), true))
        assertEquals(JobContracts.MAX_TEXT, end.text.length)
        assertTrue(end.trimmed)
        assertTrue(end.eof)
        assertThrows(IllegalArgumentException::class.java) { JobContracts.page(JSONObject().put("job", 17).put("offset", "7").put("text", "").put("eof", true)) }
    }

    @Test fun sharedStructuredItemsKeepResultsButCannotAddressInteractiveSessionFeatures() {
        val items = JSONArray().put(JSONObject().put("k", "user").put("text", "request").put("images", JSONArray().put("0.0")))
            .put(JSONObject().put("k", "tool").put("call", "a").put("name", "Bash").put("summary", "check").put("input", "{}"))
            .put(JSONObject().put("k", "result").put("call", "a").put("text", "complete").put("error", false))
            .put(JSONObject().put("k", "assistant").put("text", "Choose one").put("questions", JSONArray().put(JSONObject().put("id", "q").put("title", "Choose one").put("options", JSONArray().put("A").put("B")))))
        val page = JobContracts.page(JSONObject().put("job", 17).put("offset", 400).put("text", "records").put("items", items).put("eof", true))
        val output = JobContracts.Output(17).append(0, page)
        assertEquals("a", output.items.filterIsInstance<Item.Result>().single().call)
        assertTrue(output.items.filterIsInstance<Item.User>().single().images.isEmpty())
        assertTrue(output.items.filterIsInstance<Item.Assistant>().single().questions.isEmpty())
        assertTrue(output.items.filterIsInstance<Item.Notice>().any { "Choose one" in it.text && "A" in it.text && "B" in it.text })
        assertTrue(output.items.any { it is Item.Notice && "attachment" in it.text })
        assertEquals("complete", output.items.filterIsInstance<Item.Result>().single().text)
    }
}
