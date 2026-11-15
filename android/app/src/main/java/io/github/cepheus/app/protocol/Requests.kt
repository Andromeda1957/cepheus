package io.github.cepheus.app.protocol

import org.json.JSONArray
import org.json.JSONObject
import java.nio.ByteBuffer

/** The phone's messages, one JSON object per text frame. Each request carries `rid`. */
object Requests {
    /** The first message of a paired phone, with the last alert it holds. */
    fun auth(device: String, key: String, lastSeq: Long?, epoch: String?): String = JSONObject()
        .put("t", "auth")
        .put("device", device)
        .put("key", key)
        .put("last_seq", lastSeq ?: JSONObject.NULL)
        .put("epoch", epoch ?: JSONObject.NULL)
        .toString()

    /** The first message of a new phone, with the code the deck's pairing screen shows. */
    fun pair(code: String, name: String): String = JSONObject()
        .put("t", "pair")
        .put("code", code)
        .put("name", name)
        .toString()

    /** Deck controls stay separate from agent slash commands and cannot replace the envelope. */
    fun control(rid: Long, action: String, args: JSONObject = JSONObject()): String {
        require(action.isNotBlank())
        val request = request("control", rid).put("action", action)
        args.keys().forEach { key ->
            require(key !in setOf("t", "rid", "action")) { "Reserved control field: $key" }
            request.put(key, args.get(key))
        }
        return request.toString()
    }

    fun ping(rid: Long) = request("ping", rid).toString()

    fun commands(rid: Long, id: Long) = request("commands", rid).put("id", id).toString()

    fun models(rid: Long) = request("models", rid).toString()

    /** Follow a session's chat (or none); the reply is its newest page. */
    fun open(rid: Long, id: Long?, limit: Int) = request("open", rid)
        .put("id", id ?: JSONObject.NULL)
        .put("limit", limit)
        .toString()

    /** An older page, ending before item `before`. */
    fun chat(rid: Long, id: Long, before: Int, limit: Int) = request("chat", rid)
        .put("id", id)
        .put("before", before)
        .put("limit", limit)
        .toString()

    /** A message, and the uploads (their tokens) whose saved paths the deck types after it. */
    fun send(rid: Long, id: Long, text: String, files: List<String> = emptyList(), clientId: String? = null) = request("send", rid)
        .put("id", id)
        .put("text", text)
        .put("files", JSONArray(files))
        .putOpt("client_id", clientId)
        .toString()

    /** The chat menu's Clear: the host starts a fresh session natively and the reply names it. */
    fun clear(rid: Long, id: Long) = request("send", rid)
        .put("id", id)
        .put("text", "/clear")
        .put("relocate", true)
        .toString()

    /** Begin a file for session `id`; its pieces follow as [piece] frames. */
    fun upload(rid: Long, id: Long, name: String, size: Long, sha256: String) = request("upload", rid)
        .put("id", id)
        .put("name", name)
        .put("size", size)
        .put("sha256", sha256)
        .toString()

    /**
     * A binary frame with `length` bytes of `data` from `from`, at `offset` of
     * upload `upload`: the header's length (4 bytes, big-endian), the header
     * as JSON, then the bytes.
     */
    fun piece(rid: Long, upload: String, offset: Long, data: ByteArray, from: Int, length: Int): ByteArray {
        val header = request("chunk", rid).put("upload", upload).put("offset", offset).toString().toByteArray(Charsets.UTF_8)
        return ByteBuffer.allocate(4 + header.size + length).putInt(header.size).put(header).put(data, from, length).array()
    }

    /** The sessions in the order to keep, as the deck's Super+↑/↓ keeps it. */
    fun reorder(rid: Long, ids: List<Long>) = request("reorder", rid).put("ids", JSONArray(ids)).toString()

    fun stats(rid: Long) = request("stats", rid).toString()

    /**
     * Choose option `choice` of the prompt that waits. `inputs` and `fp` go back
     * exactly as received, so the deck presses keys only while that same prompt
     * still waits and nothing was typed since. A several-choice question
     * sends every box to leave ticked in `choices` instead, and the deck
     * ticks and unticks to match before it presses Submit.
     */
    fun answer(rid: Long, waiting: WaitingPrompt, choice: Int, text: String?, choices: List<Int> = emptyList()) =
        request("answer", rid)
            .put("id", waiting.id)
            .put("inputs", waiting.inputs)
            .put("fp", waiting.fp)
            .put("choice", choice)
            .put("text", text ?: JSONObject.NULL)
            .apply { if (choices.isNotEmpty()) put("choices", JSONArray(choices)) }
            .toString()

    fun questionReply(rid: Long, id: Long, question: String, choice: Int? = null, text: String? = null) = request("question_reply", rid)
        .put("id", id).put("question", question).putOpt("choice", choice).putOpt("text", text).toString()

    fun stop(rid: Long, id: Long) = request("stop", rid).put("id", id).toString()

    /** Claude Code's send now: the messages queued behind its running turn reach it now. */
    fun sendNow(rid: Long, id: Long) = request("send_now", rid).put("id", id).toString()

    /** Start a session; `host` names another machine the deck drives, null for the desktop. */
    fun new(rid: Long, tool: String, model: String?, effort: String?, text: String?, host: String? = null) =
        request("new", rid)
            .put("tool", tool)
            .put("model", model ?: JSONObject.NULL)
            .put("effort", effort ?: JSONObject.NULL)
            .put("text", text?.takeIf { it.isNotBlank() } ?: JSONObject.NULL)
            .apply { if (host != null) put("host", host) }
            .toString()

    fun model(rid: Long, id: Long, model: String?, effort: String?) = request("model", rid)
        .put("id", id)
        .put("model", model ?: JSONObject.NULL)
        .put("effort", effort ?: JSONObject.NULL)
        .toString()

    fun close(rid: Long, id: Long) = request("close", rid).put("id", id).toString()

    fun reopen(rid: Long, id: Long) = request("reopen", rid).put("id", id).toString()

    /** The earlier conversations the deck can resume, as its Alt+r lists them. */
    fun conversations(rid: Long) = request("conversations", rid).toString()

    /** Resume conversation `conversation` of `tool`; the reply names its session. */
    fun resume(rid: Long, tool: String, conversation: String, host: String? = null) = request("resume", rid)
        .put("tool", tool)
        .put("conversation", conversation)
        .apply { if (host != null) put("host", host) }
        .toString()

    /** The sessions whose conversation holds every word of `q`. */
    fun search(rid: Long, q: String) = request("search", rid).put("q", q).toString()

    /** A picture the owner sent in session `id`, by the key its `user` item carries. */
    fun image(rid: Long, id: Long, key: String) = request("image", rid).put("id", id).put("key", key).toString()

    fun clearErrors(rid: Long) = request("clear_errors", rid).toString()

    private fun request(t: String, rid: Long) = JSONObject().put("t", t).put("rid", rid)
}
