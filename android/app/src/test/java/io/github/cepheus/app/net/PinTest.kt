package io.github.cepheus.app.net

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import java.lang.reflect.Proxy
import java.security.cert.CertificateException
import java.security.cert.CertificateFactory
import java.security.cert.X509Certificate
import javax.net.ssl.SSLPeerUnverifiedException
import javax.net.ssl.SSLSession

class PinTest {
    // Public test certificates (no keys); their DER SHA-256 from `openssl x509 -outform DER | sha256sum`.
    private val deck = cert("pin/deck-cert.pem")
    private val other = cert("pin/other-cert.pem")
    private val deckPin = "b7985a3fbf315088366039658b1be044cd42acdddbad163cd48440d2bfa6ac54"
    private val otherPin = "193f283c5f9ab4d5286944d937c4ee78540a23a5ecbbcf033ef0ad5acf703764"

    @Test
    fun fingerprintIsSha256OfTheCertificateDer() {
        assertEquals(deckPin, Pin.sha256Hex(deck.encoded))
        assertEquals(otherPin, Pin.sha256Hex(other.encoded))
    }

    @Test
    fun trustManagerAcceptsOnlyThePinnedCertificate() {
        val trust = PinnedTrustManager(deckPin)
        trust.checkServerTrusted(arrayOf(deck), "ECDHE_ECDSA")
        assertThrows(CertificateException::class.java) { trust.checkServerTrusted(arrayOf(other), "ECDHE_ECDSA") }
        // A chain whose leaf is another certificate fails even when the pinned one follows it.
        assertThrows(CertificateException::class.java) { trust.checkServerTrusted(arrayOf(other, deck), "ECDHE_ECDSA") }
        assertThrows(CertificateException::class.java) { trust.checkServerTrusted(emptyArray(), "ECDHE_ECDSA") }
        assertThrows(CertificateException::class.java) { trust.checkServerTrusted(null, "ECDHE_ECDSA") }
    }

    @Test
    fun trustManagerTrustsNoClientsAndNoIssuers() {
        val trust = PinnedTrustManager(deckPin)
        assertThrows(CertificateException::class.java) { trust.checkClientTrusted(arrayOf(deck), "ECDHE_ECDSA") }
        assertEquals(0, trust.acceptedIssuers.size)
    }

    @Test
    fun hostnameVerifierChecksThePeerAgainstThePin() {
        val verifier = PinnedHostnameVerifier(deckPin)
        assertTrue(verifier.verify("192.0.2.10", session(deck)))
        assertFalse(verifier.verify("192.0.2.10", session(other)))
        assertFalse(verifier.verify("192.0.2.10", session(null)))
        assertFalse(verifier.verify("192.0.2.10", null))
    }

    @Test
    fun typedFingerprintsAreNormalized() {
        val grouped = deckPin.uppercase().chunked(2).joinToString(":")
        assertEquals(deckPin, Pin.normalize(grouped))
        assertEquals(deckPin, Pin.normalize(deckPin.chunked(4).joinToString(" ")))
        assertNull(Pin.normalize(deckPin.dropLast(1)))
        assertNull(Pin.normalize(deckPin.dropLast(1) + "g"))
    }

    private fun cert(resource: String): X509Certificate {
        val stream = javaClass.classLoader!!.getResourceAsStream(resource) ?: error("missing $resource")
        return stream.use { CertificateFactory.getInstance("X.509").generateCertificate(it) as X509Certificate }
    }

    /** A TLS session whose peer is `peer`, or unverified when null. */
    private fun session(peer: X509Certificate?): SSLSession =
        Proxy.newProxyInstance(javaClass.classLoader, arrayOf(SSLSession::class.java)) { _, method, _ ->
            when (method.name) {
                "getPeerCertificates" -> peer?.let { arrayOf(it) } ?: throw SSLPeerUnverifiedException("none")
                else -> null
            }
        } as SSLSession
}
