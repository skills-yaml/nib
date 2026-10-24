//! T082 phase 4: the session database, `<user home>/nib.db`.
//!
//! Sessions of every project whose state lives in the user home are rows of
//! one SQLite database (bundled `rusqlite`, see the Rust tech doc):
//! - `sessions`: one row per session, keyed by its scope (the session
//!   directory relative to the user home) and id, with the project key, the
//!   profile id, the session `revision`, a `generation` that names this row's
//!   lifetime, and the session document without its messages and events;
//! - `session_messages` and `session_events`: the messages and events, one row
//!   each, indexed by session and project. A save appends the new rows; it
//!   rewrites an earlier row only when the session changed it (event
//!   reconciliation of older audit evidence does);
//! - `session_imports`: the JSON session files imported from a project's
//!   `.nib` (see `crate::state_migration`);
//! - `schema_version`: the applied schema migrations.
//!
//! Every write is one transaction guarded by the row's `generation` and
//! `revision` (`UPDATE ... WHERE generation = ? AND revision = ?`), so a
//! writer holding a stale snapshot is rejected instead of overwriting newer
//! work. The session directory's lock files still serialize the store's
//! read-modify-write operations across processes, so caller code never runs
//! inside a database-wide write transaction. The database uses write-ahead
//! logging, `synchronous=FULL` and a busy timeout.

use super::{Session, SessionError};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Schema version this binary writes. A database with a newer version is
/// refused, so an older binary never writes rows it does not understand.
pub(crate) const SCHEMA_VERSION: i64 = 1;

/// How long a connection waits for another writer.
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// Idle connections kept per database.
const MAX_IDLE_CONNECTIONS: usize = 8;

const SCHEMA_V1: &str = "
CREATE TABLE sessions (
    scope TEXT NOT NULL,
    id TEXT NOT NULL,
    project_key TEXT NOT NULL,
    profile_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    generation TEXT NOT NULL,
    document TEXT NOT NULL,
    byte_len INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (scope, id)
);
CREATE INDEX sessions_by_project ON sessions (project_key, profile_id);
CREATE TABLE session_messages (
    scope TEXT NOT NULL,
    session_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    project_key TEXT NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (scope, session_id, idx),
    FOREIGN KEY (scope, session_id) REFERENCES sessions (scope, id)
        ON DELETE CASCADE ON UPDATE CASCADE
);
CREATE INDEX session_messages_by_project ON session_messages (project_key);
CREATE TABLE session_events (
    scope TEXT NOT NULL,
    session_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    project_key TEXT NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (scope, session_id, idx),
    FOREIGN KEY (scope, session_id) REFERENCES sessions (scope, id)
        ON DELETE CASCADE ON UPDATE CASCADE
);
CREATE INDEX session_events_by_project ON session_events (project_key);
CREATE TABLE session_imports (
    scope TEXT NOT NULL,
    session_id TEXT NOT NULL,
    source TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    imported_at TEXT NOT NULL,
    PRIMARY KEY (scope, session_id)
);
";

/// The sessions of one session directory: its scope in the database and the
/// project and profile it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionScope {
    pub(crate) scope: String,
    pub(crate) project_key: String,
    pub(crate) profile_id: String,
}

impl SessionScope {
    pub(crate) fn new(project_key: &str, profile_id: &str) -> Self {
        Self {
            scope: format!("projects/{project_key}/profiles/{profile_id}/sessions"),
            project_key: project_key.to_string(),
            profile_id: profile_id.to_string(),
        }
    }

    /// The scope of a session directory whose sessions live in the database.
    pub(crate) fn for_sessions_dir(sessions_dir: &Path) -> Option<Self> {
        let (project_key, profile_id) =
            crate::paths::NibPaths::database_session_scope(sessions_dir)?;
        Some(Self::new(&project_key, &profile_id))
    }
}

/// What a loaded row was: the guard of the next write and the stored rows a
/// save compares against, so it writes only what changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionRow {
    pub(crate) generation: String,
    pub(crate) revision: u64,
    pub(crate) messages: Vec<String>,
    pub(crate) events: Vec<String>,
}

/// A session read from the database.
#[derive(Debug)]
pub(crate) struct StoredSession {
    pub(crate) session: Session,
    pub(crate) row: SessionRow,
    pub(crate) byte_len: u64,
    pub(crate) modified: SystemTime,
}

/// A session split into the stored document and its message and event rows.
pub(crate) struct EncodedSession {
    pub(crate) document: String,
    pub(crate) messages: Vec<String>,
    pub(crate) events: Vec<String>,
}

impl EncodedSession {
    pub(crate) fn encode(session: &Session) -> Result<Self, SessionError> {
        let mut value = serde_json::to_value(session)?;
        let object = value.as_object_mut().ok_or_else(|| {
            SessionError::InvalidMutation("a session did not encode as an object".to_string())
        })?;
        let mut rows = |key: &str| -> Result<Vec<String>, SessionError> {
            match object.remove(key) {
                Some(serde_json::Value::Array(items)) => items
                    .iter()
                    .map(|item| serde_json::to_string(item).map_err(SessionError::from))
                    .collect(),
                Some(_) => Err(SessionError::InvalidMutation(format!(
                    "session {key} did not encode as a list"
                ))),
                None => Ok(Vec::new()),
            }
        };
        let messages = rows("messages")?;
        let events = rows("events")?;
        Ok(Self {
            document: serde_json::to_string(&value)?,
            messages,
            events,
        })
    }

    /// Bytes the session takes in the database, bounded like a session file.
    pub(crate) fn byte_len(&self) -> u64 {
        let rows: usize = self
            .messages
            .iter()
            .chain(&self.events)
            .map(String::len)
            .sum();
        (self.document.len() + rows) as u64
    }

    fn decode(&self) -> Result<Session, SessionError> {
        let mut value: serde_json::Value = serde_json::from_str(&self.document)?;
        let object = value.as_object_mut().ok_or_else(|| {
            SessionError::InvalidMutation("a stored session document is not an object".to_string())
        })?;
        for (key, rows) in [("messages", &self.messages), ("events", &self.events)] {
            let items = rows
                .iter()
                .map(|row| serde_json::from_str(row))
                .collect::<Result<Vec<serde_json::Value>, _>>()?;
            object.insert(key.to_string(), serde_json::Value::Array(items));
        }
        Ok(serde_json::from_value(value)?)
    }
}

/// One session database file and its idle connections.
pub(crate) struct SessionDatabase {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
}

impl std::fmt::Debug for SessionDatabase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionDatabase")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

type DatabaseRegistry = Mutex<HashMap<PathBuf, Arc<SessionDatabase>>>;

static DATABASES: OnceLock<DatabaseRegistry> = OnceLock::new();

/// A pooled connection, returned to its database when dropped.
struct PooledConnection<'a> {
    database: &'a SessionDatabase,
    connection: Option<Connection>,
}

impl std::ops::Deref for PooledConnection<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.connection.as_ref().expect("pooled connection")
    }
}

impl std::ops::DerefMut for PooledConnection<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.connection.as_mut().expect("pooled connection")
    }
}

impl Drop for PooledConnection<'_> {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            if let Ok(mut idle) = self.database.idle.lock() {
                if idle.len() < MAX_IDLE_CONNECTIONS {
                    idle.push(connection);
                }
            }
        }
    }
}

fn database_error(path: &Path, error: impl std::fmt::Display) -> SessionError {
    SessionError::Database(format!("{}: {error}", path.display()))
}

fn millis(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
        i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
    })
}

fn from_millis(value: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(u64::try_from(value).unwrap_or_default())
}

impl SessionDatabase {
    /// The database of the user home, `<user home>/nib.db`.
    pub(crate) fn for_user_home() -> Result<Arc<Self>, SessionError> {
        let path = crate::paths::NibPaths::session_database().ok_or_else(|| {
            SessionError::Database("nib has no user home for its session database".to_string())
        })?;
        Self::open(&path)
    }

    /// Opens (creating it owner-only if needed) and migrates the database at
    /// `path`. One handle is shared per path in a process.
    pub(crate) fn open(path: &Path) -> Result<Arc<Self>, SessionError> {
        let registry = DATABASES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut registry = registry
            .lock()
            .map_err(|_| SessionError::LockPoisoned(path.display().to_string()))?;
        if let Some(database) = registry.get(path) {
            if crate::fs_security::path_entry_exists(path).unwrap_or(false) {
                return Ok(database.clone());
            }
        }
        let database = Arc::new(Self {
            path: path.to_path_buf(),
            idle: Mutex::new(Vec::new()),
        });
        let mut connection = database.connect()?;
        migrate(&mut connection).map_err(|error| database_error(path, error))?;
        database
            .idle
            .lock()
            .map_err(|_| SessionError::LockPoisoned(path.display().to_string()))?
            .push(connection);
        registry.insert(path.to_path_buf(), database.clone());
        Ok(database)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn connect(&self) -> Result<Connection, SessionError> {
        create_private_file(&self.path)?;
        let connection =
            Connection::open(&self.path).map_err(|error| database_error(&self.path, error))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|error| database_error(&self.path, error))?;
        let mode: String = connection
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .map_err(|error| database_error(&self.path, error))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(database_error(
                &self.path,
                format!("write-ahead logging is unavailable (journal mode {mode})"),
            ));
        }
        connection
            .execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")
            .map_err(|error| database_error(&self.path, error))?;
        Ok(connection)
    }

    fn connection(&self) -> Result<PooledConnection<'_>, SessionError> {
        let idle = self
            .idle
            .lock()
            .map_err(|_| SessionError::LockPoisoned(self.path.display().to_string()))?
            .pop();
        let connection = match idle {
            Some(connection) => connection,
            None => self.connect()?,
        };
        Ok(PooledConnection {
            database: self,
            connection: Some(connection),
        })
    }

    /// Loads one session, or `None` when the scope has no such row.
    pub(crate) fn load(
        &self,
        scope: &SessionScope,
        id: &str,
    ) -> Result<Option<StoredSession>, SessionError> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| database_error(&self.path, error))?;
        let stored = load_in(&transaction, scope, id).map_err(|error| match error {
            LoadError::Database(error) => database_error(&self.path, error),
            LoadError::Session(error) => error,
        })?;
        transaction
            .commit()
            .map_err(|error| database_error(&self.path, error))?;
        Ok(stored)
    }

    /// Writes `session` in one transaction. `expected` is the row the caller
    /// loaded (`None`: the session must not exist yet); a row that changed
    /// since is a revision conflict and nothing is written. `before_commit`
    /// runs inside the transaction, right before it commits, and can still
    /// abort it. Returns the row now stored.
    pub(crate) fn write(
        &self,
        scope: &SessionScope,
        session: &Session,
        encoded: EncodedSession,
        expected: Option<&SessionRow>,
        before_commit: impl FnOnce() -> Result<(), SessionError>,
    ) -> Result<SessionRow, SessionError> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| database_error(&self.path, error))?;
        let revision = i64::try_from(session.revision).map_err(|_| {
            SessionError::InvalidMutation("session revision exceeds the database range".to_string())
        })?;
        let byte_len = i64::try_from(encoded.byte_len()).unwrap_or(i64::MAX);
        let now = millis(SystemTime::now());
        let database = |error: rusqlite::Error| database_error(&self.path, error);
        let generation = match expected {
            None => {
                let generation = uuid::Uuid::new_v4().to_string();
                let inserted = transaction
                    .execute(
                        "INSERT INTO sessions (scope, id, project_key, profile_id, revision, generation, document, byte_len, updated_at_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT DO NOTHING",
                        params![
                            scope.scope,
                            session.id,
                            scope.project_key,
                            scope.profile_id,
                            revision,
                            generation,
                            encoded.document,
                            byte_len,
                            now
                        ],
                    )
                    .map_err(database)?;
                if inserted != 1 {
                    return Err(SessionError::InvalidMutation(format!(
                        "session {} already exists; a concurrent writer created it",
                        session.id
                    )));
                }
                generation
            }
            Some(row) => {
                let previous = i64::try_from(row.revision).unwrap_or(i64::MAX);
                let updated = transaction
                    .execute(
                        "UPDATE sessions SET revision = ?1, document = ?2, byte_len = ?3, updated_at_ms = ?4
                         WHERE scope = ?5 AND id = ?6 AND generation = ?7 AND revision = ?8",
                        params![
                            revision,
                            encoded.document,
                            byte_len,
                            now,
                            scope.scope,
                            session.id,
                            row.generation,
                            previous
                        ],
                    )
                    .map_err(database)?;
                if updated != 1 {
                    return Err(SessionError::InvalidMutation(format!(
                        "stale session revision for {}: the stored session changed after revision {} was read",
                        session.id, row.revision
                    )));
                }
                row.generation.clone()
            }
        };
        let empty = Vec::new();
        for (table, previous, current) in [
            (
                "session_messages",
                expected.map_or(&empty, |row| &row.messages),
                &encoded.messages,
            ),
            (
                "session_events",
                expected.map_or(&empty, |row| &row.events),
                &encoded.events,
            ),
        ] {
            write_rows(&transaction, table, scope, &session.id, previous, current)
                .map_err(database)?;
        }
        before_commit()?;
        transaction.commit().map_err(database)?;
        Ok(SessionRow {
            generation,
            revision: session.revision,
            messages: encoded.messages,
            events: encoded.events,
        })
    }

    /// Deletes the session `row` names, with its messages and events.
    /// `before_delete` runs inside the transaction and can abort it.
    pub(crate) fn delete(
        &self,
        scope: &SessionScope,
        id: &str,
        row: &SessionRow,
        before_delete: impl FnOnce() -> Result<(), SessionError>,
    ) -> Result<(), SessionError> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| database_error(&self.path, error))?;
        let deleted = transaction
            .execute(
                "DELETE FROM sessions WHERE scope = ?1 AND id = ?2 AND generation = ?3 AND revision = ?4",
                params![
                    scope.scope,
                    id,
                    row.generation,
                    i64::try_from(row.revision).unwrap_or(i64::MAX)
                ],
            )
            .map_err(|error| database_error(&self.path, error))?;
        if deleted != 1 {
            return Err(SessionError::InvalidMutation(format!(
                "session {id} changed before it could be deleted and was kept"
            )));
        }
        before_delete()?;
        transaction
            .commit()
            .map_err(|error| database_error(&self.path, error))
    }

    /// The ids and stored sizes of a scope's sessions, sorted by id.
    pub(crate) fn list(&self, scope: &SessionScope) -> Result<Vec<(String, u64)>, SessionError> {
        let connection = self.connection()?;
        let database = |error: rusqlite::Error| database_error(&self.path, error);
        let mut statement = connection
            .prepare("SELECT id, byte_len FROM sessions WHERE scope = ?1 ORDER BY id")
            .map_err(database)?;
        let rows = statement
            .query_map(params![scope.scope], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    u64::try_from(row.get::<_, i64>(1)?).unwrap_or_default(),
                ))
            })
            .map_err(database)?;
        rows.collect::<Result<_, _>>().map_err(database)
    }

    /// Whether the scope holds a session other than `id`.
    pub(crate) fn has_other_sessions(
        &self,
        scope: &SessionScope,
        id: &str,
    ) -> Result<bool, SessionError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sessions WHERE scope = ?1 AND id <> ?2)",
                params![scope.scope, id],
                |row| row.get(0),
            )
            .map_err(|error| database_error(&self.path, error))
    }

    /// Removes a session a preparation created, unless it changed: with a
    /// known `generation` the row must still have it; without one, the stored
    /// session must equal `planned`. Returns whether a row was removed; a
    /// changed row is an error and is kept.
    pub(crate) fn remove_prepared(
        &self,
        scope: &SessionScope,
        planned: &Session,
        generation: Option<&str>,
    ) -> Result<bool, SessionError> {
        let Some(stored) = self.load(scope, &planned.id)? else {
            return Ok(false);
        };
        let unchanged = match generation {
            Some(generation) => stored.row.generation == generation,
            None => stored.session == *planned,
        };
        if !unchanged {
            return Err(SessionError::InvalidMutation(format!(
                "prepared session {} changed and was preserved",
                planned.id
            )));
        }
        self.delete(scope, &planned.id, &stored.row, || Ok(()))?;
        Ok(true)
    }

    /// Imports one JSON session file into `scope`, in one transaction. An
    /// identical session already there (an interrupted import) is accepted;
    /// a different one is a conflict and nothing changes.
    pub(crate) fn import(
        &self,
        scope: &SessionScope,
        session: &Session,
        source: &Path,
        sha256: &str,
        modified: SystemTime,
    ) -> Result<ImportOutcome, SessionError> {
        if let Some(stored) = self.load(scope, &session.id)? {
            return Ok(if stored.session == *session {
                self.record_import(scope, &session.id, source, sha256)?;
                ImportOutcome::AlreadyImported
            } else {
                ImportOutcome::Conflict
            });
        }
        let encoded = EncodedSession::encode(session)?;
        let byte_len = i64::try_from(encoded.byte_len()).unwrap_or(i64::MAX);
        let mut connection = self.connection()?;
        let database = |error: rusqlite::Error| database_error(&self.path, error);
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database)?;
        let inserted = transaction
            .execute(
                "INSERT INTO sessions (scope, id, project_key, profile_id, revision, generation, document, byte_len, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT DO NOTHING",
                params![
                    scope.scope,
                    session.id,
                    scope.project_key,
                    scope.profile_id,
                    i64::try_from(session.revision).unwrap_or(i64::MAX),
                    uuid::Uuid::new_v4().to_string(),
                    encoded.document,
                    byte_len,
                    millis(modified)
                ],
            )
            .map_err(database)?;
        if inserted != 1 {
            return Ok(ImportOutcome::Conflict);
        }
        let empty = Vec::new();
        write_rows(
            &transaction,
            "session_messages",
            scope,
            &session.id,
            &empty,
            &encoded.messages,
        )
        .map_err(database)?;
        write_rows(
            &transaction,
            "session_events",
            scope,
            &session.id,
            &empty,
            &encoded.events,
        )
        .map_err(database)?;
        insert_import(&transaction, scope, &session.id, source, sha256).map_err(database)?;
        transaction.commit().map_err(database)?;
        Ok(ImportOutcome::Imported)
    }

    fn record_import(
        &self,
        scope: &SessionScope,
        id: &str,
        source: &Path,
        sha256: &str,
    ) -> Result<(), SessionError> {
        let connection = self.connection()?;
        insert_import(&connection, scope, id, source, sha256)
            .map_err(|error| database_error(&self.path, error))
    }

    /// Number of sessions stored for each `(project key, profile id)`.
    pub(crate) fn session_counts(&self) -> Result<Vec<(String, String, u64)>, SessionError> {
        let connection = self.connection()?;
        let database = |error: rusqlite::Error| database_error(&self.path, error);
        let mut statement = connection
            .prepare(
                "SELECT project_key, profile_id, COUNT(*) FROM sessions
                 GROUP BY project_key, profile_id ORDER BY project_key, profile_id",
            )
            .map_err(database)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    u64::try_from(row.get::<_, i64>(2)?).unwrap_or_default(),
                ))
            })
            .map_err(database)?;
        rows.collect::<Result<_, _>>().map_err(database)
    }

    /// Moves every session of `from` to the project key `to` (a project that
    /// was moved and re-linked, see `nib doctor --fix`). Returns how many
    /// sessions moved. A session id present under both keys is a conflict
    /// and nothing moves.
    pub(crate) fn relink_project(&self, from: &str, to: &str) -> Result<u64, SessionError> {
        let mut connection = self.connection()?;
        let database = |error: rusqlite::Error| database_error(&self.path, error);
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database)?;
        let from_prefix = format!("projects/{from}/");
        let to_prefix = format!("projects/{to}/");
        let conflicts: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM sessions AS old JOIN sessions AS new
                 ON new.scope = ?2 || substr(old.scope, length(?1) + 1) AND new.id = old.id
                 WHERE old.project_key = ?3",
                params![from_prefix, to_prefix, from],
                |row| row.get(0),
            )
            .map_err(database)?;
        if conflicts > 0 {
            return Err(SessionError::InvalidMutation(format!(
                "{conflicts} sessions exist for both project keys {from} and {to}; nothing was moved"
            )));
        }
        let moved = transaction
            .execute(
                "UPDATE sessions SET project_key = ?3, scope = ?2 || substr(scope, length(?1) + 1)
                 WHERE project_key = ?4 AND substr(scope, 1, length(?1)) = ?1",
                params![from_prefix, to_prefix, to, from],
            )
            .map_err(database)?;
        for table in ["session_messages", "session_events"] {
            transaction
                .execute(
                    &format!("UPDATE {table} SET project_key = ?2 WHERE project_key = ?1"),
                    params![from, to],
                )
                .map_err(database)?;
        }
        transaction
            .execute(
                "UPDATE session_imports SET scope = ?2 || substr(scope, length(?1) + 1)
                 WHERE substr(scope, 1, length(?1)) = ?1",
                params![from_prefix, to_prefix],
            )
            .map_err(database)?;
        transaction.commit().map_err(database)?;
        Ok(moved as u64)
    }
}

/// What an import of one session file did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImportOutcome {
    Imported,
    AlreadyImported,
    Conflict,
}

enum LoadError {
    Database(rusqlite::Error),
    Session(SessionError),
}

impl From<rusqlite::Error> for LoadError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

fn load_in(
    connection: &Connection,
    scope: &SessionScope,
    id: &str,
) -> Result<Option<StoredSession>, LoadError> {
    let row = connection
        .query_row(
            "SELECT revision, generation, document, byte_len, updated_at_ms FROM sessions
             WHERE scope = ?1 AND id = ?2",
            params![scope.scope, id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((revision, generation, document, byte_len, updated_at)) = row else {
        return Ok(None);
    };
    let byte_len = u64::try_from(byte_len).unwrap_or_default();
    if byte_len > super::MAX_SESSION_JSON_BYTES {
        return Err(LoadError::Session(SessionError::FileTooLarge {
            path: format!("{}/{id}", scope.scope),
            size: byte_len,
            max: super::MAX_SESSION_JSON_BYTES,
        }));
    }
    let rows = |table: &str| -> Result<Vec<String>, rusqlite::Error> {
        let mut statement = connection.prepare(&format!(
            "SELECT idx, body FROM {table} WHERE scope = ?1 AND session_id = ?2 ORDER BY idx"
        ))?;
        let mut bodies = Vec::new();
        let mut rows = statement.query(params![scope.scope, id])?;
        while let Some(row) = rows.next()? {
            let index: i64 = row.get(0)?;
            if usize::try_from(index).ok() != Some(bodies.len()) {
                return Err(rusqlite::Error::IntegralValueOutOfRange(0, index));
            }
            bodies.push(row.get::<_, String>(1)?);
        }
        Ok(bodies)
    };
    let encoded = EncodedSession {
        document,
        messages: rows("session_messages")?,
        events: rows("session_events")?,
    };
    let session = encoded.decode().map_err(LoadError::Session)?;
    let revision = u64::try_from(revision).unwrap_or_default();
    if session.id != id {
        return Err(LoadError::Session(SessionError::SessionIdMismatch {
            expected: id.to_string(),
            actual: session.id,
        }));
    }
    if session.revision != revision {
        return Err(LoadError::Session(SessionError::InvalidMutation(format!(
            "stored session {id} has revision {} but its row has {revision}",
            session.revision
        ))));
    }
    session.validate().map_err(LoadError::Session)?;
    Ok(Some(StoredSession {
        session,
        row: SessionRow {
            generation,
            revision,
            messages: encoded.messages,
            events: encoded.events,
        },
        byte_len,
        modified: from_millis(updated_at),
    }))
}

/// Brings `previous` rows to `current`: appends new rows and rewrites or
/// removes only the rows that changed.
fn write_rows(
    connection: &Connection,
    table: &str,
    scope: &SessionScope,
    id: &str,
    previous: &[String],
    current: &[String],
) -> Result<(), rusqlite::Error> {
    if current.len() < previous.len() {
        connection.execute(
            &format!("DELETE FROM {table} WHERE scope = ?1 AND session_id = ?2 AND idx >= ?3"),
            params![scope.scope, id, current.len() as i64],
        )?;
    }
    let mut update = connection.prepare(&format!(
        "UPDATE {table} SET body = ?4 WHERE scope = ?1 AND session_id = ?2 AND idx = ?3"
    ))?;
    let mut insert = connection.prepare(&format!(
        "INSERT INTO {table} (scope, session_id, idx, project_key, body) VALUES (?1, ?2, ?3, ?4, ?5)"
    ))?;
    for (index, body) in current.iter().enumerate() {
        match previous.get(index) {
            Some(stored) if stored == body => {}
            Some(_) => {
                update.execute(params![scope.scope, id, index as i64, body])?;
            }
            None => {
                insert.execute(params![
                    scope.scope,
                    id,
                    index as i64,
                    scope.project_key,
                    body
                ])?;
            }
        }
    }
    Ok(())
}

fn insert_import(
    connection: &Connection,
    scope: &SessionScope,
    id: &str,
    source: &Path,
    sha256: &str,
) -> Result<(), rusqlite::Error> {
    connection.execute(
        "INSERT INTO session_imports (scope, session_id, source, sha256, imported_at)
         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING",
        params![
            scope.scope,
            id,
            source.display().to_string(),
            sha256,
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

/// Applies the schema migrations this binary knows, once, in a transaction.
fn migrate(connection: &mut Connection) -> Result<(), String> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            )",
        )
        .map_err(|error| error.to_string())?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let current: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if current > SCHEMA_VERSION {
        return Err(format!(
            "the session database has schema version {current}, newer than this nib understands ({SCHEMA_VERSION}); use a newer nib"
        ));
    }
    if current < 1 {
        transaction
            .execute_batch(SCHEMA_V1)
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT INTO schema_version (version, applied_at) VALUES (1, ?1)",
                params![chrono::Utc::now().to_rfc3339()],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())
}

/// Creates the database file owner-only (`0600`) before SQLite opens it, in
/// a private user home, and refuses anything but a regular file.
fn create_private_file(path: &Path) -> Result<(), SessionError> {
    if let Some(parent) = path.parent() {
        crate::state_migration::ensure_state_directory(parent)
            .map_err(|error| database_error(parent, error))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|error| database_error(path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| database_error(path, error))?;
    if !metadata.is_file() {
        return Err(database_error(path, "the session database is not a file"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "database_tests.rs"]
mod tests;
