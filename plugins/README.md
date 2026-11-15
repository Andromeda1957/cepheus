# Cepheus plugins

[Catalog](catalog.json) records bundled integrations, optional Lua plugins and
shared helpers. Lua plugins extend commands, keybindings, sidebar sections,
pickers, reports and service behavior. Settings, credentials and usage history
belong in your personal configuration and state directories.

## Install optional plugins

From the repository root:

```sh
python3 tools/install_plugins.py --list
python3 tools/install_plugins.py --check
python3 tools/install_plugins.py api-cost --dry-run
python3 tools/install_plugins.py api-cost
python3 tools/install_plugins.py --all
cepheus lua check
```

Run `:reload` in Cepheus after installation. The installer uses the directory
holding `CEPHEUS_CONFIG`, or `$XDG_CONFIG_HOME/cepheus` (normally
`~/.config/cepheus`). `--config-dir PATH` selects a separate profile. A built-in
plugin is embedded in the binary and receives no duplicate installed copy.

Identical files are left alone. A different existing file stops installation
before anything is copied. After reviewing the difference, `--update` saves the
old file as `NAME.before-plugin-update-UNIQUE` before replacing it. Personal
settings and state are never included in a package. Restore a saved file by
copying it over the installed version and running `:reload`.

To load source files directly from this checkout, run:

```sh
python3 tools/install_plugins.py --repo-sources
```

This saves `plugin_source_dir` in the selected profile. Reopen Cepheus to apply
it. Personal `init.lua`, `service.lua`, configuration and state keep their usual
locations. Remove `plugin_source_dir` to return to installed copies. Source
mode requires all enabled payloads to exist; a missing source reports an error.

## Optional packages

| Package | Purpose |
| --- | --- |
| `auto-ordering` | Persistent activity ordering within agent-type groups; working sessions retain their places and cleared sessions move last. |
| `api-cost` | One persistent API-equivalent usage total across Claude and Codex on configured hosts. |
| `sidebar-controls` | Show or hide the resources, usage and development-server indicators. |
| `agent-update` | Check or update supported native agent clients using their installed package method. |
| `agent-limit` | Save local Codex and Claude concurrency through one setting. |
| `agent-plugins` | Inspect or enable/disable plugins in each agent's own settings. |
| `agent-settings` | Shared configuration helpers required by `agent-limit` and `agent-plugins`. |
| `task-sources` | Opt-in read-only Markdown checklist and GitHub Issues sources. |
| `dot` | Connect an existing cloud conversation through a native ChatGPT host. |

## Agent settings

Install `agent-limit` or `agent-plugins` and run `:reload`; the installer includes
their shared `agent-settings` helper automatically.

| Command | Action |
| --- | --- |
| `:agent-limit` | Show the saved limit, source path and Claude synchronization status. |
| `:agent-limit N` | Save a local subagent limit from 1 to 256, excluding the parent. |
| `:agent-plugins` | List plugins known to Claude and Codex and their enabled state. |
| `:agent-plugins on NAME [claude\|codex]` | Enable a plugin for a selected agent, or each agent that knows it. |
| `:agent-plugins off NAME [claude\|codex]` | Disable a plugin. |

The concurrency source is `agents.max_concurrent_threads_per_session` in
`$CODEX_HOME/config.toml`, or `~/.codex/config.toml`. The legacy
`agents.max_threads` key is updated in place when present. The helper derives
Claude's `CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS` and
`CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS` in its user `settings.json`
(`CLAUDE_CONFIG_DIR`, otherwise `~/.claude`). Existing Codex sessions must be
reopened to load a changed limit; the plugin never restarts them. Remote hosts
retain their own settings.

Agent plugin names can be a complete `NAME@SOURCE` or an unambiguous installed
name. Claude uses `enabledPlugins`; Codex uses `[plugins."NAME@SOURCE"] enabled`.
Project or command-line settings can still override user settings. Files are
validated before mutation and unrelated settings are preserved. An ambiguous
name, invalid file or unsupported value reports an error. A filesystem failure
while saving the second agent can leave the first saved; the error identifies
that partial result so the same command can be retried.

## Launch settings

The built-in launch-settings integration selects the model and reasoning effort
for new sessions. `Tab` completes model and effort words from each provider's
available model list.

| Command | Action |
| --- | --- |
| `:launch` | Show launch choices and available model/effort commands. |
| `:<model> [effort] [program]` | Save a program's launch model and optional effort. |
| `:<effort> [program…]` | Save the effort for the specified compatible programs. |
| `:launch reset [program] [model\|effort]` | Restore provider defaults for the selected choices. |

Choices live in `launch.json` in the service's state directory. They apply to
new sessions started by the desktop, phone, scripting or scheduler, with explicit
start choices taking precedence. Resumed conversations retain their own model.
Model support comes from provider manifests or native catalogs; an incompatible
choice saves nothing. Provider settings and sessions outside Cepheus retain
their own defaults.

## Usage, ordering and updates

`api-cost` collects at most every 30 seconds and keeps one persistent total;
`:api-cost refresh` requests a fresh collection. This is an API-equivalent
estimate from recorded usage, rather than a provider invoice.

`auto-ordering` preserves agent-type groups and the positions of working
sessions. Stopped sessions sort below working sessions, with cleared sessions
last. Its saved order survives reopening the view.

`agent-update` uses native, npm or mise installation methods as appropriate.
`cepheus agent-update --check` inspects without upgrading; `--local` limits the
operation to this host and `--tool` selects one supported client. Updating a
client leaves existing agent sessions running. Provider sign-ins and package
manager configuration remain external to Cepheus.

## Bundled native integrations

The Rust core owns terminal emulation, session lifecycle, authenticated
transport, input safeguards and reusable extension facilities. Built-in provider
adapters supply Claude, Codex, Antigravity, Grok, opencode and registered-provider
launch, transcript and native messaging behavior. Bundled integrations include:

- Development services through the owned-process supervisor.
- [Repository mirroring](git-mirror/README.md), host bootstrap and sign-in synchronization.
- AI titles, security scans and extension installation.
- Desktop focus, notifications and terminal adapters.
- Provider account profiles and opt-in account rotation.
- Native peer messages and provider-specific captured-target operations.
- An optional permission supervisor and bounded prompt-avoidance advice.

Their defaults and declared commands are recorded in [catalog.json](catalog.json).
Configuration controls optional integrations without rebuilding. Provider account
profiles, remote addresses, project conventions and server commands are supplied
by your own profile; this distribution contains no configured project adapter.

## Cloud conversation connection

Install `dot` and follow [Dot setup and limitations](dot/README.md). This is a
connection to an existing cloud conversation, not an execution host. It requires
a matching Cepheus native host and a signed-in ChatGPT Linux app. Authentication
state stays on that host; preserve message delivery receipts after an uncertain
send instead of replaying it.

## Task sources, scheduler and jobs

The [`task-sources` helper](../lua/task_sources/README.md) can publish configured
Markdown checklists or GitHub Issues as descriptive metadata. Both providers
are opt-in and read-only. Queue admission requires an explicit command vector or
session prompt: a task's title never becomes executable text.

Core owns host admission, slot ownership, placements, durable queue receipts,
background jobs and scheduler state. Lua can register sources, schedules and
worker callbacks through the public service API. Queues and chores start
disabled until configured and enabled. A pause remains pending until its
executor acknowledges it; running work continues.

Jobs have their own identities, logs and exit receipts. They are separate from
interactive terminals. An uncertain supervisor result remains uncertain rather
than being rerun automatically. See the [Lua API](../skills/cepheus-plugin/api.md)
for the supported service commands and receipt contracts.

## Write a plugin

Start with `local deck = require("deck")`. Put an optional entry point in
`plugins/NAME.lua`, helpers under `lua/NAME/`, and tests beside the relevant
component. Register every installed payload and required helper in
`catalog.json`. Keep each entry point small and use the existing generic APIs
for processes, timers, filesystem reads and UI.

Use `python3 tools/install_plugins.py --check` to validate packaging before
installation. Validate Lua with `cepheus lua check`, run focused tests, and
exercise the command or UI in a separate test profile. Do not store machine
paths, credentials or usage state in a plugin package.

The [plugin skill](../skills/cepheus-plugin/SKILL.md) and
[API reference](../skills/cepheus-plugin/api.md) describe commands, keys, events,
sidebar sections, badges, pickers, reports and service facilities.
