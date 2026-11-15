use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ratatui::text::Line;

use super::*;
use crate::edit::Draft;

/// The most questions that wait while one is answered.
const QUEUE_LEN: usize = 10;
/// An answer being written is redrawn at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(50);

/// The question being answered: it, who answers, since when, what has come
/// so far and the tool its program is using, and how to stop it.
pub struct Asking {
    pub question: String,
    pub who: String,
    pub since: Instant,
    pub text: String,
    pub activity: Option<String>,
    /// The conversation it continues, by its program's id.
    pub continues: Option<String>,
    cancel: Arc<AtomicBool>,
    /// `/clear` stopped it: its answer is not kept.
    pub discard: bool,
}

/// The answers drawn as Markdown at one width, for the history as it was at
/// `generation`, and the overlay's rows from the last frame.
pub struct AskRendered {
    pub generation: u64,
    pub width: usize,
    pub theme: crate::theme::Theme,
    pub answers: Vec<Vec<Line<'static>>>,
    /// Each row, with the history entry it belongs to.
    pub rows: Vec<(Option<usize>, Line<'static>)>,
}

impl App {
    /// F4 and `:ask`: the quick-question overlay; with a question, ask it.
    pub fn open_ask(&mut self, question: &str) {
        if !self.cfg.plugin_enabled("quick-ask") {
            self.local_error("Quick questions plugin is disabled");
            return;
        }
        if !self.ask_loaded {
            self.ask_loaded = true;
            if let Some(dir) = &self.ask_dir {
                self.ask_history = ask::load_history(dir);
                self.ask_generation += 1;
            }
            // A follow-up continues the conversation the history ends with.
            self.ask_thread = self.ask_history.last().and_then(|asked| asked.thread.clone());
        }
        self.dialog = Some(Dialog::Ask { draft: Draft::default(), route: None, scroll: 0 });
        if !question.trim().is_empty() {
            if let Err(error) = self.ask_input(question, None) {
                self.set_notice(error, true);
            }
        }
    }

    /// Enter in the box: a question, asked now or after the one being
    /// answered; `/clear`, which starts over; or a command that is refused.
    pub(crate) fn ask_input(&mut self, input: &str, route: Option<ask::Route>) -> Result<(), String> {
        match ask::typed(input) {
            ask::Typed::Unknown(word) => return Err(ask::unknown_command(word)),
            ask::Typed::Clear(rest) => {
                self.clear_ask();
                return if rest.is_empty() { Ok(()) } else { self.ask_input(rest, route) };
            }
            ask::Typed::Question => {}
        }
        if self.asking.is_some() {
            if self.ask_queue.len() >= QUEUE_LEN {
                return Err(format!("{QUEUE_LEN} questions are waiting already."));
            }
            self.ask_queue.push_back((input.trim().to_string(), route));
            return Ok(());
        }
        self.ask_question(input, route)
    }

    /// Ask on a thread of its own; the answer streams back as events.
    fn ask_question(&mut self, input: &str, route: Option<ask::Route>) -> Result<(), String> {
        let start_dir = PathBuf::from(&self.state.start_dir);
        let state_dir = self.ask_dir.clone().unwrap_or_else(paths::state_dir);
        let plan = ask::plan(&self.cfg.ask, input, route, self.ask_thread.as_ref(), &start_dir, &state_dir)
            .and_then(|plan| plan.argv(&self.cfg).map(|_| plan))
            .map_err(|e| format!("Could not ask: {e:#}"))?;
        let Some(link) = &self.link else {
            return Err("This deck cannot ask questions: it has no event channel.".into());
        };
        let tx = link.tx.clone();
        let config = self.cfg.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let (question, who, continues) = (plan.question.clone(), plan.who(), plan.resume.clone());
        let asked = (question.clone(), who.clone());
        let run = move || {
            let mut shown: Option<Instant> = None;
            let reply = ask::answer(&config, &plan, &stop, &mut |said| {
                if shown.is_some_and(|at| at.elapsed() < PROGRESS_EVERY) {
                    return;
                }
                shown = Some(Instant::now());
                let _ =
                    tx.send(AppEv::AskProgress { text: said.answer().to_string(), activity: said.activity.clone() });
            });
            let (question, who) = asked;
            let asked = match reply {
                Ok(reply) => ask::Asked {
                    question,
                    who,
                    thread: reply.conversation.map(|id| plan.thread(id)).or_else(|| plan.continued()),
                    answer: reply.text,
                    stopped: reply.stopped,
                    error: false,
                    at: util::now_secs(),
                },
                Err(e) => ask::Asked {
                    question,
                    who,
                    answer: format!("{e:#}"),
                    error: true,
                    at: util::now_secs(),
                    thread: plan.continued(),
                    stopped: false,
                },
            };
            let _ = tx.send(AppEv::Answered(asked));
        };
        thread::Builder::new().name("deck-ask".into()).spawn(run).map_err(|e| format!("Could not ask: {e}"))?;
        self.asking = Some(Asking {
            question,
            who,
            since: Instant::now(),
            text: String::new(),
            activity: None,
            continues,
            cancel,
            discard: false,
        });
        Ok(())
    }

    /// The answer so far.
    pub fn ask_progress(&mut self, text: String, activity: Option<String>) {
        if let Some(asking) = &mut self.asking {
            asking.text = text;
            asking.activity = activity;
        }
    }

    /// An answer came back: keep it, newest last, and show it; then ask the
    /// next question waiting.
    pub fn answered(&mut self, asked: ask::Asked) {
        let discard = self.asking.take().is_some_and(|asking| asking.discard);
        if !discard {
            self.keep_answer(asked);
        }
        while let Some((input, route)) = self.ask_queue.pop_front() {
            match self.ask_input(&input, route) {
                Ok(()) => break,
                Err(error) => self.set_notice(error, true),
            }
        }
    }

    fn keep_answer(&mut self, asked: ask::Asked) {
        if !asked.error {
            self.ask_thread = asked.thread.clone();
        }
        let error = asked.error;
        let short = util::truncate_width(&util::one_line(&asked.answer), 80);
        let persisted = self.ask_dir.as_ref().map(|dir| ask::append_history(dir, &asked));
        match persisted {
            Some(Ok(history)) => self.ask_history = history,
            failure => {
                self.ask_history.push(asked);
                let start = self.ask_history.len().saturating_sub(ask::HISTORY_LEN);
                self.ask_history.drain(..start);
                if let Some(Err(error)) = failure {
                    self.local_error(format!("Could not save question history: {error:#}"));
                }
            }
        }
        self.ask_generation += 1;
        match &mut self.dialog {
            Some(Dialog::Ask { scroll, .. }) => *scroll = 0,
            _ => {
                let key = self.keys.label(Hotkey::Ask).unwrap_or_else(|| ":ask".into());
                self.set_notice(format!("Answer ({key} shows it): {short}"), error);
            }
        }
    }

    /// Ctrl-c: stop the answer being written, keeping what came, and the
    /// questions waiting.
    fn stop_ask(&mut self) {
        self.ask_queue.clear();
        if let Some(asking) = &self.asking {
            asking.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// `/clear` and Ctrl-l: stop, forget the conversation and empty the
    /// history, which every deck and the phone share.
    fn clear_ask(&mut self) {
        self.ask_queue.clear();
        if let Some(asking) = &mut self.asking {
            asking.discard = true;
            asking.cancel.store(true, Ordering::SeqCst);
        }
        self.ask_thread = None;
        self.ask_history.clear();
        self.ask_generation += 1;
        if let Some(dir) = &self.ask_dir {
            if let Err(error) = ask::clear_history(dir) {
                self.local_error(format!("Could not clear question history: {error:#}"));
            }
        }
    }

    /// Ctrl-y: the answer marked while scrolled up, else the last.
    fn copy_answer(&mut self) {
        let last = self.ask_history.len().checked_sub(1);
        let index = self.dialog_viewport.pick.filter(|&i| i < self.ask_history.len()).or(last);
        if let Some(asked) = index.and_then(|i| self.ask_history.get(i)) {
            let text = asked.answer.clone();
            self.handle_server(ServerMsg::Clipboard { text });
            let which = if index == last { "the last" } else { "the marked" };
            self.set_notice(format!("Copied {which} answer."), false);
        }
    }

    /// Questions asked, newest first, for ↑ to bring back.
    fn asked_questions(&self) -> Vec<String> {
        let mut questions: Vec<String> = Vec::new();
        for asked in self.ask_history.iter().rev() {
            if questions.last() != Some(&asked.question) {
                questions.push(asked.question.clone());
            }
        }
        questions
    }

    /// Keys in the quick-question box. PgUp and PgDn, Ctrl-Home and
    /// Ctrl-End and the wheel scroll it first (`navigation.rs`).
    pub(crate) fn ask_key(
        &mut self,
        mut draft: Draft,
        mut route: Option<ask::Route>,
        mut scroll: usize,
        k: KeyEvent,
    ) -> Option<Dialog> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match k.code {
            _ if self.keys.find(&k) == Some(Hotkey::Ask) => return None,
            KeyCode::Esc => return None,
            KeyCode::Enter if !draft.text().trim().is_empty() => match self.ask_input(draft.text(), route) {
                Ok(()) => {
                    draft.take();
                    scroll = 0;
                }
                Err(error) => self.set_notice(error, true),
            },
            KeyCode::Tab => {
                route = match route {
                    None => Some(ask::Route::General),
                    Some(ask::Route::General) => Some(ask::Route::Web),
                    Some(ask::Route::Web) => Some(ask::Route::Code),
                    Some(ask::Route::Code) => None,
                }
            }
            KeyCode::Up => {
                draft.older(&self.asked_questions());
            }
            KeyCode::Down => {
                draft.newer(&self.asked_questions());
            }
            KeyCode::Left if ctrl || alt => draft.word_left(),
            KeyCode::Right if ctrl || alt => draft.word_right(),
            KeyCode::Left => draft.left(),
            KeyCode::Right => draft.right(),
            KeyCode::Home => draft.home(),
            KeyCode::End => draft.end(),
            KeyCode::Backspace if ctrl || alt => draft.delete_word_back(),
            KeyCode::Backspace => draft.backspace(),
            KeyCode::Delete if ctrl || alt => draft.delete_word_forward(),
            KeyCode::Delete => draft.delete(),
            KeyCode::Char(c) if ctrl => match c {
                'a' => draft.home(),
                'e' => draft.end(),
                'b' => draft.left(),
                'f' => draft.right(),
                'w' => draft.delete_word_back(),
                'k' => draft.kill_line(),
                'u' => draft.clear(),
                'c' => self.stop_ask(),
                'l' => {
                    self.clear_ask();
                    scroll = 0;
                }
                'y' => self.copy_answer(),
                _ => {}
            },
            KeyCode::Char(c) if alt => match c {
                'b' => draft.word_left(),
                'f' => draft.word_right(),
                'd' => draft.delete_word_forward(),
                _ => {}
            },
            KeyCode::Char(c) => draft.insert(c.encode_utf8(&mut [0; 4])),
            _ => {}
        }
        Some(Dialog::Ask { draft, route, scroll })
    }
}
