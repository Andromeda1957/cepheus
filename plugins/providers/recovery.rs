//! Codex client failures and editable input. Provider chrome is never a core policy.

use crate::status::live_tail;

/// An unrecoverable shared-server connection, rather than a transient retry or
/// text an agent quoted. Codex keeps the composer editable but changes its live
/// shortcut footer to `^c quit` after automatic recovery has failed.
pub fn codex_reconnect_failed(lines: &[String]) -> bool {
    let tail = live_tail(lines, 35);
    if tail.last().is_none_or(|l| l.trim() != "^c quit") {
        return false;
    }
    let Some(composer) = tail.iter().rposition(|l| l.starts_with('›')) else { return false };
    let before = &tail[..composer];
    before.iter().any(|l| l.starts_with("• Reconnect failed — check the endpoint, then relaunch ("))
        || (before.iter().any(|l| l.starts_with("■ Automatic reconnect could not restore this session."))
            && before.iter().any(|l| l.starts_with("■ Automatic reconnect timed out while resuming the conversation.")))
}

/// The complete visible composer, empty for its placeholder. A collapsed paste,
/// hidden/truncated composer, or cursor outside the composer is not recoverable
/// from cells. Callers must keep the original client when this returns `None`.
pub fn codex_draft(lines: &[String], cursor: Option<(usize, usize)>) -> Option<String> {
    let (x, y) = cursor?;
    let top = lines.get(..=y)?.iter().rposition(|l| l.starts_with('›'))?;
    let footer = lines[top + 1..]
        .iter()
        .position(|l| l.starts_with("  ") && l.contains(" · ") && !l.trim_start().starts_with('•'))?
        + top
        + 1;
    if y >= footer {
        return None;
    }
    let mut body: Vec<&str> = lines[top..footer].iter().map(String::as_str).collect();
    while body.last().is_some_and(|l| l.trim().is_empty()) {
        body.pop();
    }
    let first = body.first()?.strip_prefix("› ").or_else(|| body.first()?.strip_prefix('›'))?;
    if top == y && x <= 2 && body.len() == 1 {
        // A placeholder is drawn after the cursor; actual text is before it.
        return (first.is_empty() || first == "Ask Codex to do anything").then(String::new);
    }
    let rest = body.iter().skip(1).map(|l| l.strip_prefix("  ").unwrap_or(l));
    let draft = std::iter::once(first).chain(rest).collect::<Vec<_>>().join("\n");
    if draft.contains("[Pasted Content") || draft.contains("[Pasted content") || draft.contains("[Image #") {
        return None;
    }
    Some(draft)
}

/// Whether discarding a known composer would discard editable input. Collapsed
/// paste markers are enough to establish nonempty without reconstructing them.
pub fn codex_draft_nonempty(lines: &[String], cursor: Option<(usize, usize)>) -> Option<bool> {
    if let Some(draft) = codex_draft(lines, cursor) {
        return Some(!draft.is_empty());
    }
    let (_, y) = cursor?;
    let top = lines.get(..=y)?.iter().rposition(|l| l.starts_with('›'))?;
    let footer = lines[top + 1..].iter().position(|l| l.starts_with("  ") && l.contains(" · "))? + top + 1;
    (y < footer
        && lines[top..footer]
            .iter()
            .any(|l| l.contains("[Pasted Content") || l.contains("[Pasted content") || l.contains("[Image #")))
    .then_some(true)
}

/// An image paste followed by separately typed text is richer than one text
/// paste. Only a photo-only marker with its original file can be replayed exactly.
pub fn codex_image_draft_recoverable(lines: &[String], cursor: Option<(usize, usize)>, original: &str) -> bool {
    if !lines.iter().any(|line| line.contains("[Image #")) {
        return true;
    }
    let Some((_, y)) = cursor else { return false };
    let Some(top) = lines.get(..=y).and_then(|lines| lines.iter().rposition(|line| line.starts_with('›'))) else {
        return false;
    };
    let Some(footer) = lines[top + 1..].iter().position(|line| line.starts_with("  ") && line.contains(" · ")) else {
        return false;
    };
    let footer = top + 1 + footer;
    if y >= footer {
        return false;
    }
    let visible =
        lines[top..footer]
            .iter()
            .enumerate()
            .map(|(i, line)| {
                if i == 0 {
                    line.strip_prefix("› ").unwrap_or(line)
                } else {
                    line.strip_prefix("  ").unwrap_or(line)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    !visible.contains("[Image #")
        || (visible.trim() == "[Image #1]"
            && std::path::Path::new(original).is_absolute()
            && std::path::Path::new(original).is_file())
}

/// A lossless fallback for an inherited client without captured input. Codex
/// wraps composer text itself, so multiline/collapsed input needs its original
/// captured payload; keeping that client is safer than guessing newlines.
pub fn codex_recovery_draft<T: alacritty_terminal::event::EventListener>(
    term: &alacritty_terminal::term::Term<T>,
) -> Option<String> {
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::cell::Flags;
    use unicode_width::UnicodeWidthStr;
    let lines: Vec<String> = (0..term.screen_lines())
        .map(|y| {
            let mut line = String::new();
            for x in 0..term.columns() {
                let cell = &term.grid()[Line(y as i32)][Column(x)];
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    continue;
                }
                line.push(if matches!(cell.c, '\0' | '\t') { ' ' } else { cell.c });
                if let Some(extra) = cell.zerowidth() {
                    line.extend(extra);
                }
            }
            line.trim_end().to_string()
        })
        .collect();
    let draft = codex_draft(&lines, crate::screen::text_cursor(term))?;
    let (cursor_x, _) = crate::screen::text_cursor(term)?;
    // Trimmed terminal padding cannot distinguish trailing draft spaces. A
    // cursor elsewhere also makes that boundary ambiguous; keep the client.
    (draft.lines().count() <= 1 && draft.width() < term.columns().saturating_sub(4) && cursor_x == 2 + draft.width())
        .then_some(draft)
}

/// Read the owner's message once from the rollout's selected representation.
/// Keeping full text avoids the phone presentation's 24,000-character clipping.
pub enum UserReceipt {
    Text(String),
    /// An exact photo-only path payload, or an unsupported image transformation.
    Images(Option<String>),
    Attached {
        paths: Vec<String>,
        text: String,
    },
}

pub fn logged_user(record: &serde_json::Value, items: bool) -> Option<UserReceipt> {
    if record.get("type")?.as_str()? != "event_msg" {
        return None;
    }
    let payload = record.get("payload")?;
    if !items {
        return (payload.get("type")?.as_str()? == "user_message")
            .then(|| payload.get("message")?.as_str().map(|s| UserReceipt::Text(s.to_string())))?;
    }
    if payload.get("type")?.as_str()? != "item_completed" {
        return None;
    }
    let item = payload.get("item")?;
    if !matches!(item.get("type")?.as_str()?, "UserMessage" | "userMessage") {
        return None;
    }
    let content = item.get("content")?.as_array()?;
    let text = content.iter().filter_map(|part| part.get("text")?.as_str()).collect::<Vec<_>>().join("\n");
    if !content
        .iter()
        .any(|part| matches!(part.get("type").and_then(|t| t.as_str()), Some("local_image" | "localImage" | "image")))
    {
        return Some(UserReceipt::Text(text));
    }
    let paths: Option<Vec<_>> = content
        .iter()
        .filter(|part| part.get("type").and_then(|t| t.as_str()) != Some("text"))
        .map(|part| {
            matches!(part.get("type")?.as_str()?, "local_image" | "localImage")
                .then(|| part.get("path")?.as_str().filter(|path| std::path::Path::new(path).is_absolute()))?
        })
        .collect();
    if let Some(paths) = paths {
        let mut remaining = text.trim();
        if !remaining.is_empty() {
            for i in 1..=paths.len() {
                let marker = format!("[Image #{i}]");
                let Some(rest) = remaining.strip_prefix(marker.as_str()) else {
                    // Native image provenance still acknowledges owned uploads
                    // when its caption representation changes. Unowned queued
                    // image input keeps its stricter exact-text matching.
                    return Some(UserReceipt::Attached { paths: paths.into_iter().map(str::to_owned).collect(), text });
                };
                remaining = rest.trim_start();
            }
        }
        return Some(if remaining.is_empty() {
            UserReceipt::Images(Some(paths.join("\n")))
        } else {
            UserReceipt::Attached { paths: paths.into_iter().map(str::to_owned).collect(), text: remaining.into() }
        });
    }
    Some(UserReceipt::Images(None))
}

/// Codex accepts slash-prefixed paths as prompts when the first token contains
/// another slash; a leading space also keeps a slash literal.
pub fn codex_slash_command(text: &str) -> bool {
    text.strip_prefix('/').and_then(|s| s.split_whitespace().next()).is_some_and(|name| !name.contains('/'))
}

pub fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
