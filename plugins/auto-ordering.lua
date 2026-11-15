-- Owner, 2026-11-05: "each session idle or not should be grouped by type
-- first and formost, then by activity". Each agent type keeps the place it
-- already has in the list (a manual move of a type changes it).
-- Owner, 2026-11-12: "it should order ones that are not working and move
-- those to the bottom, not move every working agent around non stop".
-- Within a type, sessions at work (working, or asking you something
-- mid-task) keep their places relative to each other and are never sorted
-- among themselves. Only the rest are sorted, below them: starting ones,
-- then idle conversations, and cleared or unused ones last; each keeps its
-- place relative to the others of its kind.
-- A session that stops working keeps its place for HOLD seconds, so a turn
-- that a Stop hook sends on moves nothing: Claude shows it finished for up
-- to 15 seconds first (`FINISH_SETTLE_MS` in status.rs). A clear ends the
-- wait at once.
-- It sorts only when a session starts, ends, changes status or is cleared,
-- or a wait ends, never for other state updates.
local deck = require("deck")
local path = deck.config_dir .. "/auto-ordering.json"
local HOLD = 20
local enabled = true
local due = false
-- Sessions that stopped working: the seconds each keeps its place.
local held = {}
local ticking = nil
local apply

local function read_settings()
  local file = io.open(path, "r")
  if not file then return end
  local ok, saved = pcall(deck.json.decode, file:read(4096))
  file:close()
  if ok and type(saved) == "table" then enabled = saved.enabled ~= false end
end
read_settings()

local function at_work(status)
  return status == "working" or status == "needs_you"
end

local function activity(session)
  if at_work(session.status) then return 0 end
  -- Nothing has been written in a cleared (or never used) conversation.
  if session.status == "idle" and (session.last_active or 0) == 0 then return 3 end
  if held[session.id] then return 0 end
  if session.status == "starting" then return 1 end
  return 2
end

local function stop_ticking()
  if ticking and next(held) == nil then
    deck.cancel(ticking)
    ticking = nil
  end
end

local function count_down()
  local over = false
  for id, left in pairs(held) do
    if left > 1 then
      held[id] = left - 1
    else
      held[id], over = nil, true
    end
  end
  stop_ticking()
  if over then apply() end
end

apply = function()
  if not enabled then return end
  local state = deck.state({"sessions", "order", "host_order"}) or {}
  local by_id, sessions, positions = {}, {}, {}
  local hosts, types, counts = {[""] = 0}, {}, {}
  for i, name in ipairs(state.host_order or {}) do hosts[name] = i end
  for _, session in ipairs(state.sessions or {}) do by_id[session.id] = session end
  for id in pairs(held) do
    local session = by_id[id]
    if not session or not session.alive or activity(session) == 3 then held[id] = nil end
  end
  stop_ticking()
  local function add(session)
    if not session or not session.alive or positions[session.id] then return end
    local host = session.host or ""
    if not types[host] then types[host], counts[host] = {}, 0 end
    if not types[host][session.tool] then
      counts[host] = counts[host] + 1
      types[host][session.tool] = counts[host]
    end
    sessions[#sessions + 1] = session
    positions[session.id] = #sessions
  end
  for _, id in ipairs(state.order or {}) do add(by_id[id]) end
  for _, session in ipairs(state.sessions or {}) do add(session) end
  table.sort(sessions, function(a, b)
    local ah, bh = a.host or "", b.host or ""
    if ah ~= bh then
      local ar, br = hosts[ah] or math.huge, hosts[bh] or math.huge
      if ar ~= br then return ar < br end
      return ah < bh
    end
    local at, bt = types[ah][a.tool], types[bh][b.tool]
    if at ~= bt then return at < bt end
    local aa, ba = activity(a), activity(b)
    if aa ~= ba then return aa < ba end
    return positions[a.id] < positions[b.id]
  end)
  local ids, changed = {}, false
  for i, session in ipairs(sessions) do
    ids[i] = session.id
    if not state.order or state.order[i] ~= session.id then changed = true end
  end
  if changed then deck.reorder(ids) end
end

-- Several sessions can change in one update; sort once, after them all.
local function mark()
  due = enabled
end

local function status_changed(change)
  local session = change.session
  if at_work(change.to) then
    held[session.id] = nil
    stop_ticking()
  elseif enabled and session.alive and at_work(change.from) then
    held[session.id] = HOLD
    ticking = ticking or deck.every(1, count_down)
  end
  mark()
end

deck.command("auto-ordering", function(args)
  read_settings()
  local action = args[1] or "toggle"
  if #args > 1 or not ({on = true, off = true, toggle = true, status = true})[action] then
    deck.notify("Use :auto-ordering [on|off|toggle|status]", {error = true})
    return
  end
  if action ~= "status" then
    local next_enabled = action == "on" or (action == "toggle" and not enabled)
    local output, why = io.open(path .. ".next", "w")
    if not output then deck.notify(tostring(why), {error = true}); return end
    local written, write_error = output:write(deck.json.encode({enabled = next_enabled}))
    local closed, close_error = output:close()
    if not written or not closed then
      deck.notify(tostring(write_error or close_error), {error = true}); return
    end
    local renamed, rename_error = os.rename(path .. ".next", path)
    if not renamed then deck.notify(tostring(rename_error), {error = true}); return end
    enabled = next_enabled
    held = {}
    stop_ticking()
    apply()
  end
  deck.notify("Auto ordering " .. (enabled and "on" or "off"))
end, {desc = "Group sessions by type, then activity", complete = {"on", "off", "toggle", "status"}})

deck.on("ready", apply)
deck.on("session_new", mark)
deck.on("session_end", mark)
deck.on("status", status_changed)
deck.on("cleared", mark)
deck.on("state", function()
  if due then
    due = false
    apply()
  end
end)
