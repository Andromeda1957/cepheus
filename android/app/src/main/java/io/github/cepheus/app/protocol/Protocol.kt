package io.github.cepheus.app.protocol

import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/** Reads the gateway's text frames. A frame that does not parse is [ServerMessage.Unknown]. */
object Protocol {
    /**
     * The `err` code of an answer refused because its question no longer
     * waits: answered, replaced, or not part of this conversation.
     */
    const val QUESTION_STALE = "question_stale"

    fun parse(text: String): ServerMessage = try {
        message(JSONObject(text))
    } catch (_: JSONException) {
        ServerMessage.Unknown("invalid")
    } catch (_: IllegalArgumentException) {
        ServerMessage.Unknown("invalid")
    }

    private fun message(o: JSONObject): ServerMessage = when (val t = o.text("t")) {
        "control" -> ServerMessage.Control(
            o.long("rid"), o.getString("action"), o.getJSONObject("data"),
        )
        "paired" -> ServerMessage.Paired(o.text("device"), o.text("key"))
        "denied" -> ServerMessage.Denied(o.text("reason"))
        "hello" -> ServerMessage.Hello(o.text("epoch"), o.text("version"))
        "state" -> ServerMessage.State(state(o))
        "alert" -> ServerMessage.Alert(
            seq = o.long("seq") ?: 0,
            epoch = o.text("epoch"),
            kind = AlertKind.of(o.str("kind")),
            id = o.long("id"),
            title = o.text("title"),
            reason = o.text("reason"),
            deskFocused = o.optBoolean("desk_focused"),
            at = o.long("at") ?: 0,
            replay = o.optBoolean("replay"),
            message = o.str("message"),
        )
        "prompt" -> ServerMessage.PromptUpdate(
            id = o.long("id") ?: 0,
            inputs = o.long("inputs") ?: 0,
            fp = o.text("fp"),
            prompt = o.optJSONObject("prompt")?.let(::prompt),
            replay = o.optBoolean("replay"),
        )
        "items" -> ServerMessage.Items(
            id = o.long("id") ?: 0,
            reset = o.optBoolean("reset"),
            start = o.optInt("start"),
            total = o.optInt("total"),
            generation = o.long("generation"),
            items = items(o.optJSONArray("items")),
            model = o.str("model"),
            effort = o.str("effort"),
            retainedFrom = o.long("retained_from")?.coerceIn(0, Int.MAX_VALUE.toLong())?.toInt(),
            stream = o.str("stream"),
            questions = o.optJSONArray("questions")?.let(::questions),
        )
        "chat" -> ServerMessage.Chat(
            rid = o.long("rid"),
            id = o.long("id") ?: 0,
            start = o.optInt("start"),
            total = o.optInt("total"),
            generation = o.long("generation") ?: 0,
            source = o.text("source"),
            items = items(o.optJSONArray("items")),
            model = o.str("model"),
            effort = o.str("effort"),
            retainedFrom = o.long("retained_from")?.coerceIn(0, Int.MAX_VALUE.toLong())?.toInt(),
            questions = o.optJSONArray("questions")?.let(::questions),
        )
        "notice" -> ServerMessage.Notice(o.text("text"), o.optBoolean("error"))
        "commands" -> ServerMessage.Commands(
            o.long("rid"), o.long("id") ?: 0, o.text("tool"),
            objects(o.optJSONArray("entries")).map {
                CommandEntry(it.text("name"), it.text("insert"), it.text("description"), it.text("kind"))
            },
        )
        "models" -> ServerMessage.Models(o.long("rid"), objects(o.optJSONArray("tools")).map(::toolModels))
        "conversations" -> ServerMessage.Conversations(
            o.long("rid"),
            objects(o.optJSONArray("items")).map {
                Conversation(
                    tool = it.text("tool"),
                    id = it.text("id"),
                    title = it.text("title"),
                    createdAt = it.long("created_at") ?: 0,
                    updatedAt = it.long("updated_at") ?: 0,
                    openIn = it.long("open_in"),
                    host = it.str("host"),
                )
            },
        )
        "image" -> ServerMessage.Image(
            o.long("rid"), o.long("id") ?: 0, o.text("key"), SentImage(o.text("media"), o.text("data")),
        )
        "search" -> ServerMessage.Search(
            o.long("rid"),
            o.text("q"),
            objects(o.optJSONArray("hits")).map { hit ->
                SearchHit(
                    id = hit.long("id") ?: 0,
                    count = hit.optInt("count"),
                    mine = hit.optInt("mine"),
                    snippets = objects(hit.optJSONArray("snippets")).map {
                        SearchSnippet(it.text("role"), it.text("text"), it.long("at"))
                    },
                )
            },
        )
        "pong" -> ServerMessage.Pong(o.long("rid"))
        "ok" -> ServerMessage.Ok(
            o.long("rid"), o.long("id"), o.str("upload"), o.str("path"), o.str("delivered"), o.str("client_id"),
            o.long("at"), o.optJSONArray("paths")?.let(::strings),
        )
        "err" -> ServerMessage.Err(o.long("rid"), o.text("error"), o.str("code"))
        "stats" -> ServerMessage.Stats(
            rid = o.long("rid"),
            usage = usage(o.optJSONObject("usage")),
            cpu = o.double("cpu"),
            memory = o.double("memory"),
            disk = disk(o.optJSONObject("disk")),
            label = o.text("label"),
            hosts = objects(o.optJSONArray("hosts")).map {
                HostUse(
                    name = it.str("name"),
                    label = it.text("label"),
                    online = it.optBoolean("online"),
                    reason = it.str("reason"),
                    cpu = it.double("cpu"),
                    memory = it.double("memory"),
                    disk = disk(it.optJSONObject("disk")),
                )
            },
            panels = objects(o.optJSONArray("panels")).map {
                Panel(it.text("id"), it.text("title"), strings(it.optJSONArray("lines")))
            },
        )
        else -> ServerMessage.Unknown(t)
    }

    private fun disk(o: JSONObject?) = o?.let { DiskUse(it.long("used") ?: 0, it.long("available") ?: 0) }

    private fun state(o: JSONObject) = DeckState(
        ready = !o.has("ready") || o.opt("ready") == true,
        protocol = o.long("protocol") ?: 0,
        // A terminal session is the desktop deck's own; a newer deck leaves
        // them out, an older one never had them.
        sessions = objects(o.optJSONArray("sessions")).map(::session).filter { it.tool != "shell" },
        errors = objects(o.optJSONArray("errors")).map { LogEntry(it.long("at") ?: 0, it.text("text")) },
        startDir = o.text("start_dir"),
        usage = usage(o.optJSONObject("usage")),
        outside = objects(o.optJSONArray("outside")).map(::outside),
        hosts = objects(o.optJSONArray("hosts")).map {
            HostSummary(it.text("name"), it.text("label"), it.optBoolean("online"), it.str("reason"), it.optBoolean("agents", true))
        },
        hereLabel = o.text("here"),
        attentionCapable = o.opt("attention_capable") == true,
        attention = o.optJSONObject("attention")?.let { runCatching { AttentionContracts.snapshot(it) }.getOrNull() },
        cloudAgents = objects(o.optJSONArray("cloud_agents")).map {
            CloudAgent(it.text("id"), it.text("label"), it.str("host"))
        },
    )

    private fun usage(o: JSONObject?) = Usage(
        claude = objects(o?.optJSONArray("claude")).map(::usageWindow),
        codex = objects(o?.optJSONArray("codex")).map(::usageWindow),
        grok = objects(o?.optJSONArray("grok")).map(::usageWindow),
        gemini = objects(o?.optJSONArray("gemini")).map(::usageWindow),
        claudeModels = objects(o?.optJSONArray("claude_models")).map(::usageWindow),
    )

    private fun outside(o: JSONObject) = OutsideAgent(
        pid = o.long("pid") ?: 0,
        tool = o.text("tool"),
        title = o.text("title"),
        busy = if (o.has("busy") && !o.isNull("busy")) o.optBoolean("busy") else null,
        cwd = o.text("cwd"),
        branch = o.str("branch"),
        worktree = o.str("worktree"),
        task = o.str("task"),
        startedAt = o.long("started_at") ?: 0,
        host = o.str("host"),
        hostLabel = o.str("host_label"),
    )

    /** The state as the gateway sends it, for the offline copy. */
    fun stateJson(state: DeckState): String {
        val sessions = JSONArray()
        for (s in state.sessions) {
            sessions.put(
                JSONObject()
                    .put("id", s.id).put("tool", s.tool).put("title", s.title).put("status", s.status.wire)
                    .putOpt("pid", s.pid).putOpt("conversation", s.conversation)
                    .put("detail", s.detail).put("alive", s.alive).putOpt("branch", s.branch)
                    .putOpt("worktree", s.worktree).putOpt("task", s.task).put("review", s.review)
                    .put("created_at", s.createdAt).put("status_since", s.statusSince)
                    .apply { s.expectedTarget?.let { target ->
                        put("spawned_ms", target.spawnedMs).put("pid", target.pid ?: JSONObject.NULL)
                        put("conversation", target.conversation ?: JSONObject.NULL)
                    } }
                    .putOpt("host", s.host).putOpt("host_label", s.hostLabel)
                    .put("subagents", JSONArray().also { agents ->
                        for (a in s.subagents) agents.put(JSONObject()
                            .put("name", a.name).putOpt("kind", a.kind).put("active", a.active)
                            .put("started_at", a.startedAt).put("last_active", a.lastActive))
                    }),
            )
        }
        val hosts = JSONArray()
        for (h in state.hosts) {
            hosts.put(
                JSONObject().put("name", h.name).put("label", h.label).put("online", h.online).putOpt("reason", h.reason)
                    .put("agents", h.agents),
            )
        }
        val errors = JSONArray()
        for (e in state.errors) errors.put(JSONObject().put("at", e.at).put("text", e.text))
        val windows = { list: List<UsageWindow> ->
            JSONArray().also { a ->
                for (w in list) {
                    a.put(JSONObject().put("label", w.label).put("used_percent", w.usedPercent).putOpt("resets_at", w.resetsAt))
                }
            }
        }
        val outside = JSONArray()
        for (o in state.outside) {
            outside.put(
                JSONObject()
                    .put("pid", o.pid).put("tool", o.tool).put("title", o.title).putOpt("busy", o.busy)
                    .put("cwd", o.cwd).putOpt("branch", o.branch).putOpt("worktree", o.worktree)
                    .putOpt("task", o.task).put("started_at", o.startedAt)
                    .putOpt("host", o.host).putOpt("host_label", o.hostLabel),
            )
        }
        return JSONObject()
            .put("t", "state")
            .put("ready", state.ready)
            .put("protocol", state.protocol)
            .put("sessions", sessions)
            .put("errors", errors)
            .put("start_dir", state.startDir)
            .put("usage", JSONObject()
                .put("claude", windows(state.usage.claude)).put("codex", windows(state.usage.codex))
                .put("grok", windows(state.usage.grok)).put("gemini", windows(state.usage.gemini))
                .put("claude_models", windows(state.usage.claudeModels)))
            .put("outside", outside)
            .put("hosts", hosts)
            .put("here", state.hereLabel)
            .put("cloud_agents", JSONArray().apply {
                state.cloudAgents.forEach { put(JSONObject().put("id", it.id).put("label", it.label).putOpt("host", it.host)) }
            })
            .toString()
    }

    private fun session(o: JSONObject) = Session(
        id = o.long("id") ?: 0,
        pid = o.long("pid"),
        conversation = o.str("conversation"),
        tool = o.text("tool"),
        title = o.text("title"),
        status = Status.of(o.str("status")),
        detail = o.text("detail"),
        alive = o.optBoolean("alive"),
        branch = o.str("branch"),
        worktree = o.str("worktree"),
        task = o.str("task"),
        review = o.optBoolean("review"),
        createdAt = o.long("created_at") ?: 0,
        statusSince = o.long("status_since") ?: 0,
        host = o.str("host"),
        hostLabel = o.str("host_label"),
        expectedTarget = AttentionContracts.targetOrNull(o),
        subagents = objects(o.optJSONArray("subagents")).map {
            Subagent(it.text("name"), it.str("kind"), it.optBoolean("active"),
                it.long("started_at") ?: 0, it.long("last_active") ?: 0)
        },
    )

    private fun usageWindow(o: JSONObject) =
        UsageWindow(o.text("label"), o.optDouble("used_percent", Double.NaN).takeIf { it.isFinite() }, o.long("resets_at"))

    private fun prompt(o: JSONObject) = Prompt(
        kind = o.text("kind"),
        question = o.text("question"),
        options = objects(o.optJSONArray("options")).map {
            PromptOption(it.text("label"), it.text("detail"), it.opt("checked") as? Boolean)
        },
        selected = o.optInt("selected"),
        freeText = o.long("free_text")?.toInt(),
        multi = o.optBoolean("multi"),
        submit = o.str("submit"),
    )

    private fun toolModels(o: JSONObject) = ToolModels(
        tool = o.text("tool"),
        models = objects(o.optJSONArray("models")).map {
            ModelChoice(it.text("id"), it.str("label") ?: it.text("id"), strings(it.optJSONArray("efforts")))
        },
        efforts = strings(o.optJSONArray("efforts")),
        switch = o.optBoolean("switch"),
        defaultModel = o.str("default_model"),
        defaultEffort = o.str("default_effort"),
        label = o.str("label"),
        cloud = o.optBoolean("cloud"),
    )

    fun questions(array: JSONArray?): List<AsyncQuestion> = objects(array).map {
        AsyncQuestion(it.text("id"), it.text("title"), strings(it.optJSONArray("options")), it.optBoolean("answered"), it.optBoolean("submitted"))
    }

    fun questionJson(question: AsyncQuestion): JSONObject = JSONObject().put("id", question.id)
        .put("title", question.title).put("options", JSONArray(question.options)).put("answered", question.answered).put("submitted", question.submitted)

    fun items(array: JSONArray?): List<Item> = objects(array).map(::item)

    fun item(o: JSONObject): Item = when (val k = o.text("k")) {
        "user" -> Item.User(o.text("text"), o.long("at"), strings(o.optJSONArray("images")), o.str("client_id"))
        "assistant" -> Item.Assistant(o.text("text"), o.long("at"), questions(o.optJSONArray("questions")))
        "thinking" -> Item.Thinking(o.text("text"))
        "tool" -> Item.Tool(
            o.text("call"), o.text("name"), o.text("summary"), o.text("input"), o.str("label"),
            o.optJSONArray("diff")?.takeIf { it.length() == 2 }?.let { Lines(it.optInt(0), it.optInt(1)) },
        )
        "result" -> Item.Result(o.text("call"), o.text("text"), o.optBoolean("error"))
        "notice" -> Item.Notice(o.text("text"))
        "screen" -> Item.Screen(o.text("text"))
        else -> Item.Unknown(k)
    }

    /** An item as the gateway sends it, for the offline cache. */
    fun itemJson(item: Item): JSONObject = when (item) {
        is Item.User -> JSONObject().put("k", "user").put("text", item.text).putOpt("at", item.at)
            .putOpt("images", item.images.takeIf { it.isNotEmpty() }?.let { JSONArray(it) }).putOpt("client_id", item.clientId)
        is Item.Assistant -> JSONObject().put("k", "assistant").put("text", item.text).putOpt("at", item.at)
            .put("questions", JSONArray().apply { item.questions.forEach {
                put(JSONObject().put("id", it.id).put("title", it.title).put("options", JSONArray(it.options)).put("answered", it.answered))
            } })
        is Item.Thinking -> JSONObject().put("k", "thinking").put("text", item.text)
        is Item.Tool -> JSONObject().put("k", "tool").put("call", item.call).put("name", item.name)
            .put("summary", item.summary).put("input", item.input).putOpt("label", item.label)
            .putOpt("diff", item.diff?.let { JSONArray().put(it.added).put(it.removed) })
        is Item.Result -> JSONObject().put("k", "result").put("call", item.call).put("text", item.text)
            .put("error", item.error)
        is Item.Notice -> JSONObject().put("k", "notice").put("text", item.text)
        is Item.Screen -> JSONObject().put("k", "screen").put("text", item.text)
        is Item.Unknown -> JSONObject().put("k", item.kind)
    }

    private fun objects(array: JSONArray?): List<JSONObject> =
        if (array == null) emptyList() else (0 until array.length()).mapNotNull { array.optJSONObject(it) }

    private fun strings(array: JSONArray?): List<String> =
        if (array == null) emptyList() else (0 until array.length()).mapNotNull { i ->
            array.opt(i)?.takeIf { it is String }?.toString()
        }
}

/** A string field, or null when it is absent or JSON null. */
internal fun JSONObject.str(key: String): String? =
    if (!has(key) || isNull(key)) null else opt(key)?.toString()

internal fun JSONObject.text(key: String): String = str(key) ?: ""

/** A number field, or null when it is absent, JSON null or not a number. */
internal fun JSONObject.long(key: String): Long? =
    if (!has(key) || isNull(key)) null else (opt(key) as? Number)?.toLong()

internal fun JSONObject.double(key: String): Double? =
    if (!has(key) || isNull(key)) null else (opt(key) as? Number)?.toDouble()
