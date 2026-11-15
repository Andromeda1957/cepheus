package io.github.cepheus.app.ui

import java.time.Instant
import java.time.ZoneId
import java.util.Locale
import kotlin.math.roundToLong

/** Numbers as the Stats screen and attachments write them; sizes count as the deck's header does (1 GB = 1024³ bytes). */
object Format {
    private const val KIB = 1024.0
    private const val MIB = KIB * 1024
    private const val GIB = MIB * 1024

    /** A disk's size: "412 GB", "8.4 GB", "1.8 TB", "512 MB". */
    fun size(bytes: Long): String {
        val gib = bytes / GIB
        return when {
            gib >= 1000 -> "${one(gib / 1024)} TB"
            gib >= 10 -> "${gib.roundToLong()} GB"
            gib >= 1 -> "${one(gib)} GB"
            else -> "${(bytes / MIB).roundToLong()} MB"
        }
    }

    /** An attachment's size: "900 B", "1.5 KB", "2.4 MB", "25 MB". */
    fun fileSize(bytes: Long): String = when {
        bytes < KIB -> "$bytes B"
        bytes < MIB -> "${trim(bytes / KIB)} KB"
        bytes < GIB -> "${trim(bytes / MIB)} MB"
        else -> "${trim(bytes / GIB)} GB"
    }

    fun percent(value: Double?): String = value?.let { "${it.roundToLong()}%" } ?: "–"

    /** When a usage window starts over, from its `resets_at` (seconds). */
    fun resets(resetsAt: Long?, nowSecs: Long = System.currentTimeMillis() / 1000): String? {
        resetsAt ?: return null
        val left = resetsAt - nowSecs
        return if (left <= 0) "Resets now" else "Resets in ${duration(left)}"
    }

    /** As the deck's `short_duration`, with a space: "45s", "12m", "2h 05m", "6d 23h". */
    fun duration(secs: Long): String = when {
        secs < 60 -> "${secs}s"
        secs < 3600 -> "${secs / 60}m"
        secs < 86_400 -> "${secs / 3600}h %02dm".format(Locale.ROOT, (secs % 3600) / 60)
        else -> "${secs / 86_400}d %02dh".format(Locale.ROOT, (secs % 86_400) / 3600)
    }

    /** Whether a message sent at [at] (ms) was sent today, so its time alone names it. */
    fun sentToday(at: Long, now: Long = System.currentTimeMillis(), zone: ZoneId = ZoneId.systemDefault()): Boolean =
        Instant.ofEpochMilli(at).atZone(zone).toLocalDate() == Instant.ofEpochMilli(now).atZone(zone).toLocalDate()

    private fun one(value: Double) = "%.1f".format(Locale.ROOT, value)

    /** One decimal below 10, none above, and no ".0". */
    private fun trim(value: Double): String =
        if (value >= 10) "${value.roundToLong()}" else one(value).removeSuffix(".0")
}
