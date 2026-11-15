-- Check/update installed agents on this machine and every configured SSH host.
local deck = require("deck")
deck.command("agent-update", function(args)
  local cmd = {deck.exe, "agent-update", "--config", deck.config_dir .. "/config.json"}
  for _, argument in ipairs(args) do
    if argument == "check" then cmd[#cmd + 1] = "--check"
    elseif argument == "local" then cmd[#cmd + 1] = "--local"
    else deck.notify("Use :agent-update [check] [local]", {error = true}); return end
  end
  deck.term({name = "Agent updates", cmd = cmd})
end, {desc = "Check and update Grok, Claude, Antigravity and Codex on every host", complete = {"check", "local"}})
