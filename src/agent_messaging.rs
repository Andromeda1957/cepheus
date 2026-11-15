//! Agent peer controls use native session APIs and the existing fleet transport.
use super::*;
use crate::providers::peers::{Sender, MAX_TEXT};
use serde_json::{json, Value};
use std::io::{self, Read};
use std::time::Instant;

fn receipt(conn: &mut Conn) -> Result<ServerMsg> {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let remaining = deadline.checked_duration_since(Instant::now()).context("native receipt timed out")?;
        conn.reader.get_mut().set_read_timeout(Some(remaining))?;
        match read_msg::<_, ServerMsg>(&mut conn.reader)? {
            Some(msg @ ServerMsg::PluginResult { request: 1, .. }) => return Ok(msg),
            Some(ServerMsg::Notice { text, error: true, .. }) => bail!("{text}"),
            None => bail!("the native message connection closed"),
            _ => {}
        }
    }
}

/// Whether the service has a live link to the session's host; a local
/// session is on the service itself.
pub(super) fn host_online(state: &crate::protocol::DeckState, session: &crate::protocol::SessionInfo) -> bool {
    session.host.as_ref().is_none_or(|name| state.hosts.iter().any(|host| &host.name == name && host.online))
}

pub(super) fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str).unwrap_or("list") {
        "list" => {
            anyhow::ensure!(args.len() <= 1, "usage: cepheus agent list");
            let conn = Conn::open(false)?;
            // Only the service's current link to a host says it is reachable;
            // nothing here has checked the recipient itself.
            let agents: Vec<_> = conn
                .state
                .sessions
                .iter()
                .filter(|s| s.alive && s.tool.is_agent())
                .map(|s| {
                    json!({
                        "id":s.id,"provider":s.tool,"title":s.title,"status":s.status,"host":s.host,
                        "conversation":s.conversation,"cwd":s.cwd,"worktree":s.worktree,
                        "native_transport":crate::providers::peers::transport(s.tool),
                        "host_online":host_online(&conn.state, s)
                    })
                })
                .collect();
            println!("{}", json!({"agents":agents}));
        }
        "send" => {
            let id = args.get(1).context("usage: cepheus agent send <id> --stdin | <text>")?.parse::<u64>()?;
            let mut input_args = args.get(2..).unwrap_or_default().to_vec();
            let pinned = input_args.iter().any(|arg| arg == "--conversation");
            let expected = take_flag(&mut input_args, "--conversation");
            anyhow::ensure!(!pinned || expected.is_some(), "--conversation requires a conversation ID");
            let text = match input_args.as_slice() {
                [flag] if flag == "--stdin" => {
                    let mut text = String::new();
                    std::io::Read::take(io::stdin().lock(), MAX_TEXT as u64 + 1).read_to_string(&mut text)?;
                    text
                }
                [text] if text != "--stdin" => text.clone(),
                _ => bail!("usage: cepheus agent send <id> --stdin | <text>"),
            };
            anyhow::ensure!(
                !text.trim().is_empty() && text.len() <= MAX_TEXT && !text.contains('\0'),
                "message must contain 1–8192 bytes of text"
            );
            let native = Conn::local()?;
            let source = crate::providers::peers::sender(&native.state)?;
            let mut conn = Conn::open(false)?;
            let source_id = shared_self_id(source, &conn.state)?;
            anyhow::ensure!(source_id != id, "choose another agent as the recipient");
            let target = conn
                .state
                .sessions
                .iter()
                .find(|s| s.id == id && s.alive && s.tool.is_agent())
                .context("recipient is not a live agent")?;
            if !host_online(&conn.state, target) {
                bail!("{} is offline; nothing was sent", target.host.as_deref().unwrap_or("the recipient's host"));
            }
            let conversation = target.conversation.as_deref().context("recipient conversation is not known yet")?;
            anyhow::ensure!(
                expected.as_deref().is_none_or(|expected| expected == conversation),
                "recipient has moved to a different conversation; nothing was sent"
            );
            let source = Sender {
                id: source_id,
                provider: source.tool,
                title: source.title.clone(),
                conversation: source.conversation.clone().context("sending conversation is not known yet")?,
                permission_class: crate::providers::peers::permission_class(source),
            };
            source.validate()?;
            let message_id = crate::util::uuid_v4()?;
            let mut params = json!({"id":crate::protocol::split_id(id).1,"conversation":conversation,
                "sender":source,"text":text,"message_id":message_id});
            if let Some(host) = &target.host {
                params["host"] = Value::String(host.clone());
            }
            anyhow::ensure!(conn.version >= 14, "update Cepheus before using native agent communication");
            conn.send(&ClientMsg::Plugin {
                request: 1,
                plugin: "agent-messaging".into(),
                command: "send".into(),
                args: params,
            })
            .with_context(|| {
                format!("native message {message_id} submission was uncertain; do not resend automatically")
            })?;
            let response = receipt(&mut conn).with_context(|| {
                format!("native message {message_id} has no confirmed receipt; do not resend automatically")
            })?;
            match response {
                ServerMsg::PluginResult { data, error: None, .. } => println!("{data}"),
                ServerMsg::PluginResult { data, error: Some(error), .. } => bail!("{error}; receipt: {data}"),
                _ => bail!("unexpected native message response; do not resend automatically"),
            }
        }
        _ => bail!("usage: cepheus agent list | send <id> --stdin | send <id> <text>"),
    }
    Ok(())
}
