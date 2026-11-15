//! Dot's native provider adapter on the existing authenticated Plugin channel.
use super::*;
use crate::attention::ExpectedTarget;
use crate::dot::api::{Image, Message};
use crate::dot::OwnerRequest;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    id: u64,
    expected: ExpectedTarget,
    text: String,
    #[serde(default)]
    images: Vec<Image>,
}

pub(super) fn start(
    service: &mut Service,
    client: u64,
    request: u64,
    plugin: &str,
    command: &str,
    args: &Value,
) -> bool {
    if (plugin, command) != ("provider-dot", "send") {
        return false;
    }
    let prepared = (|| -> Result<(crate::providers::Provider, u32, Message)> {
        let args: Arguments = serde_json::from_value(args.clone())?;
        let session = service.sessions.get(&args.id).context("The original Dot session is unavailable.")?;
        anyhow::ensure!(
            session.tool == Tool::Dynamic("dot") && super::provider_target::matches(&args.expected, session),
            "The Dot controller changed; the draft and images are kept."
        );
        let provider =
            service.opts.config.providers.iter().find(|p| p.id == "dot").cloned().context("Dot is not configured.")?;
        let pid = session.hosted.as_ref().and_then(|h| h.pid).context("Dot is not connected.")?;
        Ok((provider, pid, Message { text: args.text, images: args.images }))
    })();
    match prepared {
        Err(error) => service.plugin_reply(client, request, Value::Null, Some(format!("{error:#}"))),
        Ok((provider, pid, message)) => {
            let tx = service.tx.clone();
            let cancel = service.extensions.cancel.clone();
            let worker = crate::extensions::WorkGuard::new(service.extensions.workers.clone());
            thread::spawn(move || {
                let _worker = worker;
                // Image decoding and private-state I/O stay off the service actor.
                let prepared = if message.images.is_empty() {
                    OwnerRequest::prepare(&provider, pid, &message.text)
                } else {
                    OwnerRequest::with_images(&provider, pid, message)
                };
                let (data, error) = match prepared
                    .and_then(|native| native.deliver(Instant::now() + Duration::from_secs(120), &cancel))
                {
                    Ok(data) => (data, None),
                    Err(error) => (Value::Null, Some(error)),
                };
                let _ = tx.send(Ev::PluginReply { client, request, data, error });
            });
        }
    }
    true
}
