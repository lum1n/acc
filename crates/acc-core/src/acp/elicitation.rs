//! Form elicitations (`elicitation/create`, mode `form`) as ACC questions.
//!
//! The agent asks with a flat JSON schema (`requestedSchema`): one property
//! per question, a string or an array of strings with `oneOf`/`anyOf`
//! choices, or a free string, number or boolean. [`questions`] turns it
//! into the `questions` [`crate::question::parse`] reads, and [`content`]
//! turns the answers back into the form's `content`. opencode adds an
//! optional `<key>_custom` string for typed answers next to a choice; it is
//! folded into its choice, so an answer that isn't one of the options lands
//! there.

use serde_json::{Map, Value, json};

use crate::wire::{owned, str_at};

/// A form's questions, required ones first, in the `questions` shape.
pub(super) fn questions(schema: &Value, message: &str) -> Vec<Value> {
    let fields = fields(schema);
    let single = fields.len() == 1;
    fields
        .iter()
        .map(|(key, prop)| {
            let title = owned(prop, "title").filter(|t| !t.is_empty());
            let description = owned(prop, "description").filter(|d| !d.is_empty());
            let (question, header) = if single && !message.is_empty() {
                (message.to_owned(), title)
            } else {
                let text = title.clone().or(description).unwrap_or_else(|| key.clone());
                (text, None)
            };
            let options: Vec<Value> = choices(prop)
                .into_iter()
                .map(|(_, label, description)| json!({ "label": label, "description": description }))
                .collect();
            json!({
                "id": key,
                "question": question,
                "header": header,
                "options": options,
                "multiSelect": str_at(prop, "type") == Some("array"),
            })
        })
        .collect()
}

/// The form's `content` for per-question answers (see
/// [`crate::question::decode`]), in [`questions`]' order. Unanswered
/// questions are left out.
pub(super) fn content(schema: &Value, answers: &[Vec<String>]) -> Value {
    let props = schema.get("properties").and_then(Value::as_object);
    let mut content = Map::new();
    for ((key, prop), answer) in fields(schema).iter().zip(answers) {
        let answer: Vec<&str> = answer
            .iter()
            .map(String::as_str)
            .filter(|a| !a.is_empty())
            .collect();
        if answer.is_empty() {
            continue;
        }
        let custom_key = format!("{key}_custom");
        let custom = props.is_some_and(|p| p.contains_key(&custom_key));
        let opts = choices(prop);
        let pick = |a: &str| {
            opts.iter()
                .find(|(_, label, _)| label == a)
                .or_else(|| opts.iter().find(|(value, ..)| value == a))
                .map(|(value, ..)| value.clone())
        };
        let mut typed = Vec::new();
        let value = match str_at(prop, "type") {
            Some("array") => {
                let picked: Vec<Value> = answer
                    .iter()
                    .filter_map(|a| match pick(a) {
                        Some(v) => Some(Value::String(v)),
                        None if custom => {
                            typed.push(*a);
                            None
                        }
                        None => Some(Value::String((*a).to_owned())),
                    })
                    .collect();
                Value::Array(picked)
            }
            Some("boolean") => Value::Bool(matches!(
                answer[0].to_ascii_lowercase().as_str(),
                "yes" | "true" | "y" | "1"
            )),
            Some("integer") => match answer[0].trim().parse::<i64>() {
                Ok(n) => json!(n),
                Err(_) => continue,
            },
            Some("number") => match answer[0].trim().parse::<f64>() {
                Ok(n) => json!(n),
                Err(_) => continue,
            },
            _ => match pick(answer[0]) {
                Some(v) => Value::String(v),
                None if custom && !opts.is_empty() => {
                    typed.push(answer[0]);
                    Value::Null
                }
                None => Value::String(answer.join("\n")),
            },
        };
        if !value.is_null() && value.as_array().is_none_or(|a| !a.is_empty() || typed.is_empty()) {
            content.insert(key.clone(), value);
        }
        if !typed.is_empty() {
            content.insert(custom_key, Value::String(typed.join(", ")));
        }
    }
    Value::Object(content)
}

/// The schema's properties, required ones first, without the `<key>_custom`
/// companions of choices.
fn fields(schema: &Value) -> Vec<(String, Value)> {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let companion = |k: &str| {
        k.strip_suffix("_custom")
            .and_then(|base| props.get(base))
            .is_some_and(|p| !choices(p).is_empty())
    };
    let mut keys: Vec<&String> = props.keys().filter(|k| !companion(k)).collect();
    keys.sort_by_key(|k| required.iter().position(|r| r == k).unwrap_or(usize::MAX));
    keys.into_iter().map(|k| (k.clone(), props[k].clone())).collect()
}

/// `(value, label, description)` of a property's choices: `oneOf`/`anyOf`
/// of `{const, title}`, or `enum` (with the legacy `enumNames`), on the
/// property or on its `items`. A boolean is a yes/no choice.
fn choices(prop: &Value) -> Vec<(String, String, Option<String>)> {
    if str_at(prop, "type") == Some("boolean") {
        return vec![
            ("yes".into(), "Yes".into(), None),
            ("no".into(), "No".into(), None),
        ];
    }
    let target = match str_at(prop, "type") {
        Some("array") => prop.get("items").unwrap_or(&Value::Null),
        _ => prop,
    };
    let titled = target
        .get("oneOf")
        .or_else(|| target.get("anyOf"))
        .and_then(Value::as_array);
    if let Some(list) = titled {
        return list
            .iter()
            .filter_map(|o| {
                let value = str_at(o, "const")?.to_owned();
                let label = owned(o, "title").unwrap_or_else(|| value.clone());
                Some((value, label, owned(o, "description").filter(|d| !d.is_empty())))
            })
            .collect();
    }
    let names = target.get("enumNames").and_then(Value::as_array);
    target
        .get("enum")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, v)| {
            let value = v.as_str()?.to_owned();
            let label = names
                .and_then(|n| n.get(i))
                .and_then(Value::as_str)
                .map_or_else(|| value.clone(), str::to_owned);
            Some((value, label, None))
        })
        .collect()
}
