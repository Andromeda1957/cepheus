-- :agent-limit [N]: one native agent concurrency limit. Codex's config.toml
-- is its source; Claude's subagent and workflow limits are derived from it.
local deck = require("deck")
local files = require("agent_settings.files")
local json = require("agent_settings.json")
local toml = require("agent_settings.toml")

local KEY, LEGACY_KEY = "max_concurrent_threads_per_session", "max_threads"
local CLAUDE_KEYS = {"CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS", "CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS"}
-- Claude's Workflow runner accepts 1..256; the shared limit uses that range.
local MAX = 256

local function refuse(text)
  error({refused = text}, 0)
end

-- Codex's config, parsed; nil when there is none yet.
local function codex_config(path)
  local text, problem = files.read(path)
  if problem then refuse("Cannot read Codex config") end
  if text == nil then return nil end
  local doc = toml.parse(text)
  if not doc then refuse("Cannot read agent concurrency from Codex config") end
  return doc
end

local function agents_table(doc)
  local agents = doc and toml.get(doc.root, "agents")
  if agents ~= nil and not toml.is_table(agents) then refuse("Codex agents setting must be a table") end
  return agents
end

-- The saved limit, read on each use so a changed file is seen.
local function configured(doc)
  local agents = agents_table(doc)
  local value = agents and (toml.get(agents, KEY) or toml.get(agents, LEGACY_KEY))
  if value == nil then return nil end
  if value.vtype ~= "integer" or not value.int or value.int < 1 then
    refuse("Codex agent concurrency must be a positive integer")
  end
  return value.int
end

-- Claude's settings, parsed, and their text (nil when there are none yet).
local function claude_settings(path)
  local text, problem = files.read(path)
  if problem then refuse("Cannot read Claude settings") end
  local settings = text == nil and json.object() or json.decode(text)
  if not settings then refuse("Claude settings must contain valid JSON") end
  local env = json.get(settings, "env")
  if settings.t ~= "object" or (env and env.t ~= "object") then
    refuse("Claude settings and its env entry must be objects")
  end
  return settings, text
end

local function show(codex_path, claude_path)
  local value = configured(codex_config(codex_path))
  local env = json.get(claude_settings(claude_path), "env")
  local synced = value ~= nil
  for _, key in ipairs(CLAUDE_KEYS) do
    local item = env and json.get(env, key)
    if not item or json.text(item) ~= tostring(value) then synced = false end
  end
  return ("Agent limit: %s; source: %s; Claude: %s"):format(
    value and tostring(value) or "native default (unset)",
    codex_path,
    synced and "synced" or "not synced (set :agent-limit N)"
  )
end

local function save(limit, codex_path, claude_path)
  if limit < 1 or limit > MAX then refuse("Use a limit from 1 to 256 (Claude's native Workflow range)") end
  -- Check Claude's settings before Codex's change.
  claude_settings(claude_path)
  local doc = codex_config(codex_path)
  local agents = agents_table(doc)
  -- TOML refuses both spellings together: a file with only the older one
  -- keeps it rather than gaining a second.
  local key = KEY
  if agents and toml.get(agents, KEY) == nil and toml.get(agents, LEGACY_KEY) ~= nil then key = LEGACY_KEY end
  local before = doc and doc.text
  local text, problem = toml.set(doc or toml.parse(""), {"agents", key}, tostring(limit))
  if not text then refuse(problem) end
  -- The edited text must read back as the asked limit before it is saved.
  local edited = toml.parse(text)
  local fits, saved_limit = pcall(configured, edited)
  if not edited or not fits or saved_limit ~= limit then
    refuse("Codex config could not be changed safely; it was left as it was")
  end
  if text ~= before then
    local saved, reason = files.replace(codex_path, before, text)
    if not saved then refuse(reason) end
  end
  if configured(codex_config(codex_path)) ~= limit then refuse("Codex did not persist the requested agent limit") end
  local synced = pcall(function()
    local settings, original = claude_settings(claude_path)
    local env = json.member(settings, "env")
    for _, name in ipairs(CLAUDE_KEYS) do json.put(env, name, json.string(tostring(limit))) end
    local updated = json.encode(settings)
    if updated ~= original then assert(files.replace(claude_path, original, updated)) end
  end)
  if not synced then refuse("Codex saved the limit; Claude sync failed. Retry :agent-limit with the same value") end
  return ("Agent limit %d: saved in Codex and applied to Claude subagents and new workflows. "
    .. "Reopen existing Codex sessions to apply it."):format(limit)
end

deck.command("agent-limit", function(args)
  if #args > 1 or (#args == 1 and not args[1]:match("^%d+$")) then
    deck.notify("Use :agent-limit [1..256]", {error = true})
    return
  end
  local ok, result = pcall(function()
    local codex_path, codex_problem = files.resolved(files.join(files.codex_dir(), "config.toml"))
    if not codex_path then refuse(codex_problem) end
    local claude_path, claude_problem = files.resolved(files.join(files.claude_dir(), "settings.json"))
    if not claude_path then refuse(claude_problem) end
    if args[1] then return save(tonumber(args[1]), codex_path, claude_path) end
    return show(codex_path, claude_path)
  end)
  if ok then
    deck.notify(result)
  elseif type(result) == "table" and result.refused then
    deck.notify("Agent limit: " .. result.refused, {error = true})
  else
    error(result, 0)
  end
end, {desc = "Show or set the native Codex/Claude agent concurrency limit"})
