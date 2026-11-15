-- Optional read-only task providers. Each authoritative configuration load
-- first disables the previous owned descriptors; only acknowledged, current
-- explicit registrations may start provider commands.
local deck = require("deck")
if deck.context ~= "service" or not deck.task_sources then return false end
local status = { configured = 0, registered = 0, unavailable = 0 }
deck.service.command("task-sources", "status", function()
  return status
end, { desc = "Configured read-only source provider availability" })

local function reject(message)
  status.unavailable = status.unavailable + 1
  deck.notify(message, {error=true})
  return {}
end
local function configuration()
  local f = io.open(deck.config_dir .. "/task-sources.json", "r")
  if not f then return {} end
  local text = f:read(65537) or ""
  f:close()
  if #text > 65536 then return reject("Task source configuration is too large.") end
  local ok, config = pcall(deck.json.decode, text)
  if not ok or type(config) ~= "table" then return reject("Invalid task source configuration.") end
  if config.enabled ~= true then return {} end
  if config.sources ~= nil and type(config.sources) ~= "table" then
    return reject("Invalid task source configuration.")
  end
  local candidates, seen = {}, {}
  local repositories = ((deck.state({"scheduler"}) or {}).scheduler or {}).repositories or {}
  for _, source in ipairs(config.sources or {}) do
    if type(source) ~= "table" then return reject("Invalid task source configuration.") end
    if source.enabled == true then
      status.configured = status.configured + 1
      local root = repositories[source.repository]
      local provider = source.provider
      if type(source.id) ~= "string" or seen[source.id] or type(root) ~= "string"
        or (provider ~= "markdown" and provider ~= "github") or #candidates >= 100 then
        return reject("Task source configuration references an unavailable provider or repository.")
      end
      seen[source.id] = true
      local argv = {
        deck.exe, "task-source", "--provider", provider, "--source-id", source.id,
        "--repository-root", root,
      }
      if provider == "markdown" and (source.file == nil or type(source.file) == "string") then
        argv[#argv + 1], argv[#argv + 2] = "--file", source.file or "TASKS.md"
      elseif provider == "github" and type(source.github_repo) == "string" then
        argv[#argv + 1], argv[#argv + 2] = "--github-repo", source.github_repo
      else return reject("Invalid task source configuration.") end
      candidates[#candidates + 1] = {
        id = source.id, owner = "task-sources", repository = source.repository,
        mode = "command", cwd = root, argv = argv,
        interval_seconds = math.max(20, tonumber(source.interval_seconds) or 20),
        enabled = true,
      }
    end
  end
  return candidates
end

local configured, candidates = pcall(configuration)
if not configured then candidates = reject("Invalid task source configuration.") end
-- An empty desired set is an execution barrier, once per load (not per poll).
-- It also handles missing, disabled and invalid configurations. Registration
-- failures cannot leave an older same-ID command authorized to keep running.
local sent = pcall(deck.task_sources.reconcile, {
  owner = "task-sources", ids = deck.json.decode("[]"),
}, function(result)
  if not result.ok then status.unavailable = status.unavailable + 1; return end
  for _, spec in ipairs(candidates) do
    local registered = pcall(deck.task_sources.register, spec, function(reply)
      if reply.ok then status.registered = status.registered + 1
      else status.unavailable = status.unavailable + 1 end
    end)
    if not registered then status.unavailable = status.unavailable + 1 end
  end
end)
if not sent then status.unavailable = status.unavailable + 1 end
