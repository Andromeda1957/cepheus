package io.github.cepheus.app.store

import android.content.Context
import io.github.cepheus.app.protocol.DeckState
import io.github.cepheus.app.protocol.Protocol
import io.github.cepheus.app.protocol.ServerMessage
import org.json.JSONException
import org.json.JSONObject
import java.io.File
import java.io.IOException

/**
 * The last session list and the last few chats, shown while the deck is out of
 * reach. Kept in the app's private no-backup storage.
 */
class Cache(context: Context, connectionId: String = Connections.LEGACY) {
    private val dir = File(context.noBackupFilesDir,
        if (connectionId == Connections.LEGACY) "deck-cache" else "deck-cache-$connectionId")
    private var forgotten = false

    fun saveState(raw: String) = write(File(dir, STATE), raw)

    fun loadState(): DeckState? =
        read(File(dir, STATE))?.let { (Protocol.parse(it) as? ServerMessage.State)?.state }

    fun saveChat(view: ChatView) {
        val kept = view.copy(
            start = view.start + (view.items.size - CHAT_ITEMS).coerceAtLeast(0),
            items = view.items.takeLast(CHAT_ITEMS),
        )
        write(File(dir, "chat-${view.id}.json"), kept.toJson().toString())
        val chats = dir.listFiles { f -> f.name.startsWith("chat-") }.orEmpty().sortedByDescending { it.lastModified() }
        chats.drop(CHATS).forEach { it.delete() }
    }

    fun loadChat(id: Long): ChatView? = read(File(dir, "chat-$id.json"))?.let {
        try {
            ChatView.fromJson(JSONObject(it))
        } catch (_: JSONException) {
            null
        }
    }

    @Synchronized
    fun clear() {
        forgotten = true
        dir.deleteRecursively()
    }

    @Synchronized
    private fun write(file: File, text: String) {
        if (forgotten) return
        try {
            dir.mkdirs()
            val tmp = File(dir, file.name + ".tmp")
            tmp.writeText(text)
            if (!tmp.renameTo(file)) tmp.delete()
        } catch (_: IOException) {
        }
    }

    private fun read(file: File): String? = try {
        if (file.exists()) file.readText() else null
    } catch (_: IOException) {
        null
    }

    private companion object {
        const val STATE = "state.json"
        const val CHAT_ITEMS = 200
        const val CHATS = 8
    }
}
