# Cepheus Lua API

Everything is on the global `deck` table, also `require("deck")`.

## Keys and commands

- `deck.keymap(key, action, {desc})`: `key` is a chord such as `"alt+c"`,
  `"ctrl+x"`, `"f5"` or `"g"`; `action` is a command line (`"new claude"`, no
  colon) or a function. A script's keymap wins over the deck's hotkey on the
  same chord; a plain character key is skipped while typing into a session
  or on the `:` line.
- `deck.command(name, function(args), {desc, complete})`: adds `:name`.
  `args` is the list of words after the name (quotes keep spaces);
  `complete` is a list of words Tab offers.
- `deck.cmd(line)`: run a `:` command line, built-in or a script's, as if typed.

## What the deck knows

- `deck.state()`: a fresh copy of the deck's state, read only:
  `sessions` (each `id`, `tool`, `title`, `status`, `alive`, `host`, `cwd`,
  `worktree` = `{path, branch, task, exists}`, `subagents` = `[{name, kind,
  active, started_at, last_active}]`, `activity`, `last_active`,
  `created_at`, `status_since`, `review`, `containers`, `pid`, `conversation`,
  `spawned_ms`), `hosts` (`name`,
  `label`, `online`, `reason`), `branches`, `scans`, `dev_server`, `usage`,
  `start_dir`, `errors`, `current` (the highlighted session's id or nil) and
  `mode` (`normal`, `insert` or `command`). Display/event times are Unix
  seconds. Protocol 17 exposes public `SessionInfo.spawned_ms` as an exact
  hosted generation, with zero meaning unknown; retain it unchanged with PID
  and nullable conversation when capturing an idle-send target. A field
  with no value is nil (a terminal has no `worktree`), here, in event
  payloads, badges, `:set` and `deck.json.decode`.
- `deck.state().order`: live session IDs in current visual order, including
  manual movement and the shared order file. Available in UI scripts.
- `deck.state({"sessions", "order"})`: the same independent copy restricted to
  named top-level fields. State callbacks should request only the fields they
  use; `host_order` contains configured peer names without their branch data.
- `deck.start_dir`, `deck.config_dir`, `deck.plugin_dir`, `deck.plugin_source_dir`, `deck.version`, `deck.exe`.
  `exe` is the absolute path to the running Cepheus binary, for native helper
  commands such as `{deck.exe, "dot", "setup"}`.
  `config_dir` owns personal settings; `plugin_dir` owns code and helpers.
  `plugin_source_dir` is present only in repository mode, including when the two
  roots happen to be equal. The installation selects repository mode through `plugin_source_dir` in its local
  config/profile. Older builds omit `plugin_dir`; helpers can fall back to
  `config_dir` for compatibility.
- `deck.opt`: options. `deck.opt.sidebar` (part order; it wins over the
  config's `sidebar.parts`), `deck.opt.sidebar_hidden` (part IDs to hide
  without replacing the order, empty by default), `deck.opt.badges =
  false` (no badges). Any other key is the
  script's own; `:set name=value` changes one from the deck (the value is
  read as JSON, else text). The deck reads only values set in the table
  itself: a metatable's `__index` supplies no option.

## Events

`deck.on(event, function(payload))`:

| Event | Payload |
| --- | --- |
| `ready` | nil: every script has loaded |
| `state` | nil: a new state or new hosts arrived (`deck.state()` has them) |
| `session_new` | the session |
| `session_end` | the session |
| `status` | `{session, from, to}` |
| `finished` | the session |
| `needs_you` | the session |
| `cleared` | the session: `/clear` or `/new` left it an empty conversation (its `last_active` is 0) |

## Doing things

- `deck.new(tool, {host, cwd, name})`: start `claude`, `codex`, `agy`,
  `grok` or `opencode`; `host` is a host's name (nil is here).
- `deck.resume(tool, conversation, {host})`: select or reopen an agent's saved
  conversation on its host, using the same session recovery as Resume. Reuses
  an existing session for that conversation; never creates a shell.
- `deck.term({host, cwd, name, cmd})`: a terminal in the owner's shell, or
  running `cmd` (a list of words).
- `deck.send(id, text)`: send owner text through the live provider's native
  message interface (protocol 19 on coordinator and source host). Pass the exact
  body without an appended Enter or terminal escape. Success is silent;
  correlated failures appear as errors. This never types into the terminal.
- `deck.select(id)`: highlight a session.
- `deck.reorder(ids)`: reorder sessions without changing the selected session.
  Supply at most 4096 unique current session IDs; omitted sessions retain their
  relative order after those supplied. Existing host sections stay together.
  The saved order is shared with the phone. Available in UI scripts.
- `deck.notify(text, {error})`: the notice line; `error = true` also logs it
  in F2. `print(...)` is `deck.notify` of its words.

These happen after the call into Lua returns, in order.

## Programs and time

- `deck.run(argv, {cwd, timeout})`: run a program and wait, at most 2 seconds;
  returns `{ok, code, stdout, stderr, timed_out}`. It holds the screen, so
  keep it for quick commands.
- `deck.job(argv, {cwd, timeout}, function(result))`: run in the background
  (default 30 seconds, at most 300) and call back with the same table. Returns
  an id. At most 8 at once.
- `deck.every(seconds, fn)` (at least 1 second) and `deck.after(seconds, fn)`
  return an id; `deck.cancel(id)` stops one.
- `deck.fs.watch(folders, fn, {settle})`: call `fn` once writes in any of the
  folders settle (default 1 second, at most 60), instead of polling them.
  Returns the id `deck.cancel` takes and how many folders exist and are
  watched; a folder made later needs a new watch. At most 16 watches of at
  most 64 folders each.
- `cwd` defaults to `deck.start_dir`. `argv` is a list of words: no shell.

## Drawing

- `deck.ui.section(id, {title, lines, phone, sidebar, pinned, header})`: a section.
  `lines` is a list or a function returning one, called each time the deck
  draws it again; each line is text or a list of `{text, style}` pairs.
  `phone = false` keeps it off the phone; `pinned = true` keeps it at the
  top with the header. `sidebar = false` hides it from the desktop sidebar
  while preserving its phone panel. `header = true` right-aligns the first line on the
  repository title row beside the divider, reserving space and clipping the
  repository title as needed. `header = "inline"` draws the first line right
  after the title text (the start directory, its branch and ⇡⇣ counts), one
  space apart, and is clipped with the title; any other value is an error.
  Either consumes no separate sidebar row and does not
  need an entry in `deck.opt.sidebar`. `sidebar_hidden` also hides header IDs;
  phone panels retain the section's title and lines. The same id replaces the section.
- `deck.ui.machine(id, {label, cpu, memory, disk_used, disk_available, status, sidebar})`:
  a row in the stats table beside this machine and the deck's hosts. `cpu`
  and `memory` are percents from 0 to 100 and the disk values are bytes; any
  left out shows `–`. `status` (at most 80 characters) replaces the readings
  with why there are none, as `unreachable`. `sidebar = false` keeps the row
  out of the sidebar; `:machines` lists every machine either way. `label`
  (at most 40 characters) is required; the same id replaces the row; at most
  32 machines.
- `deck.ui.remove(id)`: remove a section or machine.
- `deck.ui.badge(function(session))`: return text, `{text, style}` or a list
  of pairs to show after the session's details in its row, or nil.
- `deck.ui.pick({title, items, on_choose = function(item, index, key)})`: a
  picker overlay; `/` reveals live search, Enter chooses. Each item is text or a
  list of `{text, style}` pairs, like a section line. Filtering uses the
  full text; the callback receives that text and its original 1-based index.
  An item table may also set `heading = true` (a group heading with a rule),
  `inert = true` (shown, never selected) and `right = <line>` (drawn against
  the row's right edge; it follows the text the callback receives). An item
  with `cells = {<line>, <line>, ...}` is a table row: its cells line up
  under the picker's `columns`, or under the `columns` of the heading above it
  when that heading names its own. A cell can hold a meter segment,
  `{meter = 0..100, width = cells, style = name}` (width 10 by default, at
  most 40; without a style it turns `warn` at 60 and `bad` at 85). The window
  is centred and fits its rows, up to the screen's height. Options:
  - `filter`: accepted for compatibility; all pickers now hide search until `/`.
    Outside search, `j`/`k` move, `q` closes, and with nothing to select the
    arrows scroll the panel. `Esc` clears and hides search before closing it;
  - `subtitle`: dim status at the window's top right, such as when it updated;
  - `selected`: the 1-based item selected first (headings and inert rows are
    skipped). Drawing the same picker again, with the same title and page and
    no `selected`, keeps the reader's row;
  - `columns`: up to 12 table columns, each a title or
    `{title, align = "right"}`. Their header stays above the rows while they
    scroll, under any inert text rows that start the list; columns with no
    titles line up a label and its value without a header;
  - `stats = {{value = <line>, label = "..."}, ...}`: up to 8 figures over the
    rows, each its value in bold over a dim label;
  - `tabs`: up to 8 pages, each a label or `{label, count}`; `tab` is the open
    one (1-based). ← →, Tab and Shift-Tab move between them and call
    `on_tab = function(label, index)`, which draws that page by calling
    `deck.ui.pick` again; a picker with tabs needs `on_tab`;
  - `keys = {{key = "r", label = "refresh"}}`: up to 8 more keys. A key is one
    character or `backspace` (outside search), `left`,
    `right`, `tab` or `delete`. It closes the picker and calls `on_choose` with
    the selected item and index (nil when nothing is selectable) and the key;
    the phone lists these keys as action rows, labeled with their hint or key,
    and passes the panel's initial selection when an action is tapped;
  - `on_close = function()`: called when Esc closes the picker without a
    choice, not when another picker replaces it.

  Calling `deck.ui.pick` while a picker is open replaces it in place.
- `deck.ui.styles`: the style names (`accent`, `dim`, `faint`, `bold`, `ok`,
  `warn`, `bad`, `working`, `needs`, `finished`, `task`, `review`, `claude`,
  `codex`, `antigravity`, `grok`, `opencode`, `strong`: bold in the accent
  colour, as the repository name). Other names draw plain.

## Data

- `deck.json.encode(value)` and `deck.json.decode(text)` (at most 4 MB).
  `deck.json.array(t)` marks a table, or a new one, to encode as a list even
  when it is empty.
- `deck.fs.list(dir)`: its entries (at most 10,000), each `{name, kind, size,
  ino, mtime}`, with `kind` `"file"`, `"dir"` or `"other"` (links are not
  followed) and `mtime` in nanoseconds; nil when unreadable.
  `deck.fs.stat(path)` gives one such table, following links, or nil.
  `deck.fs.read(path, max)` returns the contents, or nil when unreadable or
  larger than `max` (default 1 MB, at most 16 MB). `deck.fs.realpath(path)`.
- `deck.sha256(text)`: its SHA-256 in hex.
- The standard library is there except `os.execute`, `os.exit`, `io.popen`,
  `io.read`, `io.write` and the standard streams.


## Service extensions

Core and plugin placement is the implementing agent's decision under the scoped
Cepheus `AGENTS.md`. Prefer a plugin for provider adapters, machine/repository
integrations and workflow policies. Extend reusable core facilities when needed.
Extracted features remain bundled and on by default with compatibility preserved.

The persistent service loads `CONFIG/service.lua`, then sorted
`CONFIG/service/*.lua` (64 files maximum). It does not load UI scripts.
A plugin may return `false` at top level to decline activation for the current
repository; it then stays out of the loaded list.
`deck.context` is `"service"` there and `"ui"` in desktop/phone scripts. Trusted
service scripts have the user's local process/filesystem authority, as existing
Lua plugins do; remote clients can invoke only registered commands after the
normal deck authentication. A disabled plugin ID cannot expose a command or own
new process/metadata/title updates.

```lua
-- CONFIG/service/example.lua
local deck = require("deck")
deck.service.command("example", "echo", function(args)
  return { message = args.message or "" }
end, { desc = "Echo literal text", args = { "message" } })

deck.on("session_new", function(session)
  deck.service.annotate("example", session.id, { source = "example" })
end)
```

- `deck.service.command(plugin, name, fn, {desc, args})`: service only; register
  a callback receiving one JSON object and returning JSON. IDs use 1–32 letters,
  digits, `_` or `-`. At most 256 commands, 16 named argument fields, 16 KiB per
  request/result, and the ordinary Lua time/memory limits. Duplicate commands fail.
- `deck.service.call(plugin, name, args, callback?)`: desktop, phone or service;
  callback receives `{ok, data, error}`. Requests use explicit IDs; a late reply
  cannot call a callback in a reloaded engine. Arguments are literal JSON values.
- `deck.service.annotate(plugin, session_id, object)`: service only; updates
  `session.metadata[plugin]`, bounded to 16 KiB and 32 namespaces per session.
- `deck.service.title(plugin, session_id, text_or_nil)`: service only; automatic
  title, clipped to 200 characters and one line. The owner's explicit name wins.
  `nil` clears only that plugin's title.
- `deck.service.process(plugin, name, {cmd={...}, cwd=..., port=...})`: service
  only; launch an argument vector in its own process group with a private log.
  `nil` stops that owned process. A declared busy port is never adopted/stopped.
  At most 16 processes, 128 arguments and 4096 bytes per argument. Identical
  starts are idempotent across service handover. `deck.state().processes` reports
  owned process snapshots; `process_end` reports its plugin/name/result.

`ready`, `state`, `session_new`, `session_end`, `status`, `finished`, `needs_you`,
`cleared`, `process_end` and `shutdown` are available in the service. So is the
service-only `update_available` (the session): once per session of this host, when its
program first shows its own update notice (Claude Code and Codex), carried across a
handover. Timers/jobs use the
existing APIs. View-only
selection/picker/command operations do not execute in the service. Plugin process ownership and metadata/title state survive an exec
handover; stopping the service gracefully stops its owned processes.

Phone control discovery: `control("plugins", {})` returns
`{plugins:[{id,name,description,enabled,commands:[{name,description,args}]}]}`.
Invoke with `control("plugin", {plugin="example",command="echo",args={message="hi"}})`.
Capabilities appear only for services supporting the API. Existing phone actions
and default controls remain available while their bundled plugins are enabled.

## Durable job sessions (protocol 14)

`deck.job_session.start/status/log/stop/dismiss(args, callback)` is asynchronous
in UI and service Lua. Callbacks receive `{ok,data,error}`. This facility differs
from `deck.job` (a bounded extension helper) and `deck.service.process` (an owned
plugin process). No job argv, environment, stdin or output appears in
`deck.state().jobs`; that array contains display metadata only. `job_capable`
marks availability on a host. Finished results remain until dismissed or their
configured retention expires (7 days by default).

Start arguments are `{key,title,argv={...},cwd,placement,provider,account,stdin,
env_clear,env_allowlist={...},env={...}}`: `key`, `argv` and absolute `cwd` are
required; placement defaults to `auto`. A key owns one immutable specification
at one endpoint. Reconcile that key after a lost reply; never dispatch a new key
or runner merely because acknowledgement was lost. Start returns `{job=Info}`
with status `starting`; resource/quota/stagger holds remain visible in `reason`.
Status accepts `job` or `key` (both optional) and returns `{jobs={Info,...}}`.
Stop/dismiss take `job`, and return `{job=Info}`. Dismiss requires a terminal job.
`Info` has `id`, `title`, optional `provider`/`host`, `status` (starting, working,
finished, failed), Unix-second `created_at`/`started_at`/`finished_at`, optional
`exit_code`/`signal`, and `reason`. A lost supervisor has an unknown exit and is
never automatically retried. IDs are job references, never terminal controls.

Log accepts `{job,offset=0,limit=65536}` and returns `{job,offset,text,items,
items_truncated,eof}`. Offset is the next/end byte cursor; `text` is that raw page. Limits below 4 bytes round up to 4 so a UTF-8 character can advance intact.
Replace chat with `items` on each response: these shared conversation items parse
a cumulative bounded window, rather than the raw page. The window is 1 MiB and
600 items; truncation is explicit. Incomplete final JSON records wait; EOF means
terminal plus all raw bytes read. There is no composer, prompt answering or
interactive stdin for jobs. Provider task stdin is wrapped as non-owner input.

CLI is `ctl job OP [JSON|-]`; `-` reads a manifest privately from finite stdin.
The phone uses `control("fleet", {operation="job-OP",args=JSON})`; the same actions
are exposed by `ctl fleet job-OP JSON`. Protocol 14 and the `jobs` capability are
required on the selected executor. Queue support is advertised separately.
Explicit provider/account configuration determines quota; the
facility itself never calls a model or grants integration/shipping authority.

## Attention (protocol 15)

`deck.attention` supplies asynchronous `status`, `list`, `send`, `message`,
`cancel`, `decide`, `supervisor-status`, `supervisor-set`, `supervisor-rules`
and `supervisor-off`. Each accepts `(args, callback?)` and
returns through `{ok,data,error}`, like the queue APIs. Hyphenated names use Lua
brackets: `deck.attention["supervisor-status"]({}, callback)`. Check protocol 15
and `deck.state().attention_capable`. An older/unavailable owning service must
report Update needed rather than falling back to immediate input. Baseline
attention read/status remains protocol 15, native Read permission hooks require
16, and idle-send creation/retry requires every selected endpoint to support 17.

CLI: `cepheus ctl attention OP JSON|-` or
`cepheus ctl fleet attention-OP JSON`. The phone uses
`control("fleet",{operation="attention-OP",args=JSON})`. `host` selects an
explicit peer where applicable. Keep returned IDs opaque. Message identity is
`(host,id)`: `MessageInfo.host` is always present and null for the local service.
Pin private fetch/cancel to that host, using `"@here"` for null, rather than
routing by a guessed UUID or session. Retain original host/session/key/text and
`expected_target` when reconciling sends. Decision fanout resolves registered
shared-repository mappings from the coordinator-local canonical repository and
freezes remote recipients.

| Operation | Arguments and result |
| --- | --- |
| `status`, `list` | `{}` → `{attention=Snapshot}`; metadata only. |
| `send` | `{key,session,text,expected_target={spawned_ms,pid,conversation}}` → `{message=MessageInfo}`; protocol 17, stable key and exact immutable captured payload. |
| `message` | `{id,host}` → `{message,text}`; explicit private body fetch. |
| `cancel` | `{id,host}` → `{message}`; queued/held and no attempted delivery. |
| `decide` | `{key,repository,text,recipients?}` → `{decision={id,created_at,recipients}}`; explicit repository path and optional exact live IDs. |
| `supervisor-status` | `{}` → `{supervisor}`. |
| `supervisor-set` | `{enabled=boolean}` → `{supervisor}`; explicit local setting. |
| `supervisor-rules` | `{}` fetches private rules; `{rules=[...]}` validates/replaces them and advances the revision. |
| `supervisor-off` | `{}` disables locally and requests fleet disable; inspect `supervisor.pending` until every peer confirms. |

`Snapshot` has `queued`, `messages` and `supervisor`; its `inbox` and `stalls`
are always empty (kept for older phone apps). `queued` maps session IDs
to pending counts. `MessageInfo` has `{host,id,session,state,created_at,updated_at,
reason}`; states are `queued`, `held`, `pasted`, `submitted`, `unknown`, `canceled`.
`SupervisorInfo` has `{enabled,revision,rules,pending,audit}`; `rules` is a
count, not rule bodies.
Audit items contain `{session,at,category,rule?}`.

Messages accept nonempty text up to 16,000 UTF-8 bytes; decisions accept up to
15,900 bytes. Keys are nonempty, at most 128 bytes, without control characters.
For a new idle send, capture `expected_target` directly from the selected
session metadata before creating the intent. All three fields are required;
`spawned_ms` must be nonzero, PID must be known and greater than 1, and
`conversation` is the exact string or explicit null. Missing/zero generation
metadata holds clients with Update needed. Never refresh this object after
timeout, reconnect or a target reopen. A known prior receipt is looked up
first and reconciles its original target, without admitting replacement input.
Replay a stable key with its unchanged payload; do not substitute another
session, repository, recipient list, host or captured target after timeout. The
first decision captures process/conversation identities and publishes `.cepheus/decisions.md`
once at the repository root. Agent delivery uses private escaped non-owner
input and cannot grant repository authority.

Queued input shares the guarded admission journal. It requires an idle or
finished agent, a positively empty composer, fresh provider output and no
pending question, approval, submit or other automatic input. Per-session order
is preserved while a bounded rotating scan serves other targets. Changed
process/conversation custody becomes unknown. Cancellation cannot retract
attempted input. Unknown/pasted receipts are never automatically replayed,
including after handover; clients retain and reconcile the original intent.
Explicit body fetches do not transfer editable-draft custody.

Supervisor rules are private, at most 32, shaped
`{id,tool,pattern={kind="exact",command=...}}` or
`{id,tool,pattern={kind="argv_glob",tokens={...}}}`. Configuration alone does not
make a provider capable: a trusted adapter must be the hosted executable and
emit a private identity-bound Hook with complete command/executable/path/prompt
proof. Built-in terminal backends have no such adapter; the separate native
Claude Read hook below supplies its own structured proof. Core rechecks rule
revision and input/output custody at the guarded write. Forbidden command/path classes
always decline; only a matching current single allow-once prompt can receive
Enter. Rule counts, categories and rule IDs may be displayed; never publish
command bodies, tokens or raw Hook observations.

### Native Claude Read hooks (protocol 16)

Native permission hooks (protocol16) use dedicated local Hook frames, not Fleet
or Lua input authority: `permission_request` carries session/spawn identity,
per-process token and the original bounded Claude object; `permission_decision`
answers its prompt ID once. Native exact rules have `tool:"Read"` and
`pattern:{kind:"native_exact",input:{file_path:absolute,offset?:1..10000000,limit?:1..10000}}`.
The bundled helper uses a qualified absolute interpreter with exact `-I -S`
arguments in Claude's exec-form hook. The existing owner-controlled hosted CLI
environment remains trusted. The native hook pins `LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`, `GCONV_PATH`,
`LD_ORIGIN_PATH`, `LD_PROFILE` and `LD_DEBUG_OUTPUT` to empty through the
command-line settings source, after project/local settings. Enterprise-managed
policy is a trusted higher-precedence source; a workspace cannot replace those
pins. Paths containing CLI template placeholders are not qualified.
Paths have at most4096 UTF-8 bytes. Unknown fields/coercions decline. The shared
32-rule/unique-ID limit includes terminal rules. Core requires an actual Git
worktree and excludes authentication/state stores; files must be regular,
<=16MiB, single-link, in-worktree and free of symlink/credential/hidden paths.
The helper verifies Welcome>=16 before sending a new frame. It has a single
absolute two-second input/socket deadline; missing authority produces no stdout.
The service's categorical durable claim survives restart; duplicate or uncertain
requests never replay. A successful native audit means a complete one-request
allow response reached the socket, not that the provider completed its tool.

Claude's measured `prompt_id` identifies a user turn, not a tool-use invocation.
Pure eligibility is evaluated before any durable `create_new` claim. An
ineligible Read returns neutral/manual and leaves that turn unclaimed. After
claiming the first eligible attempt, an allow or uncertain result permanently
fences further attempts for the same turn, across restart. Hook PID/start and
the local connection identify the invocation; no tool-use ID or tool-completion
evidence is invented.

No attention operation grants merge, push, CI, deployment or shipping authority.

## Work queues and chores (protocol 14)

`deck.work_queue.add/list/cancel/reorder/enable/batch(args, callback?)` and
`deck.chores.register/list/probe/enable/run/claim/complete/update/remove(args,
callback?)` return one `{ok,data,error}` table. Claim and complete are private
service-only Lua worker operations. The authenticated owner CLI supports the
same finite JSON operations with `ctl queue OP JSON|-` and `ctl chore OP JSON|-`.
Gate these independently on protocol 14/work-queue/chores; Jobs also requires 14.

Queue add takes `{key,title,kind="job"|"session",cwd,placement="auto",priority=0,
argv?,prompt?,provider?,model?,account?,dependencies={},budget?,stdin?,env_clear?,
env_allowlist={},env={}}`. Absolute existing cwd and an immutable endpoint-local
key are required. Jobs use literal argv; sessions use provider+prompt. Internal
chore guard/context fields cannot be supplied by ordinary add. Batch takes
`{items={Spec,...}}` (1–100), validates all records/dependencies before publication,
and returns `{items}`. Add returns `{item}`. List filters optional `id`/`key` and
returns `{enabled,items,pause_pending,pause_effective}`. Enable takes `{enabled}`
and returns the same pause metadata. Dispatch defaults off. Disable requests a
new-body pause: pending executor names remain explicit until the executor gate
acknowledges paused or already-running. Running bodies continue. Only explicit
`pause_effective=true` confirms pause; no synchronous offline-host guarantee exists.

Metadata Info has kind/id/title/state/priority/created_at/updated_at/reason,
dispatch (nullable `{kind="job"|"session",id}`), dependencies/source/budget,
cancelable/provider/model/placement. Timestamps are Unix seconds. States are
queued, held, dispatched, done, failed. Accepted admission-held jobs expose held
and their dispatch identity, with cancelable false. Cancel takes `{id}` and
requires no attempt. Reorder takes the complete current cancelable `{ids}` set,
resets priorities and returns authoritative ordered `{items}`. Priority descends,
then stable FIFO. Retry unknown mutations with the same key at the same endpoint.

Dependencies are either `{queue=ID,condition="finished"}` or
`{source=ID,task=ID,condition="ready"|"merged"}`. All must hold; source facts must
be fresh and exact-state. Finished requires a successful job receipt or persisted
interactive completed-turn generation, even if the owner already began a new
turn. Unknown exits and disappearance never imply completion. Self/cyclic or
missing queue references are rejected atomically. Budget is finite positive USD
metadata; cost enforcement is separate. Payloads are private; queue records have
bounded 30-day terminal retention and retain live dependency prerequisites.

Chore registration takes `{id,owner,repository,title,target="session"|"job"|"idle",
schedule={cron="m h dom month dow",timezone="local"|"UTC"},spec,enabled=false,
completion="dispatch"|"worker",guard={...}}` and returns `{chore}`. Spec.kind must
match target; idle uses a session spec and no guard. Owners must have a live,
enabled registered service binding, or be explicit owner configuration (`owner`).
Enable/run never implicitly enables the work queue. Update takes `{id,schedule}`;
remove requires disabled with no unfinished claim. Run takes `{id,key}` and
returns `{chore,item}`; list/probe filter optional id/repository and return
`{enabled,chores}`. Public rows include owner_live, placement, guarded, provider,
registered/enabled/effective_enabled, target/schedule/next_run, claim and outcome.
Private probe additionally exposes a guard fingerprint; ordinary state does not.

Claim takes `{id,key,session}` and returns private `{claim}` with day, occurrence,
generation/session/claimed_at/stale_at, token/key/item/outcome. Complete takes
`{id,day,generation,session,token,outcome="done"|"clear"|"failed"}`. Exact day,
generation, session and token must match. An eight-hour lease becomes reclaimable
only strictly after the boundary and after prior workload quiescence; lost/offline
custody remains held. Retired request retries cannot acquire replacement leases.
Tokens remain private environment/state, never argv or public snapshots.

Daily fixed schedules claim one local-date slot, including early manual runs and
current-day catch-up. Other cron schedules use local date+scheduled minute; missed
minutes are skipped and repeated DST minutes deduplicate. Core scheduling leases
and model selection never replace a repository plugin's independent authority.
All task deliveries preserve non-owner provenance. Idle targets use the existing
fresh composer/draft/input checks; rendering/listing never delivers input.

## Task sources (protocol 14)

`deck.task_sources.register/publish/fail/reconcile/list/refresh/queue(args, callback?)`
uses one callback table `{ok,data,error}`, like `deck.job_session`.
Registration, publishing, failure reporting and reconciliation are service-only. Inspect
`deck.state().work_queue_capable` before offering source controls; older services
need an explicit update message. Job support is independently advertised and
also requires protocol 14.

- `register`: `{id,owner,repository,mode="push"|"command",cwd?,argv?,
  interval_seconds=20,enabled=true}` returns
  `{source,publisher_token,sequence}`. Repository is an immutable configured
  alias. Command mode uses a literal argv whose stdout is one JSON task array;
  push mode keeps its token in the service plugin's private state.
- `publish`: `{source,owner,publisher_token,sequence,tasks={...}}`.
  `fail` uses the same fence with a categorical `error` instead of tasks.
  Advance the last acknowledged sequence by one; reconcile registration after a
  lost response. Both return `{source}`. A failed/invalid snapshot retains the
  previous rows as stale rather than publishing an empty success.
- `reconcile`: `{owner,ids={...}}` disables omitted descriptors belonging to
  that live owner, retains their cached rows as stale, and returns
  `{sources,tasks}`. On an authoritative configuration load, reconcile an empty
  array first and register current enabled descriptors only after acknowledgement.
  Missing, invalid or disabled configuration must register none. Do this on
  load/change, never every polling interval; otherwise slow valid work would be
  cancelled repeatedly. Re-enabling requires fresh output, and results from
  disabled old jobs cannot publish.
- `list` and `refresh` accept optional `source` and return
  `{sources,tasks}`; refresh schedules work asynchronously. `queue` requires
  `{source,item,spec}`, including the caller's complete executable spec, and
  returns `{item}`. Imported titles and next steps never supply that spec.

Each task is `{source_id,id,title,state,worktree?,branch?,next_step?,target?}`; state is
`active`, `ready`, `merged` or `stopped`. A source exposes
`{id,owner,repository,host?,enabled,updated_at?,fresh,error?}`; tokens and
commands never appear in public state. IDs are opaque, endpoint-scoped and
namespaced by plugin/repository/logical source. Send every operation back to
that origin endpoint; do not coalesce unqualified peer IDs.

An optional `target={tool,conversation}` identifies a saved native agent
conversation. Its endpoint comes only from the source's `host`; targets cannot
override it or name a shell. Explicit task selection can reopen that conversation
after its worktree is detached. Resolve ownership on the source service, never
from a viewer's local bindings or matching path.

A complete snapshot permits 1,000 rows and 256 KiB encoded JSON. Source/task IDs
are at most 256 UTF-8 bytes without control characters; titles 200 bytes,
worktrees 4,096, branches 256 and next steps 4,000. Never truncate an identity.
The dedicated publish boundary permits 256 KiB; other Lua JSON calls retain
their 16 KiB limit. Refresh intervals are at least 20 seconds. Stale/offline
observations remain displayable but cannot unlock dependencies or source queue
admission. Provider policy belongs in plugins. The optional Markdown and
GitHub providers require private configuration and publish only the bounded
metadata allowed by the source registry. Custom adapters resolve native
conversation ownership at their source endpoint.

Authenticated phone controls use
`control("fleet",{operation="source-OP",args=JSON})`; CLI uses
`ctl fleet source-OP JSON`. No source operation grants integration or shipping
authority, and none invokes a model.

## Fleet facilities (protocol 14)

`deck.slots` and its alias `deck.scheduler` are available in UI and service Lua.
`status()` returns a copy of the local scheduler snapshot, or `nil` before the
first state arrives. The asynchronous
methods `hold(args, callback)`, `release(args, callback)`, `admission(args,
callback)`, `enqueue(args, callback)`, `cancel(args, callback)`, `record_rerun(args,
callback)` and `rerun(args, callback)` return through the existing callback shape
`callback({ok, data, error})`. A hold returns its ticket immediately; inspect `holders`
/ `waiters` in subsequent state to see whether it has granted.

```lua
deck.slots.admission({}, function(result)
  if not result.ok then deck.notify(result.error, {error=true}) end
end)
deck.slots.enqueue({provider="codex", placement="auto", title="Focused checks"})
```

`hold` requires `pid` and accepts `session`, `pool` (default heavy), `label`,
`compatibility` (an existing owned legacy lock directory), and idempotency `key`.
`release` takes `id` or `key`. `enqueue` accepts provider, optional model, cwd,
title, prompt (≤16000 bytes), fallback provider, account profile and placement
(`auto`, `local` or a configured host). `cancel` removes a start by id only
before dispatch. Once a remote request has been sent it is refused, preserving
the ownership receipt while delivery is reconciled; it never stops a session. Automatic input additionally requires no queued submits/answer keys/reviews, fresh provider output after prior input, and a positively empty whole composer; unknown chrome or handover freshness holds delivery without consuming a rerun or retry attempt. Manual broadcast behavior is unchanged. Queued start prompts, automatic continues and memory reruns are delivered inside an escaped
`<cross-session-message source="cepheus" encoding="html">` envelope, preserving non-owner
provenance; decode its body once for the original text. Stored queue prompt text stays unchanged. No operation grants
repository integration authority.

Snapshots include `headroom`, authoritative `memory_free` / `admitted`, pool
capacities, holders/waiters, starts with hold reason/reset, paused sessions with
known or unknown reset time, reruns and repository identity mappings. `deck.state().host_schedulers` has connected peers' snapshots;
its session references use federated IDs, while slot/start/rerun ticket IDs remain
host-scoped. Snapshots and queued payloads are private runtime state, not log data.

Native phone tools call `control("fleet", {operation=ACTION,args=JSON})`. Generic
CLI uses `cepheus ctl fleet ACTION JSON`. Actions are `capabilities`, `status`,
`admission`, `slot-hold`, `slot-release`, `slot-status`, `enqueue`, `cancel`,
`rerun-record`, `rerun-dismiss` and `rerun`. An optional `args.host` routes to an
online configured peer, is removed before forwarding, and requires protocol 14.
Federated `session` references must match their destination. Snapshot reads from
this explicit host API retain host-local session IDs; do not use them as global
session controls. Internal `start` is an idempotent, admission-gated queue start.

`rerun-record` accepts one `command` (≤4000 bytes) or `commands` (≤50), a live
`session`, and optional absolute original `cwd` / `authoritative`. The service
binds records to that session's current conversation. Repository-owned records cannot be
replayed by the deck. `rerun-dismiss` clears exact commands for a session. The
rerun action takes an id, requires free memory, an idle session and no pending
question, and preserves broadcast input safety. It never executes a shell line
or invokes a model itself. `slot-status` with an `id` returns only `{slot}` for
bounded polling; unfiltered `status` returns the host snapshot.

Queued cross-host starts carry their registered repository identity. The
receiving service resolves the mapped cwd and rejects a directory outside that
repository root, including a symlink escape. Explicit manual-host starts retain
their existing cwd behavior. Plugins use `deck.new` and the fleet API normally.

Interactive execution keys carry their creation time. Removed sessions retain
private key/fingerprint/id/turn-count receipts for 30 days, without task text.
A missing expired key returns unknown/expired instead of starting another body;
inspect custody before submitting a new item. Legacy non-timestamped receipts
remain compact and fail closed at the safety capacity rather than losing replay
ownership. This bounds new receipt retention while preserving late-reply safety.
