package io.github.cepheus.app.protocol

import org.json.JSONArray
import org.json.JSONObject

/** Public queue metadata is separate from private executable specifications. */
object QueueContracts {
    const val PROTOCOL_VERSION = 14
    data class Dispatch(val kind: String, val id: Long)
    data class SourceRef(val sourceId: String, val itemId: String)
    data class Dependency(val queue: Long? = null, val source: String? = null, val task: String? = null, val condition: String = "finished") {
        fun json(): JSONObject = JSONObject().put("condition", condition).apply {
            queue?.let { put("queue", it) }; source?.let { put("source", it) }; task?.let { put("task", it) }
        }
        val description: String get() = if (queue != null) "Queue #$queue · $condition" else "$source / $task · $condition"
    }
    data class Info(val id: Long, val title: String, val kind: String, val state: String, val priority: Int,
        val createdAt: Long?, val updatedAt: Long?, val reason: String, val dispatch: Dispatch?,
        val dependencies: List<Dependency>, val source: SourceRef?, val budget: Double?, val cancelable: Boolean,
        val provider: String?, val model: String?, val placement: String)
    data class Source(val id: String, val owner: String, val repository: String, val host: String?, val enabled: Boolean,
        val updatedAt: Long?, val fresh: Boolean, val error: String?)
    data class Task(val sourceId: String, val id: String, val title: String, val state: String,
        val worktree: String?, val branch: String?, val nextStep: String?)
    data class Claim(val day: String, val generation: Long, val session: String, val claimedAt: Long?, val staleAt: Long?)
    data class Chore(val id: String, val owner: String, val repository: String, val title: String, val registered: Boolean,
        val enabled: Boolean, val effectiveEnabled: Boolean, val ownerLive: Boolean?, val provider: String?, val target: String,
        val cron: String, val timezone: String, val nextRun: Long?, val claim: Claim?, val outcome: String?)
    data class Listing(val enabled: Boolean, val items: List<Info>, val pausePending: List<String> = emptyList(), val pauseEffective: Boolean = false) {
        val pauseConfirmed: Boolean get() = !enabled && pauseEffective && pausePending.isEmpty()
    }
    data class Sources(val sources: List<Source>, val tasks: List<Task>)
    data class Chores(val enabled: Boolean, val chores: List<Chore>)

    fun supported(value: JSONObject, facility: String = "work-queue"): Boolean = integer(value.opt("protocol"))?.let { it >= PROTOCOL_VERSION } == true &&
        value.optJSONArray("facilities")?.let { array -> (0 until array.length()).any { array.opt(it) == facility } } == true

    fun request(operation: String, args: JSONObject = JSONObject()): JSONObject {
        require(operation in setOf("capabilities", "queue-add", "queue-list", "queue-cancel", "queue-reorder", "queue-enable", "queue-batch",
            "source-list", "source-refresh", "source-queue", "chore-register", "chore-list", "chore-probe", "chore-enable", "chore-run", "chore-update", "chore-remove")) { "Unknown queue operation." }
        require(!args.has("host")) { "Queue identifiers are routed by the deck; choose placement in the executable specification." }
        return JSONObject().put("operation", operation).put("args", JSONObject(args.toString()))
    }
    fun target(operation: String, id: Long): JSONObject {
        require(id > 0) { "Invalid queue ID." }
        return request(operation, JSONObject().put("id", id))
    }
    fun reorder(items: List<Info>, id: Long, delta: Int): JSONObject {
        require(delta == -1 || delta == 1) { "Choose an adjacent queue position." }
        val ids = items.filter { it.cancelable }.map { it.id }.toMutableList()
        val from = ids.indexOf(id)
        require(from >= 0 && from + delta in ids.indices) { "This item cannot move there. Refresh the queue." }
        val other = ids[from + delta]; ids[from + delta] = id; ids[from] = other
        return request("queue-reorder", JSONObject().put("ids", JSONArray(ids)))
    }
    fun source(operation: String, id: String): JSONObject = request(operation, JSONObject().put("source", identifier(id, "source ID")))
    fun queueSource(ref: SourceRef, spec: JSONObject): JSONObject = request("source-queue", JSONObject()
        .put("source", identifier(ref.sourceId, "source ID")).put("item", identifier(ref.itemId, "task ID")).put("spec", spec))
    fun chore(operation: String, id: String, enabled: Boolean? = null, key: String? = null): JSONObject = request(operation, JSONObject()
        .put("id", identifier(id, "chore ID")).apply { enabled?.let { put("enabled", it) }; key?.let { put("key", requestKey(it)) } })
    fun schedule(cron: String, timezone: String): JSONObject {
        require(cron.length in 9..128 && cron.trim().split(Regex("\\s+")).size == 5) { "Enter a five-field cron schedule." }
        require(timezone in setOf("local", "UTC")) { "Choose local time or UTC." }
        return JSONObject().put("cron", cron.trim()).put("timezone", timezone)
    }
    fun registerChore(id: String, title: String, owner: String, repository: String, target: String, cron: String, timezone: String, spec: JSONObject): JSONObject {
        require(target in setOf("session", "job", "idle")) { "Choose a chore target." }
        require(spec.getString("kind") == if (target == "job") "job" else "session") { "Choose a chore target matching its executable work." }
        return request("chore-register", JSONObject().put("id", identifier(id, "chore ID")).put("title", title)
            .put("owner", identifier(owner, "chore owner", 64)).put("repository", identifier(repository, "repository", 4096))
            .put("target", target).put("schedule", schedule(cron, timezone)).put("spec", spec).put("enabled", false))
    }

    /** This is an explicit executable spec. A source's display title never supplies executable input. */
    fun spec(key: String, title: String, kind: String, argvText: String, prompt: String, provider: String?, model: String,
        cwd: String, placement: String, priorityText: String, dependencies: List<Dependency>, budgetText: String): JSONObject {
        requestKey(key)
        require(kind in setOf("job", "session")) { "Choose a headless job or interactive session." }
        require(title.isNotBlank() && title.toByteArray(Charsets.UTF_8).size <= 200 && title.none { it.isISOControl() }) { "Enter a title of at most 200 UTF-8 bytes without control characters." }
        // Reuse literal-argv validation without executing or sending the resulting job request.
        val argv = if (kind == "job") JobContracts.start(key, title, argvText, cwd, placement, provider).getJSONObject("args").getJSONArray("argv") else null
        require(cwd.startsWith("/") && cwd.length <= 4096 && '\u0000' !in cwd) { "Enter an absolute working directory." }
        require(placement.isNotBlank() && placement.length <= 32) { "Choose an available machine." }
        require(provider == null || provider in Tools.all) { "Unknown provider." }
        if (kind == "session") require(provider != null && prompt.isNotBlank() && prompt.toByteArray(Charsets.UTF_8).size <= 16000 && '\u0000' !in prompt) { "Choose a provider and enter the session's prompt." }
        require(model.length <= 256 && '\u0000' !in model) { "The model name is too long." }
        val priority = priorityText.toIntOrNull() ?: throw IllegalArgumentException("Enter a whole-number priority.")
        require(dependencies.size <= 100) { "Use at most 100 dependencies." }
        dependencies.forEach { dependency(it.json()) }
        val budget = if (budgetText.isBlank()) null else budgetText.toDoubleOrNull()?.takeIf { it.isFinite() && it > 0 }
            ?: throw IllegalArgumentException("Enter a positive finite budget in USD.")
        return JSONObject().put("key", key).put("title", title).put("kind", kind).put("cwd", cwd).put("placement", placement)
            .put("priority", priority).put("dependencies", JSONArray(dependencies.map { it.json() })).apply {
                argv?.let { put("argv", it) }; if (kind == "session") put("prompt", prompt)
                provider?.let { put("provider", it) }; model.takeIf { it.isNotBlank() }?.let { put("model", it) }; budget?.let { put("budget", it) }
            }
    }
    fun batch(text: String): JSONObject {
        require(text.length <= 131072) { "The batch is too large." }
        val records = try { JSONArray(text) } catch (_: Exception) { throw IllegalArgumentException("Enter a JSON array of full queue specifications.") }
        require(records.length() in 1..100) { "Use between 1 and 100 specifications." }
        val keys = mutableSetOf<String>()
        for (index in 0 until records.length()) {
            val record = records.getJSONObject(index)
            require(record.keys().asSequence().all { it in setOf("key", "title", "kind", "cwd", "argv", "prompt", "provider", "model", "account", "placement", "priority", "dependencies", "budget", "source", "stdin", "env_clear", "env_allowlist", "env") }) { "Unknown batch specification field." }
            val key = text(record, "key"); require(keys.add(key)) { "Every batch item needs a different key." }
            spec(key, text(record, "title"), text(record, "kind"), record.optJSONArray("argv")?.toString() ?: "[]",
                optionalText(record, "prompt").orEmpty(), optionalText(record, "provider"), optionalText(record, "model").orEmpty(),
                text(record, "cwd"), if (record.has("placement")) text(record, "placement") else "auto",
                if (record.has("priority")) integer(record.opt("priority"))?.toString() ?: "invalid" else "0",
                if (record.has("dependencies")) objects(record, "dependencies", 100).map(::dependency) else emptyList(),
                optionalBudget(record)?.toString().orEmpty())
        }
        return request("queue-batch", JSONObject().put("items", records))
    }

    fun dependency(value: JSONObject): Dependency {
        val condition = text(value, "condition")
        val queue = nullableInteger(value, "queue")
        val source = optionalText(value, "source"); val task = optionalText(value, "task")
        require(if (queue != null) queue > 0 && source == null && task == null && condition == "finished"
            else !source.isNullOrBlank() && !task.isNullOrBlank() && condition in setOf("ready", "merged")) { "Choose a queue item that finishes, or a source task that becomes ready or merged." }
        return Dependency(queue, source, task, condition)
    }
    fun info(value: JSONObject): Info {
        val dispatch = value.optJSONObject("dispatch")?.let {
            Dispatch(text(it, "kind").also { kind -> require(kind in setOf("job", "session")) { "Unknown dispatch kind." } }, positive(it.opt("id"), "dispatch ID"))
        }
        val source = value.optJSONObject("source")?.let { SourceRef(text(it, "source_id"), text(it, "item_id")) }
        return Info(positive(value.opt("id"), "queue ID"), text(value, "title"), text(value, "kind"), text(value, "state"),
            integer(value.opt("priority"))?.takeIf { it in Int.MIN_VALUE..Int.MAX_VALUE }?.toInt() ?: throw IllegalArgumentException("Invalid priority."),
            nullableInteger(value, "created_at"), nullableInteger(value, "updated_at"), text(value, "reason"), dispatch,
            objects(value, "dependencies", 100).map(::dependency), source, optionalBudget(value), boolean(value, "cancelable"),
            optionalText(value, "provider"), optionalText(value, "model"), text(value, "placement"))
    }
    fun listing(value: JSONObject): Listing {
        val pending = if (!value.has("pause_pending")) emptyList() else {
            val names = value.optJSONArray("pause_pending") ?: throw IllegalArgumentException("Invalid pending executor list.")
            require(names.length() <= 256) { "The pending executor list is too large." }
            (0 until names.length()).map { identifier(names.opt(it) as? String ?: throw IllegalArgumentException("Invalid pending executor name."), "pending executor name", 512) }
        }
        return Listing(boolean(value, "enabled"), objects(value, "items").map(::info), pending,
            if (value.has("pause_effective")) boolean(value, "pause_effective") else false)
    }
    fun sources(value: JSONObject): Sources = Sources(objects(value, "sources", 1000).map {
        Source(text(it, "id"), text(it, "owner"), text(it, "repository"), optionalText(it, "host"), boolean(it, "enabled"),
            nullableInteger(it, "updated_at"), boolean(it, "fresh"), optionalText(it, "error"))
    }, objects(value, "tasks").map {
        Task(text(it, "source_id"), text(it, "id"), text(it, "title"), text(it, "state"), optionalText(it, "worktree"), optionalText(it, "branch"), optionalText(it, "next_step"))
    })
    fun choreInfo(value: JSONObject): Chore {
        val schedule = value.getJSONObject("schedule")
        val claim = value.optJSONObject("claim")?.let { Claim(text(it, "day"), positive(it.opt("generation"), "claim generation"), text(it, "session"), nullableInteger(it, "claimed_at"), nullableInteger(it, "stale_at")) }
        return Chore(text(value, "id"), text(value, "owner"), text(value, "repository"), text(value, "title"), boolean(value, "registered"),
            boolean(value, "enabled"), boolean(value, "effective_enabled"), value.opt("owner_live") as? Boolean, optionalText(value, "provider"), text(value, "target"),
            text(schedule, "cron"), text(schedule, "timezone"), nullableInteger(value, "next_run"), claim, optionalText(value, "outcome"))
    }
    fun chores(value: JSONObject): Chores = Chores(boolean(value, "enabled"), objects(value, "chores", 1000).map(::choreInfo))
    private fun optionalBudget(value: JSONObject): Double? = if (value.isNull("budget")) null else
        (value.opt("budget") as? Number)?.toDouble()?.takeIf { it.isFinite() && it > 0 } ?: throw IllegalArgumentException("Invalid budget.")
    private fun objects(value: JSONObject, key: String, limit: Int = 10000): List<JSONObject> {
        val array = value.optJSONArray(key) ?: throw IllegalArgumentException("The deck did not return $key.")
        require(array.length() <= limit) { "The $key list is too large." }
        return (0 until array.length()).map { array.getJSONObject(it) }
    }
    private fun text(value: JSONObject, key: String): String = value.opt(key) as? String ?: throw IllegalArgumentException("Invalid $key.")
    private fun optionalText(value: JSONObject, key: String): String? = if (value.isNull(key)) null else text(value, key)
    private fun boolean(value: JSONObject, key: String): Boolean = value.opt(key) as? Boolean ?: throw IllegalArgumentException("Invalid $key.")
    private fun integer(value: Any?): Long? = when (value) { is Int -> value.toLong(); is Long -> value; else -> null }
    private fun nullableInteger(value: JSONObject, key: String): Long? = if (value.isNull(key)) null else
        integer(value.opt(key))?.takeIf { it >= 0 } ?: throw IllegalArgumentException("Invalid $key.")
    private fun positive(value: Any?, label: String): Long = integer(value)?.takeIf { it > 0 } ?: throw IllegalArgumentException("Invalid $label.")
    private fun requestKey(value: String): String = value.also { require(it.matches(Regex("[A-Za-z0-9_-]{1,128}"))) { "Invalid request key." } }
    private fun identifier(value: String, label: String, maxBytes: Int = 256): String = value.also { require(it.isNotBlank() && it.toByteArray(Charsets.UTF_8).size <= maxBytes && it.none { character -> character.isISOControl() }) { "Invalid $label." } }
}
