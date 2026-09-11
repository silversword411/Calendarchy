use calendarchy_core::{AccountId, Storage};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension};

/// A read-only projection of one event, shaped for rendering (Month/Agenda views,
/// DESIGN_SPEC.md §10) rather than for round-tripping back to Google — editing goes
/// through the full `Event` type in `google_api` instead.
#[derive(Debug, Clone)]
pub struct DisplayEvent {
    pub id: i64,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: String,
    pub end: String,
    pub all_day: bool,
    pub color: Option<String>,
    pub calendar_name: String,
    /// Whether `events.recurrence_rule` is set — enough for a chip badge; the actual
    /// rule text is only needed by the editor, which reads it via `EventDetail` instead.
    pub is_recurring: bool,
    /// Whether `events.hangout_link` is set — the chip badge is informational only, so
    /// (unlike `EventDetail::hangout_link`) the actual URL isn't needed here.
    pub has_video_call: bool,
    /// `events.visibility` is anything other than `Default` (Private/Confidential) —
    /// a single lock badge covers both, mirroring Google Calendar's own chip treatment.
    pub is_private: bool,
    /// The signed-in account's own RSVP, mirrored from `events.self_response_status`
    /// (populated at sync time — see `crates/service-calendar/src/storage.rs`'s
    /// `upsert_event`). `None` when the event has no attendee list at all.
    pub self_response_status: Option<AttendeeResponseStatus>,
    /// How many `event_reminders` rows this event has — a chip only needs to know
    /// "any at all," but the count is cheap to carry and more useful in a tooltip than
    /// a bare boolean.
    pub reminder_count: i64,
    /// How many `event_attendees` rows this event has *excluding* the signed-in
    /// account's own row — "1 guest" should mean one other invited person, not "just
    /// you," which is why this isn't a raw `COUNT(*)` over the table.
    pub other_attendee_count: i64,
    /// Display name (or email, when no display name is set) for each of those same
    /// non-self attendees — carried alongside the count so a chip's guest badge can
    /// name names in its tooltip instead of just a bare number. Not truncated here;
    /// `event_badge_row` decides how many to actually show before falling back to
    /// "and N more".
    pub other_attendee_names: Vec<String>,
    /// How many `event_attachments` rows this event has.
    pub attachment_count: i64,
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
/// — the source of truth for the Month/Day views (DESIGN_SPEC.md §10), which never
/// talk to the network directly (§5). The three `LEFT JOIN`ed subqueries each count
/// rows in a child table per event (`reminder_counts`/`attendee_counts` — the latter
/// excluding the signed-in account's own attendee row —
/// `attachment_counts`) — a `GROUP BY` per subquery rather than a per-event follow-up
/// query, so annotating every visible event with these counts stays one query
/// regardless of how many events are visible. `COALESCE(..., 0)` covers events with no
/// matching child rows at all, which is the common case and the reason these are
/// `LEFT` (not inner) joins.
pub fn events_for_visible_calendars(storage: &Storage) -> anyhow::Result<Vec<DisplayEvent>> {
    storage.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT events.id, COALESCE(events.title, '(No title)'), events.description, \
                    events.location, events.start, events.end, events.all_day, \
                    COALESCE(events.color, calendars.color), \
                    calendars.display_name, \
                    events.recurrence_rule, events.hangout_link, events.visibility, \
                    events.self_response_status, \
                    COALESCE(reminder_counts.cnt, 0), \
                    COALESCE(attendee_counts.cnt, 0), \
                    COALESCE(attachment_counts.cnt, 0), \
                    attendee_names.names \
             FROM events \
             JOIN calendars ON calendars.id = events.calendar_id \
             LEFT JOIN (SELECT event_id, COUNT(*) AS cnt FROM event_reminders GROUP BY event_id) \
                 reminder_counts ON reminder_counts.event_id = events.id \
             LEFT JOIN (SELECT event_id, SUM(CASE WHEN is_self = 0 THEN 1 ELSE 0 END) AS cnt \
                 FROM event_attendees GROUP BY event_id) \
                 attendee_counts ON attendee_counts.event_id = events.id \
             LEFT JOIN (SELECT event_id, COUNT(*) AS cnt FROM event_attachments GROUP BY event_id) \
                 attachment_counts ON attachment_counts.event_id = events.id \
             LEFT JOIN (SELECT event_id, GROUP_CONCAT(COALESCE(NULLIF(display_name, ''), email), '\u{1f}') AS names \
                 FROM event_attendees WHERE is_self = 0 GROUP BY event_id) \
                 attendee_names ON attendee_names.event_id = events.id \
             WHERE calendars.is_visible = 1 \
             ORDER BY events.start",
        )?;
        let rows = stmt.query_map([], |row| {
            let visibility = EventVisibility::parse(&row.get::<_, String>(11)?);
            Ok(DisplayEvent {
                id: row.get(0)?,
                title: row.get(1)?,
                description: row.get(2)?,
                location: row.get(3)?,
                start: row.get(4)?,
                end: row.get(5)?,
                all_day: row.get::<_, i64>(6)? != 0,
                color: row.get(7)?,
                calendar_name: row.get(8)?,
                is_recurring: row.get::<_, Option<String>>(9)?.is_some(),
                has_video_call: row.get::<_, Option<String>>(10)?.is_some(),
                is_private: visibility != EventVisibility::Default,
                self_response_status: row.get::<_, Option<String>>(12)?.map(|s| AttendeeResponseStatus::parse(&s)),
                reminder_count: row.get(13)?,
                other_attendee_count: row.get(14)?,
                attachment_count: row.get(15)?,
                other_attendee_names: row
                    .get::<_, Option<String>>(16)?
                    .map(|names| names.split('\u{1f}').map(str::to_string).collect())
                    .unwrap_or_default(),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
}

/// One calendar as shown in the sidebar's per-account calendar list (DESIGN_SPEC.md
/// §10) — its visibility toggle state included, since that's what the checkbox binds to.
#[derive(Debug, Clone)]
pub struct CalendarSummary {
    pub id: i64,
    pub account_id: AccountId,
    pub display_name: String,
    pub color: Option<String>,
    pub is_visible: bool,
}

/// Every calendar across every connected account, ordered so calendars for the same
/// account sort together — the sidebar groups these under one section per account.
pub fn calendars_by_account(storage: &Storage) -> anyhow::Result<Vec<CalendarSummary>> {
    storage.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, account_id, display_name, color, is_visible \
             FROM calendars \
             ORDER BY account_id, display_name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(CalendarSummary {
                id: row.get(0)?,
                account_id: AccountId(row.get(1)?),
                display_name: row.get(2)?,
                color: row.get(3)?,
                is_visible: row.get::<_, i64>(4)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
}

/// Flips a calendar's sidebar visibility toggle (DESIGN_SPEC.md §10) — purely a local
/// display preference, never synced back to Google.
pub fn set_calendar_visibility(storage: &Storage, calendar_id: i64, visible: bool) -> anyhow::Result<()> {
    storage.with_conn(|conn| {
        conn.execute(
            "UPDATE calendars SET is_visible = ?1 WHERE id = ?2",
            rusqlite::params![visible as i64, calendar_id],
        )?;
        Ok(())
    })
}

/// Sets a calendar's display color, picked from the sidebar customizer's palette
/// (DESIGN_SPEC.md §10). Purely a local display preference, like visibility — not yet
/// synced back to Google — but shared by both the sidebar swatch and every event dot
/// on that calendar, since both read the same `calendars.color` column.
pub fn set_calendar_color(storage: &Storage, calendar_id: i64, color: &str) -> anyhow::Result<()> {
    storage.with_conn(|conn| {
        conn.execute(
            "UPDATE calendars SET color = ?1 WHERE id = ?2",
            rusqlite::params![color, calendar_id],
        )?;
        Ok(())
    })
}

/// "Display this only" (the sidebar customizer's popover, DESIGN_SPEC.md §10): makes
/// `calendar_id` the sole visible calendar across every connected account, hiding all
/// others — mirrors the Google Calendar PWA's same-named action. `id = ?1` evaluates
/// to SQLite's 0/1 for false/true, so this is a single statement rather than a
/// select-then-update pair.
pub fn show_only_calendar(storage: &Storage, calendar_id: i64) -> anyhow::Result<()> {
    storage.with_conn(|conn| {
        conn.execute("UPDATE calendars SET is_visible = (id = ?1)", rusqlite::params![calendar_id])?;
        Ok(())
    })
}

/// The opposite of `show_only_calendar`: makes every calendar, across every
/// connected account, visible again.
pub fn show_all_calendars(storage: &Storage) -> anyhow::Result<()> {
    storage.with_conn(|conn| {
        conn.execute("UPDATE calendars SET is_visible = 1", [])?;
        Ok(())
    })
}

/// Makes every calendar belonging to one account visible again, leaving every
/// other account's calendars untouched.
pub fn show_all_calendars_for_account(storage: &Storage, account_id: AccountId) -> anyhow::Result<()> {
    storage.with_conn(|conn| {
        conn.execute(
            "UPDATE calendars SET is_visible = 1 WHERE account_id = ?1",
            rusqlite::params![account_id.0],
        )?;
        Ok(())
    })
}

/// Deletes an account's locally cached calendars for the Calendar service —
/// DESIGN_SPEC.md §12's "Clear local cache and resync" recovery action. Only
/// `calendars` needs an explicit delete: `events`, `sync_state`, and `pending_edits`
/// all reference `calendars(id)` with `ON DELETE CASCADE` (§8), so wiping the
/// calendars row for this account cascades everything else away with it. The account
/// itself, and its `account_services` toggle, are untouched — the caller re-populates
/// by running `Service::on_enabled` + `Service::sync` right after this returns.
pub fn clear_calendar_cache(storage: &Storage, account_id: AccountId) -> anyhow::Result<()> {
    storage.with_conn(|conn| {
        conn.execute("DELETE FROM calendars WHERE account_id = ?1", [account_id.0])?;
        Ok(())
    })
}

/// How a reminder is delivered — mirrors Google Calendar's `reminders.overrides[].method`
/// values (`"email"` / `"popup"`; Google's own UI labels `popup` as "Notification",
/// which is what the edit dialog's method dropdown shows too).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderMethod {
    Email,
    Popup,
}

impl ReminderMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            ReminderMethod::Email => "email",
            ReminderMethod::Popup => "popup",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "email" => ReminderMethod::Email,
            _ => ReminderMethod::Popup,
        }
    }
}

/// One reminder override on an event. Google Calendar allows several of these per
/// event (each independently a popup or an email, at its own lead time) rather than
/// the single lead time DESIGN_SPEC.md §10 originally scoped the event editor to.
#[derive(Debug, Clone, PartialEq)]
pub struct EventReminder {
    pub method: ReminderMethod,
    pub minutes: i64,
}

/// Whether an event blocks the calendar owner's time — Google Calendar's
/// `transparency` field (`"opaque"` = Busy, `"transparent"` = Free), shown in the
/// edit dialog as the "Busy"/"Free" dropdown next to the calendar picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventBusyStatus {
    Busy,
    Free,
}

impl EventBusyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            EventBusyStatus::Busy => "opaque",
            EventBusyStatus::Free => "transparent",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "transparent" => EventBusyStatus::Free,
            _ => EventBusyStatus::Busy,
        }
    }
}

/// Who besides the calendar owner can see this event's details — Google Calendar's
/// `visibility` field (iCalendar's `CLASS`). `Default` means "follow the calendar's
/// own sharing settings" (Google's own default), shown in the edit dialog as "Default
/// visibility". `Confidential` is a legacy CalDAV-interop value the edit dialog has no
/// dropdown entry for (it's not one of Google Calendar's own UI choices) — it's kept
/// here purely so a synced event carrying it round-trips instead of silently
/// downgrading to `Default` the moment its detail is loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventVisibility {
    Default,
    Public,
    Private,
    Confidential,
}

impl EventVisibility {
    pub fn as_str(self) -> &'static str {
        match self {
            EventVisibility::Default => "default",
            EventVisibility::Public => "public",
            EventVisibility::Private => "private",
            EventVisibility::Confidential => "confidential",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "public" => EventVisibility::Public,
            "private" => EventVisibility::Private,
            "confidential" => EventVisibility::Confidential,
            _ => EventVisibility::Default,
        }
    }
}

/// One guest's RSVP — Google Calendar's `attendees[].responseStatus` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttendeeResponseStatus {
    NeedsAction,
    Declined,
    Tentative,
    Accepted,
}

impl AttendeeResponseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AttendeeResponseStatus::NeedsAction => "needsAction",
            AttendeeResponseStatus::Declined => "declined",
            AttendeeResponseStatus::Tentative => "tentative",
            AttendeeResponseStatus::Accepted => "accepted",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "declined" => AttendeeResponseStatus::Declined,
            "tentative" => AttendeeResponseStatus::Tentative,
            "accepted" => AttendeeResponseStatus::Accepted,
            _ => AttendeeResponseStatus::NeedsAction,
        }
    }
}

/// One guest on an event, read-only — attendee management isn't a Calendarchy feature
/// (DESIGN_SPEC.md §2 scopes out calendar/guest administration), so unlike
/// `EventReminder` this has no corresponding entry in `EventEdits`.
#[derive(Debug, Clone, PartialEq)]
pub struct EventAttendeeInfo {
    pub email: String,
    pub display_name: Option<String>,
    pub response_status: AttendeeResponseStatus,
    pub is_self: bool,
    pub optional: bool,
}

/// One file attached to an event, read-only — same reasoning as
/// `EventAttendeeInfo`: attachment management isn't a Calendarchy feature, so there's
/// no corresponding entry in `EventEdits`.
#[derive(Debug, Clone, PartialEq)]
pub struct EventAttachmentInfo {
    pub file_url: String,
    pub title: Option<String>,
    pub mime_type: Option<String>,
}

/// Full detail for one event, loaded fresh when the user opens the edit dialog
/// (DESIGN_SPEC.md §10's event editor) — richer than `DisplayEvent`, which is shaped
/// for grid rendering only and doesn't carry `calendar_id`.
#[derive(Debug, Clone)]
pub struct EventDetail {
    pub id: i64,
    pub calendar_id: i64,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: String,
    pub end: String,
    pub all_day: bool,
    /// This event's own color override, distinct from `calendars.color` (§10's
    /// calendar customizer) — `None` means "use the calendar's color", same as
    /// Google Calendar's own event-color picker defaults to "Calendar color".
    pub color: Option<String>,
    pub reminders: Vec<EventReminder>,
    pub busy: EventBusyStatus,
    pub visibility: EventVisibility,
    /// This event series' repeat rule, as an RRULE value (no `"RRULE:"` prefix — see
    /// `recurrence::Recurrence::to_rrule_string`). `None` is "does not repeat". Any
    /// `EXDATE`/`RDATE`/`EXRULE` exceptions on the series live in `events`'
    /// `recurrence_exceptions` column instead (preserved through sync, but not
    /// surfaced here — §20's recurring-event edit scope note — so saving this dialog's
    /// recurrence picker never touches them).
    pub recurrence: Option<String>,
    pub organizer_email: Option<String>,
    pub organizer_name: Option<String>,
    /// Video-conferencing join link — Google's `hangoutLink`.
    pub hangout_link: Option<String>,
    /// iCalendar `SEQUENCE` / Google's `sequence` — how many times the organizer has
    /// revised this event.
    pub sequence: i64,
    pub created_at: Option<String>,
    /// The signed-in account's own RSVP, mirrored off whichever `attendees` entry has
    /// `is_self` — `None` when the event has no attendee list at all.
    pub self_response_status: Option<AttendeeResponseStatus>,
    pub attendees: Vec<EventAttendeeInfo>,
    /// A generic "more info" link — iCalendar's `URL` property (see
    /// `google_api::Event::url`'s doc comment for why Google-synced events never set
    /// this today).
    pub url: Option<String>,
    pub attachments: Vec<EventAttachmentInfo>,
}

/// Loads one event's full editable detail, or `None` if it's been deleted (e.g. by a
/// sync that ran between opening the popover and clicking Edit) out from under it.
pub fn event_detail(storage: &Storage, event_id: i64) -> anyhow::Result<Option<EventDetail>> {
    storage.with_conn(|conn| {
        let detail = conn
            .query_row(
                "SELECT id, calendar_id, COALESCE(title, ''), description, location, start, end, all_day, \
                        color, transparency, visibility, recurrence_rule, \
                        organizer_email, organizer_name, hangout_link, sequence, created_at, self_response_status, \
                        url \
                 FROM events WHERE id = ?1",
                [event_id],
                |row| {
                    Ok(EventDetail {
                        id: row.get(0)?,
                        calendar_id: row.get(1)?,
                        title: row.get(2)?,
                        description: row.get(3)?,
                        location: row.get(4)?,
                        start: row.get(5)?,
                        end: row.get(6)?,
                        all_day: row.get::<_, i64>(7)? != 0,
                        color: row.get(8)?,
                        reminders: Vec::new(),
                        busy: EventBusyStatus::parse(&row.get::<_, String>(9)?),
                        visibility: EventVisibility::parse(&row.get::<_, String>(10)?),
                        recurrence: row
                            .get::<_, Option<String>>(11)?
                            .map(|s| s.strip_prefix("RRULE:").map(str::to_string).unwrap_or(s)),
                        organizer_email: row.get(12)?,
                        organizer_name: row.get(13)?,
                        hangout_link: row.get(14)?,
                        sequence: row.get(15)?,
                        created_at: row.get(16)?,
                        self_response_status: row
                            .get::<_, Option<String>>(17)?
                            .map(|s| AttendeeResponseStatus::parse(&s)),
                        attendees: Vec::new(),
                        url: row.get(18)?,
                        attachments: Vec::new(),
                    })
                },
            )
            .optional()?;

        let Some(mut detail) = detail else {
            return Ok(None);
        };
        detail.reminders = load_reminders(conn, event_id)?;
        detail.attendees = load_attendees(conn, event_id)?;
        detail.attachments = load_attachments(conn, event_id)?;
        Ok(Some(detail))
    })
}

fn load_attachments(conn: &Connection, event_id: i64) -> anyhow::Result<Vec<EventAttachmentInfo>> {
    let mut stmt =
        conn.prepare("SELECT file_url, title, mime_type FROM event_attachments WHERE event_id = ?1 ORDER BY id")?;
    let rows = stmt.query_map([event_id], |row| {
        Ok(EventAttachmentInfo {
            file_url: row.get(0)?,
            title: row.get(1)?,
            mime_type: row.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn load_attendees(conn: &Connection, event_id: i64) -> anyhow::Result<Vec<EventAttendeeInfo>> {
    let mut stmt = conn.prepare(
        "SELECT email, display_name, response_status, is_self, optional \
         FROM event_attendees WHERE event_id = ?1 ORDER BY is_organizer DESC, id",
    )?;
    let rows = stmt.query_map([event_id], |row| {
        Ok(EventAttendeeInfo {
            email: row.get(0)?,
            display_name: row.get(1)?,
            response_status: AttendeeResponseStatus::parse(&row.get::<_, String>(2)?),
            is_self: row.get::<_, i64>(3)? != 0,
            optional: row.get::<_, i64>(4)? != 0,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn load_reminders(conn: &Connection, event_id: i64) -> anyhow::Result<Vec<EventReminder>> {
    let mut stmt = conn.prepare("SELECT method, minutes FROM event_reminders WHERE event_id = ?1 ORDER BY id")?;
    let rows = stmt.query_map([event_id], |row| {
        Ok(EventReminder {
            method: ReminderMethod::parse(&row.get::<_, String>(0)?),
            minutes: row.get(1)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Replaces every reminder on `event_id` with `reminders` — delete-then-reinsert
/// rather than a diff, since the edit dialog always hands back its full current list
/// rather than an incremental change set (same as how `update_event` overwrites every
/// other field wholesale instead of patching individual ones).
fn replace_reminders(conn: &Connection, event_id: i64, reminders: &[EventReminder]) -> anyhow::Result<()> {
    conn.execute("DELETE FROM event_reminders WHERE event_id = ?1", [event_id])?;
    for reminder in reminders {
        conn.execute(
            "INSERT INTO event_reminders (event_id, method, minutes) VALUES (?1, ?2, ?3)",
            rusqlite::params![event_id, reminder.method.as_str(), reminder.minutes],
        )?;
    }
    Ok(())
}

/// The fields the edit dialog can change. Which calendar the event lives on is passed
/// to `update_event` separately, alongside the *target* calendar's account — moving an
/// event to a calendar on a different account changes whose `pending_edits` queue (and
/// eventually whose sync engine, DESIGN_SPEC.md §9) is responsible for pushing it.
pub struct EventEdits {
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: String,
    pub end: String,
    pub all_day: bool,
    pub color: Option<String>,
    pub reminders: Vec<EventReminder>,
    pub busy: EventBusyStatus,
    pub visibility: EventVisibility,
    /// See `EventDetail::recurrence` — the same bare (no `"RRULE:"` prefix) RRULE
    /// value, `None` for "does not repeat".
    pub recurrence: Option<String>,
}

fn reminders_json(reminders: &[EventReminder]) -> serde_json::Value {
    serde_json::Value::Array(
        reminders
            .iter()
            .map(|r| serde_json::json!({ "method": r.method.as_str(), "minutes": r.minutes }))
            .collect(),
    )
}

fn edits_payload(edits: &EventEdits) -> serde_json::Value {
    serde_json::json!({
        "title": edits.title, "description": edits.description, "location": edits.location,
        "start": edits.start, "end": edits.end, "allDay": edits.all_day, "color": edits.color,
        "reminders": reminders_json(&edits.reminders), "transparency": edits.busy.as_str(),
        "visibility": edits.visibility.as_str(), "recurrence": edits.recurrence,
    })
}

/// Applies an edit made in the dialog: updates the local `events` row immediately, so
/// the UI reflects it right away (§9's optimistic-local-write pattern), and queues a
/// `pending_edits` row so a future sync-engine push (§9, roadmap phase 3) has
/// something to send to Google — no network call happens here yet.
pub fn update_event(
    storage: &Storage,
    event_id: i64,
    calendar_id: i64,
    account_id: AccountId,
    edits: &EventEdits,
) -> anyhow::Result<()> {
    storage.with_conn(|conn| {
        let (old_calendar_id, old_account_id, old_payload): (i64, i64, String) = conn.query_row(
            "SELECT events.calendar_id, calendars.account_id, json_object(
                'title', events.title, 'description', events.description, 'location', events.location,
                'start', events.start, 'end', events.end, 'allDay', events.all_day, 'color', events.color,
                'transparency', events.transparency, 'visibility', events.visibility,
                'recurrence', CASE WHEN events.recurrence_rule IS NULL THEN NULL
                    ELSE substr(events.recurrence_rule, 7) END, 'reminders', json('[]')
             ) FROM events JOIN calendars ON calendars.id = events.calendar_id WHERE events.id = ?1",
            [event_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let mut old_payload: serde_json::Value = serde_json::from_str(&old_payload)?;
        old_payload["reminders"] = reminders_json(&load_reminders(conn, event_id)?);
        conn.execute(
            "INSERT INTO event_edit_history (event_id, account_id, calendar_id, payload)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![event_id, old_account_id, old_calendar_id, old_payload.to_string()],
        )?;
        let recurrence_rule = edits.recurrence.as_deref().map(|r| format!("RRULE:{r}"));
        conn.execute(
            "UPDATE events SET
                calendar_id = ?1, title = ?2, description = ?3, location = ?4,
                start = ?5, end = ?6, all_day = ?7, color = ?8, transparency = ?9, visibility = ?10,
                recurrence_rule = ?11
             WHERE id = ?12",
            rusqlite::params![
                calendar_id,
                edits.title,
                edits.description,
                edits.location,
                edits.start,
                edits.end,
                edits.all_day as i64,
                edits.color,
                edits.busy.as_str(),
                edits.visibility.as_str(),
                recurrence_rule,
                event_id,
            ],
        )?;
        replace_reminders(conn, event_id, &edits.reminders)?;

        let payload = edits_payload(edits);
        conn.execute(
            "INSERT INTO pending_edits (account_id, calendar_id, event_id, operation, payload)
             VALUES (?1, ?2, ?3, 'update', ?4)",
            rusqlite::params![account_id.0, calendar_id, event_id, payload.to_string()],
        )?;
        Ok(())
    })
}

/// Restores the most recent event edit and queues the inverse update.
pub fn undo_last_event_edit(storage: &Storage) -> anyhow::Result<bool> {
    storage.with_conn(|conn| {
        let Some((history_id, event_id, account_id, calendar_id, payload)) = conn
            .query_row(
                "SELECT id, event_id, account_id, calendar_id, payload FROM event_edit_history
                 ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((
                    row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?, row.get::<_, String>(4)?,
                )),
            )
            .optional()?
        else { return Ok(false); };
        let payload: serde_json::Value = serde_json::from_str(&payload)?;
        let reminders = payload.get("reminders").and_then(serde_json::Value::as_array)
            .cloned().unwrap_or_default().into_iter().map(|reminder| EventReminder {
                method: ReminderMethod::parse(reminder.get("method").and_then(serde_json::Value::as_str).unwrap_or("popup")),
                minutes: reminder.get("minutes").and_then(serde_json::Value::as_i64).unwrap_or_default(),
            }).collect::<Vec<_>>();
        conn.execute(
            "UPDATE events SET calendar_id = ?1, title = ?2, description = ?3, location = ?4,
                start = ?5, end = ?6, all_day = ?7, color = ?8, transparency = ?9,
                visibility = ?10, recurrence_rule = ?11 WHERE id = ?12",
            rusqlite::params![
                calendar_id, payload["title"].as_str().unwrap_or_default(),
                payload["description"].as_str().map(str::to_string),
                payload["location"].as_str().map(str::to_string),
                payload["start"].as_str().unwrap_or_default(), payload["end"].as_str().unwrap_or_default(),
                payload["allDay"].as_bool().unwrap_or(false) as i64, payload["color"].as_str().map(str::to_string),
                payload["transparency"].as_str().unwrap_or("opaque"),
                payload["visibility"].as_str().unwrap_or("default"),
                payload["recurrence"].as_str().map(|value| format!("RRULE:{value}")), event_id,
            ],
        )?;
        replace_reminders(conn, event_id, &reminders)?;
        conn.execute(
            "INSERT INTO pending_edits (account_id, calendar_id, event_id, operation, payload)
             VALUES (?1, ?2, ?3, 'update', ?4)",
            rusqlite::params![account_id, calendar_id, event_id, payload.to_string()],
        )?;
        conn.execute("DELETE FROM event_edit_history WHERE id = ?1", [history_id])?;
        Ok(true)
    })
}

/// Inserts a locally-created event (the header bar's Create button, DESIGN_SPEC.md
/// §10) and queues a `pending_edits` row so a future sync-engine push (§9, roadmap
/// phase 3) has something to send to Google — mirrors `update_event`'s
/// optimistic-local-write pattern. There's no real Google event id yet, so
/// `google_event_id` gets a `local-`-prefixed placeholder, unique enough for the
/// `(calendar_id, google_event_id)` constraint since it's only ever read back by sync
/// (which will overwrite it with the real id once the create round-trips). Returns the
/// new row's local id.
pub fn create_event(storage: &Storage, calendar_id: i64, account_id: AccountId, edits: &EventEdits) -> anyhow::Result<i64> {
    storage.with_conn(|conn| {
        let placeholder_id = format!("local-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default());
        let recurrence_rule = edits.recurrence.as_deref().map(|r| format!("RRULE:{r}"));
        conn.execute(
            "INSERT INTO events (
                calendar_id, google_event_id, title, description, location,
                start, end, all_day, color, transparency, visibility, recurrence_rule
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                calendar_id,
                placeholder_id,
                edits.title,
                edits.description,
                edits.location,
                edits.start,
                edits.end,
                edits.all_day as i64,
                edits.color,
                edits.busy.as_str(),
                edits.visibility.as_str(),
                recurrence_rule,
            ],
        )?;
        let event_id = conn.last_insert_rowid();
        replace_reminders(conn, event_id, &edits.reminders)?;

        let payload = serde_json::json!({
            "title": edits.title,
            "description": edits.description,
            "location": edits.location,
            "start": edits.start,
            "end": edits.end,
            "allDay": edits.all_day,
            "color": edits.color,
            "reminders": reminders_json(&edits.reminders),
            "transparency": edits.busy.as_str(),
            "visibility": edits.visibility.as_str(),
            "recurrence": edits.recurrence,
        });
        conn.execute(
            "INSERT INTO pending_edits (account_id, calendar_id, event_id, operation, payload)
             VALUES (?1, ?2, ?3, 'create', ?4)",
            rusqlite::params![account_id.0, calendar_id, event_id, payload.to_string()],
        )?;
        Ok(event_id)
    })
}

/// Applies a delete made from the event popover: removes the local `events` row
/// immediately, so the UI reflects it right away (§9's optimistic-local-write
/// pattern), and queues a `pending_edits` row so a future sync-engine push (§9,
/// roadmap phase 3) has the Google event id it'll need to send the deletion.
///
/// The queued row's `event_id` is left `NULL` rather than set to `event_id`:
/// `pending_edits.event_id` is `ON DELETE CASCADE` against `events(id)`, so a
/// non-null value pointing at the row this function just deleted would make SQLite
/// cascade-delete the pending edit right along with it. A `NULL` foreign key isn't
/// checked against the parent table, so the row survives; the Google event id it
/// needs lives in `payload` instead.
pub fn delete_event(storage: &Storage, event_id: i64, calendar_id: i64, account_id: AccountId) -> anyhow::Result<()> {
    storage.with_conn(|conn| {
        let google_event_id: String =
            conn.query_row("SELECT google_event_id FROM events WHERE id = ?1", [event_id], |row| row.get(0))?;

        conn.execute("DELETE FROM events WHERE id = ?1", [event_id])?;

        let payload = serde_json::json!({ "googleEventId": google_event_id });
        conn.execute(
            "INSERT INTO pending_edits (account_id, calendar_id, event_id, operation, payload)
             VALUES (?1, ?2, NULL, 'delete', ?3)",
            rusqlite::params![account_id.0, calendar_id, payload.to_string()],
        )?;
        Ok(())
    })
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.with_timezone(&Utc))
}

/// One reminder that's ready to fire, or ready to resurface after a snooze —
/// DESIGN_SPEC.md §13's Notification Scheduler reads this every poll tick (see
/// `crates/app/src/notifications.rs`). Distinct from `EventReminder` (an event's
/// stored reminder *configuration*) — this is a single point-in-time delivery.
#[derive(Debug, Clone, PartialEq)]
pub struct DueReminder {
    pub event_id: i64,
    pub event_title: String,
    pub event_start: String,
    pub reminder_minutes: i64,
    pub fire_at: DateTime<Utc>,
}

/// Reminders due to fire right now: on visible calendars (mirrors
/// `events_for_visible_calendars`), popup-method only (an `email` reminder is already
/// delivered by Google's own servers — re-notifying for it would be redundant), whose
/// `start - minutes` has passed, and that haven't already been delivered for the
/// event's *current* start — see `reminder_notifications`' `UNIQUE(event_id,
/// reminder_minutes, event_start)` key, which is why editing an event's start
/// naturally produces a fresh reminder rather than being suppressed by an old row. A
/// previously `snoozed` firing is included again once its `snoozed_until` has passed;
/// `active`/`dismissed` firings for the same key are excluded. All-day events (whose
/// `start` is a bare date, not RFC 3339) have no wall-clock time to anchor "N minutes
/// before" against, so they're silently skipped rather than mis-timed.
pub fn due_reminders(storage: &Storage, now: DateTime<Utc>) -> anyhow::Result<Vec<DueReminder>> {
    storage.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT events.id, COALESCE(events.title, '(No title)'), events.start, event_reminders.minutes \
             FROM events \
             JOIN calendars ON calendars.id = events.calendar_id \
             JOIN event_reminders ON event_reminders.event_id = events.id \
             LEFT JOIN reminder_notifications \
                 ON reminder_notifications.event_id = events.id \
                AND reminder_notifications.reminder_minutes = event_reminders.minutes \
                AND reminder_notifications.event_start = events.start \
             WHERE calendars.is_visible = 1 \
               AND event_reminders.method = 'popup' \
               AND (reminder_notifications.id IS NULL \
                    OR (reminder_notifications.status = 'snoozed' \
                        AND reminder_notifications.snoozed_until <= ?1))",
        )?;
        let rows = stmt.query_map(rusqlite::params![now.to_rfc3339()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (event_id, event_title, event_start, reminder_minutes) = row?;
            let Some(start) = parse_rfc3339(&event_start) else { continue };
            let fire_at = start - Duration::minutes(reminder_minutes);
            if fire_at <= now {
                out.push(DueReminder {
                    event_id,
                    event_title,
                    event_start,
                    reminder_minutes,
                    fire_at,
                });
            }
        }
        Ok(out)
    })
}

/// Records that `reminder` has been delivered (or is being delivered right now) —
/// upserts a `reminder_notifications` row and returns its id. Called once per
/// `DueReminder` right after the scheduler tick decides to deliver it (DESIGN_SPEC.md
/// §13), before dispatching to the OS notifier and/or the in-app dialog, so a crash or
/// slow send between the two doesn't cause a duplicate on the next poll tick. Also
/// covers a resurfacing snoozed reminder — the `ON CONFLICT` arm resets it back to
/// `active`.
pub fn record_fired(storage: &Storage, reminder: &DueReminder) -> anyhow::Result<i64> {
    storage.with_conn(|conn| {
        conn.execute(
            "INSERT INTO reminder_notifications (event_id, event_title, event_start, reminder_minutes, status) \
             VALUES (?1, ?2, ?3, ?4, 'active') \
             ON CONFLICT(event_id, reminder_minutes, event_start) \
             DO UPDATE SET status = 'active', snoozed_until = NULL, fired_at = datetime('now')",
            rusqlite::params![reminder.event_id, reminder.event_title, reminder.event_start, reminder.reminder_minutes],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM reminder_notifications WHERE event_id = ?1 AND reminder_minutes = ?2 AND event_start = ?3",
            rusqlite::params![reminder.event_id, reminder.reminder_minutes, reminder.event_start],
            |row| row.get(0),
        )?)
    })
}

/// Snoozes one active firing until `until` — the floating dialog's Snooze button
/// (DESIGN_SPEC.md §12's "Show snoozed notifications" lead time decides what `until`
/// resolves to; see `crates/app/src/notifications.rs`). `due_reminders` picks the row
/// back up once `until` has passed.
pub fn snooze_reminder(storage: &Storage, id: i64, until: DateTime<Utc>) -> anyhow::Result<()> {
    storage.with_conn(|conn| {
        conn.execute(
            "UPDATE reminder_notifications SET status = 'snoozed', snoozed_until = ?1 WHERE id = ?2",
            rusqlite::params![until.to_rfc3339(), id],
        )?;
        Ok(())
    })
}

/// Dismisses one firing — it drops off the floating dialog's active card list and
/// moves into its "Past" history section (`recent_reminder_history`).
pub fn dismiss_reminder(storage: &Storage, id: i64) -> anyhow::Result<()> {
    storage.with_conn(|conn| {
        conn.execute("UPDATE reminder_notifications SET status = 'dismissed' WHERE id = ?1", [id])?;
        Ok(())
    })
}

/// The floating dialog's "Dismiss all" action — dismisses every currently
/// active-or-snoozed firing in one statement.
pub fn dismiss_all_active(storage: &Storage) -> anyhow::Result<()> {
    storage.with_conn(|conn| {
        conn.execute(
            "UPDATE reminder_notifications SET status = 'dismissed' WHERE status IN ('active', 'snoozed')",
            [],
        )?;
        Ok(())
    })
}

/// The floating dialog's "Snooze all" action — snoozes every currently *active*
/// firing until `until` in one statement. Unlike `dismiss_all_active`, this leaves
/// already-`snoozed` rows alone: they aren't shown in the active card list, so
/// there's nothing on screen to justify silently re-snoozing them further.
pub fn snooze_all_active(storage: &Storage, until: DateTime<Utc>) -> anyhow::Result<()> {
    storage.with_conn(|conn| {
        conn.execute(
            "UPDATE reminder_notifications SET status = 'snoozed', snoozed_until = ?1 WHERE status = 'active'",
            rusqlite::params![until.to_rfc3339()],
        )?;
        Ok(())
    })
}

/// Currently active (undismissed, not currently snoozed) firings — what the floating
/// dialog's card list renders. Read fresh on every dialog rebuild rather than trusted
/// to stay in sync with in-memory state, so the dialog is correct even right after the
/// app restarts with alerts still pending from a previous run.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveReminder {
    pub id: i64,
    pub event_title: String,
    pub event_start: String,
    pub fired_at: String,
}

pub fn active_reminder_notifications(storage: &Storage) -> anyhow::Result<Vec<ActiveReminder>> {
    storage.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, event_title, event_start, fired_at FROM reminder_notifications \
             WHERE status = 'active' ORDER BY fired_at",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ActiveReminder {
                id: row.get(0)?,
                event_title: row.get(1)?,
                event_start: row.get(2)?,
                fired_at: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
}

/// One row of the floating dialog's "Past" section — a previously dismissed firing,
/// most recent first. Snapshotted `event_title`/`event_start` (not re-read off
/// `events`) mean this still renders sensibly even if the underlying event has since
/// been edited or deleted.
#[derive(Debug, Clone, PartialEq)]
pub struct ReminderHistoryEntry {
    pub id: i64,
    pub event_title: String,
    pub event_start: String,
    pub fired_at: String,
}

pub fn recent_reminder_history(storage: &Storage, limit: i64) -> anyhow::Result<Vec<ReminderHistoryEntry>> {
    storage.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, event_title, event_start, fired_at FROM reminder_notifications \
             WHERE status = 'dismissed' ORDER BY fired_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |row| {
            Ok(ReminderHistoryEntry {
                id: row.get(0)?,
                event_title: row.get(1)?,
                event_start: row.get(2)?,
                fired_at: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
}

/// Removes one row from the floating dialog's "Past" history entirely — its "Clear"
/// action on a single history entry. Unlike `dismiss_reminder` (active → dismissed,
/// so it still shows up in history), this drops the row for good.
pub fn delete_reminder_notification(storage: &Storage, id: i64) -> anyhow::Result<()> {
    storage.with_conn(|conn| {
        conn.execute("DELETE FROM reminder_notifications WHERE id = ?1", [id])?;
        Ok(())
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
                        ..Default::default()
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

    #[test]
    fn events_for_visible_calendars_surfaces_reminder_attendee_and_attachment_counts_and_flags() {
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
                        background_color: None,
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
                        id: "busy-1".into(),
                        status: Some("confirmed".into()),
                        summary: Some("Quarterly review".into()),
                        start: EventDateTime {
                            date_time: Some("2026-09-05T09:00:00-04:00".into()),
                            ..Default::default()
                        },
                        end: EventDateTime {
                            date_time: Some("2026-09-05T10:00:00-04:00".into()),
                            ..Default::default()
                        },
                        recurrence: vec!["RRULE:FREQ=WEEKLY".into()],
                        hangout_link: Some("https://meet.google.com/abc-defg-hij".into()),
                        visibility: Some("private".into()),
                        attendees: vec![
                            crate::google_api::EventAttendee {
                                email: "jane@gmail.com".into(),
                                display_name: Some("Jane".into()),
                                response_status: "accepted".into(),
                                is_self: true,
                                optional: false,
                                organizer: true,
                            },
                            crate::google_api::EventAttendee {
                                email: "guest1@example.com".into(),
                                display_name: None,
                                response_status: "needsAction".into(),
                                is_self: false,
                                optional: false,
                                organizer: false,
                            },
                            crate::google_api::EventAttendee {
                                email: "guest2@example.com".into(),
                                display_name: None,
                                response_status: "declined".into(),
                                is_self: false,
                                optional: false,
                                organizer: false,
                            },
                            crate::google_api::EventAttendee {
                                email: "guest3@example.com".into(),
                                display_name: None,
                                response_status: "tentative".into(),
                                is_self: false,
                                optional: false,
                                organizer: false,
                            },
                        ],
                        attachments: vec![crate::google_api::EventAttachment {
                            file_url: "https://example.com/agenda.pdf".into(),
                            title: Some("Agenda".into()),
                            mime_type: Some("application/pdf".into()),
                            icon_link: None,
                            file_id: None,
                        }],
                        ..Default::default()
                    },
                )
            })
            .expect("insert event");
        let event_id: i64 = storage
            .with_conn(|conn| {
                Ok(conn.query_row("SELECT id FROM events WHERE google_event_id = 'busy-1'", [], |r| r.get(0))?)
            })
            .expect("event id");
        storage
            .with_conn(|conn| {
                replace_reminders(
                    conn,
                    event_id,
                    &[
                        EventReminder { method: ReminderMethod::Popup, minutes: 10 },
                        EventReminder { method: ReminderMethod::Email, minutes: 60 },
                    ],
                )
            })
            .expect("insert reminders");

        let visible = events_for_visible_calendars(&storage).expect("query");
        assert_eq!(visible.len(), 1);
        let event = &visible[0];
        assert_eq!(event.reminder_count, 2);
        assert_eq!(event.other_attendee_count, 3, "excludes the signed-in account's own attendee row");
        let mut names = event.other_attendee_names.clone();
        names.sort();
        assert_eq!(
            names,
            vec!["guest1@example.com", "guest2@example.com", "guest3@example.com"],
            "falls back to email for attendees with no display name, and excludes the signed-in account's own row"
        );
        assert_eq!(event.attachment_count, 1);
        assert!(event.is_recurring);
        assert!(event.has_video_call);
        assert!(event.is_private);
        assert_eq!(event.self_response_status, Some(AttendeeResponseStatus::Accepted));
    }

    fn seed_event(storage: &Storage, calendar_id: i64) -> i64 {
        storage
            .with_conn(|conn| {
                crate::storage::upsert_event(
                    conn,
                    calendar_id,
                    &crate::google_api::Event {
                        id: "abc".into(),
                        status: Some("confirmed".into()),
                        summary: Some("Standup".into()),
                        description: Some("Daily sync".into()),
                        location: Some("Room 1".into()),
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
                        ..Default::default()
                    },
                )
            })
            .expect("insert event");
        storage
            .with_conn(|conn| {
                Ok(conn.query_row("SELECT id FROM events WHERE google_event_id = 'abc'", [], |r| r.get(0))?)
            })
            .expect("event id")
    }

    #[test]
    fn set_calendar_color_updates_stored_color() {
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

        set_calendar_color(&storage, calendar_id, "#8e24aa").expect("set color");

        let calendars = calendars_by_account(&storage).expect("query");
        assert_eq!(calendars[0].color.as_deref(), Some("#8e24aa"));
    }

    #[test]
    fn show_only_calendar_hides_every_other_calendar() {
        let storage = setup();
        let account_id = AccountId(1);
        let keep = storage
            .with_conn(|conn| {
                upsert_calendar(
                    conn,
                    account_id,
                    &CalendarListEntry {
                        id: "jane@gmail.com".into(),
                        summary: "Jane".into(),
                        background_color: None,
                        access_role: "owner".into(),
                        primary: true,
                    },
                )
            })
            .expect("calendar");
        let other = storage
            .with_conn(|conn| {
                upsert_calendar(
                    conn,
                    account_id,
                    &CalendarListEntry {
                        id: "work@gmail.com".into(),
                        summary: "Work".into(),
                        background_color: None,
                        access_role: "owner".into(),
                        primary: false,
                    },
                )
            })
            .expect("calendar");

        show_only_calendar(&storage, keep).expect("isolate");

        let calendars = calendars_by_account(&storage).expect("query");
        let keep_visible = calendars.iter().find(|c| c.id == keep).expect("kept calendar").is_visible;
        let other_visible = calendars.iter().find(|c| c.id == other).expect("other calendar").is_visible;
        assert!(keep_visible);
        assert!(!other_visible);
    }

    #[test]
    fn show_all_calendars_reveals_every_hidden_calendar() {
        let storage = setup();
        let account_id = AccountId(1);
        let first = storage
            .with_conn(|conn| {
                upsert_calendar(
                    conn,
                    account_id,
                    &CalendarListEntry {
                        id: "jane@gmail.com".into(),
                        summary: "Jane".into(),
                        background_color: None,
                        access_role: "owner".into(),
                        primary: true,
                    },
                )
            })
            .expect("calendar");
        let second = storage
            .with_conn(|conn| {
                upsert_calendar(
                    conn,
                    account_id,
                    &CalendarListEntry {
                        id: "work@gmail.com".into(),
                        summary: "Work".into(),
                        background_color: None,
                        access_role: "owner".into(),
                        primary: false,
                    },
                )
            })
            .expect("calendar");
        show_only_calendar(&storage, first).expect("isolate");

        show_all_calendars(&storage).expect("show all");

        let calendars = calendars_by_account(&storage).expect("query");
        assert!(calendars.iter().find(|c| c.id == first).expect("first").is_visible);
        assert!(calendars.iter().find(|c| c.id == second).expect("second").is_visible);
    }

    #[test]
    fn show_all_calendars_for_account_only_affects_that_account() {
        let storage = setup();
        let account_a = AccountId(1);
        let account_b = AccountId(2);
        storage
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO accounts (id, provider, email) VALUES (2, 'google', 'other@gmail.com')",
                    [],
                )?;
                Ok(())
            })
            .expect("insert second account");
        let calendar_a = storage
            .with_conn(|conn| {
                upsert_calendar(
                    conn,
                    account_a,
                    &CalendarListEntry {
                        id: "jane@gmail.com".into(),
                        summary: "Jane".into(),
                        background_color: None,
                        access_role: "owner".into(),
                        primary: true,
                    },
                )
            })
            .expect("calendar a");
        let calendar_b = storage
            .with_conn(|conn| {
                upsert_calendar(
                    conn,
                    account_b,
                    &CalendarListEntry {
                        id: "other@gmail.com".into(),
                        summary: "Other".into(),
                        background_color: None,
                        access_role: "owner".into(),
                        primary: true,
                    },
                )
            })
            .expect("calendar b");
        set_calendar_visibility(&storage, calendar_a, false).expect("hide a");
        set_calendar_visibility(&storage, calendar_b, false).expect("hide b");

        show_all_calendars_for_account(&storage, account_a).expect("show all for account a");

        let calendars = calendars_by_account(&storage).expect("query");
        assert!(calendars.iter().find(|c| c.id == calendar_a).expect("calendar a").is_visible);
        assert!(!calendars.iter().find(|c| c.id == calendar_b).expect("calendar b").is_visible);
    }

    #[test]
    fn event_detail_loads_full_row_for_editing() {
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
                        background_color: None,
                        access_role: "owner".into(),
                        primary: true,
                    },
                )
            })
            .expect("calendar");
        let event_id = seed_event(&storage, calendar_id);

        let detail = event_detail(&storage, event_id).expect("query").expect("found");
        assert_eq!(detail.title, "Standup");
        assert_eq!(detail.location.as_deref(), Some("Room 1"));
        assert_eq!(detail.calendar_id, calendar_id);
        assert!(!detail.all_day);
        assert_eq!(detail.color, None);
        assert!(detail.reminders.is_empty());
        assert_eq!(detail.busy, EventBusyStatus::Busy, "an event with no explicit choice defaults to Busy");
        assert_eq!(detail.visibility, EventVisibility::Default);
        assert_eq!(detail.recurrence, None);
        assert_eq!(detail.organizer_email, None);
        assert_eq!(detail.hangout_link, None);
        assert_eq!(detail.sequence, 0);
        assert_eq!(detail.self_response_status, None, "an event with no attendee list has no self RSVP");
        assert!(detail.attendees.is_empty());
        assert_eq!(detail.url, None);
        assert!(detail.attachments.is_empty());

        assert!(event_detail(&storage, event_id + 1000).expect("query").is_none());
    }

    #[test]
    fn event_detail_surfaces_organizer_attendees_and_self_response_status() {
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
                        background_color: None,
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
                        id: "meeting-1".into(),
                        status: Some("confirmed".into()),
                        summary: Some("repeating".into()),
                        start: EventDateTime {
                            date_time: Some("2026-08-31T09:00:00Z".into()),
                            ..Default::default()
                        },
                        end: EventDateTime {
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
                    },
                )
            })
            .expect("insert event");
        let event_id = storage
            .with_conn(|conn| {
                Ok(conn.query_row("SELECT id FROM events WHERE google_event_id = 'meeting-1'", [], |r| r.get(0))?)
            })
            .expect("event id");

        let detail = event_detail(&storage, event_id).expect("query").expect("found");
        assert_eq!(detail.organizer_email.as_deref(), Some("clawmeariver@gmail.com"));
        assert_eq!(detail.organizer_name.as_deref(), Some("Claw Meariver"));
        assert_eq!(detail.hangout_link.as_deref(), Some("https://meet.google.com/yaf-zbof-ubm"));
        assert_eq!(detail.self_response_status, Some(AttendeeResponseStatus::Accepted));
        assert_eq!(detail.attendees.len(), 2);
        assert!(detail.attendees.iter().any(|a| a.email == "clawmeariver@gmail.com" && a.is_self));
        let guest = detail.attendees.iter().find(|a| a.email == "silversword@gmail.com").expect("guest");
        assert_eq!(guest.response_status, AttendeeResponseStatus::NeedsAction);
        assert!(!guest.is_self);
    }

    #[test]
    fn event_detail_surfaces_url_and_attachments() {
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
                        background_color: None,
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
                        id: "spam-1".into(),
                        status: Some("confirmed".into()),
                        summary: Some("hLNQOCeWW".into()),
                        start: EventDateTime {
                            date: Some("2022-04-17".into()),
                            ..Default::default()
                        },
                        end: EventDateTime {
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
                    },
                )
            })
            .expect("insert event");
        let event_id = storage
            .with_conn(|conn| {
                Ok(conn.query_row("SELECT id FROM events WHERE google_event_id = 'spam-1'", [], |r| r.get(0))?)
            })
            .expect("event id");

        let detail = event_detail(&storage, event_id).expect("query").expect("found");
        assert_eq!(detail.url.as_deref(), Some("http://tinyurl.com/gl84y72"));
        assert_eq!(detail.attachments.len(), 1);
        assert_eq!(detail.attachments[0].file_url, "https://gateway.icloud.com/caldav/.../attach/...");
        assert_eq!(detail.attachments[0].mime_type.as_deref(), Some("text/html"));
    }

    #[test]
    fn create_event_writes_local_row_and_queues_pending_edit() {
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
                        background_color: None,
                        access_role: "owner".into(),
                        primary: true,
                    },
                )
            })
            .expect("calendar");

        let edits = EventEdits {
            title: "New event".into(),
            description: None,
            location: Some("Room 3".into()),
            start: "2026-09-05T11:00:00-04:00".into(),
            end: "2026-09-05T11:30:00-04:00".into(),
            all_day: false,
            color: Some("#0b8043".into()),
            reminders: vec![
                EventReminder {
                    method: ReminderMethod::Popup,
                    minutes: 10,
                },
                EventReminder {
                    method: ReminderMethod::Email,
                    minutes: 1440,
                },
            ],
            busy: EventBusyStatus::Free,
            visibility: EventVisibility::Private,
            recurrence: Some("FREQ=WEEKLY;BYDAY=TU".into()),
        };
        let event_id = create_event(&storage, calendar_id, account_id, &edits).expect("create");

        let detail = event_detail(&storage, event_id).expect("query").expect("found");
        assert_eq!(detail.title, "New event");
        assert_eq!(detail.calendar_id, calendar_id);
        assert_eq!(detail.location.as_deref(), Some("Room 3"));
        assert_eq!(detail.color.as_deref(), Some("#0b8043"));
        assert_eq!(detail.busy, EventBusyStatus::Free);
        assert_eq!(detail.visibility, EventVisibility::Private);
        assert_eq!(detail.recurrence.as_deref(), Some("FREQ=WEEKLY;BYDAY=TU"));
        assert_eq!(
            detail.reminders,
            vec![
                EventReminder {
                    method: ReminderMethod::Popup,
                    minutes: 10
                },
                EventReminder {
                    method: ReminderMethod::Email,
                    minutes: 1440
                },
            ]
        );

        let pending_count: i64 = storage
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM pending_edits WHERE event_id = ?1 AND operation = 'create'",
                    [event_id],
                    |r| r.get(0),
                )?)
            })
            .expect("count");
        assert_eq!(pending_count, 1);
    }

    #[test]
    fn update_event_writes_local_row_and_queues_pending_edit() {
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
                        background_color: None,
                        access_role: "owner".into(),
                        primary: true,
                    },
                )
            })
            .expect("calendar");
        let event_id = seed_event(&storage, calendar_id);

        let edits = EventEdits {
            title: "Standup (renamed)".into(),
            description: None,
            location: Some("Room 2".into()),
            start: "2026-09-05T10:00:00-04:00".into(),
            end: "2026-09-05T10:30:00-04:00".into(),
            all_day: false,
            color: Some("#d50000".into()),
            reminders: vec![EventReminder {
                method: ReminderMethod::Popup,
                minutes: 30,
            }],
            busy: EventBusyStatus::Free,
            visibility: EventVisibility::Public,
            recurrence: None,
        };
        update_event(&storage, event_id, calendar_id, account_id, &edits).expect("update");

        let detail = event_detail(&storage, event_id).expect("query").expect("found");
        assert_eq!(detail.title, "Standup (renamed)");
        assert_eq!(detail.description, None);
        assert_eq!(detail.location.as_deref(), Some("Room 2"));
        assert_eq!(detail.start, "2026-09-05T10:00:00-04:00");
        assert_eq!(detail.color.as_deref(), Some("#d50000"));
        assert_eq!(detail.busy, EventBusyStatus::Free);
        assert_eq!(detail.visibility, EventVisibility::Public);
        assert_eq!(detail.recurrence, None, "an event with no recurrence set must round-trip as does-not-repeat");
        assert_eq!(
            detail.reminders,
            vec![EventReminder {
                method: ReminderMethod::Popup,
                minutes: 30
            }]
        );

        let pending_count: i64 = storage
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM pending_edits WHERE event_id = ?1 AND operation = 'update'",
                    [event_id],
                    |r| r.get(0),
                )?)
            })
            .expect("count");
        assert_eq!(pending_count, 1);

        // A second update with a shorter reminder list must fully replace the first
        // one, not accumulate alongside it (`replace_reminders` deletes before
        // re-inserting rather than diffing).
        let fewer_reminders = EventEdits {
            reminders: vec![EventReminder {
                method: ReminderMethod::Email,
                minutes: 5,
            }],
            ..edits
        };
        update_event(&storage, event_id, calendar_id, account_id, &fewer_reminders).expect("second update");
        let detail = event_detail(&storage, event_id).expect("query").expect("found");
        assert_eq!(
            detail.reminders,
            vec![EventReminder {
                method: ReminderMethod::Email,
                minutes: 5
            }]
        );
    }

    #[test]
    fn undo_last_event_edit_restores_previous_values() {
        let storage = setup();
        let account_id = AccountId(1);
        let calendar_id = storage.with_conn(|conn| {
            upsert_calendar(conn, account_id, &CalendarListEntry {
                id: "jane@gmail.com".into(), summary: "Jane".into(), background_color: None,
                access_role: "owner".into(), primary: true,
            })
        }).expect("calendar");
        let event_id = seed_event(&storage, calendar_id);
        let edits = EventEdits {
            title: "Changed".into(), description: Some("new".into()), location: None,
            start: "2026-09-05T11:00:00-04:00".into(), end: "2026-09-05T11:30:00-04:00".into(),
            all_day: false, color: None, reminders: vec![], busy: EventBusyStatus::Free,
            visibility: EventVisibility::Public, recurrence: Some("FREQ=DAILY".into()),
        };
        update_event(&storage, event_id, calendar_id, account_id, &edits).expect("update");
        assert!(undo_last_event_edit(&storage).expect("undo"));
        let detail = event_detail(&storage, event_id).expect("query").expect("found");
        assert_eq!(detail.title, "Standup");
        assert_eq!(detail.description.as_deref(), Some("Daily sync"));
        assert_eq!(detail.location.as_deref(), Some("Room 1"));
        assert_eq!(detail.start, "2026-09-05T09:00:00-04:00");
        assert_eq!(detail.busy, EventBusyStatus::Busy);
        assert_eq!(detail.visibility, EventVisibility::Default);
        assert_eq!(detail.recurrence, None);
        assert!(!undo_last_event_edit(&storage).expect("empty undo"));
    }

    #[test]
    fn delete_event_removes_local_row_and_queues_pending_edit() {
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
                        background_color: None,
                        access_role: "owner".into(),
                        primary: true,
                    },
                )
            })
            .expect("calendar");
        let event_id = seed_event(&storage, calendar_id);

        delete_event(&storage, event_id, calendar_id, account_id).expect("delete");

        assert!(event_detail(&storage, event_id).expect("query").is_none());

        let (pending_count, payload): (i64, String) = storage
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT count(*), COALESCE(MAX(payload), '') FROM pending_edits \
                     WHERE calendar_id = ?1 AND operation = 'delete'",
                    [calendar_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .expect("count");
        assert_eq!(pending_count, 1);
        assert!(payload.contains("abc"), "payload should carry the google event id: {payload}");
    }

    #[test]
    fn clear_calendar_cache_cascades_events_and_leaves_other_accounts_alone() {
        let storage = setup();
        let account_id = AccountId(1);
        let other_account_id = AccountId(2);
        storage
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO accounts (id, provider, email) VALUES (2, 'google', 'bob@gmail.com')",
                    [],
                )?;
                Ok(())
            })
            .expect("seed second account");
        let calendar_id = storage
            .with_conn(|conn| {
                upsert_calendar(
                    conn,
                    account_id,
                    &CalendarListEntry {
                        id: "jane@gmail.com".into(),
                        summary: "Jane".into(),
                        background_color: None,
                        access_role: "owner".into(),
                        primary: true,
                    },
                )
            })
            .expect("calendar");
        seed_event(&storage, calendar_id);
        let other_calendar_id = storage
            .with_conn(|conn| {
                upsert_calendar(
                    conn,
                    other_account_id,
                    &CalendarListEntry {
                        id: "bob@gmail.com".into(),
                        summary: "Bob".into(),
                        background_color: None,
                        access_role: "owner".into(),
                        primary: true,
                    },
                )
            })
            .expect("calendar");

        clear_calendar_cache(&storage, account_id).expect("clear");

        let calendars = calendars_by_account(&storage).expect("query");
        assert_eq!(calendars.len(), 1);
        assert_eq!(calendars[0].id, other_calendar_id);

        let event_count: i64 = storage
            .with_conn(|conn| Ok(conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?))
            .expect("count");
        assert_eq!(event_count, 0);
    }

    /// Seeds one timed event titled "Standup" starting at `start` with a single popup
    /// reminder `minutes` ahead of it — the shared setup for the
    /// `due_reminders`/`record_fired`/snooze/dismiss tests below.
    fn seed_event_with_reminder(storage: &Storage, calendar_id: i64, start: &str, minutes: i64) -> i64 {
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
                            date_time: Some(start.into()),
                            ..Default::default()
                        },
                        end: EventDateTime {
                            date_time: Some(start.into()),
                            ..Default::default()
                        },
                        recurrence: vec![],
                        etag: None,
                        updated: None,
                        ..Default::default()
                    },
                )
            })
            .expect("insert event");
        let event_id: i64 = storage
            .with_conn(|conn| {
                Ok(conn.query_row("SELECT id FROM events WHERE google_event_id = 'abc'", [], |r| r.get(0))?)
            })
            .expect("event id");
        storage
            .with_conn(|conn| {
                replace_reminders(
                    conn,
                    event_id,
                    &[EventReminder {
                        method: ReminderMethod::Popup,
                        minutes,
                    }],
                )
            })
            .expect("insert reminder");
        event_id
    }

    fn seed_calendar(storage: &Storage, account_id: AccountId) -> i64 {
        storage
            .with_conn(|conn| {
                upsert_calendar(
                    conn,
                    account_id,
                    &CalendarListEntry {
                        id: "jane@gmail.com".into(),
                        summary: "Jane".into(),
                        background_color: None,
                        access_role: "owner".into(),
                        primary: true,
                    },
                )
            })
            .expect("calendar")
    }

    #[test]
    fn due_reminders_returns_popup_reminders_past_their_lead_time() {
        let storage = setup();
        let calendar_id = seed_calendar(&storage, AccountId(1));
        // Event starts at 09:00, reminder fires 10 minutes ahead, so 08:52 is due.
        seed_event_with_reminder(&storage, calendar_id, "2026-09-05T09:00:00Z", 10);

        let not_yet = DateTime::parse_from_rfc3339("2026-09-05T08:40:00Z").unwrap().with_timezone(&Utc);
        assert!(due_reminders(&storage, not_yet).expect("query").is_empty());

        let now = DateTime::parse_from_rfc3339("2026-09-05T08:52:00Z").unwrap().with_timezone(&Utc);
        let due = due_reminders(&storage, now).expect("query");
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].event_title, "Standup");
        assert_eq!(due[0].reminder_minutes, 10);
    }

    #[test]
    fn due_reminders_skips_email_method_reminders() {
        let storage = setup();
        let calendar_id = seed_calendar(&storage, AccountId(1));
        let event_id = seed_event_with_reminder(&storage, calendar_id, "2026-09-05T09:00:00Z", 10);
        storage
            .with_conn(|conn| {
                replace_reminders(
                    conn,
                    event_id,
                    &[EventReminder {
                        method: ReminderMethod::Email,
                        minutes: 10,
                    }],
                )
            })
            .expect("insert email reminder");

        let now = DateTime::parse_from_rfc3339("2026-09-05T09:00:00Z").unwrap().with_timezone(&Utc);
        assert!(due_reminders(&storage, now).expect("query").is_empty());
    }

    #[test]
    fn record_fired_suppresses_the_same_reminder_on_the_next_poll() {
        let storage = setup();
        let calendar_id = seed_calendar(&storage, AccountId(1));
        seed_event_with_reminder(&storage, calendar_id, "2026-09-05T09:00:00Z", 10);
        let now = DateTime::parse_from_rfc3339("2026-09-05T09:00:00Z").unwrap().with_timezone(&Utc);

        let due = due_reminders(&storage, now).expect("query");
        assert_eq!(due.len(), 1);
        record_fired(&storage, &due[0]).expect("record");

        assert!(due_reminders(&storage, now).expect("query").is_empty(), "already-fired reminder shouldn't refire");
    }

    #[test]
    fn snoozed_reminder_resurfaces_only_after_snoozed_until() {
        let storage = setup();
        let calendar_id = seed_calendar(&storage, AccountId(1));
        seed_event_with_reminder(&storage, calendar_id, "2026-09-05T09:00:00Z", 10);
        let now = DateTime::parse_from_rfc3339("2026-09-05T09:00:00Z").unwrap().with_timezone(&Utc);

        let due = due_reminders(&storage, now).expect("query");
        let id = record_fired(&storage, &due[0]).expect("record");

        let snooze_until = now + Duration::minutes(5);
        snooze_reminder(&storage, id, snooze_until).expect("snooze");
        assert!(
            due_reminders(&storage, now + Duration::minutes(2)).expect("query").is_empty(),
            "still snoozed"
        );

        let resurfaced = due_reminders(&storage, snooze_until).expect("query");
        assert_eq!(resurfaced.len(), 1, "resurfaces once snoozed_until has passed");
    }

    #[test]
    fn dismissed_reminder_stays_out_of_due_list_and_shows_in_history() {
        let storage = setup();
        let calendar_id = seed_calendar(&storage, AccountId(1));
        seed_event_with_reminder(&storage, calendar_id, "2026-09-05T09:00:00Z", 10);
        let now = DateTime::parse_from_rfc3339("2026-09-05T09:00:00Z").unwrap().with_timezone(&Utc);

        let due = due_reminders(&storage, now).expect("query");
        let id = record_fired(&storage, &due[0]).expect("record");
        assert_eq!(active_reminder_notifications(&storage).expect("query").len(), 1);

        dismiss_reminder(&storage, id).expect("dismiss");
        assert!(due_reminders(&storage, now + Duration::days(1)).expect("query").is_empty());
        assert!(active_reminder_notifications(&storage).expect("query").is_empty());

        let history = recent_reminder_history(&storage, 10).expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].event_title, "Standup");

        delete_reminder_notification(&storage, id).expect("clear");
        assert!(recent_reminder_history(&storage, 10).expect("history").is_empty());
    }

    #[test]
    fn dismiss_all_active_clears_every_active_and_snoozed_row() {
        let storage = setup();
        let calendar_id = seed_calendar(&storage, AccountId(1));
        seed_event_with_reminder(&storage, calendar_id, "2026-09-05T09:00:00Z", 10);
        let now = DateTime::parse_from_rfc3339("2026-09-05T09:00:00Z").unwrap().with_timezone(&Utc);

        let due = due_reminders(&storage, now).expect("query");
        record_fired(&storage, &due[0]).expect("record");

        dismiss_all_active(&storage).expect("dismiss all");
        assert!(active_reminder_notifications(&storage).expect("query").is_empty());
        assert_eq!(recent_reminder_history(&storage, 10).expect("history").len(), 1);
    }

    #[test]
    fn snooze_all_active_snoozes_every_active_row_but_leaves_already_snoozed_rows_alone() {
        let storage = setup();
        let calendar_id = seed_calendar(&storage, AccountId(1));
        seed_event_with_reminder(&storage, calendar_id, "2026-09-05T09:00:00Z", 10);
        let now = DateTime::parse_from_rfc3339("2026-09-05T09:00:00Z").unwrap().with_timezone(&Utc);
        let due = due_reminders(&storage, now).expect("query");
        let active_id = record_fired(&storage, &due[0]).expect("record");

        // A second, already-snoozed row for a different event shouldn't be touched.
        let other_event_id = seed_event_with_reminder(&storage, calendar_id, "2026-09-05T10:00:00Z", 10);
        let snoozed_id: i64 = storage
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO reminder_notifications \
                     (event_id, event_title, event_start, reminder_minutes, status, snoozed_until) \
                     VALUES (?1, 'Standup', '2026-09-05T10:00:00Z', 10, 'snoozed', ?2)",
                    rusqlite::params![other_event_id, (now + Duration::minutes(5)).to_rfc3339()],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .expect("seed snoozed row");

        let until = now + Duration::minutes(15);
        snooze_all_active(&storage, until).expect("snooze all");

        assert!(active_reminder_notifications(&storage).expect("query").is_empty());
        let snoozed_until: String = storage
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT snoozed_until FROM reminder_notifications WHERE id = ?1",
                    [active_id],
                    |r| r.get(0),
                )?)
            })
            .expect("read snoozed_until");
        assert_eq!(snoozed_until, until.to_rfc3339());

        let untouched_snoozed_until: String = storage
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT snoozed_until FROM reminder_notifications WHERE id = ?1",
                    [snoozed_id],
                    |r| r.get(0),
                )?)
            })
            .expect("read untouched snoozed_until");
        assert_eq!(untouched_snoozed_until, (now + Duration::minutes(5)).to_rfc3339(), "already-snoozed row is left alone");
    }
}
