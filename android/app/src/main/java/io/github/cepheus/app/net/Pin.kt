package io.github.cepheus.app.net

import java.security.MessageDigest
import java.security.cert.CertificateException
import java.security.cert.X509Certificate
import javax.net.ssl.HostnameVerifier
import javax.net.ssl.SSLPeerUnverifiedException
import javax.net.ssl.SSLSession
import javax.net.ssl.X509TrustManager

/**
 * The deck's gateway has a self-signed certificate; pairing hands the phone its
 * SHA-256, and that certificate alone is trusted (cepheus/README.md, "Pairing").
 */
object Pin {
    private val HEX = Regex("^[0-9a-f]{64}$")

    fun sha256Hex(bytes: ByteArray): String =
        MessageDigest.getInstance("SHA-256").digest(bytes).joinToString("") { "%02x".format(it) }

    /** A fingerprint as typed: case, colons and spaces aside. Null unless it is 64 hex digits. */
    fun normalize(fingerprint: String): String? =
        fingerprint.filterNot { it == ':' || it.isWhitespace() }.lowercase().takeIf { HEX.matches(it) }

    fun matches(certificate: X509Certificate, pinned: String): Boolean =
        sha256Hex(certificate.encoded) == pinned
}

/** Trusts exactly the server certificate whose DER SHA-256 is [pinned]; no CA is consulted. */
class PinnedTrustManager(private val pinned: String) : X509TrustManager {
    override fun checkServerTrusted(chain: Array<out X509Certificate>?, authType: String?) {
        val leaf = chain?.firstOrNull() ?: throw CertificateException("the deck sent no certificate")
        if (!Pin.matches(leaf, pinned)) {
            throw CertificateException("the deck's certificate is not the one paired")
        }
    }

    override fun checkClientTrusted(chain: Array<out X509Certificate>?, authType: String?) {
        throw CertificateException("client certificates are not used")
    }

    override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
}

/**
 * The deck is reached by address and its certificate names no host, so the
 * host check is the pin itself, checked again on the session's peer.
 */
class PinnedHostnameVerifier(private val pinned: String) : HostnameVerifier {
    override fun verify(hostname: String?, session: SSLSession?): Boolean {
        val leaf = try {
            session?.peerCertificates?.firstOrNull() as? X509Certificate
        } catch (_: SSLPeerUnverifiedException) {
            null
        }
        return leaf != null && Pin.matches(leaf, pinned)
    }
}
