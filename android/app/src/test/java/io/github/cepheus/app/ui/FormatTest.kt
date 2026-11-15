package io.github.cepheus.app.ui

import java.time.ZoneId
import java.time.ZonedDateTime
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** The Stats screen's numbers, written as the deck's header writes them. */
class FormatTest {
    private val gib = 1024L * 1024 * 1024

    @Test
    fun sizes() {
        assertEquals("412 GB", Format.size(412 * gib))
        assertEquals("8.4 GB", Format.size((8.4 * gib).toLong()))
        assertEquals("1.8 TB", Format.size((1.8 * 1024 * gib).toLong()))
        assertEquals("512 MB", Format.size(512L * 1024 * 1024))
    }

    @Test
    fun resets() {
        val now = 1_700_000_000L
        assertEquals("Resets in 45s", Format.resets(now + 45, now))
        assertEquals("Resets in 12m", Format.resets(now + 12 * 60, now))
        assertEquals("Resets in 2h 05m", Format.resets(now + 2 * 3600 + 5 * 60, now))
        assertEquals("Resets in 6d 23h", Format.resets(now + 6 * 86_400 + 23 * 3600, now))
        assertEquals("Resets now", Format.resets(now - 10, now))
        assertNull(Format.resets(null, now))
    }

    @Test
    fun fileSizes() {
        assertEquals("0 B", Format.fileSize(0))
        assertEquals("900 B", Format.fileSize(900))
        assertEquals("1.5 KB", Format.fileSize(1536))
        assertEquals("25 MB", Format.fileSize(25L * 1024 * 1024))
        assertEquals("2.4 MB", Format.fileSize((2.4 * 1024 * 1024).toLong()))
    }

    @Test
    fun aMessageFromTodayShowsItsTimeAlone() {
        val zone = ZoneId.of("America/Chicago")
        val now = ZonedDateTime.of(2026, 11, 2, 14, 29, 0, 0, zone).toInstant().toEpochMilli()
        val morning = ZonedDateTime.of(2026, 11, 2, 0, 5, 0, 0, zone).toInstant().toEpochMilli()
        val lastNight = ZonedDateTime.of(2026, 11, 1, 23, 55, 0, 0, zone).toInstant().toEpochMilli()
        assertTrue(Format.sentToday(morning, now, zone))
        assertFalse(Format.sentToday(lastNight, now, zone))
    }
}
