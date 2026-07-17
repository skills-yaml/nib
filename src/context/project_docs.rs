//! Bounded loading of project-local standards and library documentation.

use std::collections::BinaryHeap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use super::RuntimeContextSection;

const MAX_PROJECT_DOC_FILES: usize = 64;
const MAX_PROJECT_DOC_FILE_BYTES: usize = 32 * 1024;
const MAX_PROJECT_DOC_TOTAL_BYTES: usize = 128 * 1024;
const MAX_DISCOVERY_ENTRIES: usize = 4_096;
const MAX_DISCOVERY_DEPTH: usize = 8;
const BOUNDED_MARKER: &str = "\n\n...[project documentation bounded]...";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DocumentKind {
    Standards,
    Libraries,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    kind: DocumentKind,
    path: PathBuf,
    label: String,
}

/// Loads conventional project-local documentation without following symlinks.
pub fn load_project_docs(project_root: &Path) -> Vec<RuntimeContextSection> {
    let Ok(project_root) = project_root.canonicalize() else {
        return Vec::new();
    };
    if !is_real_directory(&project_root, &project_root) {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    let mut scanned = 0usize;
    for (relative_root, kind) in [
        ("docs/standards", DocumentKind::Standards),
        ("docs/tech", DocumentKind::Standards),
        ("docs/libs", DocumentKind::Libraries),
    ] {
        collect_document_tree(
            &project_root,
            &project_root.join(relative_root),
            kind,
            0,
            &mut scanned,
            &mut candidates,
        );
    }
    for relative_root in ["backend/libs", "libs"] {
        collect_library_docs(
            &project_root,
            &project_root.join(relative_root),
            &mut scanned,
            &mut candidates,
        );
    }

    candidates.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.label.cmp(&right.label))
    });
    candidates.dedup_by(|left, right| left.path == right.path);

    let mut sections = Vec::new();
    let mut total_bytes = 0usize;
    for candidate in candidates {
        if sections.len() >= MAX_PROJECT_DOC_FILES || total_bytes >= MAX_PROJECT_DOC_TOTAL_BYTES {
            break;
        }
        let remaining = MAX_PROJECT_DOC_TOTAL_BYTES - total_bytes;
        let limit = remaining.min(MAX_PROJECT_DOC_FILE_BYTES);
        let Some(content) = read_bounded_regular_file(&project_root, &candidate.path, limit) else {
            continue;
        };
        total_bytes += content.len();
        sections.push(RuntimeContextSection {
            label: candidate.label,
            content,
        });
    }
    sections
}

fn collect_document_tree(
    project_root: &Path,
    directory: &Path,
    kind: DocumentKind,
    depth: usize,
    scanned: &mut usize,
    candidates: &mut Vec<Candidate>,
) {
    if depth > MAX_DISCOVERY_DEPTH
        || *scanned >= MAX_DISCOVERY_ENTRIES
        || !is_real_directory(project_root, directory)
    {
        return;
    }
    let remaining = MAX_DISCOVERY_ENTRIES.saturating_sub(*scanned);
    for path in sorted_directory_entries(directory, remaining) {
        if *scanned >= MAX_DISCOVERY_ENTRIES {
            break;
        }
        *scanned += 1;
        if has_hidden_or_non_utf8_name(&path) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if crate::fs_security::metadata_is_link_or_reparse(&metadata) {
            continue;
        }
        if metadata.is_dir() {
            collect_document_tree(project_root, &path, kind, depth + 1, scanned, candidates);
        } else if metadata.is_file() && is_document_file(&path) {
            push_candidate(project_root, path, kind, candidates);
        }
    }
}

fn collect_library_docs(
    project_root: &Path,
    library_root: &Path,
    scanned: &mut usize,
    candidates: &mut Vec<Candidate>,
) {
    if *scanned >= MAX_DISCOVERY_ENTRIES || !is_real_directory(project_root, library_root) {
        return;
    }
    let remaining = MAX_DISCOVERY_ENTRIES.saturating_sub(*scanned);
    for library in sorted_directory_entries(library_root, remaining) {
        if *scanned >= MAX_DISCOVERY_ENTRIES {
            break;
        }
        *scanned += 1;
        if has_hidden_or_non_utf8_name(&library) || !is_real_directory(project_root, &library) {
            continue;
        }
        let remaining = MAX_DISCOVERY_ENTRIES.saturating_sub(*scanned);
        for child in sorted_directory_entries(&library, remaining) {
            if *scanned >= MAX_DISCOVERY_ENTRIES {
                break;
            }
            *scanned += 1;
            if has_hidden_or_non_utf8_name(&child) {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&child) else {
                continue;
            };
            if crate::fs_security::metadata_is_link_or_reparse(&metadata) {
                continue;
            }
            if metadata.is_file() && is_readme(&child) {
                push_candidate(project_root, child, DocumentKind::Libraries, candidates);
            } else if metadata.is_dir()
                && child
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case("docs"))
            {
                collect_document_tree(
                    project_root,
                    &child,
                    DocumentKind::Libraries,
                    0,
                    scanned,
                    candidates,
                );
            }
        }
    }
}

fn push_candidate(
    project_root: &Path,
    path: PathBuf,
    kind: DocumentKind,
    candidates: &mut Vec<Candidate>,
) {
    let Some(label) = project_relative_label(project_root, &path) else {
        return;
    };
    candidates.push(Candidate { kind, path, label });
}

fn sorted_directory_entries(directory: &Path, limit: usize) -> Vec<PathBuf> {
    sorted_directory_entries_with_observer(directory, limit, |_| {})
}

fn sorted_directory_entries_with_observer(
    directory: &Path,
    limit: usize,
    mut observe_heap_len: impl FnMut(usize),
) -> Vec<PathBuf> {
    if limit == 0 {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths = BinaryHeap::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if paths.len() < limit {
            paths.push(path);
        } else if paths.peek().is_some_and(|largest| path < *largest) {
            paths.pop();
            paths.push(path);
        }
        observe_heap_len(paths.len());
    }
    paths.into_sorted_vec()
}

fn is_real_directory(project_root: &Path, directory: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(directory) else {
        return false;
    };
    if crate::fs_security::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return false;
    }
    if crate::fs_security::verify_directory_without_symlinks(directory).is_err() {
        return false;
    }
    directory
        .canonicalize()
        .is_ok_and(|canonical| canonical.starts_with(project_root))
}

fn has_hidden_or_non_utf8_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_none_or(|name| name.starts_with('.'))
}

fn is_document_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["md", "markdown", "txt", "rst"]
                .iter()
                .any(|allowed| extension.eq_ignore_ascii_case(allowed))
        })
}

fn is_readme(path: &Path) -> bool {
    is_document_file(path)
        && path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.eq_ignore_ascii_case("readme"))
}

fn project_relative_label(project_root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(project_root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return None;
        };
        parts.push(part.to_str()?);
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn read_bounded_regular_file(project_root: &Path, path: &Path, limit: usize) -> Option<String> {
    read_bounded_regular_file_with_hook(project_root, path, limit, || {})
}

fn read_bounded_regular_file_with_hook(
    project_root: &Path,
    path: &Path,
    limit: usize,
    before_read: impl FnOnce(),
) -> Option<String> {
    if limit == 0 {
        return None;
    }
    let parent = path.parent()?;
    if !is_real_directory(project_root, parent) {
        return None;
    }
    let before = fs::symlink_metadata(path).ok()?;
    if crate::fs_security::metadata_is_link_or_reparse(&before) || !before.is_file() {
        return None;
    }
    let canonical = path.canonicalize().ok()?;
    if !canonical.starts_with(project_root) {
        return None;
    }

    let validated_file = open_regular_file_without_following_links(path).ok()?;
    let validated = validated_file.metadata().ok()?;
    if !validated.is_file() {
        return None;
    }
    let validated_identity = opened_file_identity(validated_file)?;

    let mut file = open_regular_file_without_following_links(path).ok()?;
    let opened = file.metadata().ok()?;
    if !opened.is_file() || opened.len() != validated.len() {
        return None;
    }
    let opened_identity = opened_file_identity(file.try_clone().ok()?)?;
    if validated_identity != opened_identity
        || !is_real_directory(project_root, parent)
        || !path
            .canonicalize()
            .is_ok_and(|resolved| resolved.starts_with(project_root))
    {
        return None;
    }

    before_read();
    let mut bytes = Vec::with_capacity(limit.saturating_add(1));
    file.by_ref()
        .take(u64::try_from(limit.saturating_add(1)).ok()?)
        .read_to_end(&mut bytes)
        .ok()?;

    let after = fs::symlink_metadata(path).ok()?;
    let post_probe = open_regular_file_without_following_links(path).ok()?;
    let post_opened = post_probe.metadata().ok()?;
    let post_identity = opened_file_identity(post_probe)?;
    if crate::fs_security::metadata_is_link_or_reparse(&after)
        || !after.is_file()
        || !post_opened.is_file()
        || post_opened.len() != opened.len()
        || post_identity != opened_identity
        || !is_real_directory(project_root, parent)
        || !path
            .canonicalize()
            .is_ok_and(|resolved| resolved.starts_with(project_root))
    {
        return None;
    }

    let truncated = bytes.len() > limit;
    let content_limit = if truncated && limit > BOUNDED_MARKER.len() {
        limit - BOUNDED_MARKER.len()
    } else {
        limit
    };
    bytes.truncate(content_limit);
    let mut content = utf8_prefix(&bytes)?.to_string();
    if truncated && limit > BOUNDED_MARKER.len() {
        content.push_str(BOUNDED_MARKER);
    }
    Some(content)
}

#[cfg(any(unix, windows))]
fn open_regular_file_without_following_links(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_regular_file_without_following_links(_path: &Path) -> std::io::Result<File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "stable no-follow file identity is unavailable on this platform",
    ))
}

fn opened_file_identity(file: File) -> Option<crate::fs_security::FileIdentity> {
    crate::fs_security::FileIdentity::from_file(file).ok()
}

fn utf8_prefix(bytes: &[u8]) -> Option<&str> {
    match std::str::from_utf8(bytes) {
        Ok(content) => Some(content),
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use tempfile::tempdir;

    fn write(path: &Path, content: impl AsRef<[u8]>) {
        fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        fs::write(path, content).expect("document");
    }

    #[test]
    fn loads_only_conventional_docs_in_standards_first_lexical_order() {
        let root = tempdir().expect("tempdir");
        write(&root.path().join("docs/standards/z.md"), "standard z");
        write(&root.path().join("docs/tech/a.md"), "technical a");
        write(&root.path().join("docs/libs/omega.md"), "library omega");
        write(
            &root.path().join("backend/libs/alpha/README.md"),
            "library alpha",
        );
        write(
            &root.path().join("backend/libs/alpha/docs/guide.md"),
            "alpha guide",
        );
        write(
            &root.path().join("backend/libs/alpha/design.md"),
            "not conventional library documentation",
        );
        write(
            &root
                .path()
                .join("backend/libs/alpha/.pytest_cache/README.md"),
            "hidden cache",
        );

        let sections = load_project_docs(root.path());
        let labels = sections
            .iter()
            .map(|section| section.label.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            labels,
            [
                "docs/standards/z.md",
                "docs/tech/a.md",
                "backend/libs/alpha/README.md",
                "backend/libs/alpha/docs/guide.md",
                "docs/libs/omega.md",
            ]
        );
        assert!(!sections
            .iter()
            .any(|section| section.content.contains("not conventional")
                || section.content.contains("hidden cache")));
    }

    #[test]
    fn bounds_each_file_the_aggregate_and_document_count() {
        let root = tempdir().expect("tempdir");
        write(
            &root.path().join("docs/standards/000-large.md"),
            "x".repeat(MAX_PROJECT_DOC_FILE_BYTES * 2),
        );
        for index in 0..100 {
            write(
                &root.path().join(format!("docs/tech/{index:03}-guide.md")),
                "y".repeat(4_096),
            );
        }

        let sections = load_project_docs(root.path());

        assert!(sections.len() <= MAX_PROJECT_DOC_FILES);
        assert!(sections
            .iter()
            .all(|section| section.content.len() <= MAX_PROJECT_DOC_FILE_BYTES));
        assert!(
            sections
                .iter()
                .map(|section| section.content.len())
                .sum::<usize>()
                <= MAX_PROJECT_DOC_TOTAL_BYTES
        );
        assert!(sections[0].content.contains(BOUNDED_MARKER));
    }

    #[test]
    fn bounds_directory_selection_before_sorting_overflow_entries() {
        let root = tempdir().expect("tempdir");
        let docs = root.path().join("docs/standards");
        fs::create_dir_all(&docs).expect("standards directory");
        for index in (0..MAX_DISCOVERY_ENTRIES + 2).rev() {
            fs::write(docs.join(format!("{index:05}.md")), "standard").expect("overflow document");
        }

        let observed_entries = Cell::new(0usize);
        let peak_heap_len = Cell::new(0usize);
        let selected =
            sorted_directory_entries_with_observer(&docs, MAX_DISCOVERY_ENTRIES, |heap_len| {
                observed_entries.set(observed_entries.get() + 1);
                peak_heap_len.set(peak_heap_len.get().max(heap_len));
            });

        assert_eq!(selected.len(), MAX_DISCOVERY_ENTRIES);
        assert_eq!(observed_entries.get(), MAX_DISCOVERY_ENTRIES + 2);
        assert_eq!(peak_heap_len.get(), MAX_DISCOVERY_ENTRIES);
        assert_eq!(
            selected.first().and_then(|path| path.file_name()),
            Some(std::ffi::OsStr::new("00000.md"))
        );
        assert_eq!(
            selected.last().and_then(|path| path.file_name()),
            Some(std::ffi::OsStr::new("04095.md"))
        );
        assert!(!selected.iter().any(|path| {
            path.file_name() == Some(std::ffi::OsStr::new("04096.md"))
                || path.file_name() == Some(std::ffi::OsStr::new("04097.md"))
        }));
    }

    fn assert_replacement_during_read_fails_closed() {
        let root = tempdir().expect("tempdir");
        let path = root.path().join("docs/standards/raced.md");
        let original = root.path().join("docs/standards/original.md");
        write(&path, "validated content");

        let content = read_bounded_regular_file_with_hook(
            root.path(),
            &path,
            MAX_PROJECT_DOC_FILE_BYTES,
            || {
                fs::rename(&path, &original).expect("move validated file");
                fs::write(&path, "replacement content").expect("replace document path");
            },
        );

        assert_eq!(content, None);
    }

    #[cfg(unix)]
    #[test]
    fn unix_file_replacement_cannot_bypass_opened_identity_check() {
        assert_replacement_during_read_fails_closed();
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_replacement_cannot_bypass_opened_identity_check() {
        assert_replacement_during_read_fails_closed();
    }

    #[cfg(not(any(unix, windows)))]
    #[test]
    fn unsupported_file_identity_semantics_fail_closed() {
        let root = tempdir().expect("tempdir");
        let path = root.path().join("docs/standards/unsupported.md");
        write(&path, "must not load");

        assert_eq!(
            read_bounded_regular_file(root.path(), &path, MAX_PROJECT_DOC_FILE_BYTES),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn ignores_symlinked_files_and_directories_that_escape_the_project() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("tempdir");
        let outside = tempdir().expect("outside");
        write(
            &root.path().join("docs/standards/local.md"),
            "local standard",
        );
        write(&outside.path().join("secret.md"), "OUTSIDE_SECRET");
        symlink(
            outside.path().join("secret.md"),
            root.path().join("docs/standards/linked.md"),
        )
        .expect("file symlink");
        fs::create_dir_all(root.path().join("docs/tech")).expect("tech root");
        symlink(outside.path(), root.path().join("docs/tech/linked")).expect("directory symlink");

        let sections = load_project_docs(root.path());

        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].label, "docs/standards/local.md");
        assert!(!sections[0].content.contains("OUTSIDE_SECRET"));
    }

    #[cfg(unix)]
    #[test]
    fn ignores_project_local_symlinked_conventional_root_ancestor() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("tempdir");
        let real_docs = root.path().join("real/docs");
        write(
            &real_docs.join("standards/linked.md"),
            "must not load through docs symlink",
        );
        symlink(&real_docs, root.path().join("docs")).expect("project-local docs symlink");

        let sections = load_project_docs(root.path());

        assert!(sections.is_empty());
    }
}
