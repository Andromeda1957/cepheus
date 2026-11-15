package io.github.cepheus.app.protocol

import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class ControlsTest {
    @Test fun fleetControlsPreserveTheSelectedMachineAndExactTarget() {
        val original = JSONObject().put("id", 17)
        val payload = Controls.fleet("rerun", original, "worker")
        assertEquals("rerun", payload.getString("operation"))
        assertEquals("worker", payload.getJSONObject("args").getString("host"))
        assertEquals(17, payload.getJSONObject("args").getInt("id"))
        assertFalse(original.has("host"))
        assertFalse(Controls.fleet("status").getJSONObject("args").has("host"))
        for (operation in listOf("exec", "deploy", "")) {
            try { Controls.fleet(operation); fail("Unsupported scheduler operation accepted") }
            catch (_: IllegalArgumentException) { }
        }
        try { Controls.fleet("status", JSONObject().put("host", "other"), "worker"); fail("Host override accepted") }
        catch (_: IllegalArgumentException) { }
    }

    @Test fun olderFleetServicesGiveTheExplicitUpdateMessage() {
        assertEquals(Controls.UPDATE_NEEDED, Controls.errorMessage("unknown variant `Fleet`"))
        assertEquals(Controls.UPDATE_NEEDED, Controls.errorMessage("unknown control fleet"))
        assertEquals("Host is offline", Controls.errorMessage("Host is offline"))
    }

    @Test fun controlRepliesRemainCorrelatedAndPreserveTheirPayload() {
        val parsed = Protocol.parse("""{"t":"control","rid":42,"action":"terminal_screen","data":{"id":7,"text":"hello\nworld","alive":true}}""")
        assertEquals(42L, parsed.rid)
        val data = Controls.data("terminal_screen", parsed)
        assertEquals("hello\nworld", data.getString("text"))
        assertTrue(data.getBoolean("alive"))
    }

    @Test fun controlsNeverAcceptUnrelatedSuccessOrPayloadErrors() {
        for (reply in listOf(
            ServerMessage.Ok(4, 7),
            ServerMessage.Control(4, "other", JSONObject()),
            ServerMessage.Control(4, "install", JSONObject().put("ok", false)),
            ServerMessage.Control(4, "install", JSONObject().put("error", "not installed")),
        )) {
            try { Controls.data("install", reply); fail("A failed or unrelated reply cannot be success") }
            catch (_: IllegalArgumentException) { }
        }
    }

    @Test fun successfulControlBooleanDoesNotBecomeAStringError() {
        val data = JSONObject().put("error", false).put("answer", "Here is the answer")
        assertEquals("Here is the answer", Controls.data("plugin", ServerMessage.Control(1, "plugin", data)).getString("answer"))
        try {
            Controls.data("plugin", ServerMessage.Control(1, "plugin", JSONObject().put("error", true).put("answer", "provider unavailable")))
            fail("Failed control accepted")
        } catch (e: IllegalArgumentException) { assertEquals("provider unavailable", e.message) }
    }

    @Test fun malformedControlPayloadIsUnknown() {
        assertTrue(Protocol.parse("""{"t":"control","rid":2,"action":"overview","data":false}""") is ServerMessage.Unknown)
    }

    @Test fun controlEnvelopeKeepsLiteralArgumentsAndCannotBeOverridden() {
        val literal = "'quoted' $(not-a-shell)\nnext"
        val request = JSONObject(Requests.control(3, "terminal_input", JSONObject().put("id", 7).put("text", literal)))
        assertEquals("control", request.getString("t"))
        assertEquals(3, request.getInt("rid"))
        assertEquals(literal, request.getString("text"))
        for (reserved in listOf("rid", "t", "action")) {
            try { Requests.control(3, "terminal", JSONObject().put(reserved, "other")); fail("Reserved field accepted") }
            catch (_: IllegalArgumentException) { }
        }
    }
}
