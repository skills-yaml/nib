use std::io;
use std::path::{Component, Path, PathBuf};

/// Identity of an open file, retaining the handle that established it.
#[cfg(windows)]
#[derive(Debug)]
pub struct FileIdentity {
    _file: std::fs::File,
    volume_serial_number: u64,
    file_id: [u8; 16],
}

#[cfg(windows)]
impl FileIdentity {
    /// Reads the full filesystem identity from an already-open file.
    pub fn from_file(file: std::fs::File) -> io::Result<Self> {
        use std::mem::size_of;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
        };

        let mut identity = FILE_ID_INFO::default();
        // SAFETY: `file` remains open for the call, and `identity` is a correctly sized,
        // writable FILE_ID_INFO buffer for the requested information class.
        let succeeded = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                std::ptr::from_mut(&mut identity).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if succeeded == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            _file: file,
            volume_serial_number: identity.VolumeSerialNumber,
            file_id: identity.FileId.Identifier,
        })
    }
}

#[cfg(windows)]
impl PartialEq for FileIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.volume_serial_number == other.volume_serial_number && self.file_id == other.file_id
    }
}

#[cfg(windows)]
impl Eq for FileIdentity {}

/// Identity of an open file, retaining the handle that established it.
#[cfg(not(windows))]
#[derive(Debug, Eq, PartialEq)]
pub struct FileIdentity(same_file::Handle);

#[cfg(not(windows))]
impl FileIdentity {
    /// Reads the filesystem identity from an already-open file.
    pub fn from_file(file: std::fs::File) -> io::Result<Self> {
        same_file::Handle::from_file(file).map(Self)
    }
}

pub(crate) fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

pub(crate) fn ensure_directory_without_symlinks(path: &Path) -> io::Result<PathBuf> {
    let absolute = absolute_path(path)?;
    let mut current = PathBuf::new();

    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "directory path contains a parent component: {}",
                        path.display()
                    ),
                ))
            }
            Component::Normal(part) => {
                current.push(part);
                ensure_directory_component(&current)?;
            }
        }
    }

    let canonical = current.canonicalize()?;
    if !canonical_paths_match(&canonical, &current) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "directory path resolves through a symlink: {}",
                path.display()
            ),
        ));
    }
    verify_existing_directory_components(&absolute, path)?;
    let confirmed = absolute.canonicalize()?;
    if !canonical_paths_match(&confirmed, &canonical) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "directory path changed while it was validated: {}",
                path.display()
            ),
        ));
    }
    Ok(confirmed)
}

pub(crate) fn verify_directory_without_symlinks(path: &Path) -> io::Result<()> {
    canonicalize_existing_directory_without_symlinks(path).map(drop)
}

/// Resolves an existing directory without creating any missing path component.
///
/// This is public only for the `nib` binary's skill installer, which must share the
/// library's Windows DOS-alias and reparse-point handling.
#[doc(hidden)]
pub fn canonicalize_existing_directory_without_symlinks(path: &Path) -> io::Result<PathBuf> {
    let absolute = absolute_path(path)?;
    verify_existing_directory_components(&absolute, path)?;
    let canonical = absolute.canonicalize()?;
    if !canonical_paths_match(&canonical, &absolute) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "directory path resolves through a symlink: {}",
                path.display()
            ),
        ));
    }
    verify_existing_directory_components(&absolute, path)?;
    let confirmed = absolute.canonicalize()?;
    if !canonical_paths_match(&confirmed, &canonical) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "directory path changed while it was validated: {}",
                path.display()
            ),
        ));
    }
    Ok(confirmed)
}

pub(crate) fn canonical_path_starts_with(
    canonical_path: &Path,
    requested_root: &Path,
) -> io::Result<bool> {
    Ok(canonical_path.starts_with(requested_root.canonicalize()?))
}

fn verify_existing_directory_components(absolute: &Path, display: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "directory path contains a parent component: {}",
                        display.display()
                    ),
                ))
            }
            Component::Normal(part) => {
                current.push(part);
                let metadata = std::fs::symlink_metadata(&current)?;
                if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                    return Err(invalid_directory_component(&current));
                }
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn canonical_paths_match(canonical: &Path, requested: &Path) -> bool {
    canonical == requested
}

#[cfg(windows)]
pub(crate) fn canonical_paths_match(canonical: &Path, requested: &Path) -> bool {
    let canonical = path_without_windows_verbatim_prefix(canonical);
    let normalized_requested = path_without_windows_verbatim_prefix(requested);
    canonical == normalized_requested
        || windows_long_path(requested)
            .map(|path| path_without_windows_verbatim_prefix(&path) == canonical)
            .unwrap_or(false)
}

#[cfg(windows)]
fn windows_long_path(path: &Path) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetLongPathNameW;

    let mut input = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if input.contains(&0) {
        return None;
    }
    input.push(0);
    let required = unsafe { GetLongPathNameW(input.as_ptr(), std::ptr::null_mut(), 0) };
    if required == 0 {
        return None;
    }
    let mut output = vec![0_u16; required as usize];
    let written = unsafe { GetLongPathNameW(input.as_ptr(), output.as_mut_ptr(), required) };
    if written == 0 || written as usize >= output.len() {
        return None;
    }
    output.truncate(written as usize);
    Some(PathBuf::from(OsString::from_wide(&output)))
}

#[cfg(windows)]
pub(crate) fn path_without_windows_verbatim_prefix(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    const BACKSLASH: u16 = b'\\' as u16;
    const QUESTION: u16 = b'?' as u16;
    const COLON: u16 = b':' as u16;
    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if !encoded.starts_with(&[BACKSLASH, BACKSLASH, QUESTION, BACKSLASH]) {
        return path.to_path_buf();
    }

    let remainder = &encoded[4..];
    let ascii_eq = |value: u16, expected: u8| {
        value == u16::from(expected) || value == u16::from(expected.to_ascii_lowercase())
    };
    let normalized = if remainder.len() >= 4
        && ascii_eq(remainder[0], b'U')
        && ascii_eq(remainder[1], b'N')
        && ascii_eq(remainder[2], b'C')
        && remainder[3] == BACKSLASH
    {
        let mut normalized = vec![BACKSLASH, BACKSLASH];
        normalized.extend_from_slice(&remainder[4..]);
        normalized
    } else if remainder.get(1) == Some(&COLON) {
        remainder.to_vec()
    } else {
        return path.to_path_buf();
    };
    PathBuf::from(OsString::from_wide(&normalized))
}

#[cfg(windows)]
pub(crate) fn path_for_external_command(path: &Path) -> PathBuf {
    path_without_windows_verbatim_prefix(path)
}

#[cfg(not(windows))]
pub(crate) fn path_for_external_command(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(windows)]
pub(crate) fn rename_open_entry_no_replace_windows<
    S: std::os::windows::io::AsRawHandle + ?Sized,
>(
    parent: &cap_std::fs::Dir,
    source: &S,
    destination: &Path,
) -> io::Result<()> {
    use std::mem::{offset_of, size_of};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Wdk::Storage::FileSystem::{
        FileRenameInformation, NtSetInformationFile, FILE_RENAME_INFORMATION,
    };
    use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    let mut components = destination.components();
    let Some(Component::Normal(name)) = components.next() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows handle-relative rename requires a direct-child destination",
        ));
    };
    if components.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows handle-relative rename requires a direct-child destination",
        ));
    }

    let destination = name.encode_wide().collect::<Vec<_>>();
    if destination.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows handle-relative rename requires a non-empty destination",
        ));
    }
    let filename_bytes = destination
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "filename too long"))?;
    let filename_bytes = u32::try_from(filename_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "filename too long"))?;
    let buffer_len = size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(filename_bytes as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "filename too long"))?;
    let buffer_len_u32 = u32::try_from(buffer_len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "filename too long"))?;
    let storage_len = buffer_len.div_ceil(size_of::<usize>());
    let mut buffer = vec![0_usize; storage_len];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut io_status = IO_STATUS_BLOCK::default();
    let status = unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = parent.as_raw_handle();
        (*info).FileNameLength = filename_bytes;
        std::ptr::copy_nonoverlapping(
            destination.as_ptr(),
            buffer
                .as_mut_ptr()
                .cast::<u8>()
                .add(offset_of!(FILE_RENAME_INFORMATION, FileName))
                .cast::<u16>(),
            destination.len(),
        );
        NtSetInformationFile(
            source.as_raw_handle(),
            &mut io_status,
            buffer.as_ptr().cast(),
            buffer_len_u32,
            FileRenameInformation,
        )
    };
    if status >= 0 {
        Ok(())
    } else {
        let code = unsafe { RtlNtStatusToDosError(status) };
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

fn ensure_directory_component(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() => {
            Err(invalid_directory_component(path))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match std::fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            let metadata = std::fs::symlink_metadata(path)?;
            if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                return Err(invalid_directory_component(path));
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn invalid_directory_component(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "directory component must be a local directory and not a symlink or reparse point: {}",
            path.display()
        ),
    )
}

#[cfg(windows)]
pub(crate) fn metadata_is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub(crate) fn metadata_is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

pub(crate) fn path_entry_exists(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

const MAX_REMOVAL_ENTRIES: usize = 200_000;
const MAX_REMOVAL_DEPTH: usize = 64;
const MAX_REMOVAL_NAME_UNITS: usize = 255;
const MAX_REMOVAL_PLAN_BYTES: usize = 32 * 1024 * 1024;

#[cfg(test)]
static REPLACE_AFTER_REMOVAL_QUARANTINE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(all(test, unix))]
static REPLACE_AFTER_REMOVAL_ENTRY_QUARANTINE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(all(test, unix))]
static REPLACE_REMOVAL_QUARANTINE_BEFORE_UNLINK: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(all(test, unix))]
static REPLACE_DIRECTORY_BEFORE_HANDLE_DELETE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RemovalIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial_number: u64,
    #[cfg(windows)]
    file_id: [u8; 16],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DirectoryIdentity(RemovalIdentity);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileIdentitySnapshot(RemovalIdentity);

pub(crate) fn file_identity_snapshot(file: &std::fs::File) -> io::Result<FileIdentitySnapshot> {
    removal_identity_from_file(file).map(FileIdentitySnapshot)
}

/// Creation-time identity evidence for a direct child directory.
#[derive(Debug, Clone)]
pub(crate) struct DirectoryRemovalReceipt {
    identity: RemovalIdentity,
    file: std::sync::Arc<std::fs::File>,
}

impl DirectoryRemovalReceipt {
    pub(crate) fn from_open_directory(file: std::fs::File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory ownership receipt requires an open local directory",
            ));
        }
        Ok(Self {
            identity: removal_identity_from_file(&file)?,
            file: std::sync::Arc::new(file),
        })
    }

    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        self.identity == other.identity
    }

    pub(crate) fn identity(&self) -> DirectoryIdentity {
        DirectoryIdentity(self.identity)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemovalEntryKind {
    File,
    Directory,
    #[cfg(unix)]
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemovalEntry {
    relative: PathBuf,
    identity: RemovalIdentity,
    kind: RemovalEntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemovalPlan {
    entries: Vec<RemovalEntry>,
    directories: std::collections::HashMap<PathBuf, RemovalIdentity>,
    encoded_bytes: usize,
}

#[cfg(test)]
pub(crate) fn remove_directory_tree_capability_bound(
    parent: &Path,
    directory: &Path,
    deadline: std::time::Instant,
) -> io::Result<()> {
    remove_directory_tree_capability_bound_inner(parent, directory, None, deadline)
}

pub(crate) fn capture_directory_removal_receipt(
    parent: &Path,
    directory: &Path,
) -> io::Result<DirectoryRemovalReceipt> {
    verify_directory_without_symlinks(parent)?;
    #[cfg(windows)]
    direct_child_path(parent, directory)?;
    #[cfg(not(windows))]
    let relative = direct_child_path(parent, directory)?;
    #[cfg(windows)]
    let parent_identity = {
        let parent_file = open_directory_observation_windows(parent)?;
        removal_identity_from_file(&parent_file)?
    };
    #[cfg(not(windows))]
    let parent_directory =
        cap_std::fs::Dir::open_ambient_dir(parent, cap_std::ambient_authority())?;
    #[cfg(windows)]
    let source = open_directory_observation_windows(directory)?;
    #[cfg(not(windows))]
    let source = open_capability_entry_no_follow(&parent_directory, &relative)?;
    let metadata = source.metadata()?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(invalid_directory_component(directory));
    }
    #[cfg(windows)]
    {
        let source_identity = removal_identity_from_file(&source)?;
        verify_directory_without_symlinks(parent)?;
        let visible_parent_file = open_directory_observation_windows(parent)?;
        if removal_identity_from_file(&visible_parent_file)? != parent_identity {
            return Err(io::Error::other(format!(
                "directory receipt parent identity changed: {}",
                parent.display()
            )));
        }
        let visible_source = open_directory_observation_windows(directory)?;
        if removal_identity_from_file(&visible_source)? != source_identity {
            return Err(io::Error::other(format!(
                "directory receipt identity changed while it was captured: {}",
                directory.display()
            )));
        }
    }
    DirectoryRemovalReceipt::from_open_directory(source)
}

pub(crate) fn remove_directory_tree_capability_bound_if_matches(
    parent: &Path,
    directory: &Path,
    expected: DirectoryRemovalReceipt,
    deadline: std::time::Instant,
) -> io::Result<()> {
    remove_directory_tree_capability_bound_inner(parent, directory, Some(expected), deadline)
}

pub(crate) fn directory_removal_quarantine_exists(
    parent: &Path,
    directory: &Path,
) -> io::Result<bool> {
    verify_directory_without_symlinks(parent)?;
    let relative = direct_child_path(parent, directory)?;
    let quarantine = removal_quarantine_name(&relative);
    let parent_directory =
        cap_std::fs::Dir::open_ambient_dir(parent, cap_std::ambient_authority())?;
    capability_metadata_if_present(&parent_directory, &quarantine).map(|entry| entry.is_some())
}

fn remove_directory_tree_capability_bound_inner(
    parent: &Path,
    directory: &Path,
    expected: Option<DirectoryRemovalReceipt>,
    deadline: std::time::Instant,
) -> io::Result<()> {
    ensure_removal_time(deadline)?;
    verify_directory_without_symlinks(parent)?;
    ensure_removal_time(deadline)?;
    let relative = direct_child_path(parent, directory)?;
    let quarantine = removal_quarantine_name(&relative);

    let parent_directory =
        cap_std::fs::Dir::open_ambient_dir(parent, cap_std::ambient_authority())?;
    let parent_file = parent_directory.try_clone()?.into_std_file();
    let parent_identity = removal_identity_from_file(&parent_file)?;

    let source_metadata = capability_metadata_if_present(&parent_directory, &relative)?;
    let quarantine_metadata = capability_metadata_if_present(&parent_directory, &quarantine)?;
    match (&source_metadata, &quarantine_metadata) {
        (None, None) => {
            if expected.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "directory covered by an ownership receipt is missing; preserving the namespace as ambiguous: {}",
                        directory.display()
                    ),
                ));
            }
            ensure_removal_time(deadline)?;
            verify_parent_identity(parent, &parent_identity)?;
            return Ok(());
        }
        (None, Some(_)) => {
            return Err(io::Error::other(format!(
                "unproven cleanup quarantine exists and was preserved; persisted identity evidence is required for recovery: {}",
                parent.join(&quarantine).display()
            )));
        }
        (Some(_), Some(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "cleanup source and deterministic quarantine both exist; preserving both as ambiguous: {} and {}",
                    directory.display(),
                    parent.join(&quarantine).display()
                ),
            ));
        }
        (Some(metadata), None) => {
            if metadata.is_symlink()
                || metadata_is_capability_reparse(metadata)
                || !metadata.is_dir()
            {
                return Err(invalid_directory_component(directory));
            }
        }
    }

    let source_file = open_capability_entry_no_follow(&parent_directory, &relative)?;
    let source_metadata = source_file.metadata()?;
    if metadata_is_link_or_reparse(&source_metadata) || !source_metadata.is_dir() {
        return Err(invalid_directory_component(directory));
    }
    let source_identity = removal_identity_from_file(&source_file)?;
    if let Some(receipt) = expected.as_ref() {
        if removal_identity_from_file(&receipt.file)? != receipt.identity {
            return Err(io::Error::other(
                "directory ownership receipt identity changed while it was retained",
            ));
        }
        if receipt.identity != source_identity {
            return Err(io::Error::other(format!(
                "directory identity no longer matches its ownership receipt; replacement preserved: {}",
                directory.display()
            )));
        }
    }
    ensure_same_removal_filesystem(parent_identity, source_identity, directory)?;
    let source_directory = cap_std::fs::Dir::from_std_file(source_file.try_clone()?);
    let before_quarantine = build_removal_plan(&source_directory, source_identity, deadline)?;

    ensure_removal_time(deadline)?;
    quarantine_open_entry_no_replace(&parent_directory, &relative, &source_file, &quarantine)?;
    let quarantined = open_capability_entry_no_follow(&parent_directory, &quarantine)?;
    if removal_identity_from_file(&quarantined)? != source_identity {
        let restore = quarantine_open_entry_no_replace(
            &parent_directory,
            &quarantine,
            &quarantined,
            &relative,
        );
        return Err(match restore {
            Ok(()) => io::Error::other(format!(
                "directory identity changed during quarantine; the replacement was restored: {}",
                directory.display()
            )),
            Err(restore) => io::Error::other(format!(
                "directory identity changed during quarantine; the replacement was preserved in {} because its visible name could not be restored: {restore}",
                parent.join(&quarantine).display()
            )),
        });
    }
    drop(quarantined);

    inject_removal_replacement(directory)?;
    ensure_entry_absent(&parent_directory, &relative, directory).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "directory path was replaced after quarantine; replacement preserved: {}",
                    directory.display()
                ),
            )
        } else {
            error
        }
    })?;

    let after_quarantine = build_removal_plan(&source_directory, source_identity, deadline)?;
    if before_quarantine != after_quarantine {
        return Err(io::Error::other(format!(
            "directory tree changed while it was quarantined: {}",
            directory.display()
        )));
    }
    remove_validated_plan(&source_directory, &after_quarantine, deadline)?;
    ensure_removal_time(deadline)?;
    if source_directory.entries()?.next().is_some() {
        return Err(io::Error::other(format!(
            "quarantined directory is not empty after bounded traversal: {}",
            directory.display()
        )));
    }
    drop(source_directory);
    ensure_same_removal_filesystem(parent_identity, source_identity, directory)?;
    delete_open_entry_platform(
        &parent_directory,
        &quarantine,
        directory,
        source_file,
        RemovalEntryKind::Directory,
        deadline,
    )?;

    ensure_removal_time(deadline)?;
    verify_parent_identity(parent, &parent_identity)?;
    ensure_entry_absent(&parent_directory, &quarantine, directory)?;
    ensure_entry_absent(&parent_directory, &relative, directory).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "directory path reappeared during recursive removal; replacement preserved: {}",
                    directory.display()
                ),
            )
        } else {
            error
        }
    })?;
    Ok(())
}

fn capability_metadata_if_present(
    parent: &cap_std::fs::Dir,
    relative: &Path,
) -> io::Result<Option<cap_std::fs::Metadata>> {
    match parent.symlink_metadata(relative) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn metadata_is_capability_reparse(metadata: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_capability_reparse(_metadata: &cap_std::fs::Metadata) -> bool {
    false
}

fn removal_quarantine_name(relative: &Path) -> PathBuf {
    hashed_removal_name(b"nib-cleanup-quarantine-v1\0", ".nib-cleanup-", relative)
}

#[cfg(unix)]
fn removal_entry_quarantine_name(relative: &Path) -> PathBuf {
    hashed_removal_name(
        b"nib-cleanup-entry-quarantine-v1\0",
        ".nib-entry-cleanup-",
        relative,
    )
}

fn hashed_removal_name(domain: &[u8], prefix: &str, relative: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let mut hasher = Sha256::new();
    hasher.update(domain);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(relative.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in relative.as_os_str().encode_wide() {
            hasher.update(unit.to_le_bytes());
        }
    }
    let mut digest = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(&mut digest, "{byte:02x}").expect("writing to a String cannot fail");
    }
    PathBuf::from(format!("{prefix}{digest}"))
}

fn direct_child_path(parent: &Path, directory: &Path) -> io::Result<PathBuf> {
    let relative = directory.strip_prefix(parent).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "directory is not beneath its retained parent: {}",
                directory.display()
            ),
        )
    })?;
    let mut components = relative.components();
    let Some(Component::Normal(name)) = components.next() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "directory is not a direct child of its retained parent: {}",
                directory.display()
            ),
        ));
    };
    if components.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "directory is not a direct child of its retained parent: {}",
                directory.display()
            ),
        ));
    }
    Ok(PathBuf::from(name))
}

fn open_capability_entry_no_follow(
    parent: &cap_std::fs::Dir,
    relative: &Path,
) -> io::Result<std::fs::File> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        };
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options
            .access_mode(DELETE | FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = parent.open_with(relative, &options)?.into_std();
    let metadata = file.metadata()?;
    if metadata_is_link_or_reparse(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "recursive removal refuses a symlink or reparse point: {}",
                relative.display()
            ),
        ));
    }
    Ok(file)
}

#[cfg(windows)]
pub(crate) fn open_directory_observation_windows(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(invalid_directory_component(path));
    }
    Ok(file)
}

fn build_removal_plan(
    root: &cap_std::fs::Dir,
    root_identity: RemovalIdentity,
    deadline: std::time::Instant,
) -> io::Result<RemovalPlan> {
    ensure_removal_time(deadline)?;
    let mut plan = RemovalPlan {
        entries: Vec::new(),
        directories: std::collections::HashMap::from([(PathBuf::new(), root_identity)]),
        encoded_bytes: 0,
    };
    scan_removal_directory(root, Path::new(""), 0, root_identity, deadline, &mut plan)?;
    plan.entries.sort_unstable_by(|left, right| {
        left.relative
            .components()
            .count()
            .cmp(&right.relative.components().count())
            .then_with(|| left.relative.cmp(&right.relative))
    });
    ensure_removal_time(deadline)?;
    Ok(plan)
}

fn scan_removal_directory(
    directory: &cap_std::fs::Dir,
    relative: &Path,
    depth: usize,
    root_identity: RemovalIdentity,
    deadline: std::time::Instant,
    plan: &mut RemovalPlan,
) -> io::Result<()> {
    ensure_removal_time(deadline)?;
    if depth > MAX_REMOVAL_DEPTH {
        return Err(removal_bound_error("directory depth", MAX_REMOVAL_DEPTH));
    }
    for entry in directory.entries()? {
        ensure_removal_time(deadline)?;
        if plan.entries.len() >= MAX_REMOVAL_ENTRIES {
            return Err(removal_bound_error("entry count", MAX_REMOVAL_ENTRIES));
        }
        let name = entry?.file_name();
        validate_removal_name(&name)?;
        #[cfg(unix)]
        if removal_name_is_reserved_entry_quarantine(&name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "unproven per-entry cleanup quarantine exists; preserving it as ambiguous: {}",
                    relative.join(&name).display()
                ),
            ));
        }
        let child_relative = relative.join(&name);
        let retained_bytes = encoded_name_units(child_relative.as_os_str())
            .checked_add(std::mem::size_of::<RemovalEntry>())
            .ok_or_else(|| removal_bound_error("plan bytes", MAX_REMOVAL_PLAN_BYTES))?;
        plan.encoded_bytes = plan
            .encoded_bytes
            .checked_add(retained_bytes)
            .ok_or_else(|| removal_bound_error("plan bytes", MAX_REMOVAL_PLAN_BYTES))?;
        ensure_removal_plan_budget(plan.encoded_bytes, 0)?;
        let metadata = directory.symlink_metadata(&name)?;
        #[cfg(unix)]
        if metadata.is_symlink() {
            plan.entries.push(RemovalEntry {
                relative: child_relative,
                identity: removal_identity_from_capability_metadata(&metadata),
                kind: RemovalEntryKind::Symlink,
            });
            continue;
        }
        #[cfg(windows)]
        if metadata.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "recursive removal refuses a nested Windows reparse point: {}",
                    child_relative.display()
                ),
            ));
        }

        let opened = open_capability_entry_no_follow(directory, Path::new(&name))?;
        let opened_metadata = opened.metadata()?;
        let identity = removal_identity_from_file(&opened)?;
        let kind = if opened_metadata.is_dir() {
            RemovalEntryKind::Directory
        } else if opened_metadata.is_file() {
            RemovalEntryKind::File
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "recursive removal refuses a special filesystem entry: {}",
                    child_relative.display()
                ),
            ));
        };
        plan.entries.push(RemovalEntry {
            relative: child_relative.clone(),
            identity,
            kind,
        });
        if kind == RemovalEntryKind::Directory {
            ensure_same_removal_filesystem(root_identity, identity, &child_relative)?;
            plan.encoded_bytes = plan
                .encoded_bytes
                .checked_add(encoded_name_units(child_relative.as_os_str()))
                .ok_or_else(|| removal_bound_error("plan bytes", MAX_REMOVAL_PLAN_BYTES))?;
            plan.encoded_bytes = plan
                .encoded_bytes
                .checked_add(std::mem::size_of::<(PathBuf, RemovalIdentity)>())
                .ok_or_else(|| removal_bound_error("plan bytes", MAX_REMOVAL_PLAN_BYTES))?;
            ensure_removal_plan_budget(plan.encoded_bytes, 0)?;
            plan.directories.insert(child_relative.clone(), identity);
            let child = cap_std::fs::Dir::from_std_file(opened);
            scan_removal_directory(
                &child,
                &child_relative,
                depth + 1,
                root_identity,
                deadline,
                plan,
            )?;
        }
    }
    Ok(())
}

fn remove_validated_plan(
    root: &cap_std::fs::Dir,
    plan: &RemovalPlan,
    deadline: std::time::Instant,
) -> io::Result<()> {
    let root_identity = *plan
        .directories
        .get(Path::new(""))
        .ok_or_else(|| removal_identity_error(Path::new("")))?;
    for entry in plan.entries.iter().rev() {
        ensure_removal_time(deadline)?;
        let parent_relative = entry.relative.parent().unwrap_or_else(|| Path::new(""));
        let parent = open_validated_plan_directory(root, parent_relative, plan, deadline)?;
        let name = entry.relative.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "removal entry has no file name",
            )
        })?;
        #[cfg(unix)]
        if entry.kind == RemovalEntryKind::Symlink {
            let metadata = parent.symlink_metadata(name)?;
            if !metadata.is_symlink()
                || removal_identity_from_capability_metadata(&metadata) != entry.identity
            {
                return Err(removal_identity_error(&entry.relative));
            }
            delete_validated_symlink_unix(
                &parent,
                Path::new(name),
                &entry.relative,
                entry.identity,
                deadline,
            )?;
            ensure_entry_absent(&parent, Path::new(name), &entry.relative)?;
            continue;
        }

        let opened = open_capability_entry_no_follow(&parent, Path::new(name))?;
        let metadata = opened.metadata()?;
        let kind_matches = match entry.kind {
            RemovalEntryKind::File => metadata.is_file(),
            RemovalEntryKind::Directory => metadata.is_dir(),
            #[cfg(unix)]
            RemovalEntryKind::Symlink => false,
        };
        if !kind_matches || removal_identity_from_file(&opened)? != entry.identity {
            return Err(removal_identity_error(&entry.relative));
        }
        if entry.kind == RemovalEntryKind::Directory {
            ensure_same_removal_filesystem(root_identity, entry.identity, &entry.relative)?;
        }
        if entry.kind == RemovalEntryKind::Directory
            && cap_std::fs::Dir::from_std_file(opened.try_clone()?)
                .entries()?
                .next()
                .is_some()
        {
            return Err(io::Error::other(format!(
                "directory changed before bounded removal: {}",
                entry.relative.display()
            )));
        }
        delete_open_entry_platform(
            &parent,
            Path::new(name),
            &entry.relative,
            opened,
            entry.kind,
            deadline,
        )?;
        ensure_entry_absent(&parent, Path::new(name), &entry.relative)?;
    }
    Ok(())
}

fn ensure_removal_time(deadline: std::time::Instant) -> io::Result<()> {
    if std::time::Instant::now() < deadline {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "bounded recursive removal exceeded its absolute deadline",
        ))
    }
}

#[cfg(unix)]
fn ensure_same_removal_filesystem(
    root: RemovalIdentity,
    child: RemovalIdentity,
    path: &Path,
) -> io::Result<()> {
    if root.device == child.device {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "recursive removal refuses a nested mount or filesystem crossing: {}",
                path.display()
            ),
        ))
    }
}

#[cfg(windows)]
fn ensure_same_removal_filesystem(
    _root: RemovalIdentity,
    _child: RemovalIdentity,
    _path: &Path,
) -> io::Result<()> {
    Ok(())
}

fn open_validated_plan_directory(
    root: &cap_std::fs::Dir,
    relative: &Path,
    plan: &RemovalPlan,
    deadline: std::time::Instant,
) -> io::Result<cap_std::fs::Dir> {
    let mut directory = root.try_clone()?;
    let mut traversed = PathBuf::new();
    for component in relative.components() {
        ensure_removal_time(deadline)?;
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "removal plan contains a non-local directory component",
            ));
        };
        traversed.push(name);
        let expected = plan
            .directories
            .get(&traversed)
            .ok_or_else(|| removal_identity_error(&traversed))?;
        let opened = open_capability_entry_no_follow(&directory, Path::new(name))?;
        if !opened.metadata()?.is_dir() || removal_identity_from_file(&opened)? != *expected {
            return Err(removal_identity_error(&traversed));
        }
        directory = cap_std::fs::Dir::from_std_file(opened);
    }
    Ok(directory)
}

fn validate_removal_name(name: &std::ffi::OsStr) -> io::Result<()> {
    let units = encoded_name_units(name);
    if units == 0 || units > MAX_REMOVAL_NAME_UNITS {
        return Err(removal_bound_error(
            "entry name units",
            MAX_REMOVAL_NAME_UNITS,
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn removal_name_is_reserved_entry_quarantine(name: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;

    name.as_bytes().starts_with(b".nib-entry-cleanup-")
}

#[cfg(unix)]
fn encoded_name_units(name: &std::ffi::OsStr) -> usize {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes().len()
}

#[cfg(windows)]
fn encoded_name_units(name: &std::ffi::OsStr) -> usize {
    use std::os::windows::ffi::OsStrExt;
    name.encode_wide().count()
}

fn removal_bound_error(kind: &str, limit: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("recursive removal exceeded the {kind} limit of {limit}"),
    )
}

fn ensure_removal_plan_budget(retained_bytes: usize, pending_bytes: usize) -> io::Result<()> {
    match retained_bytes.checked_add(pending_bytes) {
        Some(bytes) if bytes <= MAX_REMOVAL_PLAN_BYTES => Ok(()),
        _ => Err(removal_bound_error("plan bytes", MAX_REMOVAL_PLAN_BYTES)),
    }
}

fn removal_identity_error(path: &Path) -> io::Error {
    io::Error::other(format!(
        "filesystem entry identity changed during bounded removal: {}",
        path.display()
    ))
}

#[cfg(unix)]
fn removal_identity_from_file(file: &std::fs::File) -> io::Result<RemovalIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(RemovalIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(unix)]
fn removal_identity_from_capability_metadata(metadata: &cap_std::fs::Metadata) -> RemovalIdentity {
    use cap_std::fs::MetadataExt;
    RemovalIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(windows)]
fn removal_identity_from_file(file: &std::fs::File) -> io::Result<RemovalIdentity> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let mut identity = FILE_ID_INFO::default();
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            std::ptr::from_mut(&mut identity).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(RemovalIdentity {
        volume_serial_number: identity.VolumeSerialNumber,
        file_id: identity.FileId.Identifier,
    })
}

fn verify_parent_identity(parent: &Path, expected: &RemovalIdentity) -> io::Result<()> {
    verify_directory_without_symlinks(parent)?;
    let visible = cap_std::fs::Dir::open_ambient_dir(parent, cap_std::ambient_authority())?;
    let identity = removal_identity_from_file(&visible.into_std_file())?;
    if identity != *expected {
        return Err(io::Error::other(format!(
            "parent directory identity changed during recursive removal: {}",
            parent.display()
        )));
    }
    Ok(())
}

fn ensure_entry_absent(
    parent: &cap_std::fs::Dir,
    relative: &Path,
    display: &Path,
) -> io::Result<()> {
    match parent.symlink_metadata(relative) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "filesystem entry reappeared during bounded removal; replacement preserved: {}",
                display.display()
            ),
        )),
        Err(error) => Err(error),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn quarantine_open_entry_no_replace(
    parent: &cap_std::fs::Dir,
    source: &Path,
    _source_file: &std::fs::File,
    destination: &Path,
) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source contains a NUL byte"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination contains a NUL byte",
        )
    })?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn quarantine_open_entry_no_replace(
    parent: &cap_std::fs::Dir,
    source: &Path,
    _source_file: &std::fs::File,
    destination: &Path,
) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source contains a NUL byte"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination contains a NUL byte",
        )
    })?;
    let result = unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn quarantine_open_entry_no_replace(
    parent: &cap_std::fs::Dir,
    _source: &Path,
    source_file: &std::fs::File,
    destination: &Path,
) -> io::Result<()> {
    rename_open_entry_no_replace_windows(parent, source_file, destination)
}

#[cfg(all(
    not(windows),
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn quarantine_open_entry_no_replace(
    _parent: &cap_std::fs::Dir,
    _source: &Path,
    _source_file: &std::fs::File,
    _destination: &Path,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform has no no-replace directory quarantine primitive",
    ))
}

#[cfg(windows)]
fn delete_open_entry_platform(
    _parent: &cap_std::fs::Dir,
    _relative: &Path,
    _display: &Path,
    opened: std::fs::File,
    _kind: RemovalEntryKind,
    deadline: std::time::Instant,
) -> io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfoEx, SetFileInformationByHandle, FILE_DISPOSITION_FLAG_DELETE,
        FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX,
    };

    ensure_removal_time(deadline)?;
    let extended = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    let deleted = unsafe {
        SetFileInformationByHandle(
            opened.as_raw_handle(),
            FileDispositionInfoEx,
            std::ptr::from_ref(&extended).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if deleted != 0 {
        drop(opened);
        return Ok(());
    }
    let error = io::Error::last_os_error();
    Err(io::Error::new(
        error.kind(),
        format!(
            "exact handle-bound deletion requires FileDispositionInfoEx POSIX semantics; refusing deferred legacy deletion: {error}"
        ),
    ))
}

#[cfg(unix)]
fn delete_open_entry_platform(
    parent: &cap_std::fs::Dir,
    relative: &Path,
    display: &Path,
    opened: std::fs::File,
    kind: RemovalEntryKind,
    deadline: std::time::Instant,
) -> io::Result<()> {
    if kind == RemovalEntryKind::Directory {
        ensure_removal_time(deadline)?;
        inject_removal_directory_replacement(parent, display, relative)?;
        cap_std::fs::Dir::from_std_file(opened).remove_open_dir()?;
        return ensure_entry_absent(parent, relative, display);
    }

    // Unix has no portable conditional unlink-by-file-ID operation. Regular files
    // are moved no-replace into a reserved name and checked again immediately
    // before relative unlink. A same-UID writer can still race after that final
    // check, so callers must treat any namespace ambiguity as cleanup-unverified.
    let expected = removal_identity_from_file(&opened)?;
    let quarantine = removal_entry_quarantine_name(display);
    if capability_metadata_if_present(parent, &quarantine)?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "unproven per-entry cleanup quarantine exists; preserving it as ambiguous: {}",
                quarantine.display()
            ),
        ));
    }
    ensure_removal_time(deadline)?;
    quarantine_open_entry_no_replace(parent, relative, &opened, &quarantine)?;
    let quarantined = open_capability_entry_no_follow(parent, &quarantine)?;
    if removal_identity_from_file(&quarantined)? != expected {
        return Err(removal_identity_error(display));
    }
    drop(quarantined);
    inject_removal_entry_replacement(parent, display, relative)?;
    ensure_entry_absent(parent, relative, display)?;
    ensure_removal_time(deadline)?;
    inject_removal_quarantine_replacement(parent, display, &quarantine)?;
    let final_open = open_capability_entry_no_follow(parent, &quarantine)?;
    if removal_identity_from_file(&final_open)? != expected {
        return Err(removal_identity_error(display));
    }
    drop(final_open);
    match kind {
        RemovalEntryKind::Directory => unreachable!("directories return before path quarantine"),
        RemovalEntryKind::File | RemovalEntryKind::Symlink => {
            parent.remove_file(&quarantine)?;
            drop(opened);
            ensure_entry_absent(parent, &quarantine, display)
        }
    }
}

#[cfg(unix)]
fn delete_validated_symlink_unix(
    parent: &cap_std::fs::Dir,
    relative: &Path,
    display: &Path,
    expected: RemovalIdentity,
    deadline: std::time::Instant,
) -> io::Result<()> {
    let quarantine = removal_entry_quarantine_name(display);
    if capability_metadata_if_present(parent, &quarantine)?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "unproven per-entry cleanup quarantine exists; preserving it as ambiguous: {}",
                quarantine.display()
            ),
        ));
    }
    let parent_file = parent.try_clone()?.into_std_file();
    ensure_removal_time(deadline)?;
    quarantine_open_entry_no_replace(parent, relative, &parent_file, &quarantine)?;
    let metadata = parent.symlink_metadata(&quarantine)?;
    if !metadata.is_symlink() || removal_identity_from_capability_metadata(&metadata) != expected {
        return Err(removal_identity_error(display));
    }
    inject_removal_entry_replacement(parent, display, relative)?;
    ensure_entry_absent(parent, relative, display)?;
    ensure_removal_time(deadline)?;
    inject_removal_quarantine_replacement(parent, display, &quarantine)?;
    let metadata = parent.symlink_metadata(&quarantine)?;
    if !metadata.is_symlink() || removal_identity_from_capability_metadata(&metadata) != expected {
        return Err(removal_identity_error(display));
    }
    parent.remove_file(&quarantine)?;
    ensure_entry_absent(parent, &quarantine, display)
}

#[cfg(test)]
fn inject_removal_replacement(directory: &Path) -> io::Result<()> {
    let replacement = REPLACE_AFTER_REMOVAL_QUARANTINE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(directory);
    if let Some(replacement) = replacement {
        std::fs::rename(replacement, directory)?;
    }
    Ok(())
}

#[cfg(not(test))]
fn inject_removal_replacement(_directory: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
fn inject_removal_entry_replacement(
    parent: &cap_std::fs::Dir,
    display: &Path,
    relative: &Path,
) -> io::Result<()> {
    let replacement = REPLACE_AFTER_REMOVAL_ENTRY_QUARANTINE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(display);
    if let Some(replacement) = replacement {
        parent.rename(replacement, parent, relative)?;
    }
    Ok(())
}

#[cfg(all(unix, not(test)))]
fn inject_removal_entry_replacement(
    _parent: &cap_std::fs::Dir,
    _display: &Path,
    _relative: &Path,
) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
fn inject_removal_quarantine_replacement(
    parent: &cap_std::fs::Dir,
    display: &Path,
    quarantine: &Path,
) -> io::Result<()> {
    let replacement = REPLACE_REMOVAL_QUARANTINE_BEFORE_UNLINK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(display);
    if let Some(replacement) = replacement {
        let displaced = hashed_removal_name(
            b"nib-test-displaced-entry-v1\0",
            ".nib-test-displaced-entry-",
            display,
        );
        if capability_metadata_if_present(parent, &displaced)?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "test displaced-entry destination already exists",
            ));
        }
        parent.rename(quarantine, parent, &displaced)?;
        parent.rename(replacement, parent, quarantine)?;
    }
    Ok(())
}

#[cfg(all(unix, not(test)))]
fn inject_removal_quarantine_replacement(
    _parent: &cap_std::fs::Dir,
    _display: &Path,
    _quarantine: &Path,
) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
fn inject_removal_directory_replacement(
    parent: &cap_std::fs::Dir,
    display: &Path,
    relative: &Path,
) -> io::Result<()> {
    let replacement = REPLACE_DIRECTORY_BEFORE_HANDLE_DELETE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(display);
    if let Some(replacement) = replacement {
        let displaced = hashed_removal_name(
            b"nib-test-displaced-directory-v1\0",
            ".nib-test-displaced-directory-",
            display,
        );
        if capability_metadata_if_present(parent, &displaced)?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "test displaced-directory destination already exists",
            ));
        }
        parent.rename(relative, parent, &displaced)?;
        parent.rename(replacement, parent, relative)?;
    }
    Ok(())
}

#[cfg(all(unix, not(test)))]
fn inject_removal_directory_replacement(
    _parent: &cap_std::fs::Dir,
    _display: &Path,
    _relative: &Path,
) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    fn removal_deadline() -> std::time::Instant {
        std::time::Instant::now() + std::time::Duration::from_secs(5)
    }

    #[test]
    fn file_identity_matches_reopened_file_and_hard_link() {
        let root = tempdir().expect("tempdir");
        let original = root.path().join("original");
        let linked = root.path().join("linked");
        std::fs::write(&original, b"identity").expect("write original");
        std::fs::hard_link(&original, &linked).expect("create hard link");

        let original_identity =
            FileIdentity::from_file(File::open(&original).expect("open original"))
                .expect("identify original");
        let reopened_identity =
            FileIdentity::from_file(File::open(&original).expect("reopen original"))
                .expect("identify reopened original");
        let linked_identity = FileIdentity::from_file(File::open(&linked).expect("open hard link"))
            .expect("identify hard link");

        assert_eq!(original_identity, reopened_identity);
        assert_eq!(original_identity, linked_identity);
    }

    #[test]
    fn file_identity_distinguishes_same_sized_files() {
        let root = tempdir().expect("tempdir");
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::write(&first, b"same-size-a").expect("write first");
        std::fs::write(&second, b"same-size-b").expect("write second");

        let first_identity = FileIdentity::from_file(File::open(first).expect("open first"))
            .expect("identify first");
        let second_identity = FileIdentity::from_file(File::open(second).expect("open second"))
            .expect("identify second");

        assert_ne!(first_identity, second_identity);
    }

    #[test]
    fn creates_local_components_and_rejects_parent_traversal() {
        let root = tempdir().expect("tempdir");
        let path = root.path().join("one/two");
        assert_eq!(
            ensure_directory_without_symlinks(&path).unwrap(),
            path.canonicalize().unwrap()
        );
        assert!(path.is_dir());
        assert!(ensure_directory_without_symlinks(&root.path().join("one/../escape")).is_err());
    }

    #[test]
    fn capability_bound_removal_deletes_only_the_opened_child_tree() {
        let root = tempdir().expect("tempdir");
        let child = root.path().join("child");
        std::fs::create_dir(&child).expect("child directory");
        std::fs::write(child.join("nested"), b"fixture").expect("nested file");

        remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
            .expect("capability-bound removal");

        assert!(!child.exists());
        assert!(root.path().is_dir());
    }

    #[test]
    fn receipt_bound_removal_preserves_a_visible_replacement() {
        let root = tempdir().expect("tempdir");
        let child = root.path().join("child");
        let displaced = root.path().join("displaced-owned-child");
        std::fs::create_dir(&child).expect("owned child directory");
        std::fs::write(child.join("owned"), b"owned").expect("owned file");
        let receipt = capture_directory_removal_receipt(root.path(), &child)
            .expect("directory ownership receipt");
        std::fs::rename(&child, &displaced).expect("displace owned directory");
        std::fs::create_dir(&child).expect("replacement directory");
        std::fs::write(child.join("sentinel"), b"replacement").expect("replacement sentinel");

        let error = remove_directory_tree_capability_bound_if_matches(
            root.path(),
            &child,
            receipt,
            removal_deadline(),
        )
        .expect_err("receipt mismatch must preserve the replacement");

        assert!(error.to_string().contains("ownership receipt"), "{error}");
        assert_eq!(
            std::fs::read(child.join("sentinel")).expect("preserved replacement"),
            b"replacement"
        );
        assert_eq!(
            std::fs::read(displaced.join("owned")).expect("preserved owned directory"),
            b"owned"
        );
    }

    #[test]
    fn capability_bound_removal_preserves_a_top_level_replacement() {
        let root = tempdir().expect("tempdir");
        let child = root.path().join("child");
        let replacement = root.path().join("replacement");
        std::fs::create_dir(&child).expect("child directory");
        std::fs::write(child.join("owned"), b"owned").expect("owned file");
        std::fs::create_dir(&replacement).expect("replacement directory");
        std::fs::write(replacement.join("replacement"), b"replacement").expect("replacement file");
        REPLACE_AFTER_REMOVAL_QUARANTINE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(child.clone(), replacement);

        let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
            .expect_err("replacement must make removal fail closed");

        assert!(
            error.to_string().contains("replaced after quarantine"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read(child.join("replacement")).expect("preserved replacement"),
            b"replacement"
        );
        let quarantines = std::fs::read_dir(root.path())
            .expect("read root")
            .map(|entry| entry.expect("root entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|name| name.starts_with(".nib-cleanup-"))
            })
            .collect::<Vec<_>>();
        assert_eq!(quarantines.len(), 1, "owned tree remains quarantined");
        assert_eq!(
            std::fs::read(quarantines[0].join("owned")).expect("quarantined owned file"),
            b"owned"
        );

        let retry = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
            .expect_err("ambiguous source and quarantine must remain preserved");
        assert!(retry.to_string().contains("preserving both"), "{retry}");
        let quarantine_count = std::fs::read_dir(root.path())
            .expect("read root after retry")
            .map(|entry| entry.expect("root entry after retry").file_name())
            .filter(|name| {
                name.to_str()
                    .is_some_and(|name| name.starts_with(".nib-cleanup-"))
            })
            .count();
        assert_eq!(quarantine_count, 1, "retry created another quarantine");
    }

    #[test]
    fn capability_bound_removal_preserves_an_unproven_quarantine_only_state() {
        let root = tempdir().expect("tempdir");
        let child = root.path().join("child");
        let quarantine = root
            .path()
            .join(removal_quarantine_name(Path::new("child")));
        std::fs::create_dir(&quarantine).expect("forged quarantine");
        std::fs::write(quarantine.join("sentinel"), b"unproven").expect("sentinel");

        let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
            .expect_err("quarantine-only state must be unproven");

        assert!(error.to_string().contains("unproven"), "{error}");
        assert_eq!(
            std::fs::read(quarantine.join("sentinel")).expect("preserved sentinel"),
            b"unproven"
        );
        assert!(!child.exists());
    }

    #[test]
    fn capability_bound_removal_honors_an_expired_deadline_without_mutation() {
        let root = tempdir().expect("tempdir");
        let child = root.path().join("child");
        std::fs::create_dir(&child).expect("child directory");
        std::fs::write(child.join("sentinel"), b"preserved").expect("sentinel");

        let error =
            remove_directory_tree_capability_bound(root.path(), &child, std::time::Instant::now())
                .expect_err("expired deadline must fail");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            std::fs::read(child.join("sentinel")).expect("preserved sentinel"),
            b"preserved"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_entry_replacement_after_quarantine_is_preserved() {
        let root = tempdir().expect("tempdir");
        let child = root.path().join("child");
        let victim = PathBuf::from("z-victim");
        let replacement = PathBuf::from("a-replacement");
        std::fs::create_dir(&child).expect("child directory");
        std::fs::write(child.join(&victim), b"owned").expect("owned file");
        std::fs::write(child.join(&replacement), b"replacement").expect("replacement file");
        REPLACE_AFTER_REMOVAL_ENTRY_QUARANTINE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(victim.clone(), replacement);

        let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
            .expect_err("entry replacement must fail closed");

        assert!(
            error.to_string().contains("replacement preserved"),
            "{error}"
        );
        let top_quarantine = root
            .path()
            .join(removal_quarantine_name(Path::new("child")));
        assert_eq!(
            std::fs::read(top_quarantine.join(&victim)).expect("preserved replacement"),
            b"replacement"
        );
        let entry_quarantines = std::fs::read_dir(&top_quarantine)
            .expect("read top quarantine")
            .map(|entry| entry.expect("top quarantine entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|name| name.starts_with(".nib-entry-cleanup-"))
            })
            .collect::<Vec<_>>();
        assert_eq!(entry_quarantines.len(), 1);
        assert_eq!(
            std::fs::read(&entry_quarantines[0]).expect("preserved owned entry"),
            b"owned"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_quarantine_replacement_before_unlink_is_preserved() {
        let root = tempdir().expect("tempdir");
        let child = root.path().join("child");
        let victim = PathBuf::from("z-final-victim");
        let replacement = PathBuf::from("a-final-replacement");
        std::fs::create_dir(&child).expect("child directory");
        std::fs::write(child.join(&victim), b"owned").expect("owned file");
        std::fs::write(child.join(&replacement), b"replacement").expect("replacement file");
        REPLACE_REMOVAL_QUARANTINE_BEFORE_UNLINK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(victim.clone(), replacement);

        let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
            .expect_err("quarantine substitution must fail closed");

        assert!(error.to_string().contains("identity changed"), "{error}");
        let top_quarantine = root
            .path()
            .join(removal_quarantine_name(Path::new("child")));
        let entry_quarantine = top_quarantine.join(removal_entry_quarantine_name(&victim));
        assert_eq!(
            std::fs::read(entry_quarantine).expect("preserved quarantine replacement"),
            b"replacement"
        );
        let displaced = std::fs::read_dir(&top_quarantine)
            .expect("read top quarantine")
            .map(|entry| entry.expect("top quarantine entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|name| name.starts_with(".nib-test-displaced-entry-"))
            })
            .expect("displaced opened entry");
        assert_eq!(
            std::fs::read(displaced).expect("preserved displaced entry"),
            b"owned"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_symlink_quarantine_replacement_before_unlink_is_preserved() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("tempdir");
        let child = root.path().join("child");
        let victim = PathBuf::from("z-final-link");
        let replacement = PathBuf::from("a-link-replacement");
        std::fs::create_dir(&child).expect("child directory");
        symlink("target", child.join(&victim)).expect("owned symlink");
        std::fs::write(child.join(&replacement), b"replacement").expect("replacement file");
        REPLACE_REMOVAL_QUARANTINE_BEFORE_UNLINK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(victim.clone(), replacement);

        let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
            .expect_err("symlink quarantine substitution must fail closed");

        assert!(error.to_string().contains("identity changed"), "{error}");
        let top_quarantine = root
            .path()
            .join(removal_quarantine_name(Path::new("child")));
        let entry_quarantine = top_quarantine.join(removal_entry_quarantine_name(&victim));
        assert_eq!(
            std::fs::read(entry_quarantine).expect("preserved quarantine replacement"),
            b"replacement"
        );
        let displaced = std::fs::read_dir(&top_quarantine)
            .expect("read top quarantine")
            .map(|entry| entry.expect("top quarantine entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|name| name.starts_with(".nib-test-displaced-entry-"))
            })
            .expect("displaced opened symlink");
        assert!(std::fs::symlink_metadata(&displaced)
            .expect("displaced symlink metadata")
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(displaced).expect("symlink target"),
            Path::new("target")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_open_directory_removal_preserves_a_boundary_replacement() {
        let root = tempdir().expect("tempdir");
        let child = root.path().join("child");
        let replacement = root.path().join("replacement");
        std::fs::create_dir(&child).expect("child directory");
        std::fs::create_dir(&replacement).expect("replacement directory");
        std::fs::write(replacement.join("sentinel"), b"replacement").expect("replacement sentinel");
        REPLACE_DIRECTORY_BEFORE_HANDLE_DELETE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(child.clone(), PathBuf::from("replacement"));

        let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
            .expect_err("directory boundary substitution must fail closed");

        assert!(
            error.to_string().contains("replacement preserved"),
            "{error}"
        );
        let top_quarantine = root
            .path()
            .join(removal_quarantine_name(Path::new("child")));
        assert_eq!(
            std::fs::read(top_quarantine.join("sentinel")).expect("preserved boundary replacement"),
            b"replacement"
        );
        assert!(!child.exists());
        assert!(!replacement.exists());
        assert!(!std::fs::read_dir(root.path())
            .expect("read root")
            .map(|entry| entry.expect("root entry").file_name())
            .any(|name| {
                name.to_str()
                    .is_some_and(|name| name.starts_with(".nib-test-displaced-directory-"))
            }));
    }

    #[cfg(unix)]
    #[test]
    fn unix_preexisting_entry_quarantine_is_preserved_as_ambiguous() {
        let root = tempdir().expect("tempdir");
        let child = root.path().join("child");
        let victim = PathBuf::from("victim");
        let collision = removal_entry_quarantine_name(&victim);
        std::fs::create_dir(&child).expect("child directory");
        std::fs::write(child.join(&victim), b"owned").expect("owned file");
        std::fs::write(child.join(&collision), b"unproven").expect("unproven collision");

        let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
            .expect_err("per-entry quarantine collision must fail closed");

        assert!(error.to_string().contains("unproven per-entry"), "{error}");
        assert_eq!(
            std::fs::read(child.join(&victim)).expect("preserved owned file"),
            b"owned"
        );
        assert_eq!(
            std::fs::read(child.join(&collision)).expect("preserved collision"),
            b"unproven"
        );
        assert!(!root
            .path()
            .join(removal_quarantine_name(Path::new("child")))
            .exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_verbatim_prefixes_compare_as_the_same_canonical_path() {
        assert!(canonical_paths_match(
            Path::new(r"\\?\C:\nib\state"),
            Path::new(r"C:\nib\state")
        ));
        assert!(canonical_paths_match(
            Path::new(r"\\?\UNC\server\share\nib"),
            Path::new(r"\\server\share\nib")
        ));
        assert!(!canonical_paths_match(
            Path::new(r"\\?\C:\nib\other"),
            Path::new(r"C:\nib\state")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_verbatim_canonical_prefix_accepts_a_real_directory() {
        let root = tempdir().expect("tempdir");
        let nested = root.path().join("one/two");

        assert_eq!(
            ensure_directory_without_symlinks(&nested).expect("safe directory"),
            nested.canonicalize().expect("canonical directory")
        );
        verify_directory_without_symlinks(&nested).expect("verify safe directory");
    }

    #[cfg(windows)]
    #[test]
    fn windows_dos_short_alias_accepts_a_real_directory() {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

        let root = tempdir().expect("tempdir");
        let canonical = root.path().canonicalize().expect("canonical tempdir");
        let long_path = path_without_windows_verbatim_prefix(&canonical);
        let mut input = long_path.as_os_str().encode_wide().collect::<Vec<_>>();
        input.push(0);
        let required = unsafe { GetShortPathNameW(input.as_ptr(), std::ptr::null_mut(), 0) };
        if required == 0 {
            panic!(
                "failed to size the DOS short-path buffer: {}",
                std::io::Error::last_os_error()
            );
        }
        let mut output = vec![0_u16; required as usize];
        let written = unsafe { GetShortPathNameW(input.as_ptr(), output.as_mut_ptr(), required) };
        if written == 0 {
            panic!(
                "failed to resolve the DOS short path: {}",
                std::io::Error::last_os_error()
            );
        }
        assert!(
            (written as usize) < output.len(),
            "DOS short-path output exceeded its sized buffer"
        );
        output.truncate(written as usize);
        let short_path = PathBuf::from(OsString::from_wide(&output));
        if short_path == long_path {
            return;
        }

        assert_eq!(
            ensure_directory_without_symlinks(&short_path).expect("safe DOS short alias"),
            canonical
        );
        verify_directory_without_symlinks(&short_path).expect("verify DOS short alias");
        let nested = canonical.join("nested");
        std::fs::create_dir(&nested).expect("nested directory");
        let canonical_nested = nested.canonicalize().expect("canonical nested directory");
        assert!(canonical_path_starts_with(&canonical_nested, &short_path)
            .expect("compare canonical child with DOS short root"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_rooted_rename_is_no_replace_and_preserves_the_open_source() {
        let root = tempdir().expect("tempdir");
        std::fs::write(root.path().join("source"), b"source").expect("source");
        let parent = cap_std::fs::Dir::open_ambient_dir(root.path(), cap_std::ambient_authority())
            .expect("parent capability");
        let source =
            open_capability_entry_no_follow(&parent, Path::new("source")).expect("open source");

        rename_open_entry_no_replace_windows(&parent, &source, Path::new("moved"))
            .expect("rooted rename");
        assert!(!root.path().join("source").exists());
        assert_eq!(
            std::fs::read(root.path().join("moved")).expect("moved source"),
            b"source"
        );

        std::fs::write(root.path().join("collision-source"), b"new").expect("collision source");
        std::fs::write(root.path().join("collision-target"), b"old").expect("collision target");
        let collision_source =
            open_capability_entry_no_follow(&parent, Path::new("collision-source"))
                .expect("open collision source");
        rename_open_entry_no_replace_windows(
            &parent,
            &collision_source,
            Path::new("collision-target"),
        )
        .expect_err("existing destination must not be replaced");
        assert_eq!(
            std::fs::read(root.path().join("collision-source")).expect("preserved source"),
            b"new"
        );
        assert_eq!(
            std::fs::read(root.path().join("collision-target")).expect("preserved target"),
            b"old"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_directory_junction_is_rejected_as_a_reparse_point() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("target");
        let junction = root.path().join("junction");
        std::fs::create_dir(&target).expect("junction target");
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .expect("create junction");
        assert!(
            output.status.success(),
            "mklink failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        assert!(verify_directory_without_symlinks(&junction).is_err());
        assert!(ensure_directory_without_symlinks(&junction.join("child")).is_err());
        assert!(
            remove_directory_tree_capability_bound(root.path(), &junction, removal_deadline())
                .is_err()
        );
        assert!(!target.join("child").exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_nested_junction_is_rejected_before_recursive_removal_mutates() {
        let root = tempdir().expect("tempdir");
        let outside = tempdir().expect("outside");
        let child = root.path().join("child");
        let junction = child.join("junction");
        std::fs::create_dir(&child).expect("child directory");
        std::fs::write(child.join("owned"), b"owned").expect("owned file");
        std::fs::write(outside.path().join("outside"), b"outside").expect("outside file");
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .output()
            .expect("create nested junction");
        assert!(
            output.status.success(),
            "mklink failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
            .expect_err("nested junction must be rejected");

        assert!(
            error.to_string().contains("reparse point"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read(child.join("owned")).expect("preserved child file"),
            b"owned"
        );
        assert_eq!(
            std::fs::read(outside.path().join("outside")).expect("preserved outside file"),
            b"outside"
        );
        assert!(junction.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_ancestor_before_creating_child() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("tempdir");
        let outside = tempdir().expect("outside");
        symlink(outside.path(), root.path().join("linked")).expect("symlink");

        assert!(ensure_directory_without_symlinks(&root.path().join("linked/state")).is_err());
        assert!(remove_directory_tree_capability_bound(
            root.path(),
            &root.path().join("linked"),
            removal_deadline()
        )
        .is_err());
        assert!(!outside.path().join("state").exists());
    }
}
