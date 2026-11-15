-- The number on the repository title row: branches with unmerged work
-- checked out on any host, each counted once. A worktree whose branch has
-- merged into its main checkout's branch does not count, nor does one on no
-- branch (detached), nor a branch that no worktree has checked out.
local deck = require("deck")

local ID = "unmerged-branches"
local shown = 0

-- `lists` holds each host's branches: the name, the worktree that has it
-- checked out (empty for none), commits ahead of the main checkout's
-- branch, and whether it is that main checkout.
local function unmerged(lists)
  local seen, n = {}, 0
  for _, list in ipairs(lists) do
    for _, b in ipairs(list) do
      if not b.main and (b.path or "") ~= "" and (b.ahead or 0) > 0 and not seen[b.branch] then
        seen[b.branch] = true
        n = n + 1
      end
    end
  end
  return n
end

-- A host connecting, or connected but not yet reporting, has branches the
-- count cannot know while hosts reconnect after a service update. A host known
-- to be down counts what it last reported; a build host reports none.
local function unknown(host)
  return host.role ~= "build" and host.stats == nil and (host.online or host.reason == "connecting")
end

-- Count again, keeping the number shown while any host's branches are unknown.
local function recount()
  local state = deck.state({ "branches", "hosts" }) or {}
  local lists = { state.branches or {} }
  for _, host in ipairs(state.hosts or {}) do
    if unknown(host) then return end
    lists[#lists + 1] = (host.stats or {}).branches or {}
  end
  local n = unmerged(lists)
  if n == shown then return end
  shown = n
  if n == 0 then
    deck.ui.remove(ID)
  else
    -- Show the count after the branch and its ⇡⇣ counts.
    deck.ui.section(ID, { title = "Unmerged branches", header = "inline", lines = { { { tostring(n), "strong" } } } })
  end
end

-- Every host's statistics tick is a state change, and reading `hosts` copies
-- each host's branch list. A change is counted at once; those in the next
-- CATCH_UP seconds are counted together when they end, and a still state is
-- not counted at all.
local CATCH_UP = 5
local holding, missed = false, false

local function hold()
  holding = true
  deck.after(CATCH_UP, function()
    holding = false
    if missed then
      missed = false
      recount()
      hold()
    end
  end)
end

deck.on("ready", recount)
deck.on("state", function()
  if holding then
    missed = true
    return
  end
  recount()
  hold()
end)
