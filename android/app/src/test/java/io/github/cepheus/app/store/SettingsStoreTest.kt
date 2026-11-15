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
        SettingsStore(memory).save(chosen.copy(background = false, theme = ThemeChoice.LIGHT))
        assertEquals(chosen.copy(background = false, theme = ThemeChoice.LIGHT), SettingsStore(memory).load())
    }
}
