package io.github.cepheus.app.ui

import io.github.cepheus.app.protocol.Controls
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.runBlocking
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test
import java.io.IOException

class ToolRequestTest {
    @Test fun explicitRejectionAndAmbiguousFailureKeepTheirOriginalTypes() = runBlocking {
        for (error in listOf(IllegalArgumentException("Rejected spec"), IOException("Reply lost"))) {
            var observed: Throwable? = null
            var completed = false
            val request = ToolRequest(this, control = { _, _ -> Result.failure(error) })
            request.run("fleet", failed = { observed = it }) { completed = true }
            coroutineContext[kotlinx.coroutines.Job]!!.children.forEach { it.join() }
            assertSame(error, observed); assertFalse(completed); assertFalse(request.busy)
            assertEquals(error.message, request.error)
        }
    }

    @Test fun busyRequestCannotBeReplacedAndRefreshKeepsThePriorSnapshot() = runBlocking {
        val response = CompletableDeferred<Result<JSONObject>>()
        var calls = 0
        val request = ToolRequest(CoroutineScope(Dispatchers.Unconfined), control = { _, _ -> calls++; response.await() })
        val previous = JSONObject().put("old", true); request.data = previous
        var first = false; var second = false
        request.run("fleet", keep = true) { first = true }
        request.run("fleet") { second = true }
        assertTrue(request.busy); assertSame(previous, request.data); assertEquals(1, calls)
        val updated = JSONObject().put("new", true); response.complete(Result.success(updated))
        assertTrue(first); assertFalse(second); assertFalse(request.busy); assertSame(updated, request.data)
    }

    @Test fun unsupportedControlsNeverReachTheGateway() {
        var calls = 0; var rejection: Throwable? = null
        val request = ToolRequest(CoroutineScope(Dispatchers.Unconfined), capabilities = { emptySet() }, control = { _, _ -> calls++; Result.success(JSONObject()) })
        request.run("fleet", failed = { rejection = it })
        assertEquals(0, calls); assertFalse(request.busy)
        assertTrue(rejection is IllegalArgumentException); assertEquals(Controls.UPDATE_NEEDED, request.error)
    }
}
