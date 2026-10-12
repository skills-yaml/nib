//! T080 phase 3: permission rules, their layers and project trust.
//!
//! Rules are `tool` or `tool(specifier)` (see `permission_matching`). The
//! user layer is the `[permissions]` table of `~/.nib/config.toml`; the
//! project-local layer and each project's trust flag live in
//! `~/.nib/permissions.json`, keyed by the canonical project path. Both are
//! outside the project, so a cloned repository or a file edit cannot write
//! them, and the Linux sandbox hides them from tool commands.

use crate::tools::models::{PolicyEffect, PolicyRule, ToolCall};
use crate::tools::permission_matching::{
    allow_command_matches, call_paths, directory_reaches, glob_matches, parse_rule,
    restrictive_command_matches, tool_matches, Specifier,
};
pub(crate) use crate::tools::permission_matching::{
    canonical_rule, command_segments, suggested_terminal_rule,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const PERMISSIONS_FILE: &str = "permissions.json";
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_RULES_PER_LIST: usize = 256;
const MAX_PROJECTS: usize = 1000;
const SAVE_ATTEMPTS: usize = 3;

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
pub(crate) struct PermissionRule {
    pub(crate) effect: RuleEffect,
    pub(crate) layer: RuleLayer,
    pub(crate) text: String,
    tool: String,
    specifier: Option<Specifier>,
}

/// A call's paths, computed once per evaluation: for a patch this runs Git.
struct CallPaths<'a> {
    call: &'a ToolCall,
    roots: &'a [&'a Path],
    paths: std::cell::OnceCell<Option<Vec<String>>>,
}

impl CallPaths<'_> {
    fn get(&self) -> Option<&Vec<String>> {
        self.paths
            .get_or_init(|| call_paths(self.call, self.roots))
            .as_ref()
    }
}

impl PermissionRule {
    /// Whether a deny or ask rule applies to a call: any part of a compound
    /// command, any path, or a directory search that reaches the rule's path.
    fn restricts(&self, call: &ToolCall, paths: &CallPaths<'_>) -> bool {
        if !tool_matches(&self.tool, &call.tool_name) {
            return false;
        }
        match &self.specifier {
            None => true,
            Some(Specifier::Command { words, prefix }) => {
                let command = call
                    .arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                restrictive_command_matches(command, words, *prefix)
            }
            Some(Specifier::Path(glob)) => {
                let Some(paths) = paths.get() else {
                    return true;
                };
                let searches = call.tool_name == "grep";
                paths.iter().any(|path| {
                    glob_matches(glob, path) || (searches && directory_reaches(glob, path))
                })
            }
        }
    }

    /// Whether an allow rule covers a non-terminal call: every path must match.
    fn allows(&self, call: &ToolCall, paths: &CallPaths<'_>) -> bool {
        if !tool_matches(&self.tool, &call.tool_name) {
            return false;
        }
        match &self.specifier {
            None => true,
            Some(Specifier::Command { .. }) => false,
            Some(Specifier::Path(glob)) => paths.get().is_some_and(|paths| {
                !paths.is_empty() && paths.iter().all(|path| glob_matches(glob, path))
            }),
        }
    }

    fn command_part_allowed(&self, segment: &[String]) -> bool {
        match &self.specifier {
            None => true,
            Some(Specifier::Command { words, prefix }) => {
                allow_command_matches(segment, words, *prefix)
            }
            Some(Specifier::Path(_)) => false,
        }
    }
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

/// `$NIB_HOME`, else `~/.nib`. Unit tests use only `TestNibHome`, never
/// the developer's home or the process environment.
pub(crate) fn nib_home() -> Option<PathBuf> {
    #[cfg(test)]
    {
        TEST_HOME.with(|home| home.borrow().clone())
    }
    #[cfg(not(test))]
    {
        if let Some(path) = std::env::var_os("NIB_HOME").filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(path));
        }
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .filter(|value| !value.is_empty())
            .map(|home| PathBuf::from(home).join(".nib"))
    }
}

/// Settings are keyed by the canonical project folder. A linked Git worktree
/// (a session or subagent worktree) uses its main project's settings.
fn project_key(project_root: &Path) -> Option<String> {
    let root = project_root.canonicalize().ok()?;
    let root = main_worktree(&root).unwrap_or(root);
    Some(root.display().to_string())
}

/// The main checkout when `root` is a linked Git worktree.
pub(crate) fn linked_worktree_main(root: &Path) -> Option<PathBuf> {
    main_worktree(&root.canonicalize().ok()?)
}

/// The main checkout of a linked worktree, accepted only when the repository
/// links back to this worktree, so a crafted `.git` file cannot claim
/// another project's trust.
fn main_worktree(root: &Path) -> Option<PathBuf> {
    let pointer = root.join(".git");
    if !std::fs::symlink_metadata(&pointer)
        .ok()?
        .file_type()
        .is_file()
    {
        return None;
    }
    let text = read_bounded(&pointer)?;
    let gitdir = root
        .join(text.trim().strip_prefix("gitdir:")?.trim())
        .canonicalize()
        .ok()?;
    let back_link = read_bounded(&gitdir.join("gitdir"))?;
    if gitdir.join(back_link.trim()).canonicalize().ok()? != pointer.canonicalize().ok()? {
        return None;
    }
    let common = read_bounded(&gitdir.join("commondir"))?;
    let common = gitdir.join(common.trim()).canonicalize().ok()?;
    if common.file_name()? != ".git" || gitdir.parent()? != common.join("worktrees") {
        return None;
    }
    common.parent().map(Path::to_path_buf)
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
    /// A rules file exists but cannot be read. Its deny rules may be
    /// missing, so every action asks until it is repaired.
    pub(crate) unreadable: bool,
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
            Err(error) => {
                set.unreadable = true;
                set.problems.push(error);
            }
        }
        set
    }

    fn load_user_layer(&mut self, home: &Path) {
        let path = home.join("config.toml");
        if std::fs::symlink_metadata(&path).is_err() {
            return;
        }
        let Some(text) = read_bounded(&path) else {
            self.unreadable = true;
            self.problems.push(format!(
                "{} is not a regular file under 1 MiB",
                path.display()
            ));
            return;
        };
        let value: toml::Value = match toml::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                self.unreadable = true;
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
                self.unreadable = true;
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
                Err(error) => {
                    // A restrictive rule that cannot be read must not be
                    // silently dropped: every action asks until it is fixed.
                    if effect != RuleEffect::Allow {
                        self.unreadable = true;
                    }
                    self.problems
                        .push(format!("{} rule ignored: {error}", layer.label()));
                }
            }
        }
    }

    /// The strongest rule decision for a call: deny, then ask, then allow.
    /// `roots` are the directory the call runs in, then the project root;
    /// paths are made relative to whichever contains them. A compound command
    /// is allowed only when every part is allowed by a rule or is read-only
    /// on its own.
    pub(crate) fn evaluate(&self, call: &ToolCall, roots: &[&Path]) -> Option<PolicyRule> {
        let paths = CallPaths {
            call,
            roots,
            paths: std::cell::OnceCell::new(),
        };
        for effect in [RuleEffect::Deny, RuleEffect::Ask] {
            if let Some(rule) = self
                .rules
                .iter()
                .find(|rule| rule.effect == effect && rule.restricts(call, &paths))
            {
                return Some(policy_rule(rule, call));
            }
        }
        if self.unreadable {
            return Some(PolicyRule {
                effect: PolicyEffect::RequireApproval,
                tool_name: call.tool_name.clone(),
                argument_contains: None,
                reason: "a permission rules file cannot be read; see /permissions".to_string(),
            });
        }
        let mut allows = self.rules.iter().filter(|rule| {
            rule.effect == RuleEffect::Allow && tool_matches(&rule.tool, &call.tool_name)
        });
        if call.tool_name != "run_terminal" {
            return allows
                .find(|rule| rule.allows(call, &paths))
                .map(|rule| policy_rule(rule, call));
        }
        let allows: Vec<&PermissionRule> = allows.collect();
        if let Some(rule) = allows.iter().find(|rule| rule.specifier.is_none()) {
            return Some(policy_rule(rule, call));
        }
        let command = call.arguments.get("command").and_then(Value::as_str)?;
        let segments = command_segments(command)?;
        let mut used = None;
        for segment in &segments {
            if let Some(rule) = allows
                .iter()
                .find(|rule| rule.command_part_allowed(segment))
            {
                used.get_or_insert(*rule);
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
        // Forget folders that no longer exist, and keep the file bounded.
        // Entries with deny or ask rules are kept even when their folder is
        // missing (an unmounted drive), so restrictions are never dropped.
        file.projects.retain(|path, entry| {
            path == &key
                || Path::new(path).is_dir()
                || !entry.deny.is_empty()
                || !entry.ask.is_empty()
        });
        if file.projects.len() > MAX_PROJECTS {
            return Err(format!(
                "nib keeps settings for at most {MAX_PROJECTS} projects"
            ));
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
    let rule = canonical_rule(rule).unwrap_or_else(|_| rule.trim().to_string());
    update_project_entry(project_root, |entry| {
        let mut removed = false;
        for effect in [RuleEffect::Allow, RuleEffect::Ask, RuleEffect::Deny] {
            let list = entry.list_mut(effect);
            let before = list.len();
            list.retain(|existing| {
                existing != &rule && canonical_rule(existing).ok().as_ref() != Some(&rule)
            });
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

#[cfg(test)]
thread_local! {
    static TEST_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Gives the current thread a temporary nib home while it lives. Unit tests
/// otherwise see no user settings at all.
#[cfg(test)]
pub(crate) struct TestNibHome {
    pub(crate) directory: tempfile::TempDir,
    previous: Option<PathBuf>,
}

#[cfg(test)]
impl TestNibHome {
    pub(crate) fn isolated() -> Self {
        let directory = tempfile::tempdir().expect("nib home");
        let previous = TEST_HOME.with(|home| home.replace(Some(directory.path().to_path_buf())));
        Self {
            directory,
            previous,
        }
    }
}

#[cfg(test)]
impl Drop for TestNibHome {
    fn drop(&mut self) {
        let previous = self.previous.take();
        TEST_HOME.with(|home| *home.borrow_mut() = previous);
    }
}

#[cfg(test)]
#[path = "permission_rules_tests.rs"]
mod tests;
