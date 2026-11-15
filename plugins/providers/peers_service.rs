//! Native inbox work stays off the terminal-hosting service loop.
use super::*;
use crate::providers::peers::{self, Sender, Target};

pub(super) fn start(
    service: &mut Service,
    client: u64,
    request: u64,
    plugin: &str,
    command: &str,
    args: &Value,
) -> bool {
    if plugin != "agent-messaging" || command != "send" {
        return false;
    }
    let prepared = (|| -> anyhow::Result<(peers::Request, String, Arc<AtomicBool>)> {
        let id = args["id"].as_u64().context("target session is required")?;
        let sender: Sender = serde_json::from_value(args["sender"].clone())?;
        let text = args["text"].as_str().context("message text is required")?;
        let message_id = args["message_id"].as_str().context("message ID is required")?;
        let text = peers::envelope(&sender, message_id, text)?;
        let session = service
            .sessions
            .get(&id)
            .filter(|s| s.alive && s.closing.is_none())
            .context("target agent is no longer live; nothing was sent")?;
        let conversation =
            session.conversation.as_deref().context("target conversation is not known yet; nothing was sent")?;
        anyhow::ensure!(
            args["conversation"].as_str() == Some(conversation),
            "target conversation changed; nothing was sent"
        );
        let pid = session.hosted.as_ref().and_then(|hosted| hosted.pid).context("target has no live native process")?;
        let root = session.account_root.as_deref().unwrap_or(if session.tool == Tool::Claude {
            &service.opts.claude_dir
        } else {
            &service.opts.codex_dir
        });
        let target = Target {
            tool: session.tool,
            pid,
            conversation,
            cwd: session.cwd.as_deref().unwrap_or(&service.opts.start_dir),
            account_root: root,
        };
        let delivery = peers::Request::prepare(target, &service.opts.config, &text, message_id)?;
        let cancel = service.messages.peer_stop(id, conversation, session.spawned_ms);
        Ok((delivery, message_id.to_string(), cancel))
    })();
    match prepared {
        Err(error) => service.plugin_reply(client, request, Value::Null, Some(format!("{error:#}"))),
        Ok((delivery, message_id, cancel)) => {
            let tx = service.tx.clone();
            let guard = crate::extensions::WorkGuard::new(service.extensions.workers.clone());
            thread::spawn(move || {
                let _guard = guard;
                let (data, error) = match delivery.deliver(&cancel) {
                    Ok(mut data) => {
                        data["message_id"] = Value::String(message_id);
                        (data, None)
                    }
                    Err(error) => (serde_json::json!({"message_id":message_id}), Some(format!("{error:#}"))),
                };
                let _ = tx.send(Ev::PluginReply { client, request, data, error });
            });
        }
    }
    true
}
