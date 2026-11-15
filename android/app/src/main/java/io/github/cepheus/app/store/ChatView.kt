package io.github.cepheus.app.store

import io.github.cepheus.app.protocol.AsyncQuestion
import io.github.cepheus.app.protocol.Item
import io.github.cepheus.app.protocol.Protocol
import io.github.cepheus.app.protocol.ServerMessage
import io.github.cepheus.app.protocol.long
import io.github.cepheus.app.protocol.str
import org.json.JSONArray
import org.json.JSONObject

/**
 * The part of one session's chat the phone holds: items `start` until
 * `start + items.size` of `total`, from one `generation` of its log.
 */
data class ChatView(
    val id: Long,
    val start: Int = 0,
    val total: Int = 0,
    val generation: Long = 0,
    val source: String = "log",
    val items: List<Item> = emptyList(),
    val model: String? = null,
    val effort: String? = null,
    /** A page has come from the gateway since the chat was opened. */
    val live: Boolean = false,
    val loadingOlder: Boolean = false,
    /** Absolute offset of the gateway's retained history, not a local page limit. */
    val retainedFrom: Int = 0,
    /** Only held in memory; never counted, paginated, or saved as a message. */
    val stream: String? = null,
    /** Conversation snapshot, independent of the indexed history window. */
    val questions: List<AsyncQuestion>? = null,
) {
    val pendingQuestions: List<AsyncQuestion> get() = (questions ?: items.filterIsInstance<Item.Assistant>()
        .flatMap { it.questions }).filterNot { it.answered }.distinctBy { it.id }

    val hasOlder: Boolean get() = start > retainedFrom

    /** Refresh the authoritative tail while retaining overlapping loaded history. */
    fun withPage(page: ServerMessage.Chat): ChatView {
        val sameChat = page.id == id && page.generation == generation && page.source == source
        val retained = sameChat && page.start >= start && page.start <= start + items.size
        val prefix = if (retained) items.take(page.start - start) else emptyList()
        return copy(
            start = if (retained) start else page.start,
            total = page.total,
            generation = page.generation,
            source = page.source,
            items = prefix + page.items,
            model = page.model ?: model,
            effort = page.effort ?: effort,
            live = true,
            loadingOlder = false,
            stream = null,
            questions = page.questions ?: if (sameChat) questions else null,
        ).retaining(if (sameChat) maxOf(retainedFrom, page.retainedFrom ?: 0) else page.retainedFrom ?: 0)
    }

    /**
     * New items: they replace what is held from their `start` on, and all of
     * it when they come from another generation, which is another chat
     * (`/clear`) or a rewritten log.
     */
    fun withItems(m: ServerMessage.Items): ChatView {
        val keep = m.start - start
        val otherChat = m.reset && m.generation != null && m.generation != generation
        val (newStart, newItems) = if (keep < 0 || keep > items.size || otherChat) {
            m.start to m.items
        } else {
            start to items.take(keep) + m.items
        }
        return copy(
            start = newStart,
            total = m.total,
            generation = m.generation ?: generation,
            source = if (newItems.any { it is Item.Screen }) "screen" else "log",
            items = newItems,
            model = m.model ?: model,
            effort = m.effort ?: effort,
            live = true,
            stream = m.stream,
            questions = m.questions ?: if (otherChat || newItems.any { it is Item.Screen }) null else questions,
        ).retaining(
            if (otherChat || newItems.any { it is Item.Screen }) m.retainedFrom ?: 0
            else maxOf(retainedFrom, m.retainedFrom ?: 0)
        )
    }

    /** An older page from `chat`; from another generation, it cannot be joined on. */
    fun withOlder(page: ServerMessage.Chat): ChatView {
        if (page.generation != generation) return copy(loadingOlder = false)
        val joined = if (page.start + page.items.size == start) {
            copy(start = page.start, items = page.items + items, loadingOlder = false,
                questions = questions ?: page.questions)
        } else copy(loadingOlder = false)
        return joined.retaining(maxOf(retainedFrom, page.retainedFrom ?: 0))
    }

    /** Release only history already evicted upstream; surviving row keys stay absolute. */
    private fun retaining(frontier: Int): ChatView {
        val floor = frontier.coerceIn(0, total.coerceAtLeast(0))
        val removed = (floor - start).coerceIn(0, items.size)
        return copy(
            start = maxOf(start, floor),
            items = if (removed == 0) items else items.drop(removed),
            retainedFrom = floor,
        )
    }

    fun toJson(): JSONObject = JSONObject()
        .put("id", id)
        .put("start", start)
        .put("total", total)
        .put("generation", generation)
        .put("source", source)
        .put("retained_from", retainedFrom)
        .putOpt("model", model)
        .putOpt("effort", effort)
        .putOpt("questions", questions?.let { JSONArray().apply { it.forEach { q -> put(Protocol.questionJson(q)) } } })
        .put("items", JSONArray().apply { items.forEach { put(Protocol.itemJson(it)) } })

    companion object {
        fun fromJson(o: JSONObject): ChatView = ChatView(
            id = o.long("id") ?: 0,
            start = o.optInt("start"),
            total = o.optInt("total"),
            generation = o.long("generation") ?: 0,
            source = o.str("source") ?: "log",
            retainedFrom = o.optInt("retained_from").coerceAtLeast(0),
            items = Protocol.items(o.optJSONArray("items")),
            model = o.str("model"),
            effort = o.str("effort"),
            questions = o.optJSONArray("questions")?.let(Protocol::questions),
        )
    }
}
