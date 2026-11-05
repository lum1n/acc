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

#[cfg(test)]
mod tests {
    use super::*;

    fn answers(a: &[&[&str]]) -> Vec<Vec<String>> {
        a.iter()
            .map(|q| q.iter().map(|s| (*s).to_owned()).collect())
            .collect()
    }

    #[test]
    fn single_field_takes_the_message_and_title_as_header() {
        let schema = json!({ "properties": { "name": { "type": "string", "title": "Name" } } });
        let qs = questions(&schema, "What is it called?");
        assert_eq!(qs[0]["question"], "What is it called?");
        assert_eq!(qs[0]["header"], "Name");
        assert_eq!(qs[0]["multiSelect"], false);
        assert_eq!(qs[0]["options"], json!([]));
    }

    #[test]
    fn several_fields_fall_back_title_description_key() {
        let schema = json!({
            "required": ["c"],
            "properties": {
                "a": { "type": "string", "title": "Alpha", "description": "d" },
                "b": { "type": "string", "description": "Beta?" },
                "c": { "type": "string" },
            }
        });
        let qs = questions(&schema, "ignored with several fields");
        let texts: Vec<&str> = qs.iter().map(|q| q["question"].as_str().unwrap()).collect();
        assert_eq!(texts, ["c", "Alpha", "Beta?"], "required first");
        assert!(qs.iter().all(|q| q["header"].is_null()));
    }

    #[test]
    fn choices_read_enum_names_and_titled_alternatives() {
        let named = json!({ "type": "string", "enum": ["a", "b"], "enumNames": ["A"] });
        assert_eq!(
            choices(&named),
            [("a".into(), "A".into(), None), ("b".into(), "b".into(), None)]
        );
        let titled = json!({ "anyOf": [
            { "const": "x", "title": "X", "description": "ex" },
            { "const": "y", "description": "" },
            { "title": "no const" },
        ] });
        assert_eq!(
            choices(&titled),
            [
                ("x".into(), "X".into(), Some("ex".into())),
                ("y".into(), "y".into(), None)
            ]
        );
        let array = json!({ "type": "array", "items": { "enum": ["p"] } });
        assert_eq!(choices(&array).len(), 1);
        assert!(choices(&json!({ "type": "string" })).is_empty());
    }

    #[test]
    fn booleans_are_yes_no_questions_and_answers() {
        let schema = json!({ "properties": { "ok": { "type": "boolean" } } });
        let qs = questions(&schema, "");
        assert_eq!(qs[0]["options"].as_array().unwrap().len(), 2);
        for (said, want) in [("Yes", true), ("TRUE", true), ("no", false), ("maybe", false)] {
            assert_eq!(
                content(&schema, &answers(&[&[said]])),
                json!({ "ok": want }),
                "{said}"
            );
        }
    }

    #[test]
    fn numbers_parse_and_bad_numbers_are_left_out() {
        let schema = json!({ "properties": {
            "n": { "type": "integer" }, "f": { "type": "number" } } });
        let fields: Vec<String> = fields(&schema).into_iter().map(|(k, _)| k).collect();
        let pick = |n: &str, f: &str| {
            let mut a = vec![Vec::new(); 2];
            a[fields.iter().position(|k| k == "n").unwrap()] = vec![n.to_owned()];
            a[fields.iter().position(|k| k == "f").unwrap()] = vec![f.to_owned()];
            content(&schema, &a)
        };
        assert_eq!(pick(" 42 ", "1.5"), json!({ "n": 42, "f": 1.5 }));
        assert_eq!(pick("4.2", "abc"), json!({}));
    }

    #[test]
    fn unanswered_and_blank_questions_are_left_out() {
        let schema = json!({ "properties": { "a": { "type": "string" }, "b": { "type": "string" } } });
        assert_eq!(content(&schema, &answers(&[&[""], &["x"]])), json!({ "b": "x" }));
        assert_eq!(content(&schema, &answers(&[&[]])), json!({}));
        assert_eq!(content(&schema, &[]), json!({}));
        // Free text with several lines is joined.
        assert_eq!(
            content(&schema, &answers(&[&["l1", "l2"]])),
            json!({ "a": "l1\nl2" })
        );
    }

    #[test]
    fn choices_map_by_label_or_value_and_unknowns_pass_through_without_a_companion() {
        let schema = json!({ "properties": { "pm": { "type": "string", "oneOf": [
            { "const": "bun", "title": "Bun" }, { "const": "npm", "title": "npm" } ] } } });
        assert_eq!(content(&schema, &answers(&[&["Bun"]])), json!({ "pm": "bun" }));
        assert_eq!(content(&schema, &answers(&[&["npm"]])), json!({ "pm": "npm" }));
        assert_eq!(content(&schema, &answers(&[&["yarn"]])), json!({ "pm": "yarn" }));
    }

    #[test]
    fn multi_select_typed_answers_go_to_the_companion() {
        let schema = json!({ "properties": {
            "x": { "type": "array", "items": { "oneOf": [{ "const": "a", "title": "A" }] } },
            "x_custom": { "type": "string" },
        } });
        // The companion isn't a question of its own.
        assert_eq!(fields(&schema).len(), 1);
        assert_eq!(
            content(&schema, &answers(&[&["A", "zed", "yo"]])),
            json!({ "x": ["a"], "x_custom": "zed, yo" })
        );
        // Only typed answers: no empty choice array.
        assert_eq!(
            content(&schema, &answers(&[&["zed"]])),
            json!({ "x_custom": "zed" })
        );
    }

    #[test]
    fn schemas_without_properties_have_no_questions() {
        assert!(questions(&json!({}), "hi").is_empty());
        assert_eq!(content(&json!({}), &answers(&[&["x"]])), json!({}));
        // A `_custom` key without a choice to attach to is a question itself.
        let schema = json!({ "properties": { "s_custom": { "type": "string" }, "s": { "type": "string" } } });
        assert_eq!(fields(&schema).len(), 2);
    }
}
