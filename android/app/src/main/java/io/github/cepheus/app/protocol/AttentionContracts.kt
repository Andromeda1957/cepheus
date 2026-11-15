package io.github.cepheus.app.protocol

import org.json.JSONArray
import org.json.JSONObject

/** Public idle-delivery metadata; queued bodies stay private to the deck. */
object AttentionContracts {
    const val PROTOCOL_VERSION = 15
    const val IDLE_SEND_PROTOCOL_VERSION = 17
    data class Target(val spawnedMs: Long, val pid: Long?, val conversation: String?)
    data class Message(val id: String, val session: Long, val state: String, val createdAt: Long, val updatedAt: Long, val reason: String, val host: String? = null) {
        val origin: String get() = host ?: "@here"
        val identity: String get() = JSONArray().put(origin).put(id).toString()
    }
    data class Snapshot(val queued: Map<Long, Int>, val messages: List<Message>)

    fun supported(value: JSONObject): Boolean = number(value.opt("protocol"))?.let { it >= PROTOCOL_VERSION } == true &&
        value.optJSONArray("facilities")?.let { array -> (0 until array.length()).any { array.opt(it) == "attention" } } == true

    fun request(operation: String, args: JSONObject = JSONObject()): JSONObject {
        require(operation in setOf("capabilities", "attention-send")) { "Unknown attention operation." }
        if (args.has("host")) {
            require(operation == "attention-send") { "This action does not take a message origin." }
            identifier(text(args, "host"))
        }
        return JSONObject().put("operation", operation).put("args", JSONObject(args.toString()))
    }
    fun idleSendSupported(value: JSONObject): Boolean = supported(value) &&
        number(value.opt("protocol"))?.let { it >= IDLE_SEND_PROTOCOL_VERSION } == true

    /** Missing or malformed legacy metadata never becomes a fresh target. */
    fun targetOrNull(value: JSONObject): Target? = runCatching {
        require(value.has("spawned_ms") && value.has("pid") && value.has("conversation"))
        val generation = number(value.opt("spawned_ms")) ?: throw IllegalArgumentException()
        val pid = if (value.isNull("pid")) null else number(value.opt("pid")) ?: throw IllegalArgumentException()
        val conversation = if (value.isNull("conversation")) null else value.opt("conversation") as? String ?: throw IllegalArgumentException()
        Target(generation, pid, conversation).also { targetJson(it) }
    }.getOrNull()

    fun targetJson(target: Target): JSONObject {
        require(target.spawnedMs > 0 && target.pid != null && target.pid in 2L..4294967295L) { Controls.UPDATE_NEEDED }
        return JSONObject().put("spawned_ms", target.spawnedMs).put("pid", target.pid)
            .put("conversation", target.conversation ?: JSONObject.NULL)
    }

    fun send(key: String, session: Long, text: String, host: String? = null, expectedTarget: Target? = null): JSONObject = request("attention-send", JSONObject()
        .put("key", identifier(key)).put("session", positive(session)).put("text", body(text))
        .put("expected_target", targetJson(expectedTarget ?: throw IllegalArgumentException(Controls.UPDATE_NEEDED)))
        .apply { host?.let { put("host", identifier(it)) } })
    fun snapshot(value: JSONObject): Snapshot {
        val o = value.optJSONObject("attention") ?: value
        val queued = o.optJSONObject("queued") ?: throw IllegalArgumentException("Invalid queued counts.")
        require(queued.length() <= 10000) { "Too many queued counts." }
        return Snapshot(queued.keys().asSequence().associate { key ->
            val id = key.toLongOrNull()?.let(::positive) ?: throw IllegalArgumentException("Invalid queued session.")
            id to nonnegative(queued, key).also { require(it <= Int.MAX_VALUE) }.toInt()
        }, objects(o, "messages").map(::message))
    }
    fun message(value: JSONObject): Message {
        val o = value.optJSONObject("message") ?: value
        require(o.has("host")) { "Message origin is missing; update the deck and reconnect." }
        return Message(identifier(text(o, "id")), positive(nonnegative(o, "session")), text(o, "state"),
            nonnegative(o, "created_at"), nonnegative(o, "updated_at"), text(o, "reason"), optionalText(o, "host")?.let(::identifier))
    }
    private fun body(value: String): String = value.also { require(it.isNotBlank() && it.toByteArray(Charsets.UTF_8).size <= 16000 && '\u0000' !in it) { "Enter a message of at most 16000 UTF-8 bytes." } }
    private fun identifier(value: String): String = value.also { require(it.isNotBlank() && it.length <= 128 && it.none(Char::isISOControl)) { "Invalid attention identity." } }
    private fun objects(o: JSONObject, key: String, limit: Int = 10000): List<JSONObject> = array(o, key, limit).let { values -> (0 until values.length()).map(values::getJSONObject) }
    private fun array(o: JSONObject, key: String, limit: Int): JSONArray = (o.optJSONArray(key) ?: throw IllegalArgumentException("Invalid $key.")).also { require(it.length() <= limit) { "Too many $key." } }
    private fun text(o: JSONObject, key: String): String = o.opt(key) as? String ?: throw IllegalArgumentException("Invalid $key.")
    private fun optionalText(o: JSONObject, key: String): String? = if (o.isNull(key)) null else text(o, key)
    private fun number(value: Any?): Long? = when (value) { is Long -> value; is Int -> value.toLong(); else -> null }
    private fun nonnegative(o: JSONObject, key: String): Long = number(o.opt(key))?.takeIf { it >= 0 } ?: throw IllegalArgumentException("Invalid $key.")
    private fun positive(value: Long): Long = value.also { require(it > 0) { "Invalid session ID." } }
}
