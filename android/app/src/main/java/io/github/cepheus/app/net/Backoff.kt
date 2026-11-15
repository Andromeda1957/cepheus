package io.github.cepheus.app.net

/** Reconnect delays: 1 s, doubling to at most 60 s, back to 1 s after a connection that signed in. */
class Backoff(private val firstMs: Long = 1_000, private val maxMs: Long = 60_000) {
    private var failures = 0

    fun next(): Long {
        val delay = if (failures >= 30) maxMs else minOf(maxMs, firstMs shl failures)
        failures++
        return delay
    }

    fun reset() {
        failures = 0
    }
}
