---
name: cepheus-plugin
description: Write or change a Cepheus (session deck) Lua plugin or init.lua - commands, keymaps, sidebar sections, session badges, pickers, terminals, events and phone panels. Use when asked to customize or script Cepheus.
---

# Cepheus plugins

The deck runs Lua 5.4 as Neovim does. At start and on `:reload` it loads, in
order:

1. `~/.config/cepheus/init.lua` (the folder holding `config.json`);
2. every `~/.config/cepheus/plugins/*.lua`, by name;
3. its bundled plugins, followed by enabled plugins in your configuration.

`require("x")` finds `~/.config/cepheus/lua/x.lua` or `lua/x/init.lua`.
`cepheus lua example` prints a full example init.lua.

Use Lua for optional integrations and workflow policy. Keep terminal ownership,
session lifecycle, authenticated transport and reusable extension facilities in
the Rust core. Headless Lua extensions use `CONFIG/service/*.lua` and the
service APIs below.

## Write one

1. Read `api.md` beside this file for every function, including the service extension API.
2. In the Cepheus source checkout, put a plugin in
   `plugins/<name>.lua` and start it with
   `local deck = require("deck")`. Keep its helpers under
   `lua/<name>/`, relevant tests in an owning `tests/` directory,
   and register every installed file in `plugins/catalog.json`.
   Built-in plugins keep their existing canonical source and a `builtin`
   catalog entry. All existing and future project plugins must have repo
   sources; an installed home-directory copy is never the only source.
   Personal configuration and usage data stay outside Git. See
   `plugins/README.md` in the checkout for packaging details.
3. Run `python3 tools/install_plugins.py --check`, then install
   with `python3 tools/install_plugins.py <name>` (or `--all`).
   Use `--config-dir PATH` for an isolated profile. Existing differing files
   require `--update`, which preserves a backup. The installer honors
   `CEPHEUS_CONFIG` and `XDG_CONFIG_HOME`; it copies helpers too.
4. Check it without the screen: `cepheus lua check` loads everything as
   the deck would, prints how many keymaps, commands, sections, badges and
   handlers it registered, and exits 1 with each error.
5. In the deck, `:reload` loads it again, F2 shows its errors, and
   `:lua =expr` evaluates an expression (`:lua =deck.state().hosts`).

## Rules the deck enforces

- A call into Lua gets a quarter of a second of processor time (loading a
  file gets two); a loop past that stops with an error, and the deck keeps
  working. Time the machine spends on other programs is not counted. Do slow
  work with `deck.job`, which runs in the background and calls back.
- Programs run from a list of words, as `{"git", "status"}`, never a shell
  line. `os.execute`, `io.popen`, `io.read`, `io.write` and `os.exit` are not
  available; `print` shows its text on the deck's notice line.
- Text styles are theme names from `deck.ui.styles` (`accent`, `dim`, `ok`,
  `warn`, `bad`, …). Never raw colours: the deck follows the terminal theme.
- A command may not take one of the deck's own names (`:new`, `:q`, …).
  Names are letters, digits, `-` and `_`, at most 32.
- Limits: 8 jobs at once, 64 timers, 256 keymaps or commands, 16 sections of
  up to 40 lines, 64 plugin files, 1 MB per file, 64 MB of memory.

## Shape of a plugin

```lua
local deck = require("deck")

local count = 0
deck.every(15, function()
  deck.job({ "git", "-C", deck.start_dir, "status", "--porcelain" }, {}, function(r)
    if r.ok then
      count = select(2, r.stdout:gsub("\n", "\n"))
    end
  end)
end)

deck.ui.section("dirty", {
  title = "DIRTY",
  lines = function()
    return { { { " " .. count .. " changed", count > 0 and "warn" or "dim" } } }
  end,
})

deck.command("dirty", function()
  deck.term({ cwd = deck.start_dir, name = "git", cmd = { "git", "status" } })
end, { desc = "git status in a terminal" })

deck.keymap("alt+d", "dirty", { desc = "git status" })
```

Use `header = true` on a section to put its first line at the right of the
repository title, or `header = "inline"` to put it right after the title
text, without adding a sidebar row. Hide parts without replacing
the order with `deck.opt.sidebar_hidden = { "resources" }`.

Order the sidebar with `deck.opt.sidebar = { "title", "dirty", "sessions" }`
(built-in parts: `title`, `resources`, `dev`, `usage`, `sessions`).
Sections show on the phone's Stats screen unless `phone = false`.

For task-source integrations, use protocol-14 `deck.task_sources` rather than
running queries in a view. Core owns snapshot fencing, freshness and dispatch;
plugins own plain-list, checklist and gh policies. Keep publisher tokens and
commands out of public state, publish only bounded allowlisted metadata, and
retain stale rows on failure. Queue only a caller-supplied full execution spec;
never turn a title or next step into executable input. See `api.md` and
`lua/task_sources/README.md` in the checkout for packaging and private config.
