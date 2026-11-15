//! The bundled provider owns native control policy; core keeps its existing
//! authenticated, bounded plugin request/reply channel.
use super::*;
use crate::providers::messages::{self, controls::Control};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    id: u64,
    expected: Option<crate::attention::ExpectedTarget>,
    conversation: String,
    action: String,
    model: Option<String>,
    effort: Option<String>,
    question: Option<String>,
    answer: Option<String>,
}

enum Action {
    Native(Control),
    Question { root: std::path::PathBuf, conversation: String, id: String, answer: String },
}

impl Action {
    fn resolve(self, deadline: Instant, cancel: &AtomicBool) -> Result<Control> {
        let live = || !cancel.load(Ordering::SeqCst) && Instant::now() < deadline;
        anyhow::ensure!(live(), "native control canceled or expired");
        match self {
            Self::Native(control) => Ok(control),
            Self::Question { root, conversation, id, answer } => {
                let path = crate::codex::find_rollout(&root, &conversation)
                    .context("the question transcript is unavailable")?;
                anyhow::ensure!(live(), "native control canceled or expired");
                let asked = crate::remote::chat::unresolved_question(&path, &id, &live);
                anyhow::ensure!(live(), "native control canceled or expired");
                let asked = asked.ok_or_else(|| {
                    crate::protocol::QuestionStale(
                        "this question is already answered or no longer belongs to this conversation".into(),
                    )
                })?;
                Ok(Control::Question { message: crate::remote::questionpick::follow_up_text(&asked, &answer)? })
            }
        }
    }
}

pub(super) fn start(
    service: &mut Service,
    client: u64,
    request: u64,
    plugin: &str,
    command: &str,
    args: &Value,
) -> bool {
    if (plugin, command) != ("provider-codex", "control") {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    let prepared = (|| -> Result<_> {
        let args: Arguments = serde_json::from_value(args.clone())?;
        let session = service.sessions.get(&args.id).context("no such local session")?;
        anyhow::ensure!(
            args.expected.as_ref().is_none_or(|expected| super::provider_target::matches(expected, session)),
            "the native target changed"
        );
        anyhow::ensure!(
            session.tool == Tool::Codex && session.alive && session.closing.is_none(),
            "this Codex session is not running"
        );
        anyhow::ensure!(
            session.conversation.as_deref() == Some(&args.conversation),
            "the session conversation changed"
        );
        anyhow::ensure!(
            session.chat.asked.is_none()
                && session.recovery.phase.is_none()
                && !session.recovery.paused
                && session.recovery.draft_files.is_empty(),
            "the session is changing or recovering"
        );
        anyhow::ensure!(crate::service::valid_conversation_id(&args.conversation), "invalid conversation identity");
        let action = match args.action.as_str() {
            "capabilities"
                if args.model.is_none()
                    && args.effort.is_none()
                    && args.question.is_none()
                    && args.answer.is_none() =>
            {
                Action::Native(Control::Capabilities)
            }
            "settings" if args.question.is_none() && args.answer.is_none() => {
                anyhow::ensure!(args.model.is_some() || args.effort.is_some(), "no model or effort given");
                anyhow::ensure!(
                    args.model.as_ref().is_none_or(|m| crate::service::valid_model_name(m))
                        && args.effort.as_ref().is_none_or(|e| crate::service::valid_effort(e)),
                    "invalid model or effort"
                );
                Action::Native(Control::Settings { model: args.model, effort: args.effort })
            }
            "question_reply" if args.model.is_none() && args.effort.is_none() => {
                let id = args.question.as_deref().context("missing question identity")?;
                let answer = args.answer.as_deref().context("missing question answer")?;
                anyhow::ensure!(
                    !id.is_empty()
                        && id.len() <= 512
                        && !answer.trim().is_empty()
                        && !answer.chars().any(|c| c.is_control() && c != '\n' && c != '\t'),
                    "invalid question answer"
                );
                // Canonical metadata, including questions older than retained chat
                // rows, validates the ID. Visible wording is never an identity.
                let root = session.account_root.as_ref().unwrap_or(&service.opts.codex_dir);
                // Transcript work belongs to the bounded worker, never the
                // service loop that owns hosted terminals.
                Action::Question {
                    root: root.clone(),
                    conversation: args.conversation.clone(),
                    id: id.to_owned(),
                    answer: answer.to_owned(),
                }
            }
            _ => anyhow::bail!("invalid native control action or fields"),
        };
        let cwd = session.cwd.as_deref().unwrap_or(&service.opts.start_dir);
        let root = session.account_root.as_ref().unwrap_or(&service.opts.codex_dir);
        let native = messages::request(Tool::Codex, &service.opts.config, &args.conversation, "", cwd, root);
        let pid = session.hosted.as_ref().and_then(|h| h.pid);
        Ok(native
            .zip(pid)
            .filter(|(native, pid)| native.matches_runtime(*pid))
            .map(|(native, pid)| (native, pid, action, args.id)))
    })();
    match prepared {
        Err(error) => service.plugin_reply(client, request, Value::Null, Some(format!("{error:#}"))),
        Ok(None) => service.plugin_reply(client, request, serde_json::json!({"supported":false}), None),
        Ok(Some((native, pid, action, id))) => {
            let tx = service.tx.clone();
            let cancel = service.sessions.get_mut(&id).unwrap().chat.control_cancel();
            let worker = crate::extensions::WorkGuard::new(service.extensions.workers.clone());
            thread::spawn(move || {
                let _worker = worker;
                let (data, error) = match action
                    .resolve(deadline, &cancel)
                    .and_then(|action| native.control_until(pid, action, deadline, &cancel))
                {
                    Ok(data) => (data, None),
                    // The gateway tells the phone by code, not by these words.
                    Err(error) if crate::protocol::is_question_stale(&error) => {
                        (serde_json::json!({"code": crate::protocol::QUESTION_STALE}), Some(format!("{error:#}")))
                    }
                    Err(error) => (Value::Null, Some(format!("{error:#}"))),
                };
                let _ = tx.send(Ev::PluginReply { client, request, data, error });
            });
        }
    }
    true
}
