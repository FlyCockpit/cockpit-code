//! D62 question sets, validation and compatibility with older single text questions.
use crate::{Control, schema::*};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) fn schema() -> Value {
    json!({"type":"array","minItems":1,"items":{"type":"object","properties":{
        "id":{"type":"string"},"text":{"type":"string"},
        "kind":{"type":"string","enum":["text","choice","boolean"]},
        "options":{"type":"array","items":{"type":"object","properties":{"label":{"type":"string"},"description":{"type":"string"}},"required":["label"],"additionalProperties":false}},
        "multiSelect":{"type":"boolean"},"allowOther":{"type":"boolean","const":true}
    },"required":["text","kind"],"additionalProperties":false}})
}
pub(crate) fn answer_schema() -> Value {
    json!({"type":"array","minItems":1,"items":{"type":"object","properties":{
        "id":{"type":"string"},"labels":{"type":"array","items":{"type":"string"}},
        "note":{"type":"string"},"other":{"type":"string"},"boolean":{"type":"boolean"},"text":{"type":"string"},"skipped":{"type":"boolean"}
    },"required":["id"],"additionalProperties":false}})
}
pub(crate) fn parse(args: &Value) -> Result<Vec<QuestionSpec>, EngineError> {
    let mut specs: Vec<QuestionSpec> = if let Some(value) = args.get("questions") {
        serde_json::from_value(value.clone())
            .map_err(|e| EngineError::bad(format!("invalid questions: {e}")))?
    } else {
        vec![QuestionSpec {
            id: String::new(),
            text: args["text"]
                .as_str()
                .ok_or_else(|| EngineError::bad("questions required"))?
                .into(),
            kind: QuestionKind::Text,
            options: vec![],
            multi_select: false,
            allow_other: true,
        }]
    };
    if specs.is_empty() {
        return Err(EngineError::bad("at least one question required"));
    }
    let mut ids = BTreeSet::new();
    let reserved = specs
        .iter()
        .filter(|q| !q.id.is_empty())
        .map(|q| q.id.clone())
        .collect::<BTreeSet<_>>();
    for (index, q) in specs.iter_mut().enumerate() {
        if q.id.is_empty() {
            let mut number = index + 1;
            while reserved.contains(&format!("q{number}")) || ids.contains(&format!("q{number}")) {
                number += 1;
            }
            q.id = format!("q{number}");
        }
        if q.text.trim().is_empty() || !ids.insert(q.id.clone()) {
            return Err(EngineError::bad(
                "questions require nonempty text and unique IDs",
            ));
        }
        if !q.allow_other {
            return Err(EngineError::bad(
                "Other is always allowed; allowOther must be true",
            ));
        }
        let labels = q
            .options
            .iter()
            .map(|o| o.label.as_str())
            .collect::<BTreeSet<_>>();
        if q.kind == QuestionKind::Choice {
            if labels.is_empty() || labels.len() != q.options.len() || labels.contains("") {
                return Err(EngineError::bad(
                    "choice questions require unique, nonempty options",
                ));
            }
        } else if q.multi_select || !q.options.is_empty() {
            return Err(EngineError::bad(
                "options and multiSelect apply only to choice questions",
            ));
        }
    }
    Ok(specs)
}
pub(crate) fn text_answers(q: &QuestionRecord, text: &str) -> Vec<QuestionAnswer> {
    q.questions
        .iter()
        .map(|spec| {
            let mut answer = QuestionAnswer {
                id: spec.id.clone(),
                ..Default::default()
            };
            if text == "user unavailable" || spec.kind == QuestionKind::Text {
                answer.text = Some(text.into());
            } else if spec.kind == QuestionKind::Boolean
                && matches!(
                    text.to_ascii_lowercase().as_str(),
                    "true" | "yes" | "false" | "no"
                )
            {
                answer.boolean = Some(matches!(text.to_ascii_lowercase().as_str(), "true" | "yes"));
            } else {
                answer.other = Some(text.into());
            }
            answer
        })
        .collect()
}
pub(crate) fn validate(q: &QuestionRecord, answers: &[QuestionAnswer]) -> Result<(), EngineError> {
    let mut ids = BTreeSet::new();
    if answers.len() != q.questions.len() {
        return Err(EngineError::bad(
            "submit one answer or skipped entry for every question in this set",
        ));
    }
    for a in answers {
        let spec = q
            .questions
            .iter()
            .find(|s| s.id == a.id)
            .ok_or_else(|| EngineError::bad("unknown question ID"))?;
        if !ids.insert(&a.id) {
            return Err(EngineError::bad("duplicate answer ID"));
        }
        let supplied = usize::from(!a.labels.is_empty())
            + usize::from(a.other.is_some())
            + usize::from(a.boolean.is_some())
            + usize::from(a.text.is_some());
        if a.skipped {
            if supplied != 0 || a.note.is_some() {
                return Err(EngineError::bad(
                    "a skipped question cannot also have an answer",
                ));
            }
            continue;
        }
        if a.text.as_deref() == Some("user unavailable") && supplied == 1 {
            continue;
        }
        if supplied == 0 {
            return Err(EngineError::bad("an answer or skipped is required"));
        }
        let valid = match spec.kind {
            QuestionKind::Text => a.text.is_some() && supplied == 1,
            QuestionKind::Boolean => (a.boolean.is_some() || a.other.is_some()) && supplied == 1,
            QuestionKind::Choice => {
                a.boolean.is_none()
                    && a.text.is_none()
                    && (spec.multi_select || a.labels.len() + usize::from(a.other.is_some()) <= 1)
                    && a.labels
                        .iter()
                        .all(|label| spec.options.iter().any(|o| &o.label == label))
                    && a.labels.iter().collect::<BTreeSet<_>>().len() == a.labels.len()
            }
        };
        if !valid {
            return Err(EngineError::bad(format!(
                "answer does not match question {}",
                a.id
            )));
        }
    }
    Ok(())
}
impl Control {
    pub(crate) fn answer_set(
        &self,
        id: &str,
        answers: Vec<QuestionAnswer>,
    ) -> Result<(), EngineError> {
        let envelope = {
            let mut j = self.journal.lock().unwrap();
            let q = j
                .questions
                .values()
                .find(|q| q.id == id || q.call_id.as_deref() == Some(id))
                .ok_or_else(|| EngineError::bad("question set not found"))?;
            if q.answer.is_some() {
                return Err(EngineError::bad("question already answered"));
            }
            validate(q, &answers)?;
            let (asker, id) = (q.asker.clone(), q.id.clone());
            j.append(&asker, HistoryEvent::QuestionsAnswered { id, answers })
        }
        .inspect_err(|e| self.claimed(e))?;
        self.emit(EngineEvent::Committed { envelope });
        self.changed.notify_waiters();
        Ok(())
    }
}
