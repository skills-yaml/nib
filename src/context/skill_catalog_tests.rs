use super::*;
use tempfile::tempdir;

fn manifest(root: &Path, directory: &str, name: &str) -> PathBuf {
    let folder = root.join(directory);
    fs::create_dir_all(&folder).unwrap();
    let path = folder.join("SKILL.md");
    fs::write(
        &path,
        format!(
            "---\nname: {name}\ndescription: Review selected code changes\n---\nFULL_BODY_MARKER\n"
        ),
    )
    .unwrap();
    path
}

#[test]
fn catalog_advertises_metadata_and_requires_explicit_or_model_loading() {
    let root = tempdir().unwrap();
    let path = manifest(root.path(), "review", "review-changes");
    let catalog = SkillCatalog::from_roots(&[root.path().into()], &[]).unwrap();
    let prompt = catalog.prompt(128_000);
    assert!(prompt.contains("review-changes"));
    assert!(prompt.contains("Review selected code"));
    assert!(!prompt.contains("FULL_BODY_MARKER"));
    assert!(catalog
        .explicit_selection("review selected code changes", &[])
        .unwrap()
        .is_empty());
    assert_eq!(
        catalog
            .explicit_selection("use $review-changes, please", &[])
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        catalog.load(path.to_str().unwrap(), false).unwrap().body,
        "FULL_BODY_MARKER"
    );
    assert!(catalog.explicit_selection("use $missing", &[]).is_err());
}

#[test]
fn duplicates_require_exact_paths_and_disabled_skills_cannot_activate() {
    let root = tempdir().unwrap();
    let a = manifest(root.path(), "a", "review");
    manifest(root.path(), "b", "review");
    let catalog = SkillCatalog::from_roots(&[root.path().into()], &[]).unwrap();
    assert_eq!(catalog.entries.len(), 2);
    assert!(catalog
        .load("review", true)
        .unwrap_err()
        .contains("ambiguous"));
    assert!(catalog.load(a.to_str().unwrap(), true).is_ok());
    let disabled = SkillCatalog::from_roots(
        &[root.path().into()],
        &[SkillConfig {
            path: a.clone(),
            enabled: false,
        }],
    )
    .unwrap();
    assert!(disabled
        .explicit_selection("ordinary request", &[a.to_string_lossy().into_owned()])
        .unwrap()
        .is_empty());
    assert!(disabled.explicit_selection("$review", &[]).is_err());
    assert!(disabled
        .load(a.to_str().unwrap(), true)
        .unwrap_err()
        .contains("disabled"));
}

#[test]
fn invocation_policy_requires_user_selection_and_revalidates_metadata() {
    let root = tempdir().unwrap();
    let path = manifest(root.path(), "review", "review");
    let policy = path.parent().unwrap().join("agents/openai.yaml");
    fs::create_dir_all(policy.parent().unwrap()).unwrap();
    fs::write(&policy, "policy:\n  allow_implicit_invocation: false\n").unwrap();
    let catalog = SkillCatalog::from_roots(&[root.path().into()], &[]).unwrap();
    assert!(!catalog.prompt(128_000).contains("\"name\":\"review\""));
    assert!(catalog.load("review", false).is_err());
    assert!(catalog.load("review", true).is_ok());
    fs::write(&policy, "policy:\n  allow_implicit_invocation: true\n").unwrap();
    assert!(catalog
        .load("review", true)
        .unwrap_err()
        .contains("changed"));
}

#[test]
fn changed_body_rejected_and_rediscovery_refreshes_it() {
    let root = tempdir().unwrap();
    let path = manifest(root.path(), "review", "review");
    let catalog = SkillCatalog::from_roots(&[root.path().into()], &[]).unwrap();
    fs::write(
        &path,
        "---\nname: review\ndescription: Updated\n---\nnew body\n",
    )
    .unwrap();
    assert!(catalog
        .load("review", true)
        .unwrap_err()
        .contains("changed"));
    let fresh = SkillCatalog::from_roots(&[root.path().into()], &[]).unwrap();
    assert_eq!(fresh.load("review", true).unwrap().body, "new body");
}

#[test]
fn budget_is_bounded_and_reports_omitted_entries() {
    let root = tempdir().unwrap();
    for index in 0..20 {
        manifest(
            root.path(),
            &format!("skill-{index}"),
            &format!("skill-{index}"),
        );
    }
    let catalog = SkillCatalog::from_roots(&[root.path().into()], &[]).unwrap();
    let prompt = catalog.prompt(8192);
    assert!(prompt.len() <= 8192 * 8 / 100);
    assert!(prompt.contains("omitted"));
    assert!(catalog.prompt(32).is_empty());
}

#[test]
fn repository_roots_stop_at_git_boundary_and_include_nested_scopes() {
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    let nested = root.path().join("src/component");
    fs::create_dir_all(&nested).unwrap();
    let roots = repo_roots(&nested);
    assert_eq!(roots.len(), 3);
    assert_eq!(roots.last().unwrap(), &root.path().join(".agents/skills"));
}

#[cfg(unix)]
#[test]
fn linked_folders_deduplicate_cycles_and_reject_retargeting() {
    use std::os::unix::fs::symlink;
    let source = tempdir().unwrap();
    let target = tempdir().unwrap();
    let original = manifest(target.path(), "version-one", "review");
    let replacement = manifest(target.path(), "version-two", "review");
    let link = source.path().join("review");
    symlink(original.parent().unwrap(), &link).unwrap();
    symlink(source.path(), source.path().join("cycle")).unwrap();
    let catalog = SkillCatalog::from_roots(
        &[source.path().into(), original.parent().unwrap().into()],
        &[],
    )
    .unwrap();
    assert_eq!(catalog.entries.len(), 1);
    assert!(catalog.load("review", true).is_ok());
    fs::remove_file(&link).unwrap();
    symlink(replacement.parent().unwrap(), &link).unwrap();
    assert!(catalog
        .load("review", true)
        .unwrap_err()
        .contains("changed"));
}

#[cfg(unix)]
#[test]
fn linked_manifests_resources_and_policy_are_rejected() {
    use std::os::unix::fs::symlink;
    let root = tempdir().unwrap();
    let path = manifest(root.path(), "review", "review");
    let outside = root.path().join("outside");
    fs::write(&outside, "secret").unwrap();
    let resource = path.parent().unwrap().join("reference.md");
    symlink(&outside, &resource).unwrap();
    assert!(super::super::skills::read_skill_resource_file(&resource, 32_768).is_err());
    fs::remove_file(&path).unwrap();
    symlink(&outside, &path).unwrap();
    assert!(SkillCatalog::from_roots(&[path.parent().unwrap().into()], &[]).is_err());
}

#[test]
fn bounded_catalog_prioritizes_repository_roots_over_user_path_spelling() {
    let root = tempdir().unwrap();
    let repository = root.path().join("z-repository");
    let user = root.path().join("a-user");
    manifest(&repository, "repo", "repository-workflow");
    for index in 0..20 {
        manifest(
            &user,
            &format!("user-{index:02}"),
            &format!("user-workflow-{index:02}"),
        );
    }
    let catalog = SkillCatalog::from_roots(&[repository, user], &[]).unwrap();
    assert_eq!(catalog.entries[0].metadata.name, "repository-workflow");
    let prompt = catalog.prompt(9_000);
    assert!(prompt.contains("repository-workflow"));
    assert!(prompt.contains("skills omitted"));
    if let Some(user_position) = prompt.find("user-workflow") {
        assert!(prompt.find("repository-workflow").unwrap() < user_position);
    }
}

#[cfg(unix)]
#[test]
fn linked_repository_skill_keeps_priority_over_its_canonical_target_spelling() {
    use std::os::unix::fs::symlink;
    let root = tempdir().unwrap();
    let repository = root.path().join("repository");
    let user = root.path().join("a-user");
    let target = manifest(root.path(), "z-cache/review", "linked-repository-workflow");
    fs::create_dir(&repository).unwrap();
    symlink(target.parent().unwrap(), repository.join("review")).unwrap();
    for index in 0..20 {
        manifest(
            &user,
            &format!("user-{index:02}"),
            &format!("user-workflow-{index:02}"),
        );
    }
    let catalog =
        SkillCatalog::from_roots(&[repository, user, root.path().join("z-cache")], &[]).unwrap();
    assert_eq!(catalog.entries.len(), 21);
    assert_eq!(
        catalog.entries[0].metadata.name,
        "linked-repository-workflow"
    );
    assert!(catalog.prompt(9_000).contains("linked-repository-workflow"));
}
