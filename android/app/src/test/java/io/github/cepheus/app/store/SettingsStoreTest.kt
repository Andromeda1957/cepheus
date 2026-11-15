package io.github.cepheus.app.store

import org.junit.Assert.assertEquals
import org.junit.Test

class SettingsStoreTest {
    private class Memory : KeyValue {
        val strings = mutableMapOf<String, String>()
        override fun getString(key: String) = strings[key]
        override fun putString(key: String, value: String?) {
            if (value == null) strings.remove(key) else strings[key] = value
        }
        override fun getLong(key: String): Long? = null
        override fun putLong(key: String, value: Long?) {}
    }

    @Test
    fun absentOrUnknownThemeIsDark() {
        val memory = Memory()
        assertEquals(ThemeChoice.DARK, AppSettings().theme)
        assertEquals(ThemeChoice.DARK, SettingsStore(memory).load().theme)
        memory.strings["theme"] = "unknown-theme"
        assertEquals(ThemeChoice.DARK, SettingsStore(memory).load().theme)
    }

    @Test
    fun choicesAreKept() {
        val memory = Memory()
        val chosen = AppSettings(
            background = true,
            notifyFinished = false,
            notifyNeedsYou = true,
            notifyErrors = false,
            theme = ThemeChoice.DARK,
        )
        SettingsStore(memory).save(chosen)
        assertEquals(chosen, SettingsStore(memory).load())
        for (theme in ThemeChoice.entries) {
            val explicit = chosen.copy(background = false, theme = theme)
            SettingsStore(memory).save(explicit)
            assertEquals(explicit, SettingsStore(memory).load())
        }
    }
}
