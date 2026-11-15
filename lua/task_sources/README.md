# Read-only task sources

Install the public helper package in a chosen profile:

```sh
python3 tools/install_plugins.py task-sources --config-dir /path/to/profile
```

The package installs `service/task-sources.lua`; its providers run as
`cepheus task-source`. Installation creates no source configuration.
With a protocol-14 service, add private `task-sources.json` beside that profile's
`config.json`, then restart the service:

```json
{
  "enabled": true,
  "sources": [
    {
      "id": "task-sources:local:project:markdown:tasks",
      "repository": "project",
      "provider": "markdown",
      "file": "TASKS.md",
      "enabled": true
    },
    {
      "id": "task-sources:local:project:github:issues",
      "repository": "project",
      "provider": "github",
      "github_repo": "example/project",
      "enabled": false
    }
  ]
}
```

`repository` names an entry in `scheduler.repositories`. Choose a stable,
namespaced source ID for its endpoint, repository, provider and logical source;
IDs are opaque and must remain unchanged. Each enabled source registers a
literal argument vector with core, which supervises refreshes even with no UI.
The interval defaults to 20 seconds and cannot be shorter. Every configuration
load first reconciles the previous descriptors to disabled, then registers the
current enabled set after acknowledgement. Removing or disabling a source,
disabling the entire configuration, or supplying invalid configuration therefore
stops further polling; cached observations stay stale until fresh output after
re-enablement. This barrier runs once per load, not on every poll. The installed
`task-sources/status` service command reports categorical availability counts.
Unknown repositories or providers do not execute a command.

The Markdown provider reads only its configured, repository-contained file.
Unchecked checklist entries are `ready`; checked entries are `stopped`.
Fenced examples are ignored. Stable item IDs use the canonical relative file,
normalized title and duplicate occurrence; checking or reordering distinct items
does not change their identities. Markdown links do not trigger additional file reads.

The GitHub provider invokes only `gh issue list` with a fixed argument vector
and the user's existing gh authentication. It requests number, title and state,
never bodies or comments. Open issues are `ready`; closed issues are `stopped`.
Enable it explicitly to permit these outbound reads. Neither provider infers
`merged`, invokes a model, writes to GitHub or stores credentials.

Every refresh is a complete bounded snapshot: at most 1,000 tasks and 256 KiB
of encoded metadata. Titles are visibly clipped at a valid UTF-8 boundary to
200 bytes; identities are rejected rather than shortened. Oversized, malformed,
failed and offline sources retain their last valid rows marked stale. Stale
observations cannot satisfy dependencies or be queued as current tasks.

Source metadata is descriptive. Queueing requires an explicit full executable
specification; a title or next step never becomes a prompt, command or owner
instruction. Source references belong to their origin endpoint; identical IDs
on another endpoint are separate registries. The core facility grants no merge,
push, CI or deployment authority.

Custom repository adapters can publish into the same registry through the Lua
service API. Supply an explicit executable specification when queueing a row.
