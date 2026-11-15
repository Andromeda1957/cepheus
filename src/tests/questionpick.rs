use super::*;
#[test]
fn only_matching_native_question_and_complete_options_can_be_answered() {
    let lines: Vec<_> = [
        "1 of 2",
        "Pick one?",
        "",
        "› 1. First",
        "  2. Second",
        "  3. Third",
        "  4. Other (write an answer)",
        "",
        "enter submit   ⌃] skip   ⇧→ main prompt",
    ]
    .map(String::from)
    .into();
    let shown = prompt::parse(&lines).unwrap();
    let mut asked = ChatQuestion {
        id: "q:0".into(),
        answered: false,
        title: "Pick one?".into(),
        options: vec!["First".into(), "Second".into(), "Third".into()],
    };
    assert!(matches(&shown, &asked));
    assert_eq!(shown.keys(1, None, 0).unwrap(), ["2"]);
    asked.title = "A different question?".into();
    assert!(!matches(&shown, &asked));
    asked.title = "Pick one?".into();
    asked.options[1] = "Changed".into();
    assert!(!matches(&shown, &asked));
}

struct Fake {
    screens: std::collections::VecDeque<(Vec<String>, u64)>,
    keys: Vec<String>,
}
impl SessionIo for Fake {
    fn screen(&mut self, _: u64) -> Result<(Vec<String>, u64)> {
        self.screens.pop_front().ok_or_else(|| anyhow::anyhow!("unexpected screen read"))
    }
    fn input(&mut self, _: u64, data: &str) -> Result<()> {
        self.keys.push(data.to_string());
        Ok(())
    }
    fn type_message(&mut self, _: u64, _: &str) -> Result<()> {
        panic!("a question must never use ordinary input")
    }
}
fn asked() -> ChatQuestion {
    ChatQuestion {
        id: "q:0".into(),
        answered: false,
        title: "Pick one?".into(),
        options: vec!["First".into(), "Second".into(), "Third".into()],
    }
}
fn editor(title: &str, navigation: &str) -> Vec<String> {
    format!("1 of 3\n{title}\n\n› 1. First\n  2. Second\n  3. Third\n  4. Other (write an answer)\n\nenter submit   ⌃] skip   ⇧→ {navigation}").lines().map(String::from).collect()
}
#[test]
fn another_input_aborts_before_answering_a_matching_question() {
    let queue = vec!["Working (esc to interrupt)".into(), "  ? 3 questions".into()];
    let mut io = Fake { screens: [(queue, 7), (editor("Pick one?", "main prompt"), 9)].into(), keys: vec![] };
    let error = find(&mut io, 1, &asked(), true).unwrap_err().to_string();
    assert!(error.contains("other input"), "{error}");
    assert_eq!(io.keys, ["\x1b[1;2D"]);
}
#[test]
fn search_rewinds_without_opening_a_queued_message() {
    let mut io = Fake {
        screens: [
            (editor("Third?", "prev question · queued messages"), 5),
            (editor("Second?", "prev question · next question"), 6),
            (editor("Pick one?", "main prompt · next question"), 7),
        ]
        .into(),
        keys: vec![],
    };
    assert_eq!(find(&mut io, 1, &asked(), true).unwrap().unwrap().1, 7);
    assert_eq!(io.keys, ["\x1b[1;2C", "\x1b[1;2C"]);
}
#[test]
fn an_expired_question_never_opens_the_queued_message() {
    let mut io = Fake { screens: [(editor("Another?", "main prompt · queued messages"), 5)].into(), keys: vec![] };
    assert!(find(&mut io, 1, &asked(), true).is_err());
    assert!(io.keys.is_empty());
}
#[test]
fn a_question_title_containing_the_old_title_is_not_the_same_question() {
    let shown = prompt::parse(&editor("Do not Pick one?", "main prompt")).unwrap();
    assert!(!matches(&shown, &asked()));
}

#[test]
fn identical_questions_are_ambiguous_even_when_one_card_is_newer() {
    let first = asked();
    let mut second = first.clone();
    second.id = "q:1".into();
    assert_eq!(lookup(&[first.clone()], &first.id), Some(first.clone()));
    let questions = vec![first.clone(), second.clone()];
    assert_eq!(lookup(&questions, &first.id), None);
    assert_eq!(lookup(&questions, &second.id), None);
    let mut first = first;
    first.title = "`# Pick?`".into();
    second.title = "# Pick?".into();
    assert_eq!(
        lookup(&[first.clone(), second], &first.id),
        None,
        "native headings and literal inline-code headings look identical"
    );
}

#[test]
fn a_retained_free_text_answer_never_receives_a_choice_shortcut() {
    let lines: Vec<String> = editor("Pick one?", "main prompt")
        .into_iter()
        .map(|line| line.replace("› 1.", "  1.").replace("  4. Other (write an answer)", "› 4. My unfinished answer"))
        .collect();
    let shown = prompt::parse(&lines).unwrap();
    assert!(shown.keys(0, None, 0).is_err());
    let mut io = Fake { screens: [(lines, 5)].into(), keys: vec![] };
    assert!(find(&mut io, 1, &asked(), true).is_err());
    assert!(io.keys.is_empty());
}

#[test]
fn markdown_question_titles_match_the_native_rendered_text() {
    for (source, visible) in [
        ("Should `:cd` start a new chat?", "Should :cd start a new chat?"),
        ("Keep **the current conversation**?", "Keep the current conversation?"),
        ("Open [Resume](https://example.com/resume)?", "Open Resume?"),
    ] {
        let shown = prompt::parse(&editor(visible, "main prompt")).unwrap();
        let mut question = asked();
        question.title = source.into();
        assert!(matches(&shown, &question), "{source} did not match {visible}");
        question.title = format!("Do not {source}");
        assert!(!matches(&shown, &question));
    }
}
