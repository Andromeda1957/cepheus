-- :agent-plugins lists every plugin Claude and Codex know, with its state;
-- :agent-plugins on|off NAME [claude|codex] turns one on or off in the agent's own
-- settings: Claude's settings.json `enabledPlugins` and Codex's config.toml
-- `[plugins."NAME@SOURCE"] enabled`. New sessions use the change.
local deck = require("deck")
local files = require("agent_settings.files")
local json = require("agent_settings.json")
local toml = require("agent_settings.toml")

local USAGE = "Use :plugin on|off NAME [claude|codex]"
local LABEL = {claude = "Claude", codex = "Codex"}
local AGENTS = {"claude", "codex"}

-- What one agent knows: each plugin installed or named in its settings.
local load = {}

function load.claude()
  local dir = files.claude_dir()
  local path = files.resolved(files.join(dir, "settings.json"))
  if not path then return nil, "Claude settings path cannot be resolved" end
  local text, problem = files.read(path)
  if problem then return nil, "Claude settings cannot be read" end
  local settings = text == nil and json.object() or json.decode(text)
  if not settings then return nil, "Claude settings must contain valid JSON" end
  local enabled = json.get(settings, "enabledPlugins")
  if settings.t ~= "object" or (enabled and enabled.t ~= "object") then
    return nil, "Claude settings and its enabledPlugins entry must be objects"
  end
  local plugins = {}
  for id, node in pairs(enabled and enabled.map or {}) do plugins[id] = node end
  local installed = deck.fs.read(files.join(dir, "plugins/installed_plugins.json"), 4 << 20)
  local ok, record = pcall(deck.json.decode, installed or "{}")
  if ok and type(record) == "table" and type(record.plugins) == "table" then
    for id in pairs(record.plugins) do
      if type(id) == "string" then plugins[id] = plugins[id] or false end
    end
  end
  return {path = path, text = text, settings = settings, plugins = plugins}
end

function load.codex()
  local dir = files.codex_dir()
  local path = files.resolved(files.join(dir, "config.toml"))
  if not path then return nil, "Codex config path cannot be resolved" end
  local text, problem = files.read(path)
  if problem then return nil, "Codex config cannot be read" end
  local doc, invalid = toml.parse(text or "")
  if not doc then return nil, invalid end
  local section = toml.get(doc.root, "plugins")
  if section ~= nil and not toml.is_table(section) then return nil, "Codex plugins setting must be a table" end
  local plugins = {}
  for _, id in ipairs(section and toml.keys(section) or {}) do plugins[id] = toml.get(section, id) end
  -- Installed: plugins/cache/SOURCE/NAME/VERSION/.codex-plugin/plugin.json.
  local cache = files.join(dir, "plugins/cache")
  for _, source in ipairs(deck.fs.list(cache) or {}) do
    for _, plugin in ipairs(deck.fs.list(files.join(cache, source.name)) or {}) do
      local folder = files.join(files.join(cache, source.name), plugin.name)
      for _, version in ipairs(deck.fs.list(folder) or {}) do
        if deck.fs.stat(files.join(folder, version.name .. "/.codex-plugin/plugin.json")) then
          local id = plugin.name .. "@" .. source.name
          plugins[id] = plugins[id] or false
          break
        end
      end
    end
  end
  return {path = path, text = text, doc = doc, plugins = plugins}
end

-- A plugin's state as each agent reads it: Claude's is on only when set to
-- true; Codex's is off only when set to false.
local state = {}

function state.claude(node)
  if not node then return "off (not set)", "dim" end
  if node.t == "true" then return "on", "ok" end
  if node.t == "false" then return "off", "warn" end
  return "not true or false", "bad"
end

local function codex_enabled(node)
  if not node then return nil end
  if not toml.is_table(node) then return "invalid" end
  local value = toml.get(node, "enabled")
  if value == nil then return nil end
  if value.vtype ~= "boolean" then return "invalid" end
  return value.bool
end

function state.codex(node)
  local enabled = codex_enabled(node)
  if enabled == nil then return "on (default)", "dim" end
  if enabled == true then return "on", "ok" end
  if enabled == false then return "off", "warn" end
  return "not true or false", "bad"
end

-- The new text that turns `id` on or off; false when it already is; nil and
-- a reason when its setting is not one this can change.
local change = {}

function change.claude(known, id, on)
  local node = known.plugins[id]
  if node and node.t ~= "true" and node.t ~= "false" then
    return nil, ("Claude's enabledPlugins setting for %s is not true or false"):format(id)
  end
  if (node and node.t == "true") == on then return false end
  json.put(json.member(known.settings, "enabledPlugins"), id, json.boolean(on))
  local text = json.encode(known.settings)
  local edited = json.decode(text)
  local plugins = edited and json.get(edited, "enabledPlugins")
  local value = plugins and json.get(plugins, id)
  if not value or value.t ~= tostring(on) then
    return nil, "Claude settings could not be changed safely; they were left as they were"
  end
  return text
end

function change.codex(known, id, on)
  local enabled = codex_enabled(known.plugins[id])
  if enabled == "invalid" then
    return nil, ("Codex's enabled setting for %s is not true or false"):format(id)
  end
  if (enabled ~= false) == on then return false end
  local text, problem = toml.set(known.doc, {"plugins", id, "enabled"}, on and "true" or "false", "boolean")
  if not text then return nil, problem end
  -- The edited text must read back with the asked state before it is saved.
  local edited = toml.parse(text)
  local section = edited and toml.get(edited.root, "plugins")
  if not section or codex_enabled(toml.get(section, id)) ~= on then
    return nil, "Codex config could not be changed safely; it was left as it was"
  end
  return text
end

-- The plugin `name` means for one agent: its exact id, or the one id whose
-- name before the @ it is. Also the ids when several match.
local function find(known, name)
  if known.plugins[name] ~= nil then return name end
  if name:find("@", 1, true) then return nil end
  local matches = {}
  for id in pairs(known.plugins) do
    if id:match("^(.-)@") == name then matches[#matches + 1] = id end
  end
  table.sort(matches)
  if #matches == 1 then return matches[1] end
  if #matches > 1 then return nil, matches end
end

local function refuse(text)
  deck.notify("Plugin: " .. text, {error = true})
end

local function toggle(args, usage)
  local action, name, agent = args[1], args[2], args[3]
  if #args < 2 or #args > 3 or (action ~= "on" and action ~= "off") or (agent and not LABEL[agent]) then
    deck.notify(usage, {error = true})
    return
  end
  local on, targets = action == "on", {}
  -- Every named agent's settings must be readable and every change possible
  -- before any file is written.
  for _, key in ipairs(agent and {agent} or AGENTS) do
    local known, problem = load[key]()
    if not known then return refuse(problem) end
    local id, matches = find(known, name)
    if matches then
      return refuse(("%s has several plugins named %s: %s; use the full name"):format(
        LABEL[key], name, table.concat(matches, ", ")))
    end
    if id then targets[#targets + 1] = {key = key, id = id, known = known} end
  end
  if #targets == 0 then
    if agent then return refuse(("%s has no plugin named %s"):format(LABEL[agent], name)) end
    return refuse("No Claude or Codex plugin is named " .. name)
  end
  for _, target in ipairs(targets) do
    local text, problem = change[target.key](target.known, target.id, on)
    if text == nil then return refuse(problem) end
    target.text = text
  end
  local said, changed = {}, false
  for _, target in ipairs(targets) do
    if target.text then
      local saved, reason = files.replace(target.known.path, target.known.text, target.text)
      if not saved then
        if changed then return refuse(table.concat(said, "; ") .. "; " .. reason) end
        return refuse(reason)
      end
      changed = true
    end
    said[#said + 1] = ("%s: %s is %s %s"):format(LABEL[target.key], target.id, target.text and "now" or "already", action)
  end
  deck.notify(table.concat(said, "; ") .. (changed and ". Reopen running sessions to apply it." or "."))
end

local function list(args, usage)
  if #args > 0 then
    deck.notify(usage, {error = true})
    return
  end
  local items = {}
  for _, key in ipairs(AGENTS) do
    items[#items + 1] = {LABEL[key], heading = true}
    local known, problem = load[key]()
    if not known then
      items[#items + 1] = {{problem, "bad"}, inert = true}
    else
      local ids = {}
      for id in pairs(known.plugins) do ids[#ids + 1] = id end
      table.sort(ids)
      if #ids == 0 then items[#items + 1] = {{"No plugins", "dim"}, inert = true} end
      for _, id in ipairs(ids) do
        local text, style = state[key](known.plugins[id] or nil)
        items[#items + 1] = {id, inert = true, right = {text, style}}
      end
    end
  end
  deck.ui.pick({
    title = "Agent plugins",
    subtitle = ":agent-plugins on|off NAME [claude|codex]",
    items = items,
    on_choose = function() end,
  })
end

deck.command("agent-plugins", function(args)
  local usage = "Use :agent-plugins [on|off NAME [claude|codex]]"
  if #args == 0 then return list(args, usage) end
  return toggle(args, usage)
end, {desc = "List or turn Claude and Codex plugins on or off", complete = {"on", "off"}})

deck.command("plugin", function(args) toggle(args, USAGE) end,
  {desc = "Turn a Claude or Codex plugin on or off: on|off NAME [claude|codex]", complete = {"on", "off"}})
deck.command("plugins", function(args) list(args, "Use :plugins") end,
  {desc = "List Claude and Codex plugins and whether each is on"})
