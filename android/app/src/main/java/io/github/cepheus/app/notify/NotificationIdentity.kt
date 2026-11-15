package io.github.cepheus.app.notify

import java.net.URLEncoder
import java.nio.charset.StandardCharsets
import java.security.MessageDigest

/** Full identities survive Android's 32-bit notification and request-code fields. */
internal object NotificationIdentity {
    private fun part(value: String) = URLEncoder.encode(value, "UTF-8")
    private fun root(connection: String) = "cepheus://notification/v2/" + part(connection)
    fun tag(kind: String, connection: String, session: Long) = "$kind:${part(connection)}:$session"
    /** Digest and stall-warning cards an older APK posted; neither exists any more. */
    fun retired(tag: String?, id: Int) = tag?.startsWith("attention_warning:") == true || id == 0 &&
        (tag == "attention_digest" || tag?.startsWith("attention_digest:") == true && tag.endsWith(":0"))
    fun session(kind: String, connection: String, tag: String?, id: Int): Long? {
        val prefix = "$kind:${part(connection)}:"
        if (tag?.startsWith(prefix) != true) return null
        return tag.removePrefix(prefix).toLongOrNull()?.takeIf { it > 0 && it.toInt() == id }
    }
    fun open(connection: String, session: Long?, screen: String?) = root(connection) +
        "/open/session/" + (session?.toString() ?: "none") + "/screen/" + (screen?.let { "named/" + part(it) } ?: "none")
    fun action(connection: String, session: Long, role: String, slot: Int, inputs: Long?, fingerprint: String?): String {
        // Fingerprints are custody metadata; no prompt text appears in intent data.
        val fence = fingerprint?.let { value -> MessageDigest.getInstance("SHA-256")
            .digest(value.toByteArray(StandardCharsets.UTF_8)).joinToString("") { "%02x".format(it) } } ?: "none"
        return root(connection) + "/session/$session/${part(role)}/$slot/inputs/" + (inputs?.toString() ?: "none") + "/prompt/" + fence
    }
    fun matchesAction(data: String?, connection: String, session: Long, role: String, slot: Int, inputs: Long?, fingerprint: String?) =
        data == action(connection, session, role, slot, inputs, fingerprint)
    fun matchesOpen(data: String?, connection: String, session: Long?, screen: String?) = data == open(connection, session, screen)
}
