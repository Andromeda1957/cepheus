# Tasks dashboard

The bundled Tasks UI displays published task-source metadata from any configured
source. It does not read a project-specific task ledger or discover work on its
own. The optional [task-sources helper](task_sources/README.md) supplies read-only
Markdown checklist and GitHub Issues providers.

| Command | View |
| --- | --- |
| `:tasks` or `:tasks open` | Ready, active and stopped tasks, grouped by state. |
| `:tasks ready` | Tasks published as ready. |
| `:tasks active` | Tasks published as active. |
| `:tasks stopped` | Tasks published as stopped. |
| `:tasks merged` | Tasks explicitly published as merged. |
| `:tasks all` | Every retained task, including merged tasks. |
| `:tasks queue [page]` | The same dashboard; choosing a row opens an explicit queue-spec command. |

Left/Right, Tab and Shift-Tab switch pages. `/` searches the current page. The
Stopped tab appears when stopped tasks exist or when explicitly requested.
Published titles appear in the Task column; opaque source and task IDs stay
unchanged for navigation and queue admission.

Enter selects a matching live agent or resumes a source-published saved native
conversation on its host. A task without a conversation reports that none was
found. Queue selection only prefills `:queue source SOURCE TASK --title 'New task
job' -- `: supply the executable and literal arguments yourself. Task titles and
next steps never become commands or prompts.

Stale sources retain their last published rows with a warning; core rejects
stale source queue admission. Opening a dashboard asks for the complete retained
list without refreshing a provider or dispatching work. Queues remain disabled
until explicitly enabled.

The existing hidden desktop section and session badges remain available. Set
`deck.opt.tasks = false` in `init.lua`, or disable the `tasks` bundled plugin in
`config.json`, to turn off the UI. Source registration is independent of the UI.

## Small local example

Configure one repository alias and the Markdown provider as described in the
[helper setup](task_sources/README.md). A real `TASKS.md` can contain:

```markdown
- [ ] Review parser diagnostics
- [ ] Document command shortcuts
- [x] Add project overview
```

After the source publishes its first snapshot, `:tasks open` shows two Ready
tasks and one Stopped task. `:tasks all` includes the same three rows. Markdown
does not infer Active or Merged; those pages remain empty unless another source
explicitly publishes those states. Checkbox changes are observed by the normal
source refresh.
