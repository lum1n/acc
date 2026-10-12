//! Multi-part questions: Claude's `AskUserQuestion` and Codex's
//! `requestUserInput` can ask several questions at once, some of them
//! multiple-choice.
//!
//! A question request's [`Permission::input`](crate::session::Permission)
//! holds them; [`parse`] reads them back. Answer them all with
//! [`Command::AnswerPermission`](crate::command::Command) carrying
//! [`encode`]d answers as its `option_id`. A plain option id still answers
//! the first question alone.

use serde_json::Value;

use crate::wire::{owned, str_at};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// Codex's question id; Claude keys answers by the question text.
    pub id: Option<String>,
    pub question: String,
    pub header: Option<String>,
    pub options: Vec<QuestionOption>,
    /// More than one option may be picked.
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

/// The questions in a request's input, in order; empty when it has none.
pub fn parse(input: &Value) -> Vec<Question> {
    let Some(qs) = input.get("questions").and_then(Value::as_array) else {
        return Vec::new();
    };
    qs.iter()
        .map(|q| Question {
            id: owned(q, "id"),
            question: owned(q, "question").unwrap_or_default(),
            header: owned(q, "header").filter(|h| !h.is_empty()),
            options: q
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|o| {
                    Some(QuestionOption {
                        label: str_at(o, "label")?.to_owned(),
                        description: owned(o, "description").filter(|d| !d.is_empty()),
                    })
                })
                .collect(),
            multi_select: q.get("multiSelect").and_then(Value::as_bool) == Some(true),
        })
        .collect()
}

/// Answers to every question (labels or free text), one list per question,
/// as an `option_id`.
pub fn encode(answers: &[Vec<String>]) -> String {
    Value::from(answers.iter().map(|a| Value::from(a.clone())).collect::<Vec<_>>()).to_string()
}

/// An `option_id` back to per-question answers: [`encode`]d, or a single
/// answer to the first question.
pub(crate) fn decode(option_id: &str) -> Vec<Vec<String>> {
    if option_id.starts_with('[')
        && let Ok(v) = serde_json::from_str::<Vec<Vec<String>>>(option_id)
    {
        return v;
    }
    vec![vec![option_id.to_owned()]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_questions_and_round_trips_answers() {
        let input = json!({"questions": [
            {"question": "Which DB?", "header": "DB", "multiSelect": false,
             "options": [{"label": "Postgres", "description": "big"}, {"label": "SQLite", "description": ""}]},
            {"id": "q2", "question": "Features?", "multiSelect": true, "options": [{"label": "A"}, {"label": "B"}]},
        ]});
        let qs = parse(&input);
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].header.as_deref(), Some("DB"));
        assert_eq!(qs[0].options[0].description.as_deref(), Some("big"));
        assert_eq!(qs[0].options[1].description, None);
        assert!(qs[1].multi_select);
        assert_eq!(qs[1].id.as_deref(), Some("q2"));

        let answers = vec![vec!["SQLite".to_owned()], vec!["A".to_owned(), "B".to_owned()]];
        assert_eq!(decode(&encode(&answers)), answers);
        assert_eq!(decode("Postgres"), vec![vec!["Postgres".to_owned()]]);
        assert_eq!(decode("[not json"), vec![vec!["[not json".to_owned()]]);
    }
}
