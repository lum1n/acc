//! ACP session configuration: `configOptions` (ACP 1.8) plus the legacy
//! `modes` and `models` fields that older agents still send.
//!
//! Config options supersede modes and models: agents SHOULD send both and
//! clients that understand config options SHOULD prefer them, so the legacy
//! fields are only used for an [`OptionId`] no config option covers.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::command::{Apply, OptionDef, OptionId};
use crate::harness::Harness;

/// One entry of an agent's `configOptions` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    /// `mode`, `model`, `model_config`, `thought_level`, or a custom `_name`.
    pub category: Option<String>,
    pub value: ConfigValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConfigValue {
    Select {
        current: String,
        /// `(value, display name)`, groups flattened in order.
        choices: Vec<(String, String)>,
    },
    Boolean {
        current: bool,
    },
}

impl ConfigOption {
    /// Parses one option; `None` for shapes this client doesn't know.
    pub fn parse(v: &Value) -> Option<Self> {
        let id = v.get("id")?.as_str()?.to_owned();
        let name = v.get("name").and_then(Value::as_str).unwrap_or(&id).to_owned();
        let category = v.get("category").and_then(Value::as_str).map(str::to_owned);
        let value = match v.get("type")?.as_str()? {
            "select" => {
                let current = v.get("currentValue")?.as_str()?.to_owned();
                let mut choices = Vec::new();
                for item in v.get("options").and_then(Value::as_array).into_iter().flatten() {
                    // A group carries its own `options`; a plain entry carries `value`.
                    let group = item.get("options").and_then(Value::as_array);
                    for o in group.map_or_else(|| std::slice::from_ref(item), Vec::as_slice) {
                        if let Some(val) = o.get("value").and_then(Value::as_str) {
                            let label = o.get("name").and_then(Value::as_str).unwrap_or(val);
                            choices.push((val.to_owned(), label.to_owned()));
                        }
                    }
                }
                ConfigValue::Select { current, choices }
            }
            "boolean" => ConfigValue::Boolean {
                current: v.get("currentValue")?.as_bool()?,
            },
            _ => return None,
        };
        Some(Self {
            id,
            name,
            category,
            value,
        })
    }

    /// Parses a `configOptions` array, skipping entries it can't read.
    pub fn parse_list(v: &Value) -> Vec<Self> {
        v.as_array()
            .map(|a| a.iter().filter_map(Self::parse).collect())
            .unwrap_or_default()
    }

    /// The ACC option this config option controls, if any.
    fn option_id(&self) -> Option<OptionId> {
        match (self.category.as_deref(), self.id.as_str()) {
            (Some("model"), _) => Some(OptionId::Model),
            (Some("thought_level"), _) | (_, "effort" | "thought_level" | "reasoning_effort") => {
                Some(OptionId::Effort)
            }
            (Some("mode"), _) => Some(OptionId::Mode),
            // Copilot exposes approvals as `allow_all` (category `permissions`, verify).
            (Some("permissions" | "_permissions"), _) | (_, "allow_all") => Some(OptionId::Permission),
            (None, "model") => Some(OptionId::Model),
            (None, "mode") => Some(OptionId::Mode),
            _ => None,
        }
    }

    fn values(&self) -> Vec<String> {
        match &self.value {
            ConfigValue::Select { choices, .. } => choices.iter().map(|(v, _)| v.clone()).collect(),
            ConfigValue::Boolean { .. } => vec!["true".into(), "false".into()],
        }
    }

    /// Display names, only when one differs from its value.
    fn labels(&self) -> Vec<String> {
        match &self.value {
            ConfigValue::Select { choices, .. } if choices.iter().any(|(v, l)| v != l) => {
                choices.iter().map(|(_, l)| l.clone()).collect()
            }
            _ => Vec::new(),
        }
    }

    fn current(&self) -> String {
        match &self.value {
            ConfigValue::Select { current, .. } => current.clone(),
            ConfigValue::Boolean { current } => current.to_string(),
        }
    }

    fn set_current(&mut self, value: &str) {
        match &mut self.value {
            ConfigValue::Select { current, .. } => *current = value.to_owned(),
            ConfigValue::Boolean { current } => *current = value == "true",
        }
    }
}

/// A `{currentId, available:[{id, name}]}` pair from the legacy fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Legacy {
    pub current: String,
    pub available: Vec<String>,
    /// Display names for `available`, empty when none differs from its id.
    pub names: Vec<String>,
}

impl Legacy {
    /// `modes: {currentModeId, availableModes:[{id}]}`.
    fn modes(v: &Value) -> Option<Self> {
        Self::parse(v, "currentModeId", "availableModes", "id")
    }

    /// `models: {currentModelId, availableModels:[{modelId}]}` (pre-1.8 unstable).
    fn models(v: &Value) -> Option<Self> {
        Self::parse(v, "currentModelId", "availableModels", "modelId")
    }

    fn parse(v: &Value, current: &str, list: &str, key: &str) -> Option<Self> {
        let (available, mut names): (Vec<String>, Vec<String>) = v
            .get(list)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let id = m.get(key).and_then(Value::as_str)?;
                let name = m.get("name").and_then(Value::as_str).unwrap_or(id);
                Some((id.to_owned(), name.to_owned()))
            })
            .unzip();
        if available == names {
            names.clear();
        }
        Some(Self {
            current: v.get(current)?.as_str()?.to_owned(),
            available,
            names,
        })
    }
}

/// How an [`OptionId`] reaches the agent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Route {
    /// `session/set_config_option`.
    Config { config_id: String, boolean: bool },
    /// Legacy `session/set_mode`.
    LegacyMode,
    /// Legacy `session/set_model` (still accepted by Copilot 1.0.95).
    LegacyModel,
    /// Only settable at spawn (`agent --model X acp`); applies on restart.
    SpawnFlag,
}

/// Everything the agent has told us about session configuration.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct OptionState {
    pub config: Vec<ConfigOption>,
    pub modes: Option<Legacy>,
    pub models: Option<Legacy>,
    /// Model passed on the command line at the next (re)start.
    pub spawn_model: Option<String>,
    /// Derived view, rebuilt on every change.
    pub defs: Vec<OptionDef>,
    pub routes: Vec<(OptionId, Route)>,
}

impl OptionState {
    /// Absorbs `configOptions`, `modes` and `models` from a session response.
    pub fn absorb_response(&mut self, result: &Value) {
        if let Some(list) = result.get("configOptions").filter(|v| v.is_array()) {
            self.config = ConfigOption::parse_list(list);
        }
        if let Some(m) = result.get("modes").and_then(Legacy::modes) {
            self.modes = Some(m);
        }
        if let Some(m) = result.get("models").and_then(Legacy::models) {
            self.models = Some(m);
        }
    }

    pub fn set_current_mode(&mut self, mode: &str) {
        if let Some(m) = &mut self.modes {
            m.current = mode.to_owned();
        }
        for opt in &mut self.config {
            if opt.option_id() == Some(OptionId::Mode) {
                opt.set_current(mode);
            }
        }
    }

    pub fn set_current_model(&mut self, model: &str) {
        if let Some(m) = &mut self.models {
            m.current = model.to_owned();
        }
    }

    /// The current model, for `SessionStarted`.
    pub fn current_model(&self) -> Option<String> {
        self.defs
            .iter()
            .find(|d| d.id == OptionId::Model)
            .and_then(|d| d.current.clone())
    }

    pub fn rebuild(&mut self, harness: Harness) {
        self.defs.clear();
        self.routes.clear();
        for opt in &self.config {
            let Some(id) = opt.option_id() else { continue };
            if self.routes.iter().any(|(r, _)| *r == id) {
                continue;
            }
            let apply = if harness == Harness::Cursor && id == OptionId::Model {
                Apply::Restart
            } else {
                Apply::Live
            };
            self.defs.push(OptionDef {
                id,
                values: opt.values(),
                labels: opt.labels(),
                current: Some(opt.current()),
                apply,
            });
            let boolean = matches!(opt.value, ConfigValue::Boolean { .. });
            self.routes.push((
                id,
                Route::Config {
                    config_id: opt.id.clone(),
                    boolean,
                },
            ));
        }
        let legacy = [
            (OptionId::Mode, &self.modes, Route::LegacyMode),
            (OptionId::Model, &self.models, Route::LegacyModel),
        ];
        for (id, legacy, route) in legacy {
            let Some(l) = legacy else { continue };
            if self.routes.iter().any(|(r, _)| *r == id) {
                continue;
            }
            let route = if harness == Harness::Cursor && id == OptionId::Model {
                Route::SpawnFlag
            } else {
                route
            };
            let apply = if route == Route::SpawnFlag {
                Apply::Restart
            } else {
                Apply::Live
            };
            self.defs.push(OptionDef {
                id,
                values: l.available.clone(),
                labels: l.names.clone(),
                current: Some(l.current.clone()),
                apply,
            });
            self.routes.push((id, route));
        }
        // Cursor can always take a model at spawn, even when ACP lists none.
        if harness == Harness::Cursor && !self.routes.iter().any(|(r, _)| *r == OptionId::Model) {
            self.defs.push(OptionDef {
                id: OptionId::Model,
                values: Vec::new(),
                labels: Vec::new(),
                current: self.spawn_model.clone(),
                apply: Apply::Restart,
            });
            self.routes.push((OptionId::Model, Route::SpawnFlag));
        }
    }

    pub fn route(&self, id: OptionId) -> Option<&Route> {
        self.routes.iter().find(|(r, _)| *r == id).map(|(_, route)| route)
    }
}

/// `session/set_config_option` params for a select or boolean option.
pub(crate) fn set_config_params(session_id: &str, config_id: &str, value: &str, boolean: bool) -> Value {
    if boolean {
        json!({
            "sessionId": session_id,
            "configId": config_id,
            "type": "boolean",
            "value": value == "true",
        })
    } else {
        json!({ "sessionId": session_id, "configId": config_id, "value": value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copilot_like() -> Value {
        json!({
            "configOptions": [
                {"id": "mode", "name": "Mode", "category": "mode", "type": "select",
                 "currentValue": "https://agentclientprotocol.com/protocol/session-modes#agent",
                 "options": [
                    {"value": "https://agentclientprotocol.com/protocol/session-modes#agent", "name": "Agent"},
                    {"value": "https://agentclientprotocol.com/protocol/session-modes#plan", "name": "Plan"}]},
                {"id": "model", "name": "Model", "category": "model", "type": "select",
                 "currentValue": "gpt-5.4",
                 "options": [{"group": "openai", "name": "OpenAI", "options": [
                    {"value": "gpt-5.4", "name": "GPT-5.4"}, {"value": "gpt-5.4-mini", "name": "GPT-5.4 mini"}]}]},
                {"id": "allow_all", "name": "Allow all", "category": "permissions", "type": "boolean",
                 "currentValue": false},
                {"id": "_agent", "name": "Agent", "category": "_agent", "type": "select",
                 "currentValue": "default", "options": []},
                {"id": "future", "name": "Future", "type": "slider", "currentValue": 3}
            ],
            "modes": {"currentModeId": "legacy", "availableModes": [{"id": "legacy", "name": "L"}]}
        })
    }

    #[test]
    fn maps_categories_and_flattens_groups() {
        let mut s = OptionState::default();
        s.absorb_response(&copilot_like());
        s.rebuild(Harness::Copilot);
        assert_eq!(s.config.len(), 4, "unknown option types are skipped");
        let model = s.defs.iter().find(|d| d.id == OptionId::Model).expect("model");
        assert_eq!(model.values, ["gpt-5.4", "gpt-5.4-mini"]);
        assert_eq!(model.apply, Apply::Live);
        let perm = s
            .defs
            .iter()
            .find(|d| d.id == OptionId::Permission)
            .expect("permission");
        assert_eq!(perm.current.as_deref(), Some("false"));
        assert_eq!(
            s.route(OptionId::Permission),
            Some(&Route::Config {
                config_id: "allow_all".into(),
                boolean: true
            })
        );
        // The config option wins over the legacy `modes` field.
        assert_eq!(
            s.route(OptionId::Mode),
            Some(&Route::Config {
                config_id: "mode".into(),
                boolean: false
            })
        );
        assert_eq!(s.current_model().as_deref(), Some("gpt-5.4"));
    }

    #[test]
    fn legacy_fields_fill_gaps() {
        let mut s = OptionState::default();
        s.absorb_response(&json!({
            "modes": {"currentModeId": "build", "availableModes": [{"id": "build"}, {"id": "plan"}]},
            "models": {"currentModelId": "m1", "availableModels": [{"modelId": "m1"}, {"modelId": "m2"}]}
        }));
        s.rebuild(Harness::Opencode);
        assert_eq!(s.route(OptionId::Mode), Some(&Route::LegacyMode));
        assert_eq!(s.route(OptionId::Model), Some(&Route::LegacyModel));
        s.set_current_mode("plan");
        s.rebuild(Harness::Opencode);
        let mode = s.defs.iter().find(|d| d.id == OptionId::Mode).expect("mode");
        assert_eq!(mode.current.as_deref(), Some("plan"));
    }

    #[test]
    fn cursor_model_is_a_restart_option() {
        let mut s = OptionState::default();
        s.rebuild(Harness::Cursor);
        assert_eq!(s.route(OptionId::Model), Some(&Route::SpawnFlag));
        let model = s.defs.iter().find(|d| d.id == OptionId::Model).expect("model");
        assert_eq!(model.apply, Apply::Restart);
    }

    #[test]
    fn boolean_params_carry_type() {
        let p = set_config_params("s", "allow_all", "true", true);
        assert_eq!(p["type"], "boolean");
        assert_eq!(p["value"], true);
        let p = set_config_params("s", "model", "x", false);
        assert!(p.get("type").is_none());
    }
}
