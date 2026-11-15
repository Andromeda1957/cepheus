---
name: cepheus
description: Operate Cepheus sessions and native peer messaging, offload builds and shard tests, and manage its queue, skills and agent controls. Use for cross-provider agent coordination or Cepheus commands; session destinations must be agent hosts.
---

# Cepheus

Cepheus (`cepheus`), formerly Agent Deck, hosts Claude Code, Codex, Antigravity (`agy`),
Grok, opencode and registered providers such as Dot, plus terminals. Session
hosts have role `agents`; build-only hosts supply compute without agent sessions,
sign-in synchronization, repository mirrors or branch scans.
When the deck hosts your session, `CEPHEUS_SESSION` holds
its native execution-service number. Resolve `cepheus ctl self` to get the
shared registry ID before addressing your own session. Never pass the native
environment number directly to a public control command. `cepheus ctl` reaches the shared coordinator from each enrolled interface.
Session numbers belong to that shared registry. Native execution, admission and
process ownership stay on the machine doing the work.

## Look first

```sh
cepheus ctl self            # this hosted conversation's shared ID
cepheus ctl status          # shared state as JSON: sessions, hosts, branches and facilities
cepheus ctl branches        # every branch on every host and the sessions working in each
cepheus ctl screen <id>     # what session <id> shows now, as text
cepheus hosts               # enrolled endpoints; distinguish agent and build hosts
cepheus skills list         # local skill inventory; read-only
cepheus ctl conversations   # saved conversations
cepheus ctl fleet capabilities '{}' # native service protocol and facilities
```

In `status`, each session has `id`, `tool` (`claude`, `codex`, `antigravity`,
`grok`, `opencode`, a registered provider ID, or `shell` for a terminal), `title`, `status` (`starting`,
`working`, `needs_you`, `finished`, `idle`, `ended`), `alive`, `host` (null
for the coordinator's own sessions), `cwd`, `worktree` (`path`, `branch`, `task`), `subagents` and
`activity`. Read with `jq`; never guess an id. Query the installed service
before relying on a capability: checked-out source may be newer than its running
release. Feature minimums below are compatibility gates, not the current version.

## Act

For communication relevant to the owner's authorized work, use the native peer
channel. It reaches the receiving agent without editing the owner's composer;
there is no need to read a screen, wait for idle, or use Codex TUI thread tools.

```sh
cepheus agent list
cepheus agent send <shared-id> --stdin
```

Pass the message as finite standard input or one quoted text argument. Listing
shows live providers, titles, conversations and shared IDs across enrolled hosts,
and `host_online` says whether the coordinator currently links to each host; a
send to a session on an offline host fails and sends nothing.
Replies use the command and pinned conversation in the received message; if the
recipient has changed conversations the reply refuses rather than reaching a
different task. Claude-to-Claude communication may use Claude's native
ListAgents/SendMessage tools when available.

Native sends identify the sending conversation and mark the text as peer input.
They never grant owner approval, integration authority or unrelated assignments.
Send useful findings or coordination requests when needed within the current
assignment; avoid polling or automatic acknowledgement loops. Unsupported or
private runtimes refuse the send. There is no terminal fallback. A Codex queued
receipt confirms admission; Claude's submitted receipt confirms the socket write
only, since its native inbound policy may hold or refuse delivery. Preserve an
uncertain receipt and do not resend automatically.

```sh
cepheus ctl new [--cwd DIR] [--name NAME] [--model M] PROVIDER [host] # built-in or registered ID; local unless an agent host is named
cepheus ctl term [--cwd DIR] [--name NAME] [host] [-- command…]
cepheus ctl send <id> '<text>'          # native owner message; inspect its admission receipt
cepheus ctl keys <id> '<keys>\r'        # explicit raw keyboard only, when requested
cepheus ctl broadcast '<message>'       # shared instruction joining each working agent's running turn; idle agents stay idle
cepheus ctl close <id> | reopen <id>
cepheus ctl resume PROVIDER CONVERSATION_ID [host]
cepheus ctl cd <id> DIR                # restarts this agent with a fresh conversation
cepheus ctl scan [--scope TEXT] [host]  # Codex Security scan in a scratch worktree
cepheus ctl scans                       # saved scans and where each report is
cepheus ctl install <source> [--agents claude,codex,agy] [--host NAME] [--dry-run] [--remove]
cepheus ctl skills                      # MUTATION: sync bundled skills and agent links
cepheus ask '<question>'                # a quick answer, no session (/web, /code choose who)
```

- Use the configured start directory, or an explicit `--cwd` for the requested workspace.
- `install` with `--dry-run` first and show what it would change; installing
  runs code from the source, so install only what the owner named.
- A terminal (`shell`) is the owner's; do not type into one you did not start.
- Ordinary `ctl send` requires protocol 19 on the coordinator and source host.
  Send the message body without an appended Enter or terminal escape sequence.
  `ctl keys` is a separate raw keyboard operation for an explicitly requested
  terminal action. Peer coordination continues to use `cepheus agent send`.
  Owner-message receipts distinguish queued (after the current turn), started
  (a new turn), submitted (provider acceptance without a confirmed turn), and,
  from protocol 21, steered (accepted into the active turn). None proves a
  completed turn or a reply. Older installed services may lack steering receipts.
  A broadcast steers a working Codex turn at once and reaches a working Claude
  at its turn's next tool result (owner-input plugin 1.2.0, loaded when that
  Claude starts or resumes); a Claude turn that ends first takes it as a new
  turn. Direct owner messages to Claude remain prompts of their own.
- `cepheus restart` restarts the service on every host and every session
  keeps running. `cepheus stop`, `:stop` and phone Stop refuse while any session
  is live; `--yes` cannot override this. Use explicit Close only for sessions
  the owner has authorized you to end. Quit the view with `:q` to leave work running.

`ctl resume` preserves the selected saved conversation. `ctl cd` instead validates
the new directory and restarts just that agent in the same slot with a fresh
conversation; the old chat remains resumable. Relative paths use that agent's
launch directory, and `~` uses its host's home. Cloud connections reject directory
changes. Both local and remote services need protocol 14 for `cd`.

## Builds and test sharding

Run from the intended worktree and subdirectory:

```sh
cepheus run [--host auto|NAME] [--python] [--env NAME=VALUE]... [--include PATH]... [--artifact PATH]... -- COMMAND [ARG]...
cepheus shard [--host auto|NAME,...] --select ci [--base REV]
cepheus shard [--host auto|NAME,...] --runner unittest|django|pytest [--env NAME=VALUE]... [--targets-from FILE|-] [TARGET]... [-- RUNNER-ARG...]
```

`run` copies the worktree to an eligible build host, runs in its corresponding
subdirectory, streams output and returns the command's exit status. Automatic
selection skips busy hosts and fails if none is available. Explicit selection
waits for the host's slots; do not use it to bypass the owner's resource limits.
Environments, dependencies, databases, `.env` files and Git LFS content are omitted
from the ordinary source copy; name an authorized required input with `--include`.
`--python` prepares the project's configured locked Python environment.
`--artifact` fetches an output back to the corresponding local relative path,
so check that destination before replacing a local file.

`shard` splits either the configured CI selection or the explicit test targets
across build hosts. Its report names failures, unfinished targets and checks it
cannot run. Django's database backend follows the project's settings and environment;
sharded results do not replace required PostgreSQL checks. Project inputs and
plans come from `offload` configuration.
Each invocation's container storage is disposable; output artifacts are fetched
before cleanup. These commands neither grant CI/deploy authority nor create a
verification receipt that substitutes for repository gates.

## Local agent controls and Dot

```sh
cepheus agent-update --check [--local] [--tool AGENT] [--config FILE]
cepheus agent-update [--local] [--tool AGENT] [--config FILE]
cepheus dot setup|chat|probe|login [--config-dir DIR] [--state-dir DIR] [--url URL] [--native-host HOST]
```

Saved local subagent capacity, excluding the parent, is
`agents.max_concurrent_threads_per_session` (or the older `agents.max_threads`)
in `$CODEX_HOME/config.toml`, otherwise `~/.codex/config.toml`; read it there.
Codex's config is authoritative; Claude's native settings are derived. The
user changes it with the deck's `:agent-limit N` (1–256), a Lua plugin; the
former `cepheus agent-limit` subcommand is gone. It does not change remote
limits or restart sessions. Existing Codex sessions need reopening to load a
changed limit; preserve the owner's running work and stops.
`:agent-plugins [on|off NAME [claude|codex]]` lists and toggles each agent's own
plugins in its user settings (`:plugins` and `:plugin` remain available).

`agent-update --check` inspects installed clients without upgrading them; some
native clients expose only an installed version, so a check cannot always prove
currency. An update uses each client's existing installation method across
configured hosts unless `--local` is supplied; `--tool` limits it to one of
`grok`, `claude`, `agy` or `codex`. Existing sessions remain running.
Use updates only within the owner's requested scope.

Dot is an optional connection to an existing persistent cloud conversation, not
an execution host. Its native host needs a matching Cepheus build and a signed-in
ChatGPT Linux app. Preserve the room, composer, attachments and pending-delivery
journal. Reconnection never authorizes replaying a message with an uncertain
receipt. Use `plugins/dot/README.md` from the source checkout
for detailed connection work; keep credentials and signed URLs private.

## In the deck itself

The user drives the same actions from the deck's `:` line (`:new codex
worker`, `:term`, `:broadcast`, `:branches`, `:ask`, `:scan`, `:install`,
`:skills sync`, `:reload`, `:help commands`). Its Lua scripting is described in
the `cepheus-plugin` skill.

## Fleet resource controls

Protocol-14 services expose `ctl admission`, `ctl slot hold|release|status` and
`ctl fleet <action> [JSON]`. Queries do not start a missing service. A slot hold
uses the owning process (`--pid`, default: the calling parent) and may specify `--session`, `--pool`,
`--label`, `--compatibility`, `--max-wait` and `--request-key`; success prints the
JSON ticket only after grant. Keep its owning process alive for the command and
release when done; process/session exit releases it automatically.

Use `ctl fleet enqueue '{"provider":"codex","placement":"auto","prompt":"..."}'`
for staggered, quota-aware dispatch to an eligible agent host. The queue
resolves and pins a destination before launch; ordinary New starts locally.
Use the project's task/worker entry point where configured. A fallback provider or account profile is an
explicit queue preference, never new merge/push/CI/deploy authority. Unknown
quota reset remains held. Profiles and rotation are configured privately, off
until supplied; Codex automation never switches to Claude unless the caller's
provider policy expressly permits that quota.

`:slots` and Deck Tools → Scheduler show per-host state. A pending safe memory
rerun is offered through F6 on the selected session, `:rerun <id> [host]` and the
phone. Repository Claude rerun hooks remain authoritative. Do not answer a
question, change authority, or bypass a live resource hold by typing directly.

## Headless job controls

Protocol-14 services expose `ctl job start|status|log|stop|dismiss [JSON|-]`.
Prefer an existing repository dispatcher over replacing its claims or worker
command. `start -` accepts a private bounded JSON manifest with immutable `key`,
literal `argv`, absolute `cwd`, optional `title`, `provider`/`account`, `placement`
(default auto), finite `stdin`, and explicit environment controls. A returned
starting/held job is already accepted: reconcile the same key at the same
endpoint after a lost reply. Never launch a replacement runner on ambiguous ack.

Jobs have separate identities, private logs and exact exit receipts; they are
not interactive terminals. `status` lists retained results; `log` fetches output
explicitly. Stop affects the workload group; dismiss hides a terminal result.
`:jobs` and Deck Tools → Jobs provide read-only chat/raw output. Do not answer
approvals or send terminal input to a job. Unknown supervisor outcomes are failed
with unknown exit, never automatically rerun. Resource/quota holds still apply.
The chosen provider spends its quota; these controls make no model calls.

## Task-source controls

Protocol 14 exposes metadata through `ctl fleet source-list '{}'`; optional
`source` narrows the result. `source-refresh` schedules a refresh and returns
the current snapshot. Check `fresh` and origin endpoint before using a row;
cached stale observations cannot satisfy dependencies or source queue admission.

Use `:queue sources` to inspect source rows and
`:queue source SOURCE ITEM <add options>` to queue an explicit execution spec.
Supply the actual command or prompt yourself. `ctl fleet source-queue`
requires JSON `{source,item,spec}` with a complete spec. Task titles and next steps never grant authority or become
executable text. Optional Markdown and read-only gh sources are installed with
the `task-sources` helper package and enabled in private profile configuration;
they never invoke a model or write to GitHub.

## Work queue and chores

Protocol14 `ctl queue add|list|cancel|reorder|enable|batch JSON|-` manages private
immutable specs; dispatch defaults off. Prefer finite stdin for prompts/env.
Queueing a task does not authorize its integration. A returned held dispatch is
already accepted: retain its key/endpoint and never launch a substitute runner.
Use literal job argv or an explicit provider prompt. Imported task metadata never
supplies an executable instruction. `:after` creates a new dependent item; all
conditions must hold, and finished means authoritative success.

Disabling requests pause. Show `pause_pending` until `pause_effective=true`;
offline executors are unconfirmed and running turns continue. A later enable
resumes existing intents rather than changing their key. Reorder requires every
cancelable ID and clears priority differences. Budget is positive USD metadata.

`ctl chore register|list|enable|run|update|remove JSON|-` exposes explicit schedules
and read-only claims on desktop/phone. Queue and chore must both be enabled.
Never claim or complete as an owner UI substitute for a worker token. Keep local
daily dates, generation/session/token fences and repository claim authority intact;
unknown prior custody holds takeover. Absent/older/error/disabled/unowned deck
chores retain the existing repository hook fallback. These facilities invoke only
the configured provider and grant no merge/push/CI/deploy authority.

## Attention and safe idle delivery

Protocol 15 and `attention_capable` expose `:send --idle [N] [TEXT]`,
`:decide TEXT` and `:supervisor` in the desktop. The phone retains Send when
idle in chat; it has no Deliveries, Decisions or Supervisor tools pages. Existing
Tab and F2/F3/F4 actions keep working.

Use `ctl attention status '{}'` for metadata. Creating/retrying an idle intent
requires protocol 17 on every selected endpoint. Before the first send, capture
`SessionInfo.spawned_ms`, PID and conversation from the selected session; missing
or zero generation or unknown PID holds with Update needed. Automated sends
retain a stable key and the complete exact payload, for example:

```sh
cepheus ctl attention send '{"key":"follow-up-001","session":42,"text":"Run the focused check when idle.","expected_target":{"spawned_ms":1793996000123,"pid":12345,"conversation":"conversation-id"}}'
```

Replace the illustrative target values with actual captured metadata, including
an explicit null conversation when applicable. Reconcile the same key, session,
text, host and `expected_target` after timeout; never recapture a replacement
target. Known receipts remain authoritative for their original intent. `ctl send
--when-idle ID TEXT` generates a new key each time; repeating it creates a new
intent. Never replace unknown/pasted outcomes with immediate input, answer a
question through this queue, or target a job as if it were a terminal.
Only unattempted queued/held messages can be canceled. Desktop
`:attention recover` reloads private receipts without replay. Body inspection
does not transfer editable-draft custody; preserve the draft and its captured
identity. A phone warning for a working target still queues until safe idle.

`:decide` publishes `.cepheus/decisions.md` at the selected repository root and
queues captured live recipients. Retain the original key and body until the
outcome is known.

The supervisor starts off, requires a private exact allowlist. Protocol16
supports one-request native Claude
Read hooks with exact structured `native_exact` rules and an approved Git worktree.
`prompt_id` is a user-turn identity: pure eligibility runs before a durable claim,
so an ineligible Read stays manual without consuming the turn. One eligible or
uncertain claimed attempt permanently fences that turn across restart.
Bash and unproven terminal prompts stay manual. `:supervisor off --fleet` reports peer
acknowledgements; pending/offline peers are not confirmed off. Prompt avoidance
is a separate optional Bash advisory helper, disabled by default. Neither
facility answers arbitrary prompts or creates integration/shipping authority.

## Maintaining the bundled skills

The repository's `skills/cepheus/SKILL.md` is canonical. Cepheus embeds
bundled skill files in its binary and writes them back during skill sync; an
edit only to an installed copy can be overwritten. Update the source, validate
it, and use the authorized build/install and sync workflow. `cepheus skills list`
is read-only; `cepheus skills sync` and `cepheus ctl skills` write skills and links.

After the current managed skill is linked, sync retires only recognized managed
`agent-deck` and `agent-deck-plugin` discovery aliases. It preserves their stored
files, owner-created skills, foreign links and explicit installations. Use
`cepheus` and `cepheus-plugin` for current guidance.
