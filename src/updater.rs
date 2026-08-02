use reqwest::blocking::{Client, Response};
use reqwest::redirect::{Attempt, Policy};
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, IsTerminal, Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use thiserror::Error;

const OFFICIAL_REPOSITORY: &str = "skills-yaml/nib";
const RELEASE_BASE_URL: &str = "https://github.com/skills-yaml/nib/releases/download/";
const RELEASE_MANIFEST: &str = "nib-release.json";
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_CHECKSUM_BYTES: usize = 4 * 1024;
const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;
const MAX_BINARY_BYTES: usize = 256 * 1024 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(1);
const UPDATE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const UPDATE_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_REDIRECTS: usize = 5;
const RELEASE_ARCHIVES: [&str; 4] = [
    "nib-linux-x86_64.tar.gz",
    "nib-macos-aarch64.tar.gz",
    "nib-macos-x86_64.tar.gz",
    "nib-windows-x86_64.zip",
];

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error(
        "this nib build is not self-update managed (channel: {channel}); reinstall from an official prod or development release"
    )]
    Unmanaged { channel: String },
    #[error("self-update is unsupported on this platform ({platform})")]
    UnsupportedPlatform { platform: String },
    #[error("invalid release metadata: {0}")]
    InvalidRelease(String),
    #[error("release request failed: {0}")]
    Network(String),
    #[error("release response exceeded the {limit}-byte limit")]
    ResponseTooLarge { limit: usize },
    #[error("another nib update is already in progress")]
    AlreadyRunning,
    #[error("installed executable cannot be updated safely: {0}")]
    UnsafeInstallation(String),
    #[error("update archive is invalid: {0}")]
    InvalidArchive(String),
    #[error("staged nib failed identity verification: {0}")]
    InvalidStagedBinary(String),
    #[error("update filesystem operation failed: {0}")]
    Filesystem(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleaseChannel {
    Prod,
    Development,
}

impl ReleaseChannel {
    fn from_embedded(value: &str) -> Option<Self> {
        match value {
            "prod" | "production" => Some(Self::Prod),
            "dev" | "development" => Some(Self::Development),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Prod => "prod",
            Self::Development => "development",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Self::Prod => "prod-latest",
            Self::Development => "development-latest",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BuildIdentity {
    channel: ReleaseChannel,
    version: String,
    commit: String,
}

impl BuildIdentity {
    fn display(&self) -> String {
        format!(
            "{} ({}, {})",
            self.version,
            self.channel.as_str(),
            short_commit(&self.commit)
        )
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize))]
#[serde(deny_unknown_fields)]
struct ReleaseAsset {
    sha256: String,
    size: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize))]
#[serde(deny_unknown_fields)]
struct ReleaseManifest {
    schema_version: u32,
    repository: String,
    channel: String,
    tag: String,
    version: String,
    commit: String,
    assets: BTreeMap<String, ReleaseAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Availability {
    Current(BuildIdentity),
    Available {
        current: BuildIdentity,
        latest: BuildIdentity,
    },
}

#[derive(Debug, Clone)]
struct Transport {
    base_url: Url,
    connect_timeout: Duration,
    total_timeout: Duration,
    allow_test_origin: bool,
}

impl Transport {
    fn startup() -> Result<Self, UpdateError> {
        Self::new(STARTUP_TIMEOUT, STARTUP_TIMEOUT)
    }

    fn update() -> Result<Self, UpdateError> {
        Self::new(UPDATE_CONNECT_TIMEOUT, UPDATE_TIMEOUT)
    }

    fn new(connect_timeout: Duration, total_timeout: Duration) -> Result<Self, UpdateError> {
        let mut allow_test_origin = false;
        let mut base = RELEASE_BASE_URL.to_string();
        if cfg!(debug_assertions) {
            if let Ok(value) = std::env::var("NIB_UPDATE_BASE_URL") {
                if !value.trim().is_empty() {
                    base = value;
                    allow_test_origin = true;
                }
            }
        }
        if !base.ends_with('/') {
            base.push('/');
        }
        let base_url = Url::parse(&base)
            .map_err(|_| UpdateError::InvalidRelease("invalid release base URL".to_string()))?;
        if !allow_test_origin && base_url.scheme() != "https" {
            return Err(UpdateError::InvalidRelease(
                "official release URL is not HTTPS".to_string(),
            ));
        }
        Ok(Self {
            base_url,
            connect_timeout,
            total_timeout,
            allow_test_origin,
        })
    }

    #[cfg(test)]
    fn for_test(base_url: Url, total_timeout: Duration) -> Self {
        Self {
            base_url,
            connect_timeout: total_timeout,
            total_timeout,
            allow_test_origin: true,
        }
    }

    fn fetch(
        &self,
        channel: ReleaseChannel,
        name: &str,
        limit: usize,
    ) -> Result<Vec<u8>, UpdateError> {
        if !is_safe_asset_name(name) {
            return Err(UpdateError::InvalidRelease(
                "unsafe release asset name".to_string(),
            ));
        }
        let url = self
            .base_url
            .join(&format!("{}/{name}", channel.tag()))
            .map_err(|_| UpdateError::InvalidRelease("invalid release asset URL".to_string()))?;
        let base_url = self.base_url.clone();
        let allow_test_origin = self.allow_test_origin;
        let redirect = Policy::custom(move |attempt: Attempt<'_>| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error(io::Error::other("too many release redirects"));
            }
            if allowed_release_url(attempt.url(), &base_url, allow_test_origin) {
                attempt.follow()
            } else {
                attempt.error(io::Error::other("release redirect left the allowed origin"))
            }
        });
        let client = Client::builder()
            .user_agent(format!("nib/{}", crate::version::package_version()))
            .connect_timeout(self.connect_timeout)
            .timeout(self.total_timeout)
            .redirect(redirect)
            .build()
            .map_err(|error| UpdateError::Network(error.to_string()))?;
        let response = client
            .get(url)
            .send()
            .map_err(|error| UpdateError::Network(error.to_string()))?;
        read_bounded_response(response, limit)
    }
}

pub fn run_update() -> Result<String, UpdateError> {
    let current = managed_current_identity()?;
    let transport = Transport::update()?;
    let manifest = fetch_manifest(&transport, current.channel)?;
    match classify(current, &manifest)? {
        Availability::Current(identity) => {
            Ok(format!("nib is already up to date: {}", identity.display()))
        }
        Availability::Available { current, latest } => {
            install_available_update(&transport, &manifest, &current, &latest)?;
            Ok(format!(
                "Updated nib: {} -> {}",
                current.display(),
                latest.display()
            ))
        }
    }
}

pub fn maybe_print_startup_notice() {
    if std::env::var_os("NIB_NO_UPDATE_CHECK").as_deref() == Some(std::ffi::OsStr::new("1")) {
        return;
    }
    let Ok(current) = managed_current_identity() else {
        return;
    };
    let Ok(transport) = Transport::startup() else {
        return;
    };
    let Ok(manifest) = fetch_manifest(&transport, current.channel) else {
        return;
    };
    let Ok(availability) = classify(current, &manifest) else {
        return;
    };
    if let Some(notice) = startup_notice(&availability) {
        if io::stderr().is_terminal() {
            eprintln!("{notice}");
        }
    }
}

fn startup_notice(availability: &Availability) -> Option<String> {
    match availability {
        Availability::Current(_) => None,
        Availability::Available { latest, .. } => Some(format!(
            "[nib] Channel update available: {}. Run `nib update`.",
            latest.display()
        )),
    }
}

fn managed_current_identity() -> Result<BuildIdentity, UpdateError> {
    let raw_channel = crate::version::build_channel();
    let Some(channel) = ReleaseChannel::from_embedded(raw_channel) else {
        return Err(UpdateError::Unmanaged {
            channel: raw_channel.to_string(),
        });
    };
    let commit = crate::version::build_commit();
    if !is_lower_hex(commit, 40) {
        return Err(UpdateError::Unmanaged {
            channel: raw_channel.to_string(),
        });
    }
    let version = crate::version::package_version();
    if !is_valid_version(version) {
        return Err(UpdateError::Unmanaged {
            channel: raw_channel.to_string(),
        });
    }
    if current_asset_name().is_none() {
        return Err(UpdateError::UnsupportedPlatform {
            platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        });
    }
    Ok(BuildIdentity {
        channel,
        version: version.to_string(),
        commit: commit.to_string(),
    })
}

fn fetch_manifest(
    transport: &Transport,
    channel: ReleaseChannel,
) -> Result<ReleaseManifest, UpdateError> {
    let bytes = transport.fetch(channel, RELEASE_MANIFEST, MAX_MANIFEST_BYTES)?;
    parse_manifest(&bytes, channel)
}

fn parse_manifest(
    bytes: &[u8],
    expected_channel: ReleaseChannel,
) -> Result<ReleaseManifest, UpdateError> {
    let manifest: ReleaseManifest = serde_json::from_slice(bytes).map_err(|_| {
        UpdateError::InvalidRelease("manifest is not valid strict JSON".to_string())
    })?;
    if manifest.schema_version != 1 {
        return Err(UpdateError::InvalidRelease(
            "unsupported manifest schema".to_string(),
        ));
    }
    if manifest.repository != OFFICIAL_REPOSITORY {
        return Err(UpdateError::InvalidRelease(
            "manifest repository does not match nib".to_string(),
        ));
    }
    if manifest.channel != expected_channel.as_str() || manifest.tag != expected_channel.tag() {
        return Err(UpdateError::InvalidRelease(
            "manifest channel or tag does not match the installed channel".to_string(),
        ));
    }
    if !is_valid_version(&manifest.version) {
        return Err(UpdateError::InvalidRelease(
            "manifest package version is invalid".to_string(),
        ));
    }
    if !is_lower_hex(&manifest.commit, 40) {
        return Err(UpdateError::InvalidRelease(
            "manifest commit is not a lowercase 40-hex SHA".to_string(),
        ));
    }
    if manifest.assets.len() != RELEASE_ARCHIVES.len()
        || RELEASE_ARCHIVES
            .iter()
            .any(|name| !manifest.assets.contains_key(*name))
    {
        return Err(UpdateError::InvalidRelease(
            "manifest does not contain the exact supported archive set".to_string(),
        ));
    }
    for (name, asset) in &manifest.assets {
        if !RELEASE_ARCHIVES.contains(&name.as_str())
            || !is_lower_hex(&asset.sha256, 64)
            || asset.size == 0
            || asset.size > MAX_ARCHIVE_BYTES as u64
        {
            return Err(UpdateError::InvalidRelease(format!(
                "invalid metadata for release asset {name}"
            )));
        }
    }
    Ok(manifest)
}

fn classify(
    current: BuildIdentity,
    manifest: &ReleaseManifest,
) -> Result<Availability, UpdateError> {
    let latest = BuildIdentity {
        channel: current.channel,
        version: manifest.version.clone(),
        commit: manifest.commit.clone(),
    };
    if current.commit == latest.commit {
        if current.version != latest.version {
            return Err(UpdateError::InvalidRelease(
                "the current commit has conflicting package versions".to_string(),
            ));
        }
        Ok(Availability::Current(current))
    } else {
        Ok(Availability::Available { current, latest })
    }
}

fn install_available_update(
    transport: &Transport,
    manifest: &ReleaseManifest,
    current: &BuildIdentity,
    latest: &BuildIdentity,
) -> Result<(), UpdateError> {
    let target = std::env::current_exe().map_err(|error| {
        UpdateError::UnsafeInstallation(format!("cannot resolve current executable: {error}"))
    })?;
    validate_target_path(&target)?;
    let parent = target.parent().ok_or_else(|| {
        UpdateError::UnsafeInstallation("current executable has no parent directory".to_string())
    })?;
    let lock_path = parent.join(".nib-update.lock");
    reject_link(&lock_path, false)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| UpdateError::Filesystem(format!("cannot open update lock: {error}")))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Err(UpdateError::AlreadyRunning),
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(UpdateError::Filesystem(format!(
                "cannot acquire update lock: {error}"
            )))
        }
    }

    let initial_identity = same_file::Handle::from_path(&target).map_err(|error| {
        UpdateError::UnsafeInstallation(format!("cannot identify current executable: {error}"))
    })?;
    let asset_name = current_asset_name().ok_or_else(|| UpdateError::UnsupportedPlatform {
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    })?;
    let asset = manifest.assets.get(asset_name).ok_or_else(|| {
        UpdateError::InvalidRelease("manifest omitted this platform archive".to_string())
    })?;
    let archive = transport.fetch(current.channel, asset_name, MAX_ARCHIVE_BYTES)?;
    if archive.len() as u64 != asset.size {
        return Err(UpdateError::InvalidArchive(
            "downloaded archive size does not match the manifest".to_string(),
        ));
    }
    let checksum_name = format!("{asset_name}.sha256");
    let checksum = transport.fetch(current.channel, &checksum_name, MAX_CHECKSUM_BYTES)?;
    let checksum_digest = parse_checksum(&checksum, asset_name)?;
    let archive_digest = hex_sha256(&archive);
    if checksum_digest != asset.sha256 || archive_digest != asset.sha256 {
        return Err(UpdateError::InvalidArchive(
            "manifest, checksum asset, and archive digest do not agree".to_string(),
        ));
    }

    let binary = extract_binary(asset_name, &archive)?;
    let staging = tempfile::Builder::new()
        .prefix(".nib-update-")
        .tempdir_in(parent)
        .map_err(|error| {
            UpdateError::Filesystem(format!("cannot create staging directory: {error}"))
        })?;
    let staged_path =
        staging.path().join(target.file_name().ok_or_else(|| {
            UpdateError::UnsafeInstallation("invalid executable name".to_string())
        })?);
    write_staged_binary(&staged_path, &binary)?;
    verify_staged_binary(&staged_path, latest)?;

    validate_target_path(&target)?;
    let final_identity = same_file::Handle::from_path(&target).map_err(|error| {
        UpdateError::UnsafeInstallation(format!("cannot re-identify current executable: {error}"))
    })?;
    if initial_identity != final_identity {
        return Err(UpdateError::UnsafeInstallation(
            "current executable changed during update".to_string(),
        ));
    }
    drop(initial_identity);
    drop(final_identity);
    replace_executable(&staged_path, &target)?;
    sync_parent(parent)?;
    drop(lock);
    Ok(())
}

fn validate_target_path(target: &Path) -> Result<(), UpdateError> {
    let metadata = fs::symlink_metadata(target).map_err(|error| {
        UpdateError::UnsafeInstallation(format!("cannot inspect current executable: {error}"))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(UpdateError::UnsafeInstallation(
            "current executable is not a regular file".to_string(),
        ));
    }
    Ok(())
}

fn reject_link(path: &Path, must_exist: bool) -> Result<(), UpdateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(UpdateError::UnsafeInstallation(format!(
                "{} is a symbolic link",
                path.file_name().unwrap_or_default().to_string_lossy()
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if !must_exist && error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(UpdateError::UnsafeInstallation(format!(
            "cannot inspect installation path: {error}"
        ))),
    }
}

fn parse_checksum(bytes: &[u8], asset_name: &str) -> Result<String, UpdateError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| UpdateError::InvalidArchive("checksum is not UTF-8".to_string()))?;
    let line = text.trim_end_matches(['\r', '\n']);
    if line.contains(['\r', '\n']) {
        return Err(UpdateError::InvalidArchive(
            "checksum contains multiple lines".to_string(),
        ));
    }
    let (digest, name) = line
        .split_once("  ")
        .ok_or_else(|| UpdateError::InvalidArchive("checksum has an invalid format".to_string()))?;
    if !is_lower_hex(digest, 64) || name != asset_name {
        return Err(UpdateError::InvalidArchive(
            "checksum digest or archive name is invalid".to_string(),
        ));
    }
    Ok(digest.to_string())
}

#[cfg(unix)]
fn extract_binary(asset_name: &str, archive_bytes: &[u8]) -> Result<Vec<u8>, UpdateError> {
    if !asset_name.ends_with(".tar.gz") {
        return Err(UpdateError::InvalidArchive(
            "unexpected archive format for Unix".to_string(),
        ));
    }
    let decoder = flate2::read::GzDecoder::new(Cursor::new(archive_bytes));
    let mut archive = tar::Archive::new(decoder);
    let mut result = None;
    let entries = archive
        .entries()
        .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
        let path = entry
            .path()
            .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
        if path.as_ref() != Path::new("nib")
            || !entry.header().entry_type().is_file()
            || result.is_some()
        {
            return Err(UpdateError::InvalidArchive(
                "archive must contain exactly one regular file named nib".to_string(),
            ));
        }
        if entry.size() == 0 || entry.size() > MAX_BINARY_BYTES as u64 {
            return Err(UpdateError::InvalidArchive(
                "expanded binary size is invalid".to_string(),
            ));
        }
        let expected_size = entry.size();
        let mut binary = Vec::with_capacity(expected_size as usize);
        entry
            .take(MAX_BINARY_BYTES as u64 + 1)
            .read_to_end(&mut binary)
            .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
        if binary.len() as u64 != expected_size || binary.len() > MAX_BINARY_BYTES {
            return Err(UpdateError::InvalidArchive(
                "expanded binary length does not match the archive".to_string(),
            ));
        }
        result = Some(binary);
    }
    result.ok_or_else(|| UpdateError::InvalidArchive("archive contains no nib binary".to_string()))
}

#[cfg(windows)]
fn extract_binary(asset_name: &str, archive_bytes: &[u8]) -> Result<Vec<u8>, UpdateError> {
    if !asset_name.ends_with(".zip") {
        return Err(UpdateError::InvalidArchive(
            "unexpected archive format for Windows".to_string(),
        ));
    }
    let reader = Cursor::new(archive_bytes);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
    if archive.len() != 1 {
        return Err(UpdateError::InvalidArchive(
            "archive must contain exactly one file".to_string(),
        ));
    }
    let entry = archive
        .by_index(0)
        .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
    if entry.is_dir()
        || entry.name() != "nib.exe"
        || entry.enclosed_name().as_deref() != Some(Path::new("nib.exe"))
        || entry.size() == 0
        || entry.size() > MAX_BINARY_BYTES as u64
    {
        return Err(UpdateError::InvalidArchive(
            "archive must contain exactly one regular file named nib.exe".to_string(),
        ));
    }
    let expected_size = entry.size();
    let mut binary = Vec::with_capacity(expected_size as usize);
    entry
        .take(MAX_BINARY_BYTES as u64 + 1)
        .read_to_end(&mut binary)
        .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
    if binary.len() as u64 != expected_size || binary.len() > MAX_BINARY_BYTES {
        return Err(UpdateError::InvalidArchive(
            "expanded binary length does not match the archive".to_string(),
        ));
    }
    Ok(binary)
}

#[cfg(not(any(unix, windows)))]
fn extract_binary(_asset_name: &str, _archive_bytes: &[u8]) -> Result<Vec<u8>, UpdateError> {
    Err(UpdateError::UnsupportedPlatform {
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    })
}

fn write_staged_binary(path: &Path, binary: &[u8]) -> Result<(), UpdateError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o755);
    }
    let mut file = options.open(path).map_err(|error| {
        UpdateError::Filesystem(format!("cannot create staged binary: {error}"))
    })?;
    file.write_all(binary)
        .and_then(|_| file.sync_all())
        .map_err(|error| UpdateError::Filesystem(format!("cannot persist staged binary: {error}")))
}

fn verify_staged_binary(path: &Path, expected: &BuildIdentity) -> Result<(), UpdateError> {
    let output = Command::new(path)
        .arg("version")
        .env("NIB_NO_UPDATE_CHECK", "1")
        .output()
        .map_err(|error| UpdateError::InvalidStagedBinary(error.to_string()))?;
    if !output.status.success() {
        return Err(UpdateError::InvalidStagedBinary(
            "the staged executable returned a failure status".to_string(),
        ));
    }
    let expected_output = format!(
        "nib {} ({} - {})",
        expected.version,
        expected.channel.as_str(),
        expected.commit
    );
    let actual = std::str::from_utf8(&output.stdout)
        .map_err(|_| UpdateError::InvalidStagedBinary("version output is not UTF-8".to_string()))?
        .trim();
    if actual != expected_output {
        return Err(UpdateError::InvalidStagedBinary(
            "embedded version, channel, or commit does not match the release".to_string(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn replace_executable(staged: &Path, target: &Path) -> Result<(), UpdateError> {
    fs::rename(staged, target)
        .map_err(|error| UpdateError::Filesystem(format!("cannot replace executable: {error}")))
}

#[cfg(windows)]
fn replace_executable(staged: &Path, target: &Path) -> Result<(), UpdateError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let staged_wide: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
    let target_wide: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let status = unsafe {
        MoveFileExW(
            staged_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if status == 0 {
        Err(UpdateError::Filesystem(format!(
            "cannot replace executable: {}",
            io::Error::last_os_error()
        )))
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn replace_executable(_staged: &Path, _target: &Path) -> Result<(), UpdateError> {
    Err(UpdateError::UnsupportedPlatform {
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    })
}

fn sync_parent(parent: &Path) -> Result<(), UpdateError> {
    #[cfg(unix)]
    {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                UpdateError::Filesystem(format!("cannot sync install directory: {error}"))
            })?;
    }
    #[cfg(not(unix))]
    let _ = parent;
    Ok(())
}

fn read_bounded_response(response: Response, limit: usize) -> Result<Vec<u8>, UpdateError> {
    if !response.status().is_success() {
        return Err(UpdateError::Network(format!(
            "HTTP status {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(UpdateError::ResponseTooLarge { limit });
    }
    let mut bytes =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
    response
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| UpdateError::Network(error.to_string()))?;
    if bytes.len() > limit {
        return Err(UpdateError::ResponseTooLarge { limit });
    }
    Ok(bytes)
}

fn allowed_release_url(url: &Url, base: &Url, allow_test_origin: bool) -> bool {
    if allow_test_origin {
        return url.scheme() == base.scheme()
            && url.host_str() == base.host_str()
            && url.port_or_known_default() == base.port_or_known_default();
    }
    if url.scheme() != "https" {
        return false;
    }
    matches!(
        url.host_str(),
        Some("github.com")
            | Some("objects.githubusercontent.com")
            | Some("release-assets.githubusercontent.com")
            | Some("github-releases.githubusercontent.com")
    )
}

fn current_asset_name() -> Option<&'static str> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Some("nib-linux-x86_64.tar.gz");
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return Some("nib-macos-x86_64.tar.gz");
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Some("nib-macos-aarch64.tar.gz");
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        return Some("nib-windows-x86_64.zip");
    }
    #[allow(unreachable_code)]
    None
}

fn is_safe_asset_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.contains(['/', '\\'])
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn is_valid_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'))
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn short_commit(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn manifest(channel: ReleaseChannel, commit: &str) -> Vec<u8> {
        let assets = RELEASE_ARCHIVES
            .iter()
            .map(|name| {
                (
                    (*name).to_string(),
                    ReleaseAsset {
                        sha256: "a".repeat(64),
                        size: 10,
                    },
                )
            })
            .collect();
        serde_json::to_vec(&ReleaseManifest {
            schema_version: 1,
            repository: OFFICIAL_REPOSITORY.to_string(),
            channel: channel.as_str().to_string(),
            tag: channel.tag().to_string(),
            version: "0.1.0".to_string(),
            commit: commit.to_string(),
            assets,
        })
        .expect("manifest JSON")
    }

    #[test]
    fn strict_manifest_distinguishes_current_and_available_builds() {
        let current_commit = "1".repeat(40);
        let latest_commit = "2".repeat(40);
        let current = BuildIdentity {
            channel: ReleaseChannel::Prod,
            version: "0.1.0".to_string(),
            commit: current_commit.clone(),
        };
        let current_manifest = parse_manifest(
            &manifest(ReleaseChannel::Prod, &current_commit),
            ReleaseChannel::Prod,
        )
        .expect("current manifest");
        assert!(matches!(
            classify(current.clone(), &current_manifest),
            Ok(Availability::Current(_))
        ));
        let next_manifest = parse_manifest(
            &manifest(ReleaseChannel::Prod, &latest_commit),
            ReleaseChannel::Prod,
        )
        .expect("next manifest");
        assert!(matches!(
            classify(current, &next_manifest),
            Ok(Availability::Available { .. })
        ));
        let available = classify(
            BuildIdentity {
                channel: ReleaseChannel::Prod,
                version: "0.1.0".to_string(),
                commit: current_commit,
            },
            &next_manifest,
        )
        .expect("available update");
        assert_eq!(
            startup_notice(&available).as_deref(),
            Some("[nib] Channel update available: 0.1.0 (prod, 2222222). Run `nib update`.")
        );
        assert!(startup_notice(&Availability::Current(BuildIdentity {
            channel: ReleaseChannel::Prod,
            version: "0.1.0".to_string(),
            commit: latest_commit,
        }))
        .is_none());
    }

    #[test]
    fn strict_manifest_rejects_unknown_fields_and_wrong_channel() {
        let mut value: serde_json::Value =
            serde_json::from_slice(&manifest(ReleaseChannel::Prod, &"1".repeat(40)))
                .expect("manifest value");
        value["unexpected"] = serde_json::json!(true);
        assert!(
            parse_manifest(&serde_json::to_vec(&value).unwrap(), ReleaseChannel::Prod).is_err()
        );
        assert!(parse_manifest(
            &manifest(ReleaseChannel::Development, &"1".repeat(40)),
            ReleaseChannel::Prod
        )
        .is_err());
    }

    #[test]
    fn checksum_requires_exact_digest_and_archive_name() {
        let digest = "a".repeat(64);
        let valid = format!("{digest}  nib-linux-x86_64.tar.gz\n");
        assert_eq!(
            parse_checksum(valid.as_bytes(), "nib-linux-x86_64.tar.gz").unwrap(),
            digest
        );
        assert!(parse_checksum(valid.as_bytes(), "nib-macos-x86_64.tar.gz").is_err());
        assert!(parse_checksum(
            format!("{} *nib-linux-x86_64.tar.gz", "a".repeat(64)).as_bytes(),
            "nib-linux-x86_64.tar.gz"
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn tar_extraction_accepts_only_one_regular_nib_binary() {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let mut encoded = Vec::new();
        {
            let encoder = GzEncoder::new(&mut encoded, Compression::default());
            let mut builder = tar::Builder::new(encoder);
            let bytes = b"binary";
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "nib", &bytes[..])
                .expect("append binary");
            builder
                .into_inner()
                .expect("finish tar")
                .finish()
                .expect("finish gzip");
        }
        assert_eq!(
            extract_binary("nib-linux-x86_64.tar.gz", &encoded).unwrap(),
            b"binary"
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_replacement_commits_one_complete_file() {
        let directory = tempfile::tempdir().expect("replacement directory");
        let target = directory.path().join("nib");
        let staged = directory.path().join("nib.staged");
        fs::write(&target, b"old").expect("old binary");
        fs::write(&staged, b"new").expect("staged binary");

        replace_executable(&staged, &target).expect("replace binary");
        sync_parent(directory.path()).expect("sync replacement");

        assert_eq!(fs::read(&target).expect("new target"), b"new");
        assert!(!staged.exists());
    }

    #[test]
    fn bounded_transport_fetches_a_local_manifest() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("local server");
        let address = listener.local_addr().expect("server address");
        let body = manifest(ReleaseChannel::Prod, &"1".repeat(40));
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request");
            consume_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("response headers");
            stream.write_all(&body).expect("response body");
        });
        let base = Url::parse(&format!("http://{address}/")).expect("base URL");
        let transport = Transport::for_test(base, Duration::from_secs(2));
        let fetched = fetch_manifest(&transport, ReleaseChannel::Prod).expect("manifest fetch");
        assert_eq!(fetched.commit, "1".repeat(40));
        server.join().expect("server");
    }

    fn consume_request(stream: &mut TcpStream) {
        let mut bytes = [0u8; 2048];
        let _ = stream.read(&mut bytes).expect("read request");
    }

    #[test]
    fn local_build_metadata_is_unmanaged_and_safe() {
        assert!(!crate::version::build_commit().is_empty());
        assert!(!crate::version::build_channel().is_empty());
        if ReleaseChannel::from_embedded(crate::version::build_channel()).is_none() {
            assert!(matches!(
                managed_current_identity(),
                Err(UpdateError::Unmanaged { .. })
            ));
        }
    }
}
