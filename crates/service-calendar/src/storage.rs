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

/// Splits Google's flat `recurrence` line list (RRULE/EXRULE/RDATE/EXDATE, RFC 5545
/// §3.3.10) into the series' own repeat rule and everything else — see
/// `Event::recurrence`'s doc comment for why these can't stay merged.
fn split_recurrence_lines(lines: &[String]) -> (Option<String>, Option<String>) {
    let (rrule, other): (Vec<&String>, Vec<&String>) =
        lines.iter().partition(|line| line.starts_with("RRULE:"));
    let join = |parts: Vec<&String>| (!parts.is_empty()).then(|| parts.into_iter().cloned().collect::<Vec<_>>().join("\n"));
    (join(rrule), join(other))
}

/// The signed-in account's own RSVP, read off whichever attendee has Google's `self`
/// flag set — `None` if the event has no attendee list (most events don't) or none of
/// its attendees is marked `self` (an oddity, but not one worth failing the sync over).
fn self_response_status(event: &Event) -> Option<&str> {
    event
        .attendees
        .iter()
        .find(|a| a.is_self)
        .map(|a| a.response_status.as_str())
}

/// Replaces every attendee on `event_id` with `attendees` — delete-then-reinsert, same
/// pattern as `query::replace_reminders`, since a sync always hands back the event's
/// full current attendee list rather than an incremental change set.
fn replace_attendees(conn: &Connection, event_id: i64, attendees: &[crate::google_api::EventAttendee]) -> anyhow::Result<()> {
    conn.execute("DELETE FROM event_attendees WHERE event_id = ?1", [event_id])?;
    for attendee in attendees {
        conn.execute(
            "INSERT INTO event_attendees (event_id, email, display_name, response_status, is_self, optional, is_organizer)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (event_id, email) DO UPDATE SET
                display_name = excluded.display_name,
                response_status = excluded.response_status,
                is_self = excluded.is_self,
                optional = excluded.optional,
                is_organizer = excluded.is_organizer",
            params![
                event_id,
                attendee.email,
                attendee.display_name,
                attendee.response_status,
                attendee.is_self,
                attendee.optional,
                attendee.organizer,
            ],
        )?;
    }
    Ok(())
}

/// Replaces every attachment on `event_id` with `attachments` — same
/// delete-then-reinsert pattern as `replace_attendees`.
fn replace_attachments(conn: &Connection, event_id: i64, attachments: &[crate::google_api::EventAttachment]) -> anyhow::Result<()> {
    conn.execute("DELETE FROM event_attachments WHERE event_id = ?1", [event_id])?;
    for attachment in attachments {
        conn.execute(
            "INSERT INTO event_attachments (event_id, file_url, title, mime_type, icon_link, file_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event_id,
                attachment.file_url,
                attachment.title,
                attachment.mime_type,
                attachment.icon_link,
                attachment.file_id,
            ],
        )?;
    }
    Ok(())
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
    let (recurrence_rule, recurrence_exceptions) = split_recurrence_lines(&event.recurrence);
    let organizer_email = event.organizer.as_ref().and_then(|o| o.email.as_deref());
    let organizer_name = event.organizer.as_ref().and_then(|o| o.display_name.as_deref());
    // Google (and an ICS export) omits `visibility`/`transparency` entirely when
    // they're at their default value, so a `None` here — not the SQL column's own
    // `NOT NULL DEFAULT` — is what has to supply "default"/"opaque": passing an
    // explicit `NULL` bypasses a column's `DEFAULT` and would violate `NOT NULL`.
    let visibility = event.visibility.as_deref().unwrap_or("default");
    let transparency = event.transparency.as_deref().unwrap_or("opaque");

    conn.execute(
        "INSERT INTO events (
            calendar_id, google_event_id, title, description, location,
            start, end, all_day, recurrence_rule, recurrence_exceptions, status, etag, updated_at,
            created_at, organizer_email, organizer_name, hangout_link, sequence, self_response_status,
            visibility, transparency, url
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
         ON CONFLICT (calendar_id, google_event_id) DO UPDATE SET
            title = excluded.title,
            description = excluded.description,
            location = excluded.location,
            start = excluded.start,
            end = excluded.end,
            all_day = excluded.all_day,
            recurrence_rule = excluded.recurrence_rule,
            recurrence_exceptions = excluded.recurrence_exceptions,
            status = excluded.status,
            etag = excluded.etag,
            updated_at = excluded.updated_at,
            created_at = excluded.created_at,
            organizer_email = excluded.organizer_email,
            organizer_name = excluded.organizer_name,
            hangout_link = excluded.hangout_link,
            sequence = excluded.sequence,
            self_response_status = excluded.self_response_status,
            visibility = excluded.visibility,
            transparency = excluded.transparency,
            url = excluded.url",
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
            recurrence_exceptions,
            event.status,
            event.etag,
            event.updated,
            event.created,
            organizer_email,
            organizer_name,
            event.hangout_link,
            event.sequence,
            self_response_status(event),
            visibility,
            transparency,
            event.url,
        ],
    )?;

    let event_row_id: i64 = conn.query_row(
        "SELECT id FROM events WHERE calendar_id = ?1 AND google_event_id = ?2",
        params![calendar_id, event.id],
        |row| row.get(0),
    )?;
    replace_attendees(conn, event_row_id, &event.attendees)?;
    replace_attachments(conn, event_row_id, &event.attachments)?;
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
            ..Default::default()
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

    /// Regression test for the exact shape of a Google-exported recurring event with
    /// an exception day (matches a real `daily repeat` series with one `EXDATE`) —
    /// before `recurrence_exceptions` existed, `upsert_event` joined `RRULE` and
    /// `EXDATE` lines into the single `recurrence_rule` column, which the edit
    /// dialog's save path (`query::update_event`) would then silently clobber down to
    /// just the `RRULE` on the next local edit.
    #[test]
    fn upsert_event_keeps_recurrence_rule_and_exceptions_separate() {
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
            id: "06vac41tc1rkgnmtttapv3pd14".into(),
            status: Some("confirmed".into()),
            summary: Some("daily repeat".into()),
            start: crate::google_api::EventDateTime {
                date_time: Some("2026-08-31T06:00:00-04:00".into()),
                ..Default::default()
            },
            end: crate::google_api::EventDateTime {
                date_time: Some("2026-08-31T07:00:00-04:00".into()),
                ..Default::default()
            },
            recurrence: vec![
                "RRULE:FREQ=DAILY;UNTIL=20260910T035959Z".into(),
                "EXDATE;TZID=America/New_York:20260903T060000".into(),
            ],
            ..Default::default()
        };
        storage.with_conn(|conn| upsert_event(conn, calendar_id, &event)).expect("insert event");

        let (rule, exceptions): (Option<String>, Option<String>) = storage
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT recurrence_rule, recurrence_exceptions FROM events WHERE google_event_id = ?1",
                    [&event.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .expect("query");
        assert_eq!(rule.as_deref(), Some("RRULE:FREQ=DAILY;UNTIL=20260910T035959Z"));
        assert_eq!(exceptions.as_deref(), Some("EXDATE;TZID=America/New_York:20260903T060000"));
    }

    #[test]
    fn upsert_event_stores_organizer_and_attendees_and_derives_self_response_status() {
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
            id: "7hviprlvh7phvs31qm0fsf9ee8".into(),
            status: Some("confirmed".into()),
            summary: Some("repeating".into()),
            start: crate::google_api::EventDateTime {
                date_time: Some("2026-08-31T09:00:00Z".into()),
                ..Default::default()
            },
            end: crate::google_api::EventDateTime {
                date_time: Some("2026-08-31T10:00:00Z".into()),
                ..Default::default()
            },
            organizer: Some(crate::google_api::EventOrganizer {
                email: Some("clawmeariver@gmail.com".into()),
                display_name: Some("Claw Meariver".into()),
            }),
            attendees: vec![
                crate::google_api::EventAttendee {
                    email: "clawmeariver@gmail.com".into(),
                    display_name: Some("Claw Meariver".into()),
                    response_status: "accepted".into(),
                    is_self: true,
                    optional: false,
                    organizer: true,
                },
                crate::google_api::EventAttendee {
                    email: "silversword@gmail.com".into(),
                    display_name: None,
                    response_status: "needsAction".into(),
                    is_self: false,
                    optional: false,
                    organizer: false,
                },
            ],
            hangout_link: Some("https://meet.google.com/yaf-zbof-ubm".into()),
            ..Default::default()
        };
        storage.with_conn(|conn| upsert_event(conn, calendar_id, &event)).expect("insert event");

        let (organizer_email, hangout_link, self_status): (Option<String>, Option<String>, Option<String>) = storage
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT organizer_email, hangout_link, self_response_status FROM events WHERE google_event_id = ?1",
                    [&event.id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?)
            })
            .expect("query");
        assert_eq!(organizer_email.as_deref(), Some("clawmeariver@gmail.com"));
        assert_eq!(hangout_link.as_deref(), Some("https://meet.google.com/yaf-zbof-ubm"));
        assert_eq!(self_status.as_deref(), Some("accepted"), "self_response_status must come from the is_self attendee");

        let attendee_count: i64 = storage
            .with_conn(|conn| Ok(conn.query_row("SELECT count(*) FROM event_attendees", [], |r| r.get(0))?))
            .expect("count");
        assert_eq!(attendee_count, 2);

        // A resync with a shrunk attendee list must fully replace the old rows, not
        // accumulate alongside them (mirrors `query::replace_reminders`'s same rule).
        let mut fewer_attendees = event.clone();
        fewer_attendees.attendees.truncate(1);
        storage.with_conn(|conn| upsert_event(conn, calendar_id, &fewer_attendees)).expect("resync");
        let attendee_count_after: i64 = storage
            .with_conn(|conn| Ok(conn.query_row("SELECT count(*) FROM event_attendees", [], |r| r.get(0))?))
            .expect("count");
        assert_eq!(attendee_count_after, 1);
    }

    /// Matches a real spam calendar invite (an unsolicited iCloud CalDAV event with a
    /// phishing/tracking `ATTACH` link) — the shape that first showed `Event` had no
    /// representation at all for Google's `attachments[]` (iCalendar's `ATTACH`).
    #[test]
    fn upsert_event_stores_attachments_and_url() {
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
            id: "1FC16337-694A-41C8-95AF-4A44AF4383AB".into(),
            status: Some("confirmed".into()),
            summary: Some("hLNQOCeWW".into()),
            start: crate::google_api::EventDateTime {
                date: Some("2022-04-17".into()),
                ..Default::default()
            },
            end: crate::google_api::EventDateTime {
                date: Some("2022-04-18".into()),
                ..Default::default()
            },
            url: Some("http://tinyurl.com/gl84y72".into()),
            attachments: vec![crate::google_api::EventAttachment {
                file_url: "https://gateway.icloud.com/caldav/.../attach/...".into(),
                title: Some("ffgg.in...html".into()),
                mime_type: Some("text/html".into()),
                icon_link: None,
                file_id: None,
            }],
            ..Default::default()
        };
        storage.with_conn(|conn| upsert_event(conn, calendar_id, &event)).expect("insert event");

        let url: Option<String> = storage
            .with_conn(|conn| {
                Ok(conn.query_row("SELECT url FROM events WHERE google_event_id = ?1", [&event.id], |r| r.get(0))?)
            })
            .expect("query");
        assert_eq!(url.as_deref(), Some("http://tinyurl.com/gl84y72"));

        let (file_url, mime_type): (String, Option<String>) = storage
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT file_url, mime_type FROM event_attachments WHERE event_id = \
                     (SELECT id FROM events WHERE google_event_id = ?1)",
                    [&event.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .expect("query");
        assert_eq!(file_url, "https://gateway.icloud.com/caldav/.../attach/...");
        assert_eq!(mime_type.as_deref(), Some("text/html"));

        // A resync with the attachment removed must clear it, not leave a stale row —
        // same "replace, don't accumulate" rule as attendees.
        let mut no_attachments = event.clone();
        no_attachments.attachments.clear();
        storage.with_conn(|conn| upsert_event(conn, calendar_id, &no_attachments)).expect("resync");
        let attachment_count: i64 = storage
            .with_conn(|conn| Ok(conn.query_row("SELECT count(*) FROM event_attachments", [], |r| r.get(0))?))
            .expect("count");
        assert_eq!(attachment_count, 0);
    }

    /// Before `Event` carried `visibility`/`transparency` at all, a synced event's
    /// actual Google-side privacy/busy setting never reached the local DB — every
    /// synced row silently kept the table's own `NOT NULL DEFAULT` ('default'/'opaque')
    /// no matter what Google returned, which matters for exactly the kind of event
    /// this regression test uses: a `CLASS:PRIVATE`/`TRANSP:TRANSPARENT` event.
    #[test]
    fn upsert_event_stores_visibility_and_transparency_and_defaults_when_google_omits_them() {
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

        let private_event = Event {
            id: "individual".into(),
            status: Some("confirmed".into()),
            summary: Some("individual".into()),
            start: crate::google_api::EventDateTime {
                date_time: Some("2026-09-07T11:45:00Z".into()),
                ..Default::default()
            },
            end: crate::google_api::EventDateTime {
                date_time: Some("2026-09-07T13:00:00Z".into()),
                ..Default::default()
            },
            visibility: Some("private".into()),
            transparency: Some("transparent".into()),
            ..Default::default()
        };
        storage.with_conn(|conn| upsert_event(conn, calendar_id, &private_event)).expect("insert private event");

        // Google (and an ICS export) omits both fields entirely when they're at their
        // default value — a `None` here must resolve to 'default'/'opaque', not NULL.
        let default_event = Event {
            id: "all-day".into(),
            status: Some("confirmed".into()),
            summary: Some("all day".into()),
            start: crate::google_api::EventDateTime {
                date: Some("2026-09-08".into()),
                ..Default::default()
            },
            end: crate::google_api::EventDateTime {
                date: Some("2026-09-09".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        storage.with_conn(|conn| upsert_event(conn, calendar_id, &default_event)).expect("insert default event");

        let fetch = |id: &str| -> (String, String) {
            storage
                .with_conn(|conn| {
                    Ok(conn.query_row(
                        "SELECT visibility, transparency FROM events WHERE google_event_id = ?1",
                        [id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?)
                })
                .expect("query")
        };
        assert_eq!(fetch("individual"), ("private".to_string(), "transparent".to_string()));
        assert_eq!(fetch("all-day"), ("default".to_string(), "opaque".to_string()));
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
