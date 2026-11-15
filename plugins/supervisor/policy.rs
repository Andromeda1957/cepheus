//! Pure, conservative permission policy. No terminal, process, model, logging or I/O.
//!
//! An empty rule list permits nothing. Core keeps the supervisor off by default
//! and must independently fence enablement, rule generation, provider intent,
//! selected one-time option and prompt identity immediately before one Enter.
//! A decision is never durable approval and must never be replayed on reconnect.
//! Re-evaluate with fresh canonical proofs after audit, immediately before Enter.
//!
//! Only a complete single simple-shell intent is eligible. No expansions,
//! comments, redirections, compound commands, wrappers or inline programs are
//! interpreted. Exact is the default; ArgvGlob anchors a fixed number of tokens,
//! with `*`/`?` only within operand tokens (literal executable, no regex).
//! All forbidden-action and path checks precede rule matching. A resolver must
//! prove every bare relative operand too, including symlinks and missing targets
//! via an existing canonical parent; unavailable/ambiguous proofs return None.
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

const MAX_COMMAND: usize = 4096;
const MAX_TOKEN: usize = 512;
const MAX_TOKENS: usize = 64;
const MAX_RULES: usize = 32;

pub struct Intent<'a> {
    pub session: u64,
    pub prompt_identity: &'a str,
    pub tool: &'a str,
    pub command: &'a str,
    pub cwd: &'a Path,
    pub worktree: &'a Path,
    pub complete: bool,
    pub single_prompt: bool,
    pub allow_once_selected: bool,
    /// Provider adapter attests a resolved single shell command, never screen text alone.
    pub resolved_simple_shell: bool,
}

pub enum Pattern {
    Exact(String),
    ArgvGlob(Vec<String>),
}

pub struct Rule {
    pub id: String,
    pub tool: String,
    pub pattern: Pattern,
}

/// Read-only snapshot resolver supplied by core. Returned paths must be fully
/// canonical absolute paths, with symlinks resolved. A nonexistent tail is only
/// provable through a canonical existing parent; no lexical-only fallback.
pub trait PathResolver {
    fn resolve(&self, path: &Path) -> Option<PathBuf>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decline {
    Incomplete,
    Identity,
    NotSingle,
    NotAllowOnce,
    UnknownShell,
    Forbidden,
    UnresolvedPath,
    OutsideWorktree,
    InvalidRules,
    NoRule,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Decline(Decline),
    AllowOnce { rule_id: String },
}

fn identifier(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
}

fn words(command: &str) -> Option<Vec<String>> {
    if command.is_empty()
        || command.len() > MAX_COMMAND
        || command
            .chars()
            .any(|c| c.is_control() || (c.is_whitespace() && c != ' ') || "$`\\;&|()<>{}[]*?~#".contains(c))
    {
        return None;
    }
    let mut result = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    for c in command.chars() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            started = true;
        } else if c.is_whitespace() {
            if started {
                if word.is_empty() || word.len() > MAX_TOKEN {
                    return None;
                }
                result.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(c);
            started = true;
        }
        if word.len() > MAX_TOKEN || result.len() > MAX_TOKENS {
            return None;
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        if word.is_empty() {
            return None;
        }
        result.push(word);
    }
    (!result.is_empty() && result.len() <= MAX_TOKENS).then_some(result)
}

fn forbidden(argv: &[String]) -> bool {
    const DENIED: &[&str] = &[
        "push",
        "deploy",
        "deployment",
        "publish",
        "release",
        "ship",
        "merge",
        "rm",
        "rmdir",
        "remove",
        "delete",
        "deletion",
        "destroy",
        "drop",
        "truncate",
        "shred",
        "unlink",
        "purge",
        "erase",
        "clean",
        "mv",
        "force",
        "reset",
        "hard",
        "credential",
        "credentials",
        "password",
        "passwd",
        "secret",
        "secrets",
        "token",
        "apikey",
        "api_key",
        "authorization",
        "bearer",
        "login",
        "logout",
        "auth",
        "ssh",
        "aws",
        "gnupg",
        "netrc",
        "npmrc",
        "pypirc",
        "env",
        "pem",
        "key",
        "kill",
        "pkill",
        "killall",
        "reboot",
        "shutdown",
        "poweroff",
        "no-verify",
    ];
    argv.iter().any(|word| {
        let lower = word.to_ascii_lowercase();
        lower.contains("://")
            || lower == "-f"
            || lower == "--no-verify"
            || lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|part| {
                DENIED.contains(&part)
                    || part.starts_with("id_rsa")
                    || part.starts_with("id_ed25519")
                    || part.ends_with("_token")
                    || part.ends_with("_password")
                    || part.ends_with("_secret")
            })
    })
}

fn canonical(path: &Path) -> bool {
    path.is_absolute() && path.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

fn path_inside(value: &str, cwd: &Path, root: &Path, resolver: &impl PathResolver) -> Result<(), Decline> {
    if value.is_empty() || value.contains('=') || value.contains('\\') {
        return Err(Decline::UnresolvedPath);
    }
    let path = Path::new(value);
    let resolved = resolver
        .resolve(&if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) })
        .filter(|p| canonical(p))
        .ok_or(Decline::UnresolvedPath)?;
    if !resolved.starts_with(root) {
        return Err(Decline::OutsideWorktree);
    }
    Ok(())
}

// This is a deliberately finite grammar, not a generic command classifier.
// Unknown programs, operations and options decline even under an exact rule.
// An option's operand role belongs to its program; a global flag table cannot
// distinguish e.g. cargo -p PACKAGE from cp -p FILE. No command is executed here.
#[derive(Clone, Copy)]
enum Grammar {
    Cargo,
    Git,
    Cat,
    Rustc,
    Pytest,
    Unittest,
}
#[derive(Clone, Copy)]
enum Operand {
    Switch,
    Path,
    Data,
}
fn grammar(argv: &[String]) -> Result<(Grammar, usize), Decline> {
    let op = argv.get(1).map(String::as_str);
    match argv[0].as_str() {
        "cargo" if op.is_some_and(|op| ["test", "check", "build", "fmt", "clippy", "metadata"].contains(&op)) => {
            Ok((Grammar::Cargo, 2))
        }
        "git" if op.is_some_and(|op| ["status", "diff", "show", "log", "ls-files", "rev-parse"].contains(&op)) => {
            Ok((Grammar::Git, 2))
        }
        "cat" => Ok((Grammar::Cat, 1)),
        "rustc" => Ok((Grammar::Rustc, 1)),
        "pytest" => Ok((Grammar::Pytest, 1)),
        "python" | "python3" if op == Some("-m") => match argv.get(2).map(String::as_str) {
            Some("pytest") => Ok((Grammar::Pytest, 3)),
            Some("unittest") => {
                Ok((Grammar::Unittest, if argv.get(3).is_some_and(|v| v == "discover") { 4 } else { 3 }))
            }
            _ => Err(Decline::UnknownShell),
        },
        _ => Err(Decline::UnknownShell),
    }
}
fn option(grammar: Grammar, flag: &str) -> Option<Operand> {
    use Operand::*;
    // Each list intentionally supports only uncomplicated options whose operand
    // semantics are known. Forwarding separators, nested configuration, inline
    // code, hooks/external diff options and arbitrary launcher flags decline.
    match grammar {
        Grammar::Cargo => match flag {
            "--offline"
            | "--locked"
            | "-q"
            | "--quiet"
            | "-v"
            | "-vv"
            | "--verbose"
            | "--workspace"
            | "--all-features"
            | "--no-default-features"
            | "--lib"
            | "--tests"
            | "--all-targets"
            | "--no-run"
            | "--check"
            | "--no-deps" => Some(Switch),
            "--manifest-path" | "--target-dir" => Some(Path),
            "-p" | "--package" | "--features" | "--bin" | "--test" | "--example" | "--color" | "--message-format"
            | "--format-version" => Some(Data),
            _ => None,
        },
        Grammar::Git => match flag {
            "--short" | "--stat" | "--name-only" | "--name-status" | "--oneline" | "--cached" | "--staged"
            | "--no-color" | "--numstat" | "--porcelain" | "--abbrev-ref" | "--show-toplevel" | "--verify"
            | "--no-ext-diff" | "--no-textconv" => Some(Switch),
            // No pretty/format option: formats can change field semantics and
            // its option availability differs by operation. Unknown declines.
            _ => None,
        },
        Grammar::Cat => match flag {
            "-n" | "--number" | "-b" | "--number-nonblank" | "-s" | "--squeeze-blank" | "-E" | "--show-ends" | "-T"
            | "--show-tabs" | "-v" | "--show-nonprinting" | "-A" | "--show-all" => Some(Switch),
            _ => None,
        },
        Grammar::Rustc => match flag {
            "-o" | "--out-dir" => Some(Path),
            "--edition" | "--color" | "--crate-name" | "--crate-type" => Some(Data),
            "--test" => Some(Switch),
            _ => None,
        },
        Grammar::Pytest => match flag {
            "-q" | "--quiet" | "-v" | "-vv" | "--verbose" | "-s" | "--collect-only" | "--disable-warnings" => {
                Some(Switch)
            }
            "--rootdir" | "--confcutdir" => Some(Path),
            "-k" | "-m" | "--color" => Some(Data),
            _ => None,
        },
        Grammar::Unittest => match flag {
            "-q" | "--quiet" | "-v" | "--verbose" | "-b" | "--buffer" | "--locals" | "-c" | "--catch" => Some(Switch),
            "-s" | "--start-directory" | "-t" | "--top-level-directory" => Some(Path),
            "-p" | "--pattern" | "-k" => Some(Data),
            _ => None,
        },
    }
}
fn data(value: &str) -> Result<(), Decline> {
    // Slash/dot-prefixed values, response files, assignments, expressions and
    // nested syntax need a richer provider proof; no lexical path exception.
    if value.is_empty()
        || value.starts_with(['-', '.', '@'])
        || !value.chars().all(|c| c.is_ascii_alphanumeric() || "_.,:-".contains(c))
    {
        return Err(Decline::UnknownShell);
    }
    Ok(())
}
fn paths(argv: &[String], cwd: &Path, root: &Path, resolver: &impl PathResolver) -> Result<(), Decline> {
    // Response files can expand before a program parses options (including
    // after -- or in an option's value position). Their contents have no path
    // or intent proof. Conservatively decline this syntax for every grammar,
    // rather than treating @file as a harmless nonexistent literal filename.
    if argv.iter().any(|word| word.starts_with('@')) {
        return Err(Decline::UnknownShell);
    }
    let (grammar, mut index) = grammar(argv)?;
    let mut options_done = false;
    while index < argv.len() {
        let value = &argv[index];
        if !options_done && value == "--" {
            // Cargo forwards everything after -- to an executable with another
            // option grammar (rustc, rustfmt, test harness). It is not modeled.
            if matches!(grammar, Grammar::Cargo) {
                return Err(Decline::UnknownShell);
            }
            options_done = true;
        } else if !options_done && value.starts_with('-') {
            let (flag, inline) = value.split_once('=').map_or((value.as_str(), None), |(f, v)| (f, Some(v)));
            match option(grammar, flag).ok_or(Decline::UnknownShell)? {
                Operand::Switch if inline.is_none() => {}
                Operand::Switch => return Err(Decline::UnknownShell),
                kind => {
                    let operand = match inline {
                        Some(value) => value,
                        None => {
                            index += 1;
                            argv.get(index).ok_or(Decline::UnknownShell)?.as_str()
                        }
                    };
                    if operand.starts_with('-') {
                        return Err(Decline::UnknownShell);
                    }
                    match kind {
                        Operand::Path => path_inside(operand, cwd, root, resolver)?,
                        Operand::Data => data(operand)?,
                        Operand::Switch => unreachable!(),
                    }
                }
            }
        } else {
            // Every bare operand (including option-looking names after --) must
            // have an actual canonical path proof inside the task's worktree.
            path_inside(value, cwd, root, resolver)?;
        }
        index += 1;
    }
    Ok(())
}

fn glob(pattern: &str, value: &str) -> bool {
    let (p, v): (Vec<_>, Vec<_>) = (pattern.chars().collect(), value.chars().collect());
    let (mut i, mut j, mut star, mut back) = (0, 0, None, 0);
    while j < v.len() {
        if i < p.len() && (p[i] == '?' || p[i] == v[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            i += 1;
            back = j;
        } else if let Some(s) = star {
            back += 1;
            j = back;
            i = s + 1;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == '*' {
        i += 1;
    }
    i == p.len()
}

/// Validate bounded rule syntax and identities for the editor. This grants no
/// permission: evaluate must still check a fresh complete intent and path proofs.
pub fn validate_rules(rules: &[Rule]) -> bool {
    if rules.len() > MAX_RULES {
        return false;
    }
    let mut ids = BTreeSet::new();
    rules.iter().all(|r| {
        identifier(&r.id, 64)
            && identifier(&r.tool, 100)
            && ids.insert(r.id.as_str())
            && match &r.pattern {
                Pattern::Exact(command) => words(command).is_some(),
                Pattern::ArgvGlob(tokens) => {
                    !tokens.is_empty()
                        && tokens.len() <= MAX_TOKENS
                        && tokens.iter().all(|t| {
                            !t.is_empty()
                                && t.len() <= MAX_TOKEN
                                && t.chars().all(|c| !c.is_control() && !"$`\\;&|()<>{}[]~#".contains(c))
                                && t.chars().filter(|c| *c == '*' || *c == '?').count() <= 8
                        })
                        && !tokens[0].contains(['*', '?'])
                }
            }
    })
}

pub fn evaluate(intent: &Intent<'_>, rules: &[Rule], resolver: &impl PathResolver) -> Decision {
    let result = (|| {
        if !intent.complete {
            return Err(Decline::Incomplete);
        }
        if intent.session == 0 || !identifier(intent.prompt_identity, 256) || !identifier(intent.tool, 100) {
            return Err(Decline::Identity);
        }
        if !intent.single_prompt {
            return Err(Decline::NotSingle);
        }
        if !intent.allow_once_selected {
            return Err(Decline::NotAllowOnce);
        }
        if !intent.resolved_simple_shell {
            return Err(Decline::UnknownShell);
        }
        let argv = words(intent.command).ok_or(Decline::UnknownShell)?;
        let mut tool_words = String::new();
        for c in intent.tool.chars() {
            if c.is_ascii_uppercase() {
                tool_words.push('_');
            }
            tool_words.push(c.to_ascii_lowercase());
        }
        if forbidden(&argv) || forbidden(&[tool_words]) {
            return Err(Decline::Forbidden);
        }
        let root = resolver.resolve(intent.worktree).filter(|p| canonical(p)).ok_or(Decline::UnresolvedPath)?;
        let cwd = resolver.resolve(intent.cwd).filter(|p| canonical(p)).ok_or(Decline::UnresolvedPath)?;
        if !cwd.starts_with(&root) {
            return Err(Decline::OutsideWorktree);
        }
        paths(&argv, &cwd, &root, resolver)?;
        if !validate_rules(rules) {
            return Err(Decline::InvalidRules);
        }
        rules
            .iter()
            .find(|rule| {
                rule.tool == intent.tool
                    && match &rule.pattern {
                        Pattern::Exact(command) => command == intent.command,
                        Pattern::ArgvGlob(tokens) => {
                            tokens.len() == argv.len()
                                && tokens.iter().zip(&argv).all(|(pattern, word)| glob(pattern, word))
                        }
                    }
            })
            .map(|rule| rule.id.clone())
            .ok_or(Decline::NoRule)
    })();
    match result {
        Ok(rule_id) => Decision::AllowOnce { rule_id },
        Err(reason) => Decision::Decline(reason),
    }
}

#[cfg(test)]
#[path = "tests/policy.rs"]
mod tests;
