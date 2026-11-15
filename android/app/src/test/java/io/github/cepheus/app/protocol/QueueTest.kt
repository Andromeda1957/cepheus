package io.github.cepheus.app.protocol

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class QueueTest {
    private fun info(id: Long = 17, cancelable: Boolean = true) = JSONObject().put("id", id).put("title", "Fixture")
        .put("kind", "job").put("state", "held").put("priority", 5).put("created_at", 1794000000L).put("updated_at", 1794000001L)
        .put("reason", "waiting for headroom").put("dispatch", JSONObject.NULL).put("dependencies", JSONArray())
        .put("source", JSONObject.NULL).put("budget", JSONObject.NULL).put("cancelable", cancelable).put("placement", "auto")
    private fun spec(key: String = "native-one", dependencies: List<QueueContracts.Dependency> = emptyList(), budget: String = "") =
        QueueContracts.spec(key, "Fixture", "job", JSONArray().put("/tool").put("two words; $(no-shell) *").put("").toString(), "", null, "", "/work/a b", "auto", "5", dependencies, budget)

    @Test fun capabilityIsExplicitAndIndependentFromJobsFacility() {
        fun caps(protocol: Any, vararg facilities: String) = JSONObject().put("protocol", protocol).put("facilities", JSONArray(facilities.toList()))
        assertFalse(QueueContracts.supported(caps(13, "work-queue")))
        assertFalse(QueueContracts.supported(caps("14", "work-queue")))
        assertFalse(QueueContracts.supported(caps(14, "jobs")))
        assertTrue(QueueContracts.supported(caps(14, "work-queue")))
        assertFalse(QueueContracts.supported(caps(14, "work-queue"), "chores"))
        assertFalse(JobContracts.supported(caps(13, "jobs")))
        assertTrue(JobContracts.supported(caps(14, "jobs")))
        assertThrows(IllegalArgumentException::class.java) { QueueContracts.request("chore-claim") }
        assertThrows(IllegalArgumentException::class.java) { QueueContracts.request("queue-list", JSONObject().put("host", "worker")) }
        val wire = JSONObject(Requests.control(81, "fleet", QueueContracts.target("queue-cancel", 9876543210)))
        assertEquals(81L, wire.getLong("rid")); assertEquals("fleet", wire.getString("action"))
        assertEquals(9876543210L, wire.getJSONObject("args").getLong("id"))
    }

    @Test fun explicitExecutableSpecsPreserveLiteralArgumentsAndPrompt() {
        val job = spec()
        assertEquals("two words; $(no-shell) *", job.getJSONArray("argv").getString(1))
        assertEquals("", job.getJSONArray("argv").getString(2)); assertFalse(job.has("command")); assertFalse(job.has("prompt"))
        val prompt = "Implement exactly <this>\nthen report & stop"
        val session = QueueContracts.spec("phone-two", "Reference title only", "session", "[]", prompt, "codex", "model-one", "/work", "worker", "0", emptyList(), "")
        assertEquals(prompt, session.getString("prompt")); assertFalse(session.has("argv"))
        val source = QueueContracts.queueSource(QueueContracts.SourceRef("tasks", "same-id"), session).getJSONObject("args")
        assertEquals("tasks", source.getString("source")); assertEquals("same-id", source.getString("item"))
        assertEquals(prompt, source.getJSONObject("spec").getString("prompt"))
        assertThrows(IllegalArgumentException::class.java) { QueueContracts.spec("phone-three", "Title is not a prompt", "session", "[]", "", "codex", "", "/work", "local", "0", emptyList(), "") }
    }

    @Test fun typedDispatchAndStrictIdsPreventSessionJobConfusion() {
        val queue = QueueContracts.info(info().put("dispatch", JSONObject().put("kind", "job").put("id", 9000000001L)))
        assertEquals("job", queue.dispatch!!.kind); assertEquals(9000000001L, queue.dispatch!!.id)
        assertEquals("waiting for headroom", queue.reason); assertEquals(1794000001L, queue.updatedAt)
        for (id in listOf<Any>("17", 1.5, 0, -1)) assertThrows(IllegalArgumentException::class.java) { QueueContracts.info(info().put("id", id)) }
        assertThrows(IllegalArgumentException::class.java) { QueueContracts.info(info().put("dispatch", JSONObject().put("kind", "terminal").put("id", 17))) }
        assertThrows(IllegalArgumentException::class.java) { QueueContracts.info(info().put("cancelable", "true")) }
    }

    @Test fun requestedPauseIsNotEffectiveUntilExecutorAcknowledgment() {
        fun snapshot(enabled: Boolean = false) = JSONObject().put("enabled", enabled).put("items", JSONArray())
        val older = QueueContracts.listing(snapshot())
        assertFalse(older.pauseEffective); assertFalse(older.pauseConfirmed); assertTrue(older.pausePending.isEmpty())
        val pending = QueueContracts.listing(snapshot().put("pause_effective", false).put("pause_pending", JSONArray().put("Offline worker").put("This machine")))
        assertEquals(listOf("Offline worker", "This machine"), pending.pausePending); assertFalse(pending.pauseConfirmed)
        val acknowledged = QueueContracts.listing(snapshot().put("pause_effective", true).put("pause_pending", JSONArray()))
        assertTrue(acknowledged.pauseConfirmed)
        assertFalse(QueueContracts.listing(snapshot(true).put("pause_effective", true)).pauseConfirmed)
        assertFalse(QueueContracts.listing(snapshot().put("pause_effective", true).put("pause_pending", JSONArray().put("Unknown executor"))).pauseConfirmed)
        assertThrows(IllegalArgumentException::class.java) { QueueContracts.listing(snapshot().put("pause_effective", "true")) }
        assertThrows(IllegalArgumentException::class.java) { QueueContracts.listing(snapshot().put("pause_pending", JSONArray().put(12))) }
    }

    @Test fun reorderingContainsTheCompleteCancelableSetWithoutDispatchedIds() {
        val rows = listOf(QueueContracts.info(info(1)), QueueContracts.info(info(2, false)), QueueContracts.info(info(3)), QueueContracts.info(info(4)))
        val ids = QueueContracts.reorder(rows, 3, -1).getJSONObject("args").getJSONArray("ids")
        assertEquals("[3,1,4]", ids.toString())
        assertThrows(IllegalArgumentException::class.java) { QueueContracts.reorder(rows, 2, 1) }
        assertThrows(IllegalArgumentException::class.java) { QueueContracts.reorder(rows, 1, -1) }
    }

    @Test fun dependenciesRetainEveryAndConditionAndRejectAmbiguousReferences() {
        val deps = listOf(QueueContracts.Dependency(queue = 1), QueueContracts.Dependency(source = "repo-a", task = "same", condition = "ready"), QueueContracts.Dependency(source = "repo-b", task = "same", condition = "merged"))
        val decoded = QueueContracts.info(info().put("dependencies", spec(dependencies = deps).getJSONArray("dependencies")))
        assertEquals(deps, decoded.dependencies)
        for (value in listOf(JSONObject().put("queue", 1).put("condition", "ready"), JSONObject().put("source", "a").put("condition", "merged"), JSONObject().put("queue", 1).put("source", "a").put("task", "b").put("condition", "finished"))) {
            assertThrows(IllegalArgumentException::class.java) { QueueContracts.dependency(value) }
        }
    }

    @Test fun sourceTasksKeepCompositeIdentityAndReadonlyMetadata() {
        fun task(source: String) = JSONObject().put("source_id", source).put("id", "same").put("title", "Same title").put("state", "ready").put("worktree", "/work/$source").put("branch", "branch-$source").put("next_step", "review")
        val value = QueueContracts.sources(JSONObject().put("sources", JSONArray()).put("tasks", JSONArray().put(task("one")).put(task("two"))))
        assertEquals(2, value.tasks.size); assertEquals("one", value.tasks[0].sourceId); assertEquals("two", value.tasks[1].sourceId)
        assertEquals("branch-two", value.tasks[1].branch); assertEquals("review", value.tasks[1].nextStep)
    }

    @Test fun choreRegistrationIsDisabledAndClaimsRemainReadonly() {
        val args = QueueContracts.registerChore("daily", "Daily check", "chores-plugin", "repository-id", "job", "0 9 * * *", "UTC", spec()).getJSONObject("args")
        assertFalse(args.getBoolean("enabled")); assertEquals("UTC", args.getJSONObject("schedule").getString("timezone"))
        assertFalse(args.has("token"))
        val value = JSONObject().put("id", "daily").put("owner", "chores-plugin").put("repository", "repository-id").put("title", "Daily check")
            .put("registered", true).put("enabled", true).put("effective_enabled", false).put("owner_live", false).put("target", "job")
            .put("schedule", args.getJSONObject("schedule")).put("next_run", 1794000600L)
            .put("claim", JSONObject().put("day", "2026-11-06").put("generation", 3).put("session", "worker").put("claimed_at", 1794000000L).put("stale_at", 1794028800L)).put("outcome", "done")
        val chore = QueueContracts.choreInfo(value)
        assertFalse(chore.effectiveEnabled); assertEquals(false, chore.ownerLive); assertEquals(3L, chore.claim!!.generation)
        assertEquals(1794028800L, chore.claim!!.staleAt); assertEquals("done", chore.outcome)
        assertThrows(IllegalArgumentException::class.java) { QueueContracts.schedule("daily", "UTC") }
        assertThrows(IllegalArgumentException::class.java) { QueueContracts.schedule("0 9 * * *", "Other/Zone") }
    }

    @Test fun budgetsAndBatchesAreFiniteBoundedAndKeyed() {
        assertFalse(spec().has("budget")); assertEquals(1.25, spec(budget = "1.25").getDouble("budget"), 0.0)
        for (budget in listOf("NaN", "Infinity", "0", "-1")) assertThrows(IllegalArgumentException::class.java) { spec(budget = budget) }
        val batch = QueueContracts.batch(JSONArray().put(spec("one")).put(spec("two")).toString()).getJSONObject("args").getJSONArray("items")
        assertEquals("one", batch.getJSONObject(0).getString("key")); assertEquals("two", batch.getJSONObject(1).getString("key"))
        assertThrows(IllegalArgumentException::class.java) { QueueContracts.batch(JSONArray().put(spec("one")).put(spec("one")).toString()) }
        assertThrows(IllegalArgumentException::class.java) { QueueContracts.batch(JSONArray().put(spec().put("host", "worker")).toString()) }
    }
}
