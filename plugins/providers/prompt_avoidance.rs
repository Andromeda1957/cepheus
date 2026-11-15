//! Advisory shell shapes only. This module never grants permission or executes input.
use serde_json::Value;
use std::io::{IsTerminal, Read};
const MAX_COMMAND: usize = 32768;
const MAX_TOKENS: usize = 4096;
const HINTS: &[(&str, &str)] = &[
    ("background", "Avoid bare &: use the harness background option or a separately supervised process."),
    ("computed-removal", "Use a literal absolute removal target instead of command substitution."),
    ("unguarded-removal", "Use a literal absolute removal target or a \"${VAR:?}\"-guarded target."),
    ("normally-set-removal", "Use a literal absolute removal target for normally set variables."),
    ("settings-write", "Avoid shell writes to .claude/settings*.json; use your repository hook configuration procedure."),
    ("self-match", "Avoid pkill -f and kill fed by pgrep -f: they can kill their own shell; use an exact owned PID or process name."),
];
fn heredoc(line: &str) -> Option<(usize, String)> {
    let at = line.find("<<")?;
    let tail = line[at + 2..].trim_start_matches('-').trim_start();
    let tail = tail.trim_start_matches(['\'', '"']);
    let end = tail.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(tail.len());
    let word = &tail[..end];
    (!word.is_empty() && !word.starts_with(|c: char| c.is_ascii_digit())).then(|| (at, word.into()))
}
fn without_heredocs(command: &str, keep_header: bool) -> String {
    let mut delimiter: Option<String> = None;
    let mut result = Vec::new();
    for line in command.lines() {
        if let Some(end) = &delimiter {
            if line.trim() == end {
                delimiter = None;
            }
            continue;
        }
        if let Some((at, end)) = heredoc(line) {
            delimiter = Some(end);
            result.push(if keep_header { line } else { &line[..at] });
        } else {
            result.push(line);
        }
    }
    result.join("\n")
}
fn background(command: &str) -> bool {
    let text = without_heredocs(command, true);
    let chars: Vec<char> = text.chars().collect();
    let (mut quote, mut escaped) = (None, false);
    for (i, &c) in chars.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            continue;
        }
        if c == '&' && (i == 0 || !"&>|<".contains(chars[i - 1])) && chars.get(i + 1).is_none_or(|c| !"&>".contains(*c))
        {
            return true;
        }
    }
    false
}
fn operator(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| ";&|()<>".contains(c))
}
fn tokens(command: &str) -> Option<(Vec<String>, bool)> {
    let source = without_heredocs(command, false);
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    let chars: Vec<char> = source.chars().collect();
    for &c in &chars {
        if escaped {
            if c != '\n' {
                word.push(c);
            }
            escaped = false;
            started = true;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            started = true;
            continue;
        }
        let punct = ";&|()<>".contains(c);
        if (c.is_whitespace() || punct != operator(&word) && !word.is_empty()) && started {
            words.push(std::mem::take(&mut word));
            started = false;
        }
        if c == '\n' {
            words.push(";".into());
        } else if !c.is_whitespace() {
            word.push(c);
            started = true;
        }
        if words.len() > MAX_TOKENS {
            return None;
        }
    }
    if quote.is_some() || escaped {
        return None;
    }
    if started {
        words.push(word);
    }
    (words.len() <= MAX_TOKENS).then(|| (words, background(command)))
}
fn assignment(s: &str) -> bool {
    s.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}
fn command_word(words: &[String]) -> (&str, &[String]) {
    let mut i = 0;
    while i < words.len() {
        let word = words[i].as_str();
        if ["if", "then", "do", "else", "elif", "while", "until", "!", "{", "}", "time"].contains(&word)
            || assignment(word)
        {
            i += 1;
            continue;
        }
        let name = word.rsplit('/').next().unwrap_or(word);
        let valued: Option<&[&str]> = match name {
            "sudo" => Some(&["-u", "-g", "-C", "-D"]),
            "doas" => Some(&["-u"]),
            "env" => Some(&["-u", "-C", "-S"]),
            "nice" => Some(&["-n"]),
            "ionice" => Some(&["-c", "-n", "-p"]),
            "stdbuf" => Some(&["-i", "-o", "-e"]),
            "taskset" => Some(&["-c"]),
            "timeout" => Some(&["-s", "-k", "--signal", "--kill-after"]),
            "xargs" => Some(&["-I", "-i", "-n", "-P", "-d", "-L", "-s", "-a", "-E"]),
            "command" | "builtin" | "exec" | "nohup" | "setsid" => Some(&[]),
            _ => None,
        };
        if let Some(valued) = valued {
            i += 1;
            while i < words.len() && words[i].starts_with('-') {
                let option = words[i].as_str();
                i += 1;
                if valued.contains(&option) && !option.contains('=') {
                    i = (i + 1).min(words.len());
                }
            }
            if i < words.len()
                && ((name == "timeout" && words[i].chars().all(|c| c.is_ascii_digit() || ".smhd".contains(c)))
                    || (name == "taskset" && words[i].chars().all(|c| c.is_ascii_hexdigit() || "x,-".contains(c))))
            {
                i += 1;
            }
            continue;
        }
        return (name, &words[i + 1..]);
    }
    ("", &[])
}
fn settings(s: &str) -> bool {
    [".claude/settings.json", ".claude/settings.local.json"].iter().any(|p| s == *p || s.ends_with(&format!("/{p}")))
}
fn removal(target: &str) -> Option<&'static str> {
    if target.contains("$(") || target.contains('`') {
        return Some("computed-removal");
    }
    let mut rest = target;
    while let Some(at) = rest.find('$') {
        rest = &rest[at + 1..];
        let Some(guard) = rest.strip_prefix('{').and_then(|s| s.split_once(":?")) else {
            return Some("unguarded-removal");
        };
        let name = guard.0;
        if name.is_empty()
            || name.starts_with(|c: char| c.is_ascii_digit())
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Some("unguarded-removal");
        }
        if [
            "HOME",
            "USERPROFILE",
            "HOMEPATH",
            "HOMEDRIVE",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
            "XDG_RUNTIME_DIR",
            "PWD",
            "OLDPWD",
            "TMPDIR",
            "TMP",
            "TEMP",
        ]
        .contains(&name)
        {
            return Some("normally-set-removal");
        }
        let Some((_, tail)) = guard.1.split_once('}') else {
            return Some("unguarded-removal");
        };
        rest = tail;
    }
    None
}
fn full_match(args: &[String]) -> bool {
    for arg in args {
        if arg == "--" || !arg.starts_with('-') {
            return false;
        }
        if arg == "--full"
            || !arg.starts_with("--") && arg[1..].chars().all(|c| c.is_ascii_alphanumeric()) && arg.contains('f')
        {
            return true;
        }
    }
    false
}
fn problems(command: &str, depth: usize) -> Vec<&'static str> {
    let Some((words, background)) = tokens(command) else {
        return Vec::new();
    };
    let mut parsed = Vec::new();
    let (mut argv, mut targets) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < words.len() {
        let word = &words[i];
        if operator(word) {
            if (word.ends_with('>') || word.ends_with(">>") || word == ">|")
                && words.get(i + 1).is_some_and(|w| !operator(w))
            {
                targets.push(words[i + 1].clone());
                i += 2;
                continue;
            }
            if ["<", "<<<"].contains(&word.as_str()) && i + 1 < words.len() {
                i += 2;
                continue;
            }
            if !argv.is_empty() || !targets.is_empty() {
                parsed.push((std::mem::take(&mut argv), std::mem::take(&mut targets)));
            }
        } else if !(word == "$" && words.get(i + 1).is_some_and(|w| w == "(")) {
            argv.push(word.trim_start_matches('`').to_string());
        }
        i += 1;
    }
    if !argv.is_empty() || !targets.is_empty() {
        parsed.push((argv, targets));
    }
    let kills = parsed.iter().any(|(a, _)| ["kill", "pkill", "killall"].contains(&command_word(a).0));
    let mut found = if background { vec!["background"] } else { Vec::new() };
    for (argv, redirects) in parsed {
        let (name, args) = command_word(&argv);
        if (name == "pkill" || name == "pgrep" && kills) && full_match(args) {
            found.push("self-match");
        }
        let operands: Vec<_> = args.iter().filter(|s| !s.starts_with('-')).collect();
        let writes: Vec<&String> = match name {
            "tee" | "truncate" | "touch" => operands,
            "cp" | "mv" | "install" | "ln" | "rsync" => operands.last().copied().into_iter().collect(),
            "sed" | "perl" if args.iter().any(|s| s.starts_with("-i")) => operands.into_iter().skip(1).collect(),
            _ => Vec::new(),
        };
        if redirects.iter().chain(writes).any(|s| settings(s)) {
            found.push("settings-write");
        }
        if ["rm", "rmdir"].contains(&name) {
            let mut done = false;
            for arg in args {
                if !done && arg == "--" {
                    done = true;
                    continue;
                }
                if !done && arg.starts_with('-') {
                    continue;
                }
                if let Some(problem) = removal(arg) {
                    found.push(problem);
                }
            }
        }
        if depth < 3 {
            if ["sh", "bash", "zsh", "dash", "ksh"].contains(&name) {
                for pair in args.windows(2) {
                    if pair[0].starts_with('-')
                        && pair[0][1..].chars().all(|c| c.is_ascii_alphabetic())
                        && pair[0].contains('c')
                    {
                        found.extend(problems(&pair[1], depth + 1));
                        break;
                    }
                }
            } else if name == "eval" && !args.is_empty() {
                found.extend(problems(&args.join(" "), depth + 1));
            }
        }
    }
    found
}
fn violations(command: &str) -> Vec<&'static str> {
    if command.len() > MAX_COMMAND || command.trim().is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for category in problems(command, 0) {
        if !found.contains(&category) {
            found.push(category);
        }
    }
    found
}
pub fn run() -> i32 {
    if std::env::var("CEPHEUS_PROMPT_AVOIDANCE_SKIP").as_deref() == Ok("1") {
        return 0;
    }
    if std::io::stdin().is_terminal() {
        return 0;
    }
    let mut raw = Vec::new();
    if std::io::stdin().take(131073).read_to_end(&mut raw).is_err() || raw.len() > 131072 {
        return 0;
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&raw) else {
        return 0;
    };
    if payload["tool_name"] != "Bash" {
        return 0;
    }
    let input = payload
        .get("tool_input")
        .filter(|v| match v {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64() != Some(0.0),
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
        })
        .or_else(|| payload.get("input"));
    let Some(command) = input.and_then(|v| v["command"].as_str()) else {
        return 0;
    };
    let found = violations(command);
    if found.is_empty() {
        return 0;
    }
    eprintln!("[prompt-avoidance] Rewrite this command to avoid a stalled or broken unattended session:");
    for category in found {
        if let Some((_, hint)) = HINTS.iter().find(|(id, _)| *id == category) {
            eprintln!("  - {hint}");
        }
    }
    2
}
#[cfg(test)]
#[path = "../../src/tests/prompt_avoidance.rs"]
mod tests;
