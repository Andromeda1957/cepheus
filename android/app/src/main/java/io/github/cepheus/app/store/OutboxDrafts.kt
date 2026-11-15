package io.github.cepheus.app.store

import org.json.JSONArray
import org.json.JSONObject

/** Recovery copies only. Restoring never resends an uncertain request. */
class OutboxDrafts(private val storage: KeyValue) {
    fun save(messages: Map<Long, List<Outgoing>>) {
        val rows = JSONArray()
        messages.forEach { (id, list) -> list.forEach { p ->
            rows.put(JSONObject().put("id", id).put("key", p.key).put("text", p.text)
                .put("files", JSONArray(p.files)).put("paths", JSONArray(p.paths)).put("uris", JSONArray(p.uris))
                .put("generation", p.generation).put("after", p.after).put("sentAt", p.sentAt)
                .put("delivered", p.delivered).put("queued", p.queued).putOpt("delivery", p.delivery)
                .put("correlated", p.correlated).putOpt("client_id", p.clientId).putOpt("conversation", p.conversation)
                .put("submitted", p.submitted).put("known_unsent", p.knownUnsent).put("tokens", JSONArray(p.tokens))
                .put("createdAt", p.createdAt).putOpt("hostAt", p.hostAt).putOpt("error", p.error))
        } }
        storage.putString("outgoing-recovery", rows.toString())
    }

    fun load(): Map<Long, List<Outgoing>> {
        val rows = runCatching { JSONArray(storage.getString("outgoing-recovery")) }.getOrNull() ?: return emptyMap()
        fun strings(o: JSONObject, key: String): List<String> = o.optJSONArray(key)?.let { a ->
            (0 until a.length()).map { a.getString(it) }
        }.orEmpty()
        return (0 until rows.length()).mapNotNull { i -> runCatching {
            val o = rows.getJSONObject(i)
            o.getLong("id") to Outgoing(o.getLong("key"), o.getString("text"), strings(o, "files"),
                strings(o, "paths"), o.getLong("generation"), o.getInt("after"), o.getLong("sentAt"),
                delivered = o.optBoolean("delivered"), queued = o.optBoolean("queued"), createdAt = o.getLong("createdAt"),
                error = if (o.optBoolean("submitted") || o.optBoolean("delivered")) null else o.optString("error").ifEmpty { "Delivery was not confirmed. Check the conversation before sending again." },
                tokens = strings(o, "tokens"), uris = strings(o, "uris"), submitted = o.optBoolean("submitted"),
                knownUnsent = o.optBoolean("known_unsent"),
                clientId = o.optString("client_id").ifEmpty { null }, conversation = o.optString("conversation").ifEmpty { null },
                delivery = o.optString("delivery").ifEmpty { null }, correlated = o.optBoolean("correlated"),
                hostAt = if (o.has("hostAt")) o.getLong("hostAt") else null)
        }.getOrNull() }.groupBy({ it.first }, { it.second })
    }
}
