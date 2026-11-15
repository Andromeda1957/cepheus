package io.github.cepheus.app.ui

import org.junit.Assert.assertEquals
import org.junit.Test

/** Back from Settings to whatever it opened over. */
class ScreensTest {
    @Test
    fun settingsGoesBackToWhatItOpenedOver() {
        assertEquals(Screens.HOME, Screens.back(Screens.SETTINGS, settingsFrom = Screens.HOME))
        assertEquals(Screens.CHAT, Screens.back(Screens.SETTINGS, settingsFrom = Screens.CHAT))
        assertEquals(Screens.HOME, Screens.back(Screens.SETTINGS, settingsFrom = "unknown"))
    }

    @Test
    fun settingsRemembersOnlyTheListOrASession() {
        assertEquals(Screens.HOME, Screens.settingsFrom(Screens.HOME, previous = Screens.CHAT))
        assertEquals(Screens.CHAT, Screens.settingsFrom(Screens.CHAT, previous = Screens.HOME))
        // Opened again from Stats or Errors, Settings still returns to where it first opened.
        assertEquals(Screens.CHAT, Screens.settingsFrom(Screens.STATS, previous = Screens.CHAT))
        assertEquals(Screens.HOME, Screens.settingsFrom(Screens.ERRORS, previous = Screens.HOME))
        assertEquals(Screens.CHAT, Screens.settingsFrom(Screens.SETTINGS, previous = Screens.CHAT))
    }
}
