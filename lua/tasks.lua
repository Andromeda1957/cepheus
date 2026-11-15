-- Task-source dashboard. Core publishes bounded task metadata from configured
-- sources; `:tasks` asks for the full list, including retained merged rows.
local deck = require("deck")
if deck.opt.tasks == false then return false end

local tasks, by_location, hidden = {}, {}, 0
local problem, read_once = nil, false
local function location(host, path, task)
  return (host or "") .. "\0" .. path .. "\0" .. task
end
local function sources_of(list)
  local sources = {}
  for _, source in ipairs(list or {}) do
    sources[source.id] = source
  end
  return sources
end
local function rows(list, sources)
  local out = {}
  for _, item in ipairs(list or {}) do
    local source = sources[item.source_id]
    if source then
      out[#out + 1] = {
        task = item.id, title = item.title or item.id, state = item.state, worktree = item.worktree or "",
        branch = item.branch or "", next = item.next_step or "",
        source_id = item.source_id, host = source.host, repository = source.repository, target = item.target,
      }
    end
  end
  return out
end
local function refresh()
  local state = deck.state({"work_queue", "work_queue_capable"}) or {}
  if state.work_queue_capable ~= true then
    problem = "Task sources need protocol 14; update the service."
    return
  end
  local snapshot = state.work_queue or {}
  local sources = sources_of(snapshot.sources)
  problem, read_once, hidden = nil, false, 0
  for _, source in pairs(sources) do
    read_once = read_once or source.updated_at ~= nil
    hidden = hidden + (source.omitted or 0)
    if not source.fresh and (source.updated_at ~= nil or source.error ~= nil) then
      problem = "task source stale; showing cached tasks"
    end
  end
  local next_locations = {}
  tasks = rows(snapshot.tasks, sources)
  for _, task in ipairs(tasks) do
    if task.worktree ~= "" then
      next_locations[location(task.host, task.worktree, task.task)] = task
    end
  end
  by_location = next_locations
end
deck.on("state", refresh)
refresh()

local function literal(value)
  if type(value) ~= "string" or value == "" or value:find("[%z\1-\31\127]") then return nil end
  return "'" .. value:gsub("'", "'\\''") .. "'"
end

deck.ui.section("tasks", {
  title = "TASKS", sidebar = false,
  lines = function()
    if problem then return { { { " " .. problem, "warn" } } } end
    if #tasks == 0 and hidden == 0 then
      return { { { read_once and " no tasks" or " reading…", "dim" } } }
    end
    -- Rows the state leaves out are all merged.
    local count = { active = 0, ready = 0, merged = hidden, stopped = 0 }
    for _, t in ipairs(tasks) do count[t.state] = count[t.state] + 1 end
    local lines = { {
      { " " .. count.active .. " active", "working" },
      { "  " .. count.ready .. " ready", "ok" },
      { "  " .. count.merged .. " merged", "dim" },
    } }
    local shown = 0
    for _, t in ipairs(tasks) do
      if t.state == "ready" and shown < 6 then
        lines[#lines + 1] = { { "  ✓ ", "ok" }, { t.title, "task" } }
        shown = shown + 1
      end
    end
    if count.ready > shown then
      lines[#lines + 1] = { { "  … " .. (count.ready - shown) .. " more · :tasks", "dim" } }
    end
    return lines
  end,
})

deck.ui.badge(function(s)
  if not s.worktree or not s.worktree.path or not s.worktree.task then return nil end
  local t = by_location[location(s.host, s.worktree.path, s.worktree.task)]
  if not t then return nil end
  if t.state == "ready" and (s.status == "idle" or s.status == "finished") then
    return { " ready", "ok" }
  elseif t.state == "merged" then
    return { " merged", "dim" }
  end
end)

local function choose(t, queue)
  if queue then
    local source, item = literal(t.source_id), literal(t.task)
    if not source or not item then deck.notify("Invalid task identity.", {error=true}); return end
    -- Native queue-source opens an explicit-spec command prefill. No
    -- title, next step or imported content becomes a command or prompt.
    deck.cmd("queue source " .. source .. " " .. item)
    return
  end
  local sessions = (deck.state({"sessions"}) or {}).sessions or {}
  local target = t.target
  if target and type(target.tool) == "string" and target.tool ~= "shell"
    and type(target.conversation) == "string" then
    for _, s in ipairs(sessions) do
      if s.alive and s.tool == target.tool and s.conversation == target.conversation and s.host == t.host then
        deck.select(s.id)
        return
      end
    end
    deck.resume(target.tool, target.conversation, { host = t.host })
    return
  end
  for _, s in ipairs(sessions) do
    if s.alive and s.tool and s.tool ~= "shell" and s.worktree and s.worktree.task == t.task
      and s.worktree.path == t.worktree and s.host == t.host then
      deck.select(s.id)
      return
    end
  end
  deck.notify("No agent conversation found for " .. t.title .. ".")
end

-- The window's pages: open work first, then each state, then everything.
local pages = {
  { label = "Open", keep = function(state) return state ~= "merged" end },
  { label = "Ready", state = "ready" },
  { label = "Active", state = "active" },
  { label = "Stopped", state = "stopped" },
  { label = "Merged", state = "merged" },
  { label = "All", keep = function() return true end },
}
local rank = { ready = 1, active = 2, stopped = 3, merged = 4 }
local marks = { ready = "✓", active = "●", stopped = "×", merged = "✓" }
local styles = { ready = "ok", active = "working", stopped = "warn", merged = "dim" }
local names = { ready = "Ready", active = "Active", stopped = "Stopped", merged = "Merged" }

local function keeps(page, state)
  if page.state then return page.state == state end
  return page.keep(state)
end

-- `wanted` names the page to show, as its tab reads; the first by default.
local function pick(all, wanted, queue)
  wanted = wanted and wanted:lower()
  local count = { active = 0, ready = 0, merged = 0, stopped = 0 }
  local hosts = {}
  for _, t in ipairs(all) do
    count[t.state] = count[t.state] + 1
    hosts[t.host or ""] = true
  end
  local many_hosts = next(hosts, next(hosts)) ~= nil
  local tabs, shown_pages, page_index = {}, {}, 1
  for _, page in ipairs(pages) do
    -- Stopped work is rare; its page appears only when there is some.
    if page.state ~= "stopped" or count.stopped > 0 or wanted == "stopped" then
      local n = 0
      for state, c in pairs(count) do
        if keeps(page, state) then n = n + c end
      end
      shown_pages[#shown_pages + 1] = page
      tabs[#tabs + 1] = { page.label, n }
      if page.label:lower() == wanted then page_index = #shown_pages end
    end
  end
  local page = shown_pages[page_index]
  local rows = {}
  for _, t in ipairs(all) do
    if keeps(page, t.state) then rows[#rows + 1] = t end
  end
  table.sort(rows, function(a, b)
    if a.state ~= b.state then return rank[a.state] < rank[b.state] end
    if a.task ~= b.task then return a.task < b.task end
    return a.source_id < b.source_id
  end)
  local columns = { "", "Task" }
  if many_hosts then columns[#columns + 1] = "Host" end
  columns[#columns + 1] = "Next step"
  local items, chosen = {}, {}
  local grouped = page.state == nil
  local last
  for _, t in ipairs(rows) do
    if grouped and t.state ~= last then
      items[#items + 1] = { heading = true, { names[t.state], styles[t.state] }, right = tostring(count[t.state]) }
      chosen[#items] = false
      last = t.state
    end
    local cells = { { marks[t.state], styles[t.state] }, { t.title, "task" } }
    if many_hosts then cells[#cells + 1] = { t.host or "here", "dim" } end
    cells[#cells + 1] = { t.next, "dim" }
    items[#items + 1] = { cells = cells }
    chosen[#items] = t
  end
  if #rows == 0 then
    local why = problem or (read_once and ("No " .. page.label:lower() .. " tasks.") or "Reading tasks…")
    items[1] = { inert = true, { why, "dim" } }
  end
  local total = count.active + count.ready + count.merged + count.stopped
  deck.ui.pick({
    title = "Tasks",
    subtitle = problem or (total == 1 and "1 task" or (total .. " tasks")),
    columns = columns,
    tabs = tabs,
    tab = page_index,
    items = items,
    on_tab = function(label) pick(all, label, queue) end,
    on_choose = function(_, i) if chosen[i] then choose(chosen[i], queue) end end,
  })
end

deck.command("tasks", function(args)
  local queue = args[1] == "queue"
  local filter = queue and args[2] or args[1]
  deck.task_sources.list({}, function(result)
    if not result.ok then
      deck.notify("Tasks: " .. tostring(result.error or "the service did not answer"), {error=true})
      return
    end
    local data = result.data or {}
    pick(rows(data.tasks, sources_of(data.sources)), filter, queue)
  end)
end, {
  desc = "task window; Enter goes to the agent; queue opens an explicit job spec",
  complete = { "open", "ready", "active", "merged", "stopped", "all", "queue" },
})
