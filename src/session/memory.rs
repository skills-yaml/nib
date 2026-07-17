use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(test)]
use std::fs;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryEntryMetadata {
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct MemoryMetadata {
    #[serde(default)]
    pub environment: HashMap<String, MemoryEntryMetadata>,
    #[serde(default)]
    pub user: HashMap<String, MemoryEntryMetadata>,
}

impl MemoryMetadata {
    fn is_empty(&self) -> bool {
        self.environment.is_empty() && self.user.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct MemoryStoreData {
    #[serde(default, skip_serializing_if = "revision_is_zero")]
    pub revision: u64,
    #[serde(default)]
    pub environment: HashMap<String, String>,
    #[serde(default)]
    pub user: HashMap<String, String>,
    /// Optional age metadata. Legacy memory files only contain the two value maps and
    /// continue to deserialize; entries without metadata are never age-pruned.
    #[serde(default, skip_serializing_if = "MemoryMetadata::is_empty")]
    pub metadata: MemoryMetadata,
}

fn revision_is_zero(revision: &u64) -> bool {
    *revision == 0
}

#[derive(Debug, Clone)]
pub struct MemoryStore {
    path: PathBuf,
}

type MemoryMutex = Mutex<()>;
type MemoryLockRegistry = Mutex<HashMap<PathBuf, Weak<MemoryMutex>>>;

static MEMORY_LOCKS: OnceLock<MemoryLockRegistry> = OnceLock::new();
const MAX_MEMORY_JSON_BYTES: u64 = 8 * 1024 * 1024;
const MAX_MEMORY_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_MEMORY_DIRECTORY_NAME_BYTES: usize = MAX_MEMORY_DIRECTORY_ENTRIES * 256;

struct OpenedMemoryStore {
    data: MemoryStoreData,
    file: Option<File>,
}

impl MemoryStore {
    pub fn new(project_root: &Path) -> Self {
        Self::at_path(project_root.join(".nib").join("memory.json"))
    }

    pub fn at_path(path: PathBuf) -> Self {
        Self {
            path: normalized_path(path),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> MemoryStoreData {
        self.load_result()
            .unwrap_or_else(|error| panic!("failed to load memory store: {error}"))
    }

    pub fn load_result(&self) -> Result<MemoryStoreData, String> {
        self.with_lock(|directory| self.load_unlocked(directory))
    }

    pub fn save(&self, data: &mut MemoryStoreData) -> Result<(), String> {
        let committed_revision = data
            .revision
            .checked_add(1)
            .ok_or_else(|| "memory revision overflowed".to_string())?;
        self.with_lock(|directory| {
            // Never replace unreadable or corrupt memory as a side effect of a save.
            let opened = self.load_opened_unlocked(directory)?;
            if data.revision != opened.data.revision {
                return Err(format!(
                    "stale memory revision: snapshot={}, current={}",
                    data.revision, opened.data.revision
                ));
            }
            let mut next = data.clone();
            next.revision = committed_revision;
            self.save_unlocked_expected(directory, &next, opened.expectation(), || Ok(()))
        })?;
        data.revision = committed_revision;
        Ok(())
    }

    pub fn update<T>(
        &self,
        update: impl FnOnce(&mut MemoryStoreData) -> Result<T, String>,
    ) -> Result<T, String> {
        self.update_with_commit_check(update, || Ok(()))
    }

    pub(crate) fn update_with_commit_check<T>(
        &self,
        update: impl FnOnce(&mut MemoryStoreData) -> Result<T, String>,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<T, String> {
        self.with_lock(|directory| {
            let mut opened = self.load_opened_unlocked(directory)?;
            let revision = opened.data.revision;
            let result = update(&mut opened.data)?;
            opened.data.revision = revision
                .checked_add(1)
                .ok_or_else(|| "memory revision overflowed".to_string())?;
            self.save_unlocked_expected(
                directory,
                &opened.data,
                opened.expectation(),
                before_commit,
            )?;
            Ok(result)
        })
    }

    pub fn environment(&self, key: &str) -> Option<String> {
        self.environment_result(key)
            .unwrap_or_else(|error| panic!("failed to load environment memory: {error}"))
    }

    pub fn user(&self, key: &str) -> Option<String> {
        self.user_result(key)
            .unwrap_or_else(|error| panic!("failed to load user memory: {error}"))
    }

    pub fn environment_result(&self, key: &str) -> Result<Option<String>, String> {
        Ok(self.load_result()?.environment.get(key).cloned())
    }

    pub fn user_result(&self, key: &str) -> Result<Option<String>, String> {
        Ok(self.load_result()?.user.get(key).cloned())
    }

    pub fn set_environment(&self, key: &str, value: &str) -> Result<(), String> {
        self.set_environment_at(key, value, Utc::now())
    }

    pub fn set_environment_at(
        &self,
        key: &str,
        value: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), String> {
        validate_key(key)?;
        self.update(|data| {
            data.environment.insert(key.to_string(), value.to_string());
            data.metadata
                .environment
                .insert(key.to_string(), MemoryEntryMetadata { updated_at });
            Ok(())
        })
    }

    pub fn set_user(&self, key: &str, value: &str) -> Result<(), String> {
        self.set_user_at(key, value, Utc::now())
    }

    pub fn set_user_at(
        &self,
        key: &str,
        value: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), String> {
        validate_key(key)?;
        self.update(|data| {
            data.user.insert(key.to_string(), value.to_string());
            data.metadata
                .user
                .insert(key.to_string(), MemoryEntryMetadata { updated_at });
            Ok(())
        })
    }

    pub fn remove_environment(&self, key: &str) -> Result<Option<String>, String> {
        validate_key(key)?;
        self.update(|data| {
            let removed = data.environment.remove(key);
            data.metadata.environment.remove(key);
            Ok(removed)
        })
    }

    pub fn remove_user(&self, key: &str) -> Result<Option<String>, String> {
        validate_key(key)?;
        self.update(|data| {
            let removed = data.user.remove(key);
            data.metadata.user.remove(key);
            Ok(removed)
        })
    }

    fn load_unlocked(
        &self,
        directory: &crate::daemons::state::StableDirectory,
    ) -> Result<MemoryStoreData, String> {
        self.load_opened_unlocked(directory)
            .map(|opened| opened.data)
    }

    #[cfg(test)]
    fn load_unlocked_with_hook(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        after_open: impl FnOnce(),
    ) -> Result<MemoryStoreData, String> {
        self.load_opened_unlocked_with_hook(directory, after_open)
            .map(|opened| opened.data)
    }

    fn load_opened_unlocked(
        &self,
        directory: &crate::daemons::state::StableDirectory,
    ) -> Result<OpenedMemoryStore, String> {
        self.load_opened_unlocked_with_hook(directory, || {})
    }

    fn load_opened_unlocked_with_hook(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        after_open: impl FnOnce(),
    ) -> Result<OpenedMemoryStore, String> {
        if !directory.path_exists(&self.path)? {
            return Ok(OpenedMemoryStore {
                data: MemoryStoreData::default(),
                file: None,
            });
        }
        let file = directory.open_read(&self.path)?;
        let opened_metadata = file
            .metadata()
            .map_err(|error| format!("failed to inspect {}: {error}", self.path.display()))?;
        if !opened_metadata.is_file() || opened_metadata.len() > MAX_MEMORY_JSON_BYTES {
            return Err(format!(
                "memory store {} is {} bytes; maximum is {} bytes",
                self.path.display(),
                opened_metadata.len(),
                MAX_MEMORY_JSON_BYTES
            ));
        }
        after_open();
        let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
        (&file)
            .take(MAX_MEMORY_JSON_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("failed to read {}: {error}", self.path.display()))?;
        if bytes.len() as u64 > MAX_MEMORY_JSON_BYTES {
            return Err(format!(
                "memory store {} exceeds the {}-byte limit",
                self.path.display(),
                MAX_MEMORY_JSON_BYTES
            ));
        }
        directory.verify_file_identity(&self.path, &file)?;
        let contents = String::from_utf8(bytes)
            .map_err(|error| format!("memory store is not UTF-8: {error}"))?;
        let data = serde_json::from_str(&contents)
            .map_err(|error| format!("failed to parse {}: {error}", self.path.display()))?;
        Ok(OpenedMemoryStore {
            data,
            file: Some(file),
        })
    }

    #[cfg(test)]
    fn save_unlocked(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        data: &MemoryStoreData,
    ) -> Result<(), String> {
        self.save_unlocked_expected(
            directory,
            data,
            crate::daemons::state::FileExpectation::Any,
            || Ok(()),
        )
    }

    fn save_unlocked_expected(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        data: &MemoryStoreData,
        expected: crate::daemons::state::FileExpectation<'_>,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let contents = serde_json::to_vec_pretty(data).map_err(|error| error.to_string())?;
        if contents.len() as u64 > MAX_MEMORY_JSON_BYTES {
            return Err(format!(
                "memory store {} would be {} bytes; maximum is {} bytes",
                self.path.display(),
                contents.len(),
                MAX_MEMORY_JSON_BYTES
            ));
        }
        directory.save_bytes_atomically_expected_with_hook(
            &self.path,
            &contents,
            ".nib-memory-",
            true,
            expected,
            before_commit,
        )?;
        let published = self.load_opened_unlocked(directory)?;
        if published.data != *data {
            return Err(format!(
                "published memory did not retain the requested contents: {}",
                self.path.display()
            ));
        }
        Ok(())
    }

    fn with_lock<T>(
        &self,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, String>,
    ) -> Result<T, String> {
        let process_lock = memory_process_lock(&self.path)?;
        let _guard = process_lock
            .lock()
            .map_err(|_| format!("memory lock was poisoned: {}", self.path.display()))?;
        let parent = self.path.parent().ok_or_else(|| {
            format!(
                "memory store has no parent directory: {}",
                self.path.display()
            )
        })?;
        crate::fs_security::ensure_directory_without_symlinks(parent)
            .map_err(|error| error.to_string())?;
        let lock_path = memory_lock_path(&self.path);
        crate::daemons::state::with_file_lock_in(&lock_path, parent, |directory| {
            directory.recover_stale_temporary_files(
                ".nib-memory-",
                MAX_MEMORY_DIRECTORY_ENTRIES,
                MAX_MEMORY_DIRECTORY_NAME_BYTES,
            )?;
            operation(directory)
        })
    }
}

impl OpenedMemoryStore {
    fn expectation(&self) -> crate::daemons::state::FileExpectation<'_> {
        self.file.as_ref().map_or(
            crate::daemons::state::FileExpectation::Missing,
            crate::daemons::state::FileExpectation::Present,
        )
    }
}

fn memory_process_lock(path: &Path) -> Result<Arc<MemoryMutex>, String> {
    let registry = MEMORY_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .map_err(|_| format!("memory lock registry was poisoned: {}", path.display()))?;
    registry.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = registry.get(path).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    registry.insert(path.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}

fn memory_lock_path(path: &Path) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "memory".into());
    file_name.push(".lock");
    path.with_file_name(file_name)
}

fn normalized_path(path: PathBuf) -> PathBuf {
    crate::fs_security::absolute_path(&path).unwrap_or(path)
}

fn validate_key(key: &str) -> Result<(), String> {
    if key.trim().is_empty() {
        Err("memory key must not be empty".to_string())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::tempdir;

    #[cfg(unix)]
    const MEMORY_COMMIT_CHILD_ROOT: &str = "NIB_MEMORY_COMMIT_CHILD_ROOT";
    #[cfg(unix)]
    const MEMORY_COMMIT_CHILD_MODE: &str = "NIB_MEMORY_COMMIT_CHILD_MODE";
    #[cfg(unix)]
    const MEMORY_COMMIT_CHILD_READY: &str = "NIB_MEMORY_COMMIT_CHILD_READY";
    #[cfg(unix)]
    const MEMORY_COMMIT_CHILD_RELEASE: &str = "NIB_MEMORY_COMMIT_CHILD_RELEASE";

    #[test]
    fn environment_and_user_memory_survive_store_restart() {
        let dir = tempdir().expect("tempdir");
        let store = MemoryStore::new(dir.path());
        store
            .set_environment("test_command", "task test")
            .expect("save environment memory");
        store
            .set_user("response_style", "concise")
            .expect("save user memory");

        let restarted = MemoryStore::new(dir.path());
        assert_eq!(
            restarted.environment("test_command").as_deref(),
            Some("task test")
        );
        assert_eq!(restarted.user("response_style").as_deref(), Some("concise"));
        assert_eq!(restarted.environment("response_style"), None);
        assert_eq!(restarted.user("test_command"), None);

        restarted
            .remove_environment("test_command")
            .expect("remove memory");
        assert_eq!(
            MemoryStore::new(dir.path()).environment("test_command"),
            None
        );
    }

    #[test]
    fn corrupt_memory_is_reported_by_strict_load() {
        let dir = tempdir().expect("tempdir");
        let store = MemoryStore::new(dir.path());
        std::fs::create_dir_all(store.path().parent().unwrap()).expect("memory directory");
        std::fs::write(store.path(), "not json").expect("corrupt memory");
        let corrupt = std::fs::read(store.path()).expect("corrupt bytes");

        assert!(store.load_result().is_err());
        assert!(store.set_environment("must_not", "overwrite").is_err());
        let mut replacement = MemoryStoreData::default();
        assert!(store.save(&mut replacement).is_err());
        assert_eq!(
            std::fs::read(store.path()).expect("preserved corruption"),
            corrupt
        );
    }

    #[test]
    fn legacy_memory_json_remains_compatible_and_age_unknown() {
        let dir = tempdir().expect("tempdir");
        let store = MemoryStore::new(dir.path());
        std::fs::create_dir_all(store.path().parent().unwrap()).expect("memory directory");
        std::fs::write(
            store.path(),
            r#"{"environment":{"gate":"task test"},"user":{"style":"concise"}}"#,
        )
        .expect("legacy memory");

        let mut data = store.load_result().expect("load legacy memory");
        assert_eq!(data.environment["gate"], "task test");
        assert_eq!(data.user["style"], "concise");
        assert!(data.metadata.is_empty());

        store.save(&mut data).expect("roundtrip legacy memory");
        let reloaded = store.load_result().expect("reload memory");
        assert_eq!(reloaded.environment, data.environment);
        assert_eq!(reloaded.user, data.user);
        assert_eq!(reloaded.metadata, data.metadata);
        assert_eq!(reloaded.revision, data.revision);
    }

    #[test]
    fn setters_record_deterministic_age_and_removals_clear_it() {
        let dir = tempdir().expect("tempdir");
        let store = MemoryStore::new(dir.path());
        let timestamp = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();

        store
            .set_environment_at("gate", "task check", timestamp)
            .expect("set environment");
        store
            .set_user_at("style", "direct", timestamp)
            .expect("set user");

        let data = store.load_result().expect("load memory");
        assert_eq!(data.metadata.environment["gate"].updated_at, timestamp);
        assert_eq!(data.metadata.user["style"].updated_at, timestamp);

        store
            .remove_environment("gate")
            .expect("remove environment");
        store.remove_user("style").expect("remove user");
        let data = store.load_result().expect("reload memory");
        assert!(!data.metadata.environment.contains_key("gate"));
        assert!(!data.metadata.user.contains_key("style"));
    }

    #[test]
    fn oversized_sparse_memory_is_rejected_before_reading() {
        let dir = tempdir().expect("tempdir");
        let store = MemoryStore::new(dir.path());
        fs::create_dir_all(store.path().parent().unwrap()).expect("memory directory");
        fs::File::create(store.path())
            .and_then(|file| file.set_len(MAX_MEMORY_JSON_BYTES + 1))
            .expect("create sparse memory");

        let error = store
            .load_result()
            .expect_err("oversized memory must fail closed");

        assert!(error.contains("maximum is"));
        assert!(store.set_user("must_not", "overwrite").is_err());
        assert_eq!(
            fs::metadata(store.path()).unwrap().len(),
            MAX_MEMORY_JSON_BYTES + 1
        );
    }

    #[test]
    fn memory_file_replacement_during_read_fails_closed() {
        let dir = tempdir().expect("tempdir");
        let store = MemoryStore::new(dir.path());
        store.set_user("style", "original").expect("seed memory");
        let replacement = store.path().with_file_name("memory.replacement.json");
        let displaced = store.path().with_file_name("memory.displaced.json");
        fs::write(
            &replacement,
            r#"{"environment":{},"user":{"style":"forged"}}"#,
        )
        .expect("replacement memory");

        let error = store
            .with_lock(|directory| {
                store.load_unlocked_with_hook(directory, || {
                    fs::rename(store.path(), &displaced).expect("displace opened memory");
                    fs::rename(&replacement, store.path()).expect("replace memory path");
                })
            })
            .expect_err("replacement identity must fail closed");

        assert!(error.contains("identity changed"), "{error}");
        assert_eq!(
            fs::read_to_string(store.path()).expect("visible replacement"),
            r#"{"environment":{},"user":{"style":"forged"}}"#
        );
        assert!(displaced.exists());
    }

    #[test]
    fn memory_commit_rejects_replacement_and_preserves_newer_state() {
        let dir = tempdir().expect("tempdir");
        let store = MemoryStore::new(dir.path());
        store.set_user("style", "original").expect("seed memory");
        let displaced = store.path().with_file_name("memory.displaced.json");
        let mut replacement = store.load_result().expect("load replacement base");
        replacement
            .user
            .insert("style".to_string(), "newer".to_string());
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("replacement JSON");

        let error = store
            .update_with_commit_check(
                |data| {
                    data.user
                        .insert("style".to_string(), "stale-update".to_string());
                    Ok(())
                },
                || {
                    fs::rename(store.path(), &displaced).map_err(|error| error.to_string())?;
                    fs::write(store.path(), &replacement_bytes).map_err(|error| error.to_string())
                },
            )
            .expect_err("replaced memory target must fail conditional commit");

        assert!(error.contains("identity changed"), "{error}");
        assert_eq!(store.user("style").as_deref(), Some("newer"));
        let displaced_data: MemoryStoreData =
            serde_json::from_slice(&fs::read(displaced).expect("displaced original memory"))
                .expect("decode displaced memory");
        assert_eq!(displaced_data.user["style"], "original");
    }

    #[cfg(unix)]
    #[test]
    fn real_child_memory_commit_barrier_and_fsync_crash_recovery() {
        if let Some(root) = std::env::var_os(MEMORY_COMMIT_CHILD_ROOT) {
            run_memory_commit_child(Path::new(&root));
            return;
        }

        let replacement_root = tempdir().expect("replacement project");
        let replacement_store = MemoryStore::new(replacement_root.path());
        replacement_store
            .set_user("style", "original")
            .expect("seed replacement memory");
        let mut replacement = replacement_store
            .load_result()
            .expect("replacement memory base");
        replacement.revision = replacement
            .revision
            .checked_add(1)
            .expect("replacement revision");
        replacement
            .user
            .insert("style".to_string(), "authoritative replacement".to_string());
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");
        let displaced = replacement_store
            .path()
            .with_file_name("memory.child-displaced");
        let ready = replacement_root.path().join("memory-replacement.ready");
        let release = replacement_root.path().join("memory-replacement.release");
        let mut child =
            spawn_memory_commit_child(replacement_root.path(), "replace", &ready, Some(&release));
        wait_for_memory_commit_child(&mut child, &ready);

        fs::rename(replacement_store.path(), &displaced).expect("displace expected memory");
        fs::write(replacement_store.path(), &replacement_bytes).expect("install replacement");
        fs::write(&release, b"release").expect("release memory child");
        let status = child.wait().expect("wait for replacement child");
        assert!(status.success(), "replacement child failed: {status}");
        assert_eq!(
            fs::read(replacement_store.path()).expect("replacement memory bytes"),
            replacement_bytes
        );
        assert_eq!(
            replacement_store.user("style").as_deref(),
            Some("authoritative replacement")
        );

        let crash_root = tempdir().expect("crash project");
        let crash_store = MemoryStore::new(crash_root.path());
        crash_store
            .set_user("style", "committed")
            .expect("seed crash memory");
        let crash_before = fs::read(crash_store.path()).expect("memory before crash");
        let crash_ready = crash_root.path().join("memory-crash.ready");
        let mut crash_child =
            spawn_memory_commit_child(crash_root.path(), "kill", &crash_ready, None);
        wait_for_memory_commit_child(&mut crash_child, &crash_ready);
        let temporary =
            memory_temporary_paths(crash_store.path().parent().expect("crash memory parent"));
        assert_eq!(temporary.len(), 1, "expected one fsynced memory temp");
        crash_child.kill().expect("kill memory writer");
        crash_child.wait().expect("reap memory writer");
        assert!(
            temporary[0].exists(),
            "killed writer temp disappeared early"
        );

        let recovered = MemoryStore::new(crash_root.path());
        assert_eq!(
            recovered.load_result().expect("recover memory").user["style"],
            "committed"
        );
        assert_eq!(
            fs::read(recovered.path()).expect("memory after recovery"),
            crash_before
        );
        assert!(
            memory_temporary_paths(recovered.path().parent().expect("memory parent")).is_empty(),
            "memory recovery left the killed writer temp"
        );
    }

    #[test]
    fn stale_memory_snapshot_cannot_overwrite_a_newer_revision() {
        let dir = tempdir().expect("tempdir");
        let store = MemoryStore::new(dir.path());
        store.set_user("style", "base").expect("seed memory");
        let mut first = store.load_result().expect("first snapshot");
        let mut stale = first.clone();
        first.user.insert("style".to_string(), "first".to_string());
        stale.user.insert("style".to_string(), "stale".to_string());

        store.save(&mut first).expect("first snapshot commit");
        let error = store
            .save(&mut stale)
            .expect_err("stale memory snapshot must be rejected");

        assert!(error.contains("stale memory revision"), "{error}");
        assert_eq!(store.user("style").as_deref(), Some("first"));
    }

    #[test]
    fn consecutive_memory_saves_refresh_the_snapshot_revision() {
        let dir = tempdir().expect("tempdir");
        let store = MemoryStore::new(dir.path());
        let mut data = MemoryStoreData::default();
        data.user.insert("style".to_string(), "first".to_string());

        store.save(&mut data).expect("first save");
        assert_eq!(data.revision, 1);
        data.user.insert("style".to_string(), "second".to_string());
        store.save(&mut data).expect("second save");
        assert_eq!(data.revision, 2);

        let persisted = store.load_result().expect("persisted memory");
        assert_eq!(persisted.revision, 2);
        assert_eq!(persisted.user["style"], "second");
    }

    #[test]
    fn memory_revision_overflow_preserves_disk_and_snapshot() {
        let dir = tempdir().expect("tempdir");
        let store = MemoryStore::new(dir.path());
        std::fs::create_dir_all(store.path().parent().expect("memory parent"))
            .expect("memory directory");
        let mut data = MemoryStoreData {
            revision: u64::MAX,
            ..MemoryStoreData::default()
        };
        std::fs::write(
            store.path(),
            serde_json::to_vec_pretty(&data).expect("overflow memory JSON"),
        )
        .expect("write overflow memory");
        let before = std::fs::read(store.path()).expect("memory before failed save");

        let error = store
            .save(&mut data)
            .expect_err("revision overflow must fail closed");

        assert!(error.contains("revision overflowed"), "{error}");
        assert_eq!(data.revision, u64::MAX);
        assert_eq!(
            std::fs::read(store.path()).expect("memory after failed save"),
            before
        );
    }

    #[cfg(unix)]
    #[test]
    fn detached_memory_parent_cannot_receive_atomic_publication() {
        let dir = tempdir().expect("tempdir");
        let store = MemoryStore::new(dir.path());
        store
            .set_environment("gate", "original")
            .expect("seed memory");
        let parent = store.path().parent().expect("memory parent").to_path_buf();
        let displaced = dir.path().join(".nib.displaced");
        let mut replacement_data = MemoryStoreData::default();
        replacement_data
            .environment
            .insert("gate".to_string(), "replacement".to_string());

        let error = store
            .with_lock(|directory| {
                fs::rename(&parent, &displaced).expect("detach memory parent");
                fs::create_dir(&parent).expect("replacement memory parent");
                store.save_unlocked(directory, &replacement_data)
            })
            .expect_err("detached parent must fail closed");

        assert!(error.contains("directory identity changed"), "{error}");
        assert!(!store.path().exists());
        fs::remove_dir(&parent).expect("remove replacement parent");
        fs::rename(&displaced, &parent).expect("restore original memory parent");
        assert_eq!(store.environment("gate").as_deref(), Some("original"));
    }

    #[cfg(unix)]
    #[test]
    fn direct_memory_store_rejects_symlinked_nib_without_outside_write() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("project");
        let outside = tempdir().expect("outside");
        symlink(outside.path(), root.path().join(".nib")).expect("symlink .nib");
        let store = MemoryStore::new(root.path());

        assert!(store.set_user("blocked", "value").is_err());
        assert!(!outside.path().join("memory.json").exists());
        assert!(!outside.path().join("memory.json.lock").exists());
    }

    #[cfg(windows)]
    #[test]
    fn direct_memory_store_rejects_junctioned_nib_without_outside_write() {
        let root = tempdir().expect("project");
        let outside = tempdir().expect("outside");
        let junction = root.path().join(".nib");
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .output()
            .expect("create junction");
        assert!(
            output.status.success(),
            "mklink failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let store = MemoryStore::new(root.path());

        assert!(store.set_user("blocked", "value").is_err());
        assert!(!outside.path().join("memory.json").exists());
        assert!(!outside.path().join("memory.json.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn memory_mutation_rechecks_ancestor_after_initial_access() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("project");
        let outside = tempdir().expect("outside");
        let store = MemoryStore::new(root.path());
        assert_eq!(store.load_result().unwrap(), MemoryStoreData::default());
        fs::remove_file(root.path().join(".nib/memory.json.lock")).expect("remove lock");
        fs::remove_dir(root.path().join(".nib")).expect("remove .nib");
        symlink(outside.path(), root.path().join(".nib")).expect("swap .nib");

        assert!(store.set_environment("blocked", "value").is_err());
        assert!(!outside.path().join("memory.json").exists());
    }

    #[cfg(unix)]
    fn run_memory_commit_child(root: &Path) {
        let mode = std::env::var(MEMORY_COMMIT_CHILD_MODE)
            .expect("memory commit child mode must be configured");
        let ready = PathBuf::from(
            std::env::var_os(MEMORY_COMMIT_CHILD_READY)
                .expect("memory commit child ready path must be configured"),
        );
        let release = std::env::var_os(MEMORY_COMMIT_CHILD_RELEASE).map(PathBuf::from);
        let store = MemoryStore::new(root);
        let result = store.update_with_commit_check(
            |data| {
                data.user
                    .insert("style".to_string(), "child must not publish".to_string());
                Ok(())
            },
            || {
                fs::write(&ready, b"ready").map_err(|error| error.to_string())?;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                loop {
                    if release.as_ref().is_some_and(|path| path.exists()) {
                        return Ok(());
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err("memory commit child timed out".to_string());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            },
        );

        match mode.as_str() {
            "replace" => {
                let error = result.expect_err("commit-barrier replacement must fail closed");
                assert!(error.contains("identity changed"), "{error}");
            }
            "kill" => panic!("memory crash child unexpectedly left its commit barrier"),
            value => panic!("unsupported memory commit child mode: {value}"),
        }
    }

    #[cfg(unix)]
    fn spawn_memory_commit_child(
        root: &Path,
        mode: &str,
        ready: &Path,
        release: Option<&Path>,
    ) -> std::process::Child {
        let _ = fs::remove_file(ready);
        if let Some(release) = release {
            let _ = fs::remove_file(release);
        }
        let mut command = std::process::Command::new(
            std::env::current_exe().expect("current memory test binary"),
        );
        command
            .args([
                "--exact",
                "session::memory::tests::real_child_memory_commit_barrier_and_fsync_crash_recovery",
                "--nocapture",
            ])
            .env(MEMORY_COMMIT_CHILD_ROOT, root)
            .env(MEMORY_COMMIT_CHILD_MODE, mode)
            .env(MEMORY_COMMIT_CHILD_READY, ready);
        if let Some(release) = release {
            command.env(MEMORY_COMMIT_CHILD_RELEASE, release);
        }
        command.spawn().expect("spawn memory commit child")
    }

    #[cfg(unix)]
    fn wait_for_memory_commit_child(child: &mut std::process::Child, ready: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect memory commit child") {
                panic!("memory commit child exited before readiness: {status}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "memory commit child did not become ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn memory_temporary_paths(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory)
            .expect("list memory directory")
            .map(|entry| entry.expect("memory directory entry").path())
            .filter(|path| {
                path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(".nib-memory-") && name.ends_with(".tmp")
                })
            })
            .collect()
    }
}
