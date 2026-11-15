//! The deck's `:` commands, the one name every way of driving it shares. A hotkey, a `:` line, a Lua
//! `deck.cmd(...)` and a keymap bound to a command line all end in `App::run_command`; `cepheus ctl` names the
//! same commands for the service's side. This module knows the commands, splits a line into words and completes
//! the word being typed.

/// What a command's arguments are, for completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Args {
    None,
    /// A program, then a host.
    ToolHost,
    /// A host and session options, with the program named by the command.
    AgentHost,
    /// A host.
    Host,
    /// One of these words.
    Words(&'static [&'static str]),
    /// A session's number in the list.
    Session,
    /// Free text.
    Text,
    /// What `:install` and `:uninstall` take.
    Install,
    /// A deck option (`:set`).
    Option,
}

/// One command: its name, other names, how it is written and what it does.
#[derive(Clone, Copy)]
pub struct Spec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub usage: &'static str,
    pub about: &'static str,
    pub args: Args,
}

pub(crate) const fn spec(
    name: &'static str,
    aliases: &'static [&'static str],
    usage: &'static str,
    about: &'static str,
    args: Args,
) -> Spec {
    Spec { name, aliases, usage, about, args }
}

/// Every built-in command, in the order the help sheet lists them. A Lua
/// command may not take one of these names.
pub const COMMANDS: &[Spec] = &[
    spec(
        "new",
        &[],
        "new [claude|codex|agy|grok|opencode] [host] [N] [--cwd DIR] [--name NAME] [--model MODEL]",
        "open the session chooser; with a program, start it directly (N: that many)",
        Args::ToolHost,
    ),
    spec(
        "term",
        &["terminal", "shell"],
        "term [host] [--cwd DIR] [--name NAME] [-- COMMAND…]",
        "a terminal session in your own shell, or running COMMAND",
        Args::Host,
    ),
    spec(
        "claude",
        &[],
        "claude [host] [N] [--cwd DIR] [--name NAME] [--model MODEL]",
        "start Claude Code directly; N starts that many",
        Args::AgentHost,
    ),
    spec(
        "codex",
        &[],
        "codex [host] [N] [--cwd DIR] [--name NAME] [--model MODEL]",
        "start Codex directly; N starts that many",
        Args::AgentHost,
    ),
    spec(
        "agy",
        &["antigravity"],
        "agy [host] [N] [--cwd DIR] [--name NAME] [--model MODEL]",
        "start Antigravity directly; N starts that many",
        Args::AgentHost,
    ),
    spec(
        "grok",
        &[],
        "grok [host] [N] [--cwd DIR] [--name NAME] [--model MODEL]",
        "start Grok directly; N starts that many",
        Args::AgentHost,
    ),
    spec(
        "opencode",
        &[],
        "opencode [host] [N] [--cwd DIR] [--name NAME] [--model MODEL]",
        "start opencode directly; N starts that many",
        Args::AgentHost,
    ),
    spec("scratch", &[], "scratch", "show the scratch terminal, or go back from it", Args::None),
    spec("cd", &[], "cd [DIR]", "change the selected agent's directory and start a fresh chat there", Args::Text),
    crate::extensions::dev_services_commands::COMMANDS[0],
    spec("broadcast", &["all"], "broadcast [message]", "steer working agents; alone it opens the dialog", Args::Text),
    spec(
        "queue",
        &[],
        "queue [list|add|cancel|reorder|enable|disable|sources|source|refresh|help]",
        "explicit work queue; :queue help lists all syntax",
        Args::Text,
    ),
    spec(
        "after",
        &[],
        "after queue ID finished <add options> | after task SOURCE TASK ready|merged <add options>",
        "create a NEW queued item with an AND dependency",
        Args::Text,
    ),
    spec("batch", &[], "batch FILE", "atomically queue up to 100 JSON specs or literal add-option lines", Args::Text),
    spec(
        "chores",
        &["chore"],
        "chores [list|register|schedule|enable|disable|run|remove]",
        "scheduled work; claims and outcomes are read-only",
        Args::Text,
    ),
    spec("slots", &[], "slots", "fleet slots, held starts and memory reruns", Args::None),
    spec("rerun", &[], "rerun <id>", "rerun a memory-stopped command once resources are free", Args::Text),
    spec("dispatch", &[], "dispatch <provider> <prompt>", "queue an admitted, staggered agent start", Args::Text),
    spec(
        "attention",
        &[],
        "attention recover|retry KEY",
        "privately inspect retained operations, then explicitly reconcile them",
        Args::Text,
    ),
    spec(
        "supervisor",
        &[],
        "supervisor [status|on|off|off --fleet|rules [JSON]]",
        "explicit exact-rule permission policy",
        Args::Text,
    ),
    spec("decide", &[], "decide <text>", "record a repository decision and queue fanout", Args::Text),
    spec("send", &[], "send [--idle] <n> <message>", "message session n", Args::Session),
    spec("go", &[], "go <n|name>", "highlight session n, or the first whose title has name", Args::Session),
    spec(
        "close",
        &[],
        "close [n|n-m|n,m,...]",
        "close the highlighted session, a range (7-10), or a list (1,3,5); ranges and lists can mix",
        Args::Session,
    ),
    spec("reopen", &[], "reopen [n]", "reopen an ended session", Args::Session),
    spec("rename", &[], "rename [name]", "name the highlighted session; alone, its own title again", Args::Text),
    spec("resume", &[], "resume", "resume an earlier conversation", Args::None),
    spec("movein", &["move-in", "move_in"], "movein", "move the selected outside session into the deck", Args::None),
    crate::extensions::desktop_commands::COMMANDS[0],
    spec("branches", &["br"], "branches", "every branch on every host and the agents on each", Args::None),
    crate::extensions::quick_ask_commands::COMMANDS[0],
    crate::extensions::security_scans_commands::COMMANDS[0],
    crate::extensions::security_scans_commands::COMMANDS[1],
    crate::extensions::agent_extensions_commands::COMMANDS[0],
    crate::extensions::agent_extensions_commands::COMMANDS[1],
    crate::extensions::agent_extensions_commands::COMMANDS[2],
    spec("hosts", &[], "hosts", "the hosts the deck drives", Args::None),
    spec("machines", &[], "machines", "CPU, memory and disk of every machine, scripts' too", Args::None),
    spec("usage", &[], "usage", "what each model has left of its limits, and when they reset", Args::None),
    spec("errors", &[], "errors [clear]", "the errors, or clear them", Args::Words(&["clear"])),
    spec("restart", &[], "restart", "restart the service on every host; every session keeps running", Args::None),
    spec("stop", &[], "stop", "stop the service and every session", Args::None),
    spec("reload", &[], "reload", "load init.lua and the plugins again", Args::None),
    spec("lua", &[], "lua <code>", "run Lua; `lua =expr` shows its value", Args::Text),
    spec("set", &[], "set [option[=value]]", "show or change a deck option (deck.opt)", Args::Option),
    crate::extensions::launch_settings_commands::COMMANDS[0],
    spec("pair", &[], "pair", "pair the phone app", Args::None),
    spec("sidebar", &[], "sidebar", "show or hide the sidebar", Args::None),
    spec("help", &["h"], "help", "keys and commands", Args::None),
    spec(
        "q",
        &["quit", "q!", "qa", "qa!", "wq", "wqa", "x", "xa"],
        "q",
        "quit the view; sessions keep running",
        Args::None,
    ),
];

/// The command a word names.
pub fn find(word: &str) -> Option<&'static Spec> {
    COMMANDS.iter().find(|c| c.name == word || c.aliases.contains(&word))
}

/// Whether `name` is a built-in command's name or alias.
pub fn is_builtin(name: &str) -> bool {
    find(name).is_some()
}

/// The programs `:new` starts, by the names it takes.
pub const TOOL_WORDS: &[&str] = &["claude", "codex", "agy", "grok", "opencode"];

/// Split a command line into words: spaces separate them, and '…' or "…"
/// keep spaces in one; a backslash keeps the next character.
pub fn words(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => match chars.next() {
                Some(next) => word.push(next),
                None => return Err("the line ends after a backslash".into()),
            },
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                match chars.next() {
                    Some(next) => word.push(next),
                    None => return Err("the line ends after a backslash".into()),
                }
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return Err("a quote is not closed".into());
    }
    if in_word {
        out.push(word);
    }
    Ok(out)
}

/// Take `--flag VALUE` (or `--flag=VALUE`) out of `words`, before any `--`.
pub fn take_value(words: &mut Vec<String>, flag: &str) -> Result<Option<String>, String> {
    let end = words.iter().position(|w| w == "--").unwrap_or(words.len());
    let prefix = format!("{flag}=");
    let Some(i) = words[..end].iter().position(|w| w == flag || w.starts_with(&prefix)) else { return Ok(None) };
    let word = words.remove(i);
    if let Some(value) = word.strip_prefix(&prefix) {
        return Ok(Some(value.to_string()));
    }
    if i >= words.len() || words[i] == "--" {
        return Err(format!("{flag} needs a value"));
    }
    Ok(Some(words.remove(i)))
}

/// Take a `--switch` out of `words`, before any `--`; whether it was there.
pub fn take_switch(words: &mut Vec<String>, switch: &str) -> bool {
    let end = words.iter().position(|w| w == "--").unwrap_or(words.len());
    match words[..end].iter().position(|w| w == switch) {
        Some(i) => {
            words.remove(i);
            true
        }
        None => false,
    }
}

/// What completion may offer beside the commands themselves.
#[derive(Clone, Debug, Default)]
pub struct Ctx {
    /// The hosts by name, `all` not included.
    pub hosts: Vec<String>,
    /// Commands scripts added, and the launch settings' model and effort
    /// words (`:opus`, `:xhigh`), with the words each completes.
    pub user: Vec<(String, Vec<String>)>,
    /// The deck's options.
    pub options: Vec<String>,
    /// How many sessions the list numbers.
    pub sessions: usize,
}

fn owned(list: &[&str]) -> Vec<String> {
    list.iter().map(|w| w.to_string()).collect()
}

/// The flags that take the next word as their value.
const VALUE_FLAGS: &[&str] = &["--cwd", "--name", "--model", "--scope", "--agents", "--host"];

/// The words in `before` that are not flags or their values.
fn positional(before: &[String]) -> usize {
    let mut count = 0;
    let mut skip = false;
    for word in before {
        if std::mem::take(&mut skip) {
            continue;
        }
        if word.starts_with('-') {
            skip = VALUE_FLAGS.contains(&word.as_str());
        } else {
            count += 1;
        }
    }
    count
}

/// The words that can follow `before` (the words already typed after the
/// command) for a command with `args`, where the word typed so far is
/// `partial`.
fn arg_words(args: Args, before: &[String], partial: &str, ctx: &Ctx) -> Vec<String> {
    let hosts = || {
        let mut out = ctx.hosts.clone();
        out.push("here".into());
        out
    };
    match args {
        Args::None | Args::Text => Vec::new(),
        _ if before.last().is_some_and(|w| VALUE_FLAGS.contains(&w.as_str()) && w != "--host" && w != "--agents") => {
            Vec::new()
        }
        Args::ToolHost | Args::AgentHost if partial.starts_with('-') => owned(&["--cwd", "--name", "--model"]),
        Args::ToolHost if positional(before) == 0 => owned(TOOL_WORDS),
        Args::ToolHost if positional(before) == 1 => hosts(),
        Args::ToolHost => Vec::new(),
        Args::AgentHost if positional(before) == 0 => hosts(),
        Args::AgentHost => Vec::new(),
        Args::Host if partial.starts_with('-') => owned(&["--cwd", "--name", "--scope"]),
        Args::Host if before.contains(&"--".to_string()) => Vec::new(),
        Args::Host if positional(before) == 0 => hosts(),
        Args::Host => Vec::new(),
        Args::Words(list) if before.is_empty() => owned(list),
        Args::Words(_) => Vec::new(),
        Args::Session if before.is_empty() => (1..=ctx.sessions.min(30)).map(|n| n.to_string()).collect(),
        Args::Session => Vec::new(),
        Args::Install if partial.starts_with('-') => owned(&["--agents", "--host", "--dry-run", "--yes"]),
        Args::Install if before.last().is_some_and(|w| w == "--host") => {
            let mut out = hosts();
            out.push("all".into());
            out
        }
        Args::Install if before.last().is_some_and(|w| w == "--agents") => {
            owned(&["claude", "codex", "agy", "claude,codex", "claude,codex,agy"])
        }
        Args::Install => Vec::new(),
        Args::Option if before.is_empty() => ctx.options.clone(),
        Args::Option => Vec::new(),
    }
}

/// Complete the last word of `line`: where it starts (a byte offset) and
/// what it can become, sorted. A line that ends in a space completes a new
/// word.
pub fn complete(line: &str, ctx: &Ctx) -> (usize, Vec<String>) {
    // After the whole separator: a typed no-break or em space is several
    // bytes, and one byte past its start split it and panicked the view.
    let start = line.char_indices().rfind(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8());
    let partial = &line[start..];
    let before: Vec<String> = line[..start].split_whitespace().map(str::to_string).collect();
    let mut options: Vec<String> = match before.split_first() {
        None => {
            let mut all: Vec<String> = COMMANDS.iter().map(|c| c.name.to_string()).collect();
            all.extend(ctx.user.iter().map(|(name, _)| name.clone()));
            all
        }
        Some((command, rest)) => match find(command) {
            Some(spec) => arg_words(spec.args, rest, partial, ctx),
            None => match ctx.user.iter().find(|(name, _)| name == command) {
                Some((_, words)) if rest.is_empty() => words.clone(),
                _ => Vec::new(),
            },
        },
    };
    options.retain(|o| o.starts_with(partial));
    options.sort();
    options.dedup();
    (start, options)
}

/// The longest start every one of `items` shares.
pub fn common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else { return String::new() };
    let mut end = first.len();
    for item in &items[1..] {
        let shared: usize =
            first.chars().zip(item.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a.len_utf8()).sum();
        end = end.min(shared);
    }
    first[..end].to_string()
}

#[cfg(test)]
#[path = "tests/actions.rs"]
mod tests;
