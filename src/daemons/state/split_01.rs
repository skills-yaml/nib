//! T043 split.

use super::*;

pub(crate) fn deterministic_artifact_name(
    prefix: &str,
    destination: &[u8],
    suffix: &str,
) -> OsString {
    let digest = Sha256::digest(destination);
    let mut name = String::with_capacity(prefix.len() + 32 + suffix.len());
    name.push_str(prefix);
    for byte in &digest[..16] {
        use std::fmt::Write as _;
        let _ = write!(name, "{byte:02x}");
    }
    name.push_str(suffix);
    OsString::from(name)
}

pub(crate) fn deterministic_previous_artifact_name(
    prefix: &str,
    destination: &std::ffi::OsStr,
) -> Result<OsString, String> {
    let destination = destination.to_str().ok_or_else(|| {
        "atomic state filenames must be valid UTF-8 so recovery can identify their target"
            .to_string()
    })?;
    let digest = Sha256::digest(destination.as_bytes());
    let mut name = String::with_capacity(prefix.len() + 32 + 10 + destination.len());
    name.push_str(prefix);
    for byte in &digest[..16] {
        use std::fmt::Write as _;
        let _ = write!(name, "{byte:02x}");
    }
    name.push_str(".previous-");
    name.push_str(destination);
    Ok(OsString::from(name))
}

pub(crate) fn parse_previous_artifact_name(
    name: &std::ffi::OsStr,
    prefix: &str,
) -> Option<OsString> {
    let name = name.to_str()?;
    let remainder = name.strip_prefix(prefix)?;
    let (digest, target) = remainder.split_once(".previous-")?;
    if digest.len() != 32
        || !digest
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return None;
    }
    let mut components = Path::new(target).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return None;
    }
    let expected =
        deterministic_previous_artifact_name(prefix, std::ffi::OsStr::new(target)).ok()?;
    (expected == name).then(|| OsString::from(target))
}

pub(crate) fn is_deterministic_artifact_name(
    name: &std::ffi::OsStr,
    prefix: &str,
    suffix: &str,
) -> bool {
    let bytes = name.as_encoded_bytes();
    let prefix = prefix.as_bytes();
    let suffix = suffix.as_bytes();
    let Some(expected_length) = prefix
        .len()
        .checked_add(32)
        .and_then(|length| length.checked_add(suffix.len()))
    else {
        return false;
    };
    bytes.len() == expected_length
        && bytes.starts_with(prefix)
        && bytes.ends_with(suffix)
        && bytes[prefix.len()..prefix.len() + 32]
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(windows)]
pub(crate) fn delete_open_file_platform(opened: &File) -> std::io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfoEx, SetFileInformationByHandle, FILE_DISPOSITION_FLAG_DELETE,
        FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX,
    };

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
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    Err(std::io::Error::new(
        error.kind(),
        format!(
            "POSIX handle deletion is unavailable; legacy deferred deletion is not safe for retained ownership handles: {error}"
        ),
    ))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn rename_file_no_replace_platform(
    directory: &cap_std::fs::Dir,
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source contains a NUL byte",
        )
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination contains a NUL byte",
        )
    })?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) fn rename_file_no_replace_platform(
    directory: &cap_std::fs::Dir,
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source contains a NUL byte",
        )
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination contains a NUL byte",
        )
    })?;
    let result = unsafe {
        libc::renameatx_np(
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
pub(crate) fn rename_file_no_replace_platform(
    _directory: &cap_std::fs::Dir,
    _source: &Path,
    _destination: &Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no no-replace file relocation primitive",
    ))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn publish_open_file_no_replace_platform(
    directory: &cap_std::fs::Dir,
    _source: &Path,
    source_file: &File,
    destination: &Path,
) -> std::io::Result<HandlePublication> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "state filename contains an interior NUL byte",
        )
    })?;
    let empty = c"";
    // AT_EMPTY_PATH binds the new name to the retained file description rather than a mutable
    // source pathname. The destination is relative to the retained directory capability.
    let result = unsafe {
        libc::linkat(
            source_file.as_raw_fd(),
            empty.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    if result == 0 {
        return Ok(HandlePublication::Linked);
    }
    let empty_path_error = std::io::Error::last_os_error();
    let fd_path = CString::new(format!("/proc/self/fd/{}", source_file.as_raw_fd()))
        .expect("numeric file descriptor path has no NUL bytes");
    let fallback = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            fd_path.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    if fallback == 0 {
        Ok(HandlePublication::Linked)
    } else {
        let fallback_error = std::io::Error::last_os_error();
        Err(std::io::Error::new(
            fallback_error.kind(),
            format!(
                "AT_EMPTY_PATH failed ({empty_path_error}); /proc/self/fd publication failed ({fallback_error})"
            ),
        ))
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn rename_open_directory_no_replace_platform(
    parent: &cap_std::fs::Dir,
    source: &Path,
    _source_directory: &cap_std::fs::Dir,
    destination: &Path,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source contains a NUL byte",
        )
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
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
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(crate) fn rename_open_directory_no_replace_platform(
    parent: &cap_std::fs::Dir,
    _source: &Path,
    source_directory: &cap_std::fs::Dir,
    destination: &Path,
) -> std::io::Result<()> {
    crate::fs_security::rename_open_entry_no_replace_windows(parent, source_directory, destination)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) fn rename_open_directory_no_replace_platform(
    parent: &cap_std::fs::Dir,
    source: &Path,
    _source_directory: &cap_std::fs::Dir,
    destination: &Path,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source contains a NUL byte",
        )
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
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
        Err(std::io::Error::last_os_error())
    }
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
pub(crate) fn rename_open_directory_no_replace_platform(
    _parent: &cap_std::fs::Dir,
    _source: &Path,
    _source_directory: &cap_std::fs::Dir,
    _destination: &Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no no-replace directory quarantine primitive",
    ))
}

#[cfg(windows)]
pub(crate) fn publish_open_file_no_replace_platform(
    directory: &cap_std::fs::Dir,
    _source: &Path,
    source_file: &File,
    destination: &Path,
) -> std::io::Result<HandlePublication> {
    crate::fs_security::rename_open_entry_no_replace_windows(directory, source_file, destination)?;
    Ok(HandlePublication::Moved)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) fn publish_open_file_no_replace_platform(
    directory: &cap_std::fs::Dir,
    source: &Path,
    _source_file: &File,
    destination: &Path,
) -> std::io::Result<HandlePublication> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "state filename contains an interior NUL byte",
        )
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "state filename contains an interior NUL byte",
        )
    })?;
    let result = unsafe {
        libc::renameatx_np(
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(HandlePublication::Moved)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
pub(crate) fn publish_open_file_no_replace_platform(
    _directory: &cap_std::fs::Dir,
    _source: &Path,
    _source_file: &File,
    _destination: &Path,
) -> std::io::Result<HandlePublication> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no handle-bound no-replace state publication primitive",
    ))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn publish_open_file_no_replace_platform(
    _directory: &cap_std::fs::Dir,
    _source: &Path,
    _source_file: &File,
    _destination: &Path,
) -> std::io::Result<HandlePublication> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no handle-bound no-replace state publication primitive",
    ))
}

pub(crate) fn same_open_file_identity(left: &File, right: &File) -> Result<bool, String> {
    let left = crate::fs_security::FileIdentity::from_file(
        left.try_clone()
            .map_err(|error| format!("failed to clone state file: {error}"))?,
    )
    .map_err(|error| format!("failed to identify state file: {error}"))?;
    let right = crate::fs_security::FileIdentity::from_file(
        right
            .try_clone()
            .map_err(|error| format!("failed to clone state file: {error}"))?,
    )
    .map_err(|error| format!("failed to identify state file: {error}"))?;
    Ok(left == right)
}

pub(crate) fn stable_directory_identity(
    directory: &cap_std::fs::Dir,
    path: &Path,
) -> Result<crate::fs_security::FileIdentity, String> {
    crate::fs_security::FileIdentity::from_file(
        directory
            .try_clone()
            .map(cap_std::fs::Dir::into_std_file)
            .map_err(|error| format!("failed to clone directory {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("failed to identify directory {}: {error}", path.display()))
}

#[cfg(windows)]
pub(crate) fn windows_visible_directory_file(path: &Path) -> Result<File, String> {
    crate::fs_security::open_directory_observation_windows(path).map_err(|error| {
        format!(
            "failed to observe state directory {}: {error}",
            path.display()
        )
    })
}

#[cfg(windows)]
pub(crate) fn windows_visible_directory_identity(
    path: &Path,
) -> Result<crate::fs_security::FileIdentity, String> {
    crate::fs_security::FileIdentity::from_file(windows_visible_directory_file(path)?)
        .map_err(|error| format!("failed to identify directory {}: {error}", path.display()))
}

pub(crate) fn read_open_file_prefix(file: &File, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = vec![0_u8; limit];
    let mut read_total = 0_usize;
    while read_total < limit {
        let offset = u64::try_from(read_total)
            .map_err(|_| std::io::Error::other("state read offset overflowed"))?;
        match read_open_file_at(file, &mut bytes[read_total..], offset) {
            Ok(0) => break,
            Ok(read) => read_total = read_total.saturating_add(read),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    bytes.truncate(read_total);
    Ok(bytes)
}

#[cfg(unix)]
pub(crate) fn read_open_file_at(
    file: &File,
    buffer: &mut [u8],
    offset: u64,
) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, offset)
}

#[cfg(windows)]
pub(crate) fn read_open_file_at(
    file: &File,
    buffer: &mut [u8],
    offset: u64,
) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn read_open_file_at(
    file: &File,
    buffer: &mut [u8],
    offset: u64,
) -> std::io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(offset))?;
    file.read(buffer)
}

pub(crate) fn configure_capability_owner_only(_options: &mut cap_std::fs::OpenOptions) {
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        _options.mode(0o600);
    }
}

pub(crate) fn configure_capability_no_follow(options: &mut cap_std::fs::OpenOptions) {
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

pub(crate) fn configure_capability_delete_access(
    options: &mut cap_std::fs::OpenOptions,
    read: bool,
    write: bool,
) {
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows_sys::Win32::Storage::FileSystem::DELETE;

        let mut access = DELETE;
        if read {
            access |= GENERIC_READ;
        }
        if write {
            access |= GENERIC_WRITE;
        }
        options.access_mode(access);
    }
    #[cfg(not(windows))]
    {
        let _ = (options, read, write);
    }
}

pub(crate) fn validate_stable_file_metadata(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), String> {
    if crate::fs_security::metadata_is_link_or_reparse(metadata) || !metadata.is_file() {
        return Err(format!(
            "state file must be a regular local file and must not be a symlink or reparse point: {}",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn validate_capability_file_metadata(
    path: &Path,
    metadata: &cap_std::fs::Metadata,
) -> Result<(), String> {
    if capability_file_metadata_is_link(metadata) || !metadata.is_file() {
        return Err(format!(
            "state file must be a regular local file and must not be a symlink or reparse point: {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn capability_file_metadata_is_link(metadata: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub(crate) fn capability_file_metadata_is_link(metadata: &cap_std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

pub(crate) fn with_file_lock<T>(
    lock_path: &Path,
    operation: impl FnOnce(&StableDirectory) -> Result<T, String>,
) -> Result<T, String> {
    let protected_directory = lock_path
        .parent()
        .ok_or_else(|| format!("lock path has no parent: {}", lock_path.display()))?;
    with_file_lock_in(lock_path, protected_directory, operation)
}

pub(crate) struct HeldFileLock {
    pub(crate) _anchor_file: File,
    pub(crate) lock_directory: StableDirectory,
    pub(crate) lock_path: PathBuf,
    pub(crate) anchor_directory: StableDirectory,
    pub(crate) anchor_path: PathBuf,
    pub(crate) locked_identity: crate::fs_security::FileIdentity,
    pub(crate) protected_directory: StableDirectory,
}

impl HeldFileLock {
    pub(crate) fn verify(&self) -> Result<(), String> {
        self.lock_directory.verify_visible()?;
        self.anchor_directory.verify_visible()?;
        verify_daemon_lock_paths_bound(
            &self.lock_directory,
            &self.lock_path,
            &self.anchor_directory,
            &self.anchor_path,
            &self.locked_identity,
        )?;
        self.protected_directory.verify_visible()
    }

    pub(crate) fn verify_until(&self, deadline: Instant) -> Result<(), String> {
        ensure_daemon_lock_deadline(Some(deadline), &self.lock_path)?;
        self.verify()?;
        ensure_daemon_lock_deadline(Some(deadline), &self.lock_path)
    }
}

/// Acquires a persistent file lock while retaining the caller's exact
/// protected-directory capability. The returned guard keeps the lock held and
/// re-verifies both the lock domain and protected directory on demand.
pub(crate) fn acquire_file_lock_in_until_bound(
    lock_path: &Path,
    protected_directory: &StableDirectory,
    deadline: Instant,
) -> Result<HeldFileLock, String> {
    ensure_daemon_lock_deadline(Some(deadline), lock_path)?;
    protected_directory.verify_visible()?;
    let parent = lock_path
        .parent()
        .ok_or_else(|| format!("lock path has no parent: {}", lock_path.display()))?;
    let file_name = lock_path
        .file_name()
        .ok_or_else(|| format!("daemon lock path has no file name: {}", lock_path.display()))?;
    let anchor_path = daemon_lock_anchor_path(lock_path)?;
    let anchor_parent = anchor_path.parent().ok_or_else(|| {
        format!(
            "daemon lock anchor has no parent: {}",
            anchor_path.display()
        )
    })?;
    let retained_root =
        common_directory_ancestor(&[parent, anchor_parent, protected_directory.path()])?;
    let root_directory = open_lock_capability_root(&retained_root)?;
    let mut namespace_guard = || {
        ensure_daemon_lock_deadline(Some(deadline), lock_path)?;
        protected_directory.verify_visible()?;
        ensure_daemon_lock_deadline(Some(deadline), lock_path)
    };
    let lock_directory = root_directory.open_or_create_descendant_directory_with_guard(
        parent,
        &mut namespace_guard,
        |_| Ok(()),
    )?;
    let anchor_directory = root_directory.open_or_create_descendant_directory_with_guard(
        anchor_parent,
        &mut namespace_guard,
        |_| Ok(()),
    )?;
    let opened_protected = root_directory.open_or_create_descendant_directory_with_guard(
        protected_directory.path(),
        &mut namespace_guard,
        |missing| {
            Err(format!(
                "protected state directory does not exist: {}",
                missing.display()
            ))
        },
    )?;
    if !opened_protected.same_identity(protected_directory) {
        return Err(format!(
            "protected state directory identity changed before lock acquisition: {}",
            protected_directory.path().display()
        ));
    }
    let lock_path = lock_directory.path().join(file_name);
    let anchor_file = open_daemon_lock_anchor_bound_with_guard(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &mut namespace_guard,
    )?;
    let locked_identity = daemon_lock_identity(&anchor_file, &anchor_path)?;
    lock_daemon_anchor(&anchor_file, &lock_path, Some(deadline))?;
    namespace_guard()?;
    repair_daemon_lock_anchor_with_guard(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &locked_identity,
        &mut namespace_guard,
    )?;
    verify_daemon_lock_paths_bound(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &locked_identity,
    )?;
    protected_directory.verify_visible()?;
    ensure_daemon_lock_deadline(Some(deadline), &lock_path)?;
    Ok(HeldFileLock {
        _anchor_file: anchor_file,
        lock_directory,
        lock_path,
        anchor_directory,
        anchor_path,
        locked_identity,
        protected_directory: protected_directory.try_clone()?,
    })
}

pub(crate) fn try_acquire_file_lock_in(
    lock_path: &Path,
    protected_directory: &Path,
) -> Result<HeldFileLock, String> {
    let parent = lock_path
        .parent()
        .ok_or_else(|| format!("lock path has no parent: {}", lock_path.display()))?;
    let parent = crate::fs_security::ensure_directory_without_symlinks(parent)
        .map_err(|error| error.to_string())?;
    let file_name = lock_path
        .file_name()
        .ok_or_else(|| format!("daemon lock path has no file name: {}", lock_path.display()))?;
    let lock_path = parent.join(file_name);
    let anchor_path = daemon_lock_anchor_path(&lock_path)?;
    let anchor_parent = anchor_path.parent().ok_or_else(|| {
        format!(
            "daemon lock anchor has no parent: {}",
            anchor_path.display()
        )
    })?;
    crate::fs_security::ensure_directory_without_symlinks(anchor_parent)
        .map_err(|error| format!("daemon lock anchor directory is unsafe: {error}"))?;
    let lock_directory = StableDirectory::open(&parent)?;
    let anchor_directory = StableDirectory::open(anchor_parent)?;
    let protected_directory = StableDirectory::open(protected_directory)?;

    let anchor_file = open_daemon_lock_anchor_bound(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
    )?;
    let locked_identity = daemon_lock_identity(&anchor_file, &anchor_path)?;
    match anchor_file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(format!(
                "state lock is already held by another owner: {}",
                lock_path.display()
            ));
        }
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(format!(
                "failed to acquire state lock {}: {error}",
                lock_path.display()
            ));
        }
    }

    lock_directory.verify_visible()?;
    anchor_directory.verify_visible()?;
    repair_daemon_lock_anchor(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &locked_identity,
    )?;
    verify_daemon_lock_paths_bound(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &locked_identity,
    )?;
    protected_directory.verify_visible()?;

    Ok(HeldFileLock {
        _anchor_file: anchor_file,
        lock_directory,
        lock_path,
        anchor_directory,
        anchor_path,
        locked_identity,
        protected_directory,
    })
}

pub(crate) fn with_file_lock_in<T>(
    lock_path: &Path,
    protected_directory: &Path,
    operation: impl FnOnce(&StableDirectory) -> Result<T, String>,
) -> Result<T, String> {
    with_file_lock_in_with_deadline(lock_path, protected_directory, None, operation)
}

pub(crate) fn with_file_lock_in_until<T>(
    lock_path: &Path,
    protected_directory: &Path,
    deadline: Instant,
    operation: impl FnOnce(&StableDirectory) -> Result<T, String>,
) -> Result<T, String> {
    with_file_lock_in_with_deadline(lock_path, protected_directory, Some(deadline), operation)
}

pub(crate) fn with_file_lock_in_until_with_setup_hook<T>(
    lock_path: &Path,
    protected_directory: &Path,
    deadline: Instant,
    operation: impl FnOnce(&StableDirectory) -> Result<T, String>,
    before_setup_step: impl FnMut() -> Result<(), String>,
) -> Result<T, String> {
    with_file_lock_in_with_deadline_and_setup_hook(
        lock_path,
        protected_directory,
        Some(deadline),
        operation,
        before_setup_step,
    )
}

pub(crate) fn with_file_lock_in_with_deadline<T>(
    lock_path: &Path,
    protected_directory: &Path,
    deadline: Option<Instant>,
    operation: impl FnOnce(&StableDirectory) -> Result<T, String>,
) -> Result<T, String> {
    with_file_lock_in_with_deadline_and_setup_hook(
        lock_path,
        protected_directory,
        deadline,
        operation,
        || Ok(()),
    )
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn with_file_lock_in_with_deadline_and_setup_hook<T>(
    lock_path: &Path,
    protected_directory: &Path,
    deadline: Option<Instant>,
    operation: impl FnOnce(&StableDirectory) -> Result<T, String>,
    mut before_setup_step: impl FnMut() -> Result<(), String>,
) -> Result<T, String> {
    ensure_daemon_lock_deadline(deadline, lock_path)?;
    let parent = lock_path
        .parent()
        .ok_or_else(|| format!("lock path has no parent: {}", lock_path.display()))?;
    let file_name = lock_path
        .file_name()
        .ok_or_else(|| format!("daemon lock path has no file name: {}", lock_path.display()))?;
    let lock_path = parent.join(file_name);
    let anchor_path = daemon_lock_anchor_path(&lock_path)?;
    let anchor_parent = anchor_path.parent().ok_or_else(|| {
        format!(
            "daemon lock anchor has no parent: {}",
            anchor_path.display()
        )
    })?;
    let (lock_directory, anchor_directory, protected_directory, anchor_file) = if deadline.is_some()
    {
        let retained_root =
            common_directory_ancestor(&[parent, anchor_parent, protected_directory])?;
        let root_directory = open_lock_capability_root(&retained_root)?;
        let mut namespace_guard = || {
            before_setup_step()?;
            ensure_daemon_lock_deadline(deadline, &lock_path)
        };
        namespace_guard()?;
        let lock_directory = root_directory.open_or_create_descendant_directory_with_guard(
            parent,
            &mut namespace_guard,
            |_| Ok(()),
        )?;
        namespace_guard()?;
        let anchor_directory = root_directory.open_or_create_descendant_directory_with_guard(
            anchor_parent,
            &mut namespace_guard,
            |_| Ok(()),
        )?;
        namespace_guard()?;
        let protected = root_directory.open_or_create_descendant_directory_with_guard(
            protected_directory,
            &mut namespace_guard,
            |missing| {
                Err(format!(
                    "protected state directory does not exist: {}",
                    missing.display()
                ))
            },
        )?;
        namespace_guard()?;
        let anchor_file = open_daemon_lock_anchor_bound_with_guard(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
            &mut namespace_guard,
        )?;
        namespace_guard()?;
        (lock_directory, anchor_directory, protected, anchor_file)
    } else {
        let parent = crate::fs_security::ensure_directory_without_symlinks(parent)
            .map_err(|error| error.to_string())?;
        crate::fs_security::ensure_directory_without_symlinks(anchor_parent)
            .map_err(|error| format!("daemon lock anchor directory is unsafe: {error}"))?;
        let lock_directory = StableDirectory::open(&parent)?;
        let anchor_directory = StableDirectory::open(anchor_parent)?;
        let protected = StableDirectory::open(protected_directory)?;
        let anchor_file = open_daemon_lock_anchor_bound(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
        )?;
        (lock_directory, anchor_directory, protected, anchor_file)
    };
    let locked_identity = daemon_lock_identity(&anchor_file, &anchor_path)?;
    lock_daemon_anchor(&anchor_file, &lock_path, deadline)?;

    lock_directory.verify_visible()?;
    anchor_directory.verify_visible()?;
    ensure_daemon_lock_deadline(deadline, &lock_path)?;
    if deadline.is_some() {
        let mut namespace_guard = || {
            before_setup_step()?;
            ensure_daemon_lock_deadline(deadline, &lock_path)
        };
        repair_daemon_lock_anchor_with_guard(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
            &locked_identity,
            &mut namespace_guard,
        )?;
    } else {
        repair_daemon_lock_anchor(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
            &locked_identity,
        )?;
    }
    verify_daemon_lock_paths_bound(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &locked_identity,
    )?;
    protected_directory.verify_visible()?;
    ensure_daemon_lock_deadline(deadline, &lock_path)?;
    let result = operation(&protected_directory);
    let operation_deadline = ensure_daemon_lock_deadline(deadline, &lock_path);
    let attachment_result = (|| {
        lock_directory.verify_visible()?;
        anchor_directory.verify_visible()?;
        verify_daemon_lock_paths_bound(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
            &locked_identity,
        )?;
        protected_directory.verify_visible()
    })();
    let outcome = match (result, operation_deadline, attachment_result) {
        (_, _, Err(error)) => Err(error),
        (_, Err(error), Ok(())) => Err(error),
        (Err(error), Ok(()), Ok(())) => Err(error),
        (Ok(value), Ok(()), Ok(())) => Ok(value),
    };
    if outcome.is_ok() {
        ensure_daemon_lock_deadline(deadline, &lock_path)?;
        cleanup_daemon_lock_anchor(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
            &locked_identity,
            deadline,
        )?;
    }
    outcome
}

pub(crate) fn lock_daemon_anchor(
    anchor_file: &File,
    lock_path: &Path,
    deadline: Option<Instant>,
) -> Result<(), String> {
    let Some(deadline) = deadline else {
        return anchor_file.lock().map_err(|error| {
            format!(
                "failed to lock daemon state {}: {error}",
                lock_path.display()
            )
        });
    };

    loop {
        ensure_daemon_lock_deadline(Some(deadline), lock_path)?;
        match anchor_file.try_lock() {
            Ok(()) => {
                ensure_daemon_lock_deadline(Some(deadline), lock_path)?;
                return Ok(());
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(format!(
                        "timed out acquiring daemon state lock: {}",
                        lock_path.display()
                    ));
                }
                thread::sleep(FILE_LOCK_POLL_INTERVAL.min(deadline - now));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!(
                    "failed to lock daemon state {}: {error}",
                    lock_path.display()
                ));
            }
        }
    }
}

pub(crate) fn ensure_daemon_lock_deadline(
    deadline: Option<Instant>,
    lock_path: &Path,
) -> Result<(), String> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(format!(
            "timed out acquiring daemon state lock: {}",
            lock_path.display()
        ));
    }
    Ok(())
}

pub(crate) fn common_directory_ancestor(paths: &[&Path]) -> Result<PathBuf, String> {
    let Some(first) = paths.first() else {
        return Err("cannot derive a retained state-directory ancestor from no paths".to_string());
    };
    first
        .ancestors()
        .find(|ancestor| paths.iter().all(|path| path.starts_with(ancestor)))
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            format!(
                "state lock paths do not share a retained directory ancestor: {}",
                first.display()
            )
        })
}

#[cfg(not(windows))]
pub(crate) fn open_lock_capability_root(retained_root: &Path) -> Result<StableDirectory, String> {
    StableDirectory::open(retained_root)
}

#[cfg(windows)]
pub(crate) fn open_lock_capability_root(retained_root: &Path) -> Result<StableDirectory, String> {
    // A retained Windows directory capability may include DELETE access. Reopening
    // that exact directory through an ambient spelling is not share-compatible on
    // every filesystem/runner combination, even when the handle itself permits
    // sharing. Open its parent ambiently, then retain the already-existing common
    // root handle-relatively before any callback or create-capable descendant walk.
    let Some(parent) = retained_root.parent() else {
        return StableDirectory::open(retained_root);
    };
    let parent_directory = StableDirectory::open(parent)?;
    parent_directory.open_child(retained_root)
}

pub(crate) fn daemon_lock_anchor_path(lock_path: &Path) -> Result<PathBuf, String> {
    let visible_directory = lock_path.parent().ok_or_else(|| {
        format!(
            "daemon lock directory has no parent: {}",
            lock_path.display()
        )
    })?;
    let fallback_anchor_directory = visible_directory.parent().ok_or_else(|| {
        format!(
            "daemon lock directory has no persistent anchor parent: {}",
            visible_directory.display()
        )
    })?;
    let visible_directory_name = visible_directory.file_name().ok_or_else(|| {
        format!(
            "daemon lock directory has no file name: {}",
            visible_directory.display()
        )
    })?;
    let anchor_directory = if visible_directory_name == ".nib" {
        git_lock_anchor_directory(fallback_anchor_directory)
    } else {
        None
    }
    .unwrap_or_else(|| fallback_anchor_directory.to_path_buf());
    let lock_file_name = lock_path
        .file_name()
        .ok_or_else(|| format!("daemon lock path has no file name: {}", lock_path.display()))?;

    let mut anchor_name = OsString::from(format!(
        ".nib-lock-{}-",
        visible_directory_name.as_encoded_bytes().len()
    ));
    anchor_name.push(visible_directory_name);
    anchor_name.push("-");
    anchor_name.push(lock_file_name);
    anchor_name.push(".anchor");
    Ok(anchor_directory.join(anchor_name))
}

pub(crate) fn git_lock_anchor_directory(project_root: &Path) -> Option<PathBuf> {
    let dot_git = project_root.join(".git");
    let metadata = fs::symlink_metadata(&dot_git).ok()?;
    if crate::fs_security::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return None;
    }
    Some(dot_git.join("nib").join("locks"))
}

#[cfg(any(unix, windows))]
pub(crate) fn cleanup_legacy_lock_pair_with_guard(
    visible_directory: &StableDirectory,
    visible_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    mut namespace_guard: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    cleanup_legacy_lock_pair_optional_with_guard(
        Some(visible_directory),
        visible_path,
        anchor_directory,
        anchor_path,
        &mut namespace_guard,
    )
}

#[cfg(any(unix, windows))]
pub(crate) fn cleanup_legacy_lock_pair_optional_with_guard(
    visible_directory: Option<&StableDirectory>,
    visible_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    mut namespace_guard: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    cleanup_legacy_lock_pair_with_hooks(
        visible_directory,
        visible_path,
        anchor_directory,
        anchor_path,
        &mut namespace_guard,
        || Ok(()),
    )
}

#[cfg(all(test, any(unix, windows)))]
pub(crate) fn cleanup_legacy_lock_pair_with_hook(
    visible_directory: &StableDirectory,
    visible_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    before_delete: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    cleanup_legacy_lock_pair_with_hooks(
        Some(visible_directory),
        visible_path,
        anchor_directory,
        anchor_path,
        &mut || Ok(()),
        before_delete,
    )
}

#[cfg(any(unix, windows))]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn cleanup_legacy_lock_pair_with_hooks(
    visible_directory: Option<&StableDirectory>,
    visible_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    namespace_guard: &mut impl FnMut() -> Result<(), String>,
    before_delete: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    namespace_guard()?;
    struct Artifact {
        pub(crate) canonical: Option<File>,
        pub(crate) quarantine: Option<File>,
        pub(crate) quarantine_path: PathBuf,
    }
    let open_artifact = |directory: &StableDirectory, path: &Path| -> Result<Artifact, String> {
        let quarantine_path = directory.deterministic_artifact_path(
            path,
            ".nib-legacy-lock-delete-",
            ".quarantine",
        )?;
        let canonical = directory
            .path_exists(path)?
            .then(|| directory.open_read_write(path))
            .transpose()?;
        let quarantine = directory
            .path_exists(&quarantine_path)?
            .then(|| directory.open_read_write(&quarantine_path))
            .transpose()?;
        if let (Some(canonical), Some(quarantine)) = (&canonical, &quarantine) {
            if !same_open_file_identity(canonical, quarantine)? {
                return Err(format!(
                    "legacy lock and its deletion quarantine have different identities; both were preserved: {}",
                    path.display()
                ));
            }
        }
        Ok(Artifact {
            canonical,
            quarantine,
            quarantine_path,
        })
    };
    let visible = match visible_directory {
        Some(directory) => open_artifact(directory, visible_path)?,
        None => Artifact {
            canonical: None,
            quarantine: None,
            quarantine_path: visible_path.to_path_buf(),
        },
    };
    let anchor = open_artifact(anchor_directory, anchor_path)?;
    let mut files = [
        anchor.canonical.as_ref(),
        anchor.quarantine.as_ref(),
        visible.canonical.as_ref(),
        visible.quarantine.as_ref(),
    ]
    .into_iter()
    .flatten();
    let Some(file) = files.next() else {
        return namespace_guard();
    };
    let identity = crate::fs_security::FileIdentity::from_file(
        file.try_clone()
            .map_err(|error| format!("failed to clone legacy lock: {error}"))?,
    )
    .map_err(|error| format!("failed to identify legacy lock: {error}"))?;
    for probe in files {
        let probe_identity = crate::fs_security::FileIdentity::from_file(
            probe
                .try_clone()
                .map_err(|error| format!("failed to clone legacy lock: {error}"))?,
        )
        .map_err(|error| format!("failed to identify legacy lock: {error}"))?;
        if probe_identity != identity {
            return Err(format!(
                "legacy lock and anchor have different identities: {}",
                visible_path.display()
            ));
        }
    }
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(format!(
                "legacy lock is still owned and cannot be migrated: {}",
                visible_path.display()
            ))
        }
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(format!(
                "failed to acquire legacy lock {}: {error}",
                visible_path.display()
            ))
        }
    }
    before_delete()?;
    for (directory, path, artifact) in visible_directory
        .map(|directory| (directory, visible_path, &visible))
        .into_iter()
        .chain(std::iter::once((anchor_directory, anchor_path, &anchor)))
    {
        if let Some(canonical) = &artifact.canonical {
            directory.remove_file_if_matches_with_guard(
                path,
                canonical,
                ".nib-legacy-lock-delete-",
                &mut *namespace_guard,
            )?;
        } else if let Some(quarantine) = &artifact.quarantine {
            directory.remove_visible_file_if_matches_direct_with_guard(
                &artifact.quarantine_path,
                quarantine,
                &mut *namespace_guard,
            )?;
        }
    }
    if let Some(visible_directory) = visible_directory {
        visible_directory.verify_visible()?;
    }
    anchor_directory.verify_visible()?;
    namespace_guard()
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn cleanup_legacy_lock_pair_with_guard(
    _visible_directory: &StableDirectory,
    visible_path: &Path,
    _anchor_directory: &StableDirectory,
    _anchor_path: &Path,
    _namespace_guard: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    Err(format!(
        "legacy lock migration is unsupported on this platform: {}",
        visible_path.display()
    ))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn cleanup_legacy_lock_pair_optional_with_guard(
    _visible_directory: Option<&StableDirectory>,
    visible_path: &Path,
    _anchor_directory: &StableDirectory,
    _anchor_path: &Path,
    _namespace_guard: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    Err(format!(
        "legacy lock migration is unsupported on this platform: {}",
        visible_path.display()
    ))
}

#[cfg(any(unix, windows))]
pub(crate) fn open_daemon_lock_anchor_bound(
    lock_directory: &StableDirectory,
    lock_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
) -> Result<File, String> {
    open_daemon_lock_anchor_bound_with_guard_and_hook(
        lock_directory,
        lock_path,
        anchor_directory,
        anchor_path,
        &mut || Ok(()),
        || Ok(()),
    )
}

#[cfg(any(unix, windows))]
pub(crate) fn open_daemon_lock_anchor_bound_with_guard(
    lock_directory: &StableDirectory,
    lock_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    mut namespace_guard: impl FnMut() -> Result<(), String>,
) -> Result<File, String> {
    open_daemon_lock_anchor_bound_with_guard_and_hook(
        lock_directory,
        lock_path,
        anchor_directory,
        anchor_path,
        &mut namespace_guard,
        || Ok(()),
    )
}

#[cfg(any(unix, windows))]
#[cfg(test)]
pub(crate) fn open_daemon_lock_anchor_bound_with_hook(
    lock_directory: &StableDirectory,
    lock_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    mut before_open: impl FnMut() -> Result<(), String>,
) -> Result<File, String> {
    open_daemon_lock_anchor_bound_with_guard_and_hook(
        lock_directory,
        lock_path,
        anchor_directory,
        anchor_path,
        &mut || Ok(()),
        &mut before_open,
    )
}

#[cfg(any(unix, windows))]
pub(crate) fn open_daemon_lock_anchor_bound_with_guard_and_hook(
    lock_directory: &StableDirectory,
    lock_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    namespace_guard: &mut impl FnMut() -> Result<(), String>,
    mut before_open: impl FnMut() -> Result<(), String>,
) -> Result<File, String> {
    const MAX_NAMESPACE_RETRIES: usize = 8;

    for attempt in 0..MAX_NAMESPACE_RETRIES {
        namespace_guard()?;
        let observed = (
            lock_directory.path_exists(lock_path)?,
            anchor_directory.path_exists(anchor_path)?,
        );
        namespace_guard()?;
        let opened = (|| {
            let (lock_exists, anchor_exists) = observed;
            if !anchor_exists {
                return if lock_exists {
                    let file = lock_directory.open_read_write(lock_path)?;
                    namespace_guard()?;
                    Ok(file)
                } else {
                    lock_directory
                        .open_read_write_create_with_guard(lock_path, &mut *namespace_guard)
                };
            }

            before_open()?;
            namespace_guard()?;
            let anchor_file = anchor_directory.open_read_write(anchor_path)?;
            namespace_guard()?;
            if lock_exists {
                let visible = lock_directory.open_read_write(lock_path)?;
                namespace_guard()?;
                let anchor_identity = daemon_lock_identity(&anchor_file, anchor_path)?;
                if daemon_lock_identity(&visible, lock_path)? != anchor_identity {
                    return Err(format!(
                        "daemon lock and persistent anchor have different identities: {}",
                        lock_path.display()
                    ));
                }
            }
            namespace_guard()?;
            Ok(anchor_file)
        })();
        match opened {
            Ok(file) => {
                namespace_guard()?;
                return Ok(file);
            }
            Err(error) => {
                namespace_guard()?;
                let current = (
                    lock_directory.path_exists(lock_path)?,
                    anchor_directory.path_exists(anchor_path)?,
                );
                namespace_guard()?;
                if current == observed {
                    return Err(error);
                }
                if attempt + 1 == MAX_NAMESPACE_RETRIES {
                    return Err(format!(
                        "daemon lock namespace changed repeatedly while opening {}: {error}",
                        lock_path.display()
                    ));
                }
            }
        }
    }
    unreachable!("bounded daemon lock open loop always returns")
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn open_daemon_lock_anchor_bound(
    _lock_directory: &StableDirectory,
    lock_path: &Path,
    _anchor_directory: &StableDirectory,
    _anchor_path: &Path,
) -> Result<File, String> {
    Err(format!(
        "persistent daemon lock anchors are unsupported on this platform: {}",
        lock_path.display()
    ))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn open_daemon_lock_anchor_bound_with_guard(
    _lock_directory: &StableDirectory,
    lock_path: &Path,
    _anchor_directory: &StableDirectory,
    _anchor_path: &Path,
    _namespace_guard: impl FnMut() -> Result<(), String>,
) -> Result<File, String> {
    Err(format!(
        "persistent daemon lock anchors are unsupported on this platform: {}",
        lock_path.display()
    ))
}

#[cfg(all(test, any(unix, windows)))]
pub(crate) fn open_daemon_lock_file_with_hook(
    path: &Path,
    after_inspect: impl FnOnce() -> Result<(), String>,
) -> Result<File, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_daemon_lock_metadata(path, &metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "failed to inspect daemon lock {}: {error}",
                path.display()
            ))
        }
    }
    after_inspect()?;

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    configure_daemon_lock_no_follow(&mut options);
    let file = options
        .open(path)
        .map_err(|error| format!("failed to open daemon lock {}: {error}", path.display()))?;
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect daemon lock {}: {error}", path.display()))?;
    let opened_metadata = file.metadata().map_err(|error| {
        format!(
            "failed to inspect open daemon lock {}: {error}",
            path.display()
        )
    })?;
    validate_daemon_lock_metadata(path, &path_metadata)?;
    validate_daemon_lock_metadata(path, &opened_metadata)?;
    let opened_identity = daemon_lock_identity(&file, path)?;
    let path_identity = open_daemon_lock_identity(path)?;
    if opened_identity != path_identity {
        return Err(format!(
            "daemon lock changed while it was opened: {}",
            path.display()
        ));
    }
    Ok(file)
}

#[cfg(all(test, any(unix, windows)))]
pub(crate) fn open_daemon_lock_identity(
    path: &Path,
) -> Result<crate::fs_security::FileIdentity, String> {
    crate::fs_security::FileIdentity::from_file(open_daemon_lock_probe(path)?)
        .map_err(|error| format!("failed to identify daemon lock {}: {error}", path.display()))
}

#[cfg(all(test, any(unix, windows)))]
pub(crate) fn open_daemon_lock_probe(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    configure_daemon_lock_no_follow(&mut options);
    let file = options
        .open(path)
        .map_err(|error| format!("failed to re-open daemon lock {}: {error}", path.display()))?;
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect daemon lock {}: {error}", path.display()))?;
    let opened_metadata = file.metadata().map_err(|error| {
        format!(
            "failed to inspect re-opened daemon lock {}: {error}",
            path.display()
        )
    })?;
    validate_daemon_lock_metadata(path, &path_metadata)?;
    validate_daemon_lock_metadata(path, &opened_metadata)?;
    Ok(file)
}

#[cfg(all(test, any(unix, windows)))]
pub(crate) fn configure_daemon_lock_no_follow(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

#[cfg(all(test, any(unix, windows)))]
pub(crate) fn validate_daemon_lock_metadata(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), String> {
    if daemon_lock_metadata_is_link(metadata) || !metadata.is_file() {
        return Err(format!(
            "daemon lock must be a regular local file and must not be a symlink or reparse point: {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(all(test, windows))]
pub(crate) fn daemon_lock_metadata_is_link(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(all(test, unix))]
pub(crate) fn daemon_lock_metadata_is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(any(unix, windows))]
pub(crate) fn daemon_lock_identity(
    file: &File,
    path: &Path,
) -> Result<crate::fs_security::FileIdentity, String> {
    crate::fs_security::FileIdentity::from_file(
        file.try_clone()
            .map_err(|error| format!("failed to clone daemon lock {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("failed to identify daemon lock {}: {error}", path.display()))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn daemon_lock_identity(_file: &File, path: &Path) -> Result<(), String> {
    Err(format!(
        "stable daemon lock identity is unsupported on this platform: {}",
        path.display()
    ))
}

#[cfg(any(unix, windows))]
pub(crate) fn verify_daemon_lock_paths_bound(
    lock_directory: &StableDirectory,
    lock_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    expected: &crate::fs_security::FileIdentity,
) -> Result<(), String> {
    for (directory, path) in [(lock_directory, lock_path), (anchor_directory, anchor_path)] {
        let probe = directory.open_read_write(path)?;
        if daemon_lock_identity(&probe, path)? != *expected {
            return Err(format!(
                "daemon lock and persistent anchor have different identities: {}",
                lock_path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(any(unix, windows))]
pub(crate) fn repair_daemon_lock_anchor(
    lock_directory: &StableDirectory,
    lock_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    expected: &crate::fs_security::FileIdentity,
) -> Result<(), String> {
    repair_daemon_lock_anchor_with_guard(
        lock_directory,
        lock_path,
        anchor_directory,
        anchor_path,
        expected,
        || Ok(()),
    )
}

#[cfg(any(unix, windows))]
pub(crate) fn repair_daemon_lock_anchor_with_guard(
    lock_directory: &StableDirectory,
    lock_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    expected: &crate::fs_security::FileIdentity,
    mut namespace_guard: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    namespace_guard()?;
    if !lock_directory.path_exists(lock_path)? {
        let anchor = anchor_directory.open_read_write(anchor_path)?;
        namespace_guard()?;
        if daemon_lock_identity(&anchor, anchor_path)? != *expected {
            return Err(format!(
                "daemon lock anchor changed before visible-path repair: {}",
                anchor_path.display()
            ));
        }
        anchor_directory.hard_link_to_with_guard(
            anchor_path,
            lock_directory,
            lock_path,
            &mut namespace_guard,
        )?;
    }
    namespace_guard()?;
    let visible = lock_directory.open_read_write(lock_path)?;
    namespace_guard()?;
    if daemon_lock_identity(&visible, lock_path)? != *expected {
        return Err(format!(
            "daemon lock visible path changed before anchor repair: {}",
            lock_path.display()
        ));
    }
    namespace_guard()?;
    if !anchor_directory.path_exists(anchor_path)? {
        namespace_guard()?;
        lock_directory.hard_link_to_with_guard(
            lock_path,
            anchor_directory,
            anchor_path,
            &mut namespace_guard,
        )?;
    }
    namespace_guard()?;
    lock_directory.sync_directory()?;
    namespace_guard()?;
    anchor_directory.sync_directory()?;
    namespace_guard()?;
    verify_daemon_lock_paths_bound(
        lock_directory,
        lock_path,
        anchor_directory,
        anchor_path,
        expected,
    )?;
    namespace_guard()?;
    lock_directory.verify_visible()?;
    namespace_guard()?;
    anchor_directory.verify_visible()?;
    namespace_guard()
}

#[cfg(any(unix, windows))]
pub(crate) fn cleanup_daemon_lock_anchor(
    lock_directory: &StableDirectory,
    lock_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    expected: &crate::fs_security::FileIdentity,
    deadline: Option<Instant>,
) -> Result<(), String> {
    let mut deadline_guard = || ensure_daemon_lock_deadline(deadline, lock_path);
    deadline_guard()?;
    let visible = lock_directory.open_read_write(lock_path)?;
    if daemon_lock_identity(&visible, lock_path)? != *expected {
        return Err(format!(
            "daemon lock visible path changed before anchor cleanup: {}",
            lock_path.display()
        ));
    }
    deadline_guard()?;
    if anchor_directory.path_exists(anchor_path)? {
        let anchor = anchor_directory.open_read_write(anchor_path)?;
        if daemon_lock_identity(&anchor, anchor_path)? != *expected {
            return Err(format!(
                "daemon lock anchor changed before cleanup: {}",
                anchor_path.display()
            ));
        }
        anchor_directory.remove_file_if_matches_with_guard(
            anchor_path,
            &anchor,
            ".nib-daemon-lock-anchor-delete-",
            &mut deadline_guard,
        )?;
    }
    deadline_guard()?;
    lock_directory.verify_visible()?;
    anchor_directory.verify_visible()?;
    deadline_guard()
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn verify_daemon_lock_paths_bound(
    _lock_directory: &StableDirectory,
    lock_path: &Path,
    _anchor_directory: &StableDirectory,
    _anchor_path: &Path,
    _expected: &(),
) -> Result<(), String> {
    Err(format!(
        "stable daemon lock identity is unsupported on this platform: {}",
        lock_path.display()
    ))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn repair_daemon_lock_anchor(
    _lock_directory: &StableDirectory,
    lock_path: &Path,
    _anchor_directory: &StableDirectory,
    _anchor_path: &Path,
    _expected: &(),
) -> Result<(), String> {
    Err(format!(
        "stable daemon lock anchor repair is unsupported on this platform: {}",
        lock_path.display()
    ))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn repair_daemon_lock_anchor_with_guard(
    _lock_directory: &StableDirectory,
    lock_path: &Path,
    _anchor_directory: &StableDirectory,
    _anchor_path: &Path,
    _expected: &(),
    _namespace_guard: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    Err(format!(
        "stable daemon lock anchor repair is unsupported on this platform: {}",
        lock_path.display()
    ))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn cleanup_daemon_lock_anchor(
    _lock_directory: &StableDirectory,
    lock_path: &Path,
    _anchor_directory: &StableDirectory,
    _anchor_path: &Path,
    _expected: &(),
    _deadline: Option<Instant>,
) -> Result<(), String> {
    Err(format!(
        "stable daemon lock anchor cleanup is unsupported on this platform: {}",
        lock_path.display()
    ))
}
