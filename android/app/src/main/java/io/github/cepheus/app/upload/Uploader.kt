package io.github.cepheus.app.upload

import io.github.cepheus.app.net.DeckException
import io.github.cepheus.app.protocol.Requests
import io.github.cepheus.app.protocol.ServerMessage
import java.io.InputStream
import java.security.MessageDigest

/** The gateway requests an upload needs; the app's are [io.github.cepheus.app.net.GatewayClient]'s. */
interface UploadLink {
    suspend fun request(build: (Long) -> String): ServerMessage

    /**
     * Build and send a binary frame before returning, after every frame sent
     * before it; its answer comes from [Sent.reply], within the request
     * deadline counted from now.
     */
    fun sendBinary(build: (Long) -> ByteArray): Sent
}

/** A request on its way: its answer, or [DeckException]. */
fun interface Sent {
    suspend fun reply(): ServerMessage
}

/** A file to attach, opened twice: once for its checksum, once to send it. */
class Source(val name: String, val open: () -> InputStream)

/** A file the deck saved whole: its token for `send`, and where it lies on the desktop. */
data class Uploaded(val token: String, val path: String, val size: Long)

/**
 * Sends one file to the deck's uploads folder for a session, in pieces over
 * the open connection (cepheus/src/remote/uploads.rs keeps them). The
 * deck checks the whole file against the checksum sent first, so a file
 * that changed or arrived short is refused rather than handed to the agent.
 */
object Uploader {
    const val FILE_MAX = 25L * 1024 * 1024











    const val PIECE = 32 * 1024
    const val IN_FLIGHT = 2

    suspend fun upload(link: UploadLink, id: Long, source: Source, progress: (Long) -> Unit = {}): Uploaded {
        val (sha256, size) = source.open().use { digest(it, source.name) }
        val start = link.request { Requests.upload(it, id, source.name, size, sha256) } as? ServerMessage.Ok
        val token = start?.upload ?: throw DeckException("The deck did not start the upload.")
        if (size == 0L) return Uploaded(token, start.path ?: throw DeckException("The deck did not save ${source.name}."), 0)
        var path: String? = null
        var offset = 0L
        source.open().use { input ->
            // Pieces sent and not yet answered, oldest first, with where each ends.
            val sent = ArrayDeque<Pair<Sent, Long>>()

            suspend fun settle() {
                val (piece, end) = sent.removeFirst()
                path = (piece.reply() as? ServerMessage.Ok)?.path
                progress(end)
            }

            val buffer = ByteArray(PIECE)
            while (offset < size) {
                val n = fill(input, buffer, minOf(PIECE.toLong(), size - offset).toInt())
                if (n == 0) break
                val at = offset
                // sendBinary builds the frame before it returns, so the buffer is free again.
                sent += link.sendBinary { Requests.piece(it, token, at, buffer, 0, n) } to at + n
                offset += n
                if (sent.size == IN_FLIGHT) settle()
            }
            while (sent.isNotEmpty()) settle()
            if (offset != size || input.read() != -1) throw DeckException("${source.name} changed while it was sent; attach it again.")
        }
        return Uploaded(token, path ?: throw DeckException("The deck did not save ${source.name}."), size)
    }

    /** The file's SHA-256 as hex and its size; refused once it passes [FILE_MAX]. */
    private fun digest(input: InputStream, name: String): Pair<String, Long> {
        val sha = MessageDigest.getInstance("SHA-256")
        val buffer = ByteArray(64 * 1024)
        var size = 0L
        while (true) {
            val n = input.read(buffer)
            if (n < 0) break
            size += n
            if (size > FILE_MAX) throw DeckException("$name is over 25 MB, the most a file may be.")
            sha.update(buffer, 0, n)
        }
        return sha.digest().joinToString("") { "%02x".format(it) } to size
    }

    /** Read up to `want` bytes, fewer only at the end of the stream. */
    private fun fill(input: InputStream, buffer: ByteArray, want: Int): Int {
        var got = 0
        while (got < want) {
            val n = input.read(buffer, got, want - got)
            if (n < 0) break
            got += n
        }
        return got
    }
}

/** A file on the composer, from picking it until the message goes. */
data class Attachment(val key: Long, val name: String, val state: State) {
    sealed interface State {
        data class Sending(val sent: Long, val size: Long?) : State
        data class Ready(val token: String, val path: String, val size: Long = 0) : State
        data class Failed(val error: String) : State
    }

    companion object {
        const val FILES_MAX = 10

        /** The tokens to send with the message; null while a file is still on its way or failed. */
        fun tokens(list: List<Attachment>): List<String>? =
            list.map { (it.state as? State.Ready)?.token ?: return null }

        /** Why `adding` more files cannot join the `count` already attached, or null. */
        fun refusal(count: Int, adding: Int): String? =
            if (count + adding > FILES_MAX) "A message can carry at most $FILES_MAX files." else null
    }
}
