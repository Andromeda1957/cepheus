//! Screen adapters for the bundled agents.
use crate::{protocol::Tool, util};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScreenSignals {
    pub working: bool,
    pub prompt: Option<&'static str>,
    /// Codex shows questions its agent asked while its turn runs on
    /// (`queued_question_line`). They wait for the owner, but typed input and
    /// Enter do not answer them, so unlike `prompt` they do not block typing.
    pub queued_question: bool,
}

/// Lower-case fragments of approval and question prompts, with a short reason.
pub(crate) const PROMPTS: &[(&str, &str)] = &[
    ("do you want to proceed?", "permission"),
    ("do you want to make this edit", "edit approval"),
    ("do you want to create", "file approval"),
    ("do you want to allow", "permission"),
    ("would you like to proceed?", "plan approval"),
    ("would you like to run the following command?", "command approval"),
    ("would you like to make the following edits?", "edit approval"),
    ("enter to select", "question"),
    ("press enter to confirm", "confirmation"),
    // opencode's permission dialog (opencode 2.1.4).
    ("△ permission required", "permission"),
];

/// Lower-case hints a program shows only while its turn runs: Claude Code's
/// and Codex's, and opencode's (`esc interrupt`, then `esc again to
/// interrupt` after one `Esc`; opencode 2.1.4).
const WORKING: &[&str] = &["esc to interrupt", "esc interrupt", "esc again to interrupt"];

/// The last `rows` lines of a screen, ending at its last line with text. A
/// fresh session draws at the top of a taller terminal and leaves the rows
/// below blank: counted from the bottom, a 50-row screen hid a 3-line turn's
/// `esc to interrupt`, so it never finished and the phone got no alert.
pub fn live_tail(lines: &[String], rows: usize) -> &[String] {
    let end = lines.iter().rposition(|l| !l.trim().is_empty()).map_or(0, |i| i + 1);
    &lines[end.saturating_sub(rows)..end]
}

/// Codex's summary of the questions its agent asked with `request_user_input_async` and the owner has not
/// opened: ` ? 1 question`, or ` ? 2 questions · 15s` while it counts down, over its key's ` shift+← to answer`
/// (Codex 0.166, `bottom_pane/questions.rs`). It sits under "Queued follow-up inputs" above the input box while
/// the turn runs, so the working line used to hide it: a question waited 12 minutes behind a spinning row.
fn queued_question_line(line: &str) -> bool {
    let Some(rest) = line.trim_end().strip_prefix("  ? ") else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    let rest = &rest[digits..];
    digits > 0
        && rest
            .strip_prefix(" questions")
            .or_else(|| rest.strip_prefix(" question"))
            .is_some_and(|after| after.is_empty() || after.starts_with(" · "))
}

/// Inspect the bottom of a live screen for activity and prompts.
pub fn screen_signals(lines: &[String]) -> ScreenSignals {
    let tail: Vec<String> = live_tail(lines, 30).iter().map(|l| l.to_lowercase()).collect();
    let working = tail.iter().any(|l| WORKING.iter().any(|hint| l.contains(hint)));
    let queued_question = tail.iter().any(|l| queued_question_line(l));
    // Codex's expanded asynchronous question editor receives keys even while
    // its agent works; ordinary messages must wait until that editor closes.
    let editor = tail.iter().any(|l| l.contains("submit") && l.contains("skip"))
        && tail.iter().any(|l| l.contains("main prompt") || l.contains("prev question") || l.contains("next question"));
    if editor {
        return ScreenSignals { working, prompt: Some("async question"), queued_question };
    }
    if working {
        return ScreenSignals { working, prompt: None, queued_question };
    }
    for (pattern, reason) in PROMPTS {
        if tail.iter().any(|l| l.contains(pattern)) {
            return ScreenSignals { working, prompt: Some(reason), queued_question };
        }
    }
    let strip = |l: &str| l.trim_start_matches(|c: char| c.is_whitespace() || c == '│' || c == '┃').to_string();
    let selected_first = tail.iter().any(|l| strip(l).starts_with("❯ 1."));
    let has_second = tail.iter().any(|l| {
        let s = strip(l);
        s.starts_with("2.") || s.starts_with("❯ 2.")
    });
    if selected_first && has_second {
        return ScreenSignals { working, prompt: Some("choice"), queued_question };
    }
    ScreenSignals { working, prompt: None, queued_question }
}

/// The error a Codex turn stopped on, when Codex now waits with an empty
/// input box: the transcript ends with a line containing one of `patterns`
/// (only blank lines and at most a few status lines below it) and the cursor
/// sits in the `›` input box with nothing typed before it. `cursor` is
/// (column, row) on `lines`. Codex draws its placeholder after the cursor,
/// so only text before the cursor counts as typed.
pub fn stopped_on_error<'a>(
    lines: &[String],
    cursor: Option<(usize, usize)>,
    patterns: &'a [String],
) -> Option<&'a str> {
    /// Status lines Codex may draw between the last message and its input box.
    const MAX_LINES_BELOW: usize = 3;
    let (x, y) = cursor?;
    let typed: String = lines.get(y)?.chars().take(x).collect();
    if !typed.trim_start().strip_prefix('›')?.trim().is_empty() {
        return None;
    }
    let mut below = 0;
    for line in lines[..y].iter().rev() {
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        // A message sent after the error: it was already answered.
        if text.starts_with('›') {
            return None;
        }
        // Codex draws its own error at the left edge behind `■`. An agent's
        // message starts with `•` and its lines are indented under it, so
        // error text the agent quotes from a log or a page never counts.
        if let Some(error) = line.strip_prefix('■').map(str::to_lowercase) {
            if let Some(p) = patterns.iter().find(|p| !p.trim().is_empty() && error.contains(&p.to_lowercase())) {
                return Some(p.as_str());
            }
        }
        below += 1;
        if below > MAX_LINES_BELOW {
            return None;
        }
    }
    None
}

/// Whether a Codex screen shows a new chat (`Some(true)`) or one with
/// messages (`Some(false)`), read upward from the `›` input box at `cursor`
/// (column, row). A new chat is the welcome card Codex draws when it starts
/// and again after `/clear` or `/new`: ">_ OpenAI Codex (v…)" over lowercase
/// "model:" and "directory:" lines (Codex 0.164), with no message after it.
/// The `/status` card says "Directory:" and is not one. `None`: neither is
/// on screen, or there is no input box.
pub fn codex_new_chat(lines: &[String], cursor: Option<(usize, usize)>) -> Option<bool> {
    /// Lines of a welcome card below its title.
    const CARD_LINES: usize = 4;
    let (_, y) = cursor?;
    if !lines.get(y)?.trim_start().starts_with('›') {
        return None;
    }
    let text = |line: &str| line.trim_matches(|c: char| c.is_whitespace() || "│╭╮╰╯─".contains(c)).to_string();
    for (i, line) in lines[..y].iter().enumerate().rev() {
        let line = text(line);
        // A user's message, or the agent's.
        if line.starts_with('›') || line.starts_with('•') {
            return Some(false);
        }
        // The boxed card names its directory; the start screen of a tall
        // terminal (a resumed Codex 0.164, 2026-10-26) names its permissions.
        if line.contains("OpenAI Codex (v")
            && lines[i + 1..y]
                .iter()
                .take(CARD_LINES)
                .any(|l| text(l).starts_with("directory:") || text(l).starts_with("permissions:"))
        {
            return Some(true);
        }
    }
    None
}

/// Claude Code's input box: the row of its `❯ ` line under a full-width `─`
/// rule, and the row of the rule that closes it when that is on screen
/// (Claude Code 2.1.317). The slash menu above the box and transcript prompts
/// have no rule over them.
fn claude_input_box(lines: &[String]) -> Option<(usize, Option<usize>)> {
    let rule = |l: &str| {
        let l = l.trim();
        l.chars().count() >= 10 && l.chars().all(|c| c == '─')
    };
    let top = (1..lines.len()).rev().find(|&y| rule(&lines[y - 1]) && lines[y].starts_with('❯'))?;
    Some((top, lines[top + 1..].iter().position(|l| rule(l)).map(|i| top + 1 + i)))
}

/// The text in Claude Code's input box (`claude_input_box`): its `❯ ` line
/// and the continuation lines down to the closing rule, joined with newlines
/// and trimmed; "" when the box is empty.
pub fn claude_composer(lines: &[String]) -> Option<String> {
    let (top, end) = claude_input_box(lines)?;
    let first = lines[top].strip_prefix('❯')?.trim();
    let rest = lines[top + 1..end.unwrap_or(lines.len())].iter().map(|l| l.trim());
    let text: Vec<&str> = std::iter::once(first).chain(rest).collect();
    Some(text.join("\n").trim().to_string())
}

/// Whether the program shows its own notice that a newer version is out, so the service can update it then
/// rather than each launch asking for one. Claude Code writes "Update available! Run: <command>" in the footer
/// under its input box (2.1.323); Codex starts with a card boxed at the left edge, "│ ✨ Update available!
/// 0.167.1 -> 0.167.2" (0.167). A conversation quoting either text is above the input box, or indented under
/// its `•`.
pub fn update_notice(tool: Tool, lines: &[String]) -> bool {
    match tool {
        Tool::Claude => claude_input_box(lines)
            .and_then(|(_, end)| end)
            .is_some_and(|end| lines[end + 1..].iter().any(|l| l.contains("Update available! Run:"))),
        Tool::Codex => lines.iter().any(|l| {
            l.strip_prefix('│')
                .and_then(|l| l.trim_start().strip_prefix('✨'))
                .is_some_and(|l| l.trim_start().starts_with("Update available!"))
        }),
        _ => false,
    }
}

/// The text in Codex's composer, when the cursor is in it.
pub fn codex_composer(lines: &[String], cursor: Option<(usize, usize)>) -> Option<String> {
    let (_, y) = cursor?;
    let text = lines.get(y)?.trim_start().strip_prefix('›')?;
    Some(text.trim().to_string())
}

/// The items of the status line Codex draws below its `›` composer at
/// `cursor` (column, row), split where it separates them with " · ". Among
/// them is the name of the thread the program is in ("GPT-6.1-Sol xhigh ·
/// Merge everything to master · weekly 64% left", Codex 0.164), the one
/// place a Codex attached to the shared server shows which thread it is in.
/// While Codex hides its cursor during work, recognize the bottom composer
/// and its shortcut hint instead. Menus and transcript text are not footers.
pub fn codex_footer(lines: &[String], cursor: Option<(usize, usize)>) -> Vec<String> {
    let y = match cursor {
        Some((_, y)) if lines.get(y).is_some_and(|l| l.trim_start().starts_with('›')) => y,
        Some(_) => return Vec::new(),
        None => {
            let tail = live_tail(lines, 8);
            let Some(row) = tail.iter().rposition(|l| l.trim_start().starts_with('›')) else {
                return Vec::new();
            };
            let below = &tail[row + 1..];
            // The shortcut hint anchors this to Codex's live chrome. A menu
            // or an answer mentioning a thread name must not select a log.
            if !below.last().is_some_and(|l| l.contains("? for shortcuts"))
                || !below
                    .iter()
                    .find(|l| !l.trim().is_empty())
                    .is_some_and(|l| l.contains(" · ") && !l.trim_start().starts_with('•'))
            {
                return Vec::new();
            }
            let end = lines.iter().rposition(|l| !l.trim().is_empty()).unwrap() + 1;
            end - tail.len() + row
        }
    };
    lines[y + 1..]
        .iter()
        .flat_map(|line| line.split(" · "))
        .map(util::one_line)
        .filter(|item| !item.is_empty())
        .collect()
}

/// The newest unfinished prose block in a native Claude/Codex screen.
/// A user marker and the composer's upper boundary anchor this to conversation
/// output; tool results and terminal chrome never become assistant messages.
pub fn live_reply(tool: Tool, lines: &[String]) -> Option<(Option<String>, String)> {
    let (prompt, bullet) = match tool {
        Tool::Claude => ("❯", "● "),
        Tool::Codex => ("›", "• "),
        _ => return None,
    };
    let composer = lines.iter().rposition(|l| l.starts_with(prompt))?;
    let end = if tool == Tool::Claude {
        let rule = lines.get(composer.checked_sub(1)?)?.trim_start();
        if rule.chars().take_while(|&c| c == '─').count() < 10 {
            return None;
        }
        composer - 1
    } else {
        if !lines[composer + 1..].iter().any(|l| l.contains("? for shortcuts")) {
            return None;
        }
        composer
    };
    // Codex draws its spinner with the same bullet as an assistant message.
    // It and queued-input controls belong to the footer, not the conversation.
    let end = if tool == Tool::Codex {
        lines[..end].iter().rposition(|l| l.starts_with("• ") && l.contains("esc to interrupt")).unwrap_or(end)
    } else {
        end
    };
    let body = &lines[..end];
    let user = body.iter().rposition(|l| l.starts_with(prompt));
    let start = body.iter().rposition(|l| l.starts_with(bullet) || (tool == Tool::Claude && l.starts_with("⏺ ")))?;
    if user.is_some_and(|user| start <= user) {
        return None;
    }
    let head = body[start].strip_prefix(bullet).or_else(|| body[start].strip_prefix("⏺ "))?.trim();
    let lower = head.to_lowercase();
    // Only native status/tool forms, never ordinary prose verbs such as
    // "Added", "Read" or "Working with". Result branches below identify tools.
    let tool_header = tool == Tool::Codex
        && (matches!(head, "Explored" | "Updated plan" | "Updated Plan")
            || lower.starts_with("waiting for background terminal ("));
    if tool_header
        || head.contains("(MCP)")
        || head.contains("esc to interrupt")
        || head.contains("shift+←")
        || head.ends_with('…')
    {
        return None;
    }
    // Claude tools use Name(arguments), including namespaced MCP calls.
    if let Some((name, _)) = head.split_once('(') {
        if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || "_:-".contains(c)) {
            return None;
        }
    }
    let mut text = head.to_string();
    for line in &body[start + 1..] {
        let trimmed = line.trim();
        if trimmed.starts_with(['⎿', '└', '├']) {
            return None;
        }
        if trimmed.is_empty() {
            text.push('\n');
        } else if let Some(rest) = line.strip_prefix("  ") {
            text.push('\n');
            text.push_str(rest);
        } else {
            break;
        }
    }
    let user_text = user.map(|user| {
        let mut text = body[user].strip_prefix(prompt).unwrap_or_default().trim().to_string();
        for line in &body[user + 1..start] {
            if let Some(rest) = line.strip_prefix("  ") {
                text.push_str(rest);
            } else if !line.trim().is_empty() {
                break;
            }
        }
        text
    });
    let text = text.trim().to_string();
    (!text.is_empty()).then_some((user_text, text))
}

/// Positive proof of an empty, active provider composer for service-generated
/// input. Text before the cursor alone misses a draft after Home. Unknown
/// chrome is held; manual input and broadcast keep their existing semantics.
pub(crate) fn empty_automatic_composer<T: alacritty_terminal::event::EventListener>(
    tool: crate::protocol::Tool,
    term: &alacritty_terminal::term::Term<T>,
) -> bool {
    use crate::{protocol::Tool, screen};
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::term::TermMode;

    if !term.mode().contains(TermMode::BRACKETED_PASTE | TermMode::SHOW_CURSOR) {
        return false;
    }
    let Some((_, y)) = screen::text_cursor(term) else { return false };
    let lines = screen::screen_text(term);
    let rule = |line: &str| {
        let line = line.trim();
        line.chars().count() >= 10 && line.chars().all(|c| c == '─')
    };
    match tool {
        Tool::Claude => {
            // A complete bounded box, including all continuation/attachment rows.
            y > 0
                && lines[y].starts_with('❯')
                && rule(&lines[y - 1])
                && lines[y + 1..].iter().any(|line| rule(line))
                && claude_composer(&lines).is_some_and(|text| text.is_empty())
                && !live_tail(&lines, 2).iter().any(|line| line.contains("Pasting…") || line.contains("Pasting..."))
        }
        Tool::Codex => {
            // Codex 0.166 renders a DIM placeholder only for an empty textarea;
            // typed text uses the default style. Disabled/interactive inputs dim
            // the arrow too. Do not whitelist placeholder words or follow the
            // viewport: inspect live styled cells, including right of cursor.
            let row = &term.grid()[Line(y as i32)];
            let Some(marker) = (0..term.columns()).find(|&x| !row[Column(x)].c.is_whitespace()) else { return false };
            if row[Column(marker)].c != '›'
                || row[Column(marker)].flags.intersects(Flags::DIM | Flags::HIDDEN | Flags::INVERSE)
                || term.grid().cursor.point.column.0 != marker + 2
                || y == 0
                || !lines[y - 1].trim().is_empty()
                || (y >= 2 && lines[y - 2].trim_start().starts_with("[Image"))
                || lines.get(y + 1).is_none_or(|line| !line.trim().is_empty())
            {
                return false;
            }
            let mut placeholder = false;
            for x in marker + 1..term.columns() {
                let cell = &row[Column(x)];
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    continue;
                }
                if cell.c.is_whitespace() || cell.c == '\0' {
                    continue;
                }
                if matches!(cell.c, '↑' | '↓')
                    || !cell.flags.contains(Flags::DIM)
                    || cell.flags.intersects(Flags::HIDDEN | Flags::INVERSE)
                {
                    return false;
                }
                placeholder = true;
            }
            placeholder
        }
        _ => false,
    }
}

/// An explicit owner Enter may clear the private edit latch only from a known
/// active composer containing actual text. Empty/image-only, slash, shell-mode,
/// disabled and unrecognized inputs keep their uncertainty. This is not an
/// automatic-send predicate: complete empty-composer proof is separate.
pub(crate) fn owner_composer_submission<T: alacritty_terminal::event::EventListener>(
    tool: crate::protocol::Tool,
    term: &alacritty_terminal::term::Term<T>,
    inline: &str,
) -> bool {
    use crate::{protocol::Tool, screen};
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::cell::Flags;

    let Some((_, y)) = screen::text_cursor(term) else { return false };
    let lines = screen::screen_text(term);
    let marker = match tool {
        Tool::Claude => '❯',
        Tool::Codex => '›',
        _ => return false,
    };
    let Some(top) = (0..=y).rev().find(|&row| lines[row].trim_start().starts_with(marker)) else { return false };
    let row = &term.grid()[Line(top as i32)];
    let Some(x) = (0..term.columns()).find(|&x| row[Column(x)].c == marker) else { return false };
    if row[Column(x)].flags.intersects(Flags::DIM | Flags::HIDDEN | Flags::INVERSE) {
        return false;
    }
    let text = std::iter::once(lines[top].trim_start().strip_prefix(marker).unwrap())
        .chain(lines[top + 1..=y].iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim_start().starts_with('/')
        || text.trim_start().starts_with('!')
        || inline.trim_start().starts_with('/')
        || inline.trim_start().starts_with('!')
    {
        return false;
    }
    if tool == Tool::Claude && claude_composer(&lines).is_none() {
        return false;
    }
    // DIM placeholder glyphs never prove an owner text submission. Attachments
    // can remain invisible in a clipped Codex composer; pixels alone cannot
    // prove that this draft was submitted.
    !inline.trim().is_empty()
        || (top..=y).any(|r| {
            let row = &term.grid()[Line(r as i32)];
            (if r == top { x + 1 } else { 0 }..term.columns()).any(|col| {
                let cell = &row[Column(col)];
                !cell.c.is_whitespace()
                    && cell.c != '\0'
                    && !cell.flags.intersects(
                        Flags::DIM | Flags::HIDDEN | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER,
                    )
            })
        })
}
