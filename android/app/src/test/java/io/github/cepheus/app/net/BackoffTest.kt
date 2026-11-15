package io.github.cepheus.app.net

import org.junit.Assert.assertEquals
import org.junit.Test

class BackoffTest {
    @Test
    fun doublesFromOneSecondToAMinute() {
        val backoff = Backoff()
        val delays = List(9) { backoff.next() }
        assertEquals(listOf(1_000L, 2_000L, 4_000L, 8_000L, 16_000L, 32_000L, 60_000L, 60_000L, 60_000L), delays)
    }

    @Test
    fun staysAtTheCapAfterManyFailures() {
        val backoff = Backoff()
        repeat(100) { backoff.next() }
        assertEquals(60_000L, backoff.next())
    }
}
