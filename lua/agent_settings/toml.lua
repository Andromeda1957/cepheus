-- Codex's config.toml, read as TOML 1.0 with the place of every table and
-- value remembered, so `set` can change one value and keep every other byte:
-- comments, order, spacing and quoting stay as they were.
local M = {}

local MAX_DEPTH = 64
local ESCAPE = {b = "\b", t = "\t", n = "\n", f = "\f", r = "\r", ['"'] = '"', ["\\"] = "\\"}

local Parser = {}
Parser.__index = Parser

local function new_table(kind)
  return {kind = kind, entries = {}}
end

function Parser:fail()
  local line = select(2, self.s:sub(1, self.i - 1):gsub("\n", "")) + 1
  error({toml = ("Codex config is not valid TOML (line %d)"):format(line)}, 0)
end

function Parser:ws()
  local _, e = self.s:find("^[ \t]*", self.i)
  self.i = e + 1
end

-- The rest of a line: spaces, a comment, then a line ending or the end.
function Parser:eol()
  self:ws()
  local s = self.s
  if s:sub(self.i, self.i) == "#" then
    self.i = s:find("[\r\n]", self.i) or (#s + 1)
  end
  if self.i > #s then return end
  if s:sub(self.i, self.i + 1) == "\r\n" then
    self.i = self.i + 2
  elseif s:sub(self.i, self.i) == "\n" then
    self.i = self.i + 1
  else
    self:fail()
  end
end

-- Spaces, line endings and comments, as an array may hold between values.
function Parser:blank()
  while true do
    self:ws()
    local c = self.s:sub(self.i, self.i)
    if c == "#" then
      self.i = self.s:find("[\r\n]", self.i) or (#self.s + 1)
    elseif c == "\n" then
      self.i = self.i + 1
    elseif self.s:sub(self.i, self.i + 1) == "\r\n" then
      self.i = self.i + 2
    else
      return
    end
  end
end

-- A \u or \U escape at `k` (the backslash): its character and where it ends.
function Parser:unicode(k)
  local s = self.s
  local size = s:sub(k + 1, k + 1) == "u" and 4 or 8
  local hex = s:sub(k + 2, k + 1 + size)
  if #hex ~= size or hex:find("[^%x]") then self.i = k; self:fail() end
  local code = tonumber(hex, 16)
  if code > 0x10FFFF or (code >= 0xD800 and code <= 0xDFFF) then self.i = k; self:fail() end
  return utf8.char(code), k + 2 + size
end

-- A one-line basic string at self.i: its text, leaving self.i after it.
function Parser:basic()
  local s, parts, j = self.s, {}, self.i + 1
  while true do
    local k = s:find('[\0-\8\10-\31\127"\\]', j)
    if not k or s:sub(k, k) ~= '"' and s:sub(k, k) ~= "\\" then self.i = k or j; self:fail() end
    parts[#parts + 1] = s:sub(j, k - 1)
    if s:sub(k, k) == '"' then
      self.i = k + 1
      return table.concat(parts)
    end
    local e = s:sub(k + 1, k + 1)
    if ESCAPE[e] then
      parts[#parts + 1], j = ESCAPE[e], k + 2
    elseif e == "u" or e == "U" then
      parts[#parts + 1], j = self:unicode(k)
    else
      self.i = k
      self:fail()
    end
  end
end

-- A one-line literal string at self.i.
function Parser:literal()
  local s = self.s
  local k = s:find("['\0-\8\10-\31\127]", self.i + 1)
  if not k or s:sub(k, k) ~= "'" then self.i = k or self.i; self:fail() end
  local text = s:sub(self.i + 1, k - 1)
  self.i = k + 1
  return text
end

-- A multi-line string whose delimiter (""" or ''') is at self.i.
function Parser:multiline(quote)
  local s, j = self.s, self.i + 3
  local stops = quote == '"' and '[\0-\8\11\12\14-\31\127"\\]' or "[\0-\8\11\12\14-\31\127']"
  while true do
    local k = s:find(stops, j)
    if not k then self.i = j; self:fail() end
    local c = s:sub(k, k)
    if c == quote then
      if s:sub(k, k + 2) == quote:rep(3) then
        local _, e = s:find("^" .. quote .. "+", k)
        if e - k + 1 > 5 then self.i = k; self:fail() end
        self.i = e + 1
        return
      end
      j = k + 1
    elseif c == "\\" then
      local e = s:sub(k + 1, k + 1)
      if ESCAPE[e] then
        j = k + 2
      elseif e == "u" or e == "U" then
        j = select(2, self:unicode(k))
      else
        local _, b = s:find("^[ \t]*\r?\n", k + 1)
        if not b then self.i = k; self:fail() end
        j = b + 1
      end
    else
      self.i = k
      self:fail()
    end
  end
end

-- Digits with single underscores only between them.
local function digits(text, class)
  return text ~= "" and not text:find("__", 1, true) and text:sub(1, 1) ~= "_" and text:sub(-1) ~= "_"
    and not (text:gsub("_", "")):find("[^" .. class .. "]")
end

local BASES = {x = {"%x", 16}, o = {"0-7", 8}, b = {"01", 2}}
local INTEGER_MAX = {x = "7fffffffffffffff", o = ("7"):rep(21), b = ("1"):rep(63)}

local function within(text, maximum)
  text = text:lower():gsub("^0+", "")
  return #text < #maximum or (#text == #maximum and text <= maximum)
end

local function datetime(token)
  local year, month, day, rest = token:match("^(%d%d%d%d)%-(%d%d)%-(%d%d)(.*)$")
  if year then
    year, month, day = tonumber(year), tonumber(month), tonumber(day)
    local leap = year % 4 == 0 and (year % 100 ~= 0 or year % 400 == 0)
    local days = {31, leap and 29 or 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31}
    if not days[month] or day < 1 or day > days[month] then return false end
    if rest == "" then return true end
    if not rest:find("^[Tt ]") then return false end
    rest = rest:sub(2)
  else
    rest = token
  end
  local hour, minute, second, zone = rest:match("^(%d%d):(%d%d):(%d%d)(.*)$")
  if not hour or tonumber(hour) > 23 or tonumber(minute) > 59 or tonumber(second) > 59 then return false end
  zone = zone:gsub("^%.%d+", "")
  if zone == "" then return true end
  if not year then return false end
  if zone == "Z" or zone == "z" then return true end
  local zh, zm = zone:match("^[+-](%d%d):(%d%d)$")
  return zh ~= nil and tonumber(zh) <= 23 and tonumber(zm) <= 59
end

-- What kind of scalar a bare token is, and its value for booleans and
-- integers; nil when it is none.
local function scalar(token)
  if token == "true" or token == "false" then return "boolean", token == "true" end
  local base, rest = token:match("^0([xob])(.*)$")
  if base then
    if not digits(rest, BASES[base][1]) then return nil end
    rest = rest:gsub("_", "")
    if not within(rest, INTEGER_MAX[base]) then return nil end
    return "integer", math.tointeger(tonumber(rest, BASES[base][2]))
  end
  if token:find("^%d%d%d%d%-%d%d%-%d%d") or token:find("^%d%d:%d%d:%d%d") then
    return datetime(token) and "datetime" or nil
  end
  local sign, body = token:match("^([+-]?)(.*)$")
  if body == "inf" or body == "nan" then return "float" end
  local whole, tail = body:match("^([%d_]+)(.*)$")
  if not whole or not digits(whole, "%d") or (whole:sub(1, 1) == "0" and #whole > 1) then return nil end
  if tail == "" then
    local magnitude = whole:gsub("_", "")
    if not within(magnitude, sign == "-" and "9223372036854775808" or "9223372036854775807") then return nil end
    return "integer", math.tointeger(tonumber(sign .. magnitude))
  end
  local fraction, exponent = tail:match("^%.([%d_]+)(.*)$")
  if fraction then
    if not digits(fraction, "%d") then return nil end
    tail = exponent
  end
  if tail == "" then return fraction and "float" or nil end
  local power = tail:match("^[eE][+-]?([%d_]+)$")
  return power and digits(power, "%d") and "float" or nil
end

-- A key: its parts, as a dotted key has several.
function Parser:key()
  local keys = {}
  while true do
    self:ws()
    local s, i = self.s, self.i
    local c = s:sub(i, i)
    if c == '"' then
      keys[#keys + 1] = self:basic()
    elseif c == "'" then
      keys[#keys + 1] = self:literal()
    else
      local _, e = s:find("^[A-Za-z0-9_-]+", i)
      if not e then self:fail() end
      keys[#keys + 1] = s:sub(i, e)
      self.i = e + 1
    end
    self:ws()
    if self.s:sub(self.i, self.i) ~= "." then return keys end
    self.i = self.i + 1
  end
end

function Parser:value(depth)
  if depth > MAX_DEPTH then self:fail() end
  local s, i = self.s, self.i
  local c = s:sub(i, i)
  local node = {kind = "value", s = i}
  if c == '"' or c == "'" then
    node.vtype = "string"
    if s:sub(i, i + 2) == c:rep(3) then
      self:multiline(c)
    elseif c == '"' then
      self:basic()
    else
      self:literal()
    end
  elseif c == "[" then
    node.vtype = "array"
    self.i = i + 1
    while true do
      self:blank()
      if s:sub(self.i, self.i) == "]" then break end
      self:value(depth + 1)
      self:blank()
      local d = s:sub(self.i, self.i)
      if d == "," then
        self.i = self.i + 1
      elseif d ~= "]" then
        self:fail()
      end
    end
    self.i = self.i + 1
  elseif c == "{" then
    node.vtype = "inline"
    local inline = new_table("inline")
    inline.open, node.table = i, inline
    self.i = i + 1
    self:ws()
    if s:sub(self.i, self.i) ~= "}" then
      while true do
        local item = self:keyval(inline, depth + 1)
        inline.last_end = item.e
        self:ws()
        local d = s:sub(self.i, self.i)
        if d == "}" then break end
        if d ~= "," then self:fail() end
        self.i = self.i + 1
      end
    end
    inline.close = self.i
    self.i = self.i + 1
  else
    local _, e = s:find("^[%w+%-.:_]+", i)
    if not e then self:fail() end
    if s:find("^%d%d%d%d%-%d%d%-%d%d ", i) and e == i + 9 then
      local _, t = s:find("^ %d%d:%d%d:%d%d[%w+%-.:]*", e + 1)
      e = t or e
    end
    local kind, value = scalar(s:sub(i, e))
    if not kind then self:fail() end
    node.vtype = kind
    if kind == "integer" then node.int = value elseif kind == "boolean" then node.bool = value end
    self.i = e + 1
  end
  node.e = self.i
  return node
end

-- `key = value` into `owner` (a section or an inline table); the value, and
-- the dotted-key tables it passed through.
function Parser:keyval(owner, depth)
  local keys = self:key()
  if self.s:sub(self.i, self.i) ~= "=" then self:fail() end
  self.i = self.i + 1
  self:ws()
  local value = self:value(depth)
  local t, passed = owner, {}
  for n = 1, #keys - 1 do
    local child = t.entries[keys[n]]
    if child == nil then
      child = new_table("dotted")
      child.owner, child.rel = owner, {table.unpack(keys, 1, n)}
      t.entries[keys[n]] = child
    elseif child.kind ~= "dotted" then
      self:fail()
    end
    passed[#passed + 1] = child
    t = child
  end
  if t.entries[keys[#keys]] ~= nil then self:fail() end
  t.entries[keys[#keys]] = value
  return value, passed
end

-- The table a [header] or [[header]] line opens.
function Parser:header(root, keys, array)
  local t = root
  for n = 1, #keys - 1 do
    local child = t.entries[keys[n]]
    if child == nil then
      child = new_table("implicit")
      child.path = {table.unpack(keys, 1, n)}
      t.entries[keys[n]] = child
    elseif child.kind == "aot" then
      child = child.items[#child.items]
    elseif child.kind ~= "implicit" and child.kind ~= "header" and child.kind ~= "dotted" then
      self:fail()
    end
    t = child
  end
  local last = keys[#keys]
  local child = t.entries[last]
  if array then
    if child == nil then
      child = {kind = "aot", items = {}}
      t.entries[last] = child
    elseif child.kind ~= "aot" then
      self:fail()
    end
    local item = new_table("header")
    item.in_array = true
    child.items[#child.items + 1] = item
    return item
  end
  if child == nil then
    child = new_table("header")
    t.entries[last] = child
  elseif child.kind == "implicit" then
    child.kind = "header"
  else
    self:fail()
  end
  child.path = keys
  return child
end

-- The document, or nil and a message naming the line that is wrong.
function M.parse(text)
  local p = setmetatable({s = text, i = 1}, Parser)
  local root = new_table("root")
  root.path = {}
  local ok, problem = pcall(function()
    local current = root
    while true do
      p:ws()
      if p.i > #text then return end
      local c = text:sub(p.i, p.i)
      if c == "#" or c == "\n" or c == "\r" then
        p:eol()
      elseif c == "[" then
        local array = text:sub(p.i, p.i + 1) == "[["
        p.i = p.i + (array and 2 or 1)
        local keys = p:key()
        local close = array and "]]" or "]"
        if text:sub(p.i, p.i + #close - 1) ~= close then p:fail() end
        p.i = p.i + #close
        p:eol()
        current = p:header(root, keys, array)
        current.body = p.i
      else
        local _, passed = p:keyval(current, 1)
        p:eol()
        current.tail = p.i
        for _, t in ipairs(passed) do t.tail = p.i end
      end
    end
  end)
  if not ok then
    if type(problem) == "table" and problem.toml then return nil, problem.toml end
    error(problem, 0)
  end
  return {text = text, root = root}
end

-- Whether a node holds keys: a table, or an inline table value.
function M.is_table(node)
  return node ~= nil and (node.entries ~= nil or node.vtype == "inline")
end

local function entries(node)
  if node.vtype == "inline" then return node.table end
  return node
end

function M.get(node, key)
  if M.is_table(node) then return entries(node).entries[key] end
end

-- A table's keys, sorted.
function M.keys(node)
  local keys = {}
  for key in pairs(entries(node).entries) do keys[#keys + 1] = key end
  table.sort(keys)
  return keys
end

local function quote(key)
  if key:find("^[A-Za-z0-9_-]+$") then return key end
  return '"' .. (key:gsub('[\0-\31\127"\\]', function(c)
    if c == '"' or c == "\\" then return "\\" .. c end
    return ("\\u%04X"):format(c:byte())
  end)) .. '"'
end

local function dotted(keys)
  local parts = {}
  for n, key in ipairs(keys) do parts[n] = quote(key) end
  return table.concat(parts, ".")
end

local function concat(a, b)
  local out = {table.unpack(a)}
  for _, item in ipairs(b) do out[#out + 1] = item end
  return out
end

-- `line` inserted as a line of its own at `at`.
local function insert_line(text, at, line, newline)
  local before = text:sub(1, at - 1)
  local gap = (before ~= "" and before:sub(-1) ~= "\n") and newline or ""
  return before .. gap .. line .. newline .. text:sub(at)
end

-- Set `path` (its keys) to `literal`, TOML text for the new value. An
-- existing value is replaced where it stands; a new key joins its table, and
-- a new table is added at the end. `vtype` names the type an existing value
-- must already have. Returns the new text, or nil and why it was refused.
function M.set(doc, path, literal, vtype)
  local text, t, n = doc.text, doc.root, 1
  local where = dotted(path)
  while n < #path do
    local child = M.get(t, path[n])
    if child == nil then break end
    if not M.is_table(child) then
      return nil, ("Codex config has %s as a value or array of tables, not a table"):format(dotted({table.unpack(path, 1, n)}))
    end
    t, n = entries(child), n + 1
  end
  if n == #path then
    local leaf = t.entries[path[n]]
    if leaf ~= nil then
      if leaf.kind ~= "value" or leaf.vtype == "inline" or (vtype and leaf.vtype ~= vtype) then
        return nil, ("Codex config's %s is not a %s"):format(where, vtype or "value")
      end
      return text:sub(1, leaf.s - 1) .. literal .. text:sub(leaf.e)
    end
  end
  local rest = {table.unpack(path, n)}
  local newline = text:find("\r\n", 1, true) and "\r\n" or "\n"
  if t.kind == "inline" or (t.kind == "dotted" and t.owner.kind == "inline") then
    local inline = t.kind == "inline" and t or t.owner
    local item = quote(rest[#rest]) .. " = " .. literal
    for k = #rest - 1, 1, -1 do item = quote(rest[k]) .. " = { " .. item .. " }" end
    if t.kind == "dotted" then item = dotted(t.rel) .. "." .. item end
    if inline.last_end then
      return text:sub(1, inline.last_end - 1) .. ", " .. item .. text:sub(inline.last_end)
    end
    return text:sub(1, inline.open) .. " " .. item .. " " .. text:sub(inline.close)
  end
  if t.kind == "dotted" then
    return insert_line(text, t.tail, dotted(concat(t.rel, rest)) .. " = " .. literal, newline)
  end
  if #rest == 1 and (t.kind == "header" or t.kind == "root") then
    return insert_line(text, t.tail or t.body or 1, quote(rest[1]) .. " = " .. literal, newline)
  end
  if t.in_array then
    return nil, ("Codex config's %s is in an array of tables"):format(where)
  end
  -- A new table, after everything else.
  local out = text
  if out ~= "" and out:sub(-1) ~= "\n" then out = out .. newline end
  if out ~= "" then out = out .. newline end
  local table_path = concat(t.path, {table.unpack(rest, 1, #rest - 1)})
  return out .. "[" .. dotted(table_path) .. "]" .. newline .. quote(rest[#rest]) .. " = " .. literal .. newline
end

return M
