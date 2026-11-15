package io.github.cepheus.app.store

import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject
import java.util.UUID

/** A separately paired gateway. Session numbers and alert cursors belong to this connection. */
data class SavedConnection(val id: String, val name: String, val credentials: Credentials)

/** All credentials remain sealed with the Android Keystore, including inactive connections. */
class Connections(
    private val kv: KeyValue,
    private val seal: (String) -> String,
    private val unseal: (String) -> String?,
) {
    fun all(): List<SavedConnection> {
        return try {
            val raw = kv.getString(PROFILES)?.let(unseal) ?: return emptyList()
            val array = JSONArray(raw)
            require(array.length() <= LIMIT)
            (0 until array.length()).map { index ->
                val row = array.getJSONObject(index)
                val id = row.getString("id")
                require(id == LEGACY || UUID.fromString(id).toString() == id)
                SavedConnection(id, row.getString("name"), Credentials(
                    row.getString("host"), row.getInt("port"), row.getString("fingerprint"),
                    row.getString("device"), row.getString("key"),
                ))
            }
        } catch (_: JSONException) {
            emptyList()
        } catch (_: IllegalArgumentException) {
            emptyList()
        }
    }

    fun active(): SavedConnection? {
        val saved = all()
        return saved.firstOrNull { it.id == kv.getString(ACTIVE) } ?: saved.firstOrNull()
    }

    @Synchronized
    fun save(credentials: Credentials, name: String, legacy: Boolean = false): SavedConnection {
        val saved = all()
        val existing = saved.firstOrNull {
            it.credentials.host.equals(credentials.host, ignoreCase = true) &&
                it.credentials.port == credentials.port && it.credentials.fingerprint == credentials.fingerprint
        }
        require(existing != null || saved.size < LIMIT) { "Remove a saved connection before adding another." }
        val connection = SavedConnection(
            existing?.id ?: if (legacy) LEGACY else UUID.randomUUID().toString(),
            name.trim().take(80).ifBlank { existing?.name ?: credentials.host }, credentials,
        )
        write(saved.filterNot { it.id == connection.id } + connection)
        kv.putString(ACTIVE, connection.id)
        return connection
    }

    @Synchronized
    fun select(id: String): Boolean {
        if (all().none { it.id == id }) return false
        kv.putString(ACTIVE, id)
        return true
    }

    @Synchronized
    fun remove(id: String) {
        val left = all().filterNot { it.id == id }
        write(left)
        if (kv.getString(ACTIVE) == id) kv.putString(ACTIVE, left.firstOrNull()?.id)
    }

    private fun write(saved: List<SavedConnection>) {
        val rows = JSONArray()
        for (connection in saved) {
            val c = connection.credentials
            rows.put(JSONObject().put("id", connection.id).put("name", connection.name)
                .put("host", c.host).put("port", c.port).put("fingerprint", c.fingerprint)
                .put("device", c.device).put("key", c.key))
        }
        kv.putString(PROFILES, seal(rows.toString()))
    }

    companion object {
        const val LEGACY = "legacy"
        const val LIMIT = 12
        private const val PROFILES = "connections.sealed"
        private const val ACTIVE = "connections.active"
    }
}

/** Keeps one gateway's alert cursor and prompt fingerprints separate from every other gateway. */
class ConnectionKeyValue(private val delegate: KeyValue, id: String) : KeyValue {
    private val prefix = if (id == Connections.LEGACY) "" else "connection.$id."
    override fun getString(key: String) = delegate.getString(prefix + key)
    override fun putString(key: String, value: String?) = delegate.putString(prefix + key, value)
    override fun getLong(key: String) = delegate.getLong(prefix + key)
    override fun putLong(key: String, value: Long?) = delegate.putLong(prefix + key, value)
}
