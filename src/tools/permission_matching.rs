//! T080 phase 3: parsing and matching of permission rules.
//!
//! Allow rules must match exactly what runs; deny and ask rules are
//! deliberately loose (wrappers, environment assignments, case and quoting
//! are ignored) so that they are hard to step around. Patch paths come from
//! Git itself, so a rule sees the same paths `git apply` writes.

use crate::tools::models::ToolCall;
use serde_json::Value;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

const MAX_RULE_BYTES: usize = 1024;

/// Programs that run other programs, or that reach arbitrary code through
/// their options. A word prefix would grant far more than the command the
/// user saw, so only the exact command is suggested for them.
const WRAPPER_PROGRAMS: &[&str] = &[
    // Privilege, environment and process wrappers.
    "sudo",
    "su",
    "doas",
    "pkexec",
    "runuser",
    "sg",
    "env",
    "command",
    "builtin",
    "exec",
    "eval",
    "xargs",
    "parallel",
    "find",
    "timeout",
    "nice",
    "ionice",
    "nohup",
    "time",
    "watch",
    "stdbuf",
    "setsid",
    "flock",
    "taskset",
    "chrt",
    "chroot",
    "unshare",
    "nsenter",
    "systemd-run",
    "script",
    "firejail",
    "busybox",
    "toybox",
    "strace",
    "ltrace",
    "gdb",
    "valgrind",
    "ssh",
    // Shells and interpreters.
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "pwsh",
    "powershell",
    "cmd",
    "osascript",
    "python",
    "python2",
    "python3",
    "pypy",
    "pypy3",
    "ipython",
    "node",
    "nodejs",
    "deno",
    "bun",
    "perl",
    "ruby",
    "irb",
    "php",
    "lua",
    "tclsh",
    "rscript",
    "java",
    "awk",
    "gawk",
    "sed",
    // Package runners and tools whose subcommands run arbitrary code.
    "npx",
    "pnpx",
    "bunx",
    "uvx",
    "git",
    "docker",
    "podman",
    "kubectl",
    "nerdctl",
];

/// Subcommands that run a named script or package; the name is kept in the
/// suggested prefix (`npm run build *`).
const RUNNER_SUBCOMMANDS: &[&str] = &["run", "exec", "x", "dlx"];

/// Leading programs that deny and ask rules look through: `sudo rm` is
/// judged as `rm`.
const TRANSPARENT_WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "command", "builtin", "exec", "nice", "ionice", "nohup", "time",
    "timeout", "stdbuf", "setsid", "xargs", "taskset", "chrt", "runuser", "pkexec",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Specifier {
    Command { words: Vec<String>, prefix: bool },
    Path(String),
}

/// The read tools a `read(...)` rule covers.
const READ_TOOLS: &[&str] = &["read_file", "list_directory", "grep"];

/// Parses `tool` or `tool(specifier)` into a canonical tool name and matcher.
/// `read` stays a family name covering every read tool.
pub(crate) fn parse_rule(text: &str) -> Result<(String, Option<Specifier>), String> {
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
        "read" => "read".to_string(),
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
        let path = normalize_rule_path(specifier);
        if path.is_empty() || path.starts_with('/') || path.split('/').any(|part| part == "..") {
            return Err(format!(
                "rule `{text}`: paths are relative to the project and stay inside it"
            ));
        }
        Specifier::Path(path)
    } else {
        return Err(format!(
            "rule `{text}`: only terminal and path tools take a specifier"
        ));
    };
    Ok((tool, Some(specifier)))
}

/// The canonical spelling of a rule, so `bash(x:*)` and `terminal(x *)` are
/// stored and removed as the same rule.
pub(crate) fn canonical_rule(text: &str) -> Result<String, String> {
    let (tool, specifier) = parse_rule(text)?;
    Ok(match specifier {
        None => tool,
        Some(Specifier::Command { words, prefix }) => {
            format!(
                "{tool}({}{})",
                words.join(" "),
                if prefix { " *" } else { "" }
            )
        }
        Some(Specifier::Path(path)) => format!("{tool}({path})"),
    })
}

pub(crate) fn tool_matches(rule_tool: &str, call_tool: &str) -> bool {
    rule_tool == call_tool || (rule_tool == "read" && READ_TOOLS.contains(&call_tool))
}

fn path_tool(tool: &str) -> bool {
    tool == "read" || tool == "apply_patch" || READ_TOOLS.contains(&tool)
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

fn program_name(word: &str) -> String {
    word.rsplit('/').next().unwrap_or(word).to_ascii_lowercase()
}

/// Exact allow matching: the words the user approved, in order.
pub(crate) fn allow_command_matches(segment: &[String], rule: &[String], prefix: bool) -> bool {
    if segment.len() < rule.len() || (!prefix && segment.len() != rule.len()) {
        return false;
    }
    segment
        .iter()
        .zip(rule)
        .all(|(word, expected)| word == expected)
}

/// Where the real program of a segment may start, for deny and ask rules.
/// Leading `NAME=value` words are skipped, and after a transparent wrapper
/// (`sudo -u root rm`, `env -i rm`) every later word is a candidate, since
/// wrapper options cannot be parsed in general.
fn candidate_starts(words: &[String]) -> Vec<&[String]> {
    let mut start = 0;
    while words
        .get(start)
        .is_some_and(|word| word.contains('=') && !word.starts_with('='))
    {
        start += 1;
    }
    let rest = &words[start..];
    match rest.first() {
        Some(word) if TRANSPARENT_WRAPPERS.contains(&program_name(word).as_str()) => {
            let mut starts = vec![rest];
            starts.extend((1..rest.len()).map(|index| &rest[index..]));
            starts
        }
        _ => vec![rest],
    }
}

/// Loose matching for deny and ask rules: wrappers, environment assignments,
/// program paths and case are ignored.
pub(crate) fn restrictive_command_matches(command: &str, rule: &[String], prefix: bool) -> bool {
    let rule: Vec<String> = rule.iter().map(|word| word.to_ascii_lowercase()).collect();
    let matches_at = |words: &[String]| {
        if words.len() < rule.len() || (!prefix && words.len() != rule.len()) {
            return false;
        }
        words
            .iter()
            .zip(&rule)
            .enumerate()
            .all(|(index, (word, expected))| {
                let word = if index == 0 {
                    program_name(word)
                } else {
                    word.to_ascii_lowercase()
                };
                &word == expected
            })
    };
    if let Some(segments) = command_segments(command) {
        return segments
            .iter()
            .any(|segment| candidate_starts(segment).into_iter().any(matches_at));
    }
    // Unsplittable: drop quoting, split on every shell operator and look for
    // the rule's words at any word position.
    let flattened: String = command
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '\\'))
        .map(|c| {
            if matches!(
                c,
                ';' | '&' | '|' | '(' | ')' | '<' | '>' | '{' | '}' | '$' | '`'
            ) || c.is_whitespace()
            {
                ' '
            } else {
                c
            }
        })
        .collect();
    let words: Vec<String> = flattened
        .split(' ')
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect();
    (0..words.len()).any(|index| {
        let candidate = &words[index..];
        candidate.len() >= rule.len()
            && candidate
                .iter()
                .zip(&rule)
                .enumerate()
                .all(|(position, (word, expected))| {
                    let word = if position == 0 {
                        program_name(word)
                    } else {
                        word.to_ascii_lowercase()
                    };
                    &word == expected
                })
    })
}

/// Workspace-relative paths a call reads or edits, or `None` when they
/// cannot be determined. Paths are given both lexically and, when they
/// exist, with symbolic links resolved.
pub(crate) fn call_paths(call: &ToolCall, roots: &[&Path]) -> Option<Vec<String>> {
    let root = roots.first()?;
    let raw: Vec<String> = if call.tool_name == "apply_patch" {
        git_patch_paths(
            call.arguments
                .get("patch")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            root,
        )
        .ok()?
    } else {
        vec![call
            .arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(".")
            .to_string()]
    };
    let mut paths = Vec::new();
    for path in &raw {
        let path = Path::new(path);
        paths.push(relative_lexical(path, roots));
        if let Ok(resolved) = root.join(path).canonicalize() {
            let canonical_roots: Vec<PathBuf> = roots
                .iter()
                .filter_map(|root| root.canonicalize().ok())
                .collect();
            let canonical_refs: Vec<&Path> = canonical_roots.iter().map(PathBuf::as_path).collect();
            paths.push(relative_lexical(&resolved, &canonical_refs));
        }
    }
    paths.sort();
    paths.dedup();
    Some(paths)
}

fn relative_lexical(path: &Path, roots: &[&Path]) -> String {
    let path = roots
        .iter()
        .find_map(|root| path.strip_prefix(root).ok())
        .unwrap_or(path);
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

/// The paths `git apply` would touch, as Git itself parses the patch (with
/// its default `-p1` stripping and C-quoted names decoded).
pub(crate) fn git_patch_paths(patch: &str, cwd: &Path) -> Result<Vec<String>, String> {
    let mut command = std::process::Command::new("git");
    command
        .args(["apply", "--numstat", "-z", "-"])
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
    ] {
        command.env_remove(key);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot read the patch's paths: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("cannot read the patch's paths")?;
    let input = patch.as_bytes().to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child
        .wait_with_output()
        .map_err(|error| format!("cannot read the patch's paths: {error}"))?;
    let _ = writer.join();
    if !output.status.success() {
        return Err(format!(
            "the patch cannot be read: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(parse_numstat(&output.stdout))
}

/// Parses `git apply --numstat -z`: `added\tdeleted\tpath\0`, or for a
/// rename or copy `added\tdeleted\t\0old\0new\0`.
pub(crate) fn parse_numstat(output: &[u8]) -> Vec<String> {
    let mut fields = output.split(|byte| *byte == 0).peekable();
    let mut paths = Vec::new();
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(field);
        let mut columns = text.splitn(3, '\t');
        let (_, _, path) = (columns.next(), columns.next(), columns.next().unwrap_or(""));
        if path.is_empty() {
            for _ in 0..2 {
                if let Some(name) = fields.next() {
                    paths.push(String::from_utf8_lossy(name).into_owned());
                }
            }
        } else {
            paths.push(path.to_string());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Gitignore-style matching: a glob without `/` matches a file name at any
/// depth; otherwise it matches from the workspace root. `**` spans
/// directories and a match on a directory covers everything below it.
/// Comparison ignores ASCII case, as macOS and Windows file systems do.
pub(crate) fn glob_matches(glob: &str, path: &str) -> bool {
    let glob = glob.to_ascii_lowercase();
    let path = path.to_ascii_lowercase();
    let path_parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if glob.is_empty() {
        return false;
    }
    if !glob.contains('/') {
        return path_parts.iter().any(|part| segment_matches(&glob, part));
    }
    let glob_parts: Vec<&str> = glob.split('/').filter(|part| !part.is_empty()).collect();
    (0..=path_parts.len()).any(|end| parts_match(&glob_parts, &path_parts[..end]))
}

/// Whether listing or searching `directory` would reach paths an anchored
/// glob names: `secrets/**` is reached by a search of `.` or `secrets`.
pub(crate) fn directory_reaches(glob: &str, directory: &str) -> bool {
    if !glob.contains('/') {
        return false;
    }
    let glob = glob.to_ascii_lowercase();
    let directory = directory.to_ascii_lowercase();
    let directory: Vec<&str> = directory
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let glob_parts: Vec<&str> = glob.split('/').filter(|part| !part.is_empty()).collect();
    directory.iter().enumerate().all(|(index, part)| {
        glob_parts
            .get(index)
            .is_some_and(|expected| *expected == "**" || segment_matches(expected, part))
    }) && directory.len() <= glob_parts.len()
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
/// Commands with glob characters offer nothing, because the shell expands
/// them and a saved `*` would read as a prefix.
pub(crate) fn suggested_terminal_rule(command: &str) -> Option<String> {
    let segments = command_segments(command)?;
    let [words] = segments.as_slice() else {
        return None;
    };
    if words.len() < 2
        || command.contains('\n')
        || words.iter().any(|word| word.contains(['*', '?', '[', ']']))
    {
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
    let wraps = |word: &str| WRAPPER_PROGRAMS.contains(&program_name(word).as_str());
    if program.contains('/')
        || program.contains('=')
        || wraps(program)
        || !subcommand_word(&words[1])
    {
        return Some(exact);
    }
    let mut prefix = vec![words[0].clone(), words[1].clone()];
    if RUNNER_SUBCOMMANDS.contains(&words[1].as_str()) {
        match words.get(2) {
            Some(script) if subcommand_word(script) && !wraps(script) => {
                prefix.push(script.clone());
            }
            _ => return Some(exact),
        }
    }
    Some(format!("{} *", prefix.join(" ")))
}
