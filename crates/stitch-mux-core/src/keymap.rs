//! Bounded key tables and sequence dispatch shared by all frontends.
//!
//! Key strings are canonical (`C-b`, `M-x`, `Up`, `F1`, or a literal single
//! character). The daemon owns the effective table; clients only translate
//! host key events into these names and ask the dispatcher what to do.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::action::ActionError;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeySpec(String);

impl KeySpec {
    pub fn parse(value: &str) -> Result<Self, KeymapError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(KeymapError::InvalidKey { key: value.into() });
        }
        let mut modifiers = BTreeSet::new();
        let mut rest = trimmed;
        while let Some((prefix, tail)) = rest.split_once('-') {
            let modifier = match prefix {
                "C" | "Ctrl" | "ctrl" => "C",
                "M" | "Alt" | "alt" => "M",
                "S" | "Shift" | "shift" => "S",
                _ => break,
            };
            if !modifiers.insert(modifier) {
                return Err(KeymapError::InvalidKey { key: value.into() });
            }
            rest = tail;
        }
        let key =
            canonical_key(rest).ok_or_else(|| KeymapError::InvalidKey { key: value.into() })?;
        let mut canonical = String::new();
        for modifier in modifiers {
            canonical.push_str(modifier);
            canonical.push('-');
        }
        canonical.push_str(&key);
        Ok(Self(canonical))
    }

    /// Build a key from frontend-neutral parts. `modifiers` accepts `ctrl`,
    /// `alt`, and `shift` (or their short forms).
    pub fn from_parts(key: &str, modifiers: &[&str]) -> Result<Self, KeymapError> {
        let mut canonical = String::new();
        let mut seen = BTreeSet::new();
        for modifier in modifiers {
            let short = match modifier.to_ascii_lowercase().as_str() {
                "c" | "ctrl" | "control" => "C",
                "m" | "alt" => "M",
                "s" | "shift" => "S",
                other => {
                    return Err(KeymapError::InvalidKey {
                        key: format!("{other}-{key}"),
                    });
                }
            };
            if seen.insert(short) {
                canonical.push_str(short);
                canonical.push('-');
            }
        }
        let key = canonical_key(key).ok_or_else(|| KeymapError::InvalidKey {
            key: key.to_owned(),
        })?;
        canonical.push_str(&key);
        Ok(Self(canonical))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn key(&self) -> &str {
        self.0.rsplit('-').next().unwrap_or(&self.0)
    }

    pub fn modifiers(&self) -> impl Iterator<Item = &str> {
        self.0.split('-').take(self.0.matches('-').count())
    }
}

fn canonical_key(value: &str) -> Option<String> {
    const NAMES: &[(&str, &str)] = &[
        ("enter", "Enter"),
        ("return", "Enter"),
        ("esc", "Esc"),
        ("escape", "Esc"),
        ("tab", "Tab"),
        ("space", "Space"),
        ("backspace", "Backspace"),
        ("bs", "Backspace"),
        ("delete", "Delete"),
        ("del", "Delete"),
        ("insert", "Insert"),
        ("ins", "Insert"),
        ("up", "Up"),
        ("down", "Down"),
        ("left", "Left"),
        ("right", "Right"),
        ("home", "Home"),
        ("end", "End"),
        ("pageup", "PageUp"),
        ("page_up", "PageUp"),
        ("pgup", "PageUp"),
        ("pagedown", "PageDown"),
        ("page_down", "PageDown"),
        ("pgdn", "PageDown"),
    ];
    let lowered = value.to_ascii_lowercase();
    if let Some((_, canonical)) = NAMES.iter().find(|(name, _)| *name == lowered) {
        return Some((*canonical).to_owned());
    }
    if lowered.len() == 2
        && lowered.starts_with('f')
        && lowered[1..]
            .parse::<u8>()
            .is_ok_and(|number| (1..=35).contains(&number))
    {
        return match lowered.as_str() {
            "f1" => Some("F1"),
            "f2" => Some("F2"),
            "f3" => Some("F3"),
            "f4" => Some("F4"),
            "f5" => Some("F5"),
            "f6" => Some("F6"),
            "f7" => Some("F7"),
            "f8" => Some("F8"),
            "f9" => Some("F9"),
            "f10" => Some("F10"),
            "f11" => Some("F11"),
            "f12" => Some("F12"),
            _ => None,
        }
        .map(str::to_owned);
    }
    let mut chars = value.chars();
    let first = chars.next()?;
    if chars.next().is_some() || first.is_control() {
        return None;
    }
    Some(first.to_string())
}

impl std::fmt::Display for KeySpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Error)]
pub enum KeymapError {
    #[error("invalid key {key:?}")]
    InvalidKey { key: String },
    #[error("binding {keys:?} uses {error}")]
    InvalidBinding { keys: String, error: ActionError },
    #[error("duplicate binding for mode {mode:?}, keys {keys:?}")]
    Duplicate { mode: String, keys: String },
    #[error("unknown mode {0:?}")]
    UnknownMode(String),
    #[error("cannot read keymap: {0}")]
    Io(#[from] std::io::Error),
    #[error("no keymap path is configured")]
    NoPath,
    #[error("invalid keymap JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeyBinding {
    pub keys: Vec<KeySpec>,
    pub action: String,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
    /// `None` is the base layer. Named modes override base bindings.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default = "builtin_source")]
    pub source: String,
}

fn builtin_source() -> String {
    "builtin".into()
}

impl KeyBinding {
    pub fn new(keys: &[&str], action: &str) -> Result<Self, KeymapError> {
        Ok(Self {
            keys: keys
                .iter()
                .map(|key| KeySpec::parse(key))
                .collect::<Result<_, _>>()?,
            action: action.into(),
            args: None,
            mode: None,
            source: builtin_source(),
        })
    }

    pub fn with_args(mut self, args: serde_json::Value) -> Self {
        self.args = Some(args);
        self
    }

    pub fn in_mode(mut self, mode: &str) -> Self {
        self.mode = Some(mode.into());
        self
    }

    pub fn from_file(mut self) -> Self {
        self.source = "user".into();
        self
    }

    pub fn sequence(&self) -> String {
        self.keys
            .iter()
            .map(KeySpec::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeyMode {
    pub name: String,
    #[serde(default)]
    pub exclusive: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Keymap {
    #[serde(default)]
    pub bindings: Vec<KeyBinding>,
    #[serde(default)]
    pub modes: Vec<KeyMode>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct KeymapConfig {
    #[serde(default)]
    bindings: Vec<BindingConfig>,
    #[serde(default)]
    modes: Vec<KeyMode>,
}

#[derive(Clone, Debug, Deserialize)]
struct BindingConfig {
    keys: Vec<String>,
    action: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
    #[serde(default)]
    mode: Option<String>,
}

impl Keymap {
    pub fn builtin() -> Self {
        let mut bindings = vec![
            KeyBinding::new(&["C-b"], "_prefix").expect("prefix key"),
            KeyBinding::new(&["C-b", "%"], "pane.split")
                .expect("split")
                .with_args(serde_json::json!({"axis": "columns", "ratio_bps": 5000})),
            KeyBinding::new(&["C-b", "\""], "pane.split")
                .expect("split")
                .with_args(serde_json::json!({"axis": "rows", "ratio_bps": 5000})),
            KeyBinding::new(&["C-b", "Left"], "pane.focus-left").expect("focus"),
            KeyBinding::new(&["C-b", "Right"], "pane.focus-right").expect("focus"),
            KeyBinding::new(&["C-b", "Up"], "pane.focus-up").expect("focus"),
            KeyBinding::new(&["C-b", "Down"], "pane.focus-down").expect("focus"),
            KeyBinding::new(&["C-b", "x"], "pane.close").expect("close"),
            KeyBinding::new(&["C-b", "z"], "pane.zoom-toggle").expect("zoom"),
            KeyBinding::new(&["C-b", "d"], "client.detach").expect("detach"),
            KeyBinding::new(&["C-b", "?"], "help.which-key").expect("help"),
            KeyBinding::new(&["C-b", "["], "prompt.previous").expect("previous prompt"),
            KeyBinding::new(&["C-b", "]"], "prompt.next").expect("next prompt"),
            KeyBinding::new(&["C-b", "r"], "mode.resize").expect("resize mode"),
            KeyBinding::new(&["Esc"], "mode.exit")
                .expect("escape")
                .in_mode("resize"),
            KeyBinding::new(&["C-b"], "mode.exit")
                .expect("prefix exit")
                .in_mode("resize"),
            KeyBinding::new(&["Left"], "pane.resize-left")
                .expect("resize")
                .in_mode("resize"),
            KeyBinding::new(&["Right"], "pane.resize-right")
                .expect("resize")
                .in_mode("resize"),
            KeyBinding::new(&["Up"], "pane.resize-up")
                .expect("resize")
                .in_mode("resize"),
            KeyBinding::new(&["Down"], "pane.resize-down")
                .expect("resize")
                .in_mode("resize"),
        ];
        for (index, binding) in bindings.iter_mut().enumerate() {
            binding.source = format!("builtin:{index}");
        }
        Self {
            bindings,
            modes: vec![KeyMode {
                name: "resize".into(),
                exclusive: true,
            }],
        }
    }

    /// Apply user bindings on top of the built-ins; same mode + same sequence
    /// replaces the built-in entry while keeping its provenance.
    pub fn with_user_json(
        mut self,
        json: &str,
        registry: &crate::action::ActionRegistry,
    ) -> Result<Self, KeymapError> {
        let config: KeymapConfig = serde_json::from_str(json)?;
        for mode in config.modes {
            if self.modes.iter().any(|existing| existing.name == mode.name) {
                self.modes.retain(|existing| existing.name != mode.name);
            }
            self.modes.push(mode);
        }
        for binding in config.bindings {
            let keys = binding
                .keys
                .iter()
                .map(|key| KeySpec::parse(key))
                .collect::<Result<Vec<_>, _>>()?;
            registry
                .validate(&binding.action, binding.args.as_ref())
                .map_err(|error| KeymapError::InvalidBinding {
                    keys: binding.keys.join(" "),
                    error,
                })?;
            let resolved = KeyBinding {
                keys,
                action: binding.action,
                args: binding.args,
                mode: binding.mode,
                source: "user".into(),
            };
            self.bindings.retain(|existing| {
                !(existing.mode == resolved.mode && existing.keys == resolved.keys)
            });
            self.bindings.push(resolved);
        }
        self.validate(registry)?;
        Ok(self)
    }

    pub fn validate(&self, registry: &crate::action::ActionRegistry) -> Result<(), KeymapError> {
        let mut seen = BTreeSet::new();
        let mode_names: BTreeSet<_> = self.modes.iter().map(|mode| mode.name.as_str()).collect();
        for binding in &self.bindings {
            if binding.keys.is_empty() {
                return Err(KeymapError::InvalidKey { key: String::new() });
            }
            if let Some(mode) = &binding.mode
                && !mode_names.contains(mode.as_str())
            {
                return Err(KeymapError::UnknownMode(mode.clone()));
            }
            let mode = binding.mode.clone().unwrap_or_default();
            if !seen.insert((mode.clone(), binding.keys.clone())) {
                return Err(KeymapError::Duplicate {
                    mode,
                    keys: binding.sequence(),
                });
            }
            registry
                .validate(&binding.action, binding.args.as_ref())
                .map_err(|error| KeymapError::InvalidBinding {
                    keys: binding.sequence(),
                    error,
                })?;
        }
        Ok(())
    }

    pub fn is_exclusive(&self, mode: &str) -> bool {
        self.modes
            .iter()
            .find(|entry| entry.name == mode)
            .is_some_and(|entry| entry.exclusive)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionInvocation {
    pub action: String,
    pub args: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    Action(ActionInvocation),
    NeedMore { hints: Vec<String> },
    Passthrough(Vec<KeySpec>),
    ModeEntered(String),
    ModeExited,
}

/// Client-local sequence state. It never mutates the effective keymap.
#[derive(Clone, Debug)]
pub struct KeyDispatcher {
    keymap: Keymap,
    pending: Vec<KeySpec>,
    mode: Option<String>,
}

impl KeyDispatcher {
    pub fn new(keymap: Keymap) -> Self {
        Self {
            keymap,
            pending: Vec::new(),
            mode: None,
        }
    }

    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
        self.pending.clear();
        self.mode = None;
    }

    pub fn active_mode(&self) -> Option<&str> {
        self.mode.as_deref()
    }

    pub fn pending(&self) -> &[KeySpec] {
        &self.pending
    }

    /// Short, bounded list of possible continuations for which-key hints.
    pub fn hints(&self) -> Vec<String> {
        let prefix: Vec<_> = self
            .pending
            .iter()
            .map(|key| key.as_str())
            .collect::<Vec<_>>();
        let prefix_len = prefix.len();
        let mut hints = Vec::new();
        for binding in self.visible_bindings() {
            if binding.keys.len() <= prefix_len
                || binding.keys[..prefix_len]
                    .iter()
                    .map(KeySpec::as_str)
                    .ne(prefix.iter().copied())
            {
                continue;
            }
            let remaining = binding.keys[prefix_len..]
                .iter()
                .map(KeySpec::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            hints.push(format!(
                "{} -> {} [{}]",
                remaining, binding.action, binding.source
            ));
            if hints.len() == 16 {
                break;
            }
        }
        hints
    }

    pub fn dispatch(&mut self, key: KeySpec) -> DispatchOutcome {
        let mut sequence = self.pending.clone();
        sequence.push(key.clone());

        if let Some(binding) = self.find_exact(&sequence).cloned() {
            self.pending.clear();
            return match binding.action.as_str() {
                "_prefix" => {
                    self.pending = sequence;
                    DispatchOutcome::NeedMore {
                        hints: self.hints(),
                    }
                }
                "mode.exit" => {
                    self.mode = None;
                    DispatchOutcome::ModeExited
                }
                action if action.starts_with("mode.") => {
                    let mode = action.trim_start_matches("mode.").to_owned();
                    if self.keymap.modes.iter().any(|entry| entry.name == mode) {
                        self.mode = Some(mode.clone());
                        DispatchOutcome::ModeEntered(mode)
                    } else {
                        DispatchOutcome::Passthrough(vec![key])
                    }
                }
                _ => DispatchOutcome::Action(ActionInvocation {
                    action: binding.action,
                    args: binding.args,
                }),
            };
        }

        if self.pending.len() == 1 && self.pending[0] == key {
            // Double prefix sends the literal prefix through to the pane.
            let passthrough = vec![self.pending[0].clone(), key];
            self.pending.clear();
            return DispatchOutcome::Passthrough(passthrough);
        }

        if self.has_prefix(&sequence) {
            self.pending = sequence;
            return DispatchOutcome::NeedMore {
                hints: self.hints(),
            };
        }

        if !self.pending.is_empty() {
            let mut passthrough = std::mem::take(&mut self.pending);
            passthrough.push(key);
            return DispatchOutcome::Passthrough(passthrough);
        }

        DispatchOutcome::Passthrough(vec![key])
    }

    fn visible_bindings(&self) -> Vec<&KeyBinding> {
        let mode = self.mode.as_deref();
        let mut result: Vec<_> = self
            .keymap
            .bindings
            .iter()
            .filter(|binding| binding.mode.as_deref() == mode)
            .collect();
        if mode.is_some() && !self.keymap.is_exclusive(mode.expect("mode")) {
            result.extend(
                self.keymap
                    .bindings
                    .iter()
                    .filter(|binding| binding.mode.is_none()),
            );
        }
        result
    }

    fn find_exact(&self, sequence: &[KeySpec]) -> Option<&KeyBinding> {
        self.visible_bindings()
            .into_iter()
            .find(|binding| binding.keys == sequence)
    }

    fn has_prefix(&self, sequence: &[KeySpec]) -> bool {
        self.visible_bindings().into_iter().any(|binding| {
            binding.keys.len() > sequence.len() && binding.keys.starts_with(sequence)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::ActionRegistry;
    use serde_json::json;

    fn dispatcher() -> KeyDispatcher {
        let keymap = Keymap::builtin();
        keymap
            .validate(&ActionRegistry::builtin())
            .expect("builtin keymap");
        KeyDispatcher::new(keymap)
    }

    #[test]
    fn builtin_keymap_validates_against_the_registry() {
        let keymap = Keymap::builtin();
        assert!(keymap.validate(&ActionRegistry::builtin()).is_ok());
        assert!(keymap.is_exclusive("resize"));
    }

    #[test]
    fn prefix_sequence_invokes_the_bound_action() {
        let mut dispatcher = dispatcher();
        assert!(matches!(
            dispatcher.dispatch(KeySpec::parse("C-b").unwrap()),
            DispatchOutcome::NeedMore { .. }
        ));
        let outcome = dispatcher.dispatch(KeySpec::parse("%").unwrap());
        assert_eq!(
            outcome,
            DispatchOutcome::Action(ActionInvocation {
                action: "pane.split".into(),
                args: Some(json!({"axis": "columns", "ratio_bps": 5000})),
            })
        );
        assert!(dispatcher.pending().is_empty());
    }

    #[test]
    fn double_prefix_is_pushed_through_to_the_pane() {
        let mut dispatcher = dispatcher();
        dispatcher.dispatch(KeySpec::parse("C-b").unwrap());
        assert_eq!(
            dispatcher.dispatch(KeySpec::parse("C-b").unwrap()),
            DispatchOutcome::Passthrough(vec![
                KeySpec::parse("C-b").unwrap(),
                KeySpec::parse("C-b").unwrap()
            ])
        );
    }

    #[test]
    fn resize_mode_overrides_arrows_and_escapes() {
        let mut dispatcher = dispatcher();
        dispatcher.dispatch(KeySpec::parse("C-b").unwrap());
        assert_eq!(
            dispatcher.dispatch(KeySpec::parse("r").unwrap()),
            DispatchOutcome::ModeEntered("resize".into())
        );
        assert_eq!(dispatcher.active_mode(), Some("resize"));
        let outcome = dispatcher.dispatch(KeySpec::parse("Left").unwrap());
        assert_eq!(
            outcome,
            DispatchOutcome::Action(ActionInvocation {
                action: "pane.resize-left".into(),
                args: None,
            })
        );
        assert_eq!(
            dispatcher.dispatch(KeySpec::parse("Esc").unwrap()),
            DispatchOutcome::ModeExited
        );
        assert_eq!(dispatcher.active_mode(), None);
    }

    #[test]
    fn unknown_sequence_passes_through_without_sticking() {
        let mut dispatcher = dispatcher();
        assert_eq!(
            dispatcher.dispatch(KeySpec::parse("q").unwrap()),
            DispatchOutcome::Passthrough(vec![KeySpec::parse("q").unwrap()])
        );
        dispatcher.dispatch(KeySpec::parse("C-b").unwrap());
        assert_eq!(
            dispatcher.dispatch(KeySpec::parse("q").unwrap()),
            DispatchOutcome::Passthrough(vec![
                KeySpec::parse("C-b").unwrap(),
                KeySpec::parse("q").unwrap()
            ])
        );
        assert!(dispatcher.pending().is_empty());
    }

    #[test]
    fn user_bindings_replace_builtins_and_report_source() {
        let registry = ActionRegistry::builtin();
        let keymap = Keymap::builtin()
            .with_user_json(
                r#"{"bindings":[{"keys":["C-b","%"],"action":"pane.focus-left"}]}"#,
                &registry,
            )
            .expect("user keymap");
        let mut dispatcher = KeyDispatcher::new(keymap);
        dispatcher.dispatch(KeySpec::parse("C-b").unwrap());
        assert_eq!(
            dispatcher.dispatch(KeySpec::parse("%").unwrap()),
            DispatchOutcome::Action(ActionInvocation {
                action: "pane.focus-left".into(),
                args: None,
            })
        );
        assert!(dispatcher.hints().iter().all(|hint| !hint.is_empty()));
    }

    #[test]
    fn invalid_user_bindings_are_rejected() {
        let registry = ActionRegistry::builtin();
        assert!(
            Keymap::builtin()
                .with_user_json(
                    r#"{"bindings":[{"keys":["C-b"],"action":"pane.nope"}]}"#,
                    &registry,
                )
                .is_err()
        );
        assert!(Keymap::builtin()
            .with_user_json(
                r#"{"bindings":[{"keys":["C-b","%"],"action":"pane.split","args":{"axis":"diagonal"}}]}"#,
                &registry,
            )
            .is_err());
    }
}
