//! Open the pending Codex question selected on the phone, preserving queued
//! messages and refusing an answer if another input changes the target.
use super::chat::ChatQuestion;
use crate::codexpick::SessionIo;
use crate::{prompt, status};
use anyhow::{bail, Context, Result};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::thread;
use std::time::{Duration, Instant};

fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// Codex renders Markdown before drawing titles and options. Compare against
// that visible text, never parse the screen itself as Markdown: inline code
// can intentionally contain literal Markdown punctuation.
fn visible(text: &str, link_destinations: bool) -> Option<String> {
    let mut out = String::new();
    let mut links = Vec::new();
    for event in Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES) {
        match event {
            Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) => out.push_str(&text),
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::Start(Tag::Heading { level, .. }) => out.push_str(&format!("{} ", "#".repeat(level as usize))),
            // These native structures carry layout-dependent prefixes. An
            // unverified identity must block native matching, not disappear
            // from duplicate detection and collide with another question.
            Event::Rule
            | Event::Start(
                Tag::BlockQuote | Tag::List(_) | Tag::Item | Tag::Table(_) | Tag::Image { .. } | Tag::CodeBlock(_),
            ) => return None,
            Event::Start(Tag::Link { dest_url, .. }) => links.push((dest_url, out.len())),
            Event::End(TagEnd::Link) => {
                if let Some((url, start)) = links.pop() {
                    if link_destinations && !url.is_empty() && &out[start..] != url.as_ref() {
                        out.push_str(&format!(" ({url})"));
                    }
                }
            }
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item | TagEnd::CodeBlock) => out.push('\n'),
            _ => {}
        }
    }
    Some(normalized(&out))
}

fn forms(text: &str) -> Option<[String; 2]> {
    // Codex includes destinations on terminals without hyperlink support.
    Some([visible(text, false)?, visible(text, true)?])
}

fn text_matches(shown: &str, source: &str) -> bool {
    forms(source).is_some_and(|forms| forms.contains(&normalized(shown)))
}

fn same_text(first: &str, second: &str) -> bool {
    match (forms(first), forms(second)) {
        (Some(first), Some(second)) => second.iter().any(|text| first.contains(text)),
        _ => true,
    }
}

pub fn matches(shown: &prompt::Prompt, asked: &ChatQuestion) -> bool {
    shown.kind == "async question"
        && !asked.answered
        && !asked.title.trim().is_empty()
        && text_matches(&shown.question, &asked.title)
        && shown.options.len() == asked.options.len() + 1
        && shown
            .options
            .iter()
            .zip(&asked.options)
            .all(|(actual, expected)| text_matches(&format!("{} {}", actual.label, actual.detail), expected))
}

/// A late answer is still a contextual answer after Codex closes its editor.
/// Use Codex's own reply envelope so the logged receipt resolves the same card.
pub fn follow_up(asked: &ChatQuestion, choice: usize) -> Result<String> {
    let answer = asked.options.get(choice).context("no such answer option")?;
    follow_up_text(asked, answer)
}

/// Preserve the native question identity for a custom answer too.
pub fn follow_up_text(asked: &ChatQuestion, answer: &str) -> Result<String> {
    let (message, index) = asked.id.rsplit_once(':').context("invalid question identity")?;
    let index: usize = index.parse().context("invalid question index")?;
    let identity = serde_json::json!(["request_user_input_async", message, index]).to_string();
    let body = serde_json::json!([{"questionItemId": identity, "question": asked.title, "answer": answer}]);
    Ok(format!("<send_user_message_question_reply>\n{body}\n</send_user_message_question_reply>"))
}

/// Resolve a stable card identity even after the native editor closes.
pub fn by_id(questions: &[ChatQuestion], id: &str) -> Option<ChatQuestion> {
    questions.iter().find(|q| q.id == id && !q.answered).cloned()
}

/// A screen cannot distinguish two questions with identical wording and choices.
/// Check all unresolved questions, including ones outside retained chat rows.
pub fn lookup(questions: &[ChatQuestion], id: &str) -> Option<ChatQuestion> {
    let asked = questions.iter().find(|q| q.id == id && !q.answered)?;
    if questions
        .iter()
        .any(|q| !q.answered && (forms(&q.title).is_none() || q.options.iter().any(|o| forms(o).is_none())))
    {
        return None;
    }
    (questions
        .iter()
        .filter(|q| {
            !q.answered
                && same_text(&q.title, &asked.title)
                && q.options.len() == asked.options.len()
                && q.options.iter().zip(&asked.options).all(|(a, b)| same_text(a, b))
        })
        .count()
        == 1)
        .then(|| asked.clone())
}

pub fn find(
    io: &mut impl SessionIo,
    id: u64,
    asked: &ChatQuestion,
    native_allowed: bool,
) -> Result<Option<(prompt::Prompt, u64)>> {
    let (mut lines, mut expected_inputs) = io.screen(id)?;
    let signals = status::screen_signals(&lines);
    if signals.prompt.is_none() && !signals.queued_question {
        return Ok(None);
    }
    if !native_allowed {
        bail!("these question choices cannot be identified safely in the open menu; answer them at the desk");
    }
    if let Some(shown) = prompt::parse(&lines).filter(|p| p.kind == "async question") {
        if shown.selected + 1 == shown.options.len() && shown.free_text != Some(shown.selected) {
            bail!("this question has a text answer in progress; finish it at the desk");
        }
        if matches(&shown, asked) {
            return Ok(Some((shown, expected_inputs)));
        }
    }
    let expanded = prompt::parse(&lines).is_some_and(|p| p.kind == "async question");
    if !expanded {
        let signals = status::screen_signals(&lines);
        if !signals.queued_question {
            if signals.prompt.is_some() {
                bail!("another question is open; answer it first");
            }
            return Ok(None);
        }
        io.input(id, "\x1b[1;2D")?;
        expected_inputs += 1;
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut previous = None;
    let mut rewind = true;
    while Instant::now() < deadline {
        if !lines.is_empty() {
            if let Some(shown) = prompt::parse(&lines).filter(|p| p.kind == "async question") {
                if shown.selected + 1 == shown.options.len() && shown.free_text != Some(shown.selected) {
                    bail!("this question has a text answer in progress; finish it at the desk");
                }
                if matches(&shown, asked) {
                    return Ok(Some((shown, expected_inputs)));
                }
                let fingerprint = shown.fingerprint();
                if previous.as_ref() != Some(&fingerprint) {
                    let footer = status::live_tail(&lines, 8).join(" ").to_lowercase();
                    let key = if rewind && footer.contains("prev question") {
                        "\x1b[1;2C"
                    } else {
                        rewind = false;
                        // At the end, Shift+Left can open a queued message.
                        // Never navigate past the question editor.
                        if !footer.contains("next question") {
                            bail!("this question is no longer waiting");
                        }
                        "\x1b[1;2D"
                    };
                    previous = Some(fingerprint);
                    io.input(id, key)?;
                    expected_inputs += 1;
                }
            }
        }
        thread::sleep(Duration::from_millis(150));
        let (next, inputs) = io.screen(id)?;
        if inputs != expected_inputs {
            bail!("the session took other input; try again");
        }
        lines = next;
        let signals = status::screen_signals(&lines);
        if !signals.queued_question && signals.prompt.is_none() {
            return Ok(None);
        }
    }
    bail!("Codex did not show this question's complete options; answer it at the desk")
}

#[cfg(test)]
#[path = "../tests/questionpick.rs"]
mod tests;
