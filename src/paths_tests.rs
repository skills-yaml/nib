use super::*;
use std::collections::BTreeSet;

#[test]
fn project_locations_share_the_project_directory() {
    let root = Path::new("/work/project");

    assert_eq!(NibPaths::project_state(root), root.join(".nib"));
    assert_eq!(NibPaths::project_prefs(root), root.join(".nib"));
    assert_eq!(NibPaths::project_skills(root), root.join(".nib/skills"));
    assert_eq!(NibPaths::project_plans(root), root.join(".nib/plans"));
    assert_eq!(
        NibPaths::session_worktrees(root),
        root.join(".nib/worktrees/sessions")
    );
    assert_eq!(
        NibPaths::subagent_worktrees(root),
        root.join(".nib/worktrees/subagents")
    );
    assert_eq!(
        NibPaths::relative_profile_state("default"),
        Path::new(".nib/profiles/default")
    );
    assert_eq!(
        NibPaths::relative_project_skills(),
        Path::new(".nib/skills")
    );
}

#[test]
fn project_directory_name_checks_are_exact_or_case_insensitive() {
    assert!(NibPaths::is_project_dir_name(OsStr::new(".nib")));
    assert!(!NibPaths::is_project_dir_name(OsStr::new(".NIB")));
    assert!(!NibPaths::is_project_dir_name(OsStr::new("nib")));
    assert!(NibPaths::is_project_dir_name_ignore_case(".NIB"));
    assert!(!NibPaths::is_project_dir_name_ignore_case(".nibs"));
}

#[test]
fn unit_tests_see_only_an_isolated_user_home() {
    let shared = NibPaths::user_home().expect("process test home");
    assert_eq!(shared, test_home::process_home());
    if let Some(real) = std::env::var_os("HOME") {
        assert!(!shared.starts_with(PathBuf::from(real).join(".nib")));
    }
    let home = TestNibHome::isolated();
    assert_eq!(
        NibPaths::user_home().as_deref(),
        Some(home.directory.path())
    );
    assert_eq!(
        NibPaths::user_config_file(),
        Some(home.directory.path().join("config.toml"))
    );
    drop(home);
    assert_eq!(NibPaths::user_home(), Some(shared));
}

#[test]
fn project_keys_are_stable_readable_and_distinct() {
    let first = tempfile::tempdir().expect("first project");
    let second = tempfile::tempdir().expect("second project");
    let named = first.path().join("My Project");
    std::fs::create_dir(&named).expect("named project");

    let key = NibPaths::project_key(&named).expect("key");
    assert_eq!(NibPaths::project_key(&named), Some(key.clone()));
    assert!(key.starts_with("My-Project-"), "{key}");
    assert_eq!(key.len(), "My-Project-".len() + 2 * PROJECT_KEY_HASH_BYTES);
    assert_ne!(
        NibPaths::project_key(first.path()),
        NibPaths::project_key(second.path())
    );
    assert_eq!(NibPaths::project_key(&first.path().join("missing")), None);

    let _home = TestNibHome::isolated();
    let directory = NibPaths::user_project_dir(&named).expect("project directory");
    assert_eq!(
        directory,
        NibPaths::user_home()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join("projects")
            .join(key)
    );
}

#[cfg(unix)]
#[test]
fn a_linked_worktree_shares_its_main_project_key() {
    let project = tempfile::tempdir().expect("project");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(project.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.name=nib",
        "-c",
        "user.email=nib@example.invalid",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "init",
    ]);
    let linked = project.path().join("linked");
    git(&["worktree", "add", "-q", "linked"]);

    assert_eq!(
        NibPaths::project_identity_root(&linked),
        Some(project.path().canonicalize().unwrap())
    );
    assert_eq!(
        NibPaths::project_key(&linked),
        NibPaths::project_key(project.path())
    );
    // A planted pointer that the repository does not link back to is not
    // accepted as a worktree of another project.
    let planted = tempfile::tempdir().expect("planted");
    std::fs::write(
        planted.path().join(".git"),
        format!(
            "gitdir: {}\n",
            project.path().join(".git/worktrees/linked").display()
        ),
    )
    .unwrap();
    assert_eq!(
        NibPaths::project_identity_root(planted.path()),
        Some(planted.path().canonicalize().unwrap())
    );
}

/// Files compiled only for tests: by name, or included through `#[path]`
/// from a test file or under `#[cfg(test)]`.
fn test_only_files(sources: &[(PathBuf, String)]) -> BTreeSet<PathBuf> {
    let by_name = |path: &Path| {
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        name == "tests.rs"
            || name.ends_with("_tests.rs")
            || name.starts_with("test_")
            || path.components().any(|part| part.as_os_str() == "tests")
    };
    let mut test_files: BTreeSet<PathBuf> = sources
        .iter()
        .map(|(path, _)| path.clone())
        .filter(|path| by_name(path))
        .collect();
    loop {
        let before = test_files.len();
        for (path, text) in sources {
            let lines: Vec<&str> = text.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                let Some(target) = line
                    .trim()
                    .strip_prefix("#[path = \"")
                    .and_then(|rest| rest.strip_suffix("\"]"))
                else {
                    continue;
                };
                let gated = lines[index.saturating_sub(2)..index]
                    .iter()
                    .any(|previous| previous.contains("#[cfg(") && previous.contains("test"));
                if gated || test_files.contains(path) {
                    test_files.insert(path.parent().expect("source parent").join(target));
                }
            }
        }
        if test_files.len() == before {
            return test_files;
        }
    }
}

fn collect_sources(directory: &Path, sources: &mut Vec<(PathBuf, String)>) {
    for entry in std::fs::read_dir(directory).expect("read source directory") {
        let path = entry.expect("source entry").path();
        if path.is_dir() {
            collect_sources(&path, sources);
        } else if path.extension() == Some(OsStr::new("rs")) {
            let text = std::fs::read_to_string(&path).expect("read source");
            sources.push((path, text));
        }
    }
}

#[test]
fn no_hand_built_nib_paths_outside_this_module() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources = Vec::new();
    collect_sources(&source_root, &mut sources);
    let test_files = test_only_files(&sources);
    let authority = source_root.join("paths.rs");

    let mut offenders = Vec::new();
    for (path, text) in &sources {
        if path == &authority || test_files.contains(path) {
            continue;
        }
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            // An inline test module closes the production part of a file.
            if line.trim() == "#[cfg(test)]"
                && lines.get(index + 1).is_some_and(|next| {
                    next.trim_start().starts_with("mod ") && next.ends_with('{')
                })
            {
                break;
            }
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            if code.contains("\".nib\"") || code.contains("\".nib/") {
                offenders.push(format!(
                    "{}:{}: {}",
                    path.strip_prefix(&source_root).unwrap_or(path).display(),
                    index + 1,
                    code
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "build nib paths with crate::paths::NibPaths instead:\n{}",
        offenders.join("\n")
    );
}
