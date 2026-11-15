-- An example ~/.config/cepheus/init.lua. The deck loads init.lua when it
-- starts and on :reload, then every plugins/*.lua beside it; `require`
-- finds modules in lua/. An error goes to F2 and the deck keeps its own
-- keys and commands.

local deck = require("deck")

-- Options. `sidebar` orders the sidebar: the built-in parts are "title",
-- "resources", "dev", "usage" and "sessions"; any other name is a section a
-- script adds. Parts left out are not drawn.
deck.opt.sidebar = { "title", "resources", "dev", "usage", "sessions" }

-- Keys: a command line, or a function.
deck.keymap("alt+c", "new claude", { desc = "new Claude Code session here" })
deck.keymap("alt+g", function()
  deck.cmd("ask /web " .. os.date("what happened in tech news on %Y-%m-%d?"))
end, { desc = "today's tech news" })

-- A command of your own: :fleet starts a Codex session on every online host.
deck.command("fleet", function(args)
  local tool = args[1] or "codex"
  for _, host in ipairs(deck.state().hosts or {}) do
    if host.online then
      deck.new(tool, { host = host.name })
    end
  end
end, { desc = "start a session on every online host", complete = { "claude", "codex", "agy" } })

-- A section of your own, shown in the sidebar and on the phone's stats.
local load = "…"
deck.every(10, function()
  deck.job({ "cat", "/proc/loadavg" }, {}, function(r)
    if r.ok then
      load = r.stdout:match("^(%S+ %S+ %S+)") or load
    end
  end)
end)
deck.ui.section("load", {
  title = "LOAD",
  lines = function()
    return { { { " " .. load, "accent" } } }
  end,
})

-- A badge after a session's details: here, how many sub-agents run.
deck.ui.badge(function(s)
  local n = 0
  for _, a in ipairs(s.subagents or {}) do
    if a.active then
      n = n + 1
    end
  end
  if n >= 3 then
    return { " busy", "warn" }
  end
end)

-- Events: "ready", "state", "status" (a session changed), "finished",
-- "needs_you", "session_new" and "session_end".
deck.on("finished", function(s)
  deck.notify(s.title .. " finished.")
end)
