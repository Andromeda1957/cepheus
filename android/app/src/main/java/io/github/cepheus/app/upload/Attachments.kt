package io.github.cepheus.app.upload

import android.content.Context
import android.content.Intent
import io.github.cepheus.app.store.KeyValue
import org.json.JSONArray
import org.json.JSONObject
import android.net.Uri
import android.provider.OpenableColumns
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import io.github.cepheus.app.Deck
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import java.io.IOException








class Attachments(
    private val context: Context,
    private var sessionId: Long,
    private val storage: KeyValue,
    private val report: (String?) -> Unit,
) {
    private val scope: CoroutineScope get() = Deck.actions
    var list by mutableStateOf(listOf<Attachment>())
        private set
    private val sources = mutableMapOf<Long, Pair<Source, Long?>>()
    private val uris = mutableMapOf<Long, String>()
    private val jobs = mutableMapOf<Long, Job>()
    private var next = 1L
    private var generation = 0L

    init {
        val saved = runCatching { JSONArray(storage.getString("attachments/$sessionId")) }.getOrNull()
        if (saved != null) restore((0 until saved.length()).map { saved.getJSONObject(it).getString("name") },
            (0 until saved.length()).map { saved.getJSONObject(it).getString("uri") })
    }

    private fun save() = storage.putString("attachments/$sessionId", JSONArray().apply {
        list.forEach { put(JSONObject().put("name", it.name).put("uri", uris[it.key])) }
    }.toString())

    fun sourceUris(): List<String> = list.map { uris[it.key].orEmpty() }

    fun canRebind(id: Long): Boolean {
        if (id == sessionId) return true
        val saved = storage.getString("attachments/$id") ?: return true
        return runCatching { JSONArray(saved).length() == 0 }.getOrDefault(false)
    }

    /** Keep the original sources, but obtain tokens bound to the fresh session. */
    fun rebind(id: Long) {
        if (id == sessionId) return
        check(canRebind(id)) { "The fresh session already has saved attachments." }
        val previous = sessionId
        generation++
        jobs.values.forEach { it.cancel() }
        jobs.clear()
        sessionId = id
        save()
        storage.putString("attachments/$previous", null)
        sources.keys.toList().forEach(::start)
    }

    /** Attach picked files; why some were left out, or null. */
    fun add(uris: List<Uri>): String? {
        val refusal = Attachment.refusal(list.size, uris.size)
        for (uri in uris.take((Attachment.FILES_MAX - list.size).coerceAtLeast(0))) {
            runCatching { context.contentResolver.takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION) }
            val (name, size) = describe(uri)
            val key = next++
            this.uris[key] = uri.toString()
            sources[key] = Source(name) { context.contentResolver.openInputStream(uri) ?: throw IOException("it is gone") } to size
            list = list + Attachment(key, name, Attachment.State.Sending(0, size))
            start(key)
        }
        save()
        return refusal
    }

    fun restore(names: List<String>, originalUris: List<String>): String? {
        if (list.isNotEmpty()) return "Finish or remove the current attachments before recovering this message."
        if (names.size != originalUris.size) return "Attach the original files again. This message has been kept."
        names.indices.forEach { i ->
            val key = next++
            val uri = Uri.parse(originalUris[i])
            uris[key] = originalUris[i]
            sources[key] = Source(names[i]) { context.contentResolver.openInputStream(uri) ?: throw IOException("Choose this file again") } to null
            list = list + Attachment(key, names[i], Attachment.State.Sending(0, null))
            start(key)
        }
        save()
        return null
    }

    fun remove(key: Long) {
        jobs.remove(key)?.cancel()
        sources.remove(key)
        uris.remove(key)
        list = list.filterNot { it.key == key }
        save()
    }

    fun retry(key: Long) {
        if (list.any { it.key == key && it.state is Attachment.State.Failed }) start(key)
    }

    /** The message went: the files are the deck's now. */
    fun clear() {
        generation++
        jobs.values.forEach { it.cancel() }
        jobs.clear()
        sources.clear()
        uris.clear()
        list = emptyList()
        save()
    }

    private fun set(key: Long, state: Attachment.State) {
        list = list.map { if (it.key == key) it.copy(state = state) else it }
    }

    private fun start(key: Long) {
        val (source, size) = sources[key] ?: return
        val target = sessionId
        val expected = generation
        set(key, Attachment.State.Sending(0, size))
        jobs[key] = scope.launch {
            val result = Deck.upload(target, source) { sent ->
                scope.launch { if (generation == expected && sessionId == target && key in sources) set(key, Attachment.State.Sending(sent, size)) }
            }
            if (generation != expected || sessionId != target) return@launch
            jobs.remove(key)
            if (key !in sources) return@launch
            result.fold(
                onSuccess = { set(key, Attachment.State.Ready(it.token, it.path, it.size)) },
                onFailure = {
                    val error = it.message ?: "The upload failed."
                    set(key, Attachment.State.Failed(error))
                    report("${source.name}: $error")
                },
            )
        }
    }

    /** The file's name and size as the app that holds it tells them. */
    private fun describe(uri: Uri): Pair<String, Long?> {
        var name: String? = null
        var size: Long? = null
        try {
            context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)
                ?.use { c ->
                    if (c.moveToFirst()) {
                        if (!c.isNull(0)) name = c.getString(0)
                        if (!c.isNull(1)) size = c.getLong(1)
                    }
                }
        } catch (_: RuntimeException) {
        }
        return (name ?: uri.lastPathSegment?.substringAfterLast('/') ?: "file") to size
    }
}
