-- Claude's settings.json as ordered JSON. Each object keeps its key order
-- (a repeated key keeps its first place and its last value, as Python's
-- json module reads it) and each scalar its original text; `encode` writes
-- the layout Python's json.dump(indent=2, ensure_ascii=False) and Claude use.
local M = {}

local MAX_DEPTH = 64
local ESCAPE = {['"'] = '"', ["\\"] = "\\", ["/"] = "/", b = "\b", f = "\f", n = "\n", r = "\r", t = "\t"}
local QUOTE = {['"'] = '\\"', ["\\"] = "\\\\", ["\b"] = "\\b", ["\f"] = "\\f", ["\n"] = "\\n", ["\r"] = "\\r", ["\t"] = "\\t"}

local function fail(at)
  error({json = at}, 0)
end

local function skip(s, i)
  local _, e = s:find("^[ \t\r\n]*", i)
  return e + 1
end

local function quote(text)
  return '"' .. (text:gsub('[\0-\31"\\]', function(c)
    return QUOTE[c] or ("\\u%04x"):format(c:byte())
  end)) .. '"'
end

-- The string starting at `i`: its node and the index after it.
local function string_at(s, i)
  local parts, j = {}, i + 1
  while true do
    local k = s:find('[\0-\31"\\]', j)
    if not k then fail(j) end
    local c = s:sub(k, k)
    parts[#parts + 1] = s:sub(j, k - 1)
    if c == '"' then
      return {t = "string", raw = s:sub(i, k), value = table.concat(parts)}, k + 1
    elseif c ~= "\\" then
      fail(k)
    end
    local e = s:sub(k + 1, k + 1)
    if ESCAPE[e] then
      parts[#parts + 1], j = ESCAPE[e], k + 2
    elseif e == "u" then
      local hex = s:match("^%x%x%x%x", k + 2)
      if not hex then fail(k) end
      local code = tonumber(hex, 16)
      j = k + 6
      local low = code >= 0xD800 and code <= 0xDBFF and s:match("^\\u(%x%x%x%x)", j)
      local second = low and tonumber(low, 16)
      if second and second >= 0xDC00 and second <= 0xDFFF then
        code, j = 0x10000 + (code - 0xD800) * 0x400 + (second - 0xDC00), j + 6
      end
      parts[#parts + 1] = utf8.char(code)
    else
      fail(k)
    end
  end
end

local function number_at(s, i)
  local _, e = s:find("^-?%d+", i)
  if not e then fail(i) end
  if s:find("^-?0%d", i) then fail(i) end
  local _, f = s:find("^%.%d+", e + 1)
  e = f or e
  local _, x = s:find("^[eE][+-]?%d+", e + 1)
  e = x or e
  return {t = "number", raw = s:sub(i, e)}, e + 1
end

local LITERALS = {t = "true", f = "false", n = "null"}

local function value_at(s, i, depth)
  if depth > MAX_DEPTH then fail(i) end
  i = skip(s, i)
  local c = s:sub(i, i)
  if c == "{" then
    local node = M.object()
    i = skip(s, i + 1)
    if s:sub(i, i) == "}" then return node, i + 1 end
    while true do
      if s:sub(i, i) ~= '"' then fail(i) end
      local key, item
      key, i = string_at(s, i)
      i = skip(s, i)
      if s:sub(i, i) ~= ":" then fail(i) end
      item, i = value_at(s, i + 1, depth + 1)
      if node.map[key.value] == nil then
        node.order[#node.order + 1] = key.value
        node.raw[key.value] = key.raw
      end
      node.map[key.value] = item
      i = skip(s, i)
      local d = s:sub(i, i)
      if d == "}" then return node, i + 1 end
      if d ~= "," then fail(i) end
      i = skip(s, i + 1)
    end
  elseif c == "[" then
    local node = {t = "array", items = {}}
    i = skip(s, i + 1)
    if s:sub(i, i) == "]" then return node, i + 1 end
    while true do
      local item
      item, i = value_at(s, i, depth + 1)
      node.items[#node.items + 1] = item
      i = skip(s, i)
      local d = s:sub(i, i)
      if d == "]" then return node, i + 1 end
      if d ~= "," then fail(i) end
      i = i + 1
    end
  elseif c == '"' then
    return string_at(s, i)
  elseif LITERALS[c] then
    local word = LITERALS[c]
    if s:sub(i, i + #word - 1) ~= word then fail(i) end
    return {t = word, raw = word}, i + #word
  elseif c == "-" or c:find("^%d") then
    return number_at(s, i)
  end
  fail(i)
end

-- The document, or nil when the text is not exactly one JSON value.
function M.decode(text)
  local ok, node, i = pcall(value_at, text, 1, 1)
  if not ok then
    if type(node) == "table" and node.json then return nil end
    error(node, 0)
  end
  if skip(text, i) <= #text then return nil end
  return node
end

local function write(node, depth, out)
  local inner = ("  "):rep(depth + 1)
  if node.t == "object" then
    if #node.order == 0 then
      out[#out + 1] = "{}"
      return
    end
    out[#out + 1] = "{"
    for n, key in ipairs(node.order) do
      out[#out + 1] = (n == 1 and "\n" or ",\n") .. inner .. node.raw[key] .. ": "
      write(node.map[key], depth + 1, out)
    end
    out[#out + 1] = "\n" .. ("  "):rep(depth) .. "}"
  elseif node.t == "array" then
    if #node.items == 0 then
      out[#out + 1] = "[]"
      return
    end
    out[#out + 1] = "["
    for n, item in ipairs(node.items) do
      out[#out + 1] = (n == 1 and "\n" or ",\n") .. inner
      write(item, depth + 1, out)
    end
    out[#out + 1] = "\n" .. ("  "):rep(depth) .. "]"
  else
    out[#out + 1] = node.raw
  end
end

function M.encode(node)
  local out = {}
  write(node, 0, out)
  out[#out + 1] = "\n"
  return table.concat(out)
end

function M.object()
  return {t = "object", order = {}, raw = {}, map = {}}
end

function M.string(text)
  return {t = "string", raw = quote(text), value = text}
end

function M.boolean(on)
  local word = on and "true" or "false"
  return {t = word, raw = word}
end

function M.get(node, key)
  if node.t == "object" then return node.map[key] end
end

-- Set `key` of an object, appending it when new.
function M.put(node, key, item)
  if node.map[key] == nil then
    node.order[#node.order + 1] = key
    node.raw[key] = quote(key)
  end
  node.map[key] = item
end

-- The object at `key`, added at the end when missing.
function M.member(node, key)
  local item = node.map[key]
  if item == nil then
    item = M.object()
    M.put(node, key, item)
  end
  return item
end

-- A scalar as Python's str() would show it, for comparing a setting.
function M.text(node)
  if node.t == "string" then return node.value end
  if node.t == "number" then return node.raw end
  return ""
end

return M
