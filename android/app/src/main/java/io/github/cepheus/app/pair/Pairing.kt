package io.github.cepheus.app.pair

import io.github.cepheus.app.net.Pin
import java.net.URLDecoder

/** Where a deck is and what proves it: its address, pinned certificate and one-time code. */
data class PairingTarget(val host: String, val port: Int, val fingerprint: String, val code: String) {
    val url: String get() = wsUrl(host, port)
}

fun wsUrl(host: String, port: Int): String {
    val h = if (host.contains(':')) "[$host]" else host
    return "wss://$h:$port/v1"
}

/**
 * Reads the deck's pairing QR code,
 * `cepheus://pair?host=<ip>&port=<port>&fp=<sha256>&code=<8 chars>`, and the
 * same four fields typed by hand.
 */
object Pairing {
    const val CODE_LENGTH = 8

    /** A code as typed: case and separators do not matter, as on the deck. */
    fun normalizeCode(code: String): String =
        code.filter { it in 'a'..'z' || it in 'A'..'Z' || it in '0'..'9' }.uppercase()

    /**
     * The schemes a pairing code may use. Desktops write the one from before
     * the rename to Cepheus, which phones with an earlier app still need.
     */
    private val PREFIXES = listOf("cepheus://pair?", "agentdeck://pair?") // legacy-name

    fun fromUri(text: String): Result<PairingTarget> {
        val trimmed = text.trim()
        val prefix = PREFIXES.firstOrNull { trimmed.startsWith(it, ignoreCase = true) }
            ?: return Result.failure(IllegalArgumentException("That is not a Cepheus pairing code."))
        val fields = trimmed.substring(prefix.length).split('&').mapNotNull { part ->
            val eq = part.indexOf('=')
            if (eq <= 0) null else part.substring(0, eq) to decode(part.substring(eq + 1))
        }.toMap()
        return fromFields(
            host = fields["host"].orEmpty(),
            port = fields["port"].orEmpty(),
            fingerprint = fields["fp"].orEmpty(),
            code = fields["code"].orEmpty(),
        )
    }

    fun fromFields(host: String, port: String, fingerprint: String, code: String): Result<PairingTarget> {
        val h = host.trim().removePrefix("[").removeSuffix("]")
        if (h.isEmpty() || h.any { it.isWhitespace() || it == '/' || it == '?' || it == '#' || it == '@' }) {
            return bad("Enter the deck's address, such as 192.0.2.10.")
        }
        val p = port.trim().toIntOrNull()
        if (p == null || p !in 1..65535) return bad("Enter the port, a number up to 65535.")
        val fp = Pin.normalize(fingerprint) ?: return bad("The certificate SHA-256 is 64 hexadecimal digits.")
        val c = normalizeCode(code)
        if (c.length != CODE_LENGTH) return bad("The code is 8 letters and digits, such as ABCD-2345.")
        return Result.success(PairingTarget(h, p, fp, c))
    }

    private fun decode(value: String): String = try {
        URLDecoder.decode(value, "UTF-8")
    } catch (_: IllegalArgumentException) {
        value
    }

    private fun bad(message: String): Result<PairingTarget> = Result.failure(IllegalArgumentException(message))
}
