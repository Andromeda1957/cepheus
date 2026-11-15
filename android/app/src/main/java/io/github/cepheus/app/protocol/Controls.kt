package io.github.cepheus.app.protocol

import org.json.JSONObject

/** A reply is usable only for the requested action, and never when the payload reports a failure. */
object Controls {
    const val UPDATE_NEEDED = "This deck gateway does not support these controls yet. Update the desktop deck, then reconnect."

    /** A fleet operation is a service request; arguments never become a shell line. */
    fun fleet(operation: String, args: JSONObject = JSONObject(), host: String? = null): JSONObject {
        require(operation in setOf("status", "admission", "slot-release", "cancel", "rerun")) { "Unknown scheduler operation." }
        val values = JSONObject(args.toString())
        require(!values.has("host")) { "Choose the scheduler host separately." }
        host?.let { require(it.isNotBlank() && it.length <= 32) { "Invalid scheduler host." }; values.put("host", it) }
        return JSONObject().put("operation", operation).put("args", values)
    }

    fun errorMessage(message: String): String {
        val text = message.lowercase()
        val unsupported = listOf("unknown request", "unknown message", "unknown control", "unsupported control").any { it in text } ||
            ("unknown variant" in text && ("control" in text || "fleet" in text))
        return if (unsupported) UPDATE_NEEDED else message
    }

    fun data(action: String, reply: ServerMessage): JSONObject {
        require(reply is ServerMessage.Control && reply.action == action) {
            UPDATE_NEEDED
        }
        val data = reply.data
        val reported = data.opt("error")
        val error = when {
            reported is String -> reported.takeIf { it.isNotBlank() }
            reported == true -> data.optString("answer").takeIf { it.isNotBlank() } ?: "The deck control failed."
            else -> null
        }
        require(error == null) { error ?: "The control failed." }
        require(!data.has("ok") || data.optBoolean("ok")) {
            val lines = data.optJSONArray("lines")
            if (lines != null && lines.length() > 0) (0 until lines.length()).joinToString("\n") { lines.optString(it) }
            else "The deck reported that the control failed."
        }
        return data
    }
}
