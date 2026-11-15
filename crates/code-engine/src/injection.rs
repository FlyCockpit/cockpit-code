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
        // Replacement copies already carry their original notice provenance.
        _ => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_notices_receive_event_provenance() {
        let event_id = "00000000-0000-4000-8000-000000000001";
        let mut event = HistoryEvent::SystemReminder {
            text: "hello".into(),
        };
        canonicalize_fresh(&mut event, event_id);
        let HistoryEvent::SystemReminder { text } = event else {
            panic!()
        };
        assert_eq!(body(&text), Some("hello"));
        assert!(text.contains("00000000000040008000000000000001"));
        assert_eq!(body(&text.replace("</system-message", "</other")), None);
    }

    #[test]
    fn replacement_history_keeps_recorded_notice_bytes() {
        let mut event = HistoryEvent::HistoryReplace {
            history: vec![
                HistoryEvent::Compaction {
                    summary: notice("summary"),
                },
                HistoryEvent::SystemReminder {
                    text: "raw stored text".into(),
                },
            ],
            reason: "replace".into(),
        };
        let before = serde_json::to_value(&event).unwrap();
        canonicalize_fresh(&mut event, &Uuid::new_v4().to_string());
        assert_eq!(serde_json::to_value(event).unwrap(), before);
    }
}
