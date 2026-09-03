use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

/// Shared handle to the app's local SQLite database. Cheap to clone (wraps an
/// `Arc<Mutex<Connection>>`); every account and every service reads/writes through
/// this same handle, each owning its own tables (see DESIGN_SPEC.md §8).
#[derive(Clone)]
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
}

impl Storage {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        migrate_core(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run a closure with access to the underlying connection. Kept narrow and
    /// synchronous on purpose — rusqlite is blocking, so callers on the async side
    /// should wrap calls in `tokio::task::spawn_blocking` rather than holding the
    /// lock across an `.await`.
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let conn = self.conn.lock().expect("storage mutex poisoned");
        f(&conn)
    }
}

/// Core schema: account identity and which services are enabled per account.
/// Individual services (Calendar, etc.) own their own tables via `Service::migrate`,
/// run separately by the `ServiceRegistry` — see DESIGN_SPEC.md §6/§8.
fn migrate_core(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS accounts (
            id            INTEGER PRIMARY KEY,
            provider      TEXT NOT NULL,
            email         TEXT NOT NULL UNIQUE,
            display_name  TEXT,
            avatar_url    TEXT,
            created_at    TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS account_services (
            account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
            service_type    TEXT NOT NULL,
            enabled         INTEGER NOT NULL DEFAULT 1,
            granted_scopes  TEXT NOT NULL DEFAULT '',
            enabled_at      TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (account_id, service_type)
        );

        CREATE TABLE IF NOT EXISTS app_settings (
            key         TEXT PRIMARY KEY,
            value       TEXT NOT NULL,
            updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
        );
        "#,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_in_memory_and_creates_core_tables() {
        let storage = Storage::open_in_memory().expect("open");
        let table_count: i64 = storage
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' \
                     AND name IN ('accounts', 'account_services', 'app_settings')",
                    [],
                    |row| row.get(0),
                )?)
            })
            .expect("query");
        assert_eq!(table_count, 3);
    }
}
