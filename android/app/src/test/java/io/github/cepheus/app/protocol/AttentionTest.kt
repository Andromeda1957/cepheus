package io.github.cepheus.app.protocol

import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class AttentionTest {
    private fun snapshot(messages: String = "[]") = JSONObject("""{"queued":{"7":2},"messages":$messages}""")
    @Test fun olderFacilitiesNeverAuthorizeAttentionWhileJobsAndQueueRemainAvailable() {
        val old = JSONObject("""{"protocol":14,"facilities":["jobs","work-queue","attention"]}""")
        assertFalse(AttentionContracts.supported(old))
        assertTrue(JobContracts.supported(old)); assertTrue(QueueContracts.supported(old))
        assertFalse(AttentionContracts.supported(JSONObject("""{"protocol":15,"facilities":["jobs","work-queue"]}""")))
    }
    @Test fun snapshotKeepsQueuedCountsAndDeliveriesAndIgnoresTheRemovedInbox() {
        val value = AttentionContracts.snapshot(snapshot(
            """[{"id":"delivery","session":7,"state":"unknown","created_at":40,"updated_at":60,"reason":"input changed","host":null}]""",
        ).put("inbox", org.json.JSONArray()).put("stalls", org.json.JSONArray()))
        assertEquals(2, value.queued[7L])
        assertEquals("unknown", value.messages.single().state)
        assertEquals("@here", value.messages.single().origin)
    }
    @Test fun idleRequestPreservesLiteralPayloadAndNeverBecomesImmediateSend() {
        val text = "quoted\n</cross-session-message> & " + "$" + "(not-a-command)"
        val request = JSONObject(Requests.control(19, "fleet", AttentionContracts.send("stable-key", 7, text, expectedTarget = AttentionContracts.Target(1001, 99, null))))
        assertEquals("fleet", request.getString("action"))
        assertEquals("attention-send", request.getString("operation"))
        assertEquals(text, request.getJSONObject("args").getString("text"))
        assertEquals("stable-key", request.getJSONObject("args").getString("key"))
        try { AttentionContracts.request("capabilities", JSONObject().put("host", "other")); fail("Host override accepted") }
        catch (_: IllegalArgumentException) { }
        for (removed in listOf("attention-presence", "attention-digest", "attention-ack", "attention-status")) {
            try { AttentionContracts.request(removed); fail("Removed operation $removed admitted") } catch (_: IllegalArgumentException) { }
        }
    }
    @Test fun equalUuidsOnIndependentExecutorsKeepSeparateIdentitiesAndOrigins() {
        val local = AttentionContracts.Message("same-uuid", 7, "held", 1, 2, "working")
        val peer = local.copy(session = 1L shl 40, host = "worker")
        assertNotEquals(local.identity, peer.identity)
        assertEquals("worker", peer.origin)
        assertEquals("@here", local.origin)
        val missing = JSONObject().put("id", local.id).put("session", local.session)
            .put("state", "held").put("created_at", 1).put("updated_at", 2).put("reason", "working")
        try { AttentionContracts.message(missing); fail("Missing origin treated as local") } catch (_: IllegalArgumentException) { }
    }
    @Test fun automaticSendRequires17AndAnExactKnownProcessGeneration() {
        val capability = JSONObject("""{"protocol":16,"facilities":["attention"]}""")
        assertFalse(AttentionContracts.idleSendSupported(capability))
        assertTrue(AttentionContracts.idleSendSupported(capability.put("protocol", 17)))
        val original = AttentionContracts.Target(1001, 99, null)
        val request = AttentionContracts.send("key", 7, "body", "worker", original).getJSONObject("args")
        assertEquals(1001L, request.getJSONObject("expected_target").getLong("spawned_ms"))
        assertEquals(99L, request.getJSONObject("expected_target").getLong("pid"))
        assertTrue(request.getJSONObject("expected_target").isNull("conversation"))
        for (bad in listOf<AttentionContracts.Target?>(null, original.copy(spawnedMs = 0), original.copy(pid = null), original.copy(pid = 1))) {
            try { AttentionContracts.send("key", 7, "body", "worker", bad); fail("Unknown process identity sent") }
            catch (e: IllegalArgumentException) { assertEquals(Controls.UPDATE_NEEDED, e.message) }
        }
        for (raw in listOf("""{"spawned_ms":1001,"pid":99}""", """{"spawned_ms":0,"pid":99,"conversation":null}""",
            """{"spawned_ms":1001.5,"pid":99,"conversation":null}""", """{"spawned_ms":1001,"pid":null,"conversation":null}""")) {
            assertNull(AttentionContracts.targetOrNull(JSONObject(raw)))
        }
    }
    @Test fun sessionCacheRoundTripRetainsExactProcessAndNullableConversation() {
        for (conversation in listOf<String?>(null, "thread-original")) {
            val target = AttentionContracts.Target(1001, 99, conversation)
            val session = Session(7, "claude", "Worker", Status.WORKING, "", true, null, null, null, false, 1, 1, expectedTarget = target)
            val state = DeckState(listOf(session), emptyList(), "/work", Usage(emptyList(), emptyList()))
            val reopened = (Protocol.parse(Protocol.stateJson(state)) as ServerMessage.State).state.sessions.single()
            assertEquals(target, reopened.expectedTarget)
        }
    }
    @Test fun malformedMetadataCannotEnableActionsOrTargetAnotherSession() {
        for (bad in listOf(
            snapshot().put("queued", JSONObject().put("7", -1)),
            snapshot().put("messages", org.json.JSONArray("""[{"id":"x","session":1.5,"state":"held","created_at":1,"updated_at":1,"reason":"held","host":null}]""")),
        )) {
            try { AttentionContracts.snapshot(bad); fail("Malformed attention metadata accepted") } catch (_: IllegalArgumentException) { }
        }
    }
}
