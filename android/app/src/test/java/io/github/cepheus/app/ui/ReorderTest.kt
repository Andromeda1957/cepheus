package io.github.cepheus.app.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * How fast the session list scrolls under a dragged row held at one of its
 * edges (`Reorder.kt`). `tools/tests/reorder_autoscroll_check.py` holds a
 * real row there on an emulator.
 */
class ReorderTest {
    // A list showing 600 px of content and 100 px rows. Scrolling starts 20 px
    // from an edge and reaches 1000 px a second 80 px further on.
    private fun speed(top: Float) =
        edgeScrollSpeed(top = top, bottom = top + 100f, end = 600f, inset = 20f, ramp = 80f, fastest = 1000f)

    @Test
    fun aRowAwayFromTheEdgesLeavesTheListStill() {
        assertEquals(0f, speed(250f), 0f)
        assertEquals("its top at the inset", 0f, speed(20f), 0f)
        assertEquals("its bottom at the inset", 0f, speed(480f), 0f)
    }

    @Test
    fun aRowHeldAtTheStartScrollsTowardItFasterTheFurtherItGoes() {
        val near = speed(10f)
        val atEdge = speed(0f)
        val under = speed(-40f)
        assertTrue("toward the start is negative: $near", near < 0f)
        assertTrue("$atEdge is faster than $near", atEdge < near)
        assertTrue("$under is faster than $atEdge", under < atEdge)
        assertEquals(-1000f, speed(-60f), 0.01f)
        assertEquals("never faster than the fastest", -1000f, speed(-400f), 0.01f)
    }

    @Test
    fun aRowHeldAtTheEndScrollsTowardItFasterTheFurtherItGoes() {
        val near = speed(490f)
        val atEdge = speed(500f)
        val under = speed(540f)
        assertTrue("toward the end is positive: $near", near > 0f)
        assertTrue("$atEdge is faster than $near", atEdge > near)
        assertTrue("$under is faster than $atEdge", under > atEdge)
        assertEquals(1000f, speed(560f), 0.01f)
        assertEquals("never faster than the fastest", 1000f, speed(900f), 0.01f)
    }

    @Test
    fun theSameDepthScrollsAsFastEitherWay() {
        assertEquals(-speed(-25f), speed(525f), 0.01f)
    }
}
