package io.github.cepheus.app.ui

import io.github.cepheus.app.protocol.Usage
import io.github.cepheus.app.protocol.UsageWindow
import org.junit.Assert.assertEquals
import org.junit.Test

class StatsTest {
    @Test
    fun onlyUsedWindowsAreShown() {
        val week = UsageWindow("wk", 0.0, 1_700_000_000)
        val fiveHours = UsageWindow("5h", 25.0, 1_700_000_000)
        val resetOnly = UsageWindow("wk", null, 1_700_000_000)
        val argon = UsageWindow("Argon wk", 0.5, null)
        assertEquals(listOf(fiveHours, argon), usedWindows(listOf(week, fiveHours, resetOnly, argon)))
        assertEquals(emptyList<UsageWindow>(), usedWindows(listOf(week, resetOnly)))
        assertEquals(emptyList<UsageWindow>(), usedWindows(null))
        assertEquals(emptyList<UsageWindow>(), usedWindows(listOf(UsageWindow("wk", Double.NaN, null))))
    }

    @Test
    fun eachUsedModelHasOneMeterForItsMostUsedWindow() {

        val usage = Usage(
            claude = listOf(UsageWindow("5h", 42.0, 1_700_003_600), UsageWindow("wk", 90.0, 1_700_600_000)),
            codex = listOf(UsageWindow("wk", 0.0, null)),
            gemini = listOf(
                UsageWindow("wk", 25.0, 1_700_600_000),
                UsageWindow("5h", 25.0, 1_700_007_200),
                UsageWindow("Argon wk", 20.0, 1_700_600_000),
                UsageWindow("Argon 5h", 0.0, 1_700_007_200),
            ),
            claudeModels = listOf(UsageWindow("Fable wk", 30.0, 1_700_600_000)),
        )
        assertEquals(
            listOf(
                ModelUsage("Claude", UsageWindow("wk", 90.0, 1_700_600_000)),
                ModelUsage("Fable", UsageWindow("wk", 30.0, 1_700_600_000)),
                ModelUsage("Gemini", UsageWindow("wk", 25.0, 1_700_600_000)),
                ModelUsage("Argon", UsageWindow("wk", 20.0, 1_700_600_000)),
            ),
            modelUsage(usage),
        )
        assertEquals(emptyList<ModelUsage>(), modelUsage(null))
    }
}
