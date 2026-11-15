//! Desktop attention state. All delivery, permission and repository custody belongs to the service.
use super::*;
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Messages,
    Supervisor,
}
impl Tab {
    pub fn next(self) -> Self {
        match self {
            Self::Messages => Self::Supervisor,
            Self::Supervisor => Self::Messages,
        }
    }
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Draft {
    pub session: u64,
    #[serde(skip)]
    pub namespace: u64,
    pub conversation: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub spawned_ms: u64,
    #[serde(default)]
    pub host: Option<String>,
    pub text: String,
    #[serde(skip)]
    pub request: Option<u64>,
    #[serde(skip)]
    pub error: Option<String>,
    pub accepted: bool,
    pub attempted: bool,
    #[serde(skip)]
    pub local_only: bool,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Decision {
    pub args: Value,
    #[serde(skip)]
    pub namespace: u64,
    pub known: bool,
    #[serde(skip)]
    pub local_only: bool,
}
pub fn operation_key() -> String {
    format!(
        "desktop-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_nanos()
    )
}
struct Pending {
    action: String,
    at: Instant,
    view: u64,
    epoch: u64,
    target: Option<String>,
    origin: Option<String>,
    key: Option<String>,
}
pub struct PrivateReader {
    pub identity: String,
    pub list_viewport: DialogViewport,
}
#[derive(Default)]
pub struct View {
    pending: HashMap<u64, Pending>,
    pub private: Option<(String, String)>,
    pub reader: Option<PrivateReader>,
    pub generation: u64,
    pub(crate) drafts: HashMap<String, Draft>,
    pub(crate) active_draft: Option<String>,
    pub(super) last_request: Option<u64>,
    private_request: Option<u64>,
    epoch: u64,
    pub(super) namespace: u64,
    pub(super) draft_for: HashMap<u64, String>,
    pub(crate) decisions: HashMap<String, Decision>,
    pub(super) store_root: Option<PathBuf>,
    pub(super) origin: String,
    pub(super) origin_namespaces: HashMap<String, u64>,
    pub(super) store_failed: bool,
}

pub fn message_key(host: Option<&str>, id: &str) -> String {
    json!([host.unwrap_or("@here"), id]).to_string()
}

pub fn label<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}
impl App {
    pub fn attention_available(&self) -> bool {
        self.service_version >= 15 && self.state.attention_capable
    }
    pub(super) fn attention_gate(&mut self) -> bool {
        if self.attention_available() {
            true
        } else {
            self.set_notice(
                "Attention needs protocol 15 and attention support; update needed on the selected service/host.",
                true,
            );
            false
        }
    }
    pub(super) fn attention_request(&mut self, action: &str, args: Value) -> bool {
        self.attention_request_mode(action, args, false)
    }
    /// Explicit retry of an immutable operation already durably recorded before its first transport attempt.
    /// Re-saving cannot block reconciliation behind a newer target owner; the service's original key remains authoritative.
    pub(super) fn attention_reconcile(&mut self, action: &str, args: Value) -> bool {
        self.attention_request_mode(action, args, true)
    }
    fn attention_request_mode(&mut self, action: &str, args: Value, reconcile: bool) -> bool {
        if action == "attention-send" && self.service_version < 17 {
            self.set_notice(
                "Identity-safe idle messages need protocol 17; update needed on the selected service/host.",
                true,
            );
            return false;
        }
        if action == "attention-supervisor-rules"
            && args
                .get("rules")
                .and_then(Value::as_array)
                .is_some_and(|rules| rules.iter().any(|rule| rule["pattern"]["kind"] == "native_exact"))
            && self.service_version < 16
        {
            self.set_notice(
                "Native permission rules need protocol 16; update needed on the selected service/host.",
                true,
            );
            return false;
        }
        if !self.attention_gate() {
            return false;
        }
        if self.attention.pending.len() >= 64 {
            self.set_notice("Attention request limit reached; check status before retrying.", true);
            return false;
        }
        if let Some(key) = args.get("key").and_then(Value::as_str) {
            if let Some((request, _)) =
                self.attention.pending.iter().find(|(_, p)| p.action == action && p.key.as_deref() == Some(key))
            {
                self.attention.last_request = Some(*request);
                return true;
            }
        }
        if !reconcile && matches!(action, "attention-send" | "attention-decide") && !self.persist_attention() {
            return false;
        }
        let request = self.next_fleet_request();
        self.attention.last_request = Some(request);
        if matches!(action, "attention-message" | "attention-supervisor-rules") {
            self.attention.private_request = Some(request);
        }
        self.attention.pending.insert(
            request,
            Pending {
                action: action.into(),
                at: Instant::now(),
                view: self.attention.generation,
                epoch: self.attention.epoch,
                target: args.get("id").and_then(Value::as_str).map(str::to_owned),
                origin: args.get("host").and_then(Value::as_str).map(str::to_owned),
                key: args.get("key").and_then(Value::as_str).map(str::to_owned),
            },
        );
        self.send(ClientMsg::Fleet { request, action: action.into(), args });
        true
    }
    pub(super) fn attention_selection(&self) -> Option<(Tab, Option<String>)> {
        let Some(Dialog::Attention { tab, sel }) = self.dialog.as_ref() else { return None };
        let id = match tab {
            Tab::Messages => self.state.attention.messages.get(*sel).map(|r| message_key(r.host.as_deref(), &r.id)),
            Tab::Supervisor => return None,
        };
        Some((*tab, id))
    }
    pub(super) fn restore_attention_selection(&mut self, selected: Option<(Tab, Option<String>)>) {
        let Some((tab, id)) = selected else { return };
        let index = id
            .and_then(|id| match tab {
                Tab::Messages => {
                    self.state.attention.messages.iter().position(|r| message_key(r.host.as_deref(), &r.id) == id)
                }
                Tab::Supervisor => None,
            })
            .unwrap_or(usize::MAX);
        if index == usize::MAX && self.attention.reader.is_none() {
            self.attention.private = None;
            self.attention.generation = self.attention.generation.wrapping_add(1);
        }
        if let Some(Dialog::Attention { sel, .. }) = self.dialog.as_mut() {
            *sel = index;
        }
    }
    pub fn open_attention(&mut self, tab: Tab) {
        if !self.attention_gate() {
            return;
        }
        self.attention.generation = self.attention.generation.wrapping_add(1);
        self.attention.private = None;
        self.attention.reader = None;
        self.dialog = Some(Dialog::Attention { tab, sel: 0 });
        self.attention_request("attention-status", json!({}));
    }
    pub(super) fn attention_namespace_changed(&mut self) {
        // origin still names the old coordinator, even when coordinator_target already changed.
        // Save before retiring its namespace; an unsuccessful save also retains RAM custody.
        self.persist_attention();
        // An invalid coordinator selection never loads a namespace. Do not replace the
        // last origin's retained custody with that empty intermediate namespace.
        if self.attention.drafts.values().any(|d| d.namespace == self.attention.namespace && !d.accepted)
            || self.attention.decisions.values().any(|d| d.namespace == self.attention.namespace && !d.known)
        {
            self.attention.origin_namespaces.insert(self.attention.origin.clone(), self.attention.namespace);
        }
        self.attention.namespace = self.attention.namespace.wrapping_add(1);
        self.attention.pending.clear();
        self.attention.private = None;
        self.attention.reader = None;
        self.attention.active_draft = None;
        self.attention.draft_for.clear();
        self.attention.generation = self.attention.generation.wrapping_add(1);
    }
    pub(super) fn attention_new_state(&mut self) {
        self.attention.epoch = self.attention.epoch.wrapping_add(1);
    }
    pub(super) fn attention_result(&mut self, request: u64, data: &Value, error: Option<&str>) -> bool {
        let Some(pending) = self.attention.pending.remove(&request) else { return false };
        if !self.attention_available() || (pending.action == "attention-send" && self.service_version < 17) {
            if let Some(draft) = pending
                .key
                .as_ref()
                .and_then(|key| self.attention.drafts.get_mut(key))
                .filter(|draft| draft.request == Some(request))
            {
                draft.request = None;
                draft.error = Some("Update needed; immutable request retained for explicit reconciliation".into());
            }
            return true;
        }
        let receipt =
            data.get("message").and_then(|v| serde_json::from_value::<crate::attention::MessageInfo>(v.clone()).ok());
        if let Some(draft) = pending.key.as_ref().and_then(|key| self.attention.drafts.get_mut(key)) {
            draft.request = None;
            draft.accepted |= error.is_none()
                && receipt.as_ref().is_some_and(|m| {
                    !m.id.is_empty()
                        && m.session == draft.session
                        && m.host.as_deref().unwrap_or("@here") == draft.host.as_deref().unwrap_or("@here")
                        && m.state != crate::attention::DeliveryState::Unknown
                });
            draft.error = (!draft.accepted).then(|| {
                "Admission remains unresolved; text is immutable. Retry the same operation to reconcile.".into()
            });
        }
        if let Some(decision) = pending.key.as_ref().and_then(|key| self.attention.decisions.get_mut(key)) {
            decision.known |= error.is_none()
                && data.get("decision").is_some_and(|d| {
                    d.get("id").and_then(Value::as_str).is_some_and(|id| !id.is_empty())
                        && d.get("recipients")
                            .and_then(Value::as_array)
                            .is_some_and(|ids| ids.iter().all(|id| id.as_u64().is_some()))
                });
        }
        if matches!(pending.action.as_str(), "attention-send" | "attention-decide") {
            self.persist_attention();
        }
        if let Some(error) = error {
            self.set_notice(format!("Attention: {}", util::one_line(error)), true);
            return true;
        }
        let selection = self.attention_selection();
        if let Some(snapshot) = data
            .get("attention")
            .filter(|_| pending.epoch == self.attention.epoch)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
        {
            self.state.attention = snapshot;
        }
        if let Some(supervisor) = data
            .get("supervisor")
            .filter(|_| pending.epoch == self.attention.epoch)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
        {
            self.state.attention.supervisor = supervisor;
        }
        if let Some(message) = data
            .get("message")
            .filter(|_| pending.epoch == self.attention.epoch)
            .and_then(|v| serde_json::from_value::<crate::attention::MessageInfo>(v.clone()).ok())
        {
            if let Some(old) =
                self.state.attention.messages.iter_mut().find(|m| {
                    message_key(m.host.as_deref(), &m.id) == message_key(message.host.as_deref(), &message.id)
                })
            {
                if message.updated_at >= old.updated_at {
                    *old = message;
                }
            } else {
                self.state.attention.messages.push(message);
            }
        }
        self.restore_attention_selection(selection);
        let same_view =
            pending.view == self.attention.generation && matches!(self.dialog, Some(Dialog::Attention { .. }));
        let target_matches = match pending.action.as_str() {
            "attention-message" => {
                self.attention_selection().is_some_and(|(tab, id)| {
                    tab == Tab::Messages
                        && id == pending.target.as_deref().map(|id| message_key(pending.origin.as_deref(), id))
                }) && data.get("message").and_then(|m| m.get("id")).and_then(Value::as_str) == pending.target.as_deref()
                    && data.get("message").and_then(|m| m.get("host")).and_then(Value::as_str).unwrap_or("@here")
                        == pending.origin.as_deref().unwrap_or("@here")
            }
            _ => true,
        };
        if same_view
            && target_matches
            && self.attention.private_request == Some(request)
            && matches!(pending.action.as_str(), "attention-message" | "attention-supervisor-rules")
        {
            let body = data
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| data.get("rules").and_then(|v| serde_json::to_string_pretty(v).ok()))
                .unwrap_or_default();
            let list_viewport = self
                .attention
                .reader
                .as_ref()
                .map(|r| r.list_viewport.clone())
                .unwrap_or_else(|| self.dialog_viewport.clone());
            let identity = if pending.action == "attention-message" {
                message_key(pending.origin.as_deref(), pending.target.as_deref().unwrap_or(""))
            } else {
                format!("{}:{}", pending.action, pending.target.as_deref().unwrap_or(""))
            };
            self.attention.reader = Some(PrivateReader { identity, list_viewport });
            self.dialog_viewport = DialogViewport::default();
            self.attention.private = Some((pending.action.clone(), body.chars().take(65536).collect()));
        }
        match pending.action.as_str() {
            "attention-send" => {
                let known =
                    pending.key.as_ref().and_then(|key| self.attention.drafts.get(key)).is_some_and(|d| d.accepted);
                self.set_notice(
                    if known {
                        "Idle message receipt known; delivery remains subject to core input safety checks."
                    } else {
                        "Idle admission remains unresolved; immutable draft retained for same-key reconciliation."
                    },
                    !known,
                );
            }
            "attention-decide" => {
                let known =
                    pending.key.as_ref().and_then(|key| self.attention.decisions.get(key)).is_some_and(|d| d.known);
                self.set_notice(
                    if known {
                        "Decision receipt known; its frozen recipients receive it when safe and idle."
                    } else {
                        "Decision receipt unresolved; exact immutable request retained for same-key reconciliation."
                    },
                    !known,
                );
            }
            "attention-supervisor-off" => self.set_notice(
                "Supervisor off requested; pending/offline hosts remain visible until acknowledged.",
                false,
            ),
            _ => {}
        }
        true
    }
    pub(super) fn poll_attention(&mut self, now: Instant) {
        let before = self.attention.pending.len();
        let expired: Vec<_> = self
            .attention
            .pending
            .iter()
            .filter(|(_, p)| now.saturating_duration_since(p.at) >= Duration::from_secs(15))
            .map(|(r, _)| *r)
            .collect();
        self.attention.pending.retain(|r, _| !expired.contains(r));
        for draft in self.attention.drafts.values_mut().filter(|d| d.request.is_some_and(|r| expired.contains(&r))) {
            draft.request = None;
            draft.error = Some("Reply unknown; retry uses the same operation key".into());
        }
        if before != self.attention.pending.len() {
            self.set_notice(
                "Attention reply timed out; check status before retrying an operation. Nothing was replayed.",
                true,
            );
        }
    }
    pub(super) fn attention_key(&mut self, tab: Tab, sel: usize, k: KeyEvent) -> Option<Dialog> {
        if !k.modifiers.is_empty() && !matches!(k.code, KeyCode::BackTab) {
            return Some(Dialog::Attention { tab, sel });
        }
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if let Some(reader) = self.attention.reader.take() {
                    self.dialog_viewport = reader.list_viewport;
                    self.attention.private = None;
                    self.attention.generation = self.attention.generation.wrapping_add(1);
                    return Some(Dialog::Attention { tab, sel });
                }
                self.attention.generation = self.attention.generation.wrapping_add(1);
                self.attention.private = None;
                return None;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.attention.reader = None;
                self.attention.generation = self.attention.generation.wrapping_add(1);
                self.attention.private = None;
                return Some(Dialog::Attention { tab: tab.next(), sel: 0 });
            }
            KeyCode::Char('r') => {
                self.attention_request("attention-status", json!({}));
            }
            KeyCode::Enter => match tab {
                Tab::Messages => {
                    if let Some(message) = self.state.attention.messages.get(sel) {
                        let args = json!({"id":message.id,"host":message.host.as_deref().unwrap_or("@here")});
                        self.attention_request("attention-message", args);
                    }
                }
                Tab::Supervisor => {
                    self.attention_request("attention-supervisor-rules", json!({}));
                }
            },
            KeyCode::Char('c') if tab == Tab::Messages => {
                if let Some(message) = self.state.attention.messages.get(sel).filter(|m| {
                    matches!(m.state, crate::attention::DeliveryState::Queued | crate::attention::DeliveryState::Held)
                }) {
                    let args = json!({"id":message.id,"host":message.host.as_deref().unwrap_or("@here")});
                    self.attention_request("attention-cancel", args);
                }
            }
            _ => {}
        }
        Some(Dialog::Attention { tab, sel })
    }
}
#[cfg(test)]
#[path = "../tests/tui_attention.rs"]
mod tests;
