use async_trait::async_trait;
use calendarchy_core::{GoogleOAuthConfig, Service, ServiceContext, ServiceKind};
use rusqlite::Connection;

use google_api::{GoogleApiError, GoogleCalendarClient};
use token_provider::KeyringAccessTokenProvider;

pub mod google_api;
pub mod query;
pub mod recurrence;
mod storage;
mod token_provider;

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

impl CalendarService {
    fn client_for(&self, ctx: &ServiceContext) -> anyhow::Result<GoogleCalendarClient> {
        let oauth_config = GoogleOAuthConfig::from_env()?;
        let provider = KeyringAccessTokenProvider::new(ctx.account_id, ctx.keyring.clone(), oauth_config);
        Ok(GoogleCalendarClient::new(Box::new(provider)))
    }

    /// Runs one incremental (or, on a first sync / expired sync token, full) sync
    /// pass for a single calendar, paging through results and persisting the new
    /// `syncToken` once the last page has been fetched (DESIGN_SPEC.md §9).
    async fn sync_one_calendar(
        &self,
        ctx: &ServiceContext,
        client: &GoogleCalendarClient,
        calendar_row_id: i64,
        google_calendar_id: &str,
    ) -> anyhow::Result<()> {
        let mut sync_token = ctx
            .storage
            .with_conn(|conn| storage::load_sync_token(conn, ctx.account_id, calendar_row_id))?;
        let mut page_token: Option<String> = None;
        let mut final_sync_token: Option<String> = None;
        let mut retried_after_expired_token = false;

        loop {
            let page = match client
                .list_events(google_calendar_id, sync_token.as_deref(), page_token.as_deref())
                .await
            {
                Ok(page) => page,
                Err(GoogleApiError::SyncTokenExpired) if !retried_after_expired_token => {
                    tracing::warn!(
                        calendar_id = calendar_row_id,
                        "sync token expired, falling back to a full resync"
                    );
                    sync_token = None;
                    page_token = None;
                    retried_after_expired_token = true;
                    continue;
                }
                Err(err) => return Err(err.into()),
            };

            ctx.storage.with_conn(|conn| {
                for event in &page.items {
                    storage::upsert_event(conn, calendar_row_id, event)?;
                }
                Ok(())
            })?;

            if page.next_sync_token.is_some() {
                final_sync_token = page.next_sync_token;
            }
            match page.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }

        if let Some(token) = final_sync_token {
            ctx.storage
                .with_conn(|conn| storage::store_sync_token(conn, ctx.account_id, calendar_row_id, &token))?;
        }
        Ok(())
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

            CREATE TABLE IF NOT EXISTS event_edit_history (
                id          INTEGER PRIMARY KEY,
                event_id    INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
                account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                calendar_id INTEGER NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
                payload     TEXT NOT NULL,
                created_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS event_reminders (
                id        INTEGER PRIMARY KEY,
                event_id  INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
                method    TEXT NOT NULL,
                minutes   INTEGER NOT NULL
            );

            -- One row per guest on an event — Google's `attendees[]`, and iCalendar's
            -- (possibly repeated) `ATTENDEE` property. `is_self` marks the signed-in
            -- account's own row, which is also where `events.self_response_status` is
            -- read from (see `storage::upsert_event`); `is_organizer` mirrors an
            -- attendee entry that also carries Google's `organizer: true` flag (a
            -- self-organized event typically lists the organizer as an attendee too,
            -- alongside the separate `events.organizer_email` column).
            CREATE TABLE IF NOT EXISTS event_attendees (
                id               INTEGER PRIMARY KEY,
                event_id         INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
                email            TEXT NOT NULL,
                display_name     TEXT,
                response_status  TEXT NOT NULL DEFAULT 'needsAction',
                is_self          INTEGER NOT NULL DEFAULT 0,
                optional         INTEGER NOT NULL DEFAULT 0,
                is_organizer     INTEGER NOT NULL DEFAULT 0,
                UNIQUE (event_id, email)
            );

            -- One row per file attached to an event — Google's `attachments[]`,
            -- iCalendar's `ATTACH` property. No uniqueness constraint (unlike
            -- `event_attendees`): `storage::replace_attachments` always
            -- delete-then-reinserts the full list on sync, same pattern as
            -- `event_reminders`, which has no constraint for the same reason.
            CREATE TABLE IF NOT EXISTS event_attachments (
                id          INTEGER PRIMARY KEY,
                event_id    INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
                file_url    TEXT NOT NULL,
                title       TEXT,
                mime_type   TEXT,
                icon_link   TEXT,
                file_id     TEXT
            );
            "#,
        )?;
        // `events.color`/`transparency`/`visibility`/etc. were all added after `events`
        // first shipped, so `CREATE TABLE IF NOT EXISTS` above is a no-op against any
        // database that already has the table — unlike a brand new column on a brand
        // new table, SQLite has no `ADD COLUMN IF NOT EXISTS`, so this checks first
        // rather than risk a "duplicate column" error against an install that's
        // already been upgraded once.
        add_column_if_missing(conn, "events", "color", "TEXT")?;
        // Google's own default: an event without an explicit choice shows as Busy.
        add_column_if_missing(conn, "events", "transparency", "TEXT NOT NULL DEFAULT 'opaque'")?;
        add_column_if_missing(conn, "events", "visibility", "TEXT NOT NULL DEFAULT 'default'")?;
        add_column_if_missing(conn, "events", "organizer_email", "TEXT")?;
        add_column_if_missing(conn, "events", "organizer_name", "TEXT")?;
        add_column_if_missing(conn, "events", "hangout_link", "TEXT")?;
        add_column_if_missing(conn, "events", "sequence", "INTEGER NOT NULL DEFAULT 0")?;
        add_column_if_missing(conn, "events", "created_at", "TEXT")?;
        // The signed-in account's own RSVP, read off whichever `event_attendees` row
        // has `is_self` (DESIGN_SPEC.md §8) — drives "Show declined events" (§12).
        // `NULL` means the event has no attendee list at all (most events don't).
        add_column_if_missing(conn, "events", "self_response_status", "TEXT")?;
        // RRULE/EXRULE/RDATE/EXDATE lines that aren't the series' own `RRULE` (mainly
        // per-instance `EXDATE` exceptions) — split out of `recurrence_rule` so that
        // saving an edit through the recurrence picker (which only ever writes a bare
        // `RRULE` back to `recurrence_rule`, see `query::update_event`) doesn't
        // silently discard them. Preserved verbatim; nothing in this app edits them
        // (DESIGN_SPEC.md §20 scopes recurring-event editing down deliberately).
        add_column_if_missing(conn, "events", "recurrence_exceptions", "TEXT")?;
        // A generic "more info" link (iCalendar's `URL` property) — see
        // `google_api::Event::url`'s doc comment for why Google sync never sets this.
        add_column_if_missing(conn, "events", "url", "TEXT")?;
        Ok(())
    }

    async fn on_enabled(&self, ctx: &ServiceContext) -> anyhow::Result<()> {
        let client = self.client_for(ctx)?;
        let calendars = client.list_calendars().await?;

        ctx.storage.with_conn(|conn| {
            for entry in &calendars {
                storage::upsert_calendar(conn, ctx.account_id, entry)?;
            }
            Ok(())
        })?;

        tracing::info!(account_id = ctx.account_id.0, count = calendars.len(), "fetched calendar list");
        Ok(())
    }

    async fn sync(&self, ctx: &ServiceContext) -> anyhow::Result<()> {
        let client = self.client_for(ctx)?;
        let calendars = ctx.storage.with_conn(|conn| storage::list_calendar_ids(conn, ctx.account_id))?;

        for (calendar_row_id, google_calendar_id) in calendars {
            self.sync_one_calendar(ctx, &client, calendar_row_id, &google_calendar_id).await?;
        }
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

/// Adds `column` to `table` (as `sql_type`) only if it isn't there already — the
/// idempotent building block `migrate` needs for evolving a table SQLite's own
/// `ALTER TABLE ... ADD COLUMN` has no `IF NOT EXISTS` form for. Table/column names
/// are always our own hardcoded schema strings, never user input, so building the SQL
/// with `format!` (SQLite can't bind identifiers as parameters) is safe here.
fn add_column_if_missing(conn: &Connection, table: &str, column: &str, sql_type: &str) -> anyhow::Result<()> {
    let already_present = conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|name| name == column);

    if !already_present {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {sql_type}"), [])?;
    }
    Ok(())
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
                     AND name IN ('calendars', 'events', 'sync_state', 'pending_edits', \
                                  'event_reminders', 'event_attendees', 'event_attachments')",
                    [],
                    |row| row.get(0),
                )?)
            })
            .expect("query");
        assert_eq!(table_count, 7);
    }

    /// `migrate` runs on every launch, against a database that (for any real user) has
    /// been through it before — most critically, `add_column_if_missing` must not
    /// error with "duplicate column" the second time it sees a database that already
    /// has `events.color`.
    #[test]
    fn migrate_is_idempotent_against_an_already_migrated_database() {
        let storage = Storage::open_in_memory().expect("open");
        let service = CalendarService::new();
        storage
            .with_conn(|conn| service.migrate(conn).map_err(anyhow::Error::from))
            .expect("first migrate");
        storage
            .with_conn(|conn| service.migrate(conn).map_err(anyhow::Error::from))
            .expect("second migrate should not error");

        let color_column_count: i64 = storage
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM pragma_table_info('events') WHERE name = 'color'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .expect("query");
        assert_eq!(color_column_count, 1, "column must exist exactly once, not be missing or duplicated");
    }
}
