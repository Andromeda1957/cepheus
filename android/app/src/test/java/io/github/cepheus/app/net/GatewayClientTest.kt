package io.github.cepheus.app.net

import io.github.cepheus.app.protocol.Protocol
import io.github.cepheus.app.protocol.Requests
import io.github.cepheus.app.protocol.ServerMessage
import io.github.cepheus.app.store.Credentials
import io.github.cepheus.app.upload.Sent
import io.github.cepheus.app.upload.Source
import io.github.cepheus.app.upload.UploadLink
import io.github.cepheus.app.upload.Uploaded
import io.github.cepheus.app.upload.Uploader
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.Closeable
import java.io.DataInputStream
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.Socket
import java.nio.ByteBuffer
import java.nio.file.Files
import java.security.KeyStore
import java.security.MessageDigest
import java.util.Base64
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import javax.net.ssl.KeyManagerFactory
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLServerSocket
import kotlin.concurrent.thread







class GatewayClientTest {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val decks = mutableListOf<SlowDeck>()
    private val links = CopyOnWriteArrayList<Link>()

    @After
    fun close() {
        scope.cancel()
        decks.forEach { it.close() }
    }

    /**
     * At 32 KiB a second a 256 KiB piece takes 8 seconds, past the 4 seconds a
     * request waits here, and the app's ping behind it went unanswered past
     * its 2.5 seconds, so the app dropped the connection mid-file.
     */
    @Test
    fun aFileOverASlowUplinkArrivesWholeOnItsOneConnection() {
        val deck = deck(rate = 32 * 1024)
        val client = connect(deck, GatewayClient.Timing(requestMs = 4_000, pingEveryMs = 700, pingTimeoutMs = 2_500))
        val data = ByteArray(192 * 1024) { (it % 251).toByte() }
        val sent = runBlocking {
            Uploader.upload(link(client), 7, Source("Screenshot.jpg") { ByteArrayInputStream(data) })
        }
        assertEquals(Uploaded("u-1", "/u/7/Screenshot.jpg", data.size.toLong()), sent)
        assertArrayEquals(data, deck.file())
        assertEquals(1, deck.connections.get())
    }

    /** The deck answers the ping late, behind what the phone sent first, but keeps talking meanwhile. */
    @Test
    fun aLateAnswerToThePingKeepsAConnectionTheDeckStillTalksOn() {
        val deck = deck(rate = 1 shl 20, pongDelayMs = 2_000, chatterEveryMs = 150)
        connect(deck, GatewayClient.Timing(pingEveryMs = 300, pingTimeoutMs = 600))
        assertTrue("the fixture did not attempt its delayed pong", deck.pongAttempted.await(10, TimeUnit.SECONDS))
        assertEquals(1, deck.connections.get())
        assertFalse("the app left a deck that was talking: $links", links.any { it is Link.Offline })
    }

    /** A connection that has gone quiet is still left and made again. */
    @Test
    fun aDeckThatFallsSilentIsLeft() {
        val deck = deck(rate = 1 shl 20, pongDelayMs = null)
        connect(deck, GatewayClient.Timing(pingEveryMs = 300, pingTimeoutMs = 600))
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(8)
        while (deck.connections.get() < 2 && System.nanoTime() < deadline) Thread.sleep(50)
        assertEquals(2, deck.connections.get())
    }

    /** A refused answer keeps the gateway's code, so the app need not read its words. */
    @Test fun aRefusalKeepsTheGatewaysCode() {
        val client = connect(deck(rate = 1 shl 20), GatewayClient.Timing())
        fun refusal(build: (Long) -> String) = runBlocking {
            try { client.request(build = build); null } catch (e: DeckException) { e }
        }
        val stale = refusal { Requests.questionReply(it, 7, "old:0", 0) }
        assertEquals(Protocol.QUESTION_STALE, stale?.code)
        assertEquals("this question is already answered or no longer belongs to this conversation", stale?.message)
        val other = refusal { Requests.stop(it, 7) }
        assertEquals("unexpected", other?.message)
        assertNull(other?.code)
    }

    @Test fun helloAndSavedStateCannotAuthorizeRequestsUntilFreshStateArrives() {
        val deck = SlowDeck(1 shl 20, 0, 0, autoState = false).also { decks += it }
        val received = LinkedBlockingQueue<ServerMessage>()
        val client = GatewayClient(scope,
            credentials = { Credentials("127.0.0.1", deck.port, deck.fingerprint, "phone", "key") },
            alertsHeld = { null to null }, onLink = { links += it }, onMessage = { received += it })
        client.start()
        assertTrue(received.poll(10, TimeUnit.SECONDS) is ServerMessage.Hello)
        assertFalse(client.online)
        runBlocking {
            try { client.request { io.github.cepheus.app.protocol.Requests.open(it, 7, 60) }; fail("hello authorized a cached route") }
            catch (_: DeckException) { }
        }
        deck.publishState(false)
        assertTrue(received.poll(10, TimeUnit.SECONDS) is ServerMessage.State)
        assertFalse(client.online)
        deck.publishState(true)
        assertTrue(received.poll(10, TimeUnit.SECONDS) is ServerMessage.State)
        assertTrue(client.online)
        client.stop()
        assertFalse(client.online)
        client.start()
        assertTrue(received.poll(10, TimeUnit.SECONDS) is ServerMessage.Hello)
        assertFalse("a replacement connection needs its own state", client.online)
        deck.publishState(true)
        assertTrue(received.poll(10, TimeUnit.SECONDS) is ServerMessage.State)
        assertTrue(client.online)
    }

    private fun deck(rate: Int, pongDelayMs: Long? = 0, chatterEveryMs: Long = 0) =
        SlowDeck(rate, pongDelayMs, chatterEveryMs).also { decks += it }

    private fun connect(deck: SlowDeck, timing: GatewayClient.Timing): GatewayClient {
        val online = CountDownLatch(1)
        val client = GatewayClient(
            scope = scope,
            credentials = { Credentials("127.0.0.1", deck.port, deck.fingerprint, "phone", "key") },
            alertsHeld = { null to null },
            onLink = {
                links += it
                if (it == Link.Online) online.countDown()
            },
            onMessage = {},
            timing = timing,
        )
        client.start()
        assertTrue("the app did not sign in: $links", online.await(10, TimeUnit.SECONDS))
        return client
    }

    /** As the app's own ([io.github.cepheus.app.Deck]). */
    private fun link(client: GatewayClient) = object : UploadLink {
        override suspend fun request(build: (Long) -> String): ServerMessage = client.request(build = build)
        override fun sendBinary(build: (Long) -> ByteArray): Sent = client.sendBinary(build = build)
    }

    /**
     * A gateway with a certificate of its own that reads at most `rate` bytes
     * a second from the phone. It answers a ping after `pongDelayMs` (never
     * when null) and sends a message every `chatterEveryMs` when that is set.
     */
    private class SlowDeck(private val rate: Int, private val pongDelayMs: Long?, chatterEveryMs: Long, private val autoState: Boolean = true) : Closeable {
        val connections = AtomicInteger()
        val pongAttempted = CountDownLatch(1)
        val port: Int
        val fingerprint: String
        private val dir: File = Files.createTempDirectory("cepheus-gateway").toFile()
        private val server: SSLServerSocket
        private val timers = Executors.newSingleThreadScheduledExecutor()
        private val sockets = CopyOnWriteArrayList<Socket>()
        private val received = ByteArrayOutputStream()
        private var size = -1L

        @Volatile
        private var current: OutputStream? = null

        init {
            val keys = selfSigned(dir)
            fingerprint = Pin.sha256Hex(keys.getCertificate("deck").encoded)
            val managers = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm())
                .apply { init(keys, PASS.toCharArray()) }.keyManagers
            val tls = SSLContext.getInstance("TLS").apply { init(managers, null, null) }
            server = tls.serverSocketFactory.createServerSocket() as SSLServerSocket
            // A small window, so the phone's bytes wait on the reader as on a slow link.
            server.receiveBufferSize = 4096
            server.bind(InetSocketAddress(InetAddress.getLoopbackAddress(), 0))
            port = server.localPort
            thread(isDaemon = true, name = "deck accept") { acceptAll() }
            if (chatterEveryMs > 0) {
                timers.scheduleWithFixedDelay(
                    { current?.let { text(it, JSONObject().put("t", "chatter")) } },
                    chatterEveryMs,
                    chatterEveryMs,
                    TimeUnit.MILLISECONDS,
                )
            }
        }

        fun file(): ByteArray = synchronized(received) { received.toByteArray() }

        fun publishState(ready: Boolean) = text(requireNotNull(current), JSONObject().put("t", "state")
            .put("ready", ready).put("sessions", org.json.JSONArray()))

        override fun close() {
            timers.shutdownNow()
            server.close()
            sockets.forEach { it.close() }
            dir.deleteRecursively()
        }

        private fun acceptAll() {
            while (true) {
                val socket = try {
                    server.accept()
                } catch (_: IOException) {
                    return
                }
                connections.incrementAndGet()
                sockets += socket
                thread(isDaemon = true, name = "deck connection") {
                    try {
                        serve(socket)
                    } catch (_: IOException) {
                    } finally {
                        socket.close()
                    }
                }
            }
        }

        private fun serve(socket: Socket) {
            val raw = socket.getInputStream()
            val out = socket.getOutputStream()
            val request = StringBuilder()
            while (!request.endsWith("\r\n\r\n")) {
                val b = raw.read()
                if (b < 0) return
                request.append(b.toChar())
            }
            val key = Regex("(?i)sec-websocket-key: *(\\S+)").find(request)!!.groupValues[1]
            val accept = Base64.getEncoder().encodeToString(
                MessageDigest.getInstance("SHA-1").digest((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").toByteArray()),
            )
            out.write(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: $accept\r\n\r\n"
                    .toByteArray(),
            )
            out.flush()
            current = out
            val input = DataInputStream(Slow(raw, rate))
            while (true) {
                val b0 = input.readUnsignedByte()
                val b1 = input.readUnsignedByte()
                val length = when (val n = b1 and 127) {
                    126 -> input.readUnsignedShort()
                    127 -> input.readLong().toInt()
                    else -> n
                }
                val mask = ByteArray(4).also { if (b1 and 128 != 0) input.readFully(it) }
                val payload = ByteArray(length).also { input.readFully(it) }
                if (b1 and 128 != 0) payload.indices.forEach { payload[it] = (payload[it].toInt() xor mask[it % 4].toInt()).toByte() }
                when (b0 and 15) {
                    1 -> answer(out, JSONObject(String(payload, Charsets.UTF_8)))
                    2 -> piece(out, payload)
                    8 -> return
                    9 -> frame(out, 0xA, payload)
                }
            }
        }

        private fun answer(out: OutputStream, request: JSONObject) {
            val rid = request.opt("rid")
            when (request.getString("t")) {
                "auth" -> {
                    text(out, JSONObject().put("t", "hello").put("epoch", "e-1").put("version", "test"))
                    if (autoState) publishState(true)
                }
                "ping" -> pongDelayMs?.let { delay ->
                    timers.schedule({
                        text(out, JSONObject().put("t", "pong").put("rid", rid))
                        pongAttempted.countDown()
                    }, delay, TimeUnit.MILLISECONDS)
                }
                "upload" -> {
                    size = request.getLong("size")
                    text(out, JSONObject().put("t", "ok").put("rid", rid).put("upload", "u-1"))
                }
                "question_reply" -> text(out, JSONObject().put("t", "err").put("rid", rid)
                    .put("error", "this question is already answered or no longer belongs to this conversation")
                    .put("code", Protocol.QUESTION_STALE))
                else -> text(out, JSONObject().put("t", "err").put("rid", rid).put("error", "unexpected"))
            }
        }

        private fun piece(out: OutputStream, frame: ByteArray) {
            val length = ByteBuffer.wrap(frame, 0, 4).int
            val header = JSONObject(String(frame, 4, length, Charsets.UTF_8))
            val whole = synchronized(received) {
                if (header.getLong("offset") != received.size().toLong()) {
                    text(out, JSONObject().put("t", "err").put("rid", header.get("rid")).put("error", "wrong offset"))
                    return
                }
                received.write(frame, 4 + length, frame.size - 4 - length)
                received.size().toLong() == size
            }
            val path: Any = if (whole) "/u/7/Screenshot.jpg" else JSONObject.NULL
            text(out, JSONObject().put("t", "ok").put("rid", header.get("rid")).put("path", path))
        }

        private fun text(out: OutputStream, message: JSONObject) = frame(out, 0x1, message.toString().toByteArray())

        private fun frame(out: OutputStream, opcode: Int, payload: ByteArray) {
            val head = ByteArrayOutputStream().apply {
                write(0x80 or opcode)
                if (payload.size < 126) {
                    write(payload.size)
                } else {
                    write(126)
                    write(payload.size shr 8)
                    write(payload.size and 255)
                }
            }
            try {
                synchronized(out) {
                    out.write(head.toByteArray())
                    out.write(payload)
                    out.flush()
                }
            } catch (_: IOException) {
            }
        }

        /** At most `rate` bytes a second, read 1 KiB at a time. */
        private class Slow(private val input: InputStream, private val rate: Int) : InputStream() {
            override fun read(): Int {
                val one = ByteArray(1)
                return if (read(one, 0, 1) < 0) -1 else one[0].toInt() and 255
            }

            override fun read(b: ByteArray, off: Int, len: Int): Int {
                val n = input.read(b, off, minOf(len, 1024))
                if (n > 0) Thread.sleep(n * 1000L / rate)
                return n
            }
        }

        companion object {
            const val PASS = "cepheus-test"

            /** A key and self-signed certificate made by the JDK's own keytool. */
            fun selfSigned(dir: File): KeyStore {
                val store = File(dir, "deck.p12")
                val keytool = File(System.getProperty("java.home"), "bin/keytool").path
                val process = ProcessBuilder(
                    keytool, "-genkeypair", "-alias", "deck", "-keyalg", "EC", "-groupname", "secp256r1",
                    "-dname", "CN=deck", "-validity", "2", "-storetype", "PKCS12", "-keystore", store.path,
                    "-storepass", PASS, "-keypass", PASS,
                ).redirectErrorStream(true).start()
                val output = process.inputStream.readBytes().decodeToString()
                check(process.waitFor() == 0) { "keytool failed: $output" }
                return KeyStore.getInstance("PKCS12").apply { store.inputStream().use { load(it, PASS.toCharArray()) } }
            }
        }
    }
}
