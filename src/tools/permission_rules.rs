//! T080 phase 3: permission rules, their layers and project trust.
//!
//! Rules are `tool` or `tool(specifier)`. The user layer is the
//! `[permissions]` table of `~/.nib/config.toml`; the project-local layer and
//! each project's trust flag live in `~/.nib/permissions.json`, keyed by the
//! canonical project path. Both are outside the project and outside the tool
//! sandbox, so neither a cloned repository nor a tool command can write them.

use crate::tools::models::{PolicyEffect, PolicyRule, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

const PERMISSIONS_FILE: &str = "permissions.json";
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_RULES_PER_LIST: usize = 256;
const MAX_RULE_BYTES: usize = 1024;
const SAVE_ATTEMPTS: usize = 3;

/// Programs that run other programs: a word prefix would grant far more than
/// the command the user saw, so only the exact command is suggested.
const WRAPPER_PROGRAMS: &[&str] = &[
    "sudo", "su", "doas", "env", "sh", "bash", "zsh", "fish", "dash", "ksh", "eval", "exec",
    "command", "builtin", "xargs", "find", "timeout", "nice", "nohup", "time", "watch", "ssh",
    "python", "python3", "node", "perl", "ruby", "deno", "bun", "php", "lua", "awk", "sed",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuleEffect {
    Allow,
    Ask,
    Deny,
}

impl RuleEffect {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "allow" => Some(Self::Allow),
            "ask" => Some(Self::Ask),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuleLayer {
    User,
    ProjectLocal,
}

impl RuleLayer {
    fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ProjectLocal => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Specifier {
    Command { words: Vec<String>, prefix: bool },
    Path(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionRule {
    pub(crate) effect: RuleEffect,
    pub(crate) layer: RuleLayer,
    pub(crate) text: String,
    tool: String,
    specifier: Option<Specifier>,
}

/// Parses `tool` or `tool(specifier)` into a canonical tool name and matcher.
fn parse_rule(text: &str) -> Result<(String, Option<Specifier>), String> {
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_RULE_BYTES {
        return Err("a rule must be 1 to 1024 bytes".to_string());
    }
    if text.chars().any(char::is_control) {
        return Err("a rule cannot contain control characters".to_string());
    }
    let (name, specifier) = match text.split_once('(') {
        Some((name, rest)) => {
            let inner = rest
                .strip_suffix(')')
                .ok_or_else(|| format!("rule `{text}` is missing its closing parenthesis"))?;
            (name.trim(), Some(inner.trim()))
        }
        None => (text, None),
    };
    let tool = match name.to_ascii_lowercase().as_str() {
        "terminal" | "bash" | "run_terminal" => "run_terminal".to_string(),
        "edit" | "apply_patch" => "apply_patch".to_string(),
        "read" | "read_file" => "read_file".to_string(),
        other
            if !other.is_empty()
                && other
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-')) =>
        {
            other.to_string()
        }
        _ => return Err(format!("rule `{text}` does not name a tool")),
    };
    let Some(specifier) = specifier.filter(|value| !value.is_empty() && *value != "*") else {
        return Ok((tool, None));
    };
    let specifier = if tool == "run_terminal" {
        let (body, prefix) = if let Some(body) = specifier.strip_suffix(":*") {
            (body, true)
        } else if let Some(body) = specifier.strip_suffix(" *") {
            (body, true)
        } else {
            (specifier, false)
        };
        let words = command_words(body).ok_or_else(|| {
            format!("rule `{text}` must name a plain command without shell operators or quotes")
        })?;
        if words.is_empty() {
            return Err(format!("rule `{text}` names no command"));
        }
        Specifier::Command { words, prefix }
    } else if path_tool(&tool) {
        Specifier::Path(normalize_rule_path(specifier))
    } else {
        return Err(format!(
            "rule `{text}`: only terminal and path tools take a specifier"
        ));
    };
    Ok((tool, Some(specifier)))
}

/// Validates a rule as typed by the user and returns its canonical spelling.
pub(crate) fn canonical_rule(text: &str) -> Result<String, String> {
    parse_rule(text)?;
    Ok(text.trim().to_string())
}

fn path_tool(tool: &str) -> bool {
    matches!(
        tool,
        "read_file" | "list_directory" | "grep" | "apply_patch"
    )
}

fn normalize_rule_path(path: &str) -> String {
    let mut path = path.trim();
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    path.trim_end_matches('/').to_string()
}

/// Splits a command segment into words the way `sh` would for plain words.
/// `None` when the text contains anything a word split cannot model.
fn command_words(segment: &str) -> Option<Vec<String>> {
    if segment.chars().any(|c| {
        (c.is_control() && c != '\t')
            || matches!(
                c,
                '`' | '$' | '\'' | '"' | '\\' | '(' | ')' | '{' | '}' | '<' | '>' | '&' | '|' | ';'
            )
    }) {
        return None;
    }
    Some(
        segment
            .split([' ', '\t'])
            .filter(|word| !word.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// Splits a compound command on `&&`, `||`, `;`, `|` and newlines. `None`
/// when quoting, substitution, redirection or a lone `&` make that unsafe.
pub(crate) fn command_segments(command: &str) -> Option<Vec<Vec<String>>> {
    let joined = command.replace("&&", "\n").replace("||", "\n");
    let mut segments = Vec::new();
    for segment in joined.split(['\n', ';', '|']) {
        let words = command_words(segment)?;
        if !words.is_empty() {
            segments.push(words);
        }
    }
    (!segments.is_empty()).then_some(segments)
}

fn program_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

impl PermissionRule {
    fn command_matches(words: &[String], rule: &[String], prefix: bool, lenient: bool) -> bool {
        if words.len() < rule.len() || (!prefix && words.len() != rule.len()) {
            return false;
        }
        words
            .iter()
            .zip(rule)
            .enumerate()
            .all(|(index, (word, expected))| {
                word == expected || (lenient && index == 0 && program_name(word) == expected)
            })
    }

    /// Whether the rule applies to one call. Allow rules need every part of a
    /// compound command; deny and ask rules need any part, and fall back to a
    /// text search when the command cannot be split.
    fn matches(&self, call: &ToolCall, root: &Path) -> bool {
        if self.tool != call.tool_name {
            return false;
        }
        let Some(specifier) = &self.specifier else {
            return true;
        };
        match specifier {
            Specifier::Command { words, prefix } => {
                let command = call
                    .arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let restrictive = self.effect != RuleEffect::Allow;
                match command_segments(command) {
                    Some(segments) => segments
                        .iter()
                        .any(|segment| Self::command_matches(segment, words, *prefix, restrictive)),
                    None => restrictive && command.contains(&words.join(" ")),
                }
            }
            Specifier::Path(glob) => {
                let paths = call_paths(call, root);
                if paths.is_empty() {
                    return self.effect != RuleEffect::Allow;
                }
                if self.effect == RuleEffect::Allow {
                    paths.iter().all(|path| glob_matches(glob, path))
                } else {
                    paths.iter().any(|path| glob_matches(glob, path))
                }
            }
        }
    }

    fn command_part_allowed(&self, segment: &[String]) -> bool {
        match &self.specifier {
            None => true,
            Some(Specifier::Command { words, prefix }) => {
                Self::command_matches(segment, words, *prefix, false)
            }
            Some(Specifier::Path(_)) => false,
        }
    }
}

/// Workspace-relative paths a call reads or edits. Absolute paths under the
/// root are made relative; paths are normalized lexically.
fn call_paths(call: &ToolCall, root: &Path) -> Vec<String> {
    let raw: Vec<String> = if call.tool_name == "apply_patch" {
        patch_paths(
            call.arguments
                .get("patch")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
    } else {
        vec![call
            .arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(".")
            .to_string()]
    };
    raw.iter()
        .map(|path| relative_lexical(Path::new(path), root))
        .collect()
}

fn relative_lexical(path: &Path, root: &Path) -> String {
    let path = path.strip_prefix(root).unwrap_or(path);
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::ParentDir => {
                if parts.pop().is_none() {
                    parts.push("..".to_string());
                }
            }
            Component::RootDir => parts.push(String::new()),
            Component::CurDir | Component::Prefix(_) => {}
        }
    }
    parts.join("/")
}

/// File paths named by a unified or Git patch's headers.
pub(crate) fn patch_paths(patch: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in patch.lines() {
        let candidate = if let Some(rest) = line.strip_prefix("diff --git ") {
            for part in rest.split(' ') {
                if let Some(path) = part.strip_prefix("a/").or_else(|| part.strip_prefix("b/")) {
                    paths.push(path.to_string());
                }
            }
            continue;
        } else if let Some(rest) = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "))
        {
            rest.split('\t').next().unwrap_or(rest)
        } else if let Some(rest) = line
            .strip_prefix("rename from ")
            .or_else(|| line.strip_prefix("rename to "))
            .or_else(|| line.strip_prefix("copy from "))
            .or_else(|| line.strip_prefix("copy to "))
        {
            rest
        } else {
            continue;
        };
        if candidate == "/dev/null" {
            continue;
        }
        let path = candidate
            .strip_prefix("a/")
            .or_else(|| candidate.strip_prefix("b/"))
            .unwrap_or(candidate);
        paths.push(path.to_string());
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Gitignore-style matching: a glob without `/` matches a file name at any
/// depth; otherwise it matches from the workspace root. `**` spans
/// directories and a match on a directory covers everything below it.
pub(crate) fn glob_matches(glob: &str, path: &str) -> bool {
    let path_parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if glob.is_empty() {
        return false;
    }
    if !glob.contains('/') {
        return path_parts.iter().any(|part| segment_matches(glob, part));
    }
    let glob_parts: Vec<&str> = glob.split('/').filter(|part| !part.is_empty()).collect();
    (0..=path_parts.len()).any(|end| parts_match(&glob_parts, &path_parts[..end]))
}

fn parts_match(glob: &[&str], path: &[&str]) -> bool {
    match glob.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| parts_match(rest, &path[skip..])),
        Some((first, rest)) => path
            .split_first()
            .is_some_and(|(part, tail)| segment_matches(first, part) && parts_match(rest, tail)),
    }
}

fn segment_matches(glob: &str, text: &str) -> bool {
    fn inner(glob: &[char], text: &[char]) -> bool {
        match glob.split_first() {
            None => text.is_empty(),
            Some(('*', rest)) => (0..=text.len()).any(|skip| inner(rest, &text[skip..])),
            Some(('?', rest)) => !text.is_empty() && inner(rest, &text[1..]),
            Some((c, rest)) => text.first() == Some(c) && inner(rest, &text[1..]),
        }
    }
    let glob: Vec<char> = glob.chars().collect();
    let text: Vec<char> = text.chars().collect();
    inner(&glob, &text)
}

/// The rule offered by "don't ask again": the program and its subcommand word
/// as a prefix, or the exact command when a prefix would grant too much.
pub(crate) fn suggested_terminal_rule(command: &str) -> Option<String> {
    let segments = command_segments(command)?;
    let [words] = segments.as_slice() else {
        return None;
    };
    if words.len() < 2 || command.contains('\n') {
        return None;
    }
    let exact = words.join(" ");
    let program = words[0].as_str();
    let subcommand_word = |word: &str| {
        word.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
            && word
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))
    };
    if program.contains('/')
        || program.contains('=')
        || WRAPPER_PROGRAMS.contains(&program)
        || program.starts_with("python")
        || !subcommand_word(&words[1])
    {
        return Some(exact);
    }
    let mut prefix = vec![words[0].clone(), words[1].clone()];
    if matches!(words[1].as_str(), "run" | "exec" | "x") {
        match words.get(2) {
            Some(script) if subcommand_word(script) => prefix.push(script.clone()),
            _ => return Some(exact),
        }
    }
    Some(format!("{} *", prefix.join(" ")))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct ProjectEntry {
    #[serde(default)]
    trusted: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ask: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    deny: Vec<String>,
}

impl ProjectEntry {
    fn list_mut(&mut self, effect: RuleEffect) -> &mut Vec<String> {
        match effect {
            RuleEffect::Allow => &mut self.allow,
            RuleEffect::Ask => &mut self.ask,
            RuleEffect::Deny => &mut self.deny,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PermissionsFile {
    version: u32,
    #[serde(default)]
    projects: BTreeMap<String, ProjectEntry>,
}

/// `$NIB_HOME`, else `~/.nib`. Unit tests never read the developer's home.
pub(crate) fn nib_home() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NIB_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    if cfg!(test) {
        return None;
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".nib"))
}

fn project_key(project_root: &Path) -> Option<String> {
    project_root
        .canonicalize()
        .ok()
        .map(|path| path.display().to_string())
}

fn read_bounded(path: &Path) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_FILE_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn load_permissions_file(home: &Path) -> Result<PermissionsFile, String> {
    let path = home.join(PERMISSIONS_FILE);
    if std::fs::symlink_metadata(&path).is_err() {
        return Ok(PermissionsFile {
            version: 1,
            ..PermissionsFile::default()
        });
    }
    let text = read_bounded(&path)
        .ok_or_else(|| format!("{} is not a regular file under 1 MiB", path.display()))?;
    let file: PermissionsFile = serde_json::from_str(&text)
        .map_err(|error| format!("{} is invalid: {error}", path.display()))?;
    if file.version != 1 {
        return Err(format!(
            "{} has unsupported version {}",
            path.display(),
            file.version
        ));
    }
    Ok(file)
}

/// Every rule that applies to `project_root`, with the project's trust flag
/// and any problems found while reading the layers.
#[derive(Debug, Clone, Default)]
pub(crate) struct RuleSet {
    pub(crate) rules: Vec<PermissionRule>,
    pub(crate) trusted: bool,
    pub(crate) problems: Vec<String>,
}

impl RuleSet {
    pub(crate) fn load(project_root: &Path) -> Self {
        let mut set = Self::default();
        let Some(home) = nib_home() else {
            return set;
        };
        set.load_user_layer(&home);
        match load_permissions_file(&home) {
            Ok(file) => {
                if let Some(entry) =
                    project_key(project_root).and_then(|key| file.projects.get(&key).cloned())
                {
                    set.trusted = entry.trusted;
                    for (effect, list) in [
                        (RuleEffect::Allow, &entry.allow),
                        (RuleEffect::Ask, &entry.ask),
                        (RuleEffect::Deny, &entry.deny),
                    ] {
                        set.push_rules(effect, RuleLayer::ProjectLocal, list);
                    }
                }
            }
            Err(error) => set.problems.push(error),
        }
        set
    }

    fn load_user_layer(&mut self, home: &Path) {
        let path = home.join("config.toml");
        if std::fs::symlink_metadata(&path).is_err() {
            return;
        }
        let Some(text) = read_bounded(&path) else {
            self.problems.push(format!(
                "{} is not a regular file under 1 MiB",
                path.display()
            ));
            return;
        };
        let value: toml::Value = match toml::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                self.problems
                    .push(format!("{} is invalid: {error}", path.display()));
                return;
            }
        };
        let Some(table) = value.get("permissions") else {
            return;
        };
        for effect in [RuleEffect::Allow, RuleEffect::Ask, RuleEffect::Deny] {
            let Some(list) = table.get(effect.label()) else {
                continue;
            };
            let Some(items) = list.as_array() else {
                self.problems.push(format!(
                    "{}: permissions.{} must be a list of strings",
                    path.display(),
                    effect.label()
                ));
                continue;
            };
            let texts: Vec<String> = items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect();
            self.push_rules(effect, RuleLayer::User, &texts);
        }
    }

    fn push_rules(&mut self, effect: RuleEffect, layer: RuleLayer, texts: &[String]) {
        for text in texts.iter().take(MAX_RULES_PER_LIST) {
            match parse_rule(text) {
                Ok((tool, specifier)) => self.rules.push(PermissionRule {
                    effect,
                    layer,
                    text: text.trim().to_string(),
                    tool,
                    specifier,
                }),
                Err(error) => self
                    .problems
                    .push(format!("{} rule ignored: {error}", layer.label())),
            }
        }
    }

    /// The strongest rule decision for a call: deny, then ask, then allow.
    /// A compound command is allowed only when every part is allowed by a
    /// rule or is read-only on its own.
    pub(crate) fn evaluate(&self, call: &ToolCall, root: &Path) -> Option<PolicyRule> {
        for effect in [RuleEffect::Deny, RuleEffect::Ask] {
            if let Some(rule) = self
                .rules
                .iter()
                .find(|rule| rule.effect == effect && rule.matches(call, root))
            {
                return Some(policy_rule(rule, call));
            }
        }
        let allows = self
            .rules
            .iter()
            .filter(|rule| rule.effect == RuleEffect::Allow && rule.tool == call.tool_name);
        if call.tool_name != "run_terminal" {
            return allows
                .clone()
                .find(|rule| rule.matches(call, root))
                .map(|rule| policy_rule(rule, call));
        }
        if let Some(rule) = allows.clone().find(|rule| rule.specifier.is_none()) {
            return Some(policy_rule(rule, call));
        }
        let command = call.arguments.get("command").and_then(Value::as_str)?;
        let segments = command_segments(command)?;
        let mut used = None;
        for segment in &segments {
            if let Some(rule) = allows
                .clone()
                .find(|rule| rule.command_part_allowed(segment))
            {
                used.get_or_insert(rule);
            } else if crate::tools::classifier::classify_command(&segment.join(" "))
                != crate::tools::classifier::ToolRisk::ReadOnly
            {
                return None;
            }
        }
        used.map(|rule| policy_rule(rule, call))
    }
}

fn policy_rule(rule: &PermissionRule, call: &ToolCall) -> PolicyRule {
    PolicyRule {
        effect: match rule.effect {
            RuleEffect::Allow => PolicyEffect::Allow,
            RuleEffect::Ask => PolicyEffect::RequireApproval,
            RuleEffect::Deny => PolicyEffect::Deny,
        },
        tool_name: call.tool_name.clone(),
        argument_contains: None,
        reason: format!(
            "{} {} rule {}",
            rule.layer.label(),
            rule.effect.label(),
            rule.text
        ),
    }
}

/// Whether the user has trusted rules shipped inside this project.
pub(crate) fn project_trusted(project_root: &Path) -> bool {
    let Some(home) = nib_home() else {
        return false;
    };
    let Some(key) = project_key(project_root) else {
        return false;
    };
    load_permissions_file(&home)
        .ok()
        .and_then(|file| file.projects.get(&key).map(|entry| entry.trusted))
        .unwrap_or(false)
}

/// Applies `change` to this project's entry and saves the file atomically,
/// retrying when another session saved first.
fn update_project_entry(
    project_root: &Path,
    change: impl Fn(&mut ProjectEntry) -> Result<bool, String>,
) -> Result<bool, String> {
    let home = nib_home().ok_or("no home directory for nib's user settings")?;
    let key = project_key(project_root)
        .ok_or("the project folder cannot be resolved for its settings")?;
    let home = crate::fs_security::ensure_directory_without_symlinks(&home)
        .map_err(|error| format!("{} is unusable: {error}", home.display()))?;
    let path = home.join(PERMISSIONS_FILE);
    let directory = crate::daemons::state::StableDirectory::open(&home)?;
    let mut last_error = String::new();
    for _ in 0..SAVE_ATTEMPTS {
        let opened = if std::fs::symlink_metadata(&path).is_ok() {
            Some(directory.open_read(&path)?)
        } else {
            None
        };
        let mut file = load_permissions_file(&home)?;
        let entry = file.projects.entry(key.clone()).or_default();
        if !change(entry)? {
            return Ok(false);
        }
        if *entry == ProjectEntry::default() {
            file.projects.remove(&key);
        }
        file.version = 1;
        let encoded = serde_json::to_vec_pretty(&file).map_err(|error| error.to_string())?;
        if encoded.len() as u64 > MAX_FILE_BYTES {
            return Err("nib's permissions file would exceed 1 MiB".to_string());
        }
        let expectation = match opened.as_ref() {
            Some(file) => crate::daemons::state::FileExpectation::Present(file),
            None => crate::daemons::state::FileExpectation::Missing,
        };
        match directory.save_bytes_atomically_expected_with_hook(
            &path,
            &encoded,
            ".permissions.json.tmp-",
            true,
            expectation,
            || Ok(()),
        ) {
            Ok(()) => return Ok(true),
            Err(error) => last_error = error,
        }
    }
    Err(format!("the permissions file was not saved: {last_error}"))
}

/// Saves a project-local rule. Returns false when it was already present.
pub(crate) fn add_project_rule(
    project_root: &Path,
    effect: RuleEffect,
    rule: &str,
) -> Result<bool, String> {
    let rule = canonical_rule(rule)?;
    update_project_entry(project_root, |entry| {
        let list = entry.list_mut(effect);
        if list.contains(&rule) {
            return Ok(false);
        }
        if list.len() >= MAX_RULES_PER_LIST {
            return Err(format!(
                "this project already has {MAX_RULES_PER_LIST} {} rules",
                effect.label()
            ));
        }
        list.push(rule.clone());
        Ok(true)
    })
}

/// Removes a project-local rule from every list. Returns false when absent.
pub(crate) fn remove_project_rule(project_root: &Path, rule: &str) -> Result<bool, String> {
    let rule = rule.trim().to_string();
    update_project_entry(project_root, |entry| {
        let mut removed = false;
        for effect in [RuleEffect::Allow, RuleEffect::Ask, RuleEffect::Deny] {
            let list = entry.list_mut(effect);
            let before = list.len();
            list.retain(|existing| existing != &rule);
            removed |= list.len() != before;
        }
        Ok(removed)
    })
}

/// Sets whether rules shipped inside the project apply. Returns false when
/// the flag already had that value.
pub(crate) fn set_project_trust(project_root: &Path, trusted: bool) -> Result<bool, String> {
    update_project_entry(project_root, |entry| {
        if entry.trusted == trusted {
            return Ok(false);
        }
        entry.trusted = trusted;
        Ok(true)
    })
}

/// Points `NIB_HOME` at a temporary directory while it lives.
#[cfg(test)]
pub(crate) struct TestNibHome {
    pub(crate) directory: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl TestNibHome {
    pub(crate) fn isolated() -> Self {
        let directory = tempfile::tempdir().expect("nib home");
        let previous = std::env::var_os("NIB_HOME");
        std::env::set_var("NIB_HOME", directory.path());
        Self {
            directory,
            previous,
        }
    }
}

#[cfg(test)]
impl Drop for TestNibHome {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("NIB_HOME", value),
            None => std::env::remove_var("NIB_HOME"),
        }
    }
}

#[cfg(test)]
#[path = "permission_rules_tests.rs"]
mod tests;
