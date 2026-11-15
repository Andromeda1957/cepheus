-- Persistent display preferences. These never start or stop the dev server.
local deck = require("deck")
local path = deck.config_dir .. "/sidebar-controls.json"
local names = {resources = "resources", ["dev-indicator"] = "dev"}
local shown = {resources = true, dev = true}

local function read_settings()
  local file = io.open(path, "r")
  if not file then return {} end
  local content = file:read("*a")
  file:close()
  local ok, value = pcall(deck.json.decode, content)
  return ok and type(value) == "table" and value or {}
end

local function apply()
  local hidden = {}
  for _, id in ipairs(deck.opt.sidebar_hidden or {}) do
    if shown[id] == nil then hidden[#hidden + 1] = id end
  end
  for _, id in ipairs({"resources", "dev"}) do
    if not shown[id] then hidden[#hidden + 1] = id end
  end
  deck.opt.sidebar_hidden = hidden
end

local saved = read_settings()
for id in pairs(shown) do shown[id] = saved[id] ~= false end
apply()

local function save(id, enabled)
  local value = read_settings()
  value[id] = enabled
  local file, error = io.open(path .. ".next", "w")
  if not file then deck.notify("Sidebar setting: " .. tostring(error), {error = true}); return false end
  local written, write_error = file:write(deck.json.encode(value))
  local closed, close_error = file:close()
  if not written or not closed then
    deck.notify("Sidebar setting: " .. tostring(write_error or close_error), {error = true})
    return false
  end
  local renamed, rename_error = os.rename(path .. ".next", path)
  if not renamed then deck.notify("Sidebar setting: " .. tostring(rename_error), {error = true}); return false end
  shown[id] = enabled
  apply()
  return true
end

for name, part in pairs(names) do
  local command, id = name, part
  deck.command(command, function(args)
    local action = args[1] or "toggle"
    if action ~= "on" and action ~= "off" and action ~= "toggle" then
      deck.notify("Use :" .. command .. " [on|off|toggle]", {error = true})
      return
    end
    local enabled = action == "on" or (action == "toggle" and not shown[id])
    if save(id, enabled) then deck.notify(command .. " " .. (enabled and "on" or "off")) end
  end, {desc = "Show or hide " .. command, complete = {"on", "off", "toggle"}})
end
