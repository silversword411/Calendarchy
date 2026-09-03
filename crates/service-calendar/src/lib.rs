use async_trait::async_trait;
use calendarchy_core::{Service, ServiceContext, ServiceKind};
use rusqlite::Connection;

/// The Calendar `Service`: the first (and for v1, only) implementation of the
/// account/service split in DESIGN_SPEC.md §6. Owns the `calendars`, `events`,
/// `sync_state`, and `pending_edits` tables (§8) and, once the Google Calendar API
/// client lands, the polling sync engine described in §9.
pub struct CalendarService;

impl CalendarService {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CalendarService {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Service for CalendarService {
    fn kind(&self) -> ServiceKind {
        ServiceKind::Calendar
    }

    fn display_name(&self) -> &'static str {
        "Calendar"
    }

    fn oauth_scopes(&self) -> &'static [&'static str] {
        &[
            "https://www.googleapis.com/auth/calendar.events",
            "https://www.googleapis.com/auth/calendar.readonly",
        ]
    }

    fn migrate(&self, conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS calendars (
                id                  INTEGER PRIMARY KEY,
                account_id          INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                google_calendar_id  TEXT NOT NULL,
                display_name        TEXT NOT NULL,
                color               TEXT,
                is_visible          INTEGER NOT NULL DEFAULT 1,
                access_role         TEXT NOT NULL,
                UNIQUE (account_id, google_calendar_id)
            );

            CREATE TABLE IF NOT EXISTS events (
                id               INTEGER PRIMARY KEY,
                calendar_id      INTEGER NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
                google_event_id  TEXT NOT NULL,
                title            TEXT,
                description      TEXT,
                location         TEXT,
                start            TEXT NOT NULL,
                end              TEXT NOT NULL,
                all_day          INTEGER NOT NULL DEFAULT 0,
                recurrence_rule  TEXT,
                status           TEXT,
                etag             TEXT,
                updated_at       TEXT,
                UNIQUE (calendar_id, google_event_id)
            );

            CREATE TABLE IF NOT EXISTS sync_state (
                account_id         INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                resource_id        INTEGER NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
                sync_token         TEXT,
                last_synced_at     TEXT,
                last_full_sync_at  TEXT,
                PRIMARY KEY (account_id, resource_id)
            );

            CREATE TABLE IF NOT EXISTS pending_edits (
                id             INTEGER PRIMARY KEY,
                account_id     INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                calendar_id    INTEGER NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
                event_id       INTEGER REFERENCES events(id) ON DELETE CASCADE,
                operation      TEXT NOT NULL,
                payload        TEXT NOT NULL,
                created_at     TEXT NOT NULL DEFAULT (datetime('now')),
                attempt_count  INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )?;
        Ok(())
    }

    async fn on_enabled(&self, ctx: &ServiceContext) -> anyhow::Result<()> {
        // First-run calendar list fetch + initial full sync lands with the Google
        // Calendar API client (DESIGN_SPEC.md §9, roadmap phase 1).
        tracing::info!(account_id = ctx.account_id.0, "calendar service enabled, sync not yet implemented");
        Ok(())
    }

    async fn sync(&self, ctx: &ServiceContext) -> anyhow::Result<()> {
        tracing::debug!(account_id = ctx.account_id.0, "calendar sync tick, no-op until API client lands");
        Ok(())
    }

    fn on_disabled(&self, ctx: &ServiceContext) -> anyhow::Result<()> {
        ctx.storage.with_conn(|conn| {
            conn.execute(
                "DELETE FROM calendars WHERE account_id = ?1",
                [ctx.account_id.0],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use calendarchy_core::Storage;

    #[test]
    fn migrate_creates_calendar_service_tables() {
        let storage = Storage::open_in_memory().expect("open");
        let service = CalendarService::new();
        storage
            .with_conn(|conn| service.migrate(conn).map_err(anyhow::Error::from))
            .expect("migrate");

        let table_count: i64 = storage
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' \
                     AND name IN ('calendars', 'events', 'sync_state', 'pending_edits')",
                    [],
                    |row| row.get(0),
                )?)
            })
            .expect("query");
        assert_eq!(table_count, 4);
    }
}
