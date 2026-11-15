package io.github.cepheus.app.upload

import io.github.cepheus.app.net.DeckException
import io.github.cepheus.app.protocol.ServerMessage
import kotlinx.coroutines.runBlocking
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.InputStream
import java.nio.ByteBuffer
import java.security.MessageDigest





class UploaderTest {
    /** A gateway that keeps what it was sent and makes the file whole once every byte arrived. */
    private class Gateway(private val size: Long, private val refusePiece: String? = null) : UploadLink {
        val requests = mutableListOf<JSONObject>()
        val pieces = mutableListOf<Pair<JSONObject, ByteArray>>()
        private var rid = 0L

        override suspend fun request(build: (Long) -> String): ServerMessage {
            requests += JSONObject(build(++rid))
            return ServerMessage.Ok(rid, null, upload = "u-1", path = if (size == 0L) "/u/7/empty.txt" else null)
        }

        override fun sendBinary(build: (Long) -> ByteArray): Sent {
            val frame = build(++rid)
            val length = ByteBuffer.wrap(frame, 0, 4).int
            pieces += JSONObject(String(frame, 4, length, Charsets.UTF_8)) to frame.copyOfRange(4 + length, frame.size)
            val arrived = pieces.sumOf { it.second.size.toLong() }
            val answer = ServerMessage.Ok(rid, null, path = if (arrived == size) "/u/7/a.bin" else null)
            return Sent {
                refusePiece?.let { throw DeckException(it) }
                answer
            }
        }
    }

    /**
     * An uplink of `rate` bytes a second, in virtual seconds ([now]): it
     * carries frames in the order sent, the deck answers each `rtt` after its
     * last byte, and an answer later than `deadline` after its request was
     * sent fails as [io.github.cepheus.app.net.GatewayClient]'s 30
     * seconds do.
     */
    private class SlowLink(
        private val size: Long,
        private val rate: Double,
        private val rtt: Double = 0.1,
        private val deadline: Double = 30.0,
    ) : UploadLink {
        var now = 0.0
            private set
        private var free = 0.0
        private var rid = 0L
        private val arrived = java.io.ByteArrayOutputStream()

        val received: ByteArray get() = arrived.toByteArray()

        override suspend fun request(build: (Long) -> String): ServerMessage {
            build(++rid)
            now += rtt
            return ServerMessage.Ok(rid, null, upload = "u-1")
        }

        override fun sendBinary(build: (Long) -> ByteArray): Sent {
            val answer = carry(build(++rid))
            return Sent { answer() }
        }

        /** Put `frame` on the uplink now; its answer, taken when the uploader waits for it. */
        fun carry(frame: ByteArray): () -> ServerMessage {
            val sentAt = now
            free = maxOf(free, now) + frame.size / rate
            val answeredAt = free + rtt
            val header = ByteBuffer.wrap(frame, 0, 4).int
            arrived.write(frame, 4 + header, frame.size - 4 - header)
            val whole = arrived.size().toLong() == size
            val rid = JSONObject(String(frame, 4, header, Charsets.UTF_8)).getLong("rid")
            return {
                if (answeredAt - sentAt > deadline) {
                    now = maxOf(now, sentAt + deadline)
                    throw DeckException("The deck did not answer in time.")
                }
                now = maxOf(now, answeredAt)
                ServerMessage.Ok(rid, null, path = if (whole) "/u/7/Screenshot.jpg" else null)
            }
        }
    }








    @Test
    fun aScreenshotCrossesASlowMobileUplinkAndShowsItsProgress() {
        val data = ByteArray(450_000) { (it % 251).toByte() }
        val link = SlowLink(data.size.toLong(), rate = 6_000.0)
        val progress = mutableListOf<Pair<Double, Long>>()
        val sent = runBlocking {
            Uploader.upload(link, 7, Source("Screenshot.jpg") { ByteArrayInputStream(data) }) { progress += link.now to it }
        }
        assertEquals(Uploaded("u-1", "/u/7/Screenshot.jpg", 450_000), sent)
        assertArrayEquals(data, link.received)
        assertEquals(450_000L, progress.last().second)
        // The chip moves at least every 10 seconds while the file goes.
        val times = listOf(0.0) + progress.map { it.first }
        val longest = times.zipWithNext { a, b -> b - a }.max()
        assertTrue("the chip stood still for $longest s", longest < 10.0)
    }

    private fun hex(data: ByteArray) =
        MessageDigest.getInstance("SHA-256").digest(data).joinToString("") { "%02x".format(it) }

    @Test
    fun aFileGoesInPiecesAfterItsChecksum() {
        val data = ByteArray(600_000) { (it % 251).toByte() }
        val gateway = Gateway(data.size.toLong())
        val progress = mutableListOf<Long>()
        val sent = runBlocking {
            Uploader.upload(gateway, 7, Source("a.bin") { ByteArrayInputStream(data) }) { progress += it }
        }
        assertEquals(Uploaded("u-1", "/u/7/a.bin", 600_000), sent)
        val start = gateway.requests.single()
        assertEquals("upload", start.getString("t"))
        assertEquals(7L, start.getLong("id"))
        assertEquals("a.bin", start.getString("name"))
        assertEquals(600_000L, start.getLong("size"))
        assertEquals(hex(data), start.getString("sha256"))
        // 32 KiB pieces: each crosses a slow uplink well within its deadline.
        val ends = (1L..18L).map { it * 32_768L } + 600_000L
        assertEquals(listOf(0L) + ends.dropLast(1), gateway.pieces.map { it.first.getLong("offset") })
        assertTrue(gateway.pieces.all { it.first.getString("upload") == "u-1" })
        assertArrayEquals(data, gateway.pieces.fold(ByteArray(0)) { all, piece -> all + piece.second })
        assertEquals(ends, progress)
    }

    /** Small pieces would cost a round trip each; two on their way keep a fast uplink busy. */
    @Test
    fun aFileOnAFastLinkKeepsTwoPiecesOnTheirWay() {
        val data = ByteArray(600_000) { (it % 251).toByte() }
        val link = SlowLink(data.size.toLong(), rate = 1_000_000.0, rtt = 0.1)
        runBlocking { Uploader.upload(link, 7, Source("a.bin") { ByteArrayInputStream(data) }) }
        assertArrayEquals(data, link.received)
        // One piece at a time takes 19 round trips, 2.6 s; two at a time, 1.4 s.
        assertTrue("took ${link.now} s", link.now < 1.6)
    }

    @Test
    fun anEmptyFileIsWholeAtOnce() {
        val gateway = Gateway(0)
        val sent = runBlocking { Uploader.upload(gateway, 7, Source("empty.txt") { ByteArrayInputStream(ByteArray(0)) }) }
        assertEquals(Uploaded("u-1", "/u/7/empty.txt", 0), sent)
        assertEquals(emptyList<Pair<JSONObject, ByteArray>>(), gateway.pieces)
    }

    @Test
    fun aFileOverTheLimitIsRefusedBeforeAnythingIsSent() {
        val gateway = Gateway(Uploader.FILE_MAX + 1)
        val error = failure { Uploader.upload(gateway, 7, Source("big.mov") { Zeros(Uploader.FILE_MAX + 1) }) }
        assertEquals("big.mov is over 25 MB, the most a file may be.", error)
        assertEquals(emptyList<JSONObject>(), gateway.requests)
    }

    @Test
    fun aPieceTheDeckRefusesFailsTheFile() {
        val data = ByteArray(1000) { 1 }
        val gateway = Gateway(1000, refusePiece = "the file did not arrive whole (checksum mismatch); send it again")
        val error = failure { Uploader.upload(gateway, 7, Source("a.bin") { ByteArrayInputStream(data) }) }
        assertEquals("the file did not arrive whole (checksum mismatch); send it again", error)
    }

    @Test
    fun aFileThatChangesWhileSentFails() {
        var reads = 0
        // The checksum pass reads 1000 bytes; the second read finds 10 fewer.
        val source = Source("a.bin") { ByteArrayInputStream(ByteArray(if (reads++ == 0) 1000 else 990)) }
        val error = failure { Uploader.upload(Gateway(1000), 7, source) }
        assertEquals("a.bin changed while it was sent; attach it again.", error)
    }

    @Test
    fun aMessageGoesOnlyWhenEveryFileArrived() {
        val ready = Attachment(1, "a.png", Attachment.State.Ready("u-1", "/u/7/a.png"))
        val other = Attachment(2, "b.pdf", Attachment.State.Ready("u-2", "/u/7/b.pdf"))
        assertEquals(listOf("u-1", "u-2"), Attachment.tokens(listOf(ready, other)))
        assertEquals(emptyList<String>(), Attachment.tokens(emptyList()))
        assertNull(Attachment.tokens(listOf(ready, Attachment(3, "c", Attachment.State.Sending(10, 100)))))
        assertNull(Attachment.tokens(listOf(ready, Attachment(4, "d", Attachment.State.Failed("no route")))))
    }

    private fun failure(block: suspend () -> Unit): String? = try {
        runBlocking { block() }
        fail("the upload succeeded")
        null
    } catch (e: DeckException) {
        e.message
    }

    private class Zeros(private var left: Long) : InputStream() {
        override fun read(): Int = if (left-- > 0) 0 else -1
        override fun read(b: ByteArray, off: Int, len: Int): Int {
            if (left <= 0) return -1
            val n = minOf(len.toLong(), left).toInt()
            b.fill(0, off, off + n)
            left -= n
            return n
        }
    }
}
