use calendarchy_core::AccountId;
use rusqlite::{params, Connection, OptionalExtension};

use crate::google_api::{CalendarListEntry, Event};

/// Insert or update a calendar row for an account, returning its local row id.
/// `is_visible` is only set to `1` on first insert — an existing row's visibility
/// (a user preference) is left untouched by later syncs.
pub fn upsert_calendar(conn: &Connection, account_id: AccountId, entry: &CalendarListEntry) -> anyhow::Result<i64> {
    conn.execute(
        "INSERT INTO calendars (account_id, google_calendar_id, display_name, color, is_visible, access_role)
         VALUES (?1, ?2, ?3, ?4, 1, ?5)
         ON CONFLICT (account_id, google_calendar_id) DO UPDATE SET
            display_name = excluded.display_name,
            color = excluded.color,
            access_role = excluded.access_role",
        params![
            account_id.0,
            entry.id,
            entry.summary,
            entry.background_color,
            entry.access_role,
        ],
    )?;
    let id = conn.query_row(
        "SELECT id FROM calendars WHERE account_id = ?1 AND google_calendar_id = ?2",
        params![account_id.0, entry.id],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// Insert, update, or (for a cancelled event) delete the local row for one event.
/// `start`/`end` are stored as whichever of Google's `date`/`dateTime` was set,
/// verbatim — DESIGN_SPEC.md's `events` schema keeps these as opaque strings; parsing
/// happens at display time, not storage time.
pub fn upsert_event(conn: &Connection, calendar_id: i64, event: &Event) -> anyhow::Result<()> {
    if event.status.as_deref() == Some("cancelled") {
        conn.execute(
            "DELETE FROM events WHERE calendar_id = ?1 AND google_event_id = ?2",
            params![calendar_id, event.id],
        )?;
        return Ok(());
    }

    // `start`/`end` are NOT NULL columns; real Google events always set one of
    // date/dateTime, but default to empty rather than risk a constraint error on a
    // malformed response.
    let start = event.start.date_time.clone().or_else(|| event.start.date.clone()).unwrap_or_default();
    let end = event.end.date_time.clone().or_else(|| event.end.date.clone()).unwrap_or_default();
    let recurrence_rule = (!event.recurrence.is_empty()).then(|| event.recurrence.join("\n"));

    conn.execute(
        "INSERT INTO events (
            calendar_id, google_event_id, title, description, location,
            start, end, all_day, recurrence_rule, status, etag, updated_at
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT (calendar_id, google_event_id) DO UPDATE SET
            title = excluded.title,
            description = excluded.description,
            location = excluded.location,
            start = excluded.start,
            end = excluded.end,
            all_day = excluded.all_day,
            recurrence_rule = excluded.recurrence_rule,
            status = excluded.status,
            etag = excluded.etag,
            updated_at = excluded.updated_at",
        params![
            calendar_id,
            event.id,
            event.summary,
            event.description,
            event.location,
            start,
            end,
            event.start.is_all_day(),
            recurrence_rule,
            event.status,
            event.etag,
            event.updated,
        ],
    )?;
    Ok(())
}

pub fn store_sync_token(conn: &Connection, account_id: AccountId, calendar_row_id: i64, sync_token: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO sync_state (account_id, resource_id, sync_token, last_synced_at, last_full_sync_at)
         VALUES (?1, ?2, ?3, datetime('now'), datetime('now'))
         ON CONFLICT (account_id, resource_id) DO UPDATE SET
            sync_token = excluded.sync_token,
            last_synced_at = datetime('now')",
        params![account_id.0, calendar_row_id, sync_token],
    )?;
    Ok(())
}

pub fn load_sync_token(conn: &Connection, account_id: AccountId, calendar_row_id: i64) -> anyhow::Result<Option<String>> {
    conn.query_row(
        "SELECT sync_token FROM sync_state WHERE account_id = ?1 AND resource_id = ?2",
        params![account_id.0, calendar_row_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Calendars registered locally for an account, as `(local row id, Google calendar id)`.
pub fn list_calendar_ids(conn: &Connection, account_id: AccountId) -> anyhow::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT id, google_calendar_id FROM calendars WHERE account_id = ?1")?;
    let rows = stmt.query_map(params![account_id.0], |row| Ok((row.get(0)?, row.get(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use calendarchy_core::{Service, Storage};

    fn setup() -> Storage {
        let storage = Storage::open_in_memory().expect("open");
        storage
            .with_conn(|conn| crate::CalendarService::new().migrate(conn).map_err(Into::into))
            .expect("migrate");
        // `calendars`/`events` reference `accounts(id)` via a foreign key, so tests
        // need a real account row to attach to.
        storage
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO accounts (id, provider, email) VALUES (1, 'google', 'jane@gmail.com')",
                    [],
                )?;
                Ok(())
            })
            .expect("seed account");
        storage
    }

    #[test]
    fn upsert_calendar_is_idempotent_and_updates_in_place() {
        let storage = setup();
        let account_id = AccountId(1);
        let entry = CalendarListEntry {
            id: "jane@gmail.com".into(),
            summary: "jane@gmail.com".into(),
            background_color: Some("#9fe1e7".into()),
            access_role: "owner".into(),
            primary: true,
        };

        let first_id = storage
            .with_conn(|conn| upsert_calendar(conn, account_id, &entry))
            .expect("insert");

        let mut renamed = entry.clone();
        renamed.summary = "Jane (renamed)".into();
        let second_id = storage
            .with_conn(|conn| upsert_calendar(conn, account_id, &renamed))
            .expect("update");

        assert_eq!(first_id, second_id, "same google_calendar_id must map to the same row");

        let stored_name: String = storage
            .with_conn(|conn| {
                Ok(conn.query_row("SELECT display_name FROM calendars WHERE id = ?1", [first_id], |r| r.get(0))?)
            })
            .expect("query");
        assert_eq!(stored_name, "Jane (renamed)");
    }

    #[test]
    fn upsert_event_stores_timed_and_all_day_events_and_deletes_cancelled() {
        let storage = setup();
        let account_id = AccountId(1);
        let calendar_id = storage
            .with_conn(|conn| {
                upsert_calendar(
                    conn,
                    account_id,
                    &CalendarListEntry {
                        id: "jane@gmail.com".into(),
                        summary: "jane@gmail.com".into(),
                        background_color: None,
                        access_role: "owner".into(),
                        primary: true,
                    },
                )
            })
            .expect("calendar");

        let event = Event {
            id: "abc123".into(),
            status: Some("confirmed".into()),
            summary: Some("Standup".into()),
            description: None,
            location: None,
            start: crate::google_api::EventDateTime {
                date_time: Some("2026-09-05T09:00:00-04:00".into()),
                ..Default::default()
            },
            end: crate::google_api::EventDateTime {
                date_time: Some("2026-09-05T09:15:00-04:00".into()),
                ..Default::default()
            },
            recurrence: vec![],
            etag: Some("\"1\"".into()),
            updated: Some("2026-09-01T12:00:00Z".into()),
        };

        storage.with_conn(|conn| upsert_event(conn, calendar_id, &event)).expect("insert event");

        let count: i64 = storage
            .with_conn(|conn| Ok(conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?))
            .expect("count");
        assert_eq!(count, 1);

        let mut cancelled = event.clone();
        cancelled.status = Some("cancelled".into());
        storage.with_conn(|conn| upsert_event(conn, calendar_id, &cancelled)).expect("cancel event");

        let count_after: i64 = storage
            .with_conn(|conn| Ok(conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?))
            .expect("count after cancel");
        assert_eq!(count_after, 0, "cancelled events must be removed locally");
    }

    #[test]
    fn sync_token_round_trips() {
        let storage = setup();
        let account_id = AccountId(1);
        let calendar_id = storage
            .with_conn(|conn| {
                upsert_calendar(
                    conn,
                    account_id,
                    &CalendarListEntry {
                        id: "jane@gmail.com".into(),
                        summary: "jane@gmail.com".into(),
                        background_color: None,
                        access_role: "owner".into(),
                        primary: true,
                    },
                )
            })
            .expect("calendar");

        assert!(storage
            .with_conn(|conn| load_sync_token(conn, account_id, calendar_id))
            .expect("load")
            .is_none());

        storage
            .with_conn(|conn| store_sync_token(conn, account_id, calendar_id, "token-1"))
            .expect("store");
        assert_eq!(
            storage.with_conn(|conn| load_sync_token(conn, account_id, calendar_id)).expect("load"),
            Some("token-1".to_string())
        );

        storage
            .with_conn(|conn| store_sync_token(conn, account_id, calendar_id, "token-2"))
            .expect("update");
        assert_eq!(
            storage.with_conn(|conn| load_sync_token(conn, account_id, calendar_id)).expect("load"),
            Some("token-2".to_string())
        );
    }
}
