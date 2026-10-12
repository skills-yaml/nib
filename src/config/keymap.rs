//! Context-aware TUI actions and persisted shortcut overrides.
use crossterm::event::{KeyCode, KeyModifiers};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum KeymapPreset {
    #[default]
    Codex,
    Legacy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TuiConfig {
    #[serde(default)]
    pub preset: KeymapPreset,
    #[serde(default)]
    pub keymap: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Send,
    Steer,
    Queue,
    Copy,
    CopySelection,
    Cancel,
    Quit,
    History,
    Editor,
    ClearView,
    Focus,
    Mode,
    SelectChat,
    Start,
    End,
    Newline,
    PageUp,
    PageDown,
    ScrollUp,
    ScrollDown,
    First,
    FollowTail,
}

impl KeyAction {
    pub const ALL: [(Self, &'static str); 22] = [
        (Self::Send, "send"),
        (Self::Steer, "steer"),
        (Self::Queue, "queue"),
        (Self::Copy, "copy"),
        (Self::CopySelection, "copy-selection"),
        (Self::Cancel, "cancel"),
        (Self::Quit, "quit"),
        (Self::History, "history"),
        (Self::Editor, "editor"),
        (Self::ClearView, "clear-view"),
        (Self::Focus, "focus"),
        (Self::Mode, "mode"),
        (Self::SelectChat, "select-chat"),
        (Self::Start, "start"),
        (Self::End, "end"),
        (Self::Newline, "newline"),
        (Self::PageUp, "page-up"),
        (Self::PageDown, "page-down"),
        (Self::ScrollUp, "scroll-up"),
        (Self::ScrollDown, "scroll-down"),
        (Self::First, "first"),
        (Self::FollowTail, "follow-tail"),
    ];
    pub fn name(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(a, _)| *a == self)
            .expect("known action")
            .1
    }
    fn parse(name: &str) -> Result<Self, String> {
        Self::ALL
            .iter()
            .find(|(_, n)| *n == name && *n != "submit")
            .map(|(a, _)| *a)
            .ok_or_else(|| format!("unknown keymap action: {name}"))
    }
}

pub fn parse_binding(binding: &str) -> Result<(KeyCode, KeyModifiers), String> {
    let mut modifiers = KeyModifiers::NONE;
    let mut parts = binding.split('-').peekable();
    while let Some(modifier) = parts.peek().copied() {
        let flag = match modifier {
            "ctrl" => KeyModifiers::CONTROL,
            "shift" => KeyModifiers::SHIFT,
            "alt" => KeyModifiers::ALT,
            _ => break,
        };
        if modifiers.contains(flag) {
            return Err(format!("duplicate modifier: {modifier}"));
        }
        modifiers |= flag;
        parts.next();
    }
    let key = parts.collect::<Vec<_>>().join("-");
    let code = match key.as_str() {
        "enter" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "esc" => KeyCode::Esc,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "page-up" => KeyCode::PageUp,
        "page-down" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        text if text.chars().count() == 1 && text.chars().all(|c| c.is_ascii_alphanumeric()) => {
            KeyCode::Char(
                text.chars()
                    .next()
                    .expect("single character")
                    .to_ascii_lowercase(),
            )
        }
        _ => return Err(format!("invalid key binding: {binding}")),
    };
    if matches!(code, KeyCode::Char(_)) && modifiers.is_empty() {
        return Err("printable keys cannot be bound without a modifier".to_string());
    }
    Ok((code, modifiers))
}

impl TuiConfig {
    pub fn action_available(context: &str, action: KeyAction) -> bool {
        match action {
            KeyAction::Send => context == "composer" || context == "running",
            KeyAction::Steer | KeyAction::Queue => context == "running",
            KeyAction::Editor | KeyAction::ClearView => context == "composer",
            KeyAction::Start | KeyAction::End | KeyAction::Newline => {
                context != "transcript" && context != "global"
            }
            _ => true,
        }
    }

    pub fn bindings(&self, context: &str, action: KeyAction) -> Vec<String> {
        if !Self::action_available(context, action) {
            return vec![];
        }
        for scope in [context, "global"] {
            if let Some(values) = self.keymap.get(scope).and_then(|m| m.get(action.name())) {
                return values.clone();
            }
        }
        let legacy = self.preset == KeymapPreset::Legacy;
        let keys: &[&str] = match action {
            KeyAction::PageUp => &["page-up"],
            KeyAction::PageDown => &["page-down"],
            KeyAction::ScrollUp => &["ctrl-up", "shift-up"],
            KeyAction::ScrollDown => &["ctrl-down", "shift-down"],
            KeyAction::First => &["ctrl-home"],
            KeyAction::FollowTail => &["ctrl-end"],
            KeyAction::Cancel => &["ctrl-c"],
            KeyAction::Quit => &["ctrl-q"],
            KeyAction::CopySelection => &["ctrl-y", "ctrl-shift-c"],
            KeyAction::Copy if !legacy => &["ctrl-o"],
            KeyAction::History => &["ctrl-r"],
            KeyAction::Editor if !legacy => &["ctrl-g"],
            KeyAction::ClearView if !legacy && context == "composer" => &["ctrl-l"],
            KeyAction::Send if context == "composer" || (legacy && context == "running") => {
                &["enter"]
            }
            KeyAction::Steer if context == "running" => {
                if legacy {
                    &["ctrl-s"]
                } else {
                    &["enter", "ctrl-s"]
                }
            }
            KeyAction::Queue if context == "running" && !legacy => &["tab"],
            KeyAction::Focus => {
                if legacy {
                    &["tab"]
                } else {
                    &["alt-t"]
                }
            }
            KeyAction::Mode => &["shift-tab"],
            KeyAction::SelectChat if legacy => &["ctrl-a"],
            KeyAction::SelectChat if context == "transcript" => &["ctrl-a"],
            KeyAction::Start if !legacy && context != "transcript" => &["ctrl-a"],
            KeyAction::End if !legacy && context != "transcript" => &["ctrl-e"],
            KeyAction::Newline => &["shift-enter", "alt-enter", "ctrl-j"],
            _ => &[],
        };
        keys.iter().map(|s| s.to_string()).collect()
    }

    pub fn resolve(
        &self,
        context: &str,
        mut code: KeyCode,
        mut modifiers: KeyModifiers,
    ) -> Option<KeyAction> {
        if code == KeyCode::BackTab {
            code = KeyCode::Tab;
            modifiers |= KeyModifiers::SHIFT;
        }
        if let KeyCode::Char(c) = code {
            code = KeyCode::Char(c.to_ascii_lowercase());
        }
        KeyAction::ALL
            .iter()
            .filter(|(_, name)| *name != "submit")
            .find_map(|(action, _)| {
                self.bindings(context, *action)
                    .iter()
                    .any(|b| parse_binding(b).ok() == Some((code, modifiers)))
                    .then_some(*action)
            })
    }

    pub fn validate(&self) -> Result<(), String> {
        for (context, actions) in &self.keymap {
            if !["global", "composer", "running", "transcript"].contains(&context.as_str()) {
                return Err(format!("unknown keymap context: {context}"));
            }
            for (action, bindings) in actions {
                let parsed = KeyAction::parse(action)?;
                if context == "global"
                    && !["composer", "running", "transcript"]
                        .iter()
                        .all(|c| Self::action_available(c, parsed))
                {
                    return Err(format!("action {action} requires a specific context"));
                }
                if context != "global" && !Self::action_available(context, parsed) {
                    return Err(format!("action {action} is unavailable in {context}"));
                }
                if bindings.len() > 8 {
                    return Err("at most eight bindings per action".to_string());
                }
                for binding in bindings {
                    parse_binding(binding)?;
                }
            }
        }
        for context in ["composer", "running", "transcript"] {
            let mut used = BTreeMap::new();
            for (action, name) in KeyAction::ALL.iter().filter(|(_, n)| *n != "submit") {
                for binding in self.bindings(context, *action) {
                    let parsed = format!("{:?}", parse_binding(&binding)?);
                    if let Some(previous) = used.insert(parsed, *name) {
                        return Err(format!(
                            "keymap collision in {context}: {previous} and {name} use {binding}"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn display(&self) -> String {
        let mut rows = vec![format!("Keymap: {:?}", self.preset)];
        for context in ["composer", "running", "transcript"] {
            for (action, name) in KeyAction::ALL.iter().filter(|(_, n)| *n != "submit") {
                let keys = self.bindings(context, *action);
                if !keys.is_empty() {
                    rows.push(format!("{context}.{name}: {}", keys.join(", ")));
                }
            }
        }
        rows.join("\n")
    }
}

pub fn keymap_command(root: &std::path::Path, arguments: &[String]) -> Result<String, String> {
    if arguments.is_empty() {
        return super::load_nib_config_full(root)
            .map(|c| c.tui.display())
            .map_err(|e| e.to_string());
    }
    super::update_nib_config(root, |config| {
        match arguments {
            [verb] if verb=="reset" => config.tui=TuiConfig::default(),
            [verb,preset] if verb=="preset" => config.tui.preset=match preset.as_str() {
                "codex"=>KeymapPreset::Codex,"legacy"=>KeymapPreset::Legacy,
                _=>return Err("preset must be codex or legacy".to_string()),
            },
            [verb,context,action,bindings @ ..] if (verb=="set" && !bindings.is_empty()) || (verb=="unbind" && bindings.is_empty()) => {
                config.tui.keymap.entry(context.clone()).or_default().insert(action.clone(),bindings.to_vec());
            }
            _=>return Err("use /keymap [reset|preset codex|preset legacy|set <context> <action> <keys...>|unbind <context> <action>]".to_string()),
        }
        config.tui.validate()?;
        Ok(())
    }).map_err(|e|e.to_string())?;
    Ok(super::load_nib_config_full(root)
        .map_err(|e| e.to_string())?
        .tui
        .display())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presets_resolve_run_and_composer_actions() {
        let mut map = TuiConfig::default();
        map.validate().unwrap();
        assert_eq!(
            map.resolve("running", KeyCode::Enter, KeyModifiers::NONE),
            Some(KeyAction::Steer)
        );
        assert_eq!(
            map.resolve("running", KeyCode::Tab, KeyModifiers::NONE),
            Some(KeyAction::Queue)
        );
        assert_eq!(
            map.resolve("composer", KeyCode::Char('a'), KeyModifiers::CONTROL),
            Some(KeyAction::Start)
        );
        map.preset = KeymapPreset::Legacy;
        map.validate().unwrap();
        assert_eq!(
            map.resolve("running", KeyCode::Enter, KeyModifiers::NONE),
            Some(KeyAction::Send)
        );
        assert_eq!(
            map.resolve("running", KeyCode::Tab, KeyModifiers::NONE),
            Some(KeyAction::Focus)
        );
        assert_eq!(
            map.resolve("composer", KeyCode::Char('a'), KeyModifiers::CONTROL),
            Some(KeyAction::SelectChat)
        );
    }
    #[test]
    fn overrides_are_atomic_persistent_and_collision_checked() {
        let root = tempfile::tempdir().unwrap();
        keymap_command(
            root.path(),
            &["set", "composer", "history", "alt-r"].map(str::to_string),
        )
        .unwrap();
        let map = super::super::load_nib_config_full(root.path()).unwrap().tui;
        assert_eq!(
            map.resolve("composer", KeyCode::Char('r'), KeyModifiers::CONTROL),
            None
        );
        assert_eq!(
            map.resolve("composer", KeyCode::Char('r'), KeyModifiers::ALT),
            Some(KeyAction::History)
        );
        let before = super::super::load_nib_config_full(root.path()).unwrap();
        assert!(keymap_command(
            root.path(),
            &["set", "composer", "history", "ctrl-c"].map(str::to_string)
        )
        .is_err());
        assert_eq!(
            super::super::load_nib_config_full(root.path()).unwrap(),
            before
        );
        assert!(keymap_command(
            root.path(),
            &["set", "unknown", "history", "alt-r"].map(str::to_string)
        )
        .is_err());
        keymap_command(
            root.path(),
            &["unbind", "composer", "history"].map(str::to_string),
        )
        .unwrap();
        assert!(super::super::load_nib_config_full(root.path())
            .unwrap()
            .tui
            .bindings("composer", KeyAction::History)
            .is_empty());
        keymap_command(root.path(), &["reset"].map(str::to_string)).unwrap();
        assert_eq!(
            super::super::load_nib_config_full(root.path()).unwrap().tui,
            TuiConfig::default()
        );
    }
}

/// Render the same validated binding used by dispatch in user-facing hints.
pub fn binding_label(binding: &str) -> String {
    binding
        .split('-')
        .map(|part| match part {
            "ctrl" => "Ctrl".to_string(),
            "shift" => "Shift".to_string(),
            "alt" => "Alt".to_string(),
            "enter" => "Enter".to_string(),
            "tab" => "Tab".to_string(),
            "esc" => "Esc".to_string(),
            "home" => "Home".to_string(),
            "end" => "End".to_string(),
            other => other.to_ascii_uppercase(),
        })
        .collect::<Vec<_>>()
        .join("+")
}

#[cfg(test)]
mod context_tests {
    use super::*;
    #[test]
    fn unavailable_actions_are_not_advertised_or_accepted() {
        let mut map = TuiConfig::default();
        assert!(map.bindings("transcript", KeyAction::Editor).is_empty());
        assert!(map.bindings("running", KeyAction::Editor).is_empty());
        map.keymap
            .entry("transcript".into())
            .or_default()
            .insert("editor".into(), vec!["alt-g".into()]);
        assert!(map.validate().unwrap_err().contains("unavailable"));
        map.keymap.clear();
        map.keymap
            .entry("global".into())
            .or_default()
            .insert("editor".into(), vec!["alt-g".into()]);
        assert!(map.validate().unwrap_err().contains("specific context"));
    }
}
