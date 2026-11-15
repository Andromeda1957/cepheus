package io.github.cepheus.app.ui

import androidx.compose.ui.graphics.Color
import org.junit.Assert.assertTrue
import org.junit.Test
import kotlin.math.pow






class ThemeContrastTest {
    private fun channel(value: Float): Double =
        if (value <= 0.04045f) value / 12.92 else ((value + 0.055) / 1.055).pow(2.4)

    private fun luminance(color: Color): Double =
        0.2126 * channel(color.red) + 0.7152 * channel(color.green) + 0.0722 * channel(color.blue)

    private fun contrast(a: Color, b: Color): Double {
        val (light, dark) = listOf(luminance(a), luminance(b)).sortedDescending()
        return (light + 0.05) / (dark + 0.05)
    }

    private fun assertContrast(name: String, fg: Color, bg: Color, floor: Double) {
        val ratio = contrast(fg, bg)
        assertTrue("$name is %.2f:1, below %.1f:1".format(ratio, floor), ratio >= floor)
    }

    private val themes = listOf(
        Triple("light", LightDeck, LightLink),
        Triple("dark", DarkDeck, DarkLink),
    )

    @Test fun textAndSecondaryTextReadOnEverySurface() {
        for ((theme, c, _) in themes) {
            val surfaces = mapOf(
                "page" to c.page, "raised" to c.raised, "bubble" to c.bubble,
                "control" to c.control, "selected" to c.selected, "code" to c.code,
            )
            for ((surface, bg) in surfaces) {
                assertContrast("$theme text on $surface", c.text, bg, 4.5)
                assertContrast("$theme secondary text on $surface", c.textSecondary, bg, 4.5)
            }
        }
    }

    @Test fun tertiaryTextReadsOnPageCardsAndControls() {
        for ((theme, c, _) in themes) {
            for ((surface, bg) in mapOf("page" to c.page, "raised" to c.raised, "control" to c.control, "code" to c.code)) {
                assertContrast("$theme tertiary text on $surface", c.textTertiary, bg, 4.5)
            }
        }
    }

    @Test fun linksAndTheBlueFillRead() {
        for ((theme, c, link) in themes) {
            assertContrast("$theme link on page", link, c.page, 4.5)
            assertContrast("$theme link on raised", link, c.raised, 4.5)
            assertContrast("$theme label on the blue fill", c.onAccent, c.accent, 4.5)
        }
    }

    @Test fun codeColoursReadOnTheCodeSurface() {
        for ((theme, c, _) in themes) {
            for (kind in CodeTokenKind.entries) {
                assertContrast("$theme $kind", codeColor(kind, theme == "dark"), c.code, 4.5)
            }
        }
    }

    @Test fun stateMarksAndMetersStandOut() {
        val marks = listOf(
            Triple("dark", listOf(DarkLink, Gold, Green), DarkDeck),
            Triple("light", listOf(LightDeck.accent, LightGold, LightGreen), LightDeck),
        )
        for ((theme, colors, c) in marks) {
            for (mark in colors) {
                assertContrast("$theme mark $mark on raised", mark, c.raised, 3.0)
                assertContrast("$theme mark $mark on page", mark, c.page, 3.0)
            }
        }
    }
}
