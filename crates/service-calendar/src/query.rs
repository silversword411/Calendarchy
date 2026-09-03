use calendarchy_core::Storage;

/// A read-only projection of one event, shaped for rendering (Month/Agenda views,
/// DESIGN_SPEC.md §10) rather than for round-tripping back to Google — editing goes
/// through the full `Event` type in `google_api` instead.
#[derive(Debug, Clone)]
pub struct DisplayEvent {
    pub id: i64,
    pub title: String,
    pub start: String,
    pub end: String,
    pub all_day: bool,
    pub color: Option<String>,
}

impl DisplayEvent {
    /// The `YYYY-MM-DD` date this event's start falls on. Both of Google's start
    /// representations — `date` (all-day) and `dateTime` (RFC 3339, timed) — begin
    /// with this, so a plain prefix slice is enough to bucket events by calendar day.
    pub fn start_date(&self) -> &str {
        self.start.get(0..10).unwrap_or(&self.start)
    }
}

/// All events on calendars the user has left visible, across every connected account
/// — the source of truth for the Month/Agenda views (DESIGN_SPEC.md §10), which never
/// talk to the network directly (§5).
pub fn events_for_visible_calendars(storage: &Storage) -> anyhow::Result<Vec<DisplayEvent>> {
    storage.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT events.id, COALESCE(events.title, '(No title)'), events.start, events.end, \
                    events.all_day, calendars.color \
             FROM events \
             JOIN calendars ON calendars.id = events.calendar_id \
             WHERE calendars.is_visible = 1 \
             ORDER BY events.start",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DisplayEvent {
                id: row.get(0)?,
                title: row.get(1)?,
                start: row.get(2)?,
                end: row.get(3)?,
                all_day: row.get::<_, i64>(4)? != 0,
                color: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::upsert_calendar;
    use crate::google_api::{CalendarListEntry, EventDateTime};
    use calendarchy_core::{AccountId, Service};

    fn setup() -> Storage {
        let storage = Storage::open_in_memory().expect("open");
        storage
            .with_conn(|conn| crate::CalendarService::new().migrate(conn).map_err(Into::into))
            .expect("migrate");
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
    fn hides_events_on_invisible_calendars() {
        let storage = setup();
        let account_id = AccountId(1);
        let calendar_id = storage
            .with_conn(|conn| {
                upsert_calendar(
                    conn,
                    account_id,
                    &CalendarListEntry {
                        id: "jane@gmail.com".into(),
                        summary: "Jane".into(),
                        background_color: Some("#9fe1e7".into()),
                        access_role: "owner".into(),
                        primary: true,
                    },
                )
            })
            .expect("calendar");

        storage
            .with_conn(|conn| {
                crate::storage::upsert_event(
                    conn,
                    calendar_id,
                    &crate::google_api::Event {
                        id: "abc".into(),
                        status: Some("confirmed".into()),
                        summary: Some("Standup".into()),
                        description: None,
                        location: None,
                        start: EventDateTime {
                            date_time: Some("2026-09-05T09:00:00-04:00".into()),
                            ..Default::default()
                        },
                        end: EventDateTime {
                            date_time: Some("2026-09-05T09:15:00-04:00".into()),
                            ..Default::default()
                        },
                        recurrence: vec![],
                        etag: None,
                        updated: None,
                    },
                )
            })
            .expect("insert event");

        let visible = events_for_visible_calendars(&storage).expect("query");
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].start_date(), "2026-09-05");

        storage
            .with_conn(|conn| {
                conn.execute("UPDATE calendars SET is_visible = 0 WHERE id = ?1", [calendar_id])?;
                Ok(())
            })
            .expect("hide calendar");

        assert!(events_for_visible_calendars(&storage).expect("query").is_empty());
    }
}
