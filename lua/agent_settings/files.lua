-- Where Claude's and Codex's own settings live, and replacing one of those
-- files whole: a private staging file beside it, written and flushed, given
-- the original's permissions, then renamed over it.
local deck = require("deck")

local M = {}

-- Settings files are small; a larger one is refused rather than edited.
local MAX_BYTES = 4 << 20

local function ran(argv)
  local ok, result = pcall(deck.run, argv)
  return ok and result.ok, ok and result.stdout or ""
end

local function env(name)
  local value = os.getenv(name)
  if value ~= nil and value ~= "" then return value end
end

function M.join(dir, name)
  if dir:sub(-1) == "/" then return dir .. name end
  return dir .. "/" .. name
end

local function home()
  return os.getenv("HOME") or "/"
end

-- The folders the agents themselves read, chosen as they choose them.
function M.claude_dir()
  return env("CLAUDE_CONFIG_DIR") or M.join(home(), ".claude")
end

function M.codex_dir()
  return env("CODEX_HOME") or M.join(home(), ".codex")
end

-- The path with every existing part's symbolic links resolved, so a linked
-- settings file is changed where it really is.
function M.resolved(path)
  local real = deck.fs.realpath(path)
  if real then return real end
  -- canonicalize/stat follow links, so both miss a dangling or looping link.
  -- Refuse it rather than renaming a new settings file over the link itself.
  local ok, result = pcall(deck.run, {"test", "-L", path})
  if not ok or result.timed_out or (result.code ~= 0 and result.code ~= 1) then
    return nil, "Cannot inspect settings path " .. path
  end
  if result.ok then return nil, "Cannot resolve settings symbolic link " .. path end
  local parent, name = path:match("^(.*)/([^/]+)$")
  if parent and parent ~= "" then
    local resolved, problem = M.resolved(parent)
    if not resolved then return nil, problem end
    return M.join(resolved, name)
  end
  return path
end

-- The file's text; nil when it does not exist; nil and a reason when it
-- exists but cannot be read.
function M.read(path)
  local text = deck.fs.read(path, MAX_BYTES)
  if text then return text end
  if deck.fs.stat(path) == nil and not ran({"test", "-L", path}) then return nil end
  return nil, "cannot be read"
end

-- Replace `path`, which held `before` (nil: it did not exist), with `text`.
-- Its permissions stay; a new file is private. Nothing changes when the file
-- changed since it was read. Returns true, or nil and a reason.
function M.replace(path, before, text)
  local dir, name = path:match("^(.*)/([^/]+)$")
  if not dir then return nil, "cannot save " .. path end
  if dir == "" then dir = "/" end
  if not deck.fs.stat(dir) and not ran({"mkdir", "-p", "--", dir}) then
    return nil, "cannot create " .. dir
  end
  -- mktemp makes the staging file readable by its owner only.
  local made, out = ran({"mktemp", M.join(dir, "." .. name .. ".cepheus-XXXXXXXX")})
  local staging = made and out:gsub("%s+$", "")
  if not staging or staging == "" then return nil, "cannot write in " .. dir end
  local function abandon(reason)
    os.remove(staging)
    return nil, reason
  end
  local file = io.open(staging, "wb")
  if not file then return abandon("cannot write in " .. dir) end
  local written = file:write(text)
  if not file:close() or not written then return abandon("cannot write " .. path) end
  if before ~= nil and not ran({"chmod", "--reference=" .. path, "--", staging}) then
    return abandon("cannot keep the permissions of " .. path)
  end
  if not ran({"sync", "--", staging}) then return abandon("cannot write " .. path) end
  local now = deck.fs.read(path, MAX_BYTES)
  local inspected, link = pcall(deck.run, {"test", "-L", path})
  if not inspected or link.timed_out or link.code ~= 1
      or now ~= before or (before == nil and deck.fs.stat(path) ~= nil) then
    return abandon(path .. " changed while it was being saved; try again")
  end
  if not os.rename(staging, path) then return abandon("cannot replace " .. path) end
  return true
end

return M
