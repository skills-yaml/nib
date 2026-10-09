//! Descriptor-relative skill reads. Every component is opened without following
//! links; held parent handles keep a pathname swap from changing the read scope.
use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

pub(super) fn open_regular_file(path: &Path) -> io::Result<File> {
    open_with_hook(path, |_| {})
}

fn open_child(parent: &cap_std::fs::Dir, name: &Path, directory: bool) -> io::Result<File> {
    let mut options = cap_std::fs::OpenOptions::new();
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(
            libc::O_NOFOLLOW | libc::O_NONBLOCK | if directory { libc::O_DIRECTORY } else { 0 },
        );
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        // Omit delete sharing to pin directories against rename while held.
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    }
    let file = parent.open_with(name, &options)?.into_std();
    let metadata = file.metadata()?;
    if crate::fs_security::metadata_is_link_or_reparse(&metadata)
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "skill path component is not a regular no-link entry",
        ));
    }
    Ok(file)
}

fn open_with_hook(path: &Path, mut after_directory: impl FnMut(&Path)) -> io::Result<File> {
    let absolute = crate::fs_security::absolute_path(path)?;
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => anchor.push(component.as_os_str()),
            Component::Normal(name) => names.push(name.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "skill paths cannot contain parent components",
                ))
            }
        }
    }
    let leaf = names
        .pop()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "skill path has no filename"))?;
    let mut directory = cap_std::fs::Dir::open_ambient_dir(&anchor, cap_std::ambient_authority())?;
    let mut traversed = anchor;
    for name in names {
        let opened = open_child(&directory, Path::new(&name), true)?;
        directory = cap_std::fs::Dir::from_std_file(opened);
        traversed.push(name);
        after_directory(&traversed);
    }
    open_child(&directory, Path::new(&leaf), false)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::symlink;

    #[test]
    fn relative_resource_directory_links_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("real")).unwrap();
        std::fs::write(root.path().join("real/guide.md"), "redirected").unwrap();
        symlink("real", root.path().join("references")).unwrap();
        assert!(open_regular_file(&root.path().join("references/guide.md")).is_err());
    }

    #[test]
    fn parent_swap_cannot_redirect_a_skill_read() {
        let root = tempfile::tempdir().unwrap();
        let skill = root.path().join("skill");
        let references = skill.join("references");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&references).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(references.join("guide.md"), "authorized").unwrap();
        std::fs::write(outside.join("guide.md"), "OUTSIDE_SECRET").unwrap();
        let skill = skill.canonicalize().unwrap();
        let result = open_with_hook(&skill.join("references/guide.md"), |opened| {
            if opened == skill {
                std::fs::rename(&references, skill.join("held-references")).unwrap();
                symlink(&outside, &references).unwrap();
            }
        });
        assert!(result.is_err());
    }

    #[test]
    fn swapping_an_open_parent_keeps_reads_bound_to_the_original_directory() {
        let root = tempfile::tempdir().unwrap();
        let references = root.path().join("skill/references");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&references).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(references.join("guide.md"), "authorized").unwrap();
        std::fs::write(outside.join("guide.md"), "OUTSIDE_SECRET").unwrap();
        let canonical = references.canonicalize().unwrap();
        let mut file = open_with_hook(&canonical.join("guide.md"), |opened| {
            if opened == canonical {
                std::fs::rename(&references, root.path().join("original")).unwrap();
                symlink(&outside, &references).unwrap();
            }
        })
        .unwrap();
        let mut content = String::new();
        file.read_to_string(&mut content).unwrap();
        assert_eq!(content, "authorized");
    }
}
