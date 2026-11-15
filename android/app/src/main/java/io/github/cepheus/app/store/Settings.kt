package io.github.cepheus.app.store

enum class ThemeChoice(val key: String, val label: String) {
    SYSTEM("system", "System default"),
    LIGHT("light", "Light"),
    DARK("dark", "Dark"),
    ;

    fun isDark(systemDark: Boolean): Boolean = when (this) {
        SYSTEM -> systemDark
        LIGHT -> false
        DARK -> true
    }

    companion object {
        fun of(key: String?): ThemeChoice = entries.firstOrNull { it.key == key } ?: SYSTEM
    }
}







data class AppSettings(
    val background: Boolean = false,
    val notifyFinished: Boolean = true,
    val notifyNeedsYou: Boolean = true,
    val notifyErrors: Boolean = true,
    val theme: ThemeChoice = ThemeChoice.SYSTEM,
)

class SettingsStore(private val kv: KeyValue) {
    fun load(): AppSettings {
        val defaults = AppSettings()
        return AppSettings(
            background = flag(BACKGROUND, defaults.background),
            notifyFinished = flag(NOTIFY_FINISHED, defaults.notifyFinished),
            notifyNeedsYou = flag(NOTIFY_NEEDS_YOU, defaults.notifyNeedsYou),
            notifyErrors = flag(NOTIFY_ERRORS, defaults.notifyErrors),
            theme = ThemeChoice.of(kv.getString(THEME)),
        )
    }

    fun save(settings: AppSettings) {
        kv.putString(BACKGROUND, settings.background.toString())
        kv.putString(NOTIFY_FINISHED, settings.notifyFinished.toString())
        kv.putString(NOTIFY_NEEDS_YOU, settings.notifyNeedsYou.toString())
        kv.putString(NOTIFY_ERRORS, settings.notifyErrors.toString())
        kv.putString(THEME, settings.theme.key)
    }

    private fun flag(key: String, default: Boolean): Boolean = kv.getString(key)?.toBooleanStrictOrNull() ?: default

    private companion object {
        const val BACKGROUND = "background"
        const val NOTIFY_FINISHED = "notify_finished"
        const val NOTIFY_NEEDS_YOU = "notify_needs_you"
        const val NOTIFY_ERRORS = "notify_errors"
        const val THEME = "theme"
    }
}
