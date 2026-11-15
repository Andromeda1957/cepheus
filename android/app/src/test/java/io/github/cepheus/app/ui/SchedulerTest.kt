package io.github.cepheus.app.ui

import io.github.cepheus.app.protocol.Requests
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class SchedulerTest {
    private fun snapshot(available: Int = 4096, total: Int = 16384, pressure: Any = 0) = JSONObject()
        .put("headroom", JSONObject().put("available_mib", available).put("total_mib", total).put("memory_pressure", pressure))
        .put("pools", JSONObject().put("heavy", 2))

    @Test fun rerunsWaitForKnownMemoryAndLeaveAuthoritativeClaudeHooksAlone() {
        val records = JSONArray().put(JSONObject().put("id", 1).put("session", 7).put("authoritative", false))
            .put(JSONObject().put("id", 2).put("session", 8).put("authoritative", true))
            .put(JSONObject().put("id", 3).put("session", 9))
        for (value in listOf(snapshot(available = 1024), snapshot(pressure = 5), snapshot(pressure = -1), snapshot(pressure = "unknown"), JSONObject())) {
            val parsed = Scheduler.snapshot(value.put("reruns", records))
            assertFalse(parsed.memoryFree)
            assertFalse(parsed.canRerun(parsed.reruns.first()))
        }
        val parsed = Scheduler.snapshot(snapshot().put("reruns", records))
        assertTrue(parsed.memoryFree)
        assertTrue(parsed.canRerun(parsed.reruns[0]))
        assertFalse(parsed.canRerun(parsed.reruns[1]))
        assertFalse("Missing authoritative state must not offer duplicate reruns", parsed.canRerun(parsed.reruns[2]))
        assertFalse("The 15% threshold also applies above 2 GiB", Scheduler.snapshot(snapshot(available = 4096, total = 65536)).memoryFree)
        assertFalse("Server policy wins over local default thresholds", Scheduler.snapshot(snapshot().put("memory_free", false)).memoryFree)
        assertTrue("A configured server threshold may differ", Scheduler.snapshot(snapshot(available = 1024).put("memory_free", true)).memoryFree)
    }

    @Test fun hundredWaitersKeepTheirOrderAndTargetsNeverSendPrivateText() {
        val waiters = JSONArray()
        repeat(100) { waiters.put(JSONObject().put("id", it + 1).put("pool", "heavy").put("label", "private-$it")) }
        val parsed = Scheduler.snapshot(snapshot().put("waiters", waiters))
        assertEquals((1..100).toList(), parsed.waiters.map { it.id })
        assertEquals(listOf("heavy" to 2), parsed.pools)
        val request = JSONObject(Requests.control(55, "fleet", Scheduler.target("slot-release", parsed.waiters[45], "worker")))
        assertEquals("fleet", request.getString("action"))
        assertEquals("slot-release", request.getString("operation"))
        assertEquals(46, request.getJSONObject("args").getInt("id"))
        assertEquals("worker", request.getJSONObject("args").getString("host"))
        assertEquals(setOf("id", "host"), request.getJSONObject("args").keys().asSequence().toSet())
        assertFalse(request.toString().contains("private"))
    }

    @Test fun malformedEntriesDoNotAcquireUsableActionTargets() {
        val values = JSONArray().put(JSONObject().put("id", JSONObject.NULL)).put(JSONObject())
            .put(JSONObject().put("id", -1)).put(JSONObject().put("id", 1.5)).put(JSONObject().put("id", "1"))
            .put(JSONObject().put("id", JSONObject()))
        val parsed = Scheduler.snapshot(snapshot().put("holders", values))
        assertTrue(parsed.holders.isEmpty())
    }

    @Test fun quotaPausesKeepKnownAndUnknownResetEvidenceWithoutActionTargets() {
        assertTrue(Scheduler.snapshot(snapshot()).paused.isEmpty())
        val records = JSONArray().put(JSONObject().put("session", 7).put("provider", "codex").put("reason", "usage limit").put("reset", 1793930400L))
            .put(JSONObject().put("session", 8).put("provider", "claude").put("reason", "usage limit").put("reset", JSONObject.NULL))
        val parsed = Scheduler.snapshot(snapshot().put("paused", records))
        assertEquals(listOf(7, 8), parsed.paused.map { it.getInt("session") })
        assertNotNull(Scheduler.reset(parsed.paused[0]))
        assertNull(Scheduler.reset(parsed.paused[1]))
        assertTrue(parsed.paused.none { it.has("id") })
        assertTrue(parsed.starts.isEmpty())
    }

    @Test fun queuedStartsExcludeDispatchReceiptsAndKeepInflightOwnership() {
        val records = JSONArray().put(JSONObject().put("id", 1).put("session", JSONObject.NULL).put("attempted_at", JSONObject.NULL).put("reason", "quota").put("reset", 1793930400L))
            .put(JSONObject().put("id", 2).put("session", 42).put("reason", "started"))
            .put(JSONObject().put("id", 3).put("session", JSONObject.NULL).put("attempted_at", 1793926800L).put("reason", "quota on peer").put("reset", 1793930400L))
        val parsed = Scheduler.snapshot(snapshot().put("starts", records))
        assertEquals(listOf(1, 3), parsed.starts.map { it.id })
        assertTrue(parsed.canCancelStart(parsed.starts[0]))
        assertFalse(parsed.canCancelStart(parsed.starts[1]))
        assertEquals("quota on peer", parsed.starts[1].value.getString("reason"))
        assertEquals(1793930400L, parsed.starts[1].value.getLong("reset"))
    }
}
