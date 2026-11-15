package io.github.cepheus.app.ui








object Screens {
    const val HOME = "home"
    const val CHAT = "chat"
    const val SETTINGS = "settings"
    const val TOOLS = "tools"
    const val TERMINAL = "terminal"

    // Notifier opens the Errors page by this name.
    const val ERRORS = "errors"
    const val STATS = "stats"

    /** Where Back goes from [screen]; [settingsFrom] is what Settings opened over. */
    fun back(screen: String, settingsFrom: String): String = when (screen) {
        STATS, ERRORS, TOOLS -> SETTINGS
        TERMINAL -> TOOLS
        SETTINGS -> if (settingsFrom == CHAT) CHAT else HOME
        else -> HOME
    }

    /**
     * How deep [screen] sits below the list: a page opens over a shallower
     * one, so moving deeper slides in from the right and Back slides out.
     */
    fun depth(screen: String): Int = when (screen) {
        HOME -> 0
        CHAT, SETTINGS -> 1
        TOOLS, STATS, ERRORS -> 2
        else -> 3
    }

    /**
     * What Settings returns to once opened from [current]: the list or a
     * session, or else what it opened over before ([previous]), since Stats
     * and Errors are its own pages.
     */
    fun settingsFrom(current: String, previous: String): String = when (current) {
        HOME, CHAT -> current
        else -> previous
    }
}
