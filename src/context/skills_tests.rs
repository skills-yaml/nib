use super::*;
use serial_test::serial;
use tempfile::tempdir;

fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
    match value {
        Some(value) => std::env::set_var(name, value),
        None => std::env::remove_var(name),
    }
}

fn write_skill(root: &Path) -> PathBuf {
    let dir = root.join("rust-safety");
    fs::create_dir_all(&dir).expect("create skill directory");
    fs::create_dir_all(dir.join("references")).expect("reference directory");
    fs::create_dir_all(dir.join("templates")).expect("asset directory");
    fs::write(dir.join("references/checks.md"), "reference checks\n").expect("reference");
    fs::write(dir.join("templates/report.md"), "report template\n").expect("asset");
    let path = dir.join("SKILL.md");
    fs::write(
        &path,
        r#"---
name: rust-safety
description: Validate Rust changes safely
version: "1.2.0"
tags: [rust, cargo]
references: [references/checks.md]
assets: [templates/report.md]
constraints:
  deny_commands: ["cargo publish"]
  require_approval_tools: [apply_patch]
hooks:
  after_tool:
    - tool: apply_patch
      command: task check
---
Run the canonical Task gates after editing.
"#,
    )
    .expect("write skill");
    path
}

#[test]
fn parses_structured_frontmatter() {
    let dir = tempdir().expect("tempdir");
    let path = write_skill(dir.path());
    let skill = parse_skill_file(&path).expect("parse skill");

    assert_eq!(skill.frontmatter.version.as_deref(), Some("1.2.0"));
    assert_eq!(skill.frontmatter.tags, ["rust", "cargo"]);
    assert_eq!(skill.frontmatter.hooks.after_tool[0].command, "task check");
    assert!(skill.body.contains("canonical Task gates"));
    assert_eq!(
        skill.references[0].path,
        PathBuf::from("references/checks.md")
    );
    assert!(skill.references[0].content.contains("reference checks"));
    assert_eq!(skill.assets, [PathBuf::from("templates/report.md")]);
}

#[test]
fn discovers_skills_and_builds_policy_rules() {
    let dir = tempdir().expect("tempdir");
    let path = write_skill(dir.path());
    let paths = find_skills_in_paths(&[dir.path().to_path_buf()]);
    assert_eq!(paths, vec![path]);

    let skill = parse_skill_file(&paths[0]).expect("parse skill");

    let rules = policy_rules_for_skills(&[skill]);
    assert!(rules.iter().any(|rule| {
        rule.effect == SkillPolicyEffect::Deny
            && rule.argument_contains.as_deref() == Some("cargo publish")
    }));
    assert!(rules.iter().any(|rule| {
        rule.effect == SkillPolicyEffect::RequireApproval
            && rule.tool_name.as_deref() == Some("apply_patch")
    }));
}

#[cfg(unix)]
#[test]
fn frontmatter_cache_refreshes_same_length_replacement_with_restored_mtime() {
    use std::fs::FileTimes;

    let directory = tempdir().expect("tempdir");
    let skill_dir = directory.path().join("cached");
    fs::create_dir(&skill_dir).expect("skill directory");
    let manifest = skill_dir.join("SKILL.md");
    let first = "---\nname: alpha\ndescription: cached metadata\n---\nbody\n";
    let second = "---\nname: bravo\ndescription: cached metadata\n---\nbody\n";
    assert_eq!(first.len(), second.len());
    fs::write(&manifest, first).expect("first manifest");
    let modified = fs::metadata(&manifest)
        .expect("first metadata")
        .modified()
        .expect("first mtime");
    assert_eq!(
        parse_skill_frontmatter_file(&manifest)
            .expect("first frontmatter")
            .name,
        "alpha"
    );

    let replacement = skill_dir.join("replacement.md");
    fs::write(&replacement, second).expect("replacement manifest");
    fs::File::options()
        .write(true)
        .open(&replacement)
        .expect("replacement handle")
        .set_times(FileTimes::new().set_modified(modified))
        .expect("restore replacement mtime");
    fs::rename(&replacement, &manifest).expect("replace manifest");

    assert_eq!(
        parse_skill_frontmatter_file(&manifest)
            .expect("refreshed frontmatter")
            .name,
        "bravo"
    );
}

#[test]
#[serial]
fn configured_global_skill_root_is_discovered() {
    let project = tempdir().expect("project tempdir");
    let global = tempdir().expect("global tempdir");
    let manifest = write_skill(global.path());
    let previous = std::env::var_os("NIB_SKILLS_DIR");
    std::env::set_var("NIB_SKILLS_DIR", global.path());

    let discovered = find_skills(project.path());

    restore_env("NIB_SKILLS_DIR", previous);
    assert!(discovered.contains(&manifest));
}

#[cfg(unix)]
#[test]
fn rejects_regular_file_replacement_between_open_and_recheck() {
    let directory = tempdir().expect("tempdir");
    let manifest = directory.path().join("SKILL.md");
    let replacement = directory.path().join("replacement.md");
    fs::write(&manifest, "---\nname: original\n---\nBody\n").expect("manifest");
    fs::write(&replacement, "---\nname: replaced\n---\nBody\n").expect("replacement");

    let result = open_stable_skill_file_with_hook(
        &manifest,
        |metadata| validate_skill_manifest_metadata(&manifest, metadata),
        || {
            fs::rename(&replacement, &manifest)?;
            Ok(())
        },
    );

    assert!(matches!(result, Err(SkillError::InvalidResource(_))));
}

#[cfg(unix)]
#[test]
fn rejects_reference_replacement_between_open_and_recheck() {
    let directory = tempdir().expect("tempdir");
    let reference = directory.path().join("reference.md");
    let replacement = directory.path().join("replacement.md");
    fs::write(&reference, "original reference\n").expect("reference");
    fs::write(&replacement, "replaced reference\n").expect("replacement");

    let result = open_stable_skill_file_with_hook(
        &reference,
        |metadata| validate_skill_reference_metadata("reference.md", metadata),
        || {
            fs::rename(&replacement, &reference)?;
            Ok(())
        },
    );

    assert!(matches!(result, Err(SkillError::InvalidResource(_))));
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_skill_resources_inside_or_outside_the_skill_root() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join("skill");
    fs::create_dir_all(&skill_dir).expect("skill dir");
    let outside = dir.path().join("outside.md");
    fs::write(&outside, "secret").expect("outside");
    symlink(&outside, skill_dir.join("reference.md")).expect("symlink");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: scoped\nreferences: [reference.md]\n---\nBody\n",
    )
    .expect("manifest");

    assert!(matches!(
        parse_skill_file(&skill_dir.join("SKILL.md")),
        Err(SkillError::InvalidResource(_))
    ));

    fs::remove_file(skill_dir.join("reference.md")).expect("remove escape symlink");
    fs::write(skill_dir.join("real.md"), "local reference").expect("local reference");
    symlink("real.md", skill_dir.join("reference.md")).expect("local symlink");
    assert!(matches!(
        parse_skill_file(&skill_dir.join("SKILL.md")),
        Err(SkillError::InvalidResource(_))
    ));
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_resource_directory_ancestor() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().expect("tempdir");
    let skill_dir = directory.path().join("skill");
    let outside = directory.path().join("outside");
    fs::create_dir_all(&skill_dir).expect("skill directory");
    fs::create_dir_all(&outside).expect("outside directory");
    fs::write(outside.join("secret.md"), "secret").expect("outside resource");
    symlink(&outside, skill_dir.join("references")).expect("ancestor symlink");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: ancestor-link\nreferences: [references/secret.md]\n---\nBody\n",
    )
    .expect("manifest");

    assert!(matches!(
        parse_skill_file(&skill_dir.join("SKILL.md")),
        Err(SkillError::InvalidResource(_))
    ));
}

#[test]
fn discovery_ignores_atomic_install_staging_directories() {
    let directory = tempdir().expect("tempdir");
    let published = directory.path().join("published");
    let staging = directory.path().join(".nib-skill-deadbeef.tmp");
    fs::create_dir_all(&published).expect("published directory");
    fs::create_dir_all(&staging).expect("staging directory");
    fs::write(
        published.join("SKILL.md"),
        "---\nname: published\n---\nBody\n",
    )
    .expect("published manifest");
    fs::write(
        staging.join("SKILL.md"),
        "---\nname: unpublished\n---\nBody\n",
    )
    .expect("staging manifest");

    let discovered = find_skills_in_paths(&[directory.path().to_path_buf()]);
    assert_eq!(discovered, vec![published.join("SKILL.md")]);
}

#[test]
fn strict_discovery_rejects_directory_skill_manifest() {
    let directory = tempdir().expect("tempdir");
    let skill = directory.path().join("directory-manifest");
    let manifest = skill.join("SKILL.md");
    fs::create_dir_all(&manifest).expect("manifest directory");

    let error = find_skills_in_paths_strict(&[directory.path().to_path_buf()])
        .expect_err("manifest directory must fail strict discovery");
    assert!(matches!(
        error,
        SkillDiscoveryError::InvalidManifestType { path } if path == manifest
    ));
}

#[cfg(unix)]
#[test]
fn strict_discovery_rejects_dangling_symlink_skill_manifest() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().expect("tempdir");
    let skill = directory.path().join("dangling-manifest");
    fs::create_dir(&skill).expect("skill directory");
    let manifest = skill.join("SKILL.md");
    symlink(directory.path().join("missing-manifest"), &manifest)
        .expect("dangling manifest symlink");

    let error = find_skills_in_paths_strict(&[directory.path().to_path_buf()])
        .expect_err("dangling manifest symlink must fail strict discovery");
    assert!(matches!(
        error,
        SkillDiscoveryError::InvalidManifestType { path } if path == manifest
    ));
}

#[cfg(unix)]
#[test]
fn strict_discovery_rejects_special_file_skill_manifest() {
    use std::os::unix::net::UnixListener;

    let directory = tempdir().expect("tempdir");
    let skill = directory.path().join("special-manifest");
    fs::create_dir(&skill).expect("skill directory");
    let manifest = skill.join("SKILL.md");
    let _listener = UnixListener::bind(&manifest).expect("manifest socket");

    let error = find_skills_in_paths_strict(&[directory.path().to_path_buf()])
        .expect_err("special-file manifest must fail strict discovery");
    assert!(matches!(
        error,
        SkillDiscoveryError::InvalidManifestType { path } if path == manifest
    ));
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_skill_manifest() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().expect("tempdir");
    let outside = directory.path().join("outside.md");
    fs::write(&outside, "---\nname: linked\n---\nBody\n").expect("outside manifest");
    let skill_dir = directory.path().join("skill");
    fs::create_dir(&skill_dir).expect("skill directory");
    symlink(&outside, skill_dir.join("SKILL.md")).expect("manifest symlink");

    assert!(matches!(
        parse_skill_file(&skill_dir.join("SKILL.md")),
        Err(SkillError::InvalidResource(_))
    ));
}

#[test]
fn discovery_caps_the_number_of_skill_manifests() {
    let directory = tempdir().expect("tempdir");
    for index in 0..(MAX_DISCOVERED_SKILLS + 4) {
        let skill = directory.path().join(format!("skill-{index:03}"));
        fs::create_dir(&skill).expect("skill directory");
        fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: skill-{index:03}\n---\nBody\n"),
        )
        .expect("skill manifest");
    }

    let discovered = find_skills_in_paths(&[directory.path().to_path_buf()]);
    assert_eq!(discovered.len(), MAX_DISCOVERED_SKILLS);
}

#[test]
fn strict_discovery_reports_skill_count_truncation() {
    let directory = tempdir().expect("tempdir");
    for name in ["alpha", "beta"] {
        let skill = directory.path().join(name);
        fs::create_dir(&skill).expect("skill directory");
        fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\n---\nBody\n"),
        )
        .expect("skill manifest");
    }

    let error = find_skills_in_paths_strict_with_limits(
        &[directory.path().to_path_buf()],
        SkillDiscoveryLimits {
            max_skills: 1,
            max_entries: 16,
            max_depth: 4,
        },
    )
    .expect_err("strict discovery must report incomplete inventory");

    assert!(matches!(
        error,
        SkillDiscoveryError::Truncated {
            kind: "skill count",
            limit: 1
        }
    ));
}

#[test]
fn strict_discovery_reports_directory_entry_truncation() {
    let directory = tempdir().expect("tempdir");
    fs::create_dir(directory.path().join("alpha")).expect("first directory");
    fs::create_dir(directory.path().join("beta")).expect("second directory");

    let error = find_skills_in_paths_strict_with_limits(
        &[directory.path().to_path_buf()],
        SkillDiscoveryLimits {
            max_skills: 16,
            max_entries: 1,
            max_depth: 4,
        },
    )
    .expect_err("strict discovery must report incomplete traversal");

    assert!(matches!(
        error,
        SkillDiscoveryError::Truncated {
            kind: "directory entry",
            limit: 1
        }
    ));
}

#[test]
fn rejects_aggregate_reference_content_over_context_bound() {
    let dir = tempdir().expect("tempdir");
    let skill_dir = dir.path().join("bounded");
    fs::create_dir_all(&skill_dir).expect("skill dir");
    let mut configured = Vec::new();
    for index in 0..3 {
        let name = format!("reference-{index}.md");
        fs::write(skill_dir.join(&name), "x".repeat(22_000)).expect("reference");
        configured.push(name);
    }
    fs::write(
        skill_dir.join("SKILL.md"),
        format!(
            "---\nname: bounded\nreferences: [{}]\n---\nBody\n",
            configured.join(", ")
        ),
    )
    .expect("manifest");

    assert!(matches!(
        parse_skill_file(&skill_dir.join("SKILL.md")),
        Err(SkillError::ReferencesTooLarge)
    ));
}

#[test]
fn rejects_oversized_sparse_manifest_and_reference_before_reading() {
    let directory = tempdir().expect("tempdir");
    let skill_dir = directory.path().join("bounded");
    fs::create_dir(&skill_dir).expect("skill directory");
    let manifest = skill_dir.join("SKILL.md");
    fs::File::create(&manifest)
        .and_then(|file| file.set_len(MAX_SKILL_FILE_BYTES + 1))
        .expect("sparse manifest");
    assert!(matches!(
        parse_skill_file(&manifest),
        Err(SkillError::ManifestTooLarge(_))
    ));

    let reference = skill_dir.join("reference.md");
    fs::File::create(&reference)
        .and_then(|file| file.set_len(MAX_REFERENCE_BYTES + 1))
        .expect("sparse reference");
    fs::write(
        &manifest,
        "---\nname: bounded\nreferences: [reference.md]\n---\nBody\n",
    )
    .expect("bounded manifest");
    assert!(matches!(
        parse_skill_file(&manifest),
        Err(SkillError::ReferenceTooLarge(_))
    ));
}

#[test]
fn rejects_oversized_and_aggregate_assets_before_installation() {
    let directory = tempdir().expect("tempdir");
    let skill_dir = directory.path().join("asset-bounds");
    fs::create_dir(&skill_dir).expect("skill directory");
    let manifest = skill_dir.join("SKILL.md");
    let oversized = skill_dir.join("oversized.bin");
    fs::File::create(&oversized)
        .and_then(|file| file.set_len(MAX_ASSET_BYTES + 1))
        .expect("sparse oversized asset");
    fs::write(
        &manifest,
        "---\nname: asset-bounds\nassets: [oversized.bin]\n---\nBody\n",
    )
    .expect("manifest");
    assert!(matches!(
        parse_skill_file(&manifest),
        Err(SkillError::AssetTooLarge(_))
    ));

    let mut assets = Vec::new();
    for index in 0..5 {
        let name = format!("asset-{index}.bin");
        fs::File::create(skill_dir.join(&name))
            .and_then(|file| file.set_len(MAX_ASSET_BYTES))
            .expect("sparse aggregate asset");
        assets.push(name);
    }
    fs::write(
        &manifest,
        format!(
            "---\nname: asset-bounds\nassets: [{}]\n---\nBody\n",
            assets.join(", ")
        ),
    )
    .expect("aggregate manifest");
    assert!(matches!(
        parse_skill_file(&manifest),
        Err(SkillError::AssetsTooLarge)
    ));
}

#[test]
fn rejects_resource_paths_beyond_the_depth_bound_before_resolution() {
    let directory = tempdir().expect("tempdir");
    let skill_dir = directory.path().join("deep-resource");
    fs::create_dir(&skill_dir).expect("skill directory");
    let relative = (0..=MAX_RESOURCE_DEPTH)
        .map(|index| format!("level-{index}"))
        .collect::<Vec<_>>()
        .join("/");
    let manifest = skill_dir.join("SKILL.md");
    fs::write(
        &manifest,
        format!("---\nname: deep-resource\nassets: [{relative}]\n---\nBody\n"),
    )
    .expect("manifest");

    assert!(matches!(
        parse_skill_frontmatter_file(&manifest),
        Err(SkillError::InvalidResource(_))
    ));
}
