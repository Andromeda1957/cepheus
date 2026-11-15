use super::*;
use crate::screen::M_APP_CURSOR;

fn lines(text: &str) -> Vec<String> {
    text.lines().map(String::from).collect()
}

/// Claude Code's permission dialog, drawn in a box.
const CLAUDE_PERMISSION: &str = "\
⏺ I will run the tests.

╭──────────────────────────────────────────────╮
│ Bash command                                 │
│                                              │
│   cargo test -q                              │
│   Run the test suite                         │
│                                              │
│ Do you want to proceed?                      │
│ ❯ 1. Yes                                     │
│   2. Yes, and don't ask again for cargo test │
│   3. No, and tell Claude what to do differently (esc) │
╰──────────────────────────────────────────────╯";

/// Codex's command approval: the question above the command.
const CODEX_APPROVAL: &str = "\
• Running the tests next.

  Would you like to run the following command?

  $ cargo test -q

› 1. Yes, proceed (y)
  2. Yes, and don't ask again for this command (p)
  3. No, and tell Codex what to do differently (esc)

  Press enter to confirm or esc to cancel";

/// Claude Code's AskUserQuestion with option descriptions.
const CLAUDE_QUESTION: &str = "\
 ☐ Colour

Which colour should the button be?

❯ 1. Red
     Warm and loud
  2. Blue
     Calm
  3. Type something.

Enter to select · ↑/↓ to navigate · Esc to cancel";

/// Claude Code 2.1.314's single-choice question: "Chat about this" sits
/// under a rule after the options.
const CLAUDE_QUESTION_CHAT: &str = "\
 ☐ Colour

Which colour?

❯ 1. Red
     Warm
  2. Blue
     Calm
  3. Type something.
────────────────────────────────────────
  4. Chat about this

Enter to select · ↑/↓ to navigate · Esc to cancel";

/// Claude Code 2.1.314's several-choice question, as the owner's session
/// showed it on 2026-10-30 and the phone could not answer: ticks in the
/// labels, an unnumbered Submit row, then "Chat about this" under a rule.
const CLAUDE_SEVERAL: &str = "\
────────────────────────────────────────
←  ☐ Fragile how  ✔ Submit  →

│ Which of these match what makes the Cepheus Android app feel fragile and glass-like next to the
│ Claude/Codex apps? Pick all that apply.

❯ 1. [ ] Connection flicker
         Every time you return to the app, a 'Connecting to the deck…' banner slides in and pushes the list
         down.
  2. [ ] Messages feel unsent
         What you send doesn't appear until the deck echoes it back.
  3. [ ] Looks thin and flat
         Hairline pale borders, every surface the same flat color, small status dots and text.
  4. [ ] Stuck or broken states
         It freezes, loses your place, gets stuck on a spinner.
  5. [ ] Type something
     Submit
────────────────────────────────────────
  6. Chat about this

Enter to select · ↑/↓ to navigate · Esc to cancel
──────────────────────────────────── implementer ─";

/// What Claude Code 2.1.314 shows after Submit: one more Enter sends.
const CLAUDE_REVIEW: &str = "\
←  ☒ Fruits  ✔ Submit  →

Review your answers

 ● Which fruits do you like?
   → Apple, mango

Ready to submit your answers?

❯ 1. Submit answers
  2. Cancel";

const DOWN: &str = "\x1b[B";
const UP: &str = "\x1b[A";

fn keys(list: &[&str]) -> Vec<String> {
    list.iter().map(|k| k.to_string()).collect()
}

#[test]
fn reads_claudes_boxed_permission_prompt() {
    let p = parse(&lines(CLAUDE_PERMISSION)).expect("a prompt");
    assert_eq!(p.kind, "permission");
    assert_eq!(p.options.len(), 3);
    assert_eq!(p.options[0].label, "Yes");
    assert_eq!(p.options[2].label, "No, and tell Claude what to do differently (esc)");
    assert_eq!(p.selected, 0);
    assert!(p.question.starts_with("Bash command"), "{}", p.question);
    assert!(p.question.contains("cargo test -q") && p.question.ends_with("Do you want to proceed?"));
    assert!(!p.question.contains("I will run"), "the transcript above the box is not the question");
    assert_eq!(p.free_text, None);
}

#[test]
fn reads_codexs_command_approval_with_the_command_under_its_question() {
    let p = parse(&lines(CODEX_APPROVAL)).expect("a prompt");
    assert_eq!(p.kind, "command approval");
    assert_eq!(p.options[0].label, "Yes, proceed (y)");
    assert_eq!(p.options.len(), 3);
    assert_eq!(p.question, "Would you like to run the following command?\n\n$ cargo test -q");
}

#[test]
fn reads_a_question_with_descriptions_and_a_free_text_option() {
    let p = parse(&lines(CLAUDE_QUESTION)).expect("a prompt");
    assert_eq!(p.question, "Which colour should the button be?");
    assert_eq!(p.options[0], PromptOption { label: "Red".into(), detail: "Warm and loud".into(), checked: None });
    assert_eq!(p.options[1].detail, "Calm");
    assert_eq!(p.free_text, Some(2));
    assert!(!p.multi);
    // At the top of a tall, mostly blank screen, as in a fresh session.
    let mut tall = lines(CLAUDE_QUESTION);
    tall.resize(60, String::new());
    assert_eq!(parse(&tall).expect("a prompt at the top").question, "Which colour should the button be?");
    // With several questions the tabs row names each one and Submit.
    let tabs = CLAUDE_QUESTION.replace(" ☐ Colour", "← ☐ Colour  ☐ Size  ✔ Submit →");
    assert_eq!(parse(&lines(&tabs)).expect("a prompt").question, "Which colour should the button be?");
}

#[test]
fn no_prompt_while_a_turn_runs_or_without_options() {
    let working = format!("{CLAUDE_PERMISSION}\n✻ Thinking… (3s · esc to interrupt)");
    assert_eq!(parse(&lines(&working)), None);
    assert_eq!(parse(&lines("Do you want to proceed?\nno options here")), None);
    assert_eq!(parse(&lines("> hello\n")), None);
}

#[test]
fn keys_move_from_the_cursor_to_the_choice_one_press_per_write() {
    let mut p = parse(&lines(CLAUDE_PERMISSION)).unwrap();
    assert_eq!(p.keys(0, None, 0).unwrap(), vec!["\r".to_string()]);
    assert_eq!(p.keys(2, None, 0).unwrap(), vec!["\x1b[B".to_string(), "\x1b[B".into(), "\r".into()]);
    assert_eq!(p.keys(1, None, M_APP_CURSOR).unwrap(), vec!["\x1bOB".to_string(), "\r".into()]);
    p.selected = 2;
    assert_eq!(p.keys(0, None, 0).unwrap(), vec!["\x1b[A".to_string(), "\x1b[A".into(), "\r".into()]);
    assert!(p.keys(3, None, 0).is_err(), "no fourth option");
    assert!(p.keys(1, Some("text"), 0).is_err(), "only a free-text option takes text");
}

#[test]
fn free_text_is_typed_into_its_option_then_submitted() {
    let p = parse(&lines(CLAUDE_QUESTION)).unwrap();
    let keys = p.keys(2, Some("Green\x1b[201~ please"), 0).unwrap();
    assert_eq!(keys.len(), 4);
    assert_eq!(&keys[..2], &["\x1b[B".to_string(), "\x1b[B".into()]);
    assert_eq!(keys[2], "Green[201~ please", "escape cannot end the text early");
    assert_eq!(keys[3], "\r");
    assert!(p.keys(2, None, 0).is_err(), "an empty answer is refused");
}

#[test]
fn chat_about_this_under_the_rule_is_an_option_too() {
    let p = parse(&lines(CLAUDE_QUESTION_CHAT)).expect("a prompt");
    let labels: Vec<&str> = p.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, ["Red", "Blue", "Type something.", "Chat about this"]);
    assert_eq!(p.free_text, Some(2));
    assert_eq!(p.submit, None);
    // Claude Code 2.1.314 moved Down from option 3 straight onto it.
    assert_eq!(p.keys(3, None, 0).unwrap(), keys(&[DOWN, DOWN, DOWN, "\r"]));
    // Lines after a rule that are not the next option end the run.
    let boxed = parse(&lines(&format!("{CLAUDE_PERMISSION}\n     indented text under the box"))).unwrap();
    assert_eq!(boxed.options.len(), 3);
    assert_eq!(boxed.options[2].detail, "");
}

#[test]
fn reads_a_several_choice_question_with_its_ticks_and_submit_row() {
    let p = parse(&lines(CLAUDE_SEVERAL)).expect("a prompt");
    assert_eq!(p.kind, "question");
    assert!(p.multi);
    assert_eq!(
        p.question,
        "Which of these match what makes the Cepheus Android app feel fragile and glass-like next to the\n\
         Claude/Codex apps? Pick all that apply."
    );
    let labels: Vec<&str> = p.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "Connection flicker",
            "Messages feel unsent",
            "Looks thin and flat",
            "Stuck or broken states",
            "Type something",
            "Chat about this"
        ]
    );
    let ticks: Vec<Option<bool>> = p.options.iter().map(|o| o.checked).collect();
    assert_eq!(ticks, [Some(false), Some(false), Some(false), Some(false), Some(false), None]);
    assert_eq!(
        p.options[2].detail,
        "Hairline pale borders, every surface the same flat color, small status dots and text."
    );
    assert_eq!(p.options[4].detail, "", "the Submit row is not the last option's description");
    assert_eq!(p.submit.as_deref(), Some("Submit"));
    assert_eq!((p.submit_at, p.on_submit, p.selected), (5, false, 0));
    assert_eq!(p.free_text, Some(4));
    // A tick moves no part of what is asked.
    let ticked = parse(&lines(&CLAUDE_SEVERAL.replace("[ ] Looks thin", "[✔] Looks thin"))).unwrap();
    assert_eq!(ticked.options[2].checked, Some(true));
    assert_eq!(ticked.fingerprint(), p.fingerprint());
    // Before another question the row reads Next.
    let next = parse(&lines(&CLAUDE_SEVERAL.replace("     Submit", "     Next"))).unwrap();
    assert_eq!(next.submit.as_deref(), Some("Next"));
    // With the cursor on the row itself.
    let on_submit =
        parse(&lines(&CLAUDE_SEVERAL.replace("❯ 1. [ ]", "  1. [ ]").replace("     Submit", "❯    Submit"))).unwrap();
    assert!(on_submit.on_submit);
    assert_eq!(on_submit.options.len(), 6);
}

#[test]
fn several_choices_tick_what_differs_type_the_free_text_and_submit() {
    let p = parse(&lines(CLAUDE_SEVERAL)).unwrap();
    // Option 3 alone, as the owner wanted to answer.
    assert_eq!(p.keys_several(&[2], None, 0).unwrap(), keys(&[DOWN, DOWN, "\r", DOWN, DOWN, DOWN, "\r"]));
    // Two boxes and typed text: typing on "Type something" ticks it.
    assert_eq!(
        p.keys_several(&[0, 2], Some(" mango "), 0).unwrap(),
        keys(&["\r", DOWN, DOWN, "\r", DOWN, DOWN, "mango", DOWN, "\r"])
    );
    // From the Submit row, with option 1 ticked at the desk: untick it, tick 2.
    let moved = CLAUDE_SEVERAL.replace("❯ 1. [ ]", "  1. [✔]").replace("     Submit", "❯    Submit");
    let moved = parse(&lines(&moved)).unwrap();
    assert_eq!(
        moved.keys_several(&[1], None, 0).unwrap(),
        keys(&[UP, UP, UP, UP, UP, "\r", DOWN, "\r", DOWN, DOWN, DOWN, DOWN, "\r"])
    );
    // Nothing to change: straight to Submit.
    assert_eq!(moved.keys_several(&[0], None, 0).unwrap(), keys(&["\r"]));
    assert!(p.keys_several(&[], None, 0).is_err(), "nothing chosen");
    assert!(p.keys_several(&[5], None, 0).is_err(), "Chat about this is no check box");
    assert!(p.keys_several(&[4], None, 0).is_err(), "the free-text option needs text");
    assert!(p.keys_several(&[9], None, 0).is_err(), "no such option");
    assert!(p.keys(0, None, 0).is_err(), "one box alone is not an answer");
}

#[test]
fn chat_about_this_is_reached_past_the_submit_row() {
    let p = parse(&lines(CLAUDE_SEVERAL)).unwrap();
    // Claude Code 2.1.314: Down from "Type something" stops on Submit first.
    assert_eq!(p.keys(5, None, 0).unwrap(), keys(&[DOWN, DOWN, DOWN, DOWN, DOWN, DOWN, "\r"]));
}

#[test]
fn several_choices_without_a_submit_row_cannot_be_sent() {
    let multi = "Pick toppings\n❯ 1. [ ] Cheese\n  2. [ ] Olives\n  3. [ ] Basil\nEnter to select";
    let p = parse(&lines(multi)).unwrap();
    assert!(p.multi);
    assert!(p.keys(0, None, 0).is_err());
    assert!(p.keys_several(&[0], None, 0).is_err());
    // One box-like label alone is text, not a check box.
    let single = parse(&lines("Pick one\n❯ 1. [x] marks the spot\n  2. Elsewhere\nEnter to select")).unwrap();
    assert!(!single.multi);
    assert_eq!(single.options[0].label, "[x] marks the spot");
}

#[test]
fn reads_claudes_answer_review() {
    let p = parse(&lines(CLAUDE_REVIEW)).expect("a prompt");
    assert!(p.is_answer_review());
    assert_eq!(p.selected, 0);
    assert!(!parse(&lines(CLAUDE_SEVERAL)).unwrap().is_answer_review());
    assert!(!parse(&lines(CLAUDE_PERMISSION)).unwrap().is_answer_review());
}

#[test]
fn the_fingerprint_names_the_question_not_the_cursor() {
    let p = parse(&lines(CLAUDE_PERMISSION)).unwrap();
    let moved =
        parse(&lines(&CLAUDE_PERMISSION.replace("❯ 1. Yes ", "  1. Yes ").replace("  2. Yes,", "❯ 2. Yes,"))).unwrap();
    assert_eq!(moved.selected, 1);
    assert_eq!(p.fingerprint(), moved.fingerprint());
    let other = parse(&lines(&CLAUDE_PERMISSION.replace("cargo test -q", "rm -rf build"))).unwrap();
    assert_ne!(p.fingerprint(), other.fingerprint());
    assert_eq!(p.fingerprint().len(), 64);
}

#[test]
fn async_question_identity_excludes_the_live_working_clock() {
    let screen = "Working (esc to interrupt) · 4s\n\n1 of 3\nPick one?\n\n› 1. First\n  2. Second\n  3. Other (write an answer)\n\nenter submit   ⌃] skip   ⇧→ main prompt";
    let before = parse(&lines(screen)).unwrap();
    let after = parse(&lines(&screen.replace("4s", "5s"))).unwrap();
    assert_eq!(before.question, "Pick one?");
    assert_eq!(before.fingerprint(), after.fingerprint());
}

#[test]
fn a_single_async_question_excludes_queued_header_and_working_clock() {
    let screen = "• Working (4s • esc to interrupt)\n\n• Queued follow-up inputs\n\n  Should :cd start a new chat?\n\n› 1. Fresh chat\n  2. Keep conversation\n  3. Other\n\nenter submit   ⌃] skip   ⇧→ main prompt";
    let question = parse(&lines(screen)).unwrap();
    assert_eq!(question.question, "Should :cd start a new chat?");
    assert_eq!(question.free_text, Some(2));
}

#[test]
fn free_text_only_async_question_is_answerable_from_the_phone() {
    let screen = "• Queued follow-up inputs\n\nWhat should happen next?\n\n› 1. Other\n\nenter submit   ⌃] skip   ⇧→ main prompt";
    let question = parse(&lines(screen)).expect("a free-text question");
    assert_eq!(question.free_text, Some(0));
    let keys = question.keys(0, Some("Preserve the draft"), 0).unwrap();
    assert_eq!(keys, vec!["1", "Preserve the draft", "\r"]);
}
