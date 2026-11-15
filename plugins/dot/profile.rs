//! The Dot's profile card: its name and the pet or picture the owner chose in
//! ChatGPT, kept as `profile.json` (and `avatar.png` or `avatar.webp`) beside
//! the transcript for the deck's view.
//!
//! `{"name": str|null, "kind": str|null, "pet": str|null, "image": str|null, "file": str|null}`:
//! `kind` is the avatar type, `pet` the built-in pet for `codex-pet`, `image`
//! a key that changes with the picture and `file` the picture stored here.
//! The native host downloads the picture itself, so its signed URL never
//! leaves that host; it does so only when `image` changes.
use std::path::Path;

use serde_json::{json, Value};

use super::state::{write_bytes, State};
use super::Error;

/// The largest picture accepted.
pub const AVATAR_MAX: usize = 2 << 20;
const FILES: [&str; 2] = ["avatar.png", "avatar.webp"];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Card {
    pub name: Option<String>,
    pub kind: Option<String>,
    pub pet: Option<String>,
    pub image: Option<String>,
}

impl Card {
    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "kind": self.kind, "pet": self.pet, "image": self.image})
    }
}

/// `[A-Za-z0-9_-]{1,64}`.
fn word(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?;
    (!text.is_empty() && text.len() <= 64 && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
        .then(|| text.to_string())
}

/// A display name: trimmed, without control characters, at most 64 characters.
fn name(value: Option<&Value>) -> Option<String> {
    let text: String = value?.as_str()?.chars().filter(|c| !c.is_control()).collect();
    let text = super::clip_chars(text.trim(), 64).trim_end();
    (!text.is_empty()).then(|| text.to_string())
}

/// A change key: a short string or number without control characters.
fn key(value: Option<&Value>) -> Option<String> {
    let text = match value? {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    (!text.is_empty() && text.chars().count() <= 256 && !text.chars().any(char::is_control)).then_some(text)
}

/// Read a card from the profile service's answer, with the picture's URL.
pub fn card(value: &Value) -> Option<(Card, Option<String>)> {
    value.as_object()?;
    let kind = word(value.get("avatar_type"));
    let manifest = value.get("avatar_manifest");
    let pet = if kind.as_deref() == Some("codex-pet") { word(manifest.and_then(|m| m.get("pet_id"))) } else { None };
    let image = key(manifest.and_then(|m| m.get("snapshot")).and_then(|s| s.get("content_sha256")))
        .or_else(|| key(value.get("avatar_id")))
        .or_else(|| key(value.get("avatar")))
        .or_else(|| key(value.get("updated_at")));
    let url = value.get("avatar_url").and_then(Value::as_str).map(str::to_string);
    Some((Card { name: name(value.get("display_name")), kind, pet, image }, url))
}

/// The same rules for a card that crossed from the native host.
fn checked(value: &Value) -> Option<Card> {
    value.as_object()?;
    let kind = word(value.get("kind"));
    let pet = if kind.as_deref() == Some("codex-pet") { word(value.get("pet")) } else { None };
    Some(Card { name: name(value.get("name")), kind, pet, image: key(value.get("image")) })
}

/// A picture's format from its first bytes: only PNG and WebP are kept.
pub fn media(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// Only the app's signed picture host, over TLS on its usual port.
pub fn avatar_allowed(url: &str, loopback: bool) -> bool {
    let Ok(uri) = url.parse::<tungstenite::http::Uri>() else { return false };
    let Some(authority) = uri.authority() else { return false };
    let host = uri.host().unwrap_or("");
    if authority.as_str().contains('@') {
        return false;
    }
    if loopback && uri.scheme_str() == Some("http") && host == "127.0.0.1" {
        return true;
    }
    uri.scheme_str() == Some("https")
        && uri.port_u16().is_none_or(|port| port == 443)
        && host.ends_with(".oaiusercontent.com")
}

fn current(state: &State) -> Option<serde_json::Map<String, Value>> {
    match state.read("profile") {
        Ok(Some(Value::Object(map))) => Some(map),
        _ => None,
    }
}

fn stored_file(state: &State, card: &serde_json::Map<String, Value>) -> Option<String> {
    let file = card.get("file")?.as_str()?;
    (FILES.contains(&file) && state.directory.join(file).is_file()).then(|| file.to_string())
}

/// The picture key the host need not send again. After a reconnect a
/// missing picture is fetched again.
pub fn have(state: &State, reconnect: bool) -> Option<String> {
    let card = current(state)?;
    let image = card.get("image")?.as_str()?.to_string();
    if reconnect && stored_file(state, &card).is_none() {
        return None;
    }
    Some(image)
}

/// Keep the host's answer. Without a card the last one stays.
pub fn record(state: &State, answer: &Value) -> Result<(), Error> {
    let Some(card) = answer.get("card").and_then(checked) else { return Ok(()) };
    let previous = current(state);
    let mut file = None;
    let avatar = answer.get("avatar").filter(|avatar| avatar.is_object());
    if let Some(avatar) = avatar {
        let data = avatar.get("data").and_then(Value::as_str).and_then(crate::util::unbase64);
        let declared = avatar.get("media").and_then(Value::as_str);
        if let Some(data) = data.filter(|d| d.len() <= AVATAR_MAX) {
            if let Some(kind) = media(&data).filter(|kind| Some(*kind) == declared) {
                let name = format!("avatar.{kind}");
                write_bytes(&state.directory.join(&name), &data)?;
                file = Some(name);
            }
        }
    }
    if file.is_none() && card.image.is_some() {
        if let Some(previous) =
            previous.as_ref().filter(|p| p.get("image").and_then(Value::as_str) == card.image.as_deref())
        {
            file = stored_file(state, previous);
        }
    }
    let value = json!({"name": card.name, "kind": card.kind, "pet": card.pet, "image": card.image, "file": file});
    if previous.map(Value::Object).as_ref() != Some(&value) {
        state.write("profile", &value)?;
    }
    for stale in FILES.iter().filter(|name| Some(**name) != file.as_deref()) {
        let path = state.directory.join(stale);
        if Path::new(&path).is_file() {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}
