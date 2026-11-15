//! Delayed provider operations use the existing authenticated plugin route and
//! validate the captured native target in the actor immediately before admission.
use super::*;
use crate::attention::{store::Target, ExpectedTarget};
use serde::Deserialize;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    id: u64,
    expected: ExpectedTarget,
    operation: String,
    data: Option<String>,
    text: Option<String>,
    client_id: Option<String>,
    attachments: Option<bool>,
    images: Option<Vec<(std::path::PathBuf, String)>>,
}

/// Images the phone uploaded, each still holding exactly the bytes the
/// gateway hashed when the owner sent it. Only the owner's own regular files
/// in the upload folders qualify, so a path in a request cannot point the
/// agent at anything else.
pub(crate) fn owner_images(
    state: &std::path::Path,
    paths: &[(std::path::PathBuf, String)],
    deadline: std::time::Instant,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<std::path::PathBuf>> {
    owner_uploads(state, paths, deadline, cancel, true)
}

/// Files of any kind the phone uploaded, checked as `owner_images` checks
/// images: Claude opens each by its path.
pub(crate) fn owner_files(
    state: &std::path::Path,
    paths: &[(std::path::PathBuf, String)],
    deadline: std::time::Instant,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<std::path::PathBuf>> {
    owner_uploads(state, paths, deadline, cancel, false)
}

fn owner_uploads(
    state: &std::path::Path,
    paths: &[(std::path::PathBuf, String)],
    deadline: std::time::Instant,
    cancel: &std::sync::atomic::AtomicBool,
    images: bool,
) -> Result<Vec<std::path::PathBuf>> {
    let noun = if images { "image" } else { "file" };
    anyhow::ensure!(!paths.is_empty() && paths.len() <= crate::remote::uploads::FILES_MAX, "invalid {noun} count");
    let roots = [state.join("uploads"), state.join("peer/uploads")];
    let roots: Vec<_> = roots.iter().filter_map(|root| root.canonicalize().ok()).collect();
    paths
        .iter()
        .map(|(path, expected)| {
            anyhow::ensure!(
                expected.len() == 64 && expected.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "invalid {noun} digest"
            );
            let mut file =
                std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
            let meta = file.metadata()?;
            anyhow::ensure!(
                meta.is_file()
                    && meta.uid() == unsafe { libc::geteuid() }
                    && meta.len() > 0
                    && meta.len() <= crate::remote::uploads::FILE_MAX,
                "invalid uploaded {noun}"
            );
            let canonical = path.canonicalize()?;
            anyhow::ensure!(
                roots.iter().any(|root| canonical.starts_with(root)),
                "{noun} is outside the upload directory"
            );
            let mut header = [0u8; 12];
            let count = (&file).take(12).read(&mut header)?;
            let b = &header[..count];
            anyhow::ensure!(
                !images
                    || b.starts_with(b"\x89PNG\r\n\x1a\n")
                    || b.starts_with(b"\xff\xd8\xff")
                    || b.starts_with(b"GIF87a")
                    || b.starts_with(b"GIF89a")
                    || (b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP")),
                "only uploaded PNG, JPEG, GIF or WebP images are supported"
            );
            let path_meta = std::fs::metadata(&canonical)?;
            anyhow::ensure!(meta.dev() == path_meta.dev() && meta.ino() == path_meta.ino(), "uploaded {noun} changed");
            use std::io::{Seek, SeekFrom};
            file.seek(SeekFrom::Start(0))?;
            let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
            let mut buf = [0u8; 65536];
            let mut total = 0u64;
            loop {
                anyhow::ensure!(
                    !cancel.load(std::sync::atomic::Ordering::SeqCst) && std::time::Instant::now() < deadline,
                    "{noun} delivery expired; nothing was sent"
                );
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                anyhow::ensure!(total <= crate::remote::uploads::FILE_MAX, "uploaded {noun} is too large");
                digest.update(&buf[..n]);
            }
            anyhow::ensure!(
                total == meta.len() && crate::prompt::hex(digest.finish().as_ref()) == *expected,
                "uploaded {noun} changed; nothing was sent"
            );
            Ok(canonical)
        })
        .collect()
}

fn provider(config: &Config, plugin: &str) -> Option<Tool> {
    match plugin {
        "provider-claude" => Some(Tool::Claude),
        "provider-codex" => Some(Tool::Codex),
        "provider-agy" => Some(Tool::Antigravity),
        "provider-grok" => Some(Tool::Grok),
        "provider-opencode" => Some(Tool::OpenCode),
        _ => {
            let id = plugin.strip_prefix("provider-")?;
            let configured = config.providers.iter().find(|p| p.id == id && !p.cloud && p.validate().is_ok())?;
            crate::providers::intern(&configured.id).ok().map(Tool::Dynamic)
        }
    }
}

pub(super) fn argument_limit(config: &Config, plugin: &str, command: &str) -> usize {
    if command == "target" && provider(config, plugin).is_some() {
        // A 32 KiB message can use six JSON bytes for each escaped character,
        // and each image adds its upload path and digest.
        6 * crate::remote::wire::SEND_MAX + 4096 + crate::remote::uploads::FILES_MAX * (4096 + 72)
    } else {
        16 * 1024
    }
}

pub(crate) fn matches(expected: &ExpectedTarget, session: &Sess) -> bool {
    session.alive
        && session.closing.is_none()
        && expected.matches(&Target {
            session: session.id,
            spawned_ms: session.spawned_ms,
            pid: session.hosted.as_ref().and_then(|hosted| hosted.pid),
            conversation: session.conversation.clone(),
        })
}

pub(super) fn start(
    service: &mut Service,
    client: u64,
    request: u64,
    plugin: &str,
    command: &str,
    args: &Value,
) -> bool {
    let Some(tool) = provider(&service.opts.config, plugin).filter(|_| command == "target") else { return false };
    let result = (|| -> Result<Value> {
        let args: Arguments = serde_json::from_value(args.clone())?;
        let session = service.sessions.get(&args.id).context("no such local session")?;
        anyhow::ensure!(session.tool == tool && matches(&args.expected, session), "the native target changed");
        anyhow::ensure!(!service.pending_directories.contains_key(&args.id), "the target is changing directory");
        match args.operation.as_str() {
            "screen"
                if args.data.is_none()
                    && args.text.is_none()
                    && args.attachments.is_none()
                    && args.images.is_none() =>
            {
                let hosted = session.hosted.as_ref().context("target is not hosted")?;
                let lines = screen::screen_text(&hosted.lock().term);
                Ok(serde_json::json!({"lines":lines,"inputs":session.inputs}))
            }
            "input" if args.text.is_none() && args.attachments.is_none() && args.images.is_none() => {
                let data = args.data.context("missing input")?;
                anyhow::ensure!(data.len() <= crate::remote::wire::SEND_MAX && !data.contains('\0'), "invalid input");
                if matches!(data.as_str(), "\x1b" | "\x03") {
                    service.messages.cancel(args.id);
                    service.sessions[&args.id].chat.cancel_controls();
                    if let Some(hosted) = &service.sessions[&args.id].hosted {
                        hosted.revoke_guarded_input();
                    }
                }
                let tx = service.tx.clone();
                service.client_input_with_receipt(
                    args.id,
                    &data,
                    Some(Box::new(move |written| {
                        let error = (!written)
                            .then(|| "Target input was not admitted; inspect the composer before retrying".into());
                        let _ = tx.send(Ev::PluginReply { client, request, data: Value::Null, error });
                    })),
                )?;
                Ok(serde_json::json!({"pending":true}))
            }
            "send" if args.data.is_none() => {
                let text = args.text.context("missing message")?;
                anyhow::ensure!(text.len() <= crate::remote::wire::SEND_MAX && !text.contains('\0'), "invalid message");
                anyhow::ensure!(
                    !args.attachments.unwrap_or(false),
                    "native attachment delivery is not available; nothing was sent"
                );
                anyhow::ensure!(
                    args.client_id.as_deref().is_none_or(crate::providers::messages::uuid),
                    "invalid message identity"
                );
                service.send_owner_images(
                    client,
                    args.id,
                    request,
                    &text,
                    &args.images.unwrap_or_default(),
                    args.client_id.as_deref(),
                );
                Ok(Value::Null)
            }
            "broadcast" if args.data.is_none() && args.attachments.is_none() && args.images.is_none() => {
                let text = args.text.context("missing message")?;
                anyhow::ensure!(text.len() <= crate::remote::wire::SEND_MAX && !text.contains('\0'), "invalid message");
                service.begin_messages(client, &[args.id], &text, false, None, None);
                Ok(Value::Null)
            }
            "clear" if args.data.is_none() && args.text.is_none() && args.attachments.is_none() => {
                service.native_clear(client, request, args.id, args.expected)?;
                Ok(serde_json::json!({"pending":true}))
            }
            _ => anyhow::bail!("invalid target operation or fields"),
        }
    })();
    match result {
        Ok(data) if data.get("pending").and_then(Value::as_bool) == Some(true) => (),
        Ok(data) => service.plugin_target_reply(client, request, data, None),
        Err(error) => service.plugin_reply(client, request, Value::Null, Some(format!("{error:#}"))),
    }
    true
}
