use super::*;

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

#[test]
fn working_indicator_wins_over_prompts() {
    let s = screen_signals(&lines("✻ Thinking… (12s · esc to interrupt)\nDo you want to proceed?"));
    assert!(s.working);
    assert_eq!(s.prompt, None);
}

#[test]
fn a_short_screen_in_a_tall_terminal_is_read_where_its_text_ends() {
    // A fresh session draws at the top: below its last line the rows are
    // blank. The phone's device check saw no turn end for 4 s of `esc to
    // interrupt` on row 3 of a 50-row screen, so no "finished" alert came.
    let mut screen = lines("ready\n> you said hello\nthinking (esc to interrupt)");
    screen.resize(50, String::new());
    assert!(screen_signals(&screen).working);
    let mut asking = lines("Bash command\n  rm -rf build\nDo you want to proceed?\n❯ 1. Yes\n  2. No");
    asking.resize(50, String::new());
    assert_eq!(screen_signals(&asking).prompt, Some("permission"));
    // A full screen still reads only its last rows.
    let mut scrolled = lines("thinking (esc to interrupt)");
    scrolled.extend((0..40).map(|i| format!("line {i}")));
    assert!(!screen_signals(&scrolled).working);
}

#[test]
fn approval_prompts_are_detected() {
    let claude =
        screen_signals(&lines("╭──────╮\n│ Bash command │\n│ Do you want to proceed? │\n│ ❯ 1. Yes │\n│   2. No │"));
    assert_eq!(claude.prompt, Some("permission"));
    let codex = screen_signals(&lines("Would you like to run the following command?\n  $ cargo test"));
    assert_eq!(codex.prompt, Some("command approval"));
    let question =
        screen_signals(&lines("Which database?\n❯ 1. Postgres\n  2. SQLite\nEnter to select · ↑/↓ to navigate"));
    assert_eq!(question.prompt, Some("question"));
}

#[test]
fn opencode_shows_when_it_works_and_when_it_asks() {
    // opencode 2.1.4 draws `esc interrupt` under its prompt while a turn runs,
    // and `esc again to interrupt` after one Esc.
    let prompt = "┃  Build · Flint 40B Free OpenCode\n╹▀▀▀▀▀▀▀▀▀▀\n";
    for hint in ["⠋ esc interrupt", "⠙ esc again to interrupt"] {
        assert!(screen_signals(&lines(&format!("{prompt}{hint}   ctrl+p commands"))).working, "{hint}");
    }
    let idle = screen_signals(&lines(&format!("{prompt}/srv/repo  shift+tab agents  ctrl+p commands")));
    assert_eq!(idle, ScreenSignals::default());
    let asking = screen_signals(&lines("△ Permission required\n  $ cargo test\n Allow once   Always allow   Reject\n"));
    assert_eq!(asking.prompt, Some("permission"));
}

#[test]
fn numbered_choice_without_known_phrase_is_a_choice() {
    let s = screen_signals(&lines("Pick one\n│ ❯ 1. First\n│   2. Second"));
    assert_eq!(s.prompt, Some("choice"));
    let plain_list = screen_signals(&lines("Steps:\n1. build\n2. test"));
    assert_eq!(plain_list.prompt, None);
}

/// A Codex 0.166 screen whose agent asked with `request_user_input_async`
/// while its turn runs on: the collapsed summary under "Queued follow-up
/// inputs" (`bottom_pane/questions.rs`), here as the owner's 2026-10-31
/// screenshot showed it.
fn codex_queued_question(summary: &str, hint: &str) -> Vec<String> {
    lines(&format!(
        "• The selected recovery copy is retained.\n\n\
• Working (20h 11m 42s • esc to interrupt) · 1 background terminal running · /ps to view · /stop to close\n\n\
• Queued follow-up inputs\n{summary}\n{hint}\n\n\
› Ask Codex to do anything\n\n\
  GPT-6.1-Sol xhigh · Authorize Groningen game recovery · weekly 15% left · Pursuing goal (20h 11m)\n\
  ← for agents · ? for shortcuts"
    ))
}

#[test]
fn a_codex_question_queued_behind_its_running_turn_needs_you() {
    // Owner, 2026-10-31, a screenshot of a Codex row still spinning over "? 1
    // question / shift+← to answer" and "0 need you": "when codex has a
    // question, nothing indicates on the agent deck that it has a question".
    // Codex's own snapshot draws the key as ⇧← and counts down beside it.
    for (summary, hint) in [("  ? 1 question", "    shift+← to answer"), ("  ? 2 questions · 15s", "    ⇧← to answer")]
    {
        let screen = codex_queued_question(summary, hint);
        assert!(screen_signals(&screen).working, "{summary}: its turn still runs");
        let mut t = Tracker::new(0, true);
        t.observe_screen(&screen_signals(&screen), 6000);
        assert!(matches!(t.update(true, false, 6000), Some(Notify::NeedsYou(_))), "{summary}");
        assert_eq!(t.status, Status::NeedsYou, "{summary}");
        // Its key can be unbound, which leaves the summary without a hint.
        let mut t = Tracker::new(0, true);
        t.observe_screen(&screen_signals(&codex_queued_question(summary, "")), 6000);
        t.update(true, true, 6000);
        assert_eq!(t.status, Status::NeedsYou, "{summary} without its hint");
    }
    // A queued message alone asks nothing, and neither does text that only
    // quotes the summary further in.
    for (summary, hint) in
        [("  ↳ also run the tests", "    shift+← edit last queued message"), ("    ? 1 question", "")]
    {
        let mut t = Tracker::new(0, true);
        t.observe_screen(&screen_signals(&codex_queued_question(summary, hint)), 6000);
        t.update(true, false, 6000);
        assert_eq!(t.status, Status::Working, "{summary}");
    }
}

#[test]
fn only_the_bottom_thirty_lines_count() {
    // Thirty lines of text below a question scroll it out of account; blank
    // rows below a screen's last text do not (see the tall-terminal test).
    let mut text = vec!["Do you want to proceed?".to_string()];
    text.extend((0..30).map(|i| format!("output {i}")));
    text.extend(std::iter::repeat_n(String::new(), 10));
    assert_eq!(screen_signals(&text).prompt, None);
}

#[test]
fn a_finished_mark_stays_until_the_owner_moves_on() {
    // Owner, 2026-10-27: "when its done make it a green checkmark until i
    // enter a prompt or clear etc". Looking at the session cleared the mark,
    // so the session on screen read "idle" once its turn ended.
    let mut t = Tracker::new(0, true);
    assert_eq!(t.update(true, false, 100), None);
    assert_eq!(t.status, Status::Starting);
    t.observe_screen(&ScreenSignals { working: true, prompt: None, queued_question: false }, 1000);
    assert_eq!(t.update(true, false, 1000), None);
    assert_eq!(t.status, Status::Working);
    t.observe_screen(&ScreenSignals::default(), 3000);
    // Owner, 2026-11-07: "notifications that agents are finished when no
    // agents are finished". A screen gone quiet marks the turn, but only the
    // program's own record of its end is announced.
    assert_eq!(t.update(true, false, 3000), None);
    assert_eq!(t.status, Status::Finished);
    assert_eq!(t.update(true, false, 4000), None);
    assert_eq!(t.status, Status::Finished);
    assert_eq!(t.update(true, true, 5000), None);
    assert_eq!(t.status, Status::Finished, "looking at it keeps the mark");
    t.user_input();
    t.update(true, true, 6000);
    assert_eq!(t.status, Status::Finished, "a prompt still being typed keeps it");
    t.clear_finished();
    assert_eq!(t.update(true, true, 7000), None);
    assert_eq!(t.status, Status::Idle, "a submitted prompt or a new chat clears it");
}

#[test]
fn a_turn_that_ends_on_screen_is_marked_finished_without_a_notification() {
    let mut t = Tracker::new(0, true);
    t.observe_screen(&ScreenSignals { working: true, prompt: None, queued_question: false }, 10);
    t.update(true, true, 10);
    assert_eq!(t.update(true, true, 2000), None, "the owner is watching it");
    assert_eq!(t.status, Status::Finished);
}

#[test]
fn working_hold_smooths_redraw_gaps() {
    let mut t = Tracker::new(0, true);
    t.observe_screen(&ScreenSignals { working: true, prompt: None, queued_question: false }, 1000);
    t.update(true, false, 1000);
    t.observe_screen(&ScreenSignals::default(), 1500);
    assert_eq!(t.update(true, false, 1500), None);
    assert_eq!(t.status, Status::Working);
}

#[test]
fn prompt_notifies_needs_you_and_input_clears_hook_reason() {
    let mut t = Tracker::new(0, true);
    t.hook_needs = Some("permission".into());
    assert_eq!(t.update(true, false, 6000), Some(Notify::NeedsYou("permission".into())));
    assert_eq!(t.status, Status::NeedsYou);
    assert_eq!(t.update(true, false, 7000), None);
    t.user_input();
    t.update(true, false, 8000);
    assert_eq!(t.status, Status::Idle);
}

/// Claude's permission prompt, which its Notification hook reports, can
/// close with no answer from the owner: Claude Code 2.1.316 denies a prompt
/// nobody answers ("the permission prompt timed out") and works on. A
/// session asked at 16:53:20 on 2026-11-02, was denied at 16:55:20 and
/// finished its turn, and the hook's report held it at "needs you" with no
/// question anywhere, refusing every phone message (owner: "when you have a
/// question. it gets lost after more output and doesnt let me send messages
/// after").
#[test]
fn a_hook_question_that_left_the_screen_no_longer_needs_you() {
    let mut t = Tracker::new(0, true);
    let working = ScreenSignals { working: true, prompt: None, queued_question: false };
    let asking = ScreenSignals { working: false, prompt: Some("permission"), queued_question: false };
    t.hook_asks("Claude needs your permission".into());
    // The hook can come before the screen draws its question.
    t.observe_screen(&working, 6000);
    t.update(true, false, 6000);
    assert_eq!(t.status, Status::NeedsYou, "the hook alone holds it until the question is drawn");
    assert!(t.blocks_input());
    t.observe_screen(&asking, 6500);
    t.update(true, false, 6500);
    assert_eq!(t.status, Status::NeedsYou);
    // Claude Code denied it unanswered and works on.
    t.observe_screen(&working, 126_500);
    t.update(true, false, 126_500);
    assert_eq!(t.status, Status::Working, "the question left the screen");
    assert!(!t.blocks_input(), "a message is no answer to a question that is gone");
}

/// A service updating in place carries its trackers over (`handover.rs`);
/// one from a build before the screen check has no record of the question
/// being drawn, and an empty screen ends its stale question too.
#[test]
fn a_handed_over_hook_question_ends_on_a_screen_without_one() {
    let mut old = serde_json::to_value(Tracker::new(0, true)).unwrap();
    old["hook_needs"] = "Claude needs your permission".into();
    old.as_object_mut().unwrap().retain(|k, _| k != "hook_awaits_screen");
    let mut t: Tracker = serde_json::from_value(old).unwrap();
    t.observe_screen(&ScreenSignals::default(), 10_000);
    t.update(true, false, 10_000);
    assert!(!t.blocks_input());
    assert_ne!(t.status, Status::NeedsYou);
}

#[test]
fn focused_prompt_changes_status_without_notifying() {
    let mut t = Tracker::new(0, true);
    t.observe_screen(&ScreenSignals { working: false, prompt: Some("permission"), queued_question: false }, 10);
    assert_eq!(t.update(true, true, 10), None);
    assert_eq!(t.status, Status::NeedsYou);
}

#[test]
fn codex_turn_complete_finishes_without_seen_working() {
    let mut t = Tracker::new(0, true);
    t.turn_complete();
    assert_eq!(t.update(true, false, 500), None, "announced once its hooks are over");
    assert_eq!(t.status, Status::Finished);
    t.observe_screen(&ScreenSignals::default(), 600);
    t.log_busy(Some(false), 700);
    assert_eq!(t.update(true, false, 750), Some(Notify::Finished));
    assert_eq!(t.update(true, false, 1000), None, "once");
}

/// The screen and the log of a program whose turn is over, seen once at `at`.
fn quiet_at(t: &mut Tracker, at: u64) {
    t.observe_screen(&ScreenSignals::default(), at);
    t.log_busy(Some(false), at);
}

#[test]
fn starting_grace_then_idle_and_exit_ends() {
    let mut t = Tracker::new(0, true);
    t.update(true, false, 4999);
    assert_eq!(t.status, Status::Starting);
    t.update(true, false, 5000);
    assert_eq!(t.status, Status::Idle);
    assert_eq!(t.since, 5);
    t.hook_needs = Some("permission".into());
    assert_eq!(t.update(false, false, 9000), None);
    assert_eq!(t.status, Status::Ended);
    assert_eq!(Tracker::new(0, false).status, Status::Ended);
}

#[test]
fn tool_busy_marks_working() {
    let mut t = Tracker::new(0, true);
    t.tool_busy = Some(true);
    t.update(true, false, 100);
    assert_eq!(t.status, Status::Working);
}

#[test]
fn clean_title_drops_spinners_and_program_names() {
    assert_eq!(clean_title("✳ Fix the login bug"), Some("Fix the login bug".into()));
    assert_eq!(clean_title("⠂ Claude Code"), None);
    assert_eq!(clean_title("codex"), None);
    assert_eq!(clean_title("   "), None);
    assert_eq!(clean_title("[agent] review"), Some("[agent] review".into()));
}

#[test]
fn hooked_sessions_notify_finished_only_when_the_program_reports_it() {
    // Owner, 2026-10-26: "it just floods notifications when it shouldnt".
    // Codex hides its working line mid-turn; only its turn-complete hook (or
    // Claude's Stop hook) ends a turn.
    let mut t = Tracker::new(0, true);
    t.finish_hook = true;
    t.observe_screen(&ScreenSignals { working: true, prompt: None, queued_question: false }, 1000);
    assert_eq!(t.update(true, false, 1000), None);
    assert_eq!(t.status, Status::Working);
    for gap in 0..5u64 {
        let at = 3000 + gap * 4000;
        t.observe_screen(&ScreenSignals::default(), at);
        assert_eq!(t.update(true, false, at), None, "a gap in the working line is not a finished turn");
        assert_ne!(t.status, Status::Finished);
        t.observe_screen(&ScreenSignals { working: true, prompt: None, queued_question: false }, at + 1000);
        t.update(true, false, at + 1000);
    }
    t.turn_complete();
    assert_eq!(t.update(true, false, 30_000), None);
    assert_eq!(t.status, Status::Finished);
    t.observe_screen(&ScreenSignals::default(), 30_500);
    t.log_busy(None, 31_000);
    assert_eq!(t.update(true, false, 31_000), Some(Notify::Finished));
    assert_eq!(t.update(true, false, 32_000), None);
}

/// A Claude session whose turn just ended by the Stop hook at 2000 ms; its
/// Stop hooks may still be running, so it is not announced yet.
fn finished_at_2000() -> Tracker {
    let mut t = Tracker::new(0, true);
    t.finish_hook = true;
    t.observe_screen(&ScreenSignals { working: true, prompt: None, queued_question: false }, 1000);
    t.log_busy(Some(true), 1000);
    t.update(true, false, 1000);
    assert_eq!(t.status, Status::Working);
    t.turn_complete();
    assert_eq!(t.update(true, false, 2000), None);
    assert_eq!(t.status, Status::Finished);
    t
}

#[test]
fn a_finished_turn_keeps_its_check_while_its_other_stop_hooks_run() {
    // Owner, 2026-10-31: "i didnt realize your session was even done as it
    // just showed idle, instead of a green checkmark". Claude runs every Stop
    // hook at once: the deck's took 139 ms, this repository's 1.4 s, and
    // until the last ends Claude keeps its spinner and its session file
    // says busy. That is still the finished turn, not a new one.
    let working = ScreenSignals { working: true, prompt: None, queued_question: false };
    let mut t = finished_at_2000();
    t.observe_screen(&working, 2200);
    t.log_busy(Some(true), 2500);
    assert_eq!(t.update(true, false, 2500), None);
    assert_eq!(t.status, Status::Finished, "the turn's own Stop hooks are not a new turn");
    t.observe_screen(&working, 3400);
    t.update(true, false, 3400);
    assert_eq!(t.status, Status::Finished);
    // The hooks end and Claude goes quiet: the turn is announced once, and
    // the check stays until a prompt.
    t.observe_screen(&ScreenSignals::default(), 3600);
    assert_eq!(t.update(true, false, 3600), None, "the log still says busy");
    t.log_busy(Some(false), 4000);
    assert_eq!(t.update(true, false, 4000), Some(Notify::Finished));
    assert_eq!(t.update(true, false, 4900), None);
    assert_eq!(t.status, Status::Finished);
    // Work after it went quiet is a new turn, though the hold had time left.
    t.observe_screen(&working, 5000);
    t.update(true, false, 5000);
    assert_eq!(t.status, Status::Working);
}

#[test]
fn work_that_outlasts_the_finished_turn_or_follows_a_prompt_is_a_new_turn() {
    let working = ScreenSignals { working: true, prompt: None, queued_question: false };
    // A Stop hook that sends Claude on (decision "block") never goes quiet:
    // within 15 seconds the session shows working again, and its report of
    // an end was no finish to announce (owner, 2026-11-07: "notifications
    // that agents are finished when no agents are finished").
    let mut t = finished_at_2000();
    for at in (3000..=17_000).step_by(250) {
        if at % 500 == 0 {
            t.observe_screen(&working, at);
        }
        if at % 1000 == 0 {
            t.log_busy(Some(true), at);
        }
        assert_eq!(t.update(true, false, at), None, "at {at} ms");
    }
    assert_eq!(t.status, Status::Working);
    // A submitted prompt is the owner moving on: its turn shows at once.
    let mut t = finished_at_2000();
    t.clear_finished();
    t.observe_screen(&working, 2200);
    t.update(true, false, 2200);
    assert_eq!(t.status, Status::Working);
}

#[test]
fn the_same_question_is_not_announced_again_within_half_a_minute() {
    let mut t = Tracker::new(0, true);
    t.hook_needs = Some("permission".into());
    assert_eq!(t.update(true, false, 6000), Some(Notify::NeedsYou("permission".into())));
    t.user_input();
    t.update(true, false, 7000);
    t.hook_needs = Some("permission".into());
    assert_eq!(t.update(true, false, 9000), None, "the same reason again within 30 s is not announced");
    assert_eq!(t.status, Status::NeedsYou, "the sidebar still shows it");
    t.user_input();
    t.update(true, false, 10_000);
    t.hook_needs = Some("plan approval".into());
    assert_eq!(t.update(true, false, 11_000), Some(Notify::NeedsYou("plan approval".into())), "a new question");
    t.user_input();
    t.update(true, false, 12_000);
    t.hook_needs = Some("plan approval".into());
    assert_eq!(t.update(true, false, 41_500), Some(Notify::NeedsYou("plan approval".into())), "30 s later");
}

/// Owner, 2026-11-07: "keep getting android notifications that agents are
/// finished when no agents are finished and i cant even message them".
/// This repository's Stop hooks send Claude on while its work is not done;
/// the deck's own Stop hook reported each of those stops as a finish.
#[test]
fn only_the_stop_no_hook_sends_on_is_announced() {
    let working = ScreenSignals { working: true, prompt: None, queued_question: false };
    let mut t = finished_at_2000();
    let mut told = Vec::new();
    // Sent on twice, each time past the hold, then done.
    for stop in [2_000u64, 30_000, 60_000] {
        if stop > 2_000 {
            t.turn_complete();
            told.extend(t.update(true, false, stop));
        }
        for at in (stop + 500..stop + 20_000).step_by(500) {
            t.observe_screen(&working, at);
            t.log_busy(Some(true), at);
            told.extend(t.update(true, false, at));
        }
    }
    assert_eq!(t.status, Status::Working);
    assert!(told.is_empty(), "{told:?}");
    t.turn_complete();
    told.extend(t.update(true, false, 90_000));
    t.observe_screen(&working, 90_200);
    t.log_busy(Some(true), 90_300);
    told.extend(t.update(true, false, 90_300));
    quiet_at(&mut t, 91_500);
    told.extend(t.update(true, false, 91_500));
    told.extend(t.update(true, false, 92_000));
    assert_eq!(told, [Notify::Finished]);
    assert_eq!(t.status, Status::Finished);

    // Sent on within the hold, it finishes again before the hold is over:
    // one finished stretch, one announcement.
    let mut t = finished_at_2000();
    t.observe_screen(&working, 3_000);
    t.log_busy(Some(true), 3_000);
    assert_eq!(t.update(true, false, 3_000), None);
    t.turn_complete();
    assert_eq!(t.update(true, false, 9_000), None);
    quiet_at(&mut t, 10_000);
    assert_eq!(t.update(true, false, 10_000), Some(Notify::Finished));
}

/// The monitor reads a log before a Stop hook arrives and reports after:
/// that report must not undo the finish (in the candidate gate, a busy
/// report within 250 ms of the hook cleared its evidence).
#[test]
fn a_busy_report_older_than_the_stop_hook_does_not_swallow_its_finish() {
    let mut t = Tracker::new(0, true);
    t.finish_hook = true;
    t.log_busy(Some(true), 1_000);
    t.update(true, false, 1_000);
    t.turn_complete();
    t.log_busy(Some(true), 1_100);
    assert_eq!(t.update(true, false, 1_250), None);
    assert_eq!(t.status, Status::Finished);
    quiet_at(&mut t, 2_000);
    assert_eq!(t.update(true, false, 2_000), Some(Notify::Finished));
}

#[test]
fn a_finish_is_announced_once_and_never_after_the_owner_saw_it() {
    // A second report of the same stop.
    let mut t = finished_at_2000();
    quiet_at(&mut t, 3_000);
    assert_eq!(t.update(true, false, 3_000), Some(Notify::Finished));
    t.turn_complete();
    assert_eq!(t.update(true, false, 3_100), None);
    quiet_at(&mut t, 4_000);
    assert_eq!(t.update(true, false, 4_000), None);
    // Watched when its hooks were done: not announced when the owner looks away.
    let mut t = finished_at_2000();
    quiet_at(&mut t, 3_000);
    assert_eq!(t.update(true, true, 3_000), None);
    assert_eq!(t.update(true, false, 4_000), None);
    assert_eq!(t.status, Status::Finished);
    // The next turn is a finish of its own.
    t.clear_finished();
    t.log_busy(Some(true), 5_000);
    t.update(true, false, 5_000);
    assert_eq!(t.status, Status::Working);
    t.turn_complete();
    t.update(true, false, 6_000);
    quiet_at(&mut t, 7_000);
    assert_eq!(t.update(true, false, 7_000), Some(Notify::Finished));
}

#[test]
fn a_finish_still_to_announce_survives_an_update_in_place() {
    let t = finished_at_2000();
    let mut t: Tracker = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
    quiet_at(&mut t, 3_000);
    assert_eq!(t.update(true, false, 3_000), Some(Notify::Finished));
    // A tracker from a build before it has nothing to announce.
    let mut old = serde_json::to_value(finished_at_2000()).unwrap();
    old.as_object_mut().unwrap().retain(|k, _| k != "finish_unannounced" && k != "finish_announced");
    let mut t: Tracker = serde_json::from_value(old).unwrap();
    quiet_at(&mut t, 3_000);
    assert_eq!(t.update(true, false, 3_000), None);
}

fn capacity() -> Vec<String> {
    vec!["Selected model is at capacity".to_string()]
}

fn codex_screen(composer: &str) -> Vec<String> {
    lines(&format!(
        "› finish sample session1 plan.md\n\n• Edited 2 files (+47 -11)\n\n• Interacted with `/root/artifact_retention`\n\n\
■ Selected model is at capacity. Please try a different model.\n\n  1 background terminal running · /ps to view · /stop to close\n\n\
{composer}\n  GPT-6.1-Sol xhigh · Finish Sample session1 plan.md · weekly 97% left"
    ))
}

#[test]
fn a_capacity_error_ending_the_transcript_waits_for_continue() {
    // Owner, 2026-10-26: "can you make it so it automatically sends
    // contontinue if this codex error appears".
    let screen = codex_screen("›");
    let row = screen.iter().position(|l| l.starts_with('›') && l.len() <= 4 && l.trim() == "›").unwrap();
    assert_eq!(stopped_on_error(&screen, Some((2, row)), &capacity()), Some("Selected model is at capacity"));
    // The placeholder Codex draws after the cursor is not typed text.
    let placeholder = codex_screen("› Ask Codex to do anything");
    assert_eq!(stopped_on_error(&placeholder, Some((2, row)), &capacity()), Some("Selected model is at capacity"));
    // Text the owner typed stays theirs.
    let typed = codex_screen("› continue");
    assert_eq!(stopped_on_error(&typed, Some((10, row)), &capacity()), None);
    // No cursor, or the cursor outside the input box (a popup): leave it.
    assert_eq!(stopped_on_error(&screen, None, &capacity()), None);
    assert_eq!(stopped_on_error(&screen, Some((0, 2)), &capacity()), None);
    // Disabled when no error text is configured.
    assert_eq!(stopped_on_error(&screen, Some((2, row)), &[]), None);
}

#[test]
fn a_capacity_error_followed_by_a_message_or_more_work_is_over() {
    let mut sent = lines(
        "■ Selected model is at capacity. Please try a different model.\n\n› continue\n\n• Working (3s · esc to interrupt)\n\n›",
    );
    let row = sent.len() - 1;
    assert_eq!(stopped_on_error(&sent, Some((2, row)), &capacity()), None, "continue was already sent");
    sent = lines(
        "■ Selected model is at capacity. Please try a different model.\n\n• Ran cargo test\n  └ ok\n\n• Edited a.rs\n\n• Explored\n  └ Read b.rs\n\n›",
    );
    let row = sent.len() - 1;
    assert_eq!(stopped_on_error(&sent, Some((2, row)), &capacity()), None, "the agent carried on after it");
}

#[test]
fn only_codexs_own_error_line_gets_continue_not_text_that_quotes_it() {
    // An agent's message can quote the error from a log, a page or a file;
    // typing "continue" then would act on text the agent was shown. Codex
    // draws its own error at the left edge behind `■`, and a message's lines
    // start with `•` or are indented under it.
    for quoted in [
        "• The build log says: Selected model is at capacity. Please try a different model.",
        "• From the page:\n  ■ Selected model is at capacity. Please try a different model.",
        "Selected model is at capacity. Please try a different model.",
    ] {
        let screen = lines(&format!("› read the log\n\n{quoted}\n\n›"));
        let row = screen.len() - 1;
        assert_eq!(stopped_on_error(&screen, Some((2, row)), &capacity()), None, "{quoted}");
    }
}

/// Codex 0.164's start screen in a tall terminal: no box, its folder and
/// permissions, a drawing, then the composer (a resumed session, 2026-10-26).
fn codex_splash() -> Vec<String> {
    lines(
        "  >_ OpenAI Codex (v0.164.0)\n     ~/projects/example/app\n  permissions: YOLO mode\n\
                                                   ⣀⣤⣤⣤⣀⣀\n                                                ⣠⣶⣿⣿⣿⣿\n\n\
› Ask Codex to do anything\n  GPT-6.1-Sol xhigh · weekly 68% left\n  ← for agents · ? for shortcuts",
    )
}

#[test]
fn codexs_start_screen_is_a_new_chat_in_either_form() {
    let splash = codex_splash();
    assert_eq!(codex_new_chat(&splash, Some((2, 6))), Some(true));
    let boxed = lines(
        "╭──────╮\n│ >_ OpenAI Codex (v0.164.0) │\n│ │\n│ model:     gpt-5.5 │\n│ directory: ~/w │\n╰──────╯\n\n› ",
    );
    assert_eq!(codex_new_chat(&boxed, Some((2, 7))), Some(true));
    let chat = lines("  >_ OpenAI Codex (v0.164.0)\n  permissions: YOLO mode\n\n› fix it\n\n• Fixed.\n\n› ");
    assert_eq!(codex_new_chat(&chat, Some((2, 7))), Some(false));
}

#[test]
fn codexs_status_line_names_its_thread() {
    // Codex #4's screen, 2026-10-26: the one place an attached Codex shows
    // which thread it is in.
    let mut screen = codex_splash();
    screen[7] = "  GPT-6.1-Sol xhigh · Merge everything to master · weekly 64% left · Main [default]".into();
    let items = [
        "GPT-6.1-Sol xhigh",
        "Merge everything to master",
        "weekly 64% left",
        "Main [default]",
        "← for agents",
        "? for shortcuts",
    ];
    assert_eq!(codex_footer(&screen, Some((2, 6))), items);
    assert!(codex_footer(&screen, Some((0, 7))).is_empty(), "the cursor is not in the composer");
    assert_eq!(codex_footer(&screen, None), items, "Codex hides its cursor while working");
}

#[test]
fn a_log_that_ended_its_turn_long_ago_does_not_let_a_screen_gap_finish_one() {
    // Owner, 2026-10-26: "notifications still fire when agents are not done".
    // Codex #4's log was a thread whose turn had ended hours before; each
    // gap in the working line on screen was announced as finished.
    let working = ScreenSignals { working: true, prompt: None, queued_question: false };
    let mut t = Tracker::new(0, true);
    t.log_busy(Some(false), 1_000);
    t.observe_screen(&working, 60_000);
    t.update(true, false, 60_000);
    assert_eq!(t.status, Status::Working);
    t.observe_screen(&ScreenSignals::default(), 65_000);
    assert_eq!(t.update(true, false, 65_000), None, "the working line hidden for five seconds");
    assert_eq!(t.status, Status::Working);
    assert_eq!(t.update(true, false, 91_000), None, "a screen gone quiet is no announced finish");
    assert_eq!(t.status, Status::Finished, "gone for half a minute, it is over");
    // A log that sees the turn start and end finishes it at once.
    let mut t = Tracker::new(0, true);
    t.log_busy(Some(false), 1_000);
    t.observe_screen(&working, 60_000);
    t.log_busy(Some(true), 61_000);
    t.update(true, false, 61_000);
    t.observe_screen(&ScreenSignals::default(), 70_000);
    t.log_busy(Some(false), 70_000);
    t.update(true, false, 71_600);
    assert_eq!(t.status, Status::Finished);
    // Just after the log says a turn ended the screen may lag: the usual hold.
    let mut t = Tracker::new(0, true);
    t.log_busy(Some(false), 100_000);
    t.observe_screen(&working, 101_000);
    t.update(true, false, 101_000);
    t.observe_screen(&ScreenSignals::default(), 102_000);
    t.update(true, false, 103_000);
    assert_eq!(t.status, Status::Finished);
}

#[test]
fn codexs_composer_text_is_read_at_the_cursor() {
    let mut screen = codex_splash();
    screen[6] = "› /clear".into();
    assert_eq!(codex_composer(&screen, Some((8, 6))).as_deref(), Some("/clear"));
    assert_eq!(codex_composer(&screen, Some((0, 7))), None, "the cursor is not in the composer");
    assert_eq!(codex_composer(&screen, None), None);
}

/// Claude Code 2.1.317 screens from a live session, the box 120 columns wide.
fn claude_box(rows: &str) -> Vec<String> {
    let rule = "─".repeat(120);
    lines(&format!(
        "❯ an earlier prompt\n\n● PONG\n\n{rule}\n{rows}\n{rule}\n  Haiku 5.5 · wk 40%\n  ⏵⏵ bypass permissions on (shift+tab to cycle)"
    ))
}

#[test]
fn claudes_input_box_is_read_below_its_rule() {
    assert_eq!(claude_composer(&claude_box("❯")).as_deref(), Some(""));
    assert_eq!(
        claude_composer(&claude_box("❯ [Image #1]Please inspect this")).as_deref(),
        Some("[Image #1]Please inspect this")
    );
    assert_eq!(claude_composer(&claude_box("❯ first line\n  second line")).as_deref(), Some("first line\nsecond line"));
    // The slash menu sits above the box; its `❯` is not the input.
    let mut menu = claude_box("❯ /clear");
    menu.insert(4, "  ❯ /clear                                          Start a new session with empty context".into());
    assert_eq!(claude_composer(&menu).as_deref(), Some("/clear"));
    // A transcript prompt with no box below it is not the input.
    assert_eq!(claude_composer(&lines("❯ an earlier prompt\n\n● PONG")), None);
}

#[test]
fn an_agents_own_update_notice_is_told_from_a_conversation_quoting_it() {
    use crate::protocol::Tool::{Claude, Codex};
    // Claude Code 2.1.323 writes it at the right of the footer under its box.
    let mut claude = claude_box("❯");
    *claude.last_mut().unwrap() += "                 Update available! Run: mise upgrade claude";
    assert!(update_notice(Claude, &claude));
    assert!(!update_notice(Claude, &claude_box("❯")));
    let typed = claude_box("❯ why does it say Update available! Run: mise upgrade claude");
    assert!(!update_notice(Claude, &typed), "typed into the box");
    let mut answer = claude_box("❯");
    answer[2] = "● The footer says Update available! Run: mise upgrade claude".into();
    assert!(!update_notice(Claude, &answer), "the conversation above the box");
    // Codex 0.167's card at the left edge above its start screen.
    let mut codex = lines(
        "╭────────────────────────────────────────────╮\n\
         │ ✨\u{200a}Update available! 0.167.1 -> 0.167.2 │\n\
         │ See https://github.com/openai/codex for installation options. │\n\
         ╰────────────────────────────────────────────╯\n",
    );
    codex.extend(codex_splash());
    assert!(update_notice(Codex, &codex));
    assert!(!update_notice(Codex, &codex_splash()));
    let quoted = lines("› what was that?\n\n• Codex showed:\n  │ ✨ Update available! 0.167.1 -> 0.167.2 │");
    assert!(!update_notice(Codex, &quoted), "an answer quoting the card");
}

#[test]
fn working_codex_footer_is_read_with_hidden_cursor() {
    let screen = lines("› add Morphys games\n\n• Working (esc to interrupt)\n\n› Ask Codex to do anything\n\n  GPT-6.1-Sol default · Add Morphy collection · weekly 88% left\n  ← for agents · ? for shortcuts");
    let items = codex_footer(&screen, None);
    assert!(items.iter().any(|item| item == "Add Morphy collection"));
    let transcript = lines("› explain this\n\n• GPT-6.1-Sol default · Other conversation · weekly 88% left");
    assert!(codex_footer(&transcript, None).is_empty());
    let overlay =
        lines("› earlier prompt\n\n  GPT-6.1-Sol · Earlier task\n\nSelect a model\n  GPT-6.1-Sol\n  Enter to confirm");
    assert!(codex_footer(&overlay, None).is_empty());
}

#[test]
fn async_question_editor_blocks_ordinary_input_even_while_working() {
    let lines = [
        "Working (esc to interrupt)",
        "Pick one?",
        "› 1. First",
        "  2. Second",
        "  3. Other (write an answer)",
        "enter submit   ⌃] skip   ⇧→ main prompt",
    ]
    .map(String::from);
    let signals = screen_signals(&lines);
    assert_eq!(signals.prompt, Some("async question"));
    assert!(signals.working);
}

#[test]
fn queue_turn_receipt_advances_within_one_second_and_survives_handover() {
    let mut t = Tracker::new(0, true);
    t.finish_hook = true;
    t.turn_complete();
    t.update(true, false, 1100);
    let old = t.completed_turns;
    let since = t.since;
    t.clear_finished();
    t.turn_complete();
    t.update(true, false, 1200);
    assert_eq!(t.since, since, "second-resolution display time cannot be a completion fence");
    assert_eq!(t.completed_turns, old + 1);
    let restored: Tracker = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
    assert_eq!(restored.completed_turns, t.completed_turns);
    t.update(true, false, 1300);
    assert_eq!(t.completed_turns, restored.completed_turns, "polling does not complete another turn");
}

#[test]
fn persistent_connection_returns_to_ready_not_finished() {
    // Dot is a persistent connection: a turn ending is not a completed task
    // (owner: Dot "is not something that finishes").
    for (persistent, expected, alerts) in [(true, Status::Idle, 0), (false, Status::Finished, 1)] {
        let mut t = Tracker::new(0, true);
        t.persistent = persistent;
        t.tool_busy = Some(true);
        assert_eq!(t.update(true, false, 1_000), None);
        assert_eq!(t.status, Status::Working);
        t.turn_complete();
        let mut notes = Vec::new();
        for at in (60_000..70_000).step_by(250) {
            t.observe_screen(&ScreenSignals::default(), at);
            t.log_busy(Some(false), at);
            notes.extend(t.update(true, false, at));
        }
        assert_eq!(t.status, expected);
        assert_eq!(notes, vec![Notify::Finished; alerts]);
    }
}

/// Quiet screen and log observations from `from` to `to` ms, every status
/// tick; the notifications the session gave meanwhile.
fn quiet(t: &mut Tracker, from: u64, to: u64) -> Vec<Notify> {
    let mut notes = Vec::new();
    for at in (from..to).step_by(250) {
        t.observe_screen(&ScreenSignals::default(), at);
        t.log_busy(Some(false), at);
        notes.extend(t.update(true, false, at));
    }
    notes
}

#[test]
fn a_turn_end_reported_twice_or_late_is_announced_once() {
    let working = ScreenSignals { working: true, prompt: None, queued_question: false };
    let mut t = Tracker::new(0, true);
    t.finish_hook = true;
    t.observe_screen(&working, 1000);
    t.update(true, false, 1000);
    // The hook delivered twice, at once and again after the announcement.
    t.turn_complete();
    t.turn_complete();
    assert_eq!(quiet(&mut t, 2000, 4000), vec![Notify::Finished]);
    t.turn_complete();
    assert_eq!(quiet(&mut t, 4000, 6000), vec![], "the same end again");
    // The owner moved on and the old end is reported late: still a turn for
    // the work queue, but no news.
    t.clear_finished();
    let turns = t.completed_turns;
    t.turn_complete();
    assert_eq!(quiet(&mut t, 6000, 8000), vec![]);
    assert_eq!(t.completed_turns, turns + 1);
    // Work seen since makes the next report a new end.
    t.observe_screen(&working, 9000);
    t.update(true, false, 9000);
    assert_eq!(t.status, Status::Working);
    t.turn_complete();
    assert_eq!(quiet(&mut t, 10_000, 12_000), vec![Notify::Finished]);
}

/// The service applies a Stop hook at once but reads screens only every
/// `SCREEN_SCAN_INTERVAL`, so a turn that ends just after the owner answers
/// its question is reported while the last read still shows that question.
/// That report was dropped: the session went from "needs you" to idle, its
/// turn neither counted for the work queue nor announced (the attention
/// delivery test saw needs_you, then idle 217 ms after the Stop hook,
/// 2026-11-09).
#[test]
fn a_turn_end_reported_while_the_last_screen_read_still_asks_is_kept() {
    let asking = ScreenSignals { working: false, prompt: Some("question"), queued_question: false };
    let mut t = Tracker::new(0, true);
    t.finish_hook = true;
    t.observe_screen(&asking, 6000);
    t.update(true, false, 6000);
    assert_eq!(t.status, Status::NeedsYou);
    let turns = t.completed_turns;
    t.user_input();
    t.turn_complete();
    assert_eq!(t.update(true, false, 6200), None);
    assert_eq!(t.status, Status::NeedsYou, "a question on screen comes first");
    assert_eq!(quiet(&mut t, 6500, 8000), vec![Notify::Finished], "the next read shows the question gone");
    assert_eq!(t.status, Status::Finished);
    assert_eq!(t.completed_turns, turns + 1);
}

#[test]
fn a_program_without_a_hook_announces_each_turn_its_own_record_ends() {
    // Grok, opencode, Antigravity and an attached Codex report no turn end;
    // their own record of each completed turn is the evidence.
    let working = ScreenSignals { working: true, prompt: None, queued_question: false };
    let mut t = Tracker::new(10_000, true);
    // The record as it stood when the session started is no finish.
    t.turn_recorded("old", 5_000, false);
    assert_eq!(quiet(&mut t, 10_000, 12_000), vec![]);
    assert_ne!(t.status, Status::Finished);
    t.observe_screen(&working, 12_000);
    t.update(true, false, 12_000);
    assert_eq!(t.status, Status::Working);
    // The screen goes quiet before the record shows the end: marked, not
    // announced, until the record shows it.
    assert_eq!(quiet(&mut t, 14_000, 16_000), vec![]);
    assert_eq!(t.status, Status::Finished);
    t.turn_recorded("first", 15_900, false);
    assert_eq!(quiet(&mut t, 16_000, 18_000), vec![Notify::Finished]);
    // Read again every monitor cycle, after a reconnect or a resync: the
    // same end.
    for _ in 0..3 {
        t.turn_recorded("first", 15_900, false);
    }
    assert_eq!(quiet(&mut t, 18_000, 20_000), vec![]);
    // An end the record shows a newer turn begun after is no finish.
    t.observe_screen(&working, 20_000);
    t.update(true, false, 20_000);
    t.turn_recorded("second", 21_000, true);
    t.observe_screen(&working, 21_500);
    assert_eq!(t.update(true, false, 21_500), None);
    assert_eq!(t.status, Status::Working);
    // Handed over to the next build, it knows the end it saw.
    let mut t: Tracker = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
    t.turn_recorded("second", 21_000, true);
    t.turn_recorded("third", 30_000, false);
    assert_eq!(quiet(&mut t, 30_000, 32_000), vec![Notify::Finished]);
}

#[test]
fn the_first_record_read_is_a_finish_only_if_it_ended_in_this_chat() {
    // A new chat's first turn ends before the monitor first reads its record.
    let mut t = Tracker::new(10_000, true);
    t.turn_recorded("first", 15_000, false);
    assert_eq!(quiet(&mut t, 15_000, 17_000), vec![Notify::Finished]);
    // A tracker handed over from a build without the record takes it as it
    // stands: no replay of the end it last showed.
    let mut old = serde_json::to_value(Tracker::new(10_000, true)).unwrap();
    old.as_object_mut().unwrap().retain(|k, _| k != "turn_end" && k != "turn_end_since_ms");
    let mut t: Tracker = serde_json::from_value(old).unwrap();
    t.turn_recorded("before the update", 60_000, false);
    assert_eq!(quiet(&mut t, 60_000, 62_000), vec![]);
    t.turn_recorded("after it", 70_000, false);
    assert_eq!(quiet(&mut t, 70_000, 72_000), vec![Notify::Finished]);
    // A program whose hook reports its turns is not read for them.
    let mut t = Tracker::new(10_000, true);
    t.finish_hook = true;
    t.turn_recorded("first", 15_000, false);
    assert_eq!(quiet(&mut t, 15_000, 17_000), vec![]);
}
