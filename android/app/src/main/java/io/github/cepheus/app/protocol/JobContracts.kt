package io.github.cepheus.app.protocol

import org.json.JSONArray
import org.json.JSONObject

/** Protocol 14 jobs use service-routed IDs, never interactive session IDs. */
object JobContracts {
    const val PROTOCOL_VERSION = 14
    const val PAGE_BYTES = 32768
    const val MAX_TEXT = 141072
    const val MAX_ITEMS = 600

    data class Info(
        val id: Long, val title: String, val provider: String?, val status: String,
        val createdAt: Long?, val startedAt: Long?, val finishedAt: Long?,
        val exitCode: Int?, val signal: Int?, val reason: String, val host: String?,
    ) {
        val terminal: Boolean get() = status == "finished" || status == "failed"
        val canStop: Boolean get() = status == "starting" || status == "working" || (status == "failed" && exitCode == null)
        val outcome: String get() = when {
            signal != null -> "Signal $signal"
            exitCode != null -> "Exit $exitCode"
            terminal -> "Exit status unknown"
            status == "starting" -> "Starting"
            status == "working" -> "Working"
            else -> "Unknown status"
        }
    }

    data class Page(val job: Long, val offset: Long, val text: String, val items: List<Item>, val eof: Boolean, val itemsTruncated: Boolean = false)

    /** Bounded raw pages and the newest cumulative chat snapshot. A stale page cannot join another job or cursor. */
    data class Output(
        val job: Long, val offset: Long = 0, val text: String = "", val items: List<Item> = emptyList(),
        val eof: Boolean = false, val trimmed: Boolean = false,
    ) {
        fun append(requestedOffset: Long, page: Page): Output {
            require(page.job == job && requestedOffset == offset && page.offset >= offset) { "The job output changed. Refresh its log." }
            require(page.offset > offset || page.text.isEmpty()) { "The job log did not advance." }
            val newText = text + page.text
            val newItems = readOnlyItems(page.items)
            return copy(offset = page.offset, text = newText.takeLast(MAX_TEXT), items = newItems.takeLast(MAX_ITEMS),
                eof = page.eof, trimmed = trimmed || page.itemsTruncated || newText.length > MAX_TEXT || newItems.size > MAX_ITEMS)
        }
    }

    fun supported(value: JSONObject): Boolean = integer(value.opt("protocol"))?.let { it >= PROTOCOL_VERSION } == true &&
        value.optJSONArray("facilities")?.let { array -> (0 until array.length()).any { array.opt(it) == "jobs" } } == true

    fun request(operation: String, args: JSONObject = JSONObject()): JSONObject {
        require(operation in setOf("capabilities", "job-start", "job-status", "job-log", "job-stop", "job-dismiss")) { "Unknown job operation." }
        require(!args.has("host")) { "Job IDs are routed by the deck; choose placement when starting." }
        return JSONObject().put("operation", operation).put("args", JSONObject(args.toString()))
    }

    fun target(operation: String, id: Long): JSONObject {
        require(id > 0) { "Invalid job ID." }
        return request(operation, JSONObject().put("job", id))
    }

    fun logRequest(id: Long, offset: Long): JSONObject {
        require(offset >= 0) { "Invalid job log cursor." }
        return request("job-log", target("job-log", id).getJSONObject("args").put("offset", offset).put("limit", PAGE_BYTES))
    }

    /** JSON argv preserves spaces, quotes, newlines and shell punctuation as literal arguments. */
    fun start(key: String, title: String, argvText: String, cwd: String, placement: String, provider: String?): JSONObject {
        require(key.matches(Regex("[A-Za-z0-9_-]{1,128}"))) { "Invalid job request key." }
        require(title.isNotBlank() && title.length <= 256) { "Enter a title of at most 256 characters." }
        require(argvText.length <= 65536) { "The argument vector is too large." }
        val argv = try { JSONArray(argvText) } catch (_: Exception) { throw IllegalArgumentException("Enter arguments as a JSON array of strings.") }
        require(argv.length() in 1..256) { "Enter between 1 and 256 arguments." }
        for (i in 0 until argv.length()) {
            val arg = argv.opt(i)
            require(arg is String && arg.length <= 16384 && '\u0000' !in arg && (i != 0 || arg.isNotBlank())) { "Every argument must be a string without NUL; the executable cannot be blank." }
        }
        require(cwd.startsWith("/") && cwd.length <= 4096 && '\u0000' !in cwd) { "Enter an absolute working directory." }
        require(placement.isNotBlank() && placement.length <= 32) { "Choose an available machine." }
        require(provider == null || provider in Tools.all) { "Unknown provider." }
        return request("job-start", JSONObject().put("key", key).put("title", title).put("argv", argv).put("cwd", cwd)
            .put("placement", placement).apply { provider?.let { put("provider", it) } })
    }

    fun info(value: JSONObject): Info = Info(
        positive(value.opt("id"), "job ID"), value.optString("title"), value.opt("provider") as? String,
        value.optString("status"), integer(value.opt("created_at")), integer(value.opt("started_at")), integer(value.opt("finished_at")),
        signedInt(value.opt("exit_code")), signedInt(value.opt("signal")), value.optString("reason"), value.opt("host") as? String,
    )

    fun jobs(value: JSONObject): List<Info> {
        val array = value.optJSONArray("jobs") ?: throw IllegalArgumentException("The deck did not return its jobs.")
        require(array.length() <= 10000) { "The job list is too large." }
        return (0 until array.length()).map { info(array.getJSONObject(it)) }
    }

    fun page(value: JSONObject): Page {
        val text = value.opt("text") as? String ?: throw IllegalArgumentException("The job log is unavailable.")
        require(text.length <= MAX_TEXT) { "The job log page is too large." }
        val array = value.optJSONArray("items")
        require(array == null || array.length() <= MAX_ITEMS) { "The job output page is too large." }
        val eof = value.opt("eof") as? Boolean ?: throw IllegalArgumentException("The job log has no completion state.")
        return Page(positive(value.opt("job"), "job ID"), integer(value.opt("offset"))?.takeIf { it >= 0 }
            ?: throw IllegalArgumentException("Invalid job log cursor."), text, Protocol.items(array), eof, value.opt("items_truncated") == true)
    }

    /** Job output has no image endpoint or question channel. Never address an interactive session. */
    fun readOnlyItems(items: List<Item>): List<Item> = items.flatMap { item -> when (item) {
        is Item.User -> listOf(item.copy(images = emptyList())) + if (item.images.isNotEmpty()) listOf(Item.Notice("${item.images.size} attachment(s); image previews are unavailable for jobs.")) else emptyList()
        is Item.Assistant -> listOf(item.copy(questions = emptyList())) + item.questions.mapNotNull { question ->
            if (question.title in item.text && question.options.all { it in item.text }) null
            else Item.Notice(question.title + "\n" + question.options.joinToString("\n") { "• $it" })
        }
        else -> listOf(item)
    } }

    private fun positive(value: Any?, label: String): Long = integer(value)?.takeIf { it > 0 }
        ?: throw IllegalArgumentException("Invalid $label.")
    private fun integer(value: Any?): Long? = when (value) { is Int -> value.toLong(); is Long -> value; else -> null }
    private fun signedInt(value: Any?): Int? = if (value == null || value == JSONObject.NULL) null else
        integer(value)?.takeIf { it in Int.MIN_VALUE..Int.MAX_VALUE }?.toInt() ?: throw IllegalArgumentException("Invalid job exit status.")
}
