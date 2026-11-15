//! A question or approval a session waits on, read from its screen, and the
//! keys that answer it. The phone shows it as a card; the service checks it
//! is still the same prompt in the step that presses the keys (`Answer`).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::keys;
use crate::status;

/// How far up from the screen's last line with text a prompt is looked for.
const PROMPT_ROWS: usize = 40;
/// Lines of question text kept above the options.
const QUESTION_LINES: usize = 14;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PromptOption {
    pub label: String,
    /// Indented lines under the option, such as a question option's description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Whether its check box is ticked, for an option of a question that
    /// takes several choices; `None` for an option without one. The box is
    /// not part of `label`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Prompt {
    /// What the program asks for, as the deck names it ("permission",
    /// "command approval", "question", "choice", …).
    pub kind: String,
    pub question: String,
    pub options: Vec<PromptOption>,
    /// The option the cursor is on.
    pub selected: usize,
    /// An option that takes typed text ("Type something."), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_text: Option<usize>,
    /// Options carry check boxes: several may be chosen, and `submit` sends
    /// them (`keys_several`).
    #[serde(default)]
    pub multi: bool,
    /// The unnumbered row under a several-choice question's options that
    /// sends the choices: Claude Code's "Submit", or "Next" while another
    /// question follows (Claude Code 2.1.314).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submit: Option<String>,
    /// How many options come before `submit`; the rest, such as Claude's
    /// "Chat about this" under its rule, come after it.
    #[serde(skip)]
    pub submit_at: usize,
    /// The cursor is on `submit` rather than on option `selected`.
    #[serde(skip)]
    pub on_submit: bool,
}

const MARKERS: [char; 4] = ['❯', '›', '>', '→'];

/// A screen line without the box drawn around a dialog.
fn unboxed(line: &str) -> &str {
    line.trim_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '┃' | '║'))
}

/// A numbered option row: whether the cursor is on it, its number and label.
fn option_row(line: &str) -> Option<(bool, usize, String)> {
    let mut rest = unboxed(line);
    let mut cursor = false;
    if let Some(c) = rest.chars().next().filter(|c| MARKERS.contains(c)) {
        cursor = true;
        rest = rest[c.len_utf8()..].trim_start();
    }
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let after = rest[digits.len()..].strip_prefix(". ")?;
    let label = after.trim();
    if label.is_empty() {
        return None;
    }
    Some((cursor, digits.parse().ok()?, label.to_string()))
}

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| matches!(c, '─' | '━' | '═' | '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘' | '-' | '▔' | '▁'))
}

/// A check box at the start of an option's label: whether it is ticked, and
/// the label after it. Claude Code draws `[ ]` and `[✔]`.
fn checkbox(label: &str) -> Option<(bool, &str)> {
    const BOXES: [(&str, bool); 10] = [
        ("[ ]", false),
        ("☐", false),
        ("◻", false),
        ("[x]", true),
        ("[X]", true),
        ("[✔]", true),
        ("☑", true),
        ("☒", true),
        ("◼", true),
        ("✔ ", true),
    ];
    BOXES.iter().find_map(|(mark, on)| label.strip_prefix(mark).map(|rest| (*on, rest.trim_start())))
}

/// The unnumbered row under a several-choice question's options that sends
/// the choices: whether the cursor is on it, and its label.
fn submit_row(line: &str) -> Option<(bool, String)> {
    let mut rest = unboxed(line);
    let mut cursor = false;
    if let Some(c) = rest.chars().next().filter(|c| MARKERS.contains(c)) {
        cursor = true;
        rest = rest[c.len_utf8()..].trim_start();
    }
    ["Submit", "Next"].iter().find(|l| rest == **l).map(|l| (cursor, l.to_string()))
}

/// Claude's question tabs above the question ("☐ Colour", or
/// "← ☐ Colour  ☐ Size  ✔ Submit →" for several): navigation, not text.
fn is_question_tabs(text: &str) -> bool {
    let text = text.trim_start_matches('←').trim_start();
    ["☐", "☑", "☒", "✔"].iter().any(|p| text.starts_with(p))
}

/// An option row while reading: its screen row, whether the cursor is on
/// it, its label and its indented detail.
type OptionRow = (usize, bool, String, String);

/// A run of numbered options while reading.
#[derive(Default)]
struct Run {
    first_row: usize,
    rows: Vec<OptionRow>,
    /// The Submit row: options above it, whether the cursor is on it, its label.
    submit: Option<(usize, bool, String)>,
    /// A rule followed the options. Claude Code draws one between its
    /// question's options (and Submit row) and "Chat about this".
    ruled: bool,
}

/// The prompt the screen shows, when the deck's screen signals say it asks
/// one and numbered options are on screen.
pub fn parse(lines: &[String]) -> Option<Prompt> {
    let kind = status::screen_signals(lines).prompt?;
    read(lines, kind)
}

/// The numbered list on screen, whatever shows it, such as Codex's `/model`
/// picker.
pub fn options(lines: &[String]) -> Option<Prompt> {
    read(lines, "list")
}

fn read(lines: &[String], kind: &str) -> Option<Prompt> {
    let tail = status::live_tail(lines, PROMPT_ROWS);
    // The last run of options numbered 1, 2, … with indented details between,
    // a Submit row under them and options after a rule.
    let mut best: Option<Run> = None;
    let mut current = Run::default();
    let finish = |run: &mut Run, best: &mut Option<Run>| {
        let run = std::mem::take(run);
        if run.rows.len() >= 2 || (kind == "async question" && run.rows.len() == 1) {
            *best = Some(run);
        }
    };
    for (i, line) in tail.iter().enumerate() {
        if let Some((cursor, n, label)) = option_row(line) {
            if n == current.rows.len() + 1 {
                if current.rows.is_empty() {
                    current.first_row = i;
                }
                current.rows.push((i, cursor, label, String::new()));
                current.ruled = false;
                continue;
            }
            if n == 1 {
                finish(&mut current, &mut best);
                current.first_row = i;
                current.rows.push((i, cursor, label, String::new()));
                continue;
            }
        }
        if current.rows.is_empty() {
            continue;
        }
        let text = unboxed(line);
        if current.ruled {
            // Only the next numbered option continues the run past a rule.
            finish(&mut current, &mut best);
            continue;
        }
        if current.submit.is_none() && current.rows.iter().filter(|r| checkbox(&r.2).is_some()).count() >= 2 {
            if let Some((cursor, label)) = submit_row(line) {
                current.submit = Some((current.rows.len(), cursor, label));
                continue;
            }
        }
        if is_rule(text) {
            current.ruled = true;
            continue;
        }
        let indented = line.trim_start_matches(['│', '┃']).starts_with("   ");
        let described = current.submit.is_some();
        let Some(last) = current.rows.last_mut() else { continue };
        if !described && !text.is_empty() && indented && i == last.0 + 1 + last.3.lines().count() {
            if !last.3.is_empty() {
                last.3.push('\n');
            }
            last.3.push_str(text);
            continue;
        }
        if text.is_empty() || !indented {
            finish(&mut current, &mut best);
        }
    }
    finish(&mut current, &mut best);
    let Run { first_row, rows, submit, .. } = best?;
    let selected = rows.iter().position(|r| r.1).unwrap_or(0);
    let mut options: Vec<PromptOption> =
        rows.into_iter().map(|(_, _, label, detail)| PromptOption { label, detail, checked: None }).collect();
    // Check boxes on at least two options make a several-choice question;
    // one box-like label alone is just its text.
    let multi = options.iter().filter(|o| checkbox(&o.label).is_some()).count() >= 2;
    if multi {
        for o in &mut options {
            if let Some((on, rest)) = checkbox(&o.label) {
                o.label = rest.to_string();
                o.checked = Some(on);
            }
        }
    }
    let free_text = if kind == "async question" {
        options
            .len()
            .checked_sub(1)
            .filter(|&i| matches!(options[i].label.as_str(), "Other" | "Other (write an answer)"))
    } else {
        options.iter().position(|o| o.label.to_lowercase().starts_with("type something"))
    };
    let (submit_at, on_submit, submit) = match submit.filter(|_| multi) {
        Some((at, cursor, label)) => (at, cursor, Some(label)),
        None => (0, false, None),
    };
    // The question: the text above the options, up to a rule or a gap.
    let mut question: Vec<&str> = Vec::new();
    let mut blank_run = 0;
    for line in tail[..first_row].iter().rev() {
        let text = unboxed(line);
        // Async editors draw `1 of 3` above the question. Older terminal
        // output includes a working clock and must not change its identity.
        let words: Vec<_> = text.split_whitespace().collect();
        let progress = kind == "async question"
            && words.len() == 3
            && words[1] == "of"
            && words[0].parse::<usize>().is_ok()
            && words[2].parse::<usize>().is_ok();
        // A single async question has no progress row. Its indented title
        // follows this exact native header, with the working clock above it.
        let async_header =
            kind == "async question" && matches!(text, "• Queued follow-up inputs" | "Queued follow-up inputs");
        if progress || async_header || is_rule(text) || is_question_tabs(text) || question.len() >= QUESTION_LINES {
            break;
        }
        if text.is_empty() {
            blank_run += 1;
            if blank_run >= 2 && !question.is_empty() {
                break;
            }
            continue;
        }
        if blank_run > 0 && !question.is_empty() {
            question.push("");
        }
        blank_run = 0;
        question.push(text);
        // Codex asks its question above the command it shows; Claude asks
        // right above the options, under the command in its box.
        let lower = text.to_lowercase();
        if question.len() > 1 && status::PROMPTS.iter().any(|(p, _)| lower.contains(p)) {
            break;
        }
    }
    question.reverse();
    Some(Prompt {
        kind: kind.to_string(),
        question: question.join("\n").trim().to_string(),
        options,
        selected,
        free_text,
        multi,
        submit,
        submit_at,
        on_submit,
    })
}

/// One arrow press per row from row `from` to row `to`.
fn arrows(from: usize, to: usize, modes: u32, out: &mut Vec<String>) {
    let up = to < from;
    out.extend((0..from.abs_diff(to)).map(|_| keys::arrow(up, modes)));
}

impl Prompt {
    /// Identifies what is asked, not where the cursor is: SHA-256 of the
    /// question and option labels, in hex.
    pub fn fingerprint(&self) -> String {
        let mut text = format!("{}\u{0}{}", self.kind, self.question);
        for o in &self.options {
            text.push('\u{0}');
            text.push_str(&o.label);
        }
        hex(ring::digest::digest(&ring::digest::SHA256, text.as_bytes()).as_ref())
    }

    /// The key presses, one write each, that move the cursor from where it
    /// is to option `choice` and press Enter there; for a free-text option,
    /// type `text` into it instead and then press Enter. `modes` are the
    /// program's terminal modes. A program reading several keys from one
    /// write may take them as one unknown sequence, so they stay separate.
    pub fn keys(&self, choice: usize, text: Option<&str>, modes: u32) -> Result<Vec<String>> {
        if choice >= self.options.len() {
            bail!("no option {}", choice + 1);
        }
        if self.kind == "async question" {
            // In Other's inline editor, a digit types into the retained draft.
            if self.selected + 1 == self.options.len() && self.free_text != Some(self.selected) {
                bail!("this question has a text answer in progress; finish it at the desk");
            }
            if choice >= 9 {
                bail!("this option has no numeric shortcut");
            }
            let mut keys = vec![(choice + 1).to_string()];
            if Some(choice) == self.free_text {
                let text = text.map(str::trim).filter(|t| !t.is_empty()).context("this option needs text")?;
                keys.push(keys::encode_paste(text, modes));
                keys.push("\r".into());
            } else if text.is_some() {
                bail!("this option takes no text");
            }
            return Ok(keys);
        }
        if self.options[choice].checked.is_some() {
            bail!("option {} is one of several choices; send them together", choice + 1);
        }
        let mut out = Vec::new();
        arrows(self.cursor_row(), self.row_of(choice), modes, &mut out);
        match text.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => {
                if self.free_text != Some(choice) {
                    bail!("option {} takes no text", choice + 1);
                }
                // The cursor on "Type something." is already in its text field.
                out.push(keys::encode_paste(t, modes));
            }
            None if self.free_text == Some(choice) => bail!("option {} needs text", choice + 1),
            None => {}
        }
        out.push("\r".into());
        Ok(out)
    }

    /// The key presses that leave exactly the options `choices` ticked on a
    /// several-choice question and send them: Enter on each box whose state
    /// differs, typed `text` into the free-text option (typing ticks it),
    /// then Enter on the Submit row. Claude Code then shows its "Review your
    /// answers" step, or the next question after "Next".
    pub fn keys_several(&self, choices: &[usize], text: Option<&str>, modes: u32) -> Result<Vec<String>> {
        if !self.multi || self.submit.is_none() {
            bail!("this question takes one choice");
        }
        let mut wanted = choices.to_vec();
        for &c in choices {
            let option = self.options.get(c).with_context(|| format!("no option {}", c + 1))?;
            if option.checked.is_none() {
                bail!("option {} cannot be chosen with others", c + 1);
            }
        }
        let text = text.map(str::trim).filter(|t| !t.is_empty());
        match (text, self.free_text) {
            (Some(_), None) => bail!("this question takes no typed answer"),
            (Some(_), Some(free)) if !wanted.contains(&free) => wanted.push(free),
            (None, Some(free)) if wanted.contains(&free) => bail!("option {} needs text", free + 1),
            _ => {}
        }
        if wanted.is_empty() {
            bail!("choose at least one option");
        }
        let mut out = Vec::new();
        let mut at = self.cursor_row();
        for (i, option) in self.options.iter().enumerate() {
            let Some(ticked) = option.checked else { continue };
            let typed = text.filter(|_| self.free_text == Some(i));
            if typed.is_none() && wanted.contains(&i) == ticked {
                continue;
            }
            arrows(at, self.row_of(i), modes, &mut out);
            at = self.row_of(i);
            out.push(match typed {
                // The cursor on "Type something" is already in its text field.
                Some(t) => keys::encode_paste(t, modes),
                None => "\r".into(),
            });
        }
        arrows(at, self.submit_at, modes, &mut out);
        out.push("\r".into());
        Ok(out)
    }

    /// Where option `i` is among the rows the arrows move through: Claude's
    /// Submit row sits between its question's options and "Chat about this".
    fn row_of(&self, i: usize) -> usize {
        if self.submit.is_some() && i >= self.submit_at {
            i + 1
        } else {
            i
        }
    }

    fn cursor_row(&self) -> usize {
        if self.on_submit {
            self.submit_at
        } else {
            self.row_of(self.selected)
        }
    }

    /// Claude Code's last step after a form of questions: "Review your
    /// answers", then "Ready to submit your answers?" with Submit answers and
    /// Cancel (Claude Code 2.1.314).
    pub fn is_answer_review(&self) -> bool {
        self.options.len() == 2 && self.options[0].label == "Submit answers" && self.options[1].label == "Cancel"
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[path = "tests/prompt.rs"]
mod tests;
