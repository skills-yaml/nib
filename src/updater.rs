use clap::{Args, ValueEnum};
use reqwest::blocking::{Client, Response};
use reqwest::redirect::{Attempt, Policy};
use reqwest::Url;
use serde::Deserialize;
#[cfg(windows)]
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
#[cfg(windows)]
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, IsTerminal, Read, Write};
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::process::Command;
#[cfg(windows)]
use std::process::Stdio;
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;
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
#[cfg(windows)]
const WINDOWS_CLEANUP_REQUEST_ENV: &str = "NIB_WINDOWS_UPDATE_CLEANUP_REQUEST";
#[cfg(windows)]
const WINDOWS_FINALIZE_REQUEST_ENV: &str = "NIB_WINDOWS_UPDATE_FINALIZE_REQUEST";
#[cfg(windows)]
const WINDOWS_FINALIZE_WORKER_PID_ENV: &str = "NIB_WINDOWS_UPDATE_FINALIZE_WORKER_PID";
#[cfg(windows)]
const WINDOWS_CLEANUP_HELPER: &str = ".nib-update-cleanup.exe";
#[cfg(windows)]
const WINDOWS_CLEANUP_REQUEST: &str = "cleanup-request.json";
#[cfg(windows)]
const WINDOWS_CLEANUP_READY: &str = "cleanup.ready";
#[cfg(windows)]
const WINDOWS_FINALIZER_READY: &str = "finalizer.ready";
#[cfg(windows)]
const WINDOWS_WORKER_READY_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(windows)]
const WINDOWS_PARENT_EXIT_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(windows)]
const WINDOWS_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);
const RELEASE_ARCHIVES: [&str; 4] = [
    "nib-linux-x86_64.tar.gz",
    "nib-macos-aarch64.tar.gz",
    "nib-macos-x86_64.tar.gz",
    "nib-windows-x86_64.zip",
];

#[derive(Debug, Clone, Args)]
pub struct UpdateArgs {
    /// Switch to a release channel before following it for future updates
    #[arg(long, value_enum)]
    pub channel: Option<UpdateChannel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UpdateChannel {
    #[value(alias = "production")]
    Prod,
    #[value(alias = "dev")]
    Development,
}

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
    #[cfg(windows)]
    #[error(
        "a prior Windows update cleanup has not converged; wait and retry or rerun the official installer"
    )]
    PendingWindowsCleanup,
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

impl From<UpdateChannel> for ReleaseChannel {
    fn from(value: UpdateChannel) -> Self {
        match value {
            UpdateChannel::Prod => Self::Prod,
            UpdateChannel::Development => Self::Development,
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

#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsCleanupState {
    PublishedWithBackup,
    PublishedClean,
    RestoreBackup,
    RolledBack,
    Ambiguous,
}

#[cfg(any(windows, test))]
fn classify_windows_cleanup_state(
    target_digest: Option<&str>,
    backup_digest: Option<&str>,
    old_digest: &str,
    candidate_digest: &str,
) -> WindowsCleanupState {
    match (target_digest, backup_digest) {
        (Some(target), Some(backup)) if target == candidate_digest && backup == old_digest => {
            WindowsCleanupState::PublishedWithBackup
        }
        (Some(target), None) if target == candidate_digest => WindowsCleanupState::PublishedClean,
        (None, Some(backup)) if backup == old_digest => WindowsCleanupState::RestoreBackup,
        (Some(target), None) if target == old_digest => WindowsCleanupState::RolledBack,
        _ => WindowsCleanupState::Ambiguous,
    }
}

#[cfg(any(windows, test))]
fn commit_windows_candidate(
    staged: &Path,
    target: &Path,
    backup: &Path,
    mut move_file: impl FnMut(&Path, &Path) -> Result<(), UpdateError>,
    verify_published: impl FnOnce() -> Result<(), UpdateError>,
) -> Result<(), UpdateError> {
    move_file(target, backup)?;
    if let Err(publish_error) = move_file(staged, target) {
        return match move_file(backup, target) {
            Ok(()) => Err(publish_error),
            Err(rollback_error) => Err(UpdateError::Filesystem(format!(
                "candidate publication failed ({publish_error}); backup rollback failed ({rollback_error})"
            ))),
        };
    }

    if let Err(publication_error) = verify_published() {
        let rollback_result = move_file(target, staged).and_then(|()| move_file(backup, target));
        return match rollback_result {
            Ok(()) => Err(publication_error),
            Err(rollback_error) => Err(UpdateError::Filesystem(format!(
                "published candidate revalidation failed ({publication_error}); backup rollback failed ({rollback_error})"
            ))),
        };
    }
    Ok(())
}

#[cfg(windows)]
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WindowsCleanupRequest {
    schema_version: u32,
    parent_pid: u32,
    nonce: String,
    target_name: Vec<u16>,
    staged_name: Vec<u16>,
    backup_name: Vec<u16>,
    old_sha256: String,
    candidate_sha256: String,
}

#[cfg(windows)]
struct WindowsCleanupPaths {
    request_path: PathBuf,
    staging: PathBuf,
    target: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
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

pub fn run_update(args: &UpdateArgs) -> Result<String, UpdateError> {
    let current = managed_current_identity()?;
    let target_channel = args
        .channel
        .map(ReleaseChannel::from)
        .unwrap_or(current.channel);
    let transport = Transport::update()?;
    let manifest = fetch_manifest(&transport, target_channel)?;
    match classify(current, target_channel, &manifest)? {
        Availability::Current(identity) => {
            Ok(format!("nib is already up to date: {}", identity.display()))
        }
        Availability::Available { current, latest } => {
            install_available_update(&transport, &manifest, &latest)?;
            Ok(completed_update_message(&current, &latest))
        }
    }
}

fn completed_update_message(current: &BuildIdentity, latest: &BuildIdentity) -> String {
    let action = if current.channel != latest.channel {
        "Switched nib channel"
    } else {
        "Updated nib"
    };
    format!("{action}: {} -> {}", current.display(), latest.display())
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
    let channel = current.channel;
    let Ok(availability) = classify(current, channel, &manifest) else {
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
            "manifest channel or tag does not match the selected channel".to_string(),
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
    target_channel: ReleaseChannel,
    manifest: &ReleaseManifest,
) -> Result<Availability, UpdateError> {
    let latest = BuildIdentity {
        channel: target_channel,
        version: manifest.version.clone(),
        commit: manifest.commit.clone(),
    };
    if current.commit == latest.commit {
        if current.version != latest.version {
            return Err(UpdateError::InvalidRelease(
                "the current commit has conflicting package versions".to_string(),
            ));
        }
        if current.channel == latest.channel {
            return Ok(Availability::Current(current));
        }
    }
    Ok(Availability::Available { current, latest })
}

fn install_available_update(
    transport: &Transport,
    manifest: &ReleaseManifest,
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
    #[cfg(windows)]
    reject_pending_windows_cleanup(parent)?;

    let initial_identity = same_file::Handle::from_path(&target).map_err(|error| {
        UpdateError::UnsafeInstallation(format!("cannot identify current executable: {error}"))
    })?;
    let asset_name = current_asset_name().ok_or_else(|| UpdateError::UnsupportedPlatform {
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    })?;
    let archive = fetch_verified_archive(transport, manifest, latest, asset_name)?;

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
    drop(final_identity);
    #[cfg(windows)]
    replace_executable_windows(staging, &staged_path, &target, latest, initial_identity)?;
    #[cfg(not(windows))]
    {
        drop(initial_identity);
        replace_executable(&staged_path, &target)?;
    }
    sync_parent(parent)?;
    drop(lock);
    Ok(())
}

fn fetch_verified_archive(
    transport: &Transport,
    manifest: &ReleaseManifest,
    latest: &BuildIdentity,
    asset_name: &str,
) -> Result<Vec<u8>, UpdateError> {
    let asset = manifest.assets.get(asset_name).ok_or_else(|| {
        UpdateError::InvalidRelease("manifest omitted this platform archive".to_string())
    })?;
    let archive = transport.fetch(latest.channel, asset_name, MAX_ARCHIVE_BYTES)?;
    if archive.len() as u64 != asset.size {
        return Err(UpdateError::InvalidArchive(
            "downloaded archive size does not match the manifest".to_string(),
        ));
    }
    let checksum_name = format!("{asset_name}.sha256");
    let checksum = transport.fetch(latest.channel, &checksum_name, MAX_CHECKSUM_BYTES)?;
    let checksum_digest = parse_checksum(&checksum, asset_name)?;
    let archive_digest = hex_sha256(&archive);
    if checksum_digest != asset.sha256 || archive_digest != asset.sha256 {
        return Err(UpdateError::InvalidArchive(
            "manifest, checksum asset, and archive digest do not agree".to_string(),
        ));
    }
    Ok(archive)
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

#[cfg(windows)]
fn reject_pending_windows_cleanup(parent: &Path) -> Result<(), UpdateError> {
    let entries = fs::read_dir(parent).map_err(|error| {
        UpdateError::Filesystem(format!("cannot inspect installation directory: {error}"))
    })?;
    for (index, entry) in entries.enumerate() {
        if index >= 1024 {
            return Err(UpdateError::UnsafeInstallation(
                "installation directory contains too many entries to validate safely".to_string(),
            ));
        }
        let entry = entry.map_err(|error| {
            UpdateError::Filesystem(format!("cannot inspect installation entry: {error}"))
        })?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".nib-update-")
        {
            return Err(UpdateError::PendingWindowsCleanup);
        }
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
    let mut command = Command::new(path);
    command.arg("version").env("NIB_NO_UPDATE_CHECK", "1");
    #[cfg(windows)]
    command
        .env_remove(WINDOWS_CLEANUP_REQUEST_ENV)
        .env_remove(WINDOWS_FINALIZE_REQUEST_ENV)
        .env_remove(WINDOWS_FINALIZE_WORKER_PID_ENV);
    let output = command
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
fn move_file_windows(source: &Path, target: &Path) -> Result<(), UpdateError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
    let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target_wide: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let status = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if status == 0 {
        Err(UpdateError::Filesystem(format!(
            "cannot move update file: {}",
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

#[cfg(windows)]
fn replace_executable_windows(
    staging: tempfile::TempDir,
    staged: &Path,
    target: &Path,
    expected: &BuildIdentity,
    initial_identity: same_file::Handle,
) -> Result<(), UpdateError> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    let parent = target.parent().ok_or_else(|| {
        UpdateError::UnsafeInstallation("current executable has no parent directory".to_string())
    })?;
    let old_sha256 = hex_sha256_file(target)?;
    let candidate_sha256 = hex_sha256_file(staged)?;
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let backup = parent.join(format!(".nib-update-previous-{nonce}.exe"));
    reject_link(&backup, false)?;
    let helper = staging.path().join(WINDOWS_CLEANUP_HELPER);
    copy_and_sync(staged, &helper)?;

    let target_name = target
        .file_name()
        .ok_or_else(|| UpdateError::UnsafeInstallation("invalid executable name".to_string()))?;
    let request = WindowsCleanupRequest {
        schema_version: 1,
        parent_pid: std::process::id(),
        nonce,
        target_name: target_name.encode_wide().collect(),
        staged_name: target_name.encode_wide().collect(),
        backup_name: backup
            .file_name()
            .expect("backup has a file name")
            .encode_wide()
            .collect(),
        old_sha256,
        candidate_sha256: candidate_sha256.clone(),
    };
    let request_path = staging.path().join(WINDOWS_CLEANUP_REQUEST);
    write_windows_cleanup_request(&request_path, &request)?;

    let mut worker = Command::new(&helper);
    worker
        .env(WINDOWS_CLEANUP_REQUEST_ENV, &request_path)
        .env_remove(WINDOWS_FINALIZE_REQUEST_ENV)
        .env_remove(WINDOWS_FINALIZE_WORKER_PID_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    let mut worker = worker.spawn().map_err(|error| {
        UpdateError::Filesystem(format!(
            "cannot start Windows update cleanup worker: {error}"
        ))
    })?;
    let staging_path = staging.keep();
    if let Err(error) = wait_for_windows_worker_ready(&staging_path, &request.nonce, &mut worker) {
        let _ = worker.kill();
        let _ = worker.wait();
        if let Ok((cleanup_request, cleanup_paths)) = load_windows_cleanup_request(&request_path) {
            let _ = remove_windows_staging(&cleanup_request, &cleanup_paths, false);
        }
        return Err(error);
    }

    validate_target_path(target)?;
    let commit_identity = same_file::Handle::from_path(target).map_err(|error| {
        UpdateError::UnsafeInstallation(format!(
            "cannot identify current executable at the Windows commit point: {error}"
        ))
    })?;
    if initial_identity != commit_identity || hex_sha256_file(target)? != request.old_sha256 {
        return Err(UpdateError::UnsafeInstallation(
            "current executable changed before Windows replacement".to_string(),
        ));
    }
    drop(initial_identity);
    drop(commit_identity);

    commit_windows_candidate(staged, target, &backup, retry_windows_move, || {
        let published_sha256 = hex_sha256_file(target)?;
        if published_sha256 != candidate_sha256 {
            return Err(UpdateError::Filesystem(
                "published executable digest does not match the verified candidate".to_string(),
            ));
        }
        verify_staged_binary(target, expected)
    })
}

#[cfg(windows)]
pub fn run_windows_update_worker_if_requested() -> Option<i32> {
    let cleanup_request = std::env::var_os(WINDOWS_CLEANUP_REQUEST_ENV);
    let finalize_request = std::env::var_os(WINDOWS_FINALIZE_REQUEST_ENV);
    let finalize_worker_pid = std::env::var_os(WINDOWS_FINALIZE_WORKER_PID_ENV);
    match (cleanup_request, finalize_request, finalize_worker_pid) {
        (None, None, None) => None,
        (Some(request), None, None) => {
            Some(if run_windows_cleanup_worker(Path::new(&request)).is_ok() {
                0
            } else {
                70
            })
        }
        (None, Some(request), Some(worker_pid)) => Some(
            match worker_pid
                .to_str()
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|pid| *pid != 0)
                .ok_or(())
                .and_then(|pid| run_windows_finalizer(Path::new(&request), pid).map_err(|_| ()))
            {
                Ok(()) => 0,
                Err(()) => 70,
            },
        ),
        _ => Some(70),
    }
}

#[cfg(windows)]
fn copy_and_sync(source: &Path, target: &Path) -> Result<(), UpdateError> {
    let mut source_file = OpenOptions::new()
        .read(true)
        .open(source)
        .map_err(|error| {
            UpdateError::Filesystem(format!("cannot open verified candidate: {error}"))
        })?;
    let mut target_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)
        .map_err(|error| {
            UpdateError::Filesystem(format!("cannot create Windows cleanup worker: {error}"))
        })?;
    io::copy(&mut source_file, &mut target_file)
        .and_then(|_| target_file.sync_all())
        .map_err(|error| {
            UpdateError::Filesystem(format!("cannot persist Windows cleanup worker: {error}"))
        })?;
    Ok(())
}

#[cfg(windows)]
fn write_windows_cleanup_request(
    path: &Path,
    request: &WindowsCleanupRequest,
) -> Result<(), UpdateError> {
    let bytes = serde_json::to_vec(request).map_err(|error| {
        UpdateError::Filesystem(format!("cannot encode Windows cleanup request: {error}"))
    })?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            UpdateError::Filesystem(format!("cannot create Windows cleanup request: {error}"))
        })?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| {
            UpdateError::Filesystem(format!("cannot persist Windows cleanup request: {error}"))
        })
}

#[cfg(windows)]
fn wait_for_windows_worker_ready(
    staging: &Path,
    nonce: &str,
    worker: &mut std::process::Child,
) -> Result<(), UpdateError> {
    let ready = staging.join(WINDOWS_CLEANUP_READY);
    wait_for_windows_ready(&ready, nonce, WINDOWS_WORKER_READY_TIMEOUT, || {
        worker.try_wait().map_err(|error| {
            UpdateError::Filesystem(format!("cannot inspect Windows cleanup worker: {error}"))
        })
    })
}

#[cfg(windows)]
fn wait_for_windows_ready(
    ready: &Path,
    nonce: &str,
    timeout: Duration,
    mut child_status: impl FnMut() -> Result<Option<std::process::ExitStatus>, UpdateError>,
) -> Result<(), UpdateError> {
    let deadline = Instant::now() + timeout;
    loop {
        match fs::read_to_string(ready) {
            Ok(value) if value == nonce => return Ok(()),
            Ok(_) => {
                return Err(UpdateError::Filesystem(
                    "Windows cleanup worker returned an invalid readiness token".to_string(),
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(UpdateError::Filesystem(format!(
                    "cannot read Windows cleanup readiness: {error}"
                )))
            }
        }
        if let Some(status) = child_status()? {
            return Err(UpdateError::Filesystem(format!(
                "Windows cleanup worker exited before readiness with {status}"
            )));
        }
        if Instant::now() >= deadline {
            return Err(UpdateError::Filesystem(
                "Windows cleanup worker readiness timed out".to_string(),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(windows)]
fn run_windows_cleanup_worker(request_path: &Path) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};

    let (request, paths) = load_windows_cleanup_request(request_path)?;
    require_current_windows_executable(&paths.staging.join(WINDOWS_CLEANUP_HELPER))?;
    let parent_handle = unsafe { OpenProcess(SYNCHRONIZE, 0, request.parent_pid) };
    if parent_handle.is_null() {
        return Err("cannot open the exact updater parent process".to_string());
    }
    if let Err(error) =
        write_windows_ready_file(&paths.staging.join(WINDOWS_CLEANUP_READY), &request.nonce)
    {
        unsafe {
            CloseHandle(parent_handle);
        }
        return Err(error);
    }
    let wait_status = unsafe {
        WaitForSingleObject(
            parent_handle,
            WINDOWS_PARENT_EXIT_TIMEOUT.as_millis() as u32,
        )
    };
    let wait_error = (wait_status == WAIT_FAILED).then(io::Error::last_os_error);
    unsafe {
        CloseHandle(parent_handle);
    }
    match wait_status {
        WAIT_OBJECT_0 => {}
        WAIT_TIMEOUT => {
            return Err("updater parent did not exit within the cleanup deadline".to_string())
        }
        WAIT_FAILED => {
            return Err(format!(
                "waiting for updater parent failed: {}",
                wait_error.expect("WAIT_FAILED captures its operating-system error")
            ))
        }
        status => {
            return Err(format!(
                "waiting for updater parent returned status {status}"
            ))
        }
    }

    reconcile_windows_cleanup(&request, &paths)?;
    start_windows_finalizer(&request, &paths)
}

#[cfg(windows)]
fn run_windows_finalizer(request_path: &Path, worker_pid: u32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};

    let (request, paths) = load_windows_cleanup_request(request_path)?;
    require_current_windows_executable(&paths.target)?;
    let target_digest = digest_if_regular_file(&paths.target)?
        .ok_or_else(|| "finalizer target is missing".to_string())?;
    if target_digest != request.old_sha256 && target_digest != request.candidate_sha256 {
        return Err("finalizer target digest is not an approved update identity".to_string());
    }
    let worker_handle = unsafe { OpenProcess(SYNCHRONIZE, 0, worker_pid) };
    if worker_handle.is_null() {
        return Err("cannot open the exact cleanup worker process".to_string());
    }
    if let Err(error) =
        write_windows_ready_file(&paths.staging.join(WINDOWS_FINALIZER_READY), &request.nonce)
    {
        unsafe {
            CloseHandle(worker_handle);
        }
        return Err(error);
    }
    let wait_status = unsafe {
        WaitForSingleObject(
            worker_handle,
            WINDOWS_WORKER_READY_TIMEOUT.as_millis() as u32,
        )
    };
    let wait_error = (wait_status == WAIT_FAILED).then(io::Error::last_os_error);
    unsafe {
        CloseHandle(worker_handle);
    }
    match wait_status {
        WAIT_OBJECT_0 => {}
        WAIT_TIMEOUT => {
            return Err("cleanup worker did not exit within the finalizer deadline".to_string())
        }
        WAIT_FAILED => {
            return Err(format!(
                "waiting for cleanup worker failed: {}",
                wait_error.expect("WAIT_FAILED captures its operating-system error")
            ))
        }
        status => {
            return Err(format!(
                "waiting for cleanup worker returned status {status}"
            ))
        }
    }
    remove_windows_staging(&request, &paths, true)
}

#[cfg(windows)]
fn load_windows_cleanup_request(
    request_path: &Path,
) -> Result<(WindowsCleanupRequest, WindowsCleanupPaths), String> {
    use std::os::windows::ffi::OsStringExt;

    if request_path.file_name() != Some(OsStr::new(WINDOWS_CLEANUP_REQUEST)) {
        return Err("cleanup request has an unexpected name".to_string());
    }
    let metadata = fs::symlink_metadata(request_path)
        .map_err(|error| format!("cannot inspect cleanup request: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("cleanup request is not a regular file".to_string());
    }
    let bytes =
        fs::read(request_path).map_err(|error| format!("cannot read cleanup request: {error}"))?;
    if bytes.is_empty() || bytes.len() > 16 * 1024 {
        return Err("cleanup request size is invalid".to_string());
    }
    let request: WindowsCleanupRequest = serde_json::from_slice(&bytes)
        .map_err(|_| "cleanup request is not valid strict JSON".to_string())?;
    if request.schema_version != 1
        || request.parent_pid == 0
        || !is_lower_hex(&request.nonce, 32)
        || !is_lower_hex(&request.old_sha256, 64)
        || !is_lower_hex(&request.candidate_sha256, 64)
        || request.old_sha256 == request.candidate_sha256
    {
        return Err("cleanup request identity is invalid".to_string());
    }

    let target_name = OsString::from_wide(&request.target_name);
    let staged_name = OsString::from_wide(&request.staged_name);
    let backup_name = OsString::from_wide(&request.backup_name);
    require_direct_windows_name(&target_name)?;
    require_direct_windows_name(&staged_name)?;
    require_direct_windows_name(&backup_name)?;
    if staged_name != target_name {
        return Err("staged and target names do not match".to_string());
    }
    if target_name == OsStr::new(WINDOWS_CLEANUP_HELPER)
        || target_name == OsStr::new(WINDOWS_CLEANUP_REQUEST)
        || target_name == OsStr::new(WINDOWS_CLEANUP_READY)
        || target_name == OsStr::new(WINDOWS_FINALIZER_READY)
    {
        return Err("target name collides with a reserved updater file".to_string());
    }
    let expected_backup = format!(".nib-update-previous-{}.exe", request.nonce);
    if backup_name != OsStr::new(&expected_backup) {
        return Err("backup name is not bound to the cleanup request".to_string());
    }

    let staging = request_path
        .parent()
        .ok_or_else(|| "cleanup request has no staging directory".to_string())?;
    let staging_name = staging
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| "staging directory name is invalid".to_string())?;
    if !staging_name.starts_with(".nib-update-") {
        return Err("cleanup request is outside an updater staging directory".to_string());
    }
    let staging_metadata = fs::symlink_metadata(staging)
        .map_err(|error| format!("cannot inspect staging directory: {error}"))?;
    if staging_metadata.file_type().is_symlink() || !staging_metadata.is_dir() {
        return Err("update staging path is not a physical directory".to_string());
    }
    let staging = fs::canonicalize(staging)
        .map_err(|error| format!("cannot resolve staging directory: {error}"))?;
    let canonical_request = fs::canonicalize(request_path)
        .map_err(|error| format!("cannot resolve cleanup request: {error}"))?;
    if canonical_request != staging.join(WINDOWS_CLEANUP_REQUEST) {
        return Err("cleanup request escaped its staging directory".to_string());
    }
    let parent = staging
        .parent()
        .ok_or_else(|| "staging directory has no installation parent".to_string())?;
    let paths = WindowsCleanupPaths {
        request_path: canonical_request,
        target: parent.join(&target_name),
        staged: staging.join(&staged_name),
        backup: parent.join(&backup_name),
        staging,
    };
    Ok((request, paths))
}

#[cfg(windows)]
fn require_direct_windows_name(name: &OsStr) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Component;

    if name.encode_wide().any(|unit| unit == 0) {
        return Err("cleanup request contains a null file-name unit".to_string());
    }
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err("cleanup request contains a non-local file name".to_string());
    }
    Ok(())
}

#[cfg(windows)]
fn require_current_windows_executable(expected: &Path) -> Result<(), String> {
    let current = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("cannot resolve worker executable: {error}"))?;
    let expected = fs::canonicalize(expected)
        .map_err(|error| format!("cannot resolve expected worker executable: {error}"))?;
    if current != expected {
        return Err("private updater mode was launched from an unexpected path".to_string());
    }
    Ok(())
}

#[cfg(windows)]
fn digest_if_regular_file(path: &Path) -> Result<Option<String>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("update recovery path is not a regular file".to_string())
        }
        Ok(_) => hex_sha256_file(path)
            .map(Some)
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot inspect update recovery path: {error}")),
    }
}

#[cfg(windows)]
fn reconcile_windows_cleanup(
    request: &WindowsCleanupRequest,
    paths: &WindowsCleanupPaths,
) -> Result<(), String> {
    let target_digest = digest_if_regular_file(&paths.target)?;
    let backup_digest = digest_if_regular_file(&paths.backup)?;
    match classify_windows_cleanup_state(
        target_digest.as_deref(),
        backup_digest.as_deref(),
        &request.old_sha256,
        &request.candidate_sha256,
    ) {
        WindowsCleanupState::PublishedWithBackup => {
            retry_windows_cleanup(|| fs::remove_file(&paths.backup))
                .map_err(|error| format!("cannot remove replaced Windows image: {error}"))?;
        }
        WindowsCleanupState::PublishedClean | WindowsCleanupState::RolledBack => {}
        WindowsCleanupState::RestoreBackup => {
            retry_windows_move(&paths.backup, &paths.target).map_err(|error| error.to_string())?;
        }
        WindowsCleanupState::Ambiguous => {
            return Err("Windows update recovery state is ambiguous".to_string())
        }
    }

    let final_target = digest_if_regular_file(&paths.target)?
        .ok_or_else(|| "Windows update recovery left the target missing".to_string())?;
    if final_target != request.old_sha256 && final_target != request.candidate_sha256 {
        return Err("Windows update recovery left an unverified target".to_string());
    }
    if digest_if_regular_file(&paths.backup)?.is_some() {
        return Err("Windows update recovery left the old image present".to_string());
    }
    Ok(())
}

#[cfg(windows)]
fn start_windows_finalizer(
    request: &WindowsCleanupRequest,
    paths: &WindowsCleanupPaths,
) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    let mut finalizer = Command::new(&paths.target);
    finalizer
        .env_remove(WINDOWS_CLEANUP_REQUEST_ENV)
        .env(WINDOWS_FINALIZE_REQUEST_ENV, &paths.request_path)
        .env(
            WINDOWS_FINALIZE_WORKER_PID_ENV,
            std::process::id().to_string(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    let mut finalizer = finalizer
        .spawn()
        .map_err(|error| format!("cannot start Windows staging finalizer: {error}"))?;
    let ready = paths.staging.join(WINDOWS_FINALIZER_READY);
    let deadline = Instant::now() + WINDOWS_WORKER_READY_TIMEOUT;
    loop {
        match fs::read_to_string(&ready) {
            Ok(value) if value == request.nonce => return Ok(()),
            Ok(_) => {
                return Err("Windows finalizer returned an invalid readiness token".to_string())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot read Windows finalizer readiness: {error}")),
        }
        if let Some(status) = finalizer
            .try_wait()
            .map_err(|error| format!("cannot inspect Windows finalizer: {error}"))?
        {
            return Err(format!(
                "Windows finalizer exited before readiness with {status}"
            ));
        }
        if Instant::now() >= deadline {
            let _ = finalizer.kill();
            let _ = finalizer.wait();
            return Err("Windows finalizer readiness timed out".to_string());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(windows)]
fn write_windows_ready_file(path: &Path, nonce: &str) -> Result<(), String> {
    write_windows_ready_file_with_hook(path, nonce, |_| {})
}

#[cfg(windows)]
fn write_windows_ready_file_with_hook(
    path: &Path,
    nonce: &str,
    before_publish: impl FnOnce(&Path),
) -> Result<(), String> {
    let file_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| "updater readiness file name is invalid".to_string())?;
    let publishing = path.with_file_name(format!("{file_name}.publishing-{nonce}"));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&publishing)
        .map_err(|error| format!("cannot create updater readiness staging file: {error}"))?;
    file.write_all(nonce.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("cannot persist updater readiness staging file: {error}"))?;
    drop(file);
    before_publish(&publishing);
    move_file_windows(&publishing, path)
        .map_err(|error| format!("cannot publish updater readiness file: {error}"))
}

#[cfg(windows)]
fn retry_windows_move(source: &Path, target: &Path) -> Result<(), UpdateError> {
    let deadline = Instant::now() + WINDOWS_CLEANUP_TIMEOUT;
    loop {
        match move_file_windows(source, target) {
            Ok(()) => return Ok(()),
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(windows)]
fn retry_windows_cleanup(mut operation: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    let deadline = Instant::now() + WINDOWS_CLEANUP_TIMEOUT;
    loop {
        match operation() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(windows)]
fn remove_windows_staging(
    request: &WindowsCleanupRequest,
    paths: &WindowsCleanupPaths,
    finalizer_ready_required: bool,
) -> Result<(), String> {
    require_nonce_file(
        &paths.staging.join(WINDOWS_CLEANUP_READY),
        &request.nonce,
        false,
    )?;
    require_nonce_file(
        &paths.staging.join(WINDOWS_FINALIZER_READY),
        &request.nonce,
        finalizer_ready_required,
    )?;
    require_optional_digest(&paths.staged, &request.candidate_sha256)?;
    require_optional_digest(
        &paths.staging.join(WINDOWS_CLEANUP_HELPER),
        &request.candidate_sha256,
    )?;

    for path in [
        paths.staging.join(WINDOWS_CLEANUP_READY),
        paths.staging.join(WINDOWS_FINALIZER_READY),
        paths.staged.clone(),
        paths.staging.join(WINDOWS_CLEANUP_HELPER),
        paths.request_path.clone(),
    ] {
        retry_windows_cleanup(|| fs::remove_file(&path))
            .map_err(|error| format!("cannot remove verified Windows update file: {error}"))?;
    }
    retry_windows_cleanup(|| fs::remove_dir(&paths.staging))
        .map_err(|error| format!("cannot remove empty Windows update staging directory: {error}"))
}

#[cfg(windows)]
fn require_nonce_file(path: &Path, nonce: &str, required: bool) -> Result<(), String> {
    match fs::read_to_string(path) {
        Ok(value) if value == nonce => Ok(()),
        Ok(_) => Err("updater readiness file has an invalid token".to_string()),
        Err(error) if !required && error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot validate updater readiness file: {error}")),
    }
}

#[cfg(windows)]
fn require_optional_digest(path: &Path, expected: &str) -> Result<(), String> {
    if let Some(actual) = digest_if_regular_file(path)? {
        if actual != expected {
            return Err("Windows update staging file has an unexpected digest".to_string());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn hex_sha256_file(path: &Path) -> Result<String, UpdateError> {
    let mut file = OpenOptions::new().read(true).open(path).map_err(|error| {
        UpdateError::Filesystem(format!("cannot read update executable: {error}"))
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            UpdateError::Filesystem(format!("cannot hash update executable: {error}"))
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
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
            classify(current.clone(), ReleaseChannel::Prod, &current_manifest),
            Ok(Availability::Current(_))
        ));
        let next_manifest = parse_manifest(
            &manifest(ReleaseChannel::Prod, &latest_commit),
            ReleaseChannel::Prod,
        )
        .expect("next manifest");
        assert!(matches!(
            classify(current, ReleaseChannel::Prod, &next_manifest),
            Ok(Availability::Available { .. })
        ));
        let available = classify(
            BuildIdentity {
                channel: ReleaseChannel::Prod,
                version: "0.1.0".to_string(),
                commit: current_commit,
            },
            ReleaseChannel::Prod,
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
    fn channel_switch_is_available_even_when_commit_is_unchanged() {
        let commit = "1".repeat(40);
        let current = BuildIdentity {
            channel: ReleaseChannel::Prod,
            version: "0.1.0".to_string(),
            commit: commit.clone(),
        };
        let development_manifest = parse_manifest(
            &manifest(ReleaseChannel::Development, &commit),
            ReleaseChannel::Development,
        )
        .expect("development manifest");

        let availability = classify(
            current.clone(),
            ReleaseChannel::Development,
            &development_manifest,
        )
        .expect("channel switch");
        assert_eq!(
            availability,
            Availability::Available {
                current,
                latest: BuildIdentity {
                    channel: ReleaseChannel::Development,
                    version: "0.1.0".to_string(),
                    commit,
                },
            }
        );
        let Availability::Available { current, latest } = availability else {
            panic!("expected channel switch");
        };
        assert_eq!(
            completed_update_message(&current, &latest),
            "Switched nib channel: 0.1.0 (prod, 1111111) -> 0.1.0 (development, 1111111)"
        );
    }

    #[test]
    fn within_channel_update_retains_update_output() {
        let current = BuildIdentity {
            channel: ReleaseChannel::Prod,
            version: "0.1.0".to_string(),
            commit: "1".repeat(40),
        };
        let latest = BuildIdentity {
            channel: ReleaseChannel::Prod,
            version: "0.2.0".to_string(),
            commit: "2".repeat(40),
        };

        assert_eq!(
            completed_update_message(&current, &latest),
            "Updated nib: 0.1.0 (prod, 1111111) -> 0.2.0 (prod, 2222222)"
        );
    }

    #[test]
    fn same_commit_with_conflicting_version_fails_for_channel_switch() {
        let commit = "1".repeat(40);
        let current = BuildIdentity {
            channel: ReleaseChannel::Prod,
            version: "0.1.0".to_string(),
            commit: commit.clone(),
        };
        let mut manifest = parse_manifest(
            &manifest(ReleaseChannel::Development, &commit),
            ReleaseChannel::Development,
        )
        .expect("development manifest");
        manifest.version = "0.2.0".to_string();

        assert!(matches!(
            classify(current, ReleaseChannel::Development, &manifest),
            Err(UpdateError::InvalidRelease(_))
        ));
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

    #[test]
    fn windows_cleanup_state_requires_digest_proven_terminal_shapes() {
        let old = "a".repeat(64);
        let candidate = "b".repeat(64);
        let unknown = "c".repeat(64);

        assert_eq!(
            classify_windows_cleanup_state(Some(&candidate), Some(&old), &old, &candidate),
            WindowsCleanupState::PublishedWithBackup
        );
        assert_eq!(
            classify_windows_cleanup_state(Some(&candidate), None, &old, &candidate),
            WindowsCleanupState::PublishedClean
        );
        assert_eq!(
            classify_windows_cleanup_state(None, Some(&old), &old, &candidate),
            WindowsCleanupState::RestoreBackup
        );
        assert_eq!(
            classify_windows_cleanup_state(Some(&old), None, &old, &candidate),
            WindowsCleanupState::RolledBack
        );

        for (target, backup) in [
            (None, None),
            (Some(old.as_str()), Some(old.as_str())),
            (Some(candidate.as_str()), Some(candidate.as_str())),
            (Some(unknown.as_str()), Some(old.as_str())),
            (Some(candidate.as_str()), Some(unknown.as_str())),
        ] {
            assert_eq!(
                classify_windows_cleanup_state(target, backup, &old, &candidate),
                WindowsCleanupState::Ambiguous
            );
        }
    }

    #[test]
    fn windows_candidate_publish_rolls_back_every_failure_boundary() {
        use std::cell::RefCell;

        let staged = Path::new("staged");
        let target = Path::new("target");
        let backup = Path::new("backup");

        let publish_calls = RefCell::new(Vec::new());
        let publish_error = commit_windows_candidate(
            staged,
            target,
            backup,
            |source, destination| {
                publish_calls
                    .borrow_mut()
                    .push((source.to_path_buf(), destination.to_path_buf()));
                if source == staged && destination == target {
                    Err(UpdateError::Filesystem(
                        "injected publish failure".to_string(),
                    ))
                } else {
                    Ok(())
                }
            },
            || panic!("failed publication must not be verified"),
        )
        .expect_err("candidate publication failure");
        assert!(publish_error
            .to_string()
            .contains("injected publish failure"));
        assert_eq!(
            publish_calls.into_inner(),
            vec![
                (target.to_path_buf(), backup.to_path_buf()),
                (staged.to_path_buf(), target.to_path_buf()),
                (backup.to_path_buf(), target.to_path_buf()),
            ]
        );

        let verify_calls = RefCell::new(Vec::new());
        let verification_error = commit_windows_candidate(
            staged,
            target,
            backup,
            |source, destination| {
                verify_calls
                    .borrow_mut()
                    .push((source.to_path_buf(), destination.to_path_buf()));
                Ok(())
            },
            || {
                Err(UpdateError::Filesystem(
                    "injected verification failure".to_string(),
                ))
            },
        )
        .expect_err("published verification failure");
        assert!(verification_error
            .to_string()
            .contains("injected verification failure"));
        assert_eq!(
            verify_calls.into_inner(),
            vec![
                (target.to_path_buf(), backup.to_path_buf()),
                (staged.to_path_buf(), target.to_path_buf()),
                (target.to_path_buf(), staged.to_path_buf()),
                (backup.to_path_buf(), target.to_path_buf()),
            ]
        );

        let rollback_error = commit_windows_candidate(
            staged,
            target,
            backup,
            |source, destination| {
                if (source == staged && destination == target)
                    || (source == backup && destination == target)
                {
                    Err(UpdateError::Filesystem("injected move failure".to_string()))
                } else {
                    Ok(())
                }
            },
            || panic!("failed publication must not be verified"),
        )
        .expect_err("rollback failure must remain visible");
        assert!(rollback_error
            .to_string()
            .contains("backup rollback failed"));
    }

    #[cfg(windows)]
    fn windows_cleanup_request_for_test(
        nonce: &str,
        target_name: &OsStr,
        old: &[u8],
        candidate: &[u8],
    ) -> WindowsCleanupRequest {
        use std::os::windows::ffi::OsStrExt;

        WindowsCleanupRequest {
            schema_version: 1,
            parent_pid: std::process::id(),
            nonce: nonce.to_string(),
            target_name: target_name.encode_wide().collect(),
            staged_name: target_name.encode_wide().collect(),
            backup_name: OsStr::new(&format!(".nib-update-previous-{nonce}.exe"))
                .encode_wide()
                .collect(),
            old_sha256: hex_sha256(old),
            candidate_sha256: hex_sha256(candidate),
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_cleanup_reconciles_published_and_interrupted_states() {
        let old: &[u8] = b"old executable";
        let candidate: &[u8] = b"candidate executable";

        for restore_interrupted in [false, true] {
            let parent = tempfile::tempdir().expect("cleanup parent");
            let staging = tempfile::Builder::new()
                .prefix(".nib-update-")
                .tempdir_in(parent.path())
                .expect("cleanup staging");
            let nonce = uuid::Uuid::new_v4().simple().to_string();
            let request =
                windows_cleanup_request_for_test(&nonce, OsStr::new("nib.exe"), old, candidate);
            let request_path = staging.path().join(WINDOWS_CLEANUP_REQUEST);
            write_windows_cleanup_request(&request_path, &request).expect("cleanup request");
            let (request, paths) =
                load_windows_cleanup_request(&request_path).expect("load cleanup request");
            fs::write(&paths.backup, old).expect("old image backup");
            if !restore_interrupted {
                fs::write(&paths.target, candidate).expect("published candidate");
            }

            reconcile_windows_cleanup(&request, &paths).expect("reconcile cleanup state");
            assert!(!paths.backup.exists());
            let expected = if restore_interrupted { old } else { candidate };
            assert_eq!(
                fs::read(&paths.target).expect("reconciled target"),
                expected
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_cleanup_replay_after_committed_state_is_idempotent() {
        let old: &[u8] = b"old executable";
        let candidate: &[u8] = b"candidate executable";
        let parent = tempfile::tempdir().expect("cleanup replay parent");
        let staging = tempfile::Builder::new()
            .prefix(".nib-update-")
            .tempdir_in(parent.path())
            .expect("cleanup replay staging");
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let request =
            windows_cleanup_request_for_test(&nonce, OsStr::new("nib.exe"), old, candidate);
        let request_path = staging.path().join(WINDOWS_CLEANUP_REQUEST);
        write_windows_cleanup_request(&request_path, &request).expect("cleanup replay request");
        let (request, paths) =
            load_windows_cleanup_request(&request_path).expect("load cleanup replay request");
        fs::write(&paths.target, candidate).expect("published candidate");

        reconcile_windows_cleanup(&request, &paths).expect("first cleanup reconciliation");
        assert_eq!(
            fs::read(&paths.target).expect("first reconciled target"),
            candidate
        );
        assert!(!paths.backup.exists());

        reconcile_windows_cleanup(&request, &paths).expect("replayed cleanup reconciliation");
        assert_eq!(fs::read(&paths.target).expect("replayed target"), candidate);
        assert!(!paths.backup.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_new_update_is_fenced_until_prior_cleanup_converges() {
        let parent = tempfile::tempdir().expect("cleanup fence parent");
        reject_pending_windows_cleanup(parent.path()).expect("clean install directory");
        fs::write(parent.path().join(".nib-update-stale"), b"evidence")
            .expect("stale cleanup evidence");
        assert!(matches!(
            reject_pending_windows_cleanup(parent.path()),
            Err(UpdateError::PendingWindowsCleanup)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_worker_readiness_is_bounded_and_token_bound() {
        let directory = tempfile::tempdir().expect("readiness fixture");
        let ready = directory.path().join("ready");
        let started = Instant::now();
        let timeout =
            wait_for_windows_ready(&ready, "expected", Duration::from_millis(50), || Ok(None))
                .expect_err("missing readiness must time out");
        assert!(timeout.to_string().contains("readiness timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));

        fs::write(&ready, b"wrong").expect("wrong readiness token");
        assert!(
            wait_for_windows_ready(&ready, "expected", Duration::from_secs(1), || Ok(None))
                .expect_err("wrong readiness token")
                .to_string()
                .contains("invalid readiness token")
        );
        fs::write(&ready, b"expected").expect("correct readiness token");
        wait_for_windows_ready(&ready, "expected", Duration::from_secs(1), || Ok(None))
            .expect("correct readiness token");
    }

    #[cfg(windows)]
    #[test]
    fn windows_readiness_is_invisible_until_the_complete_nonce_is_synced() {
        use std::sync::{Arc, Barrier};

        let directory = tempfile::tempdir().expect("readiness publication fixture");
        let ready = directory.path().join(WINDOWS_CLEANUP_READY);
        let nonce = "a".repeat(32);
        let staged = Arc::new(Barrier::new(2));
        let publish = Arc::new(Barrier::new(2));
        let child_ready = ready.clone();
        let child_nonce = nonce.clone();
        let child_staged = Arc::clone(&staged);
        let child_publish = Arc::clone(&publish);
        let writer = thread::spawn(move || {
            write_windows_ready_file_with_hook(&child_ready, &child_nonce, |publishing| {
                assert_eq!(
                    fs::read_to_string(publishing).expect("synced staged readiness"),
                    child_nonce
                );
                child_staged.wait();
                child_publish.wait();
            })
        });

        staged.wait();
        assert!(
            !ready.exists(),
            "final readiness must remain absent while publication is paused"
        );
        publish.wait();
        writer
            .join()
            .expect("readiness writer")
            .expect("publish readiness");
        assert_eq!(fs::read_to_string(&ready).expect("final readiness"), nonce);
    }

    #[cfg(windows)]
    #[test]
    fn windows_cleanup_request_rejects_traversal_unknown_fields_and_digest_replay() {
        use std::os::windows::ffi::OsStrExt;

        let parent = tempfile::tempdir().expect("cleanup parent");
        let old: &[u8] = b"old executable";
        let candidate: &[u8] = b"candidate executable";

        let traversal_staging = tempfile::Builder::new()
            .prefix(".nib-update-")
            .tempdir_in(parent.path())
            .expect("traversal staging");
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let mut traversal =
            windows_cleanup_request_for_test(&nonce, OsStr::new("nib.exe"), old, candidate);
        traversal.target_name = OsStr::new("..\\escape.exe").encode_wide().collect();
        traversal.staged_name = traversal.target_name.clone();
        let traversal_path = traversal_staging.path().join(WINDOWS_CLEANUP_REQUEST);
        write_windows_cleanup_request(&traversal_path, &traversal).expect("traversal request");
        assert!(load_windows_cleanup_request(&traversal_path).is_err());

        let unknown_staging = tempfile::Builder::new()
            .prefix(".nib-update-")
            .tempdir_in(parent.path())
            .expect("unknown-field staging");
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let valid = windows_cleanup_request_for_test(&nonce, OsStr::new("nib.exe"), old, candidate);
        let mut unknown = serde_json::to_value(valid).expect("request value");
        unknown
            .as_object_mut()
            .expect("request object")
            .insert("unexpected".to_string(), serde_json::json!(true));
        let unknown_path = unknown_staging.path().join(WINDOWS_CLEANUP_REQUEST);
        fs::write(
            &unknown_path,
            serde_json::to_vec(&unknown).expect("unknown-field JSON"),
        )
        .expect("unknown-field request");
        assert!(load_windows_cleanup_request(&unknown_path).is_err());

        let replay_staging = tempfile::Builder::new()
            .prefix(".nib-update-")
            .tempdir_in(parent.path())
            .expect("digest-replay staging");
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let mut replay =
            windows_cleanup_request_for_test(&nonce, OsStr::new("nib.exe"), old, candidate);
        replay.candidate_sha256 = replay.old_sha256.clone();
        let replay_path = replay_staging.path().join(WINDOWS_CLEANUP_REQUEST);
        write_windows_cleanup_request(&replay_path, &replay).expect("digest-replay request");
        assert!(load_windows_cleanup_request(&replay_path).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_running_image_can_be_renamed_before_candidate_publication() {
        const CHILD_ENV: &str = "NIB_TEST_WINDOWS_RUNNING_IMAGE_CHILD";
        const READY_ENV: &str = "NIB_TEST_WINDOWS_RUNNING_IMAGE_READY";
        const STOP_ENV: &str = "NIB_TEST_WINDOWS_RUNNING_IMAGE_STOP";

        if std::env::var_os(CHILD_ENV).as_deref() == Some(OsStr::new("1")) {
            let ready = PathBuf::from(std::env::var_os(READY_ENV).expect("child ready path"));
            let stop = PathBuf::from(std::env::var_os(STOP_ENV).expect("child stop path"));
            fs::write(&ready, b"ready").expect("publish child readiness");
            let deadline = Instant::now() + Duration::from_secs(30);
            while !stop.exists() {
                assert!(
                    Instant::now() < deadline,
                    "parent did not release test child"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
            return;
        }

        let directory = tempfile::tempdir().expect("Windows replacement fixture");
        let source = std::env::current_exe().expect("test executable");
        let target = directory.path().join("nib-running-test.exe");
        let backup = directory.path().join("nib-running-test.previous.exe");
        let ready = directory.path().join("child.ready");
        let stop = directory.path().join("child.stop");
        fs::copy(&source, &target).expect("copy running-image fixture");
        let mut child = Command::new(&target)
            .arg("windows_running_image_can_be_renamed_before_candidate_publication")
            .env(CHILD_ENV, "1")
            .env(READY_ENV, &ready)
            .env(STOP_ENV, &stop)
            .spawn()
            .expect("start running-image fixture");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect fixture child") {
                panic!("fixture child exited before readiness with {status}");
            }
            assert!(
                Instant::now() < deadline,
                "fixture child readiness timed out"
            );
            std::thread::sleep(Duration::from_millis(25));
        }

        move_file_windows(&target, &backup).expect("rename running executable to backup");
        fs::copy(&source, &target).expect("publish candidate at original path");
        assert!(target.is_file());
        assert!(backup.is_file());
        fs::write(&stop, b"stop").expect("release fixture child");
        assert!(child.wait().expect("wait fixture child").success());
        fs::remove_file(&backup).expect("remove exited old image");
        assert!(target.is_file());
        assert!(!backup.exists());
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
    fn bounded_transport_fetches_the_requested_channel_manifest() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("local server");
        let address = listener.local_addr().expect("server address");
        let body = manifest(ReleaseChannel::Development, &"1".repeat(40));
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request");
            let path = read_request_path(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("response headers");
            stream.write_all(&body).expect("response body");
            path
        });
        let base = Url::parse(&format!("http://{address}/")).expect("base URL");
        let transport = Transport::for_test(base, Duration::from_secs(2));
        let fetched = fetch_manifest(&transport, ReleaseChannel::Development)
            .expect("development manifest fetch");
        assert_eq!(fetched.commit, "1".repeat(40));
        assert_eq!(
            server.join().expect("server"),
            "/development-latest/nib-release.json"
        );
    }

    #[test]
    fn verified_archive_and_checksum_follow_the_target_channel() {
        let asset_name = current_asset_name().expect("supported test platform");
        let archive = b"target-channel-archive".to_vec();
        let digest = hex_sha256(&archive);
        let checksum = format!("{digest}  {asset_name}\n").into_bytes();
        let mut parsed_manifest = parse_manifest(
            &manifest(ReleaseChannel::Development, &"2".repeat(40)),
            ReleaseChannel::Development,
        )
        .expect("development manifest");
        parsed_manifest.assets.insert(
            asset_name.to_string(),
            ReleaseAsset {
                sha256: digest,
                size: archive.len() as u64,
            },
        );
        let latest = BuildIdentity {
            channel: ReleaseChannel::Development,
            version: parsed_manifest.version.clone(),
            commit: parsed_manifest.commit.clone(),
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("local server");
        let address = listener.local_addr().expect("server address");
        let archive_response = archive.clone();
        let server = thread::spawn(move || {
            let mut paths = Vec::new();
            for response in [archive_response, checksum] {
                let (mut stream, _) = listener.accept().expect("request");
                paths.push(read_request_path(&mut stream));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                )
                .expect("response headers");
                stream.write_all(&response).expect("response body");
            }
            paths
        });
        let base = Url::parse(&format!("http://{address}/")).expect("base URL");
        let transport = Transport::for_test(base, Duration::from_secs(2));

        assert_eq!(
            fetch_verified_archive(&transport, &parsed_manifest, &latest, asset_name)
                .expect("verified target-channel archive"),
            archive
        );
        assert_eq!(
            server.join().expect("server"),
            [
                format!("/development-latest/{asset_name}"),
                format!("/development-latest/{asset_name}.sha256"),
            ]
        );
    }

    fn read_request_path(stream: &mut TcpStream) -> String {
        let mut bytes = [0u8; 2048];
        let count = stream.read(&mut bytes).expect("read request");
        let request = std::str::from_utf8(&bytes[..count]).expect("HTTP request UTF-8");
        request
            .lines()
            .next()
            .and_then(|line| line.split_ascii_whitespace().nth(1))
            .expect("HTTP request path")
            .to_string()
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
