//! Typed action names, argument validation, and the built-in action registry.
//!
//! The daemon executes actions; this module only owns their schema. Frontends
//! never hard-code a shortcut's behaviour — they invoke names from this
//! registry, so GUI, terminal, CLI, and keymaps share one vocabulary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActionId(pub String);

impl ActionId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ActionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgKind {
    String,
    U64,
    I32,
    Bool,
    Enum,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArgSpec {
    pub name: String,
    pub kind: ArgKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub allowed: Vec<String>,
}

impl ArgSpec {
    pub fn string(name: &str, required: bool) -> Self {
        Self {
            name: name.into(),
            kind: ArgKind::String,
            required,
            allowed: Vec::new(),
        }
    }

    pub fn u64(name: &str, required: bool) -> Self {
        Self {
            name: name.into(),
            kind: ArgKind::U64,
            required,
            allowed: Vec::new(),
        }
    }

    pub fn i32(name: &str, required: bool) -> Self {
        Self {
            name: name.into(),
            kind: ArgKind::I32,
            required,
            allowed: Vec::new(),
        }
    }

    pub fn boolean(name: &str, required: bool) -> Self {
        Self {
            name: name.into(),
            kind: ArgKind::Bool,
            required,
            allowed: Vec::new(),
        }
    }

    pub fn enumeration(name: &str, allowed: &[&str]) -> Self {
        Self {
            name: name.into(),
            kind: ArgKind::Enum,
            required: true,
            allowed: allowed.iter().map(|value| (*value).to_owned()).collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActionDefinition {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub args: Vec<ArgSpec>,
}

impl ActionDefinition {
    pub fn new(name: &str, title: &str) -> Self {
        Self {
            name: name.into(),
            title: title.into(),
            description: None,
            category: None,
            args: Vec::new(),
        }
    }

    pub fn with_category(mut self, category: &str) -> Self {
        self.category = Some(category.into());
        self
    }

    pub fn with_description(mut self, description: &str) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_args(mut self, args: Vec<ArgSpec>) -> Self {
        self.args = args;
        self
    }

    pub fn is_internal(&self) -> bool {
        self.name.starts_with('_')
    }

    /// Reject malformed arguments before any side effect happens.
    pub fn validate(&self, args: Option<&serde_json::Value>) -> Result<(), ActionError> {
        let object = match args {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::Object(map)) => Some(map),
            Some(_) => {
                return Err(ActionError::InvalidArgs {
                    action: self.name.clone(),
                    reason: "arguments must be a JSON object".into(),
                });
            }
        };
        for spec in &self.args {
            let value = object.and_then(|map| map.get(&spec.name));
            match value {
                None if spec.required => {
                    return Err(ActionError::InvalidArgs {
                        action: self.name.clone(),
                        reason: format!("missing required argument {:?}", spec.name),
                    });
                }
                None => {}
                Some(value) => {
                    let valid = match spec.kind {
                        ArgKind::String => value.is_string(),
                        ArgKind::U64 => value.as_u64().is_some(),
                        ArgKind::I32 => value
                            .as_i64()
                            .is_some_and(|v| v >= i32::MIN as i64 && v <= i32::MAX as i64),
                        ArgKind::Bool => value.is_boolean(),
                        ArgKind::Enum => value.as_str().is_some_and(|value| {
                            spec.allowed.iter().any(|allowed| allowed == value)
                        }),
                    };
                    if !valid {
                        return Err(ActionError::InvalidArgs {
                            action: self.name.clone(),
                            reason: match spec.kind {
                                ArgKind::Enum => format!(
                                    "{:?} must be one of {}",
                                    spec.name,
                                    spec.allowed.join(", ")
                                ),
                                other => {
                                    format!("{:?} has the wrong type for {other:?}", spec.name)
                                }
                            },
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ActionError {
    #[error("unknown action {0:?}")]
    Unknown(String),
    #[error("invalid arguments for {action:?}: {reason}")]
    InvalidArgs { action: String, reason: String },
}

#[derive(Clone, Debug, Default)]
pub struct ActionRegistry {
    actions: BTreeMap<String, ActionDefinition>,
}

impl ActionRegistry {
    pub fn new(definitions: impl IntoIterator<Item = ActionDefinition>) -> Self {
        let actions = definitions
            .into_iter()
            .map(|definition| (definition.name.clone(), definition))
            .collect();
        Self { actions }
    }

    pub fn builtin() -> Self {
        Self::new([
            ActionDefinition::new("_prefix", "Prefix key")
                .with_description("Internal sequence prefix; not user-invokable.")
                .with_category("internal"),
            ActionDefinition::new("pane.split", "Split pane")
                .with_description("Split the focused pane along an axis.")
                .with_category("layout")
                .with_args(vec![
                    ArgSpec::enumeration("axis", &["columns", "rows"]),
                    ArgSpec::u64("ratio_bps", true),
                ]),
            ActionDefinition::new("pane.focus-left", "Focus pane to the left")
                .with_category("focus"),
            ActionDefinition::new("pane.focus-right", "Focus pane to the right")
                .with_category("focus"),
            ActionDefinition::new("pane.focus-up", "Focus pane above").with_category("focus"),
            ActionDefinition::new("pane.focus-down", "Focus pane below").with_category("focus"),
            ActionDefinition::new("pane.close", "Close pane").with_category("layout"),
            ActionDefinition::new("pane.zoom-toggle", "Zoom or unzoom pane")
                .with_category("layout"),
            ActionDefinition::new("pane.resize-left", "Resize viewport left")
                .with_category("resize"),
            ActionDefinition::new("pane.resize-right", "Resize viewport right")
                .with_category("resize"),
            ActionDefinition::new("pane.resize-up", "Resize viewport up").with_category("resize"),
            ActionDefinition::new("pane.resize-down", "Resize viewport down")
                .with_category("resize"),
            ActionDefinition::new("mode.resize", "Enter resize mode").with_category("mode"),
            ActionDefinition::new("mode.exit", "Exit current mode").with_category("mode"),
            ActionDefinition::new("client.detach", "Detach client").with_category("client"),
            ActionDefinition::new("help.which-key", "Show key hints").with_category("help"),
        ])
    }

    pub fn get(&self, name: &str) -> Result<&ActionDefinition, ActionError> {
        self.actions
            .get(name)
            .ok_or_else(|| ActionError::Unknown(name.into()))
    }

    pub fn validate(
        &self,
        name: &str,
        args: Option<&serde_json::Value>,
    ) -> Result<(), ActionError> {
        self.get(name)?.validate(args)
    }

    /// Public actions only; internal prefix markers never appear in palettes.
    pub fn list(&self) -> Vec<&ActionDefinition> {
        self.actions
            .values()
            .filter(|action| !action.is_internal())
            .collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.actions.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builtin_registry_exposes_the_documented_actions() {
        let registry = ActionRegistry::builtin();
        for name in [
            "pane.split",
            "pane.focus-left",
            "pane.close",
            "pane.zoom-toggle",
            "client.detach",
            "help.which-key",
            "mode.resize",
            "mode.exit",
        ] {
            assert!(registry.contains(name), "missing {name}");
        }
        assert!(!registry.list().iter().any(|action| action.is_internal()));
    }

    #[test]
    fn arguments_are_validated_before_execution() {
        let registry = ActionRegistry::builtin();
        let valid = json!({"axis": "columns", "ratio_bps": 5000});
        assert!(registry.validate("pane.split", Some(&valid)).is_ok());
        let wrong_axis = json!({"axis": "diagonal", "ratio_bps": 5000});
        assert!(matches!(
            registry.validate("pane.split", Some(&wrong_axis)),
            Err(ActionError::InvalidArgs { .. })
        ));
        let missing = json!({"axis": "columns"});
        assert!(matches!(
            registry.validate("pane.split", Some(&missing)),
            Err(ActionError::InvalidArgs { .. })
        ));
        assert!(matches!(
            registry.validate("pane.split", Some(&json!("not an object"))),
            Err(ActionError::InvalidArgs { .. })
        ));
        assert!(matches!(
            registry.validate("pane.nope", None),
            Err(ActionError::Unknown(_))
        ));
    }
}
