-- One persistent, live API-equivalent total across every deck host.
local deck = require("deck")
local config = deck.config_dir
local home = os.getenv("HOME") or ""
local state_base = os.getenv("XDG_STATE_HOME") or (home .. "/.local/state")
-- The state folder from before the rename to Cepheus keeps the running total.
local function exists(path) return os.rename(path, path) ~= nil end
local state_folder = state_base .. "/cepheus"
if not exists(state_folder) and exists(state_base .. "/agent-deck") then -- legacy-name
  state_folder = state_base .. "/agent-deck" -- legacy-name
end
local state_dir = state_folder .. "/api-cost"
local settings_file = config .. "/api-cost.json"
local enabled, busy, last_error = true, false, nil
local value = { total = 0, pending = 1, models = {}, unknown = {}, peers = {} }

local function read_json(path)
  local f = io.open(path, "r")
  if not f then return nil end
  local content = f:read("*a")
  f:close()
  local ok, data = pcall(deck.json.decode, content)
  if ok and type(data) == "table" then return data end
end

local settings = read_json(settings_file)
if settings and settings.enabled == false then enabled = false end
value = read_json(state_dir .. "/summary.json") or value

local function money(amount)
  local whole, cents = string.format("%.2f", amount or 0):match("^(%d+)(%.%d+)$")
  if not whole then return "$?" end
  whole = whole:reverse():gsub("(%d%d%d)", "%1,"):reverse():gsub("^,", "")
  return "$" .. whole .. cents
end

local function compact_money(amount)
  amount = amount or 0
  if amount < 1000 then return "$" .. math.floor(amount) end
  local tenths = math.floor(amount / 100)
  return string.format("$%d.%dk", tenths // 10, tenths % 10)
end

local function draw()
  if not enabled then deck.ui.remove("api-cost"); return end
  deck.ui.section("api-cost", {
    title = "", pinned = true, phone = true,
    lines = function()
      return { { { " API Cost: ", "dim" }, { compact_money(value.total), "ok" } } }
    end,
  })
end

local function save_enabled(next_value)
  local f, err = io.open(settings_file .. ".next", "w")
  if not f then deck.notify("API cost setting: " .. tostring(err), {error=true}); return false end
  local ok, write_err = f:write(deck.json.encode({enabled=next_value}))
  local closed, close_err = f:close()
  if not ok or not closed then
    deck.notify("API cost setting: " .. tostring(write_err or close_err), {error=true})
    return false
  end
  local renamed, rename_err = os.rename(settings_file .. ".next", settings_file)
  if not renamed then deck.notify("API cost setting: " .. tostring(rename_err), {error=true}); return false end
  enabled = next_value
  return true
end

local function refresh(force)
  if busy or not enabled then return end
  busy = true
  local ok, err = pcall(deck.service.call, "api-cost", force and "refresh" or "summary", {}, function(result)
    busy = false
    if result.ok then
      local data = result.data
      if type(data) == "table" and type(data.total) == "number" then
        value, last_error = data, nil
      else last_error = "invalid collector response" end
    else last_error = tostring(result.error or "collector unavailable") .. "; showing last total" end
    draw()
  end)
  if not ok then busy = false; last_error = tostring(err); draw() end
end

local function details(by_model)
  local items = { "API equivalent: ~" .. money(value.total) .. " USD" }
  if by_model then
    local models = {}
    for model, cost in pairs(value.models or {}) do models[#models+1] = {model, cost} end
    table.sort(models, function(a,b) return a[2] > b[2] end)
    for _, item in ipairs(models) do
      items[#items+1] = money(item[2]) .. "  " .. item[1]
    end
  else
    items[#items+1] = "Local history" .. ((value.pending or 0) > 0 and ": indexing" or ": up to date")
    for _, peer in ipairs(value.peers or {}) do
      items[#items+1] = peer.name .. ": " .. (peer.status ~= "online" and "offline (last known usage)" or (peer.pending or 0) > 0 and "indexing" or "up to date")
    end
    items[#items+1] = "Subscription usage valued at published API rates"
    items[#items+1] = "Total before subtracting subscription payments"
    items[#items+1] = "Claude + Codex history, including subagents"
  end
  for model, count in pairs(value.unknown or {}) do
    items[#items+1] = "Unpriced: " .. model .. " (" .. count .. " records)"
  end
  if last_error then items[#items+1] = last_error end
  deck.ui.pick({title="API cost estimate", items=items, on_choose=function() end})
end

deck.command("api-cost", function(args)
  local action = args[1] or "toggle"
  if action == "on" or action == "off" or action == "toggle" then
    local next_value = action == "on" or (action == "toggle" and not enabled)
    if save_enabled(next_value) then
      draw()
      if enabled then refresh() end
      deck.notify("API cost " .. (enabled and "on" or "off"))
    end
  elseif action == "models" then details(true)
  elseif action == "status" then details(false)
  elseif action == "refresh" then refresh(true)
  else deck.notify("Use :api-cost [on|off|toggle|status|models|refresh]", {error=true}) end
end, {desc="Toggle the live API-equivalent grand total", complete={"on","off","toggle","status","models","refresh"}})

draw()
deck.on("ready", function() refresh() end)
deck.every(30, function() refresh() end)
