package io.github.cepheus.app.protocol

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** Every message the gateway sends, shaped as cepheus/src/remote/hub.rs and server.rs build them. */
class ProtocolTest {
    @Test fun serviceReadinessIsAdditiveAndCustodySurvivesTheOfflineCache() {
        val legacy = (Protocol.parse("""{"t":"state","sessions":[]}""") as ServerMessage.State).state
        assertTrue(legacy.ready)
        val current = (Protocol.parse("""{"t":"state","protocol":19,"ready":false,"sessions":[{"id":1099511627783,"tool":"codex","created_at":10,"pid":321,"conversation":"same"}]}""") as ServerMessage.State).state
        assertFalse(current.ready)
        val restored = (Protocol.parse(Protocol.stateJson(current)) as ServerMessage.State).state
        assertEquals(current, restored)
        assertEquals(19L, restored.protocol)
        assertEquals(0L, legacy.protocol)
        assertEquals(321L, restored.sessions.single().pid)
        assertEquals("same", restored.sessions.single().conversation)
    }

    @Test fun onlyTheReplacementAwareClearOptsIntoRelocation() {
        val clear = org.json.JSONObject(Requests.clear(81, 7))
        assertEquals("send", clear.getString("t"))
        assertEquals("/clear", clear.getString("text"))
        assertEquals(7L, clear.getLong("id"))
        assertTrue(clear.getBoolean("relocate"))
        assertFalse(org.json.JSONObject(Requests.send(82, 7, "/clear")).has("relocate"))
        val reply = Protocol.parse("""{"t":"ok","rid":81,"id":91}""") as ServerMessage.Ok
        assertEquals(91L, reply.id)
    }
    @Test fun cloudConnectionsStayScopedAndVisibleInTheOfflineCache() {
        val parsed = Protocol.parse("""{"t":"state","sessions":[{"id":7,"tool":"dot","alive":true}],"cloud_agents":[{"id":"dot","label":"Dot"},{"id":"dot","label":"Remote Dot","host":"fedora"}]}""") as ServerMessage.State
        val state = parsed.state
        assertEquals(2, state.cloudAgents.size)
        assertTrue(state.isCloud(state.sessions.single()))
        assertFalse(state.isCloud(state.sessions.single().copy(host = "another-host")))
        assertEquals(state, (Protocol.parse(Protocol.stateJson(state)) as ServerMessage.State).state)
        assertEquals("Dot", Tools.label("dot"))
        assertTrue(Tools.canAttach("dot"))
        assertFalse(Tools.canStop("dot"))
        assertTrue(Tools.canAttach("claude"))
    }

    @Test fun structuredQuestionChoicesSurviveTheOfflineCache() {
        val question = org.json.JSONObject("""{"k":"assistant","text":"Pick one?","questions":[{"id":"ask-1:0","title":"Pick one?","options":["First","Second","Third"]}]}""")
        val cached = Protocol.itemJson(Protocol.item(question))
        assertEquals("ask-1:0", cached.getJSONArray("questions").getJSONObject(0).getString("id"))
        assertEquals("Second", cached.getJSONArray("questions").getJSONObject(0).getJSONArray("options").getString(1))
    }





    @Test fun sentPicturesAndToolWordsSurviveTheOfflineCache() {
        val user = Protocol.item(org.json.JSONObject("""{"k":"user","text":"look","images":["120.0","120.1"],"at":5}"""))
        assertEquals(Item.User("look", 5, listOf("120.0", "120.1")), user)
        assertEquals(user, Protocol.item(Protocol.itemJson(user)))
        assertFalse("no pictures, no field", Protocol.itemJson(Item.User("hi", null)).has("images"))
        val tool = Protocol.item(org.json.JSONObject(
            """{"k":"tool","call":"c","name":"Edit","summary":"/r/a.rs","input":"{}","label":"Fix it","diff":[3,1]}""",
        ))
        assertEquals(Item.Tool("c", "Edit", "/r/a.rs", "{}", "Fix it", Lines(3, 1)), tool)
        assertEquals(tool, Protocol.item(Protocol.itemJson(tool)))
        val older = Protocol.item(org.json.JSONObject("""{"k":"tool","call":"c","name":"Bash","summary":"ls","input":"ls"}"""))
        assertEquals(Item.Tool("c", "Bash", "ls", "ls"), older)

        val image = Protocol.parse("""{"t":"image","rid":4,"id":7,"key":"120.1","media":"image/png","data":"iVBORw0KGgo="}""")
        assertEquals(ServerMessage.Image(4, 7, "120.1", SentImage("image/png", "iVBORw0KGgo=")), image)
        assertEquals(4L, image.rid)
        assertFalse("the bytes are never printed", image.toString().contains("iVBOR"))
        val ask = org.json.JSONObject(Requests.image(3, 7, "120.1"))
        assertEquals(listOf("image", 3L, 7L, "120.1"), listOf(ask.getString("t"), ask.getLong("rid"), ask.getLong("id"), ask.getString("key")))
    }

    @Test
    fun identicalConversationIdsOnDifferentHostsRemainDistinct() {
        val parsed = Protocol.parse("""{"t":"conversations","rid":8,"items":[{"tool":"grok","id":"same","title":"Work"},{"tool":"grok","id":"same","title":"Work","host":"fedora"}]}""") as ServerMessage.Conversations
        assertEquals(2, parsed.items.distinct().size)
        assertEquals(listOf(null, "fedora"), parsed.items.map { it.host })
        assertEquals("fedora", org.json.JSONObject(Requests.resume(9, parsed.items[1].tool,
            parsed.items[1].id, parsed.items[1].host)).getString("host"))
        assertFalse(org.json.JSONObject(Requests.resume(10, parsed.items[0].tool,
            parsed.items[0].id, parsed.items[0].host)).has("host"))
    }
    @org.junit.Test
    fun commandCatalogIsAReplyForTheRequestedSession() {
        val parsed = Protocol.parse("""{"t":"commands","rid":42,"id":7,"tool":"codex","entries":[{"name":"/model","insert":"/model","description":"Choose model","kind":"command"},{"name":"${'$'}clean","insert":"${'$'}clean","description":"Clean sessions","kind":"skill"}]}""")
        org.junit.Assert.assertEquals("Commands", parsed.javaClass.simpleName)
        org.junit.Assert.assertEquals(42L, parsed.rid)
        parsed as ServerMessage.Commands
        assertEquals(7L, parsed.id)
        assertEquals("codex", parsed.tool)
        assertEquals(listOf("/model", "$" + "clean"), parsed.entries.map { it.insert })
    }

    /** A search's answer as hub.rs sends it: `search::Hit`s, a snippet's `at` only when the log has one. */
    @Test
    fun searchHitsWithTheirMatchingMessages() {
        val parsed = Protocol.parse(
            """{"t":"search","rid":12,"q":"agy","hits":[""" +
                """{"id":4,"count":3,"mine":1,"snippets":[{"role":"user","text":"start agy agents","at":1793527200000},""" +
                """{"role":"assistant","text":"agy is in"}]},""" +
                """{"id":7,"count":1,"mine":0,"snippets":[{"role":"screen","text":"> agy"}]}]}""",
        )
        assertEquals(12L, parsed.rid)
        parsed as ServerMessage.Search
        assertEquals("agy", parsed.q)
        assertEquals(
            listOf(
                SearchHit(
                    4, 3, 1,
                    listOf(SearchSnippet("user", "start agy agents", 1793527200000), SearchSnippet("assistant", "agy is in", null)),
                ),
                SearchHit(7, 1, 0, listOf(SearchSnippet("screen", "> agy", null))),
            ),
            parsed.hits,
        )
        assertEquals(emptyList<SearchHit>(), (Protocol.parse("""{"t":"search","rid":13,"q":"x"}""") as ServerMessage.Search).hits)
    }

    /** The resume list as hub.rs sends it: the service's `Conversation`s, `open_in` null unless open. */
    @Test
    fun conversationsToResume() {
        val parsed = Protocol.parse(
            """{"t":"conversations","rid":8,"items":[""" +
                """{"tool":"grok","id":"01a16bf6-a28a-7ce3-b29d-e722992ee7d8","title":"Fix the build","created_at":1792812690,"updated_at":1792812700,"open_in":4},""" +
                """{"tool":"claude","id":"c0ffee00","title":"Tidy the parser","created_at":1792000000,"updated_at":1792000100,"open_in":null}]}""",
        )
        assertEquals(8L, parsed.rid)
        parsed as ServerMessage.Conversations
        assertEquals(
            listOf(
                Conversation("grok", "01a16bf6-a28a-7ce3-b29d-e722992ee7d8", "Fix the build", 1792812690, 1792812700, 4),
                Conversation("claude", "c0ffee00", "Tidy the parser", 1792000000, 1792000100, null),
            ),
            parsed.items,
        )
        assertEquals(emptyList<Conversation>(), (Protocol.parse("""{"t":"conversations","rid":9}""") as ServerMessage.Conversations).items)
    }

    @Test
    fun handshakeMessages() {
        assertEquals(
            ServerMessage.Paired("d-0a1b2c3d4e5f", "ab".repeat(32)),
            Protocol.parse("""{"t":"paired","device":"d-0a1b2c3d4e5f","key":"${"ab".repeat(32)}"}"""),
        )
        assertEquals(
            ServerMessage.Denied("unknown phone or key; pair again"),
            Protocol.parse("""{"t":"denied","reason":"unknown phone or key; pair again"}"""),
        )
        assertEquals(
            ServerMessage.Hello("e-1730000000", "0.9.0"),
            Protocol.parse("""{"t":"hello","epoch":"e-1730000000","version":"0.9.0"}"""),
        )
    }

    @Test
    fun pairedNeverPrintsItsKey() {
        val paired = Protocol.parse("""{"t":"paired","device":"d-1","key":"${"cd".repeat(32)}"}""")
        assertFalse(paired.toString().contains("cd".repeat(32)))
    }

    @Test
    fun state() {
        val msg = Protocol.parse(
            """
            {"t":"state","sessions":[
              {"id":3,"tool":"claude","title":"Fix login","status":"needs_you","detail":"permission","alive":true,
               "branch":"agent/fix","worktree":"/w/fix","task":"fix-login","review":false,"created_at":1700000000,"status_since":1700000100,"subagents":[{"name":"Inspect parser","kind":"Explore","active":true,"started_at":1700000010,"last_active":1700000100},{"name":"Review parser tests","kind":null,"active":false,"started_at":1700000020,"last_active":1700000090}]},
              {"id":4,"tool":"opencode","title":"","status":"ended","detail":"","alive":false,
               "branch":null,"worktree":null,"task":null,"review":true,"created_at":1700000200,"status_since":1700000300}],
             "errors":[{"at":1700000400,"text":"codex: exited"}],
             "start_dir":"/home/owner/project",
             "usage":{"claude":[{"label":"5h","used_percent":42.5,"resets_at":1700003600,"updated_at":1700000000}],"codex":[]}}
            """.trimIndent(),
        ) as ServerMessage.State
        val (first, second) = msg.state.sessions
        assertEquals(
            Session(3, "claude", "Fix login", Status.NEEDS_YOU, "permission", true, "agent/fix", "/w/fix", "fix-login", false, 1700000000, 1700000100).copy(subagents = listOf(
                    Subagent("Inspect parser", "Explore", true, 1700000010, 1700000100),
                    Subagent("Review parser tests", null, false, 1700000020, 1700000090),
                )),
            first,
        )
        assertEquals(emptyList<Subagent>(), second.subagents)
        assertEquals(Status.ENDED, second.status)
        assertNull(second.branch)
        assertNull(second.worktree)
        assertNull(second.task)
        assertTrue(second.review)
        assertFalse(second.alive)
        assertEquals(listOf(LogEntry(1700000400, "codex: exited")), msg.state.errors)
        assertEquals("/home/owner/project", msg.state.startDir)
        assertEquals(listOf(UsageWindow("5h", 42.5, 1700003600)), msg.state.usage.claude)
    }


    @Test
    fun stateListsTheAgentsRunningOutsideTheDeck() {
        val msg = Protocol.parse(
            """
            {"t":"state","sessions":[],"errors":[],"start_dir":"/p","usage":{"claude":[],"codex":[]},
             "outside":[
              {"pid":4242,"tool":"codex","title":"Refactor auth","busy":true,"cwd":"/w/auth",
               "branch":"agent/auth","worktree":"/w/auth","task":"auth","started_at":1700000500},
              {"pid":4343,"tool":"claude","title":"","busy":null,"cwd":"/p","branch":null,"worktree":null,"task":null,
               "started_at":1700000600}]}
            """.trimIndent(),
        ) as ServerMessage.State
        assertEquals(
            listOf(
                OutsideAgent(4242, "codex", "Refactor auth", true, "/w/auth", "agent/auth", "/w/auth", "auth", 1700000500),
                OutsideAgent(4343, "claude", "", null, "/p", null, null, null, 1700000600),
            ),
            msg.state.outside,
        )
        val older = Protocol.parse("""{"t":"state","sessions":[],"errors":[],"start_dir":"/p"}""") as ServerMessage.State
        assertEquals("a gateway from before outside agents", emptyList<OutsideAgent>(), older.state.outside)
    }

    @Test
    fun theOfflineCopyKeepsTheWholeState() {
        val state = DeckState(
            sessions = listOf(
                Session(3, "claude", "Fix login", Status.NEEDS_YOU, "permission", true, "agent/fix", "/w/fix", "fix-login", false, 1700000000, 1700000100).copy(subagents = listOf(
                    Subagent("Inspect parser", "Explore", true, 1700000010, 1700000100),
                    Subagent("Review parser tests", null, false, 1700000020, 1700000090),
                )),
                Session(4, "opencode", "", Status.ENDED, "", false, null, null, null, true, 1700000200, 1700000300),
            ),
            errors = listOf(LogEntry(1700000400, "codex: exited")),
            startDir = "/home/owner/project",
            usage = Usage(
                listOf(UsageWindow("5h", 42.5, 1700003600)), listOf(UsageWindow("weekly", 3.0, null)),
                grok = listOf(UsageWindow("wk", null, 1700600000)),
                gemini = listOf(UsageWindow("Gemini Models 5h", 25.0, 1700007200)),
                claudeModels = listOf(UsageWindow("Fable wk", 30.0, 1700600000)),
            ),
            outside = listOf(OutsideAgent(4242, "codex", "Refactor auth", false, "/w/auth", null, null, null, 1700000500)),
        )
        assertEquals(state, (Protocol.parse(Protocol.stateJson(state)) as ServerMessage.State).state)
        val hosted = state.copy(
            sessions = state.sessions + Session(
                1099511627777, "codex", "On Fedora", Status.WORKING, "", true, null, null, null, false, 1, 2,
                host = "fedora", hostLabel = "Fedora",
                subagents = listOf(Subagent("Hosted task", null, true, 1, 2)),
            ),
            outside = state.outside + OutsideAgent(4194305, "claude", "", null, "/w", null, null, null, 3, "fedora", "Fedora"),
            hosts = listOf(
                HostSummary("fedora", "Fedora", true, null),
                HostSummary("ctl", "Controller", false, "refused", agents = false),
            ),
            hereLabel = "ThinkPad",
        )
        assertEquals("hosts survive the offline copy", hosted, (Protocol.parse(Protocol.stateJson(hosted)) as ServerMessage.State).state)
    }

    /** The gateway's list when the deck drives other hosts (hub.rs `state_message`). */
    @Test
    fun stateNamesEachSessionsHostAndTheHostsTheDeckDrives() {
        val msg = Protocol.parse(
            """
            {"t":"state","start_dir":"/p","errors":[],"usage":{"claude":[],"codex":[]},"here":"ThinkPad",
             "hosts":[{"name":"fedora","label":"Fedora","online":true,"reason":null},
                      {"name":"ctl","label":"Controller","online":false,"reason":"unreachable","agents":false}],
             "sessions":[{"id":1099511627777,"tool":"claude","title":"x","status":"idle","detail":"","alive":true,
                          "review":false,"created_at":1,"status_since":2,"host":"fedora","host_label":"Fedora"},
                         {"id":2,"tool":"codex","title":"y","status":"idle","detail":"","alive":true,
                          "review":false,"created_at":1,"status_since":2,"host":null,"host_label":null}],
             "outside":[]}
            """.trimIndent(),
        ) as ServerMessage.State
        assertEquals("ThinkPad", msg.state.hereLabel)
        assertEquals(listOf("fedora" to "Fedora", null to null), msg.state.sessions.map { it.host to it.hostLabel })
        assertEquals(
            listOf(
                HostSummary("fedora", "Fedora", true, null),
                HostSummary("ctl", "Controller", false, "unreachable", agents = false),
            ),
            msg.state.hosts,
        )
        val older = Protocol.parse("""{"t":"state","sessions":[],"errors":[],"start_dir":"","usage":{}}""") as ServerMessage.State
        assertEquals("a gateway from before hosts", emptyList<HostSummary>(), older.state.hosts)
    }


    @Test
    fun stats() {
        val msg = Protocol.parse(
            """
            {"t":"stats","rid":8,
             "usage":{"claude":[{"label":"5h","used_percent":42.5,"resets_at":1700003600,"updated_at":1}],
                      "codex":[{"label":"weekly","used_percent":3,"resets_at":null,"updated_at":1}]},
             "cpu":12.5,"memory":61.0,"disk":{"used":442000000000,"available":500000000000}}
            """.trimIndent(),
        )
        assertEquals(
            ServerMessage.Stats(
                rid = 8,
                usage = Usage(listOf(UsageWindow("5h", 42.5, 1700003600)), listOf(UsageWindow("weekly", 3.0, null))),
                cpu = 12.5,
                memory = 61.0,
                disk = DiskUse(442000000000, 500000000000),
            ),
            msg,
        )
        assertEquals(8L, msg.rid)
        assertEquals(942000000000, (msg as ServerMessage.Stats).disk!!.total)
        // Before the gateway's second CPU reading, and where the disk could not be read.
        val first = Protocol.parse(
            """{"t":"stats","rid":9,"usage":{"claude":[],"codex":[]},"cpu":null,"memory":null,"disk":null}""",
        ) as ServerMessage.Stats
        assertNull(first.cpu)
        assertNull(first.memory)
        assertNull(first.disk)
        assertEquals("only the desktop without other hosts", 1, first.machines.size)
    }


    @Test
    fun statsCarryEveryHostsMachineAfterTheDesktops() {
        val msg = Protocol.parse(
            """
            {"t":"stats","rid":3,"usage":{"claude":[],"codex":[]},"cpu":10,"memory":20,"disk":{"used":1,"available":3},
             "label":"ThinkPad",
             "hosts":[{"name":"fedora","label":"Fedora","online":true,"cpu":55.5,"memory":40,"disk":{"used":2,"available":2}},
                      {"name":"ctl","label":"Controller","online":false,"reason":"unreachable","cpu":null,"memory":null,"disk":null}]}
            """.trimIndent(),
        ) as ServerMessage.Stats
        assertEquals(
            listOf(
                HostUse(null, "ThinkPad", true, null, 10.0, 20.0, DiskUse(1, 3)),
                HostUse("fedora", "Fedora", true, null, 55.5, 40.0, DiskUse(2, 2)),
                HostUse("ctl", "Controller", false, "unreachable", null, null, null),
            ),
            msg.machines,
        )
    }


    @Test
    fun statsCarryTheDecksScriptPanels() {
        val msg = Protocol.parse(
            """
            {"t":"stats","rid":4,"usage":{"claude":[],"codex":[]},"cpu":null,"memory":null,"disk":null,
             "panels":[{"id":"tasks","title":"TASKS","lines":[" 3 active · 1 ready"," ✓ agent-deck-lua-api"]},
                       {"id":"load","title":"","lines":[]}]}
            """.trimIndent(),
        ) as ServerMessage.Stats
        assertEquals(
            listOf(
                Panel("tasks", "TASKS", listOf(" 3 active · 1 ready", " ✓ agent-deck-lua-api")),
                Panel("load", "", emptyList()),
            ),
            msg.panels,
        )
        val older = Protocol.parse(
            """{"t":"stats","rid":5,"usage":{"claude":[],"codex":[]},"cpu":null,"memory":null,"disk":null}""",
        ) as ServerMessage.Stats
        assertEquals("a deck from before scripts", emptyList<Panel>(), older.panels)
    }

    /** A terminal is the desktop deck's own and never a phone session. */
    @Test
    fun stateLeavesOutTheDecksTerminals() {
        val msg = Protocol.parse(
            """
            {"t":"state","start_dir":"/p","errors":[],"usage":{"claude":[],"codex":[]},
             "sessions":[{"id":1,"tool":"claude","title":"x","status":"idle","detail":"","alive":true,
                          "review":false,"created_at":1,"status_since":2},
                         {"id":2,"tool":"shell","title":"scratch","status":"idle","detail":"","alive":true,
                          "review":false,"created_at":1,"status_since":2}],
             "outside":[]}
            """.trimIndent(),
        ) as ServerMessage.State
        assertEquals(listOf(1L), msg.state.sessions.map { it.id })
    }

    @Test
    fun alerts() {
        assertEquals(
            ServerMessage.Alert(7, "e1", AlertKind.NEEDS_YOU, 3, "Fix login", "permission", true, 1700000000123),
            Protocol.parse(
                """{"t":"alert","seq":7,"epoch":"e1","kind":"needs_you","id":3,"title":"Fix login","reason":"permission","desk_focused":true,"at":1700000000123}""",
            ),
        )
        val error = Protocol.parse(
            """{"t":"alert","seq":8,"epoch":"e1","kind":"error","id":null,"title":"","reason":"codex: exited","desk_focused":false,"at":1}""",
        ) as ServerMessage.Alert
        assertEquals(AlertKind.ERROR, error.kind)
        assertNull(error.id)
        assertEquals(AlertKind.FINISHED, AlertKind.of("finished"))
        assertEquals(false, error.replay)
    }

    @Test
    fun whatCameBeforeTheConnectionIsMarkedReplay() {
        val alert = Protocol.parse(
            """{"t":"alert","seq":9,"epoch":"e1","kind":"finished","id":3,"title":"Fix login","reason":"","desk_focused":false,"at":1,"replay":true}""",
        ) as ServerMessage.Alert
        assertEquals(true, alert.replay)
        val prompt = Protocol.parse(
            """{"t":"prompt","id":3,"inputs":1,"fp":"aa","replay":true,"prompt":{"kind":"permission","question":"Run ls?","options":[{"label":"Yes"}],"selected":0}}""",
        ) as ServerMessage.PromptUpdate
        assertEquals(true, prompt.replay)
        val live = Protocol.parse("""{"t":"prompt","id":3,"inputs":1,"fp":"aa","prompt":null}""") as ServerMessage.PromptUpdate
        assertEquals(false, live.replay)
    }

    @Test
    fun promptAndItsEnd() {
        val msg = Protocol.parse(
            """
            {"t":"prompt","id":3,"inputs":41,"fp":"9f3a","prompt":{"kind":"question","question":"Which one?",
             "options":[{"label":"Red","detail":"warm"},{"label":"Blue"},{"label":"Type something."}],
             "selected":1,"free_text":2,"multi":false}}
            """.trimIndent(),
        ) as ServerMessage.PromptUpdate
        assertEquals(3L, msg.id)
        assertEquals(41L, msg.inputs)
        assertEquals("9f3a", msg.fp)
        assertEquals(
            Prompt(
                "question",
                "Which one?",
                listOf(PromptOption("Red", "warm"), PromptOption("Blue", ""), PromptOption("Type something.", "")),
                1,
                2,
                false,
            ),
            msg.prompt,
        )
        assertEquals(WaitingPrompt(3, 41, "9f3a", msg.prompt!!), msg.waiting)

        // A permission prompt has no free text; serde leaves the field out.
        val permission = Protocol.parse(
            """{"t":"prompt","id":5,"inputs":2,"fp":"aa","prompt":{"kind":"permission","question":"Run ls?","options":[{"label":"Yes"}],"selected":0,"multi":true}}""",
        ) as ServerMessage.PromptUpdate
        assertNull(permission.prompt!!.freeText)
        assertTrue(permission.prompt.multi)
        assertNull(permission.prompt.submit)
        assertNull(permission.prompt.options[0].checked)

        // A several-choice question: each box says whether it is ticked, and Submit is named.
        val several = Protocol.parse(
            """{"t":"prompt","id":5,"inputs":2,"fp":"bb","prompt":{"kind":"question","question":"Pick",""" +
                """"options":[{"label":"Cheese","checked":true},{"label":"Olives","checked":false},{"label":"Chat about this"}],""" +
                """"selected":0,"multi":true,"submit":"Next"}}""",
        ) as ServerMessage.PromptUpdate
        assertEquals(
            listOf(PromptOption("Cheese", "", true), PromptOption("Olives", "", false), PromptOption("Chat about this", "")),
            several.prompt!!.options,
        )
        assertEquals("Next", several.prompt.submit)

        val gone = Protocol.parse("""{"t":"prompt","id":3,"prompt":null}""") as ServerMessage.PromptUpdate
        assertEquals(3L, gone.id)
        assertNull(gone.prompt)
        assertNull(gone.waiting)
    }

    @Test
    fun everyItemKind() {
        val msg = Protocol.parse(
            """
            {"t":"items","id":3,"reset":false,"start":10,"total":17,"generation":2,"model":"opus","effort":"high","items":[
              {"k":"user","text":"hi","at":1700000000000},
              {"k":"assistant","text":"**hello**"},
              {"k":"thinking","text":"hmm"},
              {"k":"tool","call":"c1","name":"Bash","summary":"ls","input":"{\"command\":\"ls\"}"},
              {"k":"result","call":"c1","text":"a\nb","error":true},
              {"k":"notice","text":"Compacted"},
              {"k":"screen","text":"> _"},
              {"k":"hologram","text":"?"}]}
            """.trimIndent(),
        ) as ServerMessage.Items
        assertEquals(
            listOf(
                Item.User("hi", 1700000000000),
                Item.Assistant("**hello**", null),
                Item.Thinking("hmm"),
                Item.Tool("c1", "Bash", "ls", "{\"command\":\"ls\"}"),
                Item.Result("c1", "a\nb", true),
                Item.Notice("Compacted"),
                Item.Screen("> _"),
                Item.Unknown("hologram"),
            ),
            msg.items,
        )
        assertEquals(10, msg.start)
        assertEquals(17, msg.total)
        assertEquals(2L, msg.generation)
        assertEquals("opus", msg.model)
        assertEquals("high", msg.effort)
        assertFalse(msg.reset)
    }

    @Test
    fun screenItemsCarryNoGeneration() {
        val msg = Protocol.parse("""{"t":"items","id":9,"reset":true,"start":0,"total":1,"items":[{"k":"screen","text":"$ "}]}""")
            as ServerMessage.Items
        assertTrue(msg.reset)
        assertNull(msg.generation)
        assertNull(msg.model)
    }

    @Test
    fun itemsSurviveTheOfflineCopy() {
        val items = listOf(
            Item.User("hi", 5),
            Item.Assistant("yo", null),
            Item.Thinking("t"),
            Item.Tool("c", "Read", "a.txt", "{}"),
            Item.Result("c", "x", false),
            Item.Notice("n"),
            Item.Screen("s"),
        )
        assertEquals(items, items.map { Protocol.item(Protocol.itemJson(it)) })
    }

    @Test
    fun chatPages() {
        assertEquals(
            ServerMessage.Chat(12, 3, 40, 100, 1, "log", listOf(Item.User("hi", null)), "gpt-5.5", null),
            Protocol.parse(
                """{"t":"chat","rid":12,"id":3,"start":40,"total":100,"generation":1,"source":"log","items":[{"k":"user","text":"hi"}],"model":"gpt-5.5","effort":null}""",
            ),
        )
        val none = Protocol.parse("""{"t":"chat","id":4,"start":0,"total":0,"items":[],"source":"none","generation":0,"rid":13}""")
            as ServerMessage.Chat
        assertEquals("none", none.source)
        assertEquals(13L, none.rid)
    }

    @Test
    fun repliesAndNotices() {
        assertEquals(ServerMessage.Notice("#3: no such worktree", true), Protocol.parse("""{"t":"notice","text":"#3: no such worktree","error":true}"""))
        assertEquals(ServerMessage.Pong(4), Protocol.parse("""{"t":"pong","rid":4}"""))
        assertEquals(ServerMessage.Ok(5, null), Protocol.parse("""{"t":"ok","rid":5}"""))
        assertEquals(ServerMessage.Ok(6, 11), Protocol.parse("""{"t":"ok","rid":6,"id":11}"""))
        // An upload's token, then each piece's reply: the saved path once the file is whole.
        assertEquals(
            ServerMessage.Ok(12, null, upload = "u-0a1b", path = null),
            Protocol.parse("""{"t":"ok","rid":12,"upload":"u-0a1b","path":null}"""),
        )
        assertEquals(
            ServerMessage.Ok(13, null, path = "/s/uploads/7/photo.jpg"),
            Protocol.parse("""{"t":"ok","rid":13,"path":"/s/uploads/7/photo.jpg"}"""),
        )
        assertEquals(ServerMessage.Err(7, "no such session"), Protocol.parse("""{"t":"err","rid":7,"error":"no such session"}"""))
        assertEquals(ServerMessage.Err(null, "bad request: x"), Protocol.parse("""{"t":"err","rid":null,"error":"bad request: x"}"""))
        assertEquals(
            ServerMessage.Err(8, "no question is waiting", Protocol.QUESTION_STALE),
            Protocol.parse("""{"t":"err","rid":8,"error":"no question is waiting","code":"question_stale"}"""),
        )
        assertEquals(5L, Protocol.parse("""{"t":"ok","rid":5}""").rid)
        assertNull(Protocol.parse("""{"t":"notice","text":"x","error":false}""").rid)
    }

    @Test
    fun models() {
        val msg = Protocol.parse(
            """
            {"t":"models","rid":2,"tools":[
              {"tool":"claude","models":[{"id":"opus","label":"Opus"},{"id":"sonnet","label":"Sonnet"}],"efforts":["low","high"],"switch":true,
               "default_model":"opus","default_effort":"xhigh"},
              {"tool":"codex","models":[{"id":"gpt-5.5","label":"GPT-5.5","efforts":["medium","xhigh"]}],"efforts":[],"switch":true,
               "default_model":"gpt-5.5","default_effort":"xhigh"},
              {"tool":"grok","models":[],"efforts":[],"switch":false}]}
            """.trimIndent(),
        ) as ServerMessage.Models
        assertEquals(2L, msg.rid)
        val (claude, codex, grok) = msg.tools
        assertEquals(listOf(ModelChoice("opus", "Opus", emptyList()), ModelChoice("sonnet", "Sonnet", emptyList())), claude.models)
        assertEquals(listOf("low", "high"), claude.effortsFor("opus"))
        assertEquals(listOf("medium", "xhigh"), codex.effortsFor("gpt-5.5"))
        assertTrue(codex.switch)
        assertFalse(grok.switch)
        assertEquals(emptyList<ModelChoice>(), grok.models)

        // claude/codex": the programs' own defaults are named, not just "Default".
        assertEquals("Default · Opus", claude.defaultModelLabel())
        assertEquals("Default · xhigh", claude.defaultEffortLabel())
        assertEquals("Default · GPT-5.5", codex.defaultModelLabel())
        assertEquals("the default model's efforts", listOf("medium", "xhigh"), codex.effortsFor(null))
        assertEquals("Default", grok.defaultModelLabel())
        assertEquals("Default", grok.defaultEffortLabel())
    }

    @Test
    fun unknownAndBrokenFrames() {
        assertEquals(ServerMessage.Unknown("weather"), Protocol.parse("""{"t":"weather"}"""))
        assertEquals(ServerMessage.Unknown("invalid"), Protocol.parse("""{"t":"""))
        assertEquals(ServerMessage.Unknown("invalid"), Protocol.parse("[]"))
    }
}
