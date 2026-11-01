//! Harness notices are assigned provenance once, before durable insertion.
use crate::schema::HistoryEvent;
use uuid::Uuid;

/// UUID v4 contributes 122 random bits. Never call this while serializing history.
pub(crate) fn notice(text: impl AsRef<str>) -> String {
    with_id(text.as_ref(), &Uuid::new_v4().simple().to_string())
}
fn with_id(text: &str, id: &str) -> String {
    format!("<system-message id=\"{id}\">\n{text}\n</system-message id=\"{id}\">")
}
/// Fresh inserted notices always receive the owning event's random id. Supplied
/// text (including model summaries) cannot assert its own harness provenance.
pub(crate) fn canonicalize_fresh(event: &mut HistoryEvent, event_id: &str) {
    match event {
        HistoryEvent::SystemReminder { text }
        | HistoryEvent::SubscriptionNotice { text, .. }
        | HistoryEvent::Compaction { summary: text } => {
            let id = Uuid::parse_str(event_id)
                .unwrap_or_else(|_| Uuid::new_v4())
                .simple()
                .to_string();
            *text = with_id(text, &id);
        }
        // History replacement copies come from the existing canonical journal,
        // and must retain the ids/bytes of the original injected notices.
        _ => canonicalize(event, event_id),
    }
}
/// Trusted storage replay keeps recorded notice ids, upgrading legacy raw text.
pub(crate) fn canonicalize(event: &mut HistoryEvent, event_id: &str) {
    match event {
        HistoryEvent::SystemReminder { text } | HistoryEvent::Compaction { summary: text } => {
            if body(text).is_none() {
                let id = Uuid::parse_str(event_id)
                    .unwrap_or_else(|_| Uuid::new_v4())
                    .simple()
                    .to_string();
                *text = with_id(text, &id);
            }
        }
        HistoryEvent::HistoryReplace { history, .. } => {
            for (index, event) in history.iter_mut().enumerate() {
                // Canonical replacement copies retain their existing ids. Legacy
                // nested entries get separate ids once when the envelope is read.
                let id = format!("{}{:08x}", event_id.replace('-', ""), index);
                canonicalize_legacy(event, &id);
            }
        }
        _ => {}
    }
}
fn canonicalize_legacy(event: &mut HistoryEvent, seed: &str) {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(seed.as_bytes());
    // The seed includes the durable random event id; derive stable distinct ids
    // for old replacement entries without changing them on each replay.
    let id = hash[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    match event {
        HistoryEvent::SystemReminder { text } | HistoryEvent::Compaction { summary: text }
            if body(text).is_none() =>
        {
            *text = with_id(text, &id)
        }
        _ => canonicalize(event, &id),
    }
}
/// Recognize the entire canonical wrapper, including matching random ids.
pub(crate) fn body(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("<system-message id=\"")?;
    let (id, rest) = rest.split_once("\">\n")?;
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    rest.strip_suffix(&format!("\n</system-message id=\"{id}\">"))
}
