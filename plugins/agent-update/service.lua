-- When a Claude or Codex session says a newer version of its program is out,
-- update that program on this machine in the background (owner, 2026-11-12:
-- "when claude code shows an update is avaiable - the service should run the
-- update in the background, not hit their api and check every single
-- session"). Launches no longer check for updates; the agent's own notice,
-- which the service hears as `update_available`, is the trigger. Only a
-- failure is announced.
local deck = require("deck")
if deck.context ~= "service" then return false end

local NAMES = { claude = "Claude", codex = "Codex" }
-- Between two runs for one program: a release its installer does not list
-- yet is asked about again at most this often (seconds).
local SPACING = 30 * 60

local running = {}
-- When the last run for each program ended (Unix seconds).
local ended = {}

local function failure(tool, result)
  if result.timed_out then return "timed out" end
  local line = ("\n" .. (result.stdout or "")):match("\n%s*" .. tool .. ": failed ([^\n]*)")
  if line then return line end
  local err = (result.stderr or ""):match("[^\n]+")
  return err or ("exited with status " .. tostring(result.code))
end

deck.on("update_available", function(session)
  local tool = session.tool
  if not NAMES[tool] or running[tool] then return end
  local now = os.time()
  if ended[tool] then
    -- A session started before the last run ended has the version from
    -- before it, so its notice is no news.
    local started = (session.spawned_ms or 0) > 0 and session.spawned_ms / 1000 or (session.created_at or 0)
    if started <= ended[tool] or now - ended[tool] < SPACING then return end
  end
  running[tool] = true
  local launched = pcall(deck.job, { deck.exe, "agent-update", "--local", "--tool", tool }, { timeout = 300 },
    function(result)
      running[tool] = nil
      ended[tool] = os.time()
      -- The owner's own `:agent-update` holds the machine's lock and updates it too.
      if result.ok or (result.stdout or ""):find("An agent update is already running", 1, true) then return end
      deck.notify(NAMES[tool] .. " background update failed: " .. failure(tool, result)
        .. ". Run :agent-update local to retry.", { error = true })
    end)
  -- Eight jobs already running, or the service stopping: a later notice tries again.
  if not launched then running[tool] = nil end
end)
