package io.github.cepheus.app.protocol

import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.nio.ByteBuffer

/** The requests as cepheus/src/remote/wire.rs deserializes them. */
class RequestsTest {
    @Test
    fun searchSendsTheQueryAsTyped() {
        val search = JSONObject(Requests.search(5, "agy  agents"))
        assertEquals("search", search.getString("t"))
        assertEquals(5L, search.getLong("rid"))
        assertEquals("agy  agents", search.getString("q"))
    }

    @Test
    fun answerEchoesThePromptsInputsAndFingerprint() {
        val update = Protocol.parse(
            """{"t":"prompt","id":3,"inputs":41,"fp":"9f3a0c","prompt":{"kind":"question","question":"Which?",""" +
                """"options":[{"label":"Red"},{"label":"Type something."}],"selected":0,"free_text":1,"multi":false}}""",
        ) as ServerMessage.PromptUpdate
        val waiting = update.waiting!!

        val choice = JSONObject(Requests.answer(9, waiting, 0, null))
        assertEquals("answer", choice.getString("t"))
        assertEquals(9L, choice.getLong("rid"))
        assertEquals(3L, choice.getLong("id"))
        assertEquals(41L, choice.getLong("inputs"))
        assertEquals("9f3a0c", choice.getString("fp"))
        assertEquals(0, choice.getInt("choice"))
        assertTrue(choice.isNull("text"))

        val typed = JSONObject(Requests.answer(10, waiting, 1, "Green"))
        assertEquals(41L, typed.getLong("inputs"))
        assertEquals("9f3a0c", typed.getString("fp"))
        assertEquals(1, typed.getInt("choice"))
        assertEquals("Green", typed.getString("text"))
        assertTrue("a single choice sends no choices", typed.isNull("choices"))

        // A several-choice question sends every box to leave ticked.
        val several = JSONObject(Requests.answer(11, waiting, 0, "mango", listOf(0, 3)))
        assertEquals("[0,3]", several.getJSONArray("choices").toString())
        assertEquals("mango", several.getString("text"))
    }

    @Test
    fun authCarriesTheLastAlertHeld() {
        val first = JSONObject(Requests.auth("d-1", "k", null, null))
        assertEquals("auth", first.getString("t"))
        assertEquals("d-1", first.getString("device"))
        assertEquals("k", first.getString("key"))
        assertTrue(first.isNull("last_seq"))
        assertTrue(first.isNull("epoch"))
        val later = JSONObject(Requests.auth("d-1", "k", 12, "e1"))
        assertEquals(12L, later.getLong("last_seq"))
        assertEquals("e1", later.getString("epoch"))
    }

    @Test
    fun pairSendsTheCodeAndName() {
        val pair = JSONObject(Requests.pair("ABCD2345", "Pixel 9"))
        val fields = pair.keys().asSequence().associateWith { pair.get(it) }
        assertEquals(mapOf("t" to "pair", "code" to "ABCD2345", "name" to "Pixel 9"), fields)
    }

    @Test
    fun everyRequestCarriesItsRid() {
        val requests = mapOf(
            "ping" to Requests.ping(1),
            "models" to Requests.models(2),
            "open" to Requests.open(3, 7, 60),
            "chat" to Requests.chat(4, 7, 40, 60),
            "send" to Requests.send(5, 7, "hello"),
            "stop" to Requests.stop(6, 7),
            "new" to Requests.new(7, "codex", "gpt-5.5", "high", "go"),
            "model" to Requests.model(8, 7, "opus", null),
            "close" to Requests.close(9, 7),
            "reopen" to Requests.reopen(10, 7),
            "clear_errors" to Requests.clearErrors(11),
            "reorder" to Requests.reorder(12, listOf(3, 1, 2)),
            "stats" to Requests.stats(13),
            "upload" to Requests.upload(14, 7, "photo.jpg", 10, "ab".repeat(32)),
            "commands" to Requests.commands(15, 7),
            "send_now" to Requests.sendNow(16, 7),
        )
        requests.entries.forEachIndexed { i, (t, text) ->
            val o = JSONObject(text)
            assertEquals(t, o.getString("t"))
            assertEquals(i + 1L, o.getLong("rid"))
        }
    }

    @Test
    fun requestFields() {
        val open = JSONObject(Requests.open(1, null, 60))
        assertTrue(open.isNull("id"))
        assertEquals(60, open.getInt("limit"))

        val chat = JSONObject(Requests.chat(1, 7, 40, 60))
        assertEquals(7L, chat.getLong("id"))
        assertEquals(40, chat.getInt("before"))

        val new = JSONObject(Requests.new(1, "claude", null, null, "  "))
        assertEquals("claude", new.getString("tool"))
        assertTrue(new.isNull("model"))
        assertTrue(new.isNull("effort"))
        assertTrue(new.isNull("text"))
        assertFalse("the desktop is the default: no host", new.has("host"))
        assertEquals("fedora", JSONObject(Requests.new(1, "claude", null, null, null, "fedora")).getString("host"))

        val model = JSONObject(Requests.model(1, 7, null, "xhigh"))
        assertTrue(model.isNull("model"))
        assertEquals("xhigh", model.getString("effort"))

        val send = JSONObject(Requests.send(1, 7, "line one\nline two"))
        assertEquals("line one\nline two", send.getString("text"))
        assertEquals(0, send.getJSONArray("files").length())

        val attached = JSONObject(Requests.send(1, 7, "look", listOf("u-1", "u-2")))
        assertEquals(listOf("u-1", "u-2"), attached.getJSONArray("files").let { a -> List(a.length()) { a.getString(it) } })

        val reorder = JSONObject(Requests.reorder(1, listOf(3, 1, 2)))
        assertEquals(listOf(3L, 1L, 2L), reorder.getJSONArray("ids").let { a -> List(a.length()) { a.getLong(it) } })

        val upload = JSONObject(Requests.upload(1, 7, "photo.jpg", 26_214_400, "ab".repeat(32)))
        assertEquals(7L, upload.getLong("id"))
        assertEquals("photo.jpg", upload.getString("name"))
        assertEquals(26_214_400L, upload.getLong("size"))
        assertEquals("ab".repeat(32), upload.getString("sha256"))
    }

    /** The resume list and a resume, as wire.rs `Req::Conversations` and `Req::Resume` read them. */
    @Test
    fun conversationsAndResume() {
        val list = JSONObject(Requests.conversations(4))
        assertEquals("conversations", list.getString("t"))
        assertEquals(4L, list.getLong("rid"))
        assertEquals(2, list.length())

        val resume = JSONObject(Requests.resume(5, "opencode", "ses_ead42ac29ffei5Xcr4QiFwE9uu"))
        assertEquals("resume", resume.getString("t"))
        assertEquals(5L, resume.getLong("rid"))
        assertEquals("opencode", resume.getString("tool"))
        assertEquals("ses_ead42ac29ffei5Xcr4QiFwE9uu", resume.getString("conversation"))
    }

    /** A piece as cepheus/src/remote/wire.rs `piece_frame` reads it. */
    @Test
    fun aPieceIsItsHeadersLengthThenTheHeaderThenTheBytes() {
        val data = byteArrayOf(0, 1, 2, 3, 4, 5, 6, 7)
        val frame = Requests.piece(21, "u-0a1b", 262_144, data, 2, 4)
        val length = ByteBuffer.wrap(frame, 0, 4).int
        val header = JSONObject(String(frame, 4, length, Charsets.UTF_8))
        assertEquals("chunk", header.getString("t"))
        assertEquals(21L, header.getLong("rid"))
        assertEquals("u-0a1b", header.getString("upload"))
        assertEquals(262_144L, header.getLong("offset"))
        assertArrayEquals(byteArrayOf(2, 3, 4, 5), frame.copyOfRange(4 + length, frame.size))
    }
}
