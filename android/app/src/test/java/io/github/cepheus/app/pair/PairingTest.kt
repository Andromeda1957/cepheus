package io.github.cepheus.app.pair

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class PairingTest {
    private val fp = "b7985a3fbf315088366039658b1be044cd42acdddbad163cd48440d2bfa6ac54"

    @Test
    fun readsTheDecksQrPayload() {
        // As cepheus/src/pairing.rs `payload` writes it.
        val target = Pairing.fromUri("agentdeck://pair?host=192.0.2.10&port=8765&fp=$fp&code=ABCD2345").getOrThrow() // legacy-name
        assertEquals(PairingTarget("192.0.2.10", 8765, fp, "ABCD2345"), target)
        assertEquals("wss://192.0.2.10:8765/v1", target.url)
    }

    @Test
    fun readsAnIpv6Host() {
        val target = Pairing.fromUri("cepheus://pair?host=[fd00::2]&port=8765&fp=$fp&code=ABCD2345").getOrThrow()
        assertEquals("fd00::2", target.host)
        assertEquals("wss://[fd00::2]:8765/v1", target.url)
    }

    @Test
    fun typedDetailsMayUseADashAnyCaseAndSpaces() {
        val target = Pairing.fromFields(" 192.0.2.10 ", "8765", fp.uppercase().chunked(4).joinToString(" "), "abcd-2345").getOrThrow()
        assertEquals(PairingTarget("192.0.2.10", 8765, fp, "ABCD2345"), target)
        assertEquals("ABCD2345", Pairing.normalizeCode("ab cd-23_45"))
    }

    @Test
    fun refusesWhatIsNotAPairingCode() {
        assertTrue(Pairing.fromUri("https://example.com/?host=1.2.3.4").isFailure)
        assertTrue(Pairing.fromUri("cepheus://pair?host=10.0.0.1&port=8765&code=ABCD2345").isFailure)
    }

    @Test
    fun refusesBadFields() {
        assertTrue(Pairing.fromFields("", "8765", fp, "ABCD2345").isFailure)
        assertTrue(Pairing.fromFields("10.0.0.1/x", "8765", fp, "ABCD2345").isFailure)
        assertTrue(Pairing.fromFields("10.0.0.1", "0", fp, "ABCD2345").isFailure)
        assertTrue(Pairing.fromFields("10.0.0.1", "70000", fp, "ABCD2345").isFailure)
        assertTrue(Pairing.fromFields("10.0.0.1", "8765", fp.drop(2), "ABCD2345").isFailure)
        assertTrue(Pairing.fromFields("10.0.0.1", "8765", fp, "ABCD-234").isFailure)
        assertTrue(Pairing.fromFields("10.0.0.1", "8765", fp, "ABCD-23456").isFailure)
    }
}
