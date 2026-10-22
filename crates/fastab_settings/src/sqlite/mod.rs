use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use fastab_util::directories::fig_data_dir;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::types::FromSql;
use rusqlite::{Connection, Error, ToSql, params};
use serde_json::Map;
use tracing::{info, warn};

use crate::Result;
use crate::error::DbOpenError;

const STATE_TABLE_NAME: &str = "state";
const AUTH_TABLE_NAME: &str = "auth_kv";
const POOL_MAX_SIZE: u32 = 4;

static LIGHT_POOL: AtomicBool = AtomicBool::new(false);
/// Set once [`DATABASE`] starts opening, after which the pool shape is fixed.
static DATABASE_OPENED: AtomicBool = AtomicBool::new(false);

/// Opts this process into a pool that keeps nothing open between uses: one
/// maintenance thread instead of r2d2's three, and no idle connection held.
/// For `fterm`, which stays resident once per terminal tab and only touches
/// the database at startup and once per finished command. Takes effect only
/// before the first [`database()`] call; a later call is a bug in the caller,
/// so it is reported instead of being ignored silently.
pub fn prefer_light_pool() {
    LIGHT_POOL.store(true, Ordering::SeqCst);
    let too_late = DATABASE_OPENED.load(Ordering::SeqCst);
    if too_late {
        warn!("prefer_light_pool() ran after the database was opened; the full pool stays");
    }
    debug_assert!(
        !too_late,
        "prefer_light_pool() must run before the first database() call"
    );
}

pub static DATABASE: LazyLock<Result<Db, DbOpenError>> = LazyLock::new(|| {
    DATABASE_OPENED.store(true, Ordering::SeqCst);
    let db = Db::new().map_err(|e| DbOpenError(e.to_string()))?;
    db.migrate().map_err(|e| DbOpenError(e.to_string()))?;
    Ok(db)
});

pub fn database() -> Result<&'static Db, DbOpenError> {
    match DATABASE.as_ref() {
        Ok(db) => Ok(db),
        Err(err) => Err(err.clone()),
    }
}

#[derive(Debug)]
struct Migration {
    name: &'static str,
    sql: &'static str,
}

macro_rules! migrations {
    ($($name:expr),*) => {{
        &[
            $(
                Migration {
                    name: $name,
                    sql: include_str!(concat!("migrations/", $name, ".sql")),
                }
            ),*
        ]
    }};
}

const MIGRATIONS: &[Migration] = migrations![
    "000_migration_table",
    "001_history_table",
    "002_drop_history_in_ssh_docker",
    "003_improved_history_timing",
    "004_state_table",
    "005_auth_table"
];

#[derive(Debug, Clone)]
pub struct Db {
    pub(crate) pool: Pool<SqliteConnectionManager>,
}

impl Db {
    fn path() -> Result<PathBuf> {
        Ok(fig_data_dir()?.join("data.sqlite3"))
    }

    pub fn new() -> Result<Self> {
        Self::open(&Self::path()?)
    }

    fn open(path: &Path) -> Result<Self> {
        Self::open_pool(
            path,
            std::time::Duration::from_secs(60),
            LIGHT_POOL.load(Ordering::SeqCst),
        )
    }

    #[cfg(test)]
    fn open_with_idle_timeout(path: &Path, idle_timeout: std::time::Duration) -> Result<Self> {
        Self::open_pool(path, idle_timeout, false)
    }

    fn open_pool(path: &Path, idle_timeout: std::time::Duration, light: bool) -> Result<Self> {
        // make the parent dir if it doesnt exist
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }

        // Create/restrict the main file before SQLite opens it: WAL and SHM
        // inherit its mode. Repair existing sidecars before any secret write.
        #[cfg(unix)]
        protect_database_files(path)?;

        let conn = SqliteConnectionManager::file(path).with_init(init_connection);
        // The default r2d2 checkout timeout is 30s. The completion engine's
        // supervisor thread reads this database between requests; if wedged
        // generator threads are sitting on connections, a 30s wait there
        // freezes every completion in the queue. Fail fast instead — callers
        // already treat database errors as best-effort.
        //
        // Keep room for concurrent readers without prebuilding four SQLite
        // connections and retaining their page caches in every process.
        // r2d2 otherwise defaults min_idle to max_size. Its 30s reaper closes
        // expired idle connections, then replenishes the one-connection floor;
        // it does not preserve a particular connection indefinitely. With the
        // production timeout, a fully idle pool shrinks after about 60-90s.
        let mut builder = Pool::builder()
            .max_size(POOL_MAX_SIZE)
            .min_idle(Some(if light { 0 } else { 1 }))
            .idle_timeout(Some(idle_timeout))
            .connection_timeout(std::time::Duration::from_secs(3));
        if light {
            let thread_pool = scheduled_thread_pool::ScheduledThreadPool::builder()
                .num_threads(1)
                .thread_name_pattern("sqlite-pool-{}")
                .build();
            builder = builder.thread_pool(Arc::new(thread_pool));
        }
        let pool = builder.build(conn)?;

        // Also check sidecars created while the pool was initialized.
        #[cfg(unix)]
        protect_database_files(path)?;

        Ok(Self { pool })
    }

    /// Isolated SQLite storage for downstream credential tests; never opens
    /// the user's database or installs a process-wide override.
    #[cfg(feature = "test-support")]
    pub fn open_test(path: &Path) -> Result<Self> {
        let db = Self::open(path)?;
        db.migrate()?;
        Ok(db)
    }

    pub(crate) fn mock() -> Self {
        let conn = SqliteConnectionManager::memory();
        let pool = Pool::builder().build(conn).unwrap();
        Self { pool }
    }

    pub fn migrate(&self) -> Result<()> {
        let mut conn = self.pool.get()?;
        let transaction = conn.transaction()?;

        let max_version = max_migration_version(&transaction);

        for (version, migration) in MIGRATIONS.iter().enumerate() {
            if has_migration(&transaction, version, max_version)? {
                continue;
            }

            // execute the migration
            transaction.execute_batch(migration.sql)?;

            info!(%version, name =% migration.name, "Applying migration");

            // insert the migration entry
            transaction.execute(
                "INSERT INTO migrations (version, migration_time) VALUES (?1, strftime('%s', 'now'));",
                params![version],
            )?;
        }

        // commit the transaction
        transaction.commit()?;

        Ok(())
    }

    fn get_value<T: FromSql>(&self, table: &'static str, key: impl AsRef<str>) -> Result<Option<T>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!("SELECT value FROM {table} WHERE key = ?1"))?;
        match stmt.query_row([key.as_ref()], |row| row.get(0)) {
            Ok(data) => Ok(Some(data)),
            Err(Error::QueryReturnedNoRows) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub fn get_state_value(&self, key: impl AsRef<str>) -> Result<Option<serde_json::Value>> {
        self.get_value(STATE_TABLE_NAME, key)
    }

    pub fn get_auth_value(&self, key: impl AsRef<str>) -> Result<Option<String>> {
        self.get_value(AUTH_TABLE_NAME, key)
    }

    fn set_value<T: ToSql>(&self, table: &'static str, key: impl AsRef<str>, value: T) -> Result<()> {
        self.pool.get()?.execute(
            &format!("INSERT OR REPLACE INTO {table} (key, value) VALUES (?1, ?2)"),
            params![key.as_ref(), value],
        )?;
        Ok(())
    }

    pub fn set_state_value(&self, key: impl AsRef<str>, value: impl Into<serde_json::Value>) -> Result<()> {
        self.set_value(STATE_TABLE_NAME, key, value.into())
    }

    pub fn set_auth_value(&self, key: impl AsRef<str>, value: impl Into<String>) -> Result<()> {
        self.set_value(AUTH_TABLE_NAME, key, value.into())
    }

    fn unset_value(&self, table: &'static str, key: impl AsRef<str>) -> Result<()> {
        self.pool
            .get()?
            .execute(&format!("DELETE FROM {table} WHERE key = ?1"), [key.as_ref()])?;
        Ok(())
    }

    pub fn unset_state_value(&self, key: impl AsRef<str>) -> Result<()> {
        self.unset_value(STATE_TABLE_NAME, key)
    }

    pub fn unset_auth_value(&self, key: impl AsRef<str>) -> Result<()> {
        self.unset_value(AUTH_TABLE_NAME, key)
    }

    fn is_value_set(&self, table: &'static str, key: impl AsRef<str>) -> Result<bool> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!("SELECT value FROM {table} WHERE key = ?1"))?;
        match stmt.query_row([key.as_ref()], |_| Ok(())) {
            Ok(()) => Ok(true),
            Err(Error::QueryReturnedNoRows) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    pub fn is_state_value_set(&self, key: impl AsRef<str>) -> Result<bool> {
        self.is_value_set(STATE_TABLE_NAME, key)
    }

    pub fn is_auth_value_set(&self, key: impl AsRef<str>) -> Result<bool> {
        self.is_value_set(AUTH_TABLE_NAME, key)
    }

    fn all_values(&self, table: &'static str) -> Result<Map<String, serde_json::Value>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!("SELECT key, value FROM {table}"))?;
        let rows = stmt.query_map([], |row| {
            let key: String = row.get(0)?;
            let value: serde_json::Value = row.get(1)?;
            Ok((key, value))
        })?;

        let mut map = Map::new();
        for (key, value) in rows.flatten() {
            map.insert(key, value);
        }

        Ok(map)
    }

    pub fn all_state_values(&self) -> Result<Map<String, serde_json::Value>> {
        self.all_values(STATE_TABLE_NAME)
    }

    // atomic style operations

    fn atomic_op<T: FromSql + ToSql>(
        &self,
        key: impl AsRef<str>,
        op: impl FnOnce(&Option<T>) -> Option<T>,
    ) -> Result<Option<T>> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;

        let value = tx.query_row::<Option<T>, _, _>(
            &format!("SELECT value FROM {STATE_TABLE_NAME} WHERE key = ?1"),
            [key.as_ref()],
            |row| row.get(0),
        );

        let value_0: Option<T> = match value {
            Ok(value) => value,
            Err(Error::QueryReturnedNoRows) => None,
            Err(err) => return Err(err.into()),
        };

        let value_1 = op(&value_0);

        if let Some(value) = value_1 {
            tx.execute(
                &format!("INSERT OR REPLACE INTO {STATE_TABLE_NAME} (key, value) VALUES (?1, ?2)"),
                params![key.as_ref(), value],
            )?;
        } else {
            tx.execute(
                &format!("DELETE FROM {STATE_TABLE_NAME} WHERE key = ?1"),
                [key.as_ref()],
            )?;
        }

        tx.commit()?;

        Ok(value_0)
    }

    /// Atomically get the value of a key, then perform an or operation on it
    /// and set the new value. If the key does not exist, set it to the or value.
    pub fn atomic_bool_or(&self, key: impl AsRef<str>, or: bool) -> Result<bool> {
        self.atomic_op::<serde_json::Value>(key, |val| match val {
            // Some(val) => Some(serde_json::Value::Bool( || or)),
            Some(serde_json::Value::Bool(b)) => Some(serde_json::Value::Bool(*b || or)),
            Some(_) | None => Some(serde_json::Value::Bool(or)),
        })
        .map(|val| val.and_then(|val| val.as_bool()).unwrap_or(false))
    }
}

#[cfg(unix)]
fn protect_database_files(path: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::fs::OpenOptions;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    // Close a newly created file before another Db::open can start SQLite on
    // it. Closing any descriptor for an existing SQLite file would release
    // this process's POSIX locks, including the SHM deadman-switch lock.
    static CREATE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    let _guard = CREATE_LOCK.lock();

    fn restrict(path: &Path, create: bool) -> std::io::Result<()> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                match OpenOptions::new().write(true).create_new(true).mode(0o600).open(path) {
                    Ok(file) => drop(file),
                    // Another process may have created the database. Never
                    // open an existing file outside SQLite, even to chmod it.
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
                    Err(error) => return Err(error),
                }
                std::fs::symlink_metadata(path)?
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "SQLite storage must be a regular file",
            ));
        }
        let c_path = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: c_path is NUL-terminated and lives through the call. A
        // path-based chmod keeps SQLite's file descriptors and locks intact;
        // NOFOLLOW prevents a replaced symlink from changing its target.
        let result = unsafe { libc::fchmodat(libc::AT_FDCWD, c_path.as_ptr(), 0o600, libc::AT_SYMLINK_NOFOLLOW) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            // SQLite may remove a sidecar concurrently. New sidecars inherit
            // the restricted main mode.
            if !create && error.kind() == std::io::ErrorKind::NotFound {
                return Ok(());
            }
            return Err(error);
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => Ok(()),
            Ok(_) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "SQLite storage must be a regular file",
            )),
            Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    restrict(path, true)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        restrict(Path::new(&sidecar), false)?;
    }
    Ok(())
}

/// Applied to every pooled connection of the on-disk database.
///
/// fterm inserts history rows while the desktop reads them; without WAL a
/// writer blocks readers for the whole transaction, and without a busy
/// timeout a contended statement fails immediately with `SQLITE_BUSY`. WAL
/// lets the reader and writer proceed concurrently, and the busy timeout
/// bounds the residual contention instead of surfacing it as flaky errors.
fn init_connection(conn: &mut Connection) -> std::result::Result<(), Error> {
    conn.busy_timeout(std::time::Duration::from_secs(1))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

fn max_migration_version<C: Deref<Target = Connection>>(conn: &C) -> Option<i64> {
    let mut stmt = conn.prepare("SELECT MAX(version) FROM migrations").ok()?;
    stmt.query_row([], |row| row.get(0)).ok()
}

fn has_migration<C: Deref<Target = Connection>>(conn: &C, version: usize, max_version: Option<i64>) -> Result<bool> {
    // IMPORTANT: Due to a bug with the first 7 migrations, we have to check manually
    //
    // Background: the migrations table stores two identifying keys: the sqlite auto-generated
    // auto-incrementing key `id`, and the `version` which is the index of the `MIGRATIONS`
    // constant.
    //
    // Checking whether a migration exists would compare id with version, but since id is 1-indexed
    // and version is 0-indexed, we would actually skip the last migration! Therefore, it's
    // possible users are missing a critical migration (namely, auth_kv table creation) when
    // upgrading to the qchat build (which includes two new migrations). Hence, we have to check
    // all migrations until version 7 to make sure that nothing is missed.
    if version <= 7 {
        let mut stmt = match conn.prepare("SELECT COUNT(*) FROM migrations WHERE version = ?1") {
            Ok(stmt) => stmt,
            // If the migrations table does not exist, then we can reasonably say no migrations
            // will exist.
            Err(Error::SqliteFailure(_, Some(msg))) if msg.contains("no such table") => {
                return Ok(false);
            },
            Err(err) => return Err(err.into()),
        };
        let count: i32 = stmt.query_row([version], |row| row.get(0))?;
        return Ok(count >= 1);
    }

    // Continuing from the previously implemented logic - any migrations after the 7th can have a simple
    // maximum version check, since we can reasonably assume if any version >=7 will have all
    // migrations prior to it.
    #[allow(clippy::match_like_matches_macro)]
    Ok(match max_version {
        Some(max_version) if max_version >= version as i64 => true,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock() -> Db {
        let db = Db::mock();
        db.migrate().unwrap();
        db
    }

    #[test]
    fn file_pool_starts_small_and_grows_for_concurrent_checkouts() {
        let tempdir = tempfile::tempdir().unwrap();
        let db = Db::open(&tempdir.path().join("data.sqlite3")).unwrap();
        assert_eq!(db.pool.state().connections, 1);
        assert_eq!(db.pool.state().idle_connections, 1);

        let connections: Vec<_> = (0..POOL_MAX_SIZE).map(|_| db.pool.get().unwrap()).collect();
        assert_eq!(db.pool.state().connections, POOL_MAX_SIZE);
        assert_eq!(db.pool.state().idle_connections, 0);
        assert!(db.pool.try_get().is_none());
        drop(connections);
    }

    #[test]
    fn light_pool_opens_connections_on_demand_and_keeps_none_idle() {
        let tempdir = tempfile::tempdir().unwrap();
        let db = Db::open_pool(
            &tempdir.path().join("data.sqlite3"),
            std::time::Duration::from_secs(60),
            true,
        )
        .unwrap();
        assert_eq!(db.pool.min_idle(), Some(0));
        assert_eq!(db.pool.state().connections, 0);
        db.migrate().unwrap();
        db.set_state_value("light", true).unwrap();
        assert_eq!(db.get_state_value("light").unwrap(), Some(serde_json::json!(true)));
    }

    #[test]
    fn file_pool_retires_idle_connections_without_losing_data_or_a_checked_out_transaction() {
        use std::time::{Duration, Instant};

        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("data.sqlite3");
        // Exercise r2d2's real 30s reaper, shortening only the idle threshold.
        let db = Db::open_with_idle_timeout(&path, Duration::from_millis(100)).unwrap();
        db.migrate().unwrap();
        db.set_state_value("retained", "before").unwrap();

        let mut connections: Vec<_> = (0..POOL_MAX_SIZE).map(|_| db.pool.get().unwrap()).collect();
        let mut held = connections.pop().unwrap();
        let transaction = held.transaction().unwrap();
        transaction
            .execute_batch("CREATE TEMP TABLE held_connection(value); INSERT INTO held_connection VALUES (42)")
            .unwrap();
        transaction
            .execute(
                "UPDATE state SET value = ?1 WHERE key = 'retained'",
                [serde_json::json!("after")],
            )
            .unwrap();
        drop(connections);

        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let state = db.pool.state();
            if state.connections == 2 && state.idle_connections == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "idle connections were not retired: {state:?}"
            );
            std::thread::sleep(Duration::from_millis(250));
        }

        assert_eq!(
            db.get_state_value("retained").unwrap(),
            Some(serde_json::json!("before"))
        );
        let held_value: i64 = transaction
            .query_row("SELECT value FROM held_connection", [], |row| row.get(0))
            .unwrap();
        assert_eq!(held_value, 42);
        transaction.commit().unwrap();
        drop(held);
        drop(db);

        let reopened = Db::open(&path).unwrap();
        assert_eq!(
            reopened.get_state_value("retained").unwrap(),
            Some(serde_json::json!("after"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn permission_repairs_preserve_cross_process_sqlite_locks() {
        const CHILD_PATH: &str = "FASTAB_SQLITE_LOCK_TEST_PATH";
        const TEST_NAME: &str = "sqlite::tests::permission_repairs_preserve_cross_process_sqlite_locks";

        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let conn = Connection::open(path).unwrap();
            conn.busy_timeout(std::time::Duration::ZERO).unwrap();
            let result = conn.execute_batch("BEGIN IMMEDIATE");
            assert!(
                matches!(
                    result,
                    Err(Error::SqliteFailure(ref error, _))
                        if error.code == rusqlite::ErrorCode::DatabaseBusy
                ),
                "another process acquired a write lock held by the parent: {result:?}"
            );
            return;
        }

        let assert_child_is_blocked = |path: &Path, phase: &str| {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env(CHILD_PATH, path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{phase}: child status {}; stdout: {}; stderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };

        for reopen in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("data.sqlite3");
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE probe(value); BEGIN IMMEDIATE")
                .unwrap();
            assert_child_is_blocked(&path, "before permission repair");

            // A second connection in this process would only test SQLite's
            // bookkeeping. A child observes whether the kernel lock survived.
            let reopened = if reopen {
                Some(Db::open(&path).unwrap())
            } else {
                protect_database_files(&path).unwrap();
                None
            };
            assert_child_is_blocked(
                &path,
                if reopen {
                    "after Db::open"
                } else {
                    "after permission repair"
                },
            );
            conn.execute_batch("ROLLBACK").unwrap();
            drop(reopened);
        }
    }

    #[cfg(unix)]
    #[test]
    fn database_and_live_sidecars_are_private_before_auth_writes() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.sqlite3");
        let db = Db::open(&path).unwrap();
        db.migrate().unwrap();
        db.set_auth_value("test-key", "test-secret").unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let file = dir.path().join(format!("data.sqlite3{suffix}"));
            assert_eq!(std::fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn repairs_existing_sidecars_without_changing_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let directory_mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
        let path = dir.path().join("data.sqlite3");
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let file = dir.path().join(format!("data.sqlite3{suffix}"));
            std::fs::write(&file, "retained contents").unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        protect_database_files(&path).unwrap();
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let file = dir.path().join(format!("data.sqlite3{suffix}"));
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "retained contents");
            assert_eq!(std::fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode(),
            directory_mode
        );
    }

    #[cfg(unix)]
    #[test]
    fn permission_repairs_reject_symlinks_without_changing_their_targets() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        for suffix in ["", "-wal", "-shm", "-journal"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("data.sqlite3");
            let target = dir.path().join("unrelated-file");
            std::fs::write(&target, "retained contents").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
            if !suffix.is_empty() {
                std::fs::write(&path, "").unwrap();
            }
            symlink(&target, dir.path().join(format!("data.sqlite3{suffix}"))).unwrap();

            let error = protect_database_files(&path).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "retained contents");
            assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o644);
        }
    }

    #[test]
    fn test_migrate() {
        let db = mock();

        // assert migration count is correct
        let max_migration = max_migration_version(&&*db.pool.get().unwrap());
        assert_eq!(max_migration, Some(MIGRATIONS.len() as i64 - 1));
    }

    #[test]
    fn list_migrations() {
        // Assert the migrations are in order
        assert!(MIGRATIONS.windows(2).all(|w| w[0].name <= w[1].name));

        // Assert the migrations start with their index
        assert!(
            MIGRATIONS
                .iter()
                .enumerate()
                .all(|(i, m)| m.name.starts_with(&format!("{:03}_", i)))
        );

        // Assert all the files in migrations/ are in the list
        let migration_folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sqlite/migrations");
        let migration_count = std::fs::read_dir(migration_folder).unwrap().count();
        assert_eq!(MIGRATIONS.len(), migration_count);
    }

    #[test]
    fn state_table_tests() {
        let db = mock();

        // set
        db.set_state_value("test", "test").unwrap();
        db.set_state_value("int", 1).unwrap();
        db.set_state_value("float", 1.0).unwrap();
        db.set_state_value("bool", true).unwrap();
        db.set_state_value("null", ()).unwrap();
        db.set_state_value("array", vec![1, 2, 3]).unwrap();
        db.set_state_value("object", serde_json::json!({ "test": "test" }))
            .unwrap();
        db.set_state_value("binary", b"test".to_vec()).unwrap();

        // get
        assert_eq!(db.get_state_value("test").unwrap().unwrap(), "test");
        assert_eq!(db.get_state_value("int").unwrap().unwrap(), 1);
        assert_eq!(db.get_state_value("float").unwrap().unwrap(), 1.0);
        assert_eq!(db.get_state_value("bool").unwrap().unwrap(), true);
        assert_eq!(db.get_state_value("null").unwrap().unwrap(), serde_json::Value::Null);
        assert_eq!(
            db.get_state_value("array").unwrap().unwrap(),
            serde_json::json!([1, 2, 3])
        );
        assert_eq!(
            db.get_state_value("object").unwrap().unwrap(),
            serde_json::json!({ "test": "test" })
        );
        assert_eq!(
            db.get_state_value("binary").unwrap().unwrap(),
            serde_json::json!(b"test".to_vec())
        );

        // unset
        db.unset_state_value("test").unwrap();
        db.unset_state_value("int").unwrap();

        // is_set
        assert!(!db.is_state_value_set("test").unwrap());
        assert!(!db.is_state_value_set("int").unwrap());
        assert!(db.is_state_value_set("float").unwrap());
        assert!(db.is_state_value_set("bool").unwrap());
    }

    #[test]
    fn auth_table_tests() {
        let db = mock();

        db.set_auth_value("test", "test").unwrap();
        assert_eq!(db.get_auth_value("test").unwrap().unwrap(), "test");
        assert!(db.is_auth_value_set("test").unwrap());
        db.unset_auth_value("test").unwrap();
        assert!(!db.is_auth_value_set("test").unwrap());

        assert_eq!(db.get_auth_value("test2").unwrap(), None);
        assert!(!db.is_auth_value_set("test2").unwrap());
    }

    #[test]
    fn db_open_time() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("data.sqlite3");

        // init the db
        let db = Db::open(&path).unwrap();
        db.migrate().unwrap();
        drop(db);

        let test_count = 100;

        let instant = std::time::Instant::now();
        let db = Db::open(&path).unwrap();
        for _ in 0..test_count {
            db.set_state_value("test", "test").unwrap();
            db.get_state_value("test").unwrap().unwrap();
        }
        let elapsed = instant.elapsed() / test_count;
        println!("time: {:?}", elapsed);
    }

    #[test]
    fn test_atomic_bool() {
        let key = "test";
        let db = mock();

        let cases = [
            (None, false, false, false),
            (None, true, false, true),
            (Some(false), false, false, false),
            (Some(false), true, false, true),
            (Some(true), false, true, true),
            (Some(true), true, true, true),
        ];

        for (a, b, c, d) in cases {
            db.set_state_value(key, a).unwrap();
            assert_eq!(db.atomic_bool_or(key, b).unwrap(), c);
            assert_eq!(db.get_state_value(key).unwrap().unwrap(), d);
            db.unset_state_value(key).unwrap();
        }
    }
}
