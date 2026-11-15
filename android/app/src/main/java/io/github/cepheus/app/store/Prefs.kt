package io.github.cepheus.app.store

import android.content.Context
import android.content.SharedPreferences
import androidx.core.content.edit
import io.github.cepheus.app.pair.wsUrl

/** A paired deck: where it is, the certificate it must present, and this phone's sign-in. */
data class Credentials(
    val host: String,
    val port: Int,
    val fingerprint: String,
    val device: String,
    val key: String,
) {
    val url: String get() = wsUrl(host, port)

    // The key is a credential: never print it.
    override fun toString() = "Credentials(host=$host, port=$port, device=$device)"
}

/** Small values the app keeps between runs. */
interface KeyValue {
    fun getString(key: String): String?
    fun putString(key: String, value: String?)
    fun getLong(key: String): Long?
    fun putLong(key: String, value: Long?)
}

class PrefsKeyValue(private val prefs: SharedPreferences) : KeyValue {
    override fun getString(key: String): String? = prefs.getString(key, null)

    override fun putString(key: String, value: String?) = prefs.edit {
        if (value == null) remove(key) else putString(key, value)
    }

    override fun getLong(key: String): Long? = if (prefs.contains(key)) prefs.getLong(key, 0) else null

    override fun putLong(key: String, value: Long?) = prefs.edit {
        if (value == null) remove(key) else putLong(key, value)
    }
}

class Prefs(context: Context) {
    private val prefs = context.getSharedPreferences("agentdeck", Context.MODE_PRIVATE)
    val keyValue: KeyValue = PrefsKeyValue(prefs)
    val connections = Connections(keyValue, Secrets::encrypt, Secrets::decrypt)

    /** The Settings screen's choices, in a file of their own so unpairing keeps them. */
    val settings = SettingsStore(PrefsKeyValue(context.getSharedPreferences("settings", Context.MODE_PRIVATE)))

    init {
        // Migrate the existing pairing without changing its cache or alert cursor.
        if (connections.all().isEmpty()) legacyCredentials()?.let { c ->
            connections.save(c, c.host, legacy = true)
        }
        if (connections.all().isNotEmpty()) prefs.edit {
            remove(HOST); remove(PORT); remove(FP); remove(DEVICE); remove(KEY)
        }
    }

    fun credentials(): Credentials? = connections.active()?.credentials

    private fun legacyCredentials(): Credentials? {
        val host = prefs.getString(HOST, null) ?: return null
        val fp = prefs.getString(FP, null) ?: return null
        val device = prefs.getString(DEVICE, null)?.let(Secrets::decrypt) ?: return null
        val key = prefs.getString(KEY, null)?.let(Secrets::decrypt) ?: return null
        return Credentials(host, prefs.getInt(PORT, 0), fp, device, key)
    }

    var batteryAsked: Boolean
        get() = prefs.getBoolean(BATTERY_ASKED, false)
        set(value) = prefs.edit { putBoolean(BATTERY_ASKED, value) }

    private companion object {
        const val HOST = "host"
        const val PORT = "port"
        const val FP = "fp"
        const val DEVICE = "device_sealed"
        const val KEY = "key_sealed"
        const val BATTERY_ASKED = "battery_asked"
    }
}
